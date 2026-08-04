# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this is

rheo-os (design codename "Lattice OS") is a greenfield operating system for
modern server hardware: a `no_std` Rust kernel, capability-based security,
built emulation-first in QEMU on three ISAs (x86-64, ARM64, RISC-V 64).

The design lives in `docs/` and is the source of truth. Read
`docs/ARCHITECTURE.md` first; `docs/BUILD-ORDER.md` says what gets built in
what order; `docs/DEVELOPMENT.md` covers the day-to-day mechanics.

**`docs/ARCHITECTURE-DEBT.md` 7 is the consolidated "named but not built" register** -
everything designed or promised in `docs/` with no code behind it, in one list, each row
carrying its gate and what blocks it, marked **G** (provable in this container), **L**
(needs hardware) or **P** (blocked on a prerequisite in the same table). Read it before
planning: two entries are the whole critical path - per-ISA topology discovery unblocks
almost all of the resource graph and is provable here, and stage E2 unblocks almost all of
the execution model and is a pure refactor with the existing suite as its gate.

**`docs/RESOURCE-GRAPH.md` is the machine model** - one typed graph of CPUs, memory
nodes, devices, engines and links, with **cost vectors** rather than a scalar distance
(HBM is low-latency *and* high-bandwidth; CXL is neither), queried rather than hardcoded,
and generalising to remote hosts by making a cluster the same graph with larger costs. It
names the concrete gaps today - SLIT and HMAT unparsed, a device's proximity domain unread
so driver DMA is placed blind, LLC and SMT sets unmodelled - and its gate is **provable in
this container**, since QEMU accepts `-numa dist` and `hmat=on`.

**`docs/GREENFIELD.md` is the design rationale** - the answer to "how would you build
an OS today, knowing everything the last fifty years produced, including the good ideas
that never shipped". It carries the lineage already implemented, ten unlanded research
ideas judged one at a time against three tests (does it beat what we have, does it fit or
fight, can it be proven here), a refusals table with reasons, and a ranked list of what it
changes. Read it to understand *why* the design is shaped as it is, and before proposing
anything that adds a kernel object - the exercise's own result is that none of the ten
needed one.

**`docs/EXECUTION-MODEL.md` is the execution framework** - what a thread, a
process, a task and a CPU each are here, drawn as a hierarchy and as a
dependency graph, with the information budget each carries, the defaults a cell
gets when it asks for nothing, ten invariants, every use case simulated against
the graph, and FRED behind observation. Read it before touching scheduling,
vcores, or the trap path: it exists because five separate defects in that area
turned out to be one defect, and it names the cause.

**`docs/ENGINEERING.md` is the engineering standard** - how a change lands
here: observe-never-infer (a capability is claimed only from evidence the
code cannot fake), waits expressed as deadlines not iteration counts, one
owner per shared resource, deterministic proofs with hand-computed oracles
and live paths as a bonus that degrades with a printed reason, rejections as
deliverables, saying exactly what is true (built / proven / partially proven
/ deferred), and composing before extending so existing proofs stay valid
unchanged. Every rule there was forced by a real defect in this tree and is
cited with its scar. Read it before writing code, and follow its section 12
checklist for each slice of work.

## Current state

BUILD-ORDER.md steps 0-5 are done, plus slices of 6-10, and a native
shell: exception vectors + cycle counters + context switch per ISA; a
bitmap frame allocator and per-ISA paging (Sv39 / AArch64 4 KiB granule /
x86-64 4-level) with the MMU on, the kernel run in the **higher half** on all
three ISAs (docs/MEMORY.md) so the whole low half is free for user programs -
a stock Linux `ET_EXEC` (0x400000) loads unmodified; the capability core
(runtime-tested for
the four ARCHITECTURE.md 8.2 proof properties); the queue-pair ABI with
per-entry grant checks and flow-context propagation; **cells in real user
mode behind hardware address spaces** (RISC-V U-mode, ARM64 EL0, x86-64
ring 3), isolation MMU-enforced, with a cross-cell directed switch.

The full single-host **kernel object model** is implemented: memory grants
(typed, commit/decommit/seal), a monotonic clock + interval wall clock +
**cryptographic per-cell entropy** (a ChaCha20 DRBG with fast key erasure,
non-blocking; the design's "library call over the cell's own
DRBG state, not a syscall" path is proven at the primitive level in the host
comparison, while the lsh `rand` builtin currently draws via `SYS_RANDOM` -
linking the full DRBG into a U-mode cell awaits the runtime's `.user` heap +
`mem*` shims, per docs/TIME-IDENTITY.md 4), typed event
streams with flow context, EDF-admitted reservations, leases with fencing
tokens + epoch revocation, and a dependency graph executed on a compute
engine (attest-by-measurement). On
top of these, **lsh** runs as a U-mode shell cell over a PTY (serial line
discipline bridged by the kernel), with builtins that query the real
objects (`uptime`, `rand`, `meminfo`, `caps`, `ps`, `event`, `graph`,
`reserve`, `lease`) and the machine inventory (`cpuinfo`, `lspci`, `numa`);
a pipeline is a dependency graph submitted to the kernel (docs/SHELL.md).
Run it: `cargo xtask run --bin lsh --arch <isa>`.

**The DRBG is fed by a multi-source entropy pool, and every ISA is now really
seeded** (docs/TIME-IDENTITY.md 4a, `kernel/src/rng/`). Seeding used to read the
CPU's hardware RNG and nothing else, which is fine on x86-64 (RDSEED) and ARM64
(RNDR) and gives **nothing** on RISC-V, whose Zkr `seed` CSR needs an M-mode
`mseccfg` grant this firmware does not give - so that ISA reported
`root_seed_source=Fallback`, a cycle-counter loop that is deterministic under
QEMU and therefore not a source at all. The output algorithm is **unchanged**
(ChaCha20 fast key erasure - it is what makes a read cheap) and the read path
still touches only the calling core's own root with no lock; what is new is
everything upstream of it. **`rng::entropy`** is a 256-bit ChaCha20 pool whose
absorb step is `K' = ChaCha20_block(K, seq++)[0..32] XOR C`, which **cannot
reduce entropy in either direction** - an attacker who chose `C` but does not
know `K` cannot predict the first term, and an attacker who knows `K` gets a
pool that *recovers* to whatever `C` carries. Every source is **mixed**; only
some are **counted**, because this kernel has no entropy estimator and inventing
one would let a predictable source declare the pool ready: a CPU instruction and
a randomness device credit in full after their health tests, the jitter source
credits at most 1 bit per sample and **only when its own tests pass**, and NIC /
NVMe / disk / UART event timings, `/dev/urandom` writes and the boot cycle
counter credit **zero**. **`kernel/src/hw/virtio_rng.rs`** is a new virtio-rng
driver over the same two transports every other virtio driver here uses, so
riscv64 now reports `seed_source=Device` - the fix is a driver, not a per-ISA
workaround. **`rng::jitter`** is the software-only fallback for a machine with
no randomness hardware at all (a data-dependent scratch walk timed 256 times,
the `jitterentropy`/`haveged` idea) and it is measured before it is counted:
aarch64 refuses here with "deltas repeat", x86-64 credits 42 bits and riscv64
20, all far below the 256-bit target, which is the honest answer on a machine
whose cycle counter is deterministic. **The pool cannot run out**: extraction
re-keys the state rather than consuming it, so a read always succeeds (there is
no blocking `/dev/random` and no need for one), `seeded` is **sticky** so
draining the credit counter can never un-seed a machine, and `replenish` tops
the pool up from every askable source whenever credit falls below half a seed
(jitter gated on *not yet seeded*, since its job is a first seed, not
steady-state supply). **`/dev/urandom` writes now do something** - they used to
be discarded while returning success, the stub-reporting-success shape
docs/ENGINEERING.md 7 rejects. And **`rng::health::check()` runs on every boot**:
the ChaCha20 RFC 8439 known-answer test, the FIPS 140-2 continuous test, and an
SP 800-90B window over live output, each a panic on failure because a broken
generator must not reach a cell - silent when healthy, since a line in all ~210
logs saying the same thing is noise. Performance is unchanged where it matters:
an interrupt handler calls `absorb_fast`, two atomic operations into **this
core's own** scratch (no lock, so a handler can never wait on a thread holding
the pool lock), and the pool is drained once every 1024 DRBG derivations off the
*consume* path rather than by a timer. Proven by `rng` on all three ISAs with a
randomness device attached, by `librheoterm` (24 console bytes reach the pool on
all three ISAs) and by `linuxproc`'s `sysx` (an unmodified static-glibc binary
writes 64 bytes to `/dev/urandom` and the kernel asserts exactly 64 more mixed
bytes and exactly zero more credited bits - a number the program cannot see and
so cannot fake); ten controls observed firing, one of which earned its keep
twice, since its first two versions passed - removing one jitter check let the
next one catch the same window, and the test asserted `distinct` but not
`longest_run` when crediting.

Three more RNG items came out of a second pass over the unmerged branches.
**Fast key erasure has two rules and only one was implemented**: the DRBG
re-keyed on every refill (rule 1) but did not erase a byte as it was handed out
(rule 2), so up to 256 bytes of *already-delivered* output sat in the buffer
until the next refill and an attacker who captured the state recovered them -
djb's recording attacker, the exact case the construction defeats. `fill_bytes`
wipes as it copies now, `refill` wipes its whole keystream local and `reseed`
wipes the buffer tail it abandons, word-wide where alignment allows (a per-byte
volatile loop measured ~2x slower on bulk draws); honest limit, `Drbg` is `Copy`
so a caller that copied the struct leaves a stale image no wipe reaches. **The
firmware boot seed** (`/chosen/rng-seed`) is now read - device-tree platforms
hand a kernel entropy before any device is up, `hw::fdt` captures it in the walk
it already performs and `rng::init` absorbs it first, credited in full for the
reason Linux credits it (a bootloader that lied had already loaded the kernel);
QEMU's riscv64 `virt` supplies 32 bytes = 256 bits, while x86-64 and an ARM64
bare-ELF boot have no device tree and are asserted to report it *absent*. And
the interrupt hook's cost is **measured**, not read off the source:
`entropy_mix_event` is **15** icount ticks against 367 for a single `rng_next_u64`
draw and 1,422 for the locked `entropy_absorb_32B` path - ~24x cheaper than one
draw, which is what a handler that quietly grew a lock would break. One control
here is recorded as **self-defeating and replaced**: disabling the seed capture
also made `rng_seed()` return `None`, so the test took its "no device tree"
branch and passed - the same switch flipped the source and the detector.

**A TPM and HID devices are entropy sources too, and both are driven.** The pool
now **names no driver**: a randomness device *registers* with it
(`entropy::register_device_source`) and the pool asks every registered one - it
used to call `hw::virtio_rng::refill()` by name, which put a driver's name inside
the entropy subsystem, so adding the TPM changed no line of the pool.
**`kernel/src/hw/tpm.rs`** drives the FIFO/TIS interface from the TCG PC Client
Platform TPM Profile plus one TPM 2.0 command, `TPM2_GetRandom` - a TPM is
required by its own specification to contain a hardware RNG and is the one source
on a server that is neither the CPU vendor's instruction nor a paravirtual device.
Discovery is firmware's: the ACPI **TPM2** table on x86-64 (start method 6 = TIS at
the PTP's fixed `0xFED4_0000`, 7/8 = CRB, which is recognised and *not* driven), a
`tcg,tpm-tis-mmio` device-tree node on RISC-V, and on ARM64 - which gets no device
tree at all on a bare-ELF boot - a built-in `arch::TPM_TIS_CANDIDATE` that is
**probed, not asserted** (map it, read `TPM_DID_VID`, report absent on all-ones or
all-zeros). That read is *guarded* (`arch::mmio_probe_u32`, reusing the temporary
exception vector the PSCI conduit probe installs), because an undecoded address
raises an external abort on ARM64 that killed the boot in the first version - found
by the control that changes the constant. RISC-V needed a real device-tree fix: the
TPM sits on a `platform-bus`, so its `reg` is **bus-relative** and read as absolute
it was physical 0 - the walker now applies a one-level `ranges` translation with
per-depth `#address-cells`, resolved *lazily* when a child asks, since QEMU writes
`ranges` before `#address-cells`. Proven on **all three ISAs** against a real
`swtpm` backend: `TPM2_Startup` (the chip arrives unstarted, so the
`TPM_RC_INITIALIZE` retry runs every time) then `TPM2_GetRandom` giving 32 bytes and
then 32 different ones, vendor/device `0x00011014`.
**`kernel/src/hw/virtio_input.rs`** is the HID half - a person pressing a key is
unpredictable in a way no deterministic machine reproduces, the source Linux has
collected since its first `/dev/random`. `rng::feed_hid` is named apart from
`feed_interrupt` because they are different claims (a machine's own timing versus a
person); both are mixed and credited **zero**. And it is proven with **real
keystrokes**: a keyboard nobody types on produces no events, so `xtask` attaches a
virtio keyboard and presses keys over QEMU's monitor protocol - hand-written JSON
over a Unix socket, four fixed messages, nothing parsed back, so xtask stays
dependency-free - repeatedly over a window, since nothing signals when the guest has
posted its buffers. All three ISAs read the device's own name from config space
(`QEMU Virtio Keyboard`) and take **4 key events** into the pool; the wait is a
deadline and a run with no keystroke reports that rather than failing. Three more
controls observed firing (a changed TPM command code -> `rc 0x95`; an ARM64
candidate pointed at an undecoded address -> "no TPM" with the boot surviving; the
`feed_hid` call removed -> "4 HID events arrived but none reached the pool").

**The input path is not a keylogger, by construction, and a captured key does not
last forever.** Two hardening passes over the above. (1) `rng::feed_hid` takes a
**sequence number and no event**, so a caller cannot pass a key code even by
mistake - a property a reviewer checks in one line rather than a promise about
what callers do - and the console byte path, which used to pass the byte, follows
the same rule; the HID DMA buffer is wiped as it is drained, asserted by
`virtio_input::wiped() == events()` - the wipe **read back** before the buffer goes
to the device again, which is the only race-free place to ask, since a returned
buffer can be refilled by the next keystroke before any later scan runs (the first
version scanned the buffers afterwards and failed intermittently on riscv64 saying a
drained event was still there, when a *new* one had arrived). It costs nothing, because the unpredictability is
in *when* a key was pressed, not which one, and mixing a key code would put what a
person typed into kernel state for a source credited zero. (2) Against an attacker
who captures the pool or a root: past output is already safe (fast key erasure,
both rules), *future* output needed a fix - a re-key only happened at a full 256
**credited** bits, so a machine whose sources went quiet kept a captured key for
the whole boot, and a root is now re-keyed after at most `REKEY_EVERY` derivations
**whatever the pool holds**, mixing everything absorbed since (honest: a *chance*
of recovery, not a guarantee, since uncredited input may be predictable - what it
removes is "compromised forever"). Influence is already handled by the credit rule,
and the remaining real risk - an attacker controlling *every* credited source - is
met by independence at the one key that matters: `entropy::seed_from_all()` asks
the CPU instruction **and** every device **and** jitter before the first extract
rather than stopping at whichever answered first. On **quantum**: Shor does not
apply (no public-key structure, just a stream cipher), Grover halves symmetric
strength so the 256-bit key gives ~128 bits, which is why the pool's target is the
full key width and `CREDIT_TARGET == 256` is asserted rather than assumed; and
ChaCha20 here is pure integer ARX with no tables and no secret-dependent branches,
so there is no timing or cache channel. Two more controls firing (the HID wipe
removed; the lifetime bound removed).

A **hardware-discovery** layer (`kernel/src/hw/`) builds one portable
machine `Inventory` at boot: firmware source (ACPI on x86-64 via the PVH
RSDP, a flattened device tree on RISC-V, **PSCI `AFFINITY_INFO`** on ARM64 - which
has no firmware table for a bare-ELF boot, so the CPU list is *asked for* rather
than assumed; the rest of its QEMU-virt profile stays built in), CPU count and instruction-set features (CPUID / `ID_AA64*` / the
device-tree ISA string), the typed physical memory map (DDR / reserved /
ACPI / pmem), NUMA topology (SRAT memory + CPU affinities, memory regions
split at node boundaries), and PCIe enumeration through the ECAM/config
space, classifying each function into an engine kind - GPU, NIC, NVMe, or a
processing accelerator (NPU/TPU, PCI base class 0x12). The `hwinfo` test
kernel asserts the basics on all three ISAs; `cargo xtask run --bin hwinfo`
prints the full inventory. **Real-GPU stage 1 is done** (docs/GPU-HARDWARE.md
12): PCIe enumeration now recurses bridges (programming secondary bus numbers
where firmware left them zero - the bare arm/riscv boots have no firmware to
do it; x86 q35's `-kernel` path runs SeaBIOS, which got there first), sizes
every BAR by the mask probe, walks the capability list (MSI/MSI-X/PCIe/FLR),
and offers opt-in BAR assignment from a per-ISA host-bridge window
(`hw::assign_pci_bars`, invisible to boots that skip it); `kernel/src/hw/gpu.rs`
recognises every display-class function by vendor (NVIDIA, AMD, Intel, virtio,
Bochs, Cirrus, VMware, Red Hat/QXL) AND silicon family (NVIDIA
Pascal/Turing/Ampere/Ada/Hopper/Blackwell, AMD GCN/RDNA/CDNA, Intel Xe) into
the inventory, resolves a per-vendor driver front-end (`vendor_driver`
declaring each vendor's lowering path per ACCELERATORS.md 4), drives **every
GPU QEMU models** (AMD/Bochs/Cirrus/VMware/QXL via a framebuffer-aperture
write+read-back, virtio-gpu via its 2D command driver - up to six vendors on
x86-64, up to four on arm/riscv where VMware+QXL are x86-only). **Which models a
QEMU build has is observed, not listed**: it was a constant in `xtask` and a
matching count in the test, and QXL needs a QEMU built with SPICE - against one
without it, `-device qxl` is rejected *at launch*, a zero-byte serial log naming
no cause. `xtask` asks `-device help` and attaches what is there, printing each
drop; the test asserts a property of the **bus** - every enumerated function whose
vendor has a linear framebuffer is driven (counted before driving anything), with
a floor of three - and a vendor QEMU cannot model is covered the way NVIDIA and
Intel already were, its PCI id classified directly and its absence reported. Each
registers in the engine table behind `SYS_ENGINE_INFO(out_va, index)` enumeration (kind + PCI
vendor ID + declared op-boundary preemption, an honest zero measured cost -
recognised and registered, not yet driven). The `gpuhw` test proves it on all
three ISAs against QEMU's real `ati-vga` (AMD, 0x1002), a Bochs display, and
a virtio-gpu behind a `pcie-root-port`; NVIDIA/Intel have no QEMU device
model and report skip-with-reason. The stage closes with the tree's first
real vendor-GPU MMIO: the AMD framebuffer aperture mapped via
`arch::mmio_map_window` (a second x86-64 fixed window beside the pmem one;
the missing 1..2 GiB gigapage on RISC-V; `phys_to_virt` on ARM64), written
through and read back on all three ISAs; plus opt-in **attach measurement**
(`hw::gpu_attach_measure`: ticks/KiB streamed through each aperture - a
transport measurement, reported live by `SYS_ENGINE_INFO`) and a **real Bochs
2D modeset** (`hw/gpu.rs::bochs_modeset` over the DISPI/VBE interface:
640x480x32 + LFB, framebuffer render + pixel read-back on all three ISAs),
so every GPU device model QEMU has is genuinely driven - virtio-gpu by the
Phase H 2D driver, AMD through its framebuffer aperture, Bochs by a working
2D modeset. **Real-GPU stage 2 (the IOMMU) is done on x86-64 AND ARM64**
(docs/GPU-HARDWARE.md 4, BUILD-ORDER step 12): two backends, both proven by
the `iommu` test with a real device (virtio-blk negotiating
`VIRTIO_F_ACCESS_PLATFORM`) - a read succeeds through an identity domain,
then FAULTS once the domain is revoked. **x86-64 VT-d**
(`kernel/src/hw/iommu.rs`): DMAR-discovered register base, root/context/
second-level page tables, queued invalidation (QEMU's caching-mode IOMMU
only tears down device shadow mappings via QI), fault read from the
fault-recording register. **ARM64 SMMUv3** (`kernel/src/hw/smmuv3.rs`):
linear stream table + Context Descriptor + LPAE stage-1 page tables (QEMU
models stage-1 only), a command queue for STE/TLB invalidation, fault read
from the event queue. (Fixing this also fixed a real latent bug: the
virtio-blk PCI path passed a hardcoded 0 ECAM base to the config accessor,
which x86 ignores but ARM/RISC-V use as the MMIO base - so virtio-blk-pci
never worked on ARM; now it uses the discovered ECAM base.) RISC-V
skips-with-reason (no QEMU IOMMU model in 8.2).

The **strand runtime** (`runtime/`, BUILD-ORDER.md step 7,
docs/CONCURRENCY.md) is the userspace library that brings native async and
`alloc` on the OS's own terms - not a POSIX threading port. It has a
free-list **heap allocator** (`GlobalAlloc`, host-fuzzed) so `alloc`
collections (Box/Vec/String/BTreeMap) work in a cell (the kernel itself is
allocation-free); an async **executor** where a *strand* is a `Future` that
"blocks" structurally by parking on a token and is woken by the queue-pair
completion carrying that token in `user_data` - one drained completion ring,
N strands resumed, no blocking syscall; an async **channel** (park on empty,
wake on send); `spawn`/typed `JoinHandle` (structured concurrency),
`yield_now`, and locking adapted to the model - an async **`Mutex`** that
parks a strand on contention (never loses the vcore) and a fair `TicketLock`
for the future multi-vcore case; and **capability rights at the type level**
(KERNEL-RUST.md 2: `Rights<MASK>` + `SubsetOf`, so widening a capability's
rights is a compile error). The `runtime` test kernel exercises all of it -
including strands doing I/O over the real queue-pair ABI - on all three ISAs.
Full Rust (generics, traits, async/await) runs natively; the runtime is
proven kernel-context here, and the same library links into a U-mode cell
(that last integration - a `.user` heap grant + `mem*` shims - is future
work, the shell shows the U-mode constraints).

**Strands are validated as light threads** (comparison/threads/): the exact
executor, measured on the host against the named runtimes, spawns+tears down
a task in ~85 ns and switches in ~12 ns - roughly **1,200-1,600x faster than
OS threads** (Rust `std::thread`, Python `threading`), ~150x faster than
Python `asyncio`, and ~8-17x faster than Go goroutines (strands are
stackless, so there is no per-task stack to allocate). In-QEMU icount path
lengths (`bench` `p4_*`): ~450 instructions to spawn+tear down, ~150 to
switch, consistent across ISAs. (.NET 10 is not installed here, so it is left
unmeasured with an architectural note rather than a fabricated number.)

**Concurrency, async and synchronisation are measured, not asserted** (the `runtime`
kernel's three closing phases, all three ISAs, each with a hand-computed oracle and an
observed negative control). **Concurrency**: 256 strands take 4 rounds each and every
round of the shared order vector is a **permutation of all 256** - so all 256 were live
at once (none ran to completion first) and none took two turns in a round (none
monopolised the vcore); letting one strand skip its yield breaks it on the first round
boundary. **Async**: 63 queue operations (the ring's depth) are outstanding at a single
instant - the measurement is taken where the executor first runs dry, with *every*
strand parked and *none* finished - and one service pass then wakes exactly 63, one park
and one wake per operation with no re-polling; servicing each submission as it is made
breaks it. **Sync**: 256 strands contend on one async `Mutex`, each **suspended inside
its critical section** so every peer must park, and two oracles hold - exactly 256
increments (no lost update) and never more than **1** concurrent holder, sampled from
inside the section, because a right total alone would only mean the interleaving
happened to be benign.

A **POSIX + filesystem stack** (`posix/`, docs/FILESYSTEMS.md,
POSIX-PERSONALITY.md) sits on a **VFS** translation layer (a `FileSystem`
trait): a read-write **ramfs** (the working store), a read-only **ext4**
driver - the **`ext4plus`** crate (Google's read-only `ext4-view`), adapted to
`posix::FileSystem` over a `posix::BlockSource` in the separate **`ext4fs`**
crate so its deps stay out of the dependency-free `posix` (docs/FILESYSTEMS.md
names the crate + its transitive deps per the no-deps rule; it **replaced** the
original hand-rolled bounded parser, which was extent-depth-0 only, so a ~1.7 MB
glibc `libc.so.6` now reads); driven in `sync` mode at the kernel-resident
`svc::FileOps` seam, flipping to async when the FS becomes a service cell over
the queue ABI (where NVMe's queues earn it) - a mount table + path resolution
(the per-session `/`), the **POSIX
fd surface** (`open/read/write/close/lseek/stat/getdents/mkdir/unlink` with
errno), and a **`std::fs`-shaped facade** (`File`, `OpenOptions`,
`read`/`write`/`read_to_string`, `read_dir`, `metadata`) so standard-library
file code runs natively. The `posix` test kernel exercises ramfs rw, ext4 ro
(incl. a multi-block file), the errno surface, and the std facade on all
three ISAs.

**NVMe runs** (docs/SUBSTRATE.md S5, the `nvmefs` kernel, all three ISAs): a
hand-written NVMe 1.4 driver (`kernel/src/hw/nvme.rs`) brings up a real controller
over PCIe - disable, publish the admin queue pair (`AQA`/`ASQ`/`ACQ`), enable,
`IDENTIFY` namespace 1, `SET FEATURES` number-of-queues, create the I/O completion
queue then the submission queue that names it - and serves `NVM READ`/`NVM WRITE`
behind the same `BlockDevice` seam virtio-blk uses. Why NVMe rather than more
virtio-blk: virtio-blk is paravirtual, one queue, a hypervisor behind it; NVMe is
what real storage presents - **paired submission/completion queues in host memory,
a doorbell, out-of-order completion, one queue pair per core** - and that last
property is the rest of S5 and the same shape as this OS's own queue ABI. It is
also the first device here that **needs a BAR**: virtio-pci is driven through the
`VIRTIO_PCI_CAP_PCI_CFG` config tunnel precisely to avoid one, while NVMe's
register file *is* BAR0, so `nvmefs` calls `hw::assign_pci_bars()` and maps the
window. The test is deliberately `blockfs` with the transport swapped - same ext4
image, same two files, same byte-exact assertions, same bounded cache proving the
bytes streamed - because that is the claim being made about the seam; plus a
**write round trip** (last sector, fresh handle so the cache cannot answer it,
pattern written and read back, then the original restored and read back, so the
device must return two different things for one sector in order) on a drive
attached `snapshot=on` so the committed fixture is untouched. **Completions raise an interrupt** on x86-64 (MSI-X, one vector per queue delivered
to that queue's own core), so a waiting core halts instead of spinning - 31
interrupts and ~30 halts per run; ARM64 needs a GICv3 ITS and RISC-V an IMSIC
target, so both poll **and say so**, with the test asserting the two can never
disagree. The path is **verified by observation** before use, because a claimed
interrupt that never arrives is a hang rather than a slow path (the reap loop halts
and so never reaches its deadline - masking the table entry turned a passing test
into a 120 s timeout); the verification itself must not be able to halt either, so
`arch::irq_window` opens a one-instruction window rather than using `idle_wait`; and
each channel is verified **by the core that owns it** against **that CPU's** counter,
since a global one would let a busy sibling answer the question. A per-core queue
needs a per-core interrupt: with every MSI aimed at the boot CPU a secondary halted
while the primary took its vector, caught by the `smp` two-core phase. That verification then caught two more defects: a
secondary's LAPIC is software-enabled by nobody (the AP trampoline sets none, and a
core that never armed a timer never enabled its own), fixed by
`arch::irq_ready_this_cpu` - split out of `enable_timer_irq_this_cpu` rather than
reusing it, since that also writes `TMICT = 0` and would silently disarm the timer
arbiter's deadline; and eight MSI-X table entries route nothing on their own,
because the vector a completion queue raises is a field in its *create* command
(`CDW11[31:16]`) and leaving it zero sent all eight through entry 0 onto the boot
CPU. `smp` now asserts `poll_fallbacks() == 0`, which fails by name when that field
is reverted - a queue whose vector goes elsewhere still returns the right bytes, its
owner just polls, so nothing else catches it. **RISC-V MSI was implemented and withdrawn**, and the result is a
measurement rather than an unexplored gap: an MSI there is a write into the per-hart
IMSIC file, and with the table entry programmed to `0x2800_0000` identity 32 the
entry reads back correctly, Message Control reads `0x8040_8011` (enabled, unmasked)
and the hart has `eidelivery=1`/`eie0` bit 32/`sie.SEIE` set - yet `eip0` stays **0**
after a completion, so the device's write never reached the IMSIC. The open question
is QEMU's PCIe-DMA routing, not the driver; shipping a write path that provably does
nothing where it can be tested, on the strength of "it should work on hardware", is
the untested claim this tree refuses, so it is `None` with the evidence recorded in
`arch/riscv64/mod.rs`. **A GICv3 ITS driver was written for ARM64 and withdrawn too**, getting further:
every command was consumed (`GITS_CREADR` caught `GITS_CWRITER` after MAPD/MAPC/
MAPTI/INV/SYNC) and LPIs were enabled over a shared 8 KiB config table and a
64 KiB-aligned per-core pending table (statics - the frame allocator offers neither
contiguity nor that alignment), and it turned up a real defect worth recording:
`GITS_TYPER.PTA` is **0** here, so `MAPC`'s `RDbase` is a *processor number*, not a
redistributor address - and getting that wrong looks exactly like getting everything
else wrong, since the commands are still accepted and the queue still drains. No LPI
was ever taken, so the open question is which mapping QEMU disagrees with, not
whether the tables were published. Both withdrawals share a shape worth naming: the
per-ISA MSI seam (`msi_target`/`msi_route`/`irq_ready_this_cpu`) is in place and the
x86-64 implementation behind it is proven, so returning to either is filling in one
function against a working contract from recorded evidence. **NVMe DMA is IOMMU-mediated** (the `iommu` kernel now runs the controller behind an
identity domain and asserts the read succeeds - a distinct claim from the virtio-blk
proof, since NVMe DMAs from queues and staging buffers it allocated itself). Getting
there uncovered **two pre-existing defects**, both invisible until a second device
exists: `arch::mmio_map_window` mapped *every* caller at the same VA, so the second
driver silently replaced the first's mapping and the IOMMU's register writes went
into an NVMe BAR (allocated per caller now, exhaustion refused rather than wrapped);
and the VT-d queued-invalidation wait was **unbounded** (`while IQH != IQT`, no
deadline), which turned that into a 120-second timeout with no output at all - now
bounded with the reason printed, as is the root-table handshake beside it. The **revoke** half is proven too - DMA outside the domain faults and the read
fails - and it is what made the driver's completion wait honest, the third defect the
gate turned up: a completion wait **halts**, and the only thing that ends the halt is
the completion interrupt, raised by the same device whose DMA the wait depends on. So
revoking the domain stopped both together, the halt had no wake source, and the wait's
own five-second deadline was never reached - a **hang, not a timeout**, which any
wedged controller reproduces on real hardware. The halt now carries an arbiter
deadline of its own (`ktimer::TimerClient::Storage`) with `other_source = false`;
`true` lets the arbiter halt on the device alone, which is the hang again and was the
first attempt. Where no hardware timer is up the arbiter declines to halt and the wait
spins - slower, deadline reachable, the right way round. Proven by reverting to
`park(true)`, which hangs at exactly that point. Adding the slot caught a hazard of
its own: `ktimer::CLIENTS` is a hand-written count of a hand-written enum, and getting
it wrong is an out-of-bounds index from a driver at run time rather than a compile
error - asserted against the last variant now. Honest: no storage *cell* itself (DRIVERS.md D2 - a
userspace driver owning the queues behind BAR grants and forwarded interrupts), no
ARM64 or RISC-V MSI, and
transfers bounce through page-aligned frames one page per command so `PRP1`
addresses every command and no PRP list is built.

**A cell can be handed a device's registers** (docs/DRIVERS.md 4.1, the first of D2's
three legs - the *mapping*, not the capability bundle): `load::map_device_bar` maps a
whole-page span of an enumerated, assigned, memory-space BAR into a cell's address space
as the new `MapPerm::UserDevice` - uncached device memory per ISA (x86-64 `PCD|PWT` =
PAT entry 3 UC; ARM64 MAIR attr 0 Device-nGnRnE, unshared; RISC-V base Sv39 has **no**
cacheability field, so it is a plain mapping and that is stated rather than implied) -
refusing I/O-space and unassigned BARs and anything over `USER_BAR_MAX` (4 MiB), in its
own fixed region (`USER_BAR_VA`, 28 GiB) below the ISA user floor. `SYS_GRANT` with
`MemKind::DeviceBar` **stays refused**, which is the point: the window is minted by
whatever launches the cell, exactly as the W^X exception, the cell-spawn capability and
the queue pair are, so a cell still cannot give itself a device. Proven by `nvmefs` on
**all three ISAs** against a value the cell cannot fabricate - the NVMe controller's VS
register at BAR0+0x08, read at the unprivileged level through the granted window and
equal to the kernel's own MMIO read of the same register (`0x10400` here, a live reading
rather than a constant; aiming the cell 4 bytes off makes the comparison fail, observed)
- with the **bound** asserted in the same phase, since a window that overran its BAR
would hand the cell the next device's registers: the same cell aimed one page past the
mapping faults, and removing the grant entirely faults too (both observed), so the read
is a real access through the page tables. Honest: **cacheability is unobservable under
QEMU** (TCG models no cache), so the attributes are asserted by construction in the
per-ISA paging code and not by measurement; and the rest of D2 - a config-space
capability, a cell-lifetime IOMMU domain, a cell-reachable DMA-map verb, and the
per-device IRQ wait - is not built, so no driver has been re-homed into a cell.

**And the storage data path is per-core** (docs/SUBSTRATE.md S5, the `smp` kernel's
NVMe phase, all three ISAs): the driver creates one queue pair *and one bounce frame*
per CPU, and a core submits on its own - selected by CPU index. Two counters make it a
measurement: submissions per queue, and submissions on a queue the submitting CPU does
not own. Two cores read **different** sectors at the same instant (meeting at a
rendezvous first, so the overlap is real) and it is asserted that two distinct queues
took work, each core's bytes are its own, and **zero** submissions crossed a core. Two
things had to be got right, both initially wrong the same way - a fact guessed rather
than asked for. (1) A core with no queue of its own is now **refused**, not quietly
given core 0's: the first version counted the fallback and carried on, which reads as a
degraded mode and is not one, because two cores on one ring is a data race that presents
as *wrong bytes* - found exactly that way, the same sector reading back differently on
round 3 with no fault and no log. (2) `RefCell` was the wrong primitive and not
stylistically: its borrow flag is a plain `Cell`, so a `RefCell` is `!Sync` and a type
containing one cannot soundly be shared between cores whatever the access pattern
underneath - and this device *is* reached from two. It is a `SpinLock` now, never
contended because of the partitioning, with a `const` assertion that `Nvme: Sync` so a
future field cannot undo it silently (the same call `mm::frames` already made: whether a
structure needs a lock is a property of the structure, not of which cargo features are
enabled). **The queue also has depth**: a core issues up to 8 commands with **one
doorbell**, each staging through its own frame from a per-channel pool, instead of paying
a controller round trip per page - asserted by a read large enough to fill a batch, with
every page of the batch checked against the same page read singly (a count alone would
not catch a batch that mixed its pages up), and the assertion observed failing when the
plan is reverted to one command per batch. Worth recording about the completion path:
QEMU's controller genuinely reorders (all eight completions of an eight-deep batch arrive
out of submission order), but **two drafts claimed more than that supports and both
negative controls passed** - one said assuming submission order would corrupt on
hardware, the next restructured the copy to happen per completion so the command
identifier would be load-bearing, and substituting submission order for the looked-up
identifier changed nothing either time, because each command's `PRP1` already names its
own staging frame and a batch that waits for all N does disjoint copies whose order
cannot matter. The identifier's real job here is bounding a completion to its batch; it
becomes load-bearing only once a completion is acted on before its siblings arrive, which
is the interrupt path and is not built. The second draft was reverted rather than kept.
The RefCell fix was only reachable because **ARM64 CPU enumeration was
fixed first** - see docs/SMP.md 7: `discover` reported one CPU while four ran, so the
driver sized its queues from a field that lied. It now enumerates from PSCI
`AFFINITY_INFO` (`arch/aarch64/psci.S`, always compiled - how many CPUs a machine has is
a property of the hardware, not of a build configuration), reporting `firmware=Psci
cpus=4` in the **non-SMP** build, and the driver no longer depends on that count being
right anyway: it sizes by the CPU *index space*, which is the quantity it indexes with.

A **live-disk block stack** closes the loop from storage transport to
filesystem: a **`BlockDevice` trait** (`kernel/src/hw/block.rs`, 512-byte
sectors, transport-agnostic) and a **virtio-blk driver**
(`kernel/src/hw/virtio_blk.rs`) - reset/feature negotiation, a split
virtqueue, and the block request protocol - over **two transports**:
virtio-mmio on arm/riscv `virt`, and **virtio-pci on x86-64 q35**. The x86
path drives the device *entirely through PCI configuration space* using the
`VIRTIO_PCI_CAP_PCI_CFG` capability (virtio spec 4.1.4.8), so no BAR needs
to be assigned or mapped - which matters because PVH boot has no firmware to
program BARs. Since the kernel moved to the high half (docs/MEMORY.md) PA no
longer equals VA, so the driver hands the device **physical** addresses for
the virtqueue via `virt_to_phys` (the queue lives in the kernel's own RAM,
reached through its linear map). The `blockfs` test kernel discovers the device,
mounts a real ext4 image off the *live disk* (attached by QEMU with `-drive`),
and reads files through `std::fs` - on **all three ISAs** - and it now
**streams**: the ext4 driver reads through a `posix::BlockSource`
(byte-addressed `read_at`), and `blockfs` mounts the disk behind a bounded,
allocation-free LRU `kernel::hw::block::BlockCache` (`CAPACITY = LINE*LINES`)
rather than slurping the whole disk into RAM. The proof asserts the streaming
property directly: the 7800-byte multi-block file reads correctly through an
**8 KiB** cache over a **512 KiB** disk (`CAPACITY < disk`), with
`block::cache_fills() > 0` proving the bytes came off the device on demand -
so a filesystem no longer needs the whole image resident (the "binary need not
reside whole in RAM" rung, docs/ARCHITECTURE-DEBT.md 4.0 blocker 2). An in-RAM
`&[u8]` is still one `BlockSource` (the `posix` kernel's path, unchanged). At
the `BlockDevice` seam existing Rust FS drivers (redoxfs, fatfs, a read/write
ext4 crate) can be dropped in rather than hand-written - gated by the no-deps
rule (a doc must name any crate).

A **native-userland** path is being built out so real Rust/C/C++ apps
(eventually the uutils/coreutils) run as cells, recompiled for a rheo-os
target over a relibc-style Rust libc (docs/USERLAND.md, milestones M1-M5).
Done so far: an **ELF loader** (`kernel/src/elf.rs` + `load.rs`) with a
general per-cell address space (`arch::paging_map_frame` maps an arbitrary
user VA to an allocated frame, creating tables on demand) - a separately-
compiled program (the `userland` crate) is loaded into a cell and run, no
longer baked into the kernel image (the `elfrun` test); and a
**multi-argument POSIX syscall surface** - the ABI is now
`decode_syscall -> (nr, [u64;6])`, with kernel-native `mmap`-anon +
`exit_group`, and fd-based `open/close/read/write/lseek` forwarded to a
**personality handler** (`svc::FileOps`, function pointers a service
registers) backed by the `posix/` VFS, keeping the kernel filesystem-free
(the `posixrun` test runs a native program that reads a file over the VFS);
and a **Rust libc** (`libc/`, package `rheo-libc`) - a relibc-style
translation layer with `crt0` (`_start`), a heap over `SYS_MMAP` wired as the
global allocator (so Rust `alloc` works) plus C `malloc`/`free`, fd-based I/O
wrappers, and `println!` (the `libcrun` test runs `libcdemo`, which builds a
`Vec`, round-trips C `malloc`, formats output, and reads a file via the VFS).
A **custom Rust target + std port** works (M4): real `std` compiles, links,
and **runs on the OS on all three ISAs** - a std program using
`String`/`Vec`/`format!`/`println!` returning an `ExitCode` (the `stdrun`
test). The `rheo-os` targets (`targets/rheo_os-*.json`, `os = "rheo"`,
soft-float) build std via a repo-held, idempotent rust-src patch
(`cargo xtask std-patch`, `targets/patch-std.py` + `targets/std-rheo/`): std
routes rheo to the single-threaded portable fallbacks (SMP deferred) with real
rheo arms for the heap (a hole-list allocator over `SYS_MMAP`), non-blocking
`stdio` (fds over the M2 syscalls), and `process::exit` (`SYS_EXIT_GROUP`); a
crt0 (`rheo-rt`) provides `_start`. The `rheo_os-*` std targets stay soft-float
(the kernel now enables U-mode FP/SIMD and saves it across switches, and
**librheo** cells build hard-float - see the tile framework below; flipping the
std targets to hard-float is a follow-on). Also built
alongside as an M4-prep workload: **rheo-json** (`json/`), a dependency-free
zero-copy JSON parser that runs on the OS and is benchmarked against simdjson
(docs/JSON.md).

**Coreutils run on the OS** (M5, docs/USERLAND.md): standard command-line
tools as a U-mode cell. Three OS capabilities land first - **argv/env** (the
kernel builds the System V initial process stack in `setup_stack`; the crt0
reads `argc`/`argv` so `std::env::args` works; env is an in-process table), and
a **`std::fs` read/write path over the VFS** (a rheo `std` `fs` arm translating
`File`/`metadata`/`read_dir` onto the file syscalls, now including `stat`/
`fstat`/`getdents`, forwarded to the POSIX personality). On top of them
**rheo-coreutils** (`targets/std-rheo/coreutils/`) is a busybox-style multicall
program built against real `std` - `true`/`false`/`echo`/`cat`/`wc`/`head`/
`ls`/`seq`/`basename`/`dirname`/`nl`/`pwd`/`env`, dispatched by `argv`. The
`coreutils` test loads it with a real `argv`, runs one utility per invocation
over a ramfs, and asserts each utility's exit code and exact stdout on all
three ISAs. These are faithful from-scratch ports, not the upstream uutils
crate (whose clap/uucore tree needs `std` surface rheo lacks - `IsTerminal`,
locale, terminal width - so it is deferred; docs/USERLAND.md M5).

A **Linux personality** is complete (docs/LINUX-COMPAT.md, milestones
L0-L7 all done) so *unmodified* Linux binaries - unpatched Rust std
(`*-unknown-linux-gnu`), glibc-linked C (static and dynamic), stock tools - run
as cells. It is
kernel-resident like `svc.rs` (a documented bridge to a future personality
cell) and adds no kernel object: PIDs/fds/signals are per-cell synthesized
state. L0 is done: a per-cell `Personality { Native, Linux }` tag branches
dispatch *before* the syscall number is decoded (the ABIs collide),
per-ISA Linux syscall-number tables live in `arch::linux_abi` (x86-64
legacy table; asm-generic table shared by arm64/riscv64), unknown numbers
log `linux: ENOSYS nr=<n>` and return -ENOSYS. L1 added the ELF auxv,
ET_DYN loading, the U-mode thread pointer (fs_base / tpidr_el0 / tp), and
U-mode FP/SIMD enablement. **L2 is done**: the core syscall set (read/write/
readv/writev, openat/close/lseek, fstat/newfstatat, getdents64, brk,
anonymous mmap/munmap/mprotect, poll/ppoll, uname, clock_gettime, getrandom,
prlimit64, ioctl(TIOCGWINSZ), dup/fcntl, and the identity/recorded/ENOSYS
calls - honesty table in docs/LINUX-COMPAT.md), a per-cell fd table
(`kernel/src/linux/fd.rs`: console, VFS files, /dev/{null,zero,urandom}),
real memory over the cell's own address space (`AddressSpace::unmap`/
`protect` + per-ISA `paging_unmap_frame`/`paging_protect` + `frames::free`;
`kernel/src/linux/mem.rs`), and per-ISA `ticks_to_ns`. The `linuxrun` test
now also runs, on **all three ISAs** with exact stdout + exit asserted, two
**unpatched static-glibc** binaries built from source: a Rust `std` hello and
a C hello, each loaded at glibc's **stock ET_EXEC base, no relink**
(docs/LINUX-COMPAT.md L2). glibc (not musl) is the supported libc. **L3 is
done**: the **unmodified upstream uutils/coreutils** multicall binary (crates.io
`coreutils` 0.0.29, pinned, static-glibc, built from source by xtask - no binary
in git) runs as a `Personality::Linux` cell on **all three ISAs**; the
`linuxtools` test asserts exact stdout + exit for eleven utilities (true, false,
echo, cat, seq, head, wc, basename, dirname, ls, pwd). L3 added per-cell cwd
(getcwd/chdir), statx, mremap, a single-process pipe2 (splice stays ENOSYS -
uu_cat falls back), a per-fd getdents64 cursor, `/proc/self/auxv`, and the
`openat` dirfd-as-`int` fix. **L4 is done**: **threads as multi-context cells**
(the CONCURRENCY.md vcore model made real) - a Linux cell holds up to 8
execution contexts (a `TrapFrame` + FP save area each), scheduled
**cooperatively, round-robin, at syscall boundaries** on the single CPU
(`kernel/src/linux/thread.rs`), sharing one address space and kernel stack with
eager FP/SIMD save-restore and per-context TLS. Added `clone` (pthread flag set,
arch-specific arg order via `CLONE_BACKWARDS`), `futex` (WAIT/WAKE + _BITSET),
per-context `gettid`, thread `exit` vs `exit_group`, CHILD_CLEARTID clear+wake,
`set_tid_address`, real `sched_yield`, and `sched_getaffinity`/`prctl`(name);
memory gained demand-commit (PROT_NONE `mmap` reserves without frames,
`mprotect` commits) so glibc's per-thread 64 MiB arenas don't exhaust the pool.
PIDs/TIDs/futex waiter lists stay per-cell synthesized state (no kernel object).
An **unpatched multi-threaded Rust `std`** binary (4 `std::thread`s + `mpsc` +
`Mutex` + `Arc<AtomicUsize>` + join) runs on **all three ISAs** with exact
stdout/exit asserted (the `linuxthreads` test), and `sort` (rayon-threaded) is
re-enabled in `linuxtools`. Cooperative-only (a spinning thread starves siblings
until timer preemption, task #27) and futex wake is FIFO (priority inheritance is
a documented TODO) - both disclosed in docs/LINUX-COMPAT.md + CONCURRENCY.md.
**L5 is done**: **synthesized POSIX signals, no kernel object** - dispositions a
per-cell table, masks/pending/altstack per-context (`kernel/src/linux/signal.rs`).
Delivery is a **saved-`TrapFrame` rewrite** in trap context: a Linux `rt_sigframe`
(siginfo + ucontext with the interrupted GPRs/PC/SP + saved mask) is built on the
user stack, the frame's PC set to the handler and its return to the restorer -
glibc's `SA_RESTORER` on x86-64, an injected 2-instruction `rt_sigreturn`
trampoline page (`arch::SIGTRAMP_VA`) on ARM64/RISC-V; `rt_sigreturn` restores.
Real `rt_sigaction`/`rt_sigprocmask`/`sigaltstack`/`rt_sigreturn` (the L2 stubs)
plus `kill`/`tgkill`/`tkill`/`rt_sigqueueinfo` (self-targeting). **Synchronous
faults become signals**: `on_user_trap` maps the per-ISA fault cause (vector /
ESR EC / scause -> `arch::FaultCause`) to SIGSEGV/SIGBUS/SIGILL/SIGFPE and, for a
Linux cell with an installed unblocked handler, delivers by frame rewrite; else
terminates 128+signo. A **native** cell fault stays terminal (`Outcome::Faulted`)
- delivery is behind the Linux branch only; the x86-64 ring-3 fault path
(`vectors.S`) now captures the full `TrapFrame` and resumes via `sysret`
(ARM64/RISC-V already carried the frame). The `linuxsig` test asserts, on **all
three ISAs**, three static-glibc C fixtures: `sig_raise` (async
`raise(SIGUSR1)`->handler->resume, exit 0), `sig_segv` (null write ->
SIGSEGV handler `_exit(0)`, not a killed cell), `sig_dfl` (`raise(SIGABRT)`, no
handler -> terminate 134), and `sig_fp`. Scope: self / fault delivery (a signal to
a non-running sibling context is recorded pending, not force-delivered).
**FP/SIMD across a handler is now saved and restored** (docs/SUBSTRATE.md S4): a
handler runs on the *live* register file - the kernel is soft-float, so nothing
between the trap and delivery touches it - so one FP instruction inside a handler
used to destroy the interrupted code's vector registers with no fault and no log,
harmless for the programs L5 shipped with and fatal for a JIT taking a profiling
signal mid-vector-loop. Delivery saves the image to the **user stack**, above the
frame it writes, so nesting works by construction (each delivery its own area,
each `rt_sigreturn` its own restore); the kernel keeps only the VAs, four deep per
context, and past that the delivery still happens with the loss printed. The
`sig_fp` fixture is worth reading as a lesson in what such a proof must look like:
**two earlier versions passed with the fix deleted** - `raise()` is a call, so
caller-saved FP is already dead across it, and a handler is an ordinary C function
that *preserves* the callee-saved registers a register allocator would pick - so
only inline asm on both sides (the `librheoipc` register-pattern technique) makes
it an experiment. It also found a **fourth SYSRET-provenance defect**: returning
from a handler to the interrupted instruction had never been exercised, and on
x86-64 `rt_sigreturn` rewrites its frame **in place**, so the frame-*pointer*
provenance test saw "the frame I entered on" and took SYSRET, overwriting the
restored RCX/R11 with the resume RIP/RFLAGS - RCX held a pointer, so the resume
jumped into it and looped. The test is now the precondition itself (SYSRET is
correct exactly when RCX already holds the return RIP and R11 the RFLAGS, which
SYSCALL guarantees on entry), so no flag can be forgotten. Both fixes observed
failing when reverted.
**L6 is done**: **processes - fork / execve / wait4 / cross-cell pipes** (no new
kernel object; all per-cell synthesized state, `kernel/src/linux/proc.rs` +
`pipe.rs`). `fork` is clone-cell-within-capability-bundle: a new `user` cell in
the parent's bundle with the parent's committed pages **eager-copied** (COW
deferred) via a per-ISA page-table user-leaf walk (`arch::paging_for_each_user_leaf`
behind `AddressSpace::fork_from`/`free_user_frames`), the `LinuxState`/fds/cwd/
signal dispositions deep-copied, child pid synthesized. `execve` **streams** a new
ELF from the VFS into a fresh address space (`load::exec_elf_from_vfs`: only the
header + phdrs are buffered; segments read page-by-page into destination frames,
the kernel holds no whole-image buffer). `wait4` blocks the parent cooperatively
until a child exits, reaping WIFEXITED/WIFSIGNALED status and freeing the child.
`pipe2` is a **global cross-cell** ring buffer (`kernel/src/linux/pipe.rs`) whose
two ends live in different cells after fork; reads/writes block cooperatively with
cross-cell wake, EOF on all-writers-closed, SIGPIPE on all-readers-closed. The run
loop is **generalized**: a cell that blocks or exits hands the CPU to the next
runnable cell via the same address-space switch the native `SYS_SWITCH` uses -
the native path is byte-for-byte unchanged, `crate::linux::proc` drives the
cross-cell switch behind the `Personality::Linux` branch. `MAX_CELLS` 8 -> 16. The
`linuxproc` test proves it on **all three ISAs**: (A) a direct static-glibc
multi-process fixture (`procdemo`: pipe2+fork+dup2+execve of `/bin/cecho` from the
VFS+wait4, exact transcript+exit), and (B) the **P11 gate** - `rsh`, a minimal
from-scratch static-glibc shell (dash cross-build was out of budget), forks+execs
the unpatched upstream uutils/coreutils multicall over pipelines and `&&`/`||`,
passing a 12-command coreutils suite **12/12 = 100%** (gate >= 80%; POSIX-
PERSONALITY.md 5).
**L7 is done** (the final milestone): **dynamic linking - an *unmodified,
dynamically-linked* glibc binary runs**. `load::load_elf_linux` parses `PT_INTERP`
and, when present, loads BOTH the main program (ET_DYN, 4 GiB bias) and the ELF
interpreter `ld-linux-*.so` (ET_DYN, 64 GiB bias `LINUX_INTERP_BASE`, streamed
from the VFS), starting execution in ld.so with the auxv carrying `AT_BASE` =
interp bias, `AT_PHDR`/`AT_ENTRY` = the main program's. The kernel does **no
relocation processing** - ld.so self-relocates then maps + relocates the program
and `libc.so.6` (initial-exec TLS + IRELATIVE/ifunc relocs run to completion on
all three ISAs). `mmap` gained **file-backed MAP_PRIVATE + MAP_FIXED**
(`kernel/src/linux/mem.rs`): ld.so reserves a library's span then MAP_FIXED-
overlays each segment (text/data from the file, an anonymous **zeroed** overlay
for the bss - the anon path frees+replaces the reservation's file frames, or
libc's stdio/malloc bss locks keep file garbage and self-deadlock), plus
`pread64` and x86-64 legacy `access`. The real per-ISA glibc (`ld-linux` +
`libc.so.6`) is **copied from the cross toolchain at build time** into a
gitignored dir (never committed) and seeded into a ramfs `/lib`; a missing
runtime lib makes that ISA skip-with-reason (static coverage stays). The
`linuxdyn` test runs a stock **dynamically-linked glibc C hello** (gcc default
PIE, unmodified) on **all three ISAs**, exact stdout + exit asserted, **three
ways**: loaded directly (initial-load), `execve`d from a ramfs VFS (the streaming
`execve` path parses `PT_INTERP` and streams the interpreter demand-paged, sharing
`stream_elf_at` with the initial-load path - GOAL-DISK-2), and **`execve`d off a
real ext4 image on a live virtio-blk disk** (GOAL-DISK-2b: mounted via
`ext4fs`/`ext4plus` + the block cache; the program, its `ld.so` and `libc.so.6`
all stream off the disk on demand, none resident whole - 447-590 block-cache fills
per ISA). That is a dynamically-linked glibc binary running unmodified, launched
straight off ext4 - the shape a shell launching Claude Code needs. This closes
"unmodified Linux binaries run" for the common dynamic case; the whole
**L0-L7 Linux personality is complete** - unpatched static and dynamic glibc C,
unpatched Rust `std`, and the real upstream uutils/coreutils all run as cells,
kernel-resident like `svc.rs` and adding no kernel object (`MAX_MAPPED_FILES` is
now **64**, headroom for a production binary's dozen-plus shared libraries).
**Multi-library dynamic linking works** (GOAL-DYN-MULTILIB, #169): a binary
linking a *second* shared library (`dmath`: `libc` + `libm`) now runs, proven by
`linuxdyn`'s multi-library phase on all three ISAs. The defect was never in ld.so
or version resolution - it was the `stat`/`fstat` block reporting `st_ino = 1` for
**every** file (the kernel↔VFS bridge `abi::Stat` dropped the VFS `NodeId`), so
glibc's ld.so, which dedups shared objects by `(st_dev, st_ino)`, treated the
second library as an already-loaded copy of the first and never mapped it. The fix
plumbs the real inode through `abi::Stat.ino` into every Linux `st_ino`/`stx_ino`;
recorded as a scar in docs/ENGINEERING.md 11 ("a field left constant is a field
that lies") and in docs/LINUX-COMPAT.md L7.
**L8 has begun** (docs/LINUX-COMPAT.md L8, docs/NETSTACK.md rheo-net Phase N1d):
**AF_UNIX (Unix domain) sockets** - `socket`/`socketpair`/`bind`/`listen`/
`accept`/`connect`/`sendmsg`/`recvmsg` on SOCK_STREAM, sockets as per-cell fds
whose byte transport reuses the **L6 cross-cell ring** (a connection is two rings,
one per direction) plus a global name registry (`kernel/src/linux/unixsock.rs`) -
no new kernel object, the L6 `pipe2` precedent. The `linuxunix` test runs an
unmodified static-glibc AF_UNIX C fixture (socketpair+fork + bind/listen/connect/
accept over an abstract name) on **all three ISAs**. SCM_RIGHTS fd-passing and
SOCK_DGRAM are documented deferrals.

**The seven measured syscalls are closed** (docs/LINUX-COMPAT.md L8-EVENTFD,
docs/ARCHITECTURE-DEBT.md 4.0 blocker 3): the set the real Claude Code binary was
observed issuing in its startup `strace` and the personality did not dispatch. Six
were advisory; **`eventfd2` was not** - it is the epoll event loop's only wakeup
path, so refusing it removes the mechanism rather than degrading the program. It
lands as a per-cell fd over a per-personality registry
(`kernel/src/linux/eventfd.rs`) - **no new kernel object**, the `epoll`/`pipe`
precedent - with the counter in the **registry, not the descriptor**, because
`dup`/`fork` alias one object and a per-descriptor counter would give two counters
that silently stop waking each other; a zero counter is genuinely not readable, so
`poll`/`epoll` report it unready and a blocking read parks through the pipe's
`Block::EventFdRead` machinery. Alongside it: the **legacy x86-64 `open`** (nr 2),
whose absence refused every `open` on that ISA **and nowhere else** - glibc
prefers it over `openat` there, the same two-numbers trap as `readlink`;
**`sysinfo`** with real frame-pool and cell-clock numbers (Bun sizes its heap from
them, so a zeroed answer is worse than a refusal, and the fields that read 0 are 0
because they are 0 - no page cache, no swap, no load average);
**`sched_setscheduler`** accepting the one policy in force and refusing real-time
`-EPERM` rather than accepting and dropping it; **`close_range`** actually closing
its range; and **`clone3`/`rseq`** refused *deliberately* instead of falling
through the unknown-number log. The `sysx` fixture in `linuxproc` proves it on
**all three ISAs**, asserting each refusal *as* a refusal, with four narrow
reverts each observed failing.

**The real Node.js binary runs unmodified on the OS** (GOAL-NODE,
docs/LINUX-COMPAT.md, the `linuxnode` test): the actual `/opt/node22/bin/node`
(v22, dynamic, 124 MB, V8 + libuv) streams off a live ext4 disk over
virtio-blk-pci (`ext4fs`/`ext4plus` + block cache, ~15k fills, none resident
whole), `ld-linux` links all **seven** shared libraries (glibc + libstdc++ +
libgcc_s), V8 initialises, libuv runs its event loop, and it evaluates
`console.log("rheo:"+(40+2))`, prints exactly `rheo:42`, and **exits 0** on
x86-64 (arm/riscv have no node build and skip). It runs **with V8's JIT enabled**:
the cell is minted the W^X exception capability (ARCHITECTURE.md 5.1) and the run
log shows V8 taking it - `mprotect PROT_WRITE|PROT_EXEC granted`. It first ran
`--jitless`, on the Ignition interpreter, because the exception did not exist yet;
W^X is still structural, since every other kernel in the suite mints nothing of the
sort and is refused exactly as before, which is what makes this a capability rather
than a setting. This is
the production JavaScript runtime Claude Code runs on, executing unmodified via
the Linux personality + POSIX translation. It needed four measured legacy calls
(`gettimeofday` - which libuv *asserts* on -, `clock_getres`, `time`; io_uring
refused deliberately) and, the real blocker, **per-context blocking**: a cell's
proc-level block (`epoll_wait`/`poll`/`nanosleep`/pipe/eventfd/console/`wait4`)
now lives **per execution context** (`thread.rs` `pblock`, judged + completed by
`proc.rs`) rather than parking the whole cell - so Node's main thread can block on
`epoll_wait` for an eventfd a V8 worker must write while the worker keeps running
(before, the whole cell parked and the scheduler correctly reported a deadlock).
When a context blocks the scheduler runs a `Ready` sibling first, then an
already-satisfiable sibling (Node's teardown: main writes the eventfd then
futex-waits for the worker parked on it, which is resumed and `FUTEX_WAKE`s main),
and only parks the whole cell (the pre-existing cross-cell path) when every
context is blocked - a single-context cell falls straight to that path,
byte-for-byte the old behaviour, which is why the whole Linux suite stays green.
**Node now also runs under real timer preemption** (docs/SUBSTRATE.md 15, S3'): the
`linuxnode` boot enables queue-driven dispatch, 30 slices are genuinely taken to
sibling contexts mid-run, and it still prints exactly `rheo:42` and exits 0. That is
the useful half of the preemption proof - a preemption kernel that only ever preempts
a purpose-built spinner has not been tested by anything. Still **single-CPU** (one core,
so preemption is the CPU changing hands, not two contexts running at once); one `poll`
waiter per cell (the copied pollset is per-cell; `epoll`, which Node uses, is
unlimited).

**The real Bun binary EVALUATES JavaScript and exits 0** (GOAL-BUN,
docs/LINUX-COMPAT.md, the `linuxbun` test - a **partial**): the actual
`/root/.bun/bin/bun` (v1.3, dynamic, 99 MB, JavaScriptCore + a Zig runtime) streams
off a live ext4 disk (~3,500 fills), dynamically links its whole library set, and
JSC initialises **including its 128 GiB Gigacage** - a single `MAP_NORESERVE`
reservation the kernel now demand-fills (the Linux mmap window was raised to
80..252 GiB for it, and a failed eager commit no longer leaks a phantom VMA), spawns
a worker via **`clone3`** (now implemented: it decodes `struct clone_args` and routes
to the same context-creation path as `clone`), and sets up its libuv event loop
(`BUN_JSC_useJIT=0`, host-verified zero RWX). It **evaluates
`console.log("rheo:"+(40+2))`, prints exactly `rheo:42`, and exits 0** - with its JIT
enabled (through the capability-gated W^X exception, below) and under preemption. Held to
the same strict gate as Node; no partial is accepted.

**Three wrong diagnoses, and the measurement that ended them.** Worth recording, because
each guess cost a full experiment. (1) **The scheduler**: all 205 of Bun's startup
syscalls came from its main thread and the worker it spawned never got the CPU, so the
cooperative scheduler was blamed and the prediction written down was "when preemption
lands, Bun prints `rheo:42`". Preemption landed, the worker measurably got the CPU (66
preemptions to a sibling context), and Bun aborted *identically with preemption
disabled*. (2) **The JIT**: W^X refused JavaScriptCore's RWX arena, so that was blamed
next; the arena is now granted and Bun aborted at the same point again. (3) After two
eliminations, the honest position was that the cause was unknown.

What ended it was not a fourth guess but **evidence**. The personality now prints the
path of every refused `open` and dumps the last 24 syscalls before a fatal signal; the
trace showed glibc's `abort()` preamble (`rt_sigprocmask`, `gettid`, `getpid`, `tgkill`)
preceded by a run of probes, and the refused-path log named them. **`/proc/self/maps` was
the one that mattered** - JavaScriptCore reads its own memory map, and a JS engine that
cannot find its own mappings cannot proceed. It is now **synthesized from the cell's real
VMA list** (`vma::render_maps` -> `FdKind::ProcMaps`), rendered once at open so a reader
gets one consistent snapshot; every line comes from a record the personality actually
holds, which is why it is generated rather than seeded as a file - a static `maps` would
be a fabricated memory layout, and a runtime reading it to locate its own code would be
misled rather than refused. Alongside it, the disk fixture seeds the handful of `/proc`
and `/sys` values this kernel genuinely has (`overcommit_memory` = 0 - accurate, since
`mmap` reserves and frames arrive on fault - `mmap_min_addr` = 65536, `cgroup` = `0::/`,
cgroup `memory.max`/`memory.high` = `max`). **Two files that were seeded are not any
more**, for the same reason `maps` never was: `cpu/online` was the constant `0-0` and
`/proc/stat` had one `cpu0` line however many cores were up, and both are **synthesized
from `smp::online_count()`** now with the seeded copies deleted so a constant cannot
answer first - libuv sizes its thread pool from the first, and counting the second's
`cpuN` lines is what every libc's `get_nprocs` falls back to. `/proc/stat`'s ten jiffy
fields stay **0**: `sched::dispatch` charges a vcore the ns it ran, aggregated across
CPUs with no user-versus-kernel split, so there is nothing to convert into them, and
splitting the charged time across them would invent a breakdown a reader would compute a
CPU percentage from.

The lesson is the cheapness of the fix relative to the guesses: two large,
correctly-built mechanisms were driven to completion on the strength of a plausible
story, and a one-line "print the path that failed" would have answered it first
(docs/ENGINEERING.md 1 - observe, never infer). Both diagnostics are kept.

Still refused and correctly so: `/etc/localtime` (glibc falls back to UTC, which is right
- there is no timezone database and inventing one is worse than the fallback),
`bunfig.toml`, the `glibc-hwcaps` probes, `trace_marker`, `/proc/self/statm`.

**The real Claude Code binary runs on the OS** (GOAL-CLAUDE, docs/LINUX-COMPAT.md, the
`linuxclaude` test): `/opt/claude-code/bin/claude`, **275 MB**, the workload
docs/ARCHITECTURE-DEBT.md 4.0 measured this tree against and named as the target. It is
a **Bun-compiled single-file executable** - the same JavaScriptCore runtime `linuxbun`
proves, at nearly three times the size with an entire application bundled in - so it
needed no new mechanism at all: it streams off a live ext4 disk over virtio-blk-pci
(~116,000 block-cache fills, none resident whole), demand-pages, links its glibc set
(`librt` on top of bun's), brings up JSC with its **JIT enabled** (the capability-gated
W^X exception), runs **under preemption** (2,467 slices taken to sibling contexts), and
prints exactly `2.1.220 (Claude Code)` with exit 0. Held to the strict gate; asserted on
an exact transcript.

**Honest scope, because "runs Claude Code" invites a bigger reading than the evidence
supports.** It runs `claude --version`, which is what can be asserted deterministically:
it exercises the whole load path, JSC bring-up, the bundled application's startup and its
argument handling, and needs **no network and no credentials**. Driving a *conversation*
would need outbound TLS to an API from inside a cell - the N3b/N5a stack wired into a
cell, which is a networking task rather than anything about running this binary - and is
not claimed. One real defect found on the way: the runtime disk image was a flat 200 MiB
sized for `node`, so a 275 MB binary did not fit, and the failure surfaced as `execve`
refusing at boot with a message that said nothing about disk size; the image is now sized
from the payload.

**timerfd is done** (docs/LINUX-COMPAT.md L8-TIMERFD, GOAL-TIMERFD):
`timerfd_create`/`settime`/`gettime` - the **timer source of libuv**, and thus of
Node.js and the async/JS world. A per-cell fd over a per-personality registry
(`kernel/src/linux/timerfd.rs`) - **no new kernel object**, the `eventfd`/`epoll`
precedent - whose expiry is an ordinary **cell-clock deadline**, the same wait
`nanosleep` (`proc::Block::Timer`) parks on and the same the scheduler already
halts for through the timer arbiter's `CellSleep` slice, so it composes the
existing time machinery and touches no deadline arithmetic. A blocking read parks
on the deadline (no runnable-peer needed - the clock is the wake source, unlike an
eventfd), and for epoll the timerfd's per-fd source is `idle::TIMER` and its
readiness is "expired", so the existing poll/epoll timer-slice idle path wakes the
loop unchanged. One-shot + periodic; `read` returns and consumes the expiration
count; `write` is `-EINVAL`. The `timerx` fixture in `linuxpoll` proves it on
**all three ISAs**: a blocking read parks on a 20 ms one-shot (exactly one
expiration), epoll_wait wakes on a second, and the disarmed timer reads zero.

**A real JavaScript engine runs on the OS** (GOAL-JS, docs/LINUX-COMPAT.md): the
`jsdemo` fixture is the pure-Rust **`boa_engine`** crate (pinned in
`tests/linux-fixtures/jsdemo/Cargo.lock`) - a complete JS runtime (lexer, parser,
bytecode compiler, register VM, heap, GC) built static-glibc and run **unmodified**
under the Linux personality by `linuxrun` on **all three ISAs**. It evaluates real
JavaScript (a function, `Array.reduce`, an arrow closure, string concat) and prints
`js: rheo:42` (exit 0), exercising the L2-L8 syscall surface and demand-paging a
~9.5 MB image (~1580 pages recorded, ~18 frames at load - the loader scales). This
is the on-goal proxy for Node/Claude Code: a genuine language runtime executing on
rheo-os. It is **not** V8/libuv/Node itself (that ~100 MB binary is the remaining
distance) - stated plainly, not overclaimed. The `libuv` event-loop core (epoll
multiplexing timerfd + eventfd + pipe) is separately proven by `uvloop` in
`linuxpoll`.

**librheo** (`librheo/`, docs/LIBRHEO.md) is the greenfield **native userspace
foundation library** - the role a libc plays, rebuilt for this kernel:
async-first, capability-native, built ON `runtime/` (not a POSIX threading
port, and distinct from `libc/`'s C/POSIX compat role). **Phase A** ships the
spine: a `no_std`+`alloc` crate (a loaded ELF cell for the bare targets) with
`mem` (a grow-on-demand global allocator over `runtime::Heap` + `SYS_MMAP`,
which added `Heap::add_region`), `rng` (a ChaCha20 fast-key-erasure DRBG as a
**library call**, seeded once over `SYS_RANDOM` - realizing TIME-IDENTITY.md 4
in a cell), `cap` (capability-typed handles over `runtime::rights` + a
startup `CapSet`), and `rt` (the strand executor + a **userland reactor**:
submit -> `SYS_DOORBELL` -> drain CQ -> `complete(user_data)`, so a strand
parked on a token wakes on its completion). To make a loaded cell actually own
a queue pair, the **queue ABI was redesigned to a stable on-wire layout**
(`kernel/src/queue/mod.rs`): a `repr(C)` `QueueHeader` (version, depth, the four
ring indices at fixed offsets, sq/cq offsets) followed by the SQ/CQ arrays, with
`QueuePair` now an overlay over a single region (`init`/`attach`) rather than
head/tail atomics inside the Rust struct - unified, so bench-core/queue-pipeline/
runtime/isolation-hw stay green. The loader gained `map_queue` (maps the ring at
**16 GiB** = `USER_QUEUE_VA` into a loaded cell, mints a `QueuePair` cap, records
`(qp_va, cap_id)`), `submit` carries args + `reap` returns the full `CqEntry` +
`SYS_DOORBELL` returns the processed count, and **`SYS_QUEUE_INFO` (31)** reports
`(qp_va, cap_id)` to the cell. The `librhearun` test loads `librheo-demo` into a
cell with a real mapped queue pair and asserts it exits `0x42` - reached only if
heap+rng+cap and an **async queue round-trip** (8 strands each `submit_and_await`
an `OP_ECHO` and verify the echo) all pass, on **all three ISAs**.
**Phase B** makes librheo the **terabytes / analytical-DB / warehouse**
substrate: **typed memory grants exposed to a cell** (`SYS_GRANT`/`COMMIT`/
`DECOMMIT`/`SEAL`/`MMAP_FILE`/`MUNMAP` - object 5 as mechanism: reserve typed
address space + mint a MemoryGrant cap, demand-commit frames without a fault
handler, seal immutable, mmap a VFS file, and a real unmap that fixes the anon-
`SYS_MMAP` frame leak; DDR always real and **PMEM real where a QEMU nvdimm is
exposed** (Phase J: x86-64 q35 via the ACPI NFIT + a separate pmem allocator,
distinct from the DDR pool; arm/riscv `virt` expose no nvdimm so PMEM
skips-with-reason to DDR - docs/MEMORY.md 2.1), HBM/CXL/Remote emulated-as-DDR,
device-BAR refused, and the **NUMA node hint now real** (see the NUMA paragraph
below) - all honest/documented, per-cell grant
tables as fixed statics, every commit/decommit/seal grant-checked), and **real
async I/O opcodes
over the queue** (`OP_OPEN`/`READ`/`WRITE`/`CLOSE`/`FSTAT` bridged to
`kernel_process` via `svc::FileOps`, completing through the CQ with the strand
token; per-opcode rights replacing hardcoded-WRITE; distinct CapError statuses;
IO.md contract flags with the inline-vs-by-reference threshold - small writes
inline, larger I/O by-reference and **zero-copy** straight into the cell's mapped
pages). librheo gained `mem` (`Grant`/`Arena`/`Mapping` + NUMA hint), `io` (async
`File`/`read_at`/`write_at`/batched/`Contract`/`Stream`, zero-copy `read_into` a
grant), and a thin `store` (`Dataset`). The `librheodata` test is the **mini-
DuckDB proof**: a columnar dataset (65536 rows x 2 u32 cols, generated by xtask,
never committed) on a **live virtio-blk disk** is served to a librheo cell, which
mmaps it zero-copy, fans a `SUM`/`COUNT`/`MAX`-under-predicate columnar scan
across 8 strands, and asserts the exact aggregate (`SUM=1073741824`), on **all
three ISAs**.

**Phase C** makes librheo the **parallel / accelerated compute + QoS** substrate:
**userspace-built dependency graphs submitted to the compute engine** (a new
`OP_GRAPH_SUBMIT` queue opcode - a cell writes an `abi::GraphNode` list into its
own buffer, the kernel validates the edges topologically, runs it on the CPU
engine, and writes per-node results back, completing through the CQ with the
strand token - objects 4/6 as mechanism), **engine introspection**
(`SYS_ENGINE_INFO` surfaces the throughput measured at attach - attest-by-
measurement), and **admission-checked reservations** (`SYS_RESERVE_ADMIT`/`QUERY`/
`RELEASE` - the per-cell EDF schedulability math in `sched.rs` admits CPU
budget/period/deadline + an advisory memory floor, mints a Reservation capability,
and refuses an over-committed set cleanly; object 7). librheo gained `compute`
(`map_reduce`/`parallel_for`/`scan` strand workers, `Engine::info`, and a
`GraphBuilder` that builds + `submit().await`s a graph) and `sched` (`Reservation`
RAII handle with typed rejections, plus a `lattice-rt`-shaped `Priority`/
`PeriodicTask`/`TimingReport`). The `librheocompute` test proves it on **all three
ISAs**: a parallel `map_reduce` aggregation (`SUM=4194304`), a submitted graph
(`(6+1)*6=42`), a reservation admitted (committed ppm > 0) then two cleanly
rejected (bad-params + over-commit), and the engine's measured throughput
reported. Honest: the admission **math** is real and enforced at admit, but
runtime enforcement is SMP/preemption-gated (task #27) - the runtime is
single-CPU cooperative, so parallel strands interleave and a reservation is an
admitted, not-yet-scheduled guarantee; the CPU engine is the only real engine
(GPU/NPU accelerators ride the same graph/engine API as attested-firmware future
work); a free-form graph's buffer-reduce node is the documented next step.

**Phase D** brings up the **kernel's first hardware interrupt** and the terminal.
A native cell blocking on console input (`SYS_WAIT_INPUT`) parks, and the kernel
idles until a byte arrives instead of spinning - the OS's **first block-and-wake**.
Bytes land in a portable kernel-side **RX ring** (`kernel/src/input.rs`); the
reactor gains a console-read slot (`rt::read_console`) that parks a strand and, when
no queue completion is ready, blocks in `SYS_WAIT_INPUT`. The interrupt path is
boot-critical and opt-in (`arch::enable_uart_rx_irq`, called only by the Phase D
test, so the other kernels are untouched): **interrupt-driven on RISC-V and
ARM64**. RISC-V routes the 16550 UART's IRQ (source 10) through the **AIA** (S-mode
APLIC in MSI mode -> S-mode IMSIC via the `siselect`/`sireg`/`stopei` CSRs ->
`sip.SEIP`) and takes the S external interrupt to drain the UART, halting at `wfi`
(cells run with `sstatus.SIE` clear; SIE set only to service a pending SEI after
`wfi` woke on it). ARM64 routes the PL011 UART's IRQ (SPI 33) through the **GICv3**
(GICD + the boot CPU's GICR, CPU interface via `ICC_*_EL1`) and takes it in the
current-EL-SPx vector slot, draining the byte and EOI'ing via `ICC_EOIR1_EL1`;
cells run at EL0 with IRQ masked and the kernel unmasks (`daifclr`) only after
`wfi`. Both are genuine 0%-CPU parks. **x86-64 was a poll** here - under QEMU's TCG the
LAPIC ISR/IRR looked unmodelled and IOAPIC-routed lines looked not to re-deliver, so
faking it would have been dishonest - but **SMP phase 1 disproved that** (every one of
those observations had been made through the inert x2APIC MSR block, where a missing
EOI genuinely makes the first interrupt the last): x86-64 now routes COM1's ISA IRQ 4
through the **IO-APIC** to vector 0x21 and halts at `hlt`, verified end to end at
bring-up, so UART RX is interrupt-driven on all three ISAs (docs/SMP.md 8). On top of the raw byte
substrate, librheo gained **`term`** - the
byte-stream terminal discipline: `input` (a decoder: CSI/SS3 escape sequences ->
typed `Key`s, UTF-8, control chars, async `next_key().await`), `edit` (a line
editor with insertion, cursor moves, word/line kill, history recall, completion
hook), and `render` (a buffered, minimal-diff renderer, batched writes). The
`librheoterm` test drives a read-eval loop with scripted keystrokes (typing,
backspace, cursor-left + insert, an arrow-key escape, Up-arrow history) and asserts
the exact committed lines + exit on **all three ISAs**, plus the idle-park (kernel
idled at `wfi`/`hlt`) on **all three**. Honest: RISC-V and ARM64 each carry a
device-loopback caveat (QEMU's 16550/PL011 loopback
does not drive the interrupt-controller line, so the deterministic test raises the
controller line directly - RISC-V the IMSIC MSI, ARM64 `GICD_ISPENDR` for SPI 33 -
exactly the interrupt the device would raise; the byte is genuinely delivered and a
genuine interrupt genuinely wakes `wfi`, docs/LIBRHEO.md Phase D). This is "wake on
input", not preemptive scheduling
(SMP/#27). The scripted-byte path also carries a **verified delivery check**:
`input::pump` used to return "there is data" on the strength of *having halted*, and
a halt ends on any enabled interrupt - so once the timer one-shot became real on
every ISA a competing deadline could end it with the UART handler never having run,
which showed up as an intermittently failing `schedidle` (docs/ENGINEERING.md 11).
It now checks the ring, recovers the byte from the UART FIFO if the interrupt did
not deliver it, and pushes it directly with a printed reason if the wire has nothing
either - with per-tier counters, so a degraded interrupt path is reported rather
than inferred away (all three ISAs report zero recoveries).

**Phase E** makes librheo the substrate for **services and a Wayland-class
compositor**: two cells share a **typed cross-cell queue pair** and pass
ownership of a **sealed buffer grant** (zero-copy), with a **flip/present
completion**. Two kernel additions, both **exposing** existing objects (no new
object): **`SYS_CONNECT` (43)** reports a cell's shared-channel end
(`ChannelInfo { chan_va, cap_id, role }`) - a cross-cell channel is **one ring
region mapped into two cells** at 24 GiB (`load::alloc_channel` +
`map_channel_into`), whose header the kernel writes once but **never drains**:
the two cells drive the SPSC rings directly over the shared frames (IO.md 6, the
queue object 3), so a message is a pure shared-memory write. **`SYS_GRANT_SHARE`
(44)** delegates a **sealed** grant to the peer cell (objects 2/5): the kernel
maps its frames **read-only** into the peer (`AddressSpace::share_ro_into`, over
the existing per-ISA leaf walk), mints a MemoryGrant cap there on the **same**
object (epoch-revocable), and reports `ShareInfo { peer_va, peer_cap_id }` -
**zero-copy shared memory, the dmabuf equivalent** (requires DELEGATE + sealed;
`SYS_GRANT` now mints `READ|WRITE|MAP|DELEGATE`). librheo gained **`ipc`**
(`Channel` - a typed cross-cell queue pair with client `send`/`await_completion`
+ server `recv`/`complete` over `SqEntry`/`CqEntry`, the cooperative
`switch_to_peer` hand-off, and `share`/`recv_buffer` for zero-copy buffer
passing) and **`display`** (`Surface` - a client drawable backed by a sealed
buffer grant, `commit` seals+delegates+sends+awaits the flip; `Compositor` - an
in-memory framebuffer, `present` composites the shared buffer + replies with the
flip completion; `InputEvent` reuses the Phase D `term::input::Key` as the HID
event-stream shape). The `librheowl` test runs **one binary as two cells** (the
test kernel wires both + the shared channel + roles): the client draws a 64x64
buffer, seals + shares it, commits a frame; the compositor maps the shared grant
read-only (zero-copy - same frames), composites it, and returns a flip completion
carrying its checksum; the client asserts that checksum **equals its own known
value** (`0x3eb4f800`) - proving zero-copy cross-cell sharing - and exits `0x42`
on **all three ISAs**. Honest: zero-copy is real (the only copy is the
compositor's own composite into its framebuffer); the synchronous channel is an
explicit peer hand-off (the compositor uses it); a **fully-symmetric async
`Sender`/`Receiver` parking on the reactor** is now done as **Phase J** (below);
spawn-driven connect is Phase F; real
GPU (virtio-gpu scanout) is deferred - the mechanism (shared sealed buffer + typed
present queue + flip completion + input-event shape) is the deliverable.

A **Phase J polish trio** refines librheo Phases E/F (additive userspace, no new
kernel object). **(1) Symmetric async IPC**: `ipc::Channel::split()` yields an
async `AsyncSender`/`AsyncReceiver` that **park on the strand reactor** (a channel
slot in `rt`, mirroring the console/timer/wait slots) instead of spinning on
`switch_to_peer`. A strand that `recv`s parks, the vcore runs the cell's other
strands, and only when all have parked does `block_on` hand the CPU to the peer
(`SYS_SWITCH`) and deliver the message. The in-cell wait is a genuine reactor park;
the cell-boundary hand-off stays a cooperative switch under the single-CPU model (a
truly parallel producer/consumer awaits SMP #27) - honest. The `librheoipc` test
runs one binary as two cells that **ping-pong 8 typed messages** over the async
Sender/Receiver; the consumer asserts the exact sequence **and**
`rt::chan_wakeups() == 8` (every message a genuine reactor park+wake, not a spin),
exiting `0x42` on **all three ISAs**. **(2) Cross-cell stdout pipelines**:
**`SYS_SPAWN` now propagates the parent's channel to the spawned child** (maps the
same frames RW via `AddressSpace::share_rw_into`, mints a channel cap into the
shared bundle, records the child's end with the opposite role - no new kernel
object, it composes Cell + QueuePair). `proc::spawn_piped` returns a `Pipe {
child, tx, rx }`: the child streams its output over the async channel (not through
the kernel), the parent reads it with `rx`, then reaps the child. Honest: the pipe
connects a spawned child to its **parent** (a valid `cur^1` pair; the child frees
the shared frames on exit after the parent drained them); two sibling spawned
stages await a directed switch/SMP (#27). The `librheopipe` test spawns
`/bin/pipesrc`, which streams `"ABCDEFGHIJKL"` back over the inherited channel; the
orchestrator reconstructs+verifies it and exits `0x42` on **all three ISAs**.
**(3) The full `term` line editor in `lrsh`**: the librheo-native shell now drives
the Phase D editor - `KeyReader` (parking on input) + `LineEditor` (in-line cursor
edits, backspace, word/line kill, **Up/Down history**, a **Tab command-name
completion hook**) + the buffered `Renderer` - instead of a raw line read; committed
lines still run builtins or spawn `/bin/<cmd>`. The `librheoproc` shell scenario
feeds scripted keystrokes (typing, a backspace edit `child 9`->`child 8`, Up-arrow
history recall, `ec`<Tab>->`echo`) and asserts the committed-command evidence
(`child 8` ran twice, no `child 9` command, completion produced `echo`) + exit
`0x42` on **all three ISAs**.

**Phase F** closes librheo as a **complete foundation**: a native **process
model**, **time**, a **librheo-native shell**, an **embedded** proof, and honest
benchmarks. Three kernel additions **expose** the Cell object (1) / arm-timer verb
(no new object; per-cell synthesized state in `kernel/src/nproc.rs`, mirroring
`linux::proc` for `Personality::Native` cells). **`SYS_SPAWN` (45)** - gated by a
**cell-spawn capability** (`ObjectKind::Cell` + WRITE - no ambient authority) -
streams an ELF from the VFS into a **new** native cell with its own address space +
queue pair (sharing the parent's cap bundle like `fork`), builds its SysV stack
from the caller's argv/envp, and returns a child handle; **`SYS_WAIT` (46)** blocks
the parent cooperatively (generalizing the L6 cross-cell run loop), runs the child,
and reaps its exit code (a faulted native child is reaped with `FAULT_EXIT`=139 -
native cells have no signals); **`SYS_ARM_TIMER` (47)** is a one-shot deadline,
now the OS's **second interrupt**, **interrupt-driven on all three ISAs** (the
kernel arms the per-ISA timer and halts at `wfi`/`hlt` until it fires - a genuine
0%-CPU park: RISC-V Sstc `stimecmp`, ARM64 CNTV virtual timer via the GICv3,
x86-64 the LAPIC one-shot; opt-in via `arch::enable_timer_irq`, with a cooperative
deadline-check fallback where not wired). x86-64 took the long way: its LAPIC
one-shot was claimed, rheo-net N2h made bring-up **verify** it and found QEMU 8.2
TCG reports no x2APIC (leaving that MSR block inert) so it fell back honestly, and
**SMP phase 1** then fixed the capability by driving the LAPIC over **xAPIC MMIO**
(docs/SMP.md 5).
librheo gained **`proc`** (`spawn`/`Child::wait().await`/`args`/`env`/`identity`),
**`time`** (monotonic `Instant`/`now` + async `sleep`/`timeout`/`interval` over the
reactor's timer slot), and a **`net`** stub (deferred - networking is a service).
It is **feature-gated**: `default=["full"]`; an **embedded** cell builds
`--no-default-features` (spine only: cap/rt/mem/sys) - `librheo-embed` does a direct
queue round-trip and is **~9x smaller** loadable than a full binary. **`lrsh`** is
the librheo-native shell (builtins + `spawn`/`wait` of native coreutils over the
Phase D console path; **Phase J** wires in the full `term` line editor - see
below). The `librheoproc` test proves it on **all three ISAs**: an
orchestrator spawns `/bin/echo` + three `/bin/child` cells (argv fan-out), reduces
exit codes to 12, and a `time::sleep` wakes on the timer (asserting a genuine
`wfi`/`hlt` idle-park on all three ISAs since SMP phase 1); `lrsh` runs a scripted keystroke
session through the term editor (committed-command evidence + exit `0x42`); and the spine-only `librheo-embed`
round-trips. Benchmarks (icount, per TOOLING.md): full async round-trip ~1,433
(x86-64) / ~2,048 (riscv64) instructions, spawn+wait ~263k (x86-64) / ~539k
(riscv64) - process create is dominated by ELF stream-load + child crt0, the honest
price of a new address space. Honest deferrals: the `net` stack -
docs/LIBRHEO.md has the full A-F accounting. (The **x86-64 timer** was a deferral
here until SMP phase 1 gave it a real LAPIC over xAPIC MMIO; see the SMP section at
the end for the x86-64 UART RX outcome.) **librheo A-F is complete.**

**Phase G** turns the Phase F `net` stub into the real **NIC data path - raw
Ethernet frames over a virtio-net driver** (docs/NETWORKING.md, LIBRHEO.md Phase
G); the IP/TCP/QUIC stack stays a **service**, deferred. A hand-written
**virtio-net driver** (`kernel/src/hw/virtio_net.rs`) mirrors virtio-blk over the
**two transports** - virtio-mmio on arm/riscv `virt`, virtio-pci on x86-64 q35
(via the `VIRTIO_PCI_CAP_PCI_CFG` config tunnel, no BAR mapping) - with reset +
**minimal** feature negotiation (`VIRTIO_F_VERSION_1` + `VIRTIO_NET_F_MAC`; no
mergeable-rx-buffers or checksum/GSO offload), an **RX** and a **TX** split
virtqueue, the 12-byte v1 `virtio_net_hdr`, and the MAC from device config; DMA
uses **physical** addresses (`virt_to_phys`), polled (a device RX IRQ is a later
refinement). Three **queue opcodes** (`OP_NET_TX`/`OP_NET_RX`/`OP_NET_MAC`, no new
kernel object) bridge a cell's async submissions to the driver in `kernel_process`,
completing with the strand token - the Phase B `io` model. librheo's **`net`** is
now real: `mac`/`send`/`recv` of raw frames (`connect`/`listen` stay `Unsupported`
- IP/TCP is a service). The `librheonet` test proves it on **all three ISAs**: a
librheo cell reads the NIC MAC, sends a **broadcast ARP request** for the SLIRP
gateway `10.0.2.2`, and **receives SLIRP's ARP reply** (a deterministic,
network-free RX proof over QEMU `-netdev user`), asserting ethertype + opcode +
sender IP and exiting `0x42`. Deferred: the full transport stack (IP/TCP/QUIC/TLS
in a cell), a socket `ObjectKind` + steering grants, header/payload split, and the
device RX interrupt. **librheo A-G is complete.**

**Phase H** brings up a **real GPU: a virtio-gpu 2D driver wired to the Phase E
compositor** (docs/DISPLAY.md, LIBRHEO.md Phase H); VIRGL/3D and the full display
pipeline stay deferred. A hand-written **virtio-gpu 2D driver**
(`kernel/src/hw/virtio_gpu.rs`, the plain 2D / VIRGL-off subset of virtio spec
5.7) mirrors virtio-net/blk over the **two transports** - virtio-mmio on
arm/riscv `virt`, virtio-pci on x86-64 q35 (via the `VIRTIO_PCI_CAP_PCI_CFG`
config tunnel, no BAR mapping) - with reset + **minimal** feature negotiation
(`VIRTIO_F_VERSION_1` only; no VIRGL/EDID), a single **controlq**, and every 2D
command a `virtio_gpu_ctrl_hdr` + body submitted as a **2-descriptor chain**
(`[readable command][writable response]`, the virtio-blk request/status shape)
polled for its `RESP_OK_*` code. Bring-up: `GET_DISPLAY_INFO` -> `CREATE_2D`
(resource 1, `B8G8R8A8_UNORM`, **128x128**) -> `ATTACH_BACKING` (a kernel-side
framebuffer of **16 frame-pool frames**, one `virtio_gpu_mem_entry` per frame, so
no contiguous alloc is needed) -> `SET_SCANOUT` (scanout 0); a present is
`TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`. All rings/buffers/framebuffer come from
the frame pool (only static is a small `Option<VirtioGpu>`); DMA by **physical**
address (`virt_to_phys`). One **queue opcode** (`OP_GPU_PRESENT`, no new kernel
object - it extends the queue object with a mechanism) bridges a cell's async
present to the driver in `kernel_process`, copying the cell's framebuffer into the
resource then transfer+flush. librheo's **`display`** gained `Gpu`/`Scanout` (draw
into a framebuffer grant, `present().await` to the real device); the Phase E
in-memory `Compositor` is unchanged (still proves zero-copy). The `librheogpu`
test proves it on **all three ISAs**: a librheo cell draws a known 128x128 RGBA
frame and presents it, exiting `0x42`. QEMU runs **headless** (`-display none`),
so - like virtio-net's network-free ARP proof - the proof is the **genuine driver
round-trip**: **all six 2D commands return OK** from the real device model
(`GET_DISPLAY_INFO` reports QEMU's default 1280x800 even headless; then create-2d,
attach, set-scanout, transfer, flush). No claim of visible output; the 2D scanout
command round-trip + compositor present wiring is the deliverable. **librheo A-H
is complete.**

**rheo-net N2d** (docs/NETSTACK.md 16) makes the network **receive** side as async
as the send side - the OS's **third interrupt source**. Before it,
`librheo::net::recv` was a re-poll (`OP_NET_RX` returned "nothing available" and the
cell submitted it again), and the reactor had no network slot, so a cell waiting for
a packet **spun a core**. Three pieces: **`SYS_WAIT_NET` (48)** - a park-until-frame
verb in the shape of `SYS_WAIT_INPUT` (`kernel/src/net_rx.rs`, portable), mechanism
only, **no new kernel object** (it exposes the same virtio-net driver the `OP_NET_*`
opcodes bridge to), taking a `timeout_ns` deadline so a transport can wait for "a
frame **or** the RTO, whichever comes first" (the per-ISA timer gained
`timer_arm`/`timer_expired`/`timer_disarm`; N2h below took ownership of them);
a **NIC RX interrupt**, wired from the virtio-mmio slot the driver records
(`arch::enable_virtio_net_irq`, opt-in like the Phase D UART IRQ, so the other 47
kernels boot unchanged) - the handler ACKs the device
(`InterruptStatus`/`InterruptACK`) and counts the arrival, the TX ring now sets
`VRING_AVAIL_F_NO_INTERRUPT` (transmit stays polled), and on RISC-V
`riscv_user_trap` now services a device interrupt taken **in U-mode** (S-mode
interrupts are always enabled there) instead of reading it as a fault; and a
**reactor network slot** (`net_rx_req` beside console/timer/wait/channel) so
`net::recv`/`recv_timeout` **park** and wake on the frame, with `net::try_recv`
keeping the non-blocking drain a batching transport needs. The kernel's RX ring
*is* the receive virtqueue's 16 pre-posted device-DMA'd buffers (a second ring would
only add a copy - documented). The `netwait` test proves it on **all three ISAs**: a
cell parks on `net::recv`, is woken by SLIRP's **ARP reply**, parks again for a **TCP
reset**, and finally parks with a 20 ms deadline on an empty queue; asserted are one
reactor wakeup **per** receive (`rt::net_wakeups()` - one park + one wake, never N
re-polls), a witness strand that ran **while** the receiver was parked, and
kernel-side `net_rx::irq_count() > 0` + `did_idle()` on the interrupt-driven ISAs.
Per-ISA honesty (the wait has **three modes**, chosen by portable logic over the
`arch` predicates - `net_rx::IdleMode`): **riscv64** (APLIC-S source `1+slot` in MSI
mode -> IMSIC -> `sip.SEIP`) and **aarch64** (GICv3 SPI `16+slot`) are genuinely
**NIC-interrupt-driven** and halt at `wfi` - a real 0%-CPU park. **x86-64 has no NIC
RX interrupt** (its NIC is virtio-*pci* driven through the `VIRTIO_PCI_CAP_PCI_CFG`
tunnel with no BAR assigned to hold an MSI-X table; the other half of the old
justification - "legacy INTx rides the QEMU-TCG IOAPIC path that does not re-deliver" -
was **disproved** by SMP phase 1, which drives the UART RX line through exactly that
path, docs/SMP.md 8); its wait was documented as a **timer-backed
idle** borrowing the LAPIC (poll, `hlt` for a 500 us slice, re-poll), and **N2h verified
that claim and it did not hold** - QEMU 8.2 TCG reports no x2APIC, so the LAPIC MSR
block was inert and x86-64 was honestly `IdleMode::Poll`, a spin; **SMP phase 1** then
made the LAPIC real over xAPIC MMIO, so x86-64 is back in `TimerIdle` with the halts
**measured** (21 on a bounded wait, 253 in the escalation phase). `interrupt_driven()` reports
**false** wherever no NIC line exists, so a timer wake is never dressed up as a NIC
interrupt. `timeout_ns` is a
**monotonic deadline in every mode** - `POLL_BUDGET` is only a backstop for an
indefinite wait on the last-resort poll path and can never truncate a caller's timeout.
MSI-X through the config tunnel, interrupt coalescing, and zero-copy receive are the
documented next steps.

**rheo-net N2h** (docs/NETSTACK.md 16, the Phase N2h section) removes a **real
production defect** in that wait path and replaces its one magic constant, all internal
mechanism - **no new kernel object, no new verb, no new dependency**. Every ISA has
exactly **one** hardware one-shot, and two subsystems armed it *directly*:
`net_rx::wait_frame` (the receive deadline + the poll slices) and `time::arm_timer`
(`SYS_ARM_TIMER`, every cell's `sleep`/`timeout`). Last-armer-wins, and each
**disarmed the timer on its way out** - so the inner requester's completion destroyed
the outer requester's deadline *and* `arch::timer_expired()` then reported that
deadline elapsed (a lost deadline **and** a false expiry). Latent only because the OS is
single-CPU cooperative; fatal the moment BBR paces continuously beside a TCP RTO. The
fix is a **kernel timer arbiter** (`kernel/src/ktimer.rs`, portable, allocation-free): a
fixed 5-slot table (`RxPoll`, `RxDeadline`, `CellSleep`, `NetTimer`, and `Pacer`
**reserved for BBR**) with `register`/`cancel`/`expired`/`service`/`park`, which arms
the hardware for the **nearest** deadline only, marks **every** due client on a firing,
and re-arms the nearest **remaining** deadline instead of disarming (`preserved()`
counts exactly the deadlines the old code lost). Deadlines are monotonic ns in the
timer's **own** domain (a new `arch::timer_now_ns()` - on RISC-V the `time` CSR, not the
instruction counter), and the arbiter works with no timer at all (pure software
comparison, which the bounded-poll path needs). The **single-owner invariant** - the
arbiter is the only kernel caller of `arch::timer_arm`/`expired`/`disarm`/`park` - is
enforced by construction: the old `arch::timer_wait` arm-wait-disarm helper is **gone**
from all three ISAs, replaced by `arch::timer_park()` (halt once, no arming). Second,
the fixed 500 us receive slice becomes an **adaptive NAPI-style escalation**: **hot** (a
bounded busy-poll after recent activity - a frame, an interrupt, or a **transmit**),
**warm** (short slices), **cold** (long slices, or an indefinite park where the NIC
interrupt exists), with per-profile constants mirroring the `net` crate's
`hft`/`edge`/`warehouse`/`embedded` features (`net_rx::set_profile`, default `Edge`);
the slice is the single shared `RxPoll` slot, so N waiters can never become N wakeups.
New counters (`spin_polls`/`timer_slices`/`halts`/`escalations`/`tier`, and the
arbiter's `arms`/`firings`/`parks`/`preserved`) make the duty cycle **measured, not
claimed** - and `did_idle()` is now set only when a park genuinely halted, which is what
exposed the x86-64 LAPIC above (and a `librheo-orch` 4 us sleep that could never reach a
park - now 2 ms and genuinely parking). The `netwait` test proves it on **all three
ISAs**: the pre-N2h pattern **reproduced** as a false expiry (skipped-with-reason where
no verified one-shot exists - which was x86-64 until SMP phase 1, and is now no ISA),
three concurrent arbiter deadlines each honoured at their
own time in order with none lost, a 30 ms cell sleep surviving a full 5 ms receive wait,
and the escalation law asserted both as a pure function and as observed counters (4096
spin polls then 253-331 genuine timer-slice halts on every ISA) - with the
riscv/arm NIC-interrupt assertions unchanged.

**rheo-net N4a** (docs/NETSTACK.md 17) is the **network service cell + concurrent
fan-out** - the keystone every remaining network scenario rides on (app-protocol
servers, the remote-INET bridge for Linux binaries = **N4b**, onion routing,
DHCP/zeroconf/NTP). Doctrine puts the stack in userspace, so a long-lived **service
cell** must serve **many** client cells; Phase E only proved one-to-one. Two hard
blockers had to go: a cell held **exactly one** channel end (three spawned children
would all inherit the *same* ring - a race, not a fan-out), and `SYS_SWITCH` is a
*directed* `cur^1` hand-off (from client cell 2 it reaches cell 3, never the service
at cell 0 - a livelock). N4a takes the **minimal mechanism extension** of the existing
spawn/channel path - **no new kernel object**, everything composing Cell (object 1)
with QueuePair (object 3), exactly the L6-pipe / Phase-J precedents: a **per-cell
channel table** (`MAX_CELL_CHANNELS` = 4, a fixed static array - the kernel stays
allocation-free), each slot its own ring region at `channel_slot_va(slot)`, with
`SYS_CONNECT(out, slot)` gaining the slot argument + a `count`; **`SYS_SPAWN` gaining
a `chan_spec`** (`SPAWN_CHAN_SLOT | slot << 8` = hand the child the caller's slot
`slot`, always landing at the child's **own slot 0** so a client binary is
slot-agnostic; `chan_spec` 0 is the byte-for-byte Phase J default, and
`AddressSpace::share_rw_into` gained the matching `dst_base`); and **`SYS_YIELD`
(49)** - hand the CPU to the next runnable native cell, round-robin, the caller
staying runnable. `SYS_YIELD` is the same cooperative cross-cell scheduler
`SYS_WAIT`/child-exit already drive (`kernel/src/nproc.rs`) exposed as a plain yield,
transfers no authority (one capability bundle), and **degenerates to `cur^1`** where
the caller has no native process tree, so the Phase E/J two-cell path is unchanged;
the reactor's channel idle path now uses it. On top, `net::service` ships the
framework: a **`Service`** binding one channel end per client and running **one
strand per client** (each parked on its own `AsyncReceiver`; the reactor scans slots
in order, which is what round-robins them), a thin **`Client`** for the spawned cell,
and a word-wide protocol (`Request { op, client, seq }` packed in the message tag,
argument/result in its `u32`): `OP_ECHO` (a per-client keyed transform), `OP_RESOLVE`
(a catalogue name id -> an IPv4, answered from the **network-free** tiers of
`net::dns` - a `HostsTable` + a TTL `Cache`), `OP_BYE`. librheo gained per-slot
reactor channels (`attach_channel_slot`, `chan_send_on`/`chan_recv_on`),
`ipc::Channel::open_slot`, `proc::spawn_on_channel`, and two witnesses -
`rt::chan_wakeups_on(slot)` + `rt::chan_max_pending()` (over a new **non-destructive**
`Qp::sq_pending`/`cq_pending` peek). The `netservice` test proves it on **all three
ISAs**: one service cell serves **three** client cells, each over its **own** channel,
and exits `0x42` only if every client got its **distinct correct** response (each
predicts its echo + its own name `10.1.1.1`/`10.2.2.2`/`10.3.3.3` and exits `id+1`,
codes asserted `1,2,3` - so each was really reaped), `served == [4,3,3]`, the
**interleave witness** `order == [0,1,2, 0,1,2, 0,1,2, 0]` (strand k reaches round r
only after strands `0..k` did - no strand monopolised the vcore), the **in-flight
witness** `max_in_flight == 3` (all three requests queued at the same instant, before
the first reply), and `wakeups == [4,3,3]` (one genuine reactor park+wake per message,
never a spin). The core is **deterministic and network-free**; a **bonus live** ARP for
the SLIRP gateway, performed *inside client 0's serving strand* (parking on the wire
per N2d while siblings run), resolves on all three ISAs and **degrades honestly** to
`REPLY_NONE` with no NIC. Honest: **concurrent, not parallel** (one CPU, cooperative -
a service strand cannot compute while a client computes; SMP is task #27); fan-out is
**parent-shaped** (the service spawns its clients - a **name-based rendezvous** for
unrelated cells is the genuinely new capability and a documented follow-on); the
protocol is word-wide (bigger requests ride a shared sealed grant, the Phase E
mechanism); and 4 clients is the fixed-array ceiling.

**rheo-net N5a** (docs/NETSTACK.md 19) adds the first **application protocols** -
**HTTP/1.1 + HTTP/2, client and server** - the gateway most remaining scenarios ride
on (WAF/DPI inspects HTTP, S3-style storage *is* HTTP, Arrow Flight is gRPC over
HTTP/2). Both live in the crate's **always-compiled** half (HTTP is parsing plus
synchronous state machines, so it needs neither librheo nor the NIC and links in
either posture); **no kernel change, no new object/verb, no new dependency, no
`cfg(target_arch)`**. **`net::http1`** parses **zero-copy** - method, target, reason
phrase and every header name/value are `&[u8]` slices *of the caller's buffer* (the
rheo-json `Cow::Borrowed` discipline; case-insensitivity applied at compare time), so
a WAF datapath classifies a request without allocating per header; the owned
`OwnedRequest`/`OwnedResponse` the `Client`/`Server` helpers return are the *only*
copy. Framing covers `Content-Length` + **chunked** both directions, bodiless
statuses, and the RFC 9112 persistence rule. **Request smuggling is a parser
property, not a filter**: `Content-Length` *and* `Transfer-Encoding` (either order),
duplicate `Content-Length` (even when equal), `5, 5` / `+5` / `0x5`, a non-`chunked`
final coding, bare LF, `Host : x`, obs-fold, non-token names, control bytes in
values, oversized/too-many headers, a double space in the request line and a
non-1.x version are each rejected **with their own error** - 22 shapes asserted,
plus 4 chunked-framing rejections (a non-empty trailer is refused, not dropped).
`http1::scan` reuses the `json/src/scan.rs` idiom - scalar oracle + a branchless
wide path + fuzz equivalence - but the wide path is **SWAR** (`u64` load/compare/
mask/ctz, 8 bytes per step) rather than SSE2, so it stays portable with no
`cfg(target_arch)`; a target-specific SIMD kernel is deferred. **`net::http2`** ships
the frame layer (DATA/HEADERS/SETTINGS/WINDOW_UPDATE/PING/RST_STREAM/GOAWAY/
CONTINUATION, PRIORITY parsed-and-ignored), the preface, the stream state machine,
**connection- and stream-level flow control** (a send is bounded by
`min(conn, stream, max_frame)`, the remainder queued until a WINDOW_UPDATE credits
it; the *connection* window correctly starts at 65535 and is changed only by
WINDOW_UPDATE, never by `SETTINGS_INITIAL_WINDOW_SIZE`), and **HPACK** - static
table, dynamic table with size updates, and the RFC 7541 Appendix B **Huffman code
generated mechanically from the authoritative RFC text** (blob-hash cross-checked;
the generator asserted prefix-freedom + canonicality, which is what lets the decoder
be a per-length canonical lookup rather than a tree), with the padding/EOS rules
enforced. `conn` has the **same synchronous seam as `net::tcp`** (`on_bytes` /
`take_out` / `next_event`, no I/O inside), which is what makes h2 provable with no
live peer and transport-agnostic. For h2 over TLS, N5a added a **minimal RFC 7301
ALPN** to the N3b handshake (offer in ClientHello, selection echoed in
EncryptedExtensions, both sides must agree) - with an empty list the ClientHello is
byte-for-byte N3b's, so the RFC 8448 KAT is untouched; `h2` is therefore
**negotiated, not assumed**. The `nethttp` test kernel proves it all on **all three
ISAs** (pure compute, no netdev): the h1 codec with the **zero-copy borrow asserted
by pointer range**, the 22 smuggling rejections, the **SWAR == scalar** oracle over
20,000 fuzz buffers, an **h1 client talking to our h1 server over real `net::tcp`**
across the in-cell `VirtualLink` (POST+body byte-exact, a chunked response
reassembled exactly, a **second request on the same never-closed connection**, a
404), **HPACK against RFC 7541 Appendix C** - C.1 integers, C.2.1-C.2.4, and the
**C.3.1-C.3.3 / C.4.1-C.4.3 sequences** (C.4 Huffman) decoded to the RFC's exact
header lists *and* re-encoded to the RFC's exact bytes with the dynamic table sizes
**55/57/110/164** checked (indices 62/63 are only reachable if both tables evolve
identically), Huffman edge cases, **h2** (preface + SETTINGS acked both ways,
HEADERS+DATA, a **flow-control-gated body** - 16 of 39 bytes arrive, the remainder
asserted still queued, the server's WINDOW_UPDATE releases it - a second concurrent
stream, RST_STREAM/PING-ACK/GOAWAY, and four protocol errors asserted to fail), and
**HTTPS**: one h1 exchange through the N3b TLS record layer with the plaintext
asserted absent from the ciphertext, a tampered record still rejected, and ALPN
negotiating `http/1.1` and `h2`. The **live GET is skipped with a reason** - SLIRP
has no HTTP server, so there is nothing deterministic to fetch and nothing is faked.
Deferred: **HTTP/3** (it is HTTP over QUIC = N7), trailers, server push, PRIORITY as
a scheduler, `CONNECT`/`Upgrade` (incl. `h2c` upgrade), `100-continue`, content
codings, a resumable chunked decoder (the one-shot re-scan is O(n^2) per body), a
bound-to-port server *cell* (that composes N4a fan-out with the N6 inbound
steering), and gRPC / Arrow Flight / Kafka (N5b/N5c) on top of this h2.

**rheo-net N4b** (docs/NETSTACK.md 18, docs/LINUX-COMPAT.md L8-INET-REMOTE) is
**real remote networking for unmodified Linux binaries** - the biggest functional
unlock left. L8-INET gave `AF_INET`/`AF_INET6` sockets but **loopback only**: a
non-loopback destination was refused `-ENETUNREACH`, because a *local* TCP connection
degenerates to the L6 ring the kernel already has while a *remote* one needs the real
segment/RTO machinery, and the kernel is **allocation-free**. N4b's key: that is true
of the `kernel/` **library**, not of a *kernel binary* - a test kernel declares its own
`#[global_allocator]` and already links alloc crates. So the kernel gains **a bridge,
not a stack**, and **no kernel object**: `svc::SocketOps` (10 `fn` pointers -
`local_ip`, `udp_bind`/`close`/`send`/`recv`/`pending`, `tcp_connect`/`send`/`recv`/
`close` - plus `set_socket_ops`), mirroring `svc::FileOps` line for line (the pattern
that keeps the kernel **filesystem-free** while serving `open`/`read`/`write`), and
`kernel/src/linux/fd.rs` forwards every **non-loopback** operation to it via two new
`FdKind` variants (`InetUdpRemote`/`InetTcpRemote`). **Loopback is byte-for-byte
unchanged** (`linuxinet` still asserts its transcript), and with no bridge registered
the answer stays `-ENETUNREACH`, so all 49 pre-existing kernels behave identically.
The registrant is `tests/src/inet_personality.rs` (the sibling of
`vfs_personality.rs`), which links **`rheo-net` in a new librheo-free `codec`
posture** - librheo supplies a *cell's* `_start`/panic handler/global allocator, which
a kernel binary cannot link, so the crate now has a default `hosted` feature gating
the async endpoints (`arp::resolve`, `udp::UdpEndpoint`, `icmp`, `dns`, `timer`,
`local`, `service`) and `--no-default-features` leaves the pure synchronous layers
(`eth`/`ip`/`arp` packets/`udp` codec/`tcp`/`cc`/`shard`/`wire` framing). Everything on
the wire is the stack's own code - `eth` framing, `arp` request/reply + a 4-entry cache
with real next-hop routing, `ip` headers + checksum, `udp` build/parse + pseudo-header
checksum, and the full RFC 793 `tcp::Connection`, whose **synchronous**
`poll(now)`/`on_wire_segment(now, bytes)` seam (written for the N2a in-cell link)
drives straight from a syscall trap. Three small documented driver accessors were added
(`virtio_net::send_frame_slice`/`recv_frame_slice`/`mac_addr`) plus a
`net_rx::wait_frame_slice` twin, so a remote receive **parks** on the N2d
park-until-frame primitive - a genuine WFI idle on riscv64/aarch64, a timer-backed
`hlt` idle on x86-64 (which has no NIC RX line). The `linuxnet` test proves it on **all three ISAs**: an
**unmodified static-glibc C binary** (`inetremote.c`) hand-builds a DNS query,
`sendto`s it to SLIRP's responder `10.0.2.3:53` and `recvfrom`s the reply, asserting
its **structure** (txid echoed, QR set, sender `10.0.2.3:53` - never a resolved
address, which SLIRP proxies non-deterministically), then `connect()`s to a closed
gateway port `10.0.2.2:9` where SLIRP's **real reset** becomes `ECONNREFUSED`; each
phase prints one line from a small fixed set so the transcript stays exact while
nothing is fabricated, and the kernel also asserts the receive genuinely parked
(`net_rx::irq_count() > 0` + `did_idle()`). Honest scope: **UDP remote is complete**;
**TCP connect is real and proven** (SYN on the wire, RTO retransmit, RST -> refused,
deadline -> `ETIMEDOUT`) while TCP **data transfer is implemented but unproven** -
**correcting the earlier wording**, SLIRP *does* proxy outbound TCP (the same proxying
that makes the live DNS below resolve real names), so the obstacle is not the wire but
the absence of a **deterministic peer**; arranging one (a host-side sink at
`10.0.2.2:<port>`, a `guestfwd`ed listener, or an N4a peer cell) is what closes it. One
real defect on that path **is** fixed: `op_tcp_send` never pumped RX, so the peer's ACKs
never reached the state machine, `snd_una` never advanced, the send queue filled and
`write` returned 0 -> EAGAIN forever - any body larger than the send window
**deadlocked**; the send path now drains the NIC first (reasoned + code-reviewed,
**not proven**, per docs/ENGINEERING.md 7). Also honest: **IPv6 remote** stays
`-ENETUNREACH`; no remote **listener** (inbound needs NIC
steering grants); remote handles are not refcounted across `dup`/`fork`; fixed
registries (4 UDP / 4 TCP / 4 ARP); one documented 2 s receive + 3 s connect bound (no
`SO_RCVTIMEO`; a descriptor made non-blocking with `fcntl(F_SETFL, O_NONBLOCK)` passes a
zero deadline and reports `EAGAIN`); no DHCP (the SLIRP identity 10.0.2.15/gw 10.0.2.2 is
fixed); and moving the datapath into the **N4a service cell** awaits N4a's deferred
name-based rendezvous.

**Name resolution + four Linux-personality stubs that reported success** (docs/
LINUX-COMPAT.md, docs/NETSTACK.md 18). Hand-building a DNS packet proved the datapath
but not what real programs do, which is call **`getaddrinfo`** - and that failed,
because nothing in the tree provided `/etc/resolv.conf`, so glibc fell back to its
built-in nameserver `127.0.0.1:53`, which the personality classifies as **loopback** and
routed into the in-kernel datagram queue where nothing listens - and **the send reported
success anyway**. Four fixes, each an `ENGINEERING.md` 7 violation of the same shape (a
stub reporting success while doing nothing), each with a proof observed to fail without
it: (1) the `linuxnet`-class kernels seed `/etc/{nsswitch.conf,hosts,resolv.conf}`
(nameserver `10.0.2.3`, non-loopback) and a loopback datagram to a port with no bound
endpoint now returns `-ECONNREFUSED` - the `resolve` fixture asserts that refusal, then
`rheo.test` -> **10.9.8.7** out of the seeded `/etc/hosts` (deterministic, no wire), then
reports a **live** `getaddrinfo` of a real public name (resolves on all three ISAs here;
the address is never asserted). (2) **`futex` honours its timeout** - it was ignored
entirely, so `pthread_cond_timedwait` hung; the timespec in arg 3 is now read
(relative for `FUTEX_WAIT`, absolute for `FUTEX_WAIT_BITSET`, CLOCK_REALTIME when
`FUTEX_CLOCK_REALTIME` is set), compared in the **cell's own clock domain**
(`linux::cell_clock_ns`, the domain the program computed the deadline in), and parked on
through the timer arbiter's new `FutexWait` slot - never `arch::timer_*` directly. A wait
with no timeout and no runnable sibling can never be satisfied: it now reports `-EAGAIN`
plus one console line instead of 0 ("you were woken"). The `condwait` fixture in
`linuxthreads` times out twice on a never-signalled condvar; without the fix it hangs to
the 120 s boot-test timeout (observed). Bounding the new pointer argument also caught a
defect of its own making: validating futex arg 3 unconditionally refused a legitimate
`FUTEX_WAKE` with `-EFAULT` (real callers leave that register dirty because it is a
*count* there), which silently stopped every waiter waking - observed as rayon-threaded
`sort` producing no output on ARM64 while the other two ISAs passed; the check is now
command-aware. (3) **`fcntl` stops lying**: it ended in
`_ => 0`, so every unimplemented command reported success - file locking now answers
`-ENOLCK` (no lock manager) and anything else `-EINVAL`; `F_SETFL(O_NONBLOCK)` is
**honoured** on every fd kind (a would-block read returns `-EAGAIN`, and an empty
non-blocking console no longer answers 0 = end-of-input) while `O_APPEND`/`O_ASYNC` are
**refused** rather than dropped; `F_GETFL` reports the real access mode;
`FD_CLOEXEC` is tracked and **`execve` closes those descriptors** (it used to keep every
fd). The `fcntlx` fixture in `linuxproc` asserts all four, including self-`execve`ing to
check one fd is gone and another survived. (4) **Limits raised for the real target**:
the frame pool 128 -> **512 MiB**, the per-cell budget 96 -> **384 MiB**, the global
reserve 8 -> 16 MiB, the Linux stack 1 -> **8 MiB** (with `RLIMIT_STACK` now derived
from the one constant, since glibc sizes thread stacks from it - and since then the
stack is sized from the image's own **`PT_GNU_STACK`** request, 8 MiB being only the
floor for an image that asks for nothing; docs/LINUX-COMPAT.md). QEMU gives 1 GiB and the
pool base sits 64 MiB into RAM, so the headroom is deliberate - firmware puts blobs near
the *top* of RAM (RISC-V `virt`'s DTB at ~`0xBFE0_0000`). This is a **limit raise, not a
design change**; the proper fix is **demand paging**, which has since landed for
file-backed `mmap` (see the demand-paging paragraph below). Found worse than
described along the way: **`poll` does not compute readiness at all** - every open fd is
reported ready and the timeout is ignored - which is simultaneously what lets glibc's
resolver reach its blocking `recvfrom` and the reason creation-time
`O_NONBLOCK`/`SOCK_NONBLOCK` still cannot be honoured; a waiting, readiness-computing
`poll` is the next rung and the two must land together.

**rheo-net N4c** (docs/NETSTACK.md 20) is **host configuration** - the two questions a
host must answer before anything works, *who am I on this link* and *what time is it*.
Four pieces, all ordinary userspace over UDP/ARP, **no kernel object, no new verb, no
new dependency, no `cfg(target_arch)`**: **`net::hostcfg`**, the host-config store
(address / netmask / gateway / DNS servers / search domains / hostname / source) owning
the on-link-vs-gateway `next_hop` decision - `HostConfig::slirp()` is now the **one**
place QEMU's guest/gateway/resolver addresses are named, and
`udp::UdpEndpoint::from_host_config` / `dns::Config` / `net::service` read the store
instead of carrying literals (a link-local claim deliberately **clears** the gateway);
**`net::dhcp`**, a DHCP client (RFC 2131 - the BOOTP codec + magic cookie + TLV
options, DISCOVER->OFFER->REQUEST->ACK->BOUND, T1/T2 renewal + rebinding + expiry with
the RFC defaults and a `T1 > T2` clamp, NAK, DECLINE, RELEASE, and the broadcast
`0.0.0.0 -> 255.255.255.255` **no-ARP** framing that is the whole special case);
**`net::zeroconf`**, IPv4 link-local (RFC 3927 - the `0.0.0.0`-sender ARP **probe**,
conflict re-pick, bounded announce, defend-once-then-yield) plus **mDNS** (RFC 6762
over the **`dns` codec unchanged** - which is why N4c made that codec
posture-independent: QU bit, cache-flush bit, TTL-0 goodbye, `.local` scoping, the RFC
1112 `01:00:5e` MAC mapping); and **`net::ntp`**, an SNTP/NTPv4 client whose answer is
a **bounded interval** (half-width `delay/2` widened by the server's root distance),
adjusting a *userspace offset* and never a system clock. Codecs + state machines are
always compiled; only the four async drivers are `hosted`. The `nethostcfg` test proves
it on **all three ISAs**: a deterministic **network-free** core (a complete DISCOVER
byte oracle with every uncovered byte asserted zero, the state-machine walk driven by
OFFER/ACK from our *own* encoder, renewal/rebind/expiry/NAK, seven rejections each with
its own error, DECLINE/RELEASE, the store read back by `dns::Config` + `UdpEndpoint`,
the link-local KAT + conflict protocol, mDNS oracles, and the NTP KAT - offset exactly
**+250 ms**, delay **1.5 s**, half-width **750 ms**/**1.75 s** - plus nine rejections
and the KoD backoff), then four **bonus live** phases: SLIRP **does** run a DHCP
server, so a real DISCOVER->OFFER->REQUEST->ACK yields `10.0.2.15/24` on every ISA
(reported, never asserted - it is a property of the QEMU backend, and nothing ever
synthesises a lease), while NTP and mDNS skip-with-reason and the link-local probes go
out and report *absence of evidence*. N4c also **fixed two real defects**: every live
wait is now bounded by a **duration** rather than a drain count (a drain is an
interrupt park on one ISA and a poll on another, so a count meant different things per
ISA and blew the test budget) - `wire::recv_frame_timeout` -> `net::recv_timeout` ->
`SYS_WAIT_NET`, with counts that remain counting **frames**, which is what forced the
kernel wait's deadline-not-spin-count semantics and its timer-backed idle above; and
`LinkLocal::announce` used to serve both the bounded announcement sequence and an
unbounded post-claim *defence*, so once claimed it returned a frame forever and a
driver's `while let Some(f) = ll.announce()` never terminated (announcing and
`defend()` are now separate, and the boundedness is asserted). The test also asserts
the kernel's **wait mode** per ISA: `NicInterrupt` + `did_idle()` on riscv64/aarch64
(3-4 genuine device interrupts per run), `TimerIdle` + `did_idle()` +
`interrupt_driven() == false` on x86-64. Deferred: DHCPv6/SLAAC, DNS-SD, mDNS
known-answer suppression + name probing + the RFC timing schedule, IGMP/MLD, PTP/NTS
and any clock discipline, and running the four clients inside the N4a service cell
(needs N4a's name-based rendezvous).

**rheo-net N2e** (docs/NETSTACK.md 21) makes congestion control **rate-based by
default**: a from-scratch **BBRv3** (`net/src/bbr.rs`) plus the **pacer** it cannot work
without (`net/src/pacer.rs`), both in the crate's always-compiled half (synchronous state
machines, like `tcp`), integer/fixed-point, portable, **no new kernel object, no new
verb, no new dependency**. Loss-based control is the wrong *default* for the paths this
stack targets - on a high-BDP path one loss costs seconds of throughput, on lossy
wireless **loss is not congestion**, and a loss-based controller only backs off once a
buffer has already overflowed (bufferbloat). BBR replaces the inference with a
**measurement**: a windowed max delivery rate + a windowed min RTT, sending at that rate
with ~1 BDP in flight. The `CongestionControl` trait grew its **rate-based half** -
`RateSample`/`on_rate_sample`, `pacing_rate_bps`, `inflight_cap`, `min_rtt_ns`/`bw_bps`/
`rounds` - **all default-implemented**, so `FixedWindow`/`Reno`/`Cubic` are
**byte-for-byte unchanged** (`pacing_rate_bps() == 0` is exactly what keeps them
unpaced). **BBR's ACK clock is this OS's completion clock**: the delivery-rate sample
falls out of the send/ack bookkeeping (every first transmission - the `snd_max` test
Karn's algorithm already uses - records the `delivered` counter + timestamps; the ACK
computes `delivered_diff / max(ack_elapsed, send_elapsed)`, the `tcp_rate.c` idea), and
in a hosted cell those ACKs arrive as queue completions carrying the flow id. The state
machine is Startup (pacing gain **2.77x**, exponential, exiting on a bandwidth plateau
or an excessive-loss round) -> Drain (**0.75x**, until in-flight is one BDP) -> ProbeBW
(Down **0.9** / Cruise **0.95** / Refill **1.0** / Up **1.25**) -> ProbeRTT (entered on a
**10 s** stale min-RTT, in-flight capped at **0.5 BDP**), with the max-bw filter a ring
of per-round maxima that genuinely **expires** and the min-RTT filter taking any lower
sample but a higher one only after the window. The **loss response is not a
multiplicative collapse**: a loss trims `inflight_hi` by 30%, at most once per round and
**floored at one BDP**, leaving the bandwidth estimate, the pacing rate and the min-RTT
untouched (there is no `ssthresh` at all) - so random loss on an unqueued path costs
nothing, while *genuine* congestion still lands as a falling delivery rate that shrinks
the BDP through the filter. **Pacing is a precondition, not a knob** (unpaced, BBR bursts
a window into the link and is worse than CUBIC): a token bucket releases data segments at
the controller's rate with a `max(2*MSS, rate*1ms)` burst, and its deadline goes to the
**N2h arbiter's reserved `Pacer` slot** - the arbiter's first **continuously re-armed**
client - via `librheo::time::sleep_pacing` -> `SYS_ARM_TIMER`'s new second argument (a
slot selector, `0` = the pre-N2e cell-sleep shape: **not a new verb**, the N4a
`SYS_CONNECT`-slot precedent). N2e also finished N2h's unification: `SYS_ARM_TIMER`'s
no-hardware-timer path had its own spin loop that bypassed the arbiter, so a deadline on
an ISA without a verified one-shot was invisible to every other client; there is now one
path (register/park/cancel) that halts where the interrupt is wired and honours the same
deadline in software where it is not. On **reservations**: expressing the *byte rate* as
an object-7 reservation is **honestly rejected** (a reservation admits CPU time against a
core; the kernel holds no authority over link capacity, and reservations are per-cell
while a shard owns many flows - it would hand back a guarantee nothing can keep), but the
**pacer's own CPU cost** is a genuine periodic task, so `pacer::admit_pacing_cpu` asks
the real admission controller "can this cell afford to pace at this rate?" and gets a
clean refusal when it cannot. Per-profile tunings mirror N2h's poll tiers (`hft`
latency-first: 2 s min-RTT window, 50 ms ProbeRTT, 1.5 BDP cap; `warehouse`
throughput-first: 20-round bandwidth window, 4 s cruise, 2.5 BDP; `edge` the balanced
default; **`embedded` keeps CUBIC** - BBR's filters plus a per-segment timer buy little
on a known link). The `nettcpcc` test keeps its eight N2b trajectories **unchanged** and
adds eleven, on **all three ISAs**: the scripted Startup/Drain/ProbeBW/ProbeRTT walk
against hand-computed oracles, both filters (a 20 MB/s sample held exactly 10 rounds then
expiring), the **loss != congestion** headline (12 rounds at a 10 MB/s link rate with a
random-loss episode every fourth: BBR keeps its 10 MB/s estimate, 95% pacing and 1 BDP
in flight = **100% of the link rate**, while CUBIC on the identical trace falls to
187,534 bytes = **37%** - BBR sending **2.6x** faster - and then the converse, BBR halving
its rate when the *delivery rate* halves), connection-level pacing (2 segments in the
burst, then every release exactly one segment-time apart), BBR-vs-Reno loss recovery
(cwnd 20,440 with no ssthresh vs 11,680 after a slow-start restart), the window
controllers asserted unpaced/uncapped, the CPU-reservation admission (two paces admitted,
a third `Overcommit`, 100 Gb/s `BadParams`), and **14 live releases parked on real
arbiter pacer deadlines**. Kernel-side it proves the **continuous-re-arm** property N2h
could not: 40 back-to-back pacer deadlines while a 20 ms RTO and a 40 ms sleep stay
outstanding throughout, none lost, then firing in order. Honest deferrals: ECN, SACK
(so loss is charged per signal, one MSS), randomised ProbeBW cruise, hardware
timestamps, a delay-shaping `VirtualLink` (a *closed-loop* in-cell model proof - the
instant loopback yields no rate samples, which is why the model is scripted), two
concurrent timer waiters in one cell (the reactor still has one timer slot - a pre-N2e
limit), and byte-rate admission. Wall-clock throughput/jitter remain hardware-lab
numbers; only integer state and icount are reported here.

A **unified tile framework** (`librheo/src/tile/`, docs/TILES.md) makes
tile-centric compute (the TileLang/cuTile/Triton direction; SME/AMX; NPU/TPU
systolic; FPGA) a **library discipline over existing kernel objects** - one
tile program, every engine, **zero new kernel objects/verbs**. A tile is shape
x dtype x memory space; a `TileBuf<D>` is a dtype-tagged buffer over a memory
grant (object 5); a `TileProgram` is built once and lowered per engine - the
`CpuExecutor` runs it strand-parallel in the cell (scalar inner kernels, yield
at every tile-loop back-edge), and the **`EngineExecutor` lowers the SAME
program to dependency-graph nodes** (object 6) for the kernel's CPU engine now
and device engines when their driver cells exist (`EngineUnavailable`, never
faked). The kernel slice is **two graph-node op codes** inside the existing
`OP_GRAPH_SUBMIT` payload (the LIBRHEO.md Phase C buffer-node step): op 4
BufReduce (wrapping sum) and op 5 TileGemm (bounded int8->i32 GEMM, FNV
receipt), each carrying a `#[repr(C)]` descriptor's cell VA (validated with
hard caps -> `STATUS_DENIED`, never a fault); the engine executes them via a
`#[path]` source-include of librheo's dependency-free tile kernels (shared
verbatim with bench-core and the host comparison). The **dtype matrix** covers
every quantization size - native I8/U8/I32/F32 (computed directly) plus
storage F16/Bf16/FP8 E4M3/FP8 E5M2/TF32/int4-block (bit-exact soft-float-safe
conversions; MMA *over* a storage dtype is a compile error until a device
lowers it). A **deterministic `TileSim`** counts work + traffic (never timing);
its bytes-staged ordering is validated against host wall-clock in
`comparison/tiles` (both rank tilings `[256,128,64,32,16]`). The `librheotile`
test proves the framework (tiled GEMM bit-exact vs a naive reference, sim
determinism, contracts, the full dtype round-trip, CpuExecutor == kernel-engine
receipts) and `librheotilebattle` the production-shaped battle tier (scaled
7B-class layer GEMMs, an attention block, paged-KV prefix sharing, the
librheodata columnar reduce as tiles, a 100-run soak, boundary shapes, a
64-deep pipeline fence) - both on **all three ISAs**; `p6_*` benches report the
per-tile-op path lengths. **In-cell SIMD now runs**: librheo cells build
hard-float (SSE2/NEON/F+D baseline), the kernel enables AVX/AVX-512 for U-mode
on CPUID and saves/restores vector state across cell switches with XSAVE (the
kernel stays soft-float), and `tile::simd` runtime-dispatches the GEMM after a
boot probe (functionality-checks each tier bit-exact vs scalar, benchmarks,
picks the fastest, scalar fallback) - `librheotile` asserts the AVX2 kernel ran
bit-exact on-OS. Honest: QEMU TCG exposes AVX2 but not AVX-512 (so AVX-512/VNNI
light up only on real hardware, host-proven in comparison/tiles) and models no
SIMD speedup (so under emulation the probe's benchmark may keep scalar - the
selection adapts to the real host); pipelining is cooperative interleaving (SMP
#27); device engines are enumerated, not executing. The battle tier surfaced two real
latent fixes - a grant-slot leak on `SYS_MUNMAP` (freed frames but not the
per-cell table slot) and an f16-subnormal rounding bug - and the per-cell
grant table (16->64) and object table (128->512) caps were raised for
real-workload headroom, both flagged in docs/TILES.md 12.

The **hard-float / FP / SIMD merge** brought the tiles workstream and the
rheo-net workstream together, and closed the one defect that only existed once
they were in the same tree. FP/SIMD save-restore across the native cross-cell
switch was written when there were two switch paths; rheo-net N4a had since
added a third, **`SYS_YIELD`** - the round-robin yield a service cell's client
fan-out and the strand reactor's channel idle path both drive. A textual merge
compiled, passed all 168 pre-existing checks, and **silently corrupted a
hard-float cell's vector registers on exactly that path** (no fault, no log,
wrong numbers). The fix makes the invariant structural: `user::switch_native_cell`
is the *only* native cross-cell switch and swaps the register file *and* the
address space, so `SYS_SWITCH`, `nproc::reschedule` (`SYS_WAIT` / child exit or
fault), `SYS_YIELD` and a cell's first entry via `user::run` all carry it; the
bare `switch_to_cell` is documented as the **Linux** personality's switch, which
keeps its own per-*context* FP (a Linux cell has up to 8). The proof is a phase
of `librheoipc`: two hard-float cells pin a per-role pattern in 16 vector
registers inside a **single** `asm!` block that also contains the `SYS_YIELD`
(so the compiler cannot spill around the switch), and assert the register file
returns **bit-identical** - 256 bytes on x86-64/ARM64, 128 on RISC-V, on all
three ISAs - with the "read back the *peer's* pattern" case reported separately
because that is what an unswapped switch produces, plus a kernel-side swap
counter bumped only inside the restore. Verified in both directions: reverting
`yield_cell` to the bare switch makes 7 of 8 rounds report the peer's pattern
and panics the kernel. The merge also re-made three choices that had been
premised on soft-float-only (docs/NETSTACK.md 22): **integer-only CC math** is
kept and is now a *hard* constraint, not a convenience - `cc`/`bbr`/`pacer`/`tcp`
link in the librheo-free codec posture *beside a kernel binary* for the N4b
bridge, and FP in kernel context would falsify the very premise the FP
save/restore rests on; **forced software crypto backends** stay the default
after the hardware path was tried and **verified working** (it builds clean on
the hard-float cell target - the N3a LLVM miscompile does not reproduce there -
and with `+aes` at baseline emits 477 AES instructions and passes every N3a
vector on-OS, AES-GCM included), because baseline `+aes` has **no graceful
fallback** where this tree's own pattern is probe-verify-fall-back, and because
the throughput win is unmeasurable under QEMU; and **SWAR** stays in the HTTP
scanner because `http1` must link in the codec posture, which drops librheo
entirely, so `librheo::tile::simd` is structurally unreachable from it. Both
follow-ons are named in docs/NETSTACK.md 22 rather than half-done.

A **syscall-surface hardening** pass closed three critical defects an
architecture audit found - all of them in the seam between a cell's arguments and
the kernel's own memory, none of them visible to the capability core's own proofs
(docs/ENGINEERING.md 12, docs/ARCHITECTURE.md 8.2). **(F1)** There was no
`access_ok` equivalent in the tree: every out-parameter syscall, every queue
payload VA and every buffer handed to a `svc::FileOps`/`SocketOps` handler was
dereferenced in kernel mode at a **cell-supplied** address while the cell's root
was active - and every cell root maps all kernel RAM supervisor-RWX - so
`SYS_GRANT(out_va = <any kernel VA>)` was a 16-byte arbitrary kernel write with a
steerable first word. `kernel/src/user.rs` now owns one portable, allocation-free
check (`user_write_ok`/`user_read_ok` + `user_out`/`user_in`/`user_buf`/
`user_slice`): null, alignment, overflow-checked add, and a range test against
`USER_VA_MAX = 2^38` (RISC-V Sv39's user half - the narrowest of the three ISAs,
with every loader region pinned below it by a `const` assert) plus the shared
`.user` window, which on riscv64/x86-64 is linked high beside the kernel yet is
genuinely mapped U into every cell root (writes restricted to its per-cell
`.user.data`/`.user.bss`). Every named site is routed through it - the five
`user.rs` out-parameters, `svc.rs`'s EngineInfo/CpuFeatures/ShellIo/DebugWrite/six
FileOps forwards, `graph_submit` (node array, result array, tile descriptors, and
each descriptor's own matrix VAs by their exact computed extents), and the whole
`queue::run_opcode` payload surface (-> `STATUS_DENIED`) - and the Linux
personality is bounded at its **single dispatch point** (`linux::ptr_args_ok`,
-EFAULT) rather than at ~60 individual dereferences, which also bounded `readv`'s
and `poll`'s previously **unbounded** iovcnt/nfds array walks. **(F2)**
`SYS_MMAP`/`SYS_COMMIT` took `len` from the cell and looped a `frames::alloc` that
ended in `panic!("frame pool exhausted")`, so `mmap(1 << 40)` took the machine
down - worse than the OOM killer ARCHITECTURE.md 5 forbids, because it is an OOM
*panic*. `frames::alloc` is now fallible (`Option`, every caller audited; the
kernel-internal ones `expect` with the reason they cannot fail), guarded by a
global reserve (`USER_RESERVE_FRAMES`, 8 MiB) and a per-cell budget
(`MAX_FRAMES_PER_CELL`, 96 MiB), both checked before a frame is taken, with
rollback on partial failure, so exhaustion is a clean refusal (`-ENOMEM` on the
Linux paths). **(F3)** `SYS_MUNMAP` freed whatever the page tables returned, with
no capability, ownership or bounds check - and three frame sets in a cell's
address space are not its own (the shared channel ring, a peer's shared sealed
grant, its own queue-pair region), so it was a cross-cell use-after-free whose
second free tripped a "double free" assertion: a kernel panic from unprivileged
code. It now routes through `grant_resolve` exactly like its `SYS_COMMIT`/
`DECOMMIT`/`SEAL` siblings (a peer's grant is refused for free - the capability
minted into the peer carries READ, not MAP), plus the cell's own anon/file-mmap
bump regions; `unmap_range` refuses anything outside the cell's user VA range and
frees through a new `frames::free_if_pool`. The **`security`** test kernel proves
all three from a real unprivileged cell on all three ISAs against evidence the
cell cannot fake (a canary in kernel `.bss` it has no mapping for, the frame-pool
delta against a hand-computed baseline, a queue ring that still completes an
`OP_NOP` after the refusal), each with a control phase showing the legitimate path
intact; `librheowl` gained the cross-cell half - the compositor's attempt to free
the **client's** sealed grant is refused and the zero-copy checksum still matches.
Each phase was verified to **fail** with its fix reverted.

**The scheduler idle state** (docs/ARCHITECTURE-DEBT.md 2.4, the keystone) makes
`IO.md`'s and `CONCURRENCY.md`'s "blocking does not exist below the library level"
**true of the kernel**. It was not: `SYS_ARM_TIMER`, `SYS_WAIT_INPUT` and
`SYS_WAIT_NET` each waited *inside the trap*, in kernel context, without ever
consulting the scheduler - so one cell's `sleep` idled the whole machine while its
siblings sat runnable - and `reschedule` **panicked** when nothing was runnable, so
"every cell is waiting for the outside world" (a server's normal steady state) was
not an expressible state at all. Each wait is now a **registration**: the cell
records its condition, returns to the run loop, a sibling runs, and the syscall is
completed when the scheduler switches back into the waiter with its own address space
active. `kernel/src/idle.rs` adds **no kernel object and no verb** - it composes the
timer arbiter (still the only owner of `arch::timer_*`), the NIC RX line and the UART
RX line, halting where an interrupt can wake and saying plainly (`idle::spins()`)
where nothing can. Two waits deliberately keep their in-trap path, because parking on
them could never end: a receive with no NIC, and an indefinite receive with no NIC
interrupt (only `wait_frame`'s own `POLL_BUDGET` backstop ends that). A state with
**no** wake source left prints which cell/pid is blocked on what and ends the run
with `abi::DEADLOCK_EXIT`. On the Linux side the same machinery makes `poll`/`ppoll`
compute **real per-`FdKind` readiness** and honour their timeout, `epoll_wait`
actually wait, `nanosleep` actually sleep (in the cell's own clock domain), and stdin
block instead of answering 0 = end-of-input; `svc::SocketOps` gained the missing
`tcp_pending` (its absence *was* the hardcoded "a remote TCP socket is always
readable"), and creation-time `O_NONBLOCK`/`SOCK_NONBLOCK` is honoured. Those had to
land in one slice: glibc's resolver worked *because* `poll` lied and the flag was
dropped, so fixing either alone breaks DNS - it now blocks in `poll` until the socket
is readable (the scheduler idling on the NIC) and its non-blocking `recvfrom`
succeeds. Honest scope: this is **cooperative** - a cell yields at a syscall
boundary, so a compute-bound cell still holds the CPU until timer preemption (#27) -
and a Linux *thread*-level block still parks the whole cell rather than only the
calling context. Proven by `schedidle` and `linuxpoll` on all three ISAs, both
observed failing without the fix.

**Demand paging** (docs/LINUX-COMPAT.md "Demand paging",
docs/ARCHITECTURE-DEBT.md 4.0 blocker 2) makes a **file-backed `MAP_PRIVATE` `mmap`
and the ELF image itself cost what the program touches, not what it reserved**. Both
used to read every page into a fresh frame before returning - not a size problem to
answer with a bigger pool, the wrong design at any size. A **resumable user page fault** now fills a page on first
touch: `on_user_trap` calls `linux::fill_fault` *before* the L5 fault-to-signal branch,
and `linux::mem::fault` asks three questions in order - is anything mapped here (no
record = a genuine SIGSEGV), **is the page already present** (then this was a
*permission* refusal, and since `FaultCause` carries no read/write bit the page tables
are the source of truth via `AddressSpace::is_mapped`/`arch::paging_mapped` - guessing
repopulates and re-faults forever, measured at 78,780 fills in the revert probe), and
does the mapping permit any access (a `PROT_NONE` record is a *reservation* glibc
commits later with `mprotect`). A mapping owns a VFS handle in **`linux::filemap`**
rather than the caller's fd, because `ld.so` closes the fd immediately after `mmap` - a
global, fixed-size, refcounted registry, **no kernel object** (the `pipe`/`epoll`/
`eventfd` pattern), one reference per live `Vma` record, taken at `fork`
(`VmaList::inherit_files`, beside `fds::inherit_pipe_ends`) and given back at exit and
`munmap`. Both halves are load-bearing: without the `fork` addref a child's exit frees
an entry the **parent** still maps, and the parent's next untouched page reads zeros
with no fault and no log. Two kernel prerequisites had to land with it - a cell hands
the kernel pointers into its own memory, so the **kernel** becomes a reader of an
absent page (a load fault at a kernel PC, not resumable here - this is why Linux has
`copy_from_user` + a fixup table); the F1 pointer helpers gained "ensure present"
beside "in range", **on the dereference helpers only**, because putting it on the bare
range predicates cost a ~2,900x amplification (`unmap_range` bounds a range with them
and so materialised every page just before freeing it) versus **0** kernel pre-faults
where it now sits, measured every run. And x86-64's ring-3 fault resume used `sysretq`,
which *consumes* RCX/R11 - harmless while signal delivery was the only fault resume,
fatal for the first path that genuinely re-executes; faults now resume via
`iret_resume`. Proven by `mmapdp` in `linuxproc` on **all three ISAs** (64 file pages
mapped, exactly **5** filled, each carrying its own per-page byte so the offset
arithmetic holds at the top of the mapping; 100 rereads free; a write to a *filled*
read-only page still SIGSEGV; a page still filling from the file after a forked sharer
exited; the registry back where it started) and by `linuxdyn`, where `ld.so` maps a real
1.5-2.1 MB `libc`. **The ELF image is demand-paged too**: `load::load_elf_linux`
**records** the `PT_LOAD`s the fault handler can fill (`load::SegRecorder`) and copies
only the ones it cannot, printing which segment and why each time - the two conditions
being `p_filesz == p_memsz` (a `.bss` tail inside one record produced a null
dereference in a static Rust binary) and `p_offset` congruent to `p_vaddr` mod the page
size (paging fills whole pages). Because the image is already resident in kernel memory
rather than in a file, `filemap` carries a second store kind, and because a segment's
content ends mid-page, `Vma::file_len` says how far a record is backed - past it the
pages are zeros, not the next segment's bytes. `user::reset` must run **before** the
load, since it clears the registry the loader registers the image in (the old order
zeroed every page - an illegal instruction at the entry point; `filemap::alive` now
says so). Measured on riscv64: `rusthello`'s 201 image pages cost **16** frames at load
instead of 201, and `linuxrun` asserts that inequality on all three ISAs. **`execve` and
the ELF interpreter are demand-paged too**: both stream from the VFS, and the obstacle
was never the streaming but recording against the *caller's* fd, which is closed on
return - so a mapping opens its own handle over the path (the `mmap` precedent), the
recorder holds two stores because a dynamically linked program is two files, and
`exec_reinit` records as `install_cell` does. Witnessed by
`load::recorded_pages()`/`eager_pages()`, since both paths run inside a syscall where a
test can measure nothing directly: `linuxdyn` records 1 program + 1 `ld.so` segment (35
pages recorded, 4 copied), `linuxproc`'s fork+execve phase 221 recorded to 21 copied.
That slice also introduced and fixed a regression the matrix caught -
`exec_elf_from_vfs` is shared with the **native** `SYS_SPAWN`, which has no VMA list, so
a lazy image left its child with an address space full of holes; the eager and lazy loads
are now separate functions, the eager one keeping the old name so an unaware caller gets
a correct image. (`fork` is copy-on-write and the stack grows on fault - see the two
paragraphs below; a segment with a `.bss` tail and a **native** cell's image stay eager,
both riding this same handler.)

**Copy-on-write `fork`** (docs/LINUX-COMPAT.md "Demand paging", docs/ARCHITECTURE-DEBT.md
4.0 blocker 2) makes a fork **share** the parent's pages rather than copy them. It used
to eager-copy every committed page, so a process paid its whole resident set to fork -
more, for a large program, than its image ever cost. Now `AddressSpace::fork_from` shares
read-only into the child and marks both sides copy-on-write; each page privates on first
write in `linux::mem::fault`. Measured on riscv64: a fork of a 2406-page (9.4 MiB)
process shares 2406 pages, copies 0, costs 12 frames of child page tables - **200x**.
Three pieces: a **per-frame refcount** in `frames` (`free` is now a decrement, so every
pre-COW caller is unchanged; `share` refuses a non-pool page or the ceiling and the caller
copies), a **software PTE bit per ISA** (`arch::paging_cow_protect_user`/`_at`/`_clear` -
Sv39 RSW 8, AArch64 55, x86-64 52; the mark lives in the page table not the VMA list, so
it covers the stack and `brk` heap that have no VMA record), and the **parent
write-protect** (the half that fails silently - without it the parent writes through to
memory the child now sees). Proven by `cowfork` in `linuxproc` on all three ISAs with
`mm::fork_pages()`/`fork_frames()` as the oracle, both halves observed failing when
reverted.

Every kernel access to a cell's memory now goes through **one seam, `kernel/src/uaccess.rs`**
(docs/LINUX-COMPAT.md "the uaccess seam"). Lazy mapping makes readiness a moving target -
demand paging made presence lazy, COW makes writability lazy on top - and a fault in kernel
mode is not resumable, so each strength must be resolved before the access (the
`copy_from_user`/fixup-table problem). Before this, ~98 sites touched cell memory and 51
dereferenced the raw VA with only a bounds check done elsewhere, so each lazy feature
re-opened a 98-site audit; all 51 now route through `uaccess`, which offers bounds-only
predicates (kept separate - folding presence in cost a measured ~2,900x amplification),
resolve-and-hand-back, and resolve-and-perform (`read`/`write`/`copy_in`/`copy_out`/`fill`,
where a site cannot forget to resolve). Kernel *mechanism* (refcount, share, cow-protect,
fault delivery) is kept separate from COW *policy* (personality code) so the policy can
move behind a userspace process server later, the seL4 way; it is pre-resolution, not a
fixup table, which SMP (task #27) will need.

The **stack grows on fault** too (docs/LINUX-COMPAT.md, docs/ARCHITECTURE-DEBT.md 4.0
blocker 2), which closes the last eager path in the Linux memory model - image, file
`mmap`, `fork`, and stack are all lazy now. `setup_stack` maps only the top page (argv/
envp/auxv) and `install_cell`/`exec_reinit` register the rest of the `PT_GNU_STACK`
request as an anonymous RW reservation (`mem::reserve_stack`); a touch below the top page
faults in through the same handler, a touch below the reservation is a SIGSEGV (the guard
page from the bound, not a dedicated page). An image asking for 64 MiB of stack used to
pay it before `main`; it now pays one page plus what it touches. `stackx` (linuxproc, all
three ISAs) proves it: a 12 MiB request's 9280 KiB of writes appear as 2380 demand fills,
59 when the eager mapping is restored. Still eager and named: a `.bss`-tail segment and a
native cell's image, both riding this handler.

**Substrate 2 mechanisms are built** (docs/SUBSTRATE.md, docs/DRIVERS.md):
the pieces that replace the bring-up scaffolding - the fixed `MAX_*` tables, the
magic VA map, the six-slot timer arbiter, the absent scheduler order - exist and
are proven, **beside** the old paths rather than under them. **Funded kernel
metadata** (`mm/kmeta.rs`): a `Funded<T>` table over a page directory of frames
**charged to the owning cell**, growing past every ceiling it replaces (proven to
4096 elements vs the old 512-entry object table), `Option`-fallible with rollback,
per-owner charge ledger so exhaustion is attributable instead of a global "table
full". **A per-ISA user VA ceiling** (`arch::USER_VA_TOP`: x86-64 `2^47`, ARM64
`2^48`, RISC-V Sv39 `2^38` as its own floor rather than everyone's) plus a real
**VA region allocator** (`mm/vaspace.rs`) - first-fit with guard gaps, overlap
refused not evicted, a mid-range release *splits* the straddling record. **The
per-CPU primitives are now always compiled** (`smp.rs`: `SpinLock`, the generic
`PerCpu<T>`, a **total** `cpu_index()`), because per-CPU-ness is a property of a
data structure, not of a build configuration - the alternative was writing every
per-core subsystem twice, which is the shape that produced the FP/SIMD
`SYS_YIELD` defect. The byte-identity property SMP phase 1 maintained is
therefore superseded by a stronger one - *enabling the feature must not change
single-CPU behaviour* - and docs/SMP.md records the trade rather than dropping it.
The **timer arbiter is per-CPU** and gained a **hierarchical timing wheel**
(`ktimer/wheel.rs`): O(1) arm/cancel over funded nodes with cascading, so an
unbounded number of same-kind deadlines works (one QUIC connection needs five;
Node arms thousands) - 64 concurrent deadlines are proven honoured **in deadline
order** beside the named-client slots, neither losing the other's. A **metrics
pipeline** (`metrics.rs`): per-CPU HDR-style histograms, real percentiles,
**jitter defined once as P95-P50**, integer-only, buckets lazily funded per
(CPU, metric). The **scheduler order** (`sched/bore.rs` + `sched/vcore.rs`): the
BORE burst score as an integer log2 with fork inheritance, feeding **one
deadline-ordered EEVDF queue** that holds hard-deadline reservations, virtual-
deadline fair work and residual work together - the eligibility gate proven to
defer an over-consumer that still holds the earliest deadline (the only
configuration where EEVDF differs observably from EDF). **Userspace std is
hard-float on all three ISAs** (`targets/rheo_os-*.json`: SSE2 / NEON / `+f,+d`
with `lp64d` - the ABI name must move with the features or FP runs under a
soft-float convention); the kernel stays FP-free as a *performance* choice (the
Linux `-mno-sse` precedent: no syscall, trap or interrupt then saves the vector
file), with a designed `kernel_fp_begin` escape hatch. **Per-CPU DRBG roots** so
`getrandom` takes no cross-core lock, secondary roots *derived* (fast key erasure)
and never copied - two cores with the same key would emit identical streams that
look random in isolation. And **`madvise` stops being `Ret(0)`**: DONTNEED/FREE
genuinely decommit (how every allocator returns memory), `MADV_WIPEONFORK` is
recorded per VMA and honoured by `fork` (the fix for a forked userspace CSPRNG
producing its parent's stream), advisory values are accepted, the rest refused
with a reason. Proven by the **`substrate`** test kernel - nine phases against
frame-pool deltas, the charge ledger, structural invariants and hand-computed
oracles - on all three ISAs, with the whole pre-existing suite green **unedited**.
It found two real defects on the way: the wheel returned fired timers in
allocation order rather than deadline order (a transport would apply a later RTO
before an earlier one), and RISC-V's `cpu_index()` read an uninitialised `tp`.
**Two of the three migration stages have now begun** (docs/SUBSTRATE.md 15).

**S1' - four fixed tables are gone.** The Linux **context tables**
(`MAX_THREADS = 8`; `INITIAL_CONTEXTS` is now a reservation, not a ceiling), the
per-cell **signal** contexts, the per-cell **VMA list** (`MAX_VMAS = 128` - measured
as the wrong shape: V8's pointer-compression cage plus code ranges, JSC's Gigacage,
and glibc's 64 MiB arena *per thread* put a JIT-bearing runtime past a hundred
records before it runs a line of its own code, and a full table did not fail cleanly
- `remove` dropped the tail of a split mapping), and the global **mapped-file
registry** (`MAX_MAPPED_FILES = 64`, handle widened `u8` -> `u16`, since the width
was the real ceiling once the table could grow and a wrapping handle points a mapping
at *another file's* bytes - neither a fault nor a refusal). Three structural lessons,
all recorded: a funded table **cannot be raw-copied** (`fork`'s
`copy_nonoverlapping` of `LinuxState` duplicated the descriptor, so parent and child
addressed one shared directory frame - each funded field now needs an explicit deep
copy, and an unfundable child makes the fork `-EAGAIN`); **every slot-handback path
becomes a release path** (two real leaks existed the moment the tables became funded
- a reaped cell's context tables were only released by the between-runs reset, so
every fork+exec pair leaked until the next boot); and **a global table's one-off
growth must not land inside a per-operation measurement** (the registry growing
lazily charged its frames to whichever operation was first, which broke `linuxrun`'s
demand-paging assertion immediately - it is funded at its reset point now, so its
storage is a boot cost). Also removed a silent truncation that predates the work:
`apply_fork_advice` collected marked ranges into `[_; MAX_VMAS]` scratch and dropped
anything past the end, so a `MADV_WIPEONFORK` region could quietly keep its parent's
random state.

**S3' - the ready queue dispatches, and a cell can be preempted on all three
ISAs** (task #27's core). `sched::dispatch` is the **seam**: the two `reschedule`
functions and `SYS_YIELD` ask the EEVDF+BORE queue for the *order* while the
personality's own state stays the sole authority on *runnability*, reconciled at the
pick so the two can never disagree; disabled, `pick` is the pre-migration round-robin
expression for expression, so the migration turns on one boot at a time. CPU time is
charged and every relinquish recorded **at the transition itself** - which is what
makes the BORE score measured rather than inferred, because this kernel has no path
from running to not-running that does not pass through a named call. `sched::preempt`
takes the CPU away: the timer arbiter gains a `Preempt` slot, the interrupt handler
sets one flag, and the portable `user::on_user_interrupt` decides at trap exit whether
the CPU moves - a **sibling context of the same cell first** (the `linuxbun` shape),
then another cell; splitting "note it" from "act on it" is not ceremony, since an
interrupt can land while the kernel holds a reference into a funded table and a
scheduler invoked from the handler would be reentrant. Each ISA needed a real change:
riscv64's user trap already serviced U-mode interrupts, **aarch64's lower-EL IRQ slot
was a *fatal* slot** and cells ran with `SPSR.I` set, and **x86-64 cells ran with `IF`
clear** with a LAPIC stub that saved only caller-saved registers (the timer vector now
routes through `common_trap`, reusing its ring-3 frame capture and IRET resume rather
than writing a second one). Both mask changes are read at frame-construction time, so
a cooperative boot's frames keep the pre-migration bits exactly. The **`preempt`**
kernel proves it with its own negative control in the same binary: two cells run a
compute loop that issues **no syscall at all**; cooperatively cell 0 runs all 24
rounds unbroken and cell 1 never gets the CPU (asserted - that is what makes the other
phase evidence of anything), and with dispatch on the shared order vector interleaves,
the longest unbroken run dropping 24 -> 2-9 with 14-33 slices actually taken. An
interleave is only producible if something took the CPU away mid-loop.

**Every online core computes at the same time** (docs/SMP.md 10, the `smp` kernel, all
three ISAs). Its prerequisite landed first: `frames`' bitmap, reference counts, used
counter and search hint are one data structure with four fields that every operation
touches several of, so they are behind a `SpinLock` - **unconditionally**, not behind
the `smp` feature, because locking is a property of the data structure rather than of a
build configuration (the lesson that produced the `SYS_YIELD` FP defect: state whose
safety depends on which features are enabled gets written twice and diverges); an
uncontended acquire is one atomic exchange, unmeasurable next to zeroing a frame. On
top of it **every online core drains a shared work queue**: an int8 GEMM - the tile framework's
own `gemm_i8_i32`, shared verbatim - has its output rows split into blocks, and each
core claims blocks from a single `fetch_add` cursor until exhausted. Split by output
rows so the two write disjoint ranges of C and the compute needs no lock at all;
*claimed* rather than pre-assigned so the division is a result, not an assumption - with
a static half-and-half split the faster core idles and the per-core counts prove nothing
because they were decided in advance. The result is asserted **bit-identical** to a
single-core oracle, **every** online core's count is asserted nonzero and the counts to
sum to the queue (they vary run to run - 8/8, 9/7 on two cores; 4/3/6/3 on four), and the
frame pool's used counter is asserted still to agree with its bitmap (the invariant a lost
update breaks). **The queue is drained by all four cores, not two**: the job used to be
`take()`n out of its slot by the first secondary to see it, so the phase was inherently
primary-plus-one while the rest sat in their idle loops beside undrained blocks. It is
published by **generation** now - each core drains a given round once - and the primary
waits for *the queue* (every block accounted to the core that did it) rather than for one
secondary's flag, since with N participants it cannot know how many will signal. The
round runs twice: once before `start_all` (2 cores, all that are up) and once after
(4 cores). Observed failing when the `take()` is restored. The parallelism is proven by a **barrier rather than by
timing**, and by an **N-way** one: the two-way rendezvous could only ever witness a pair,
since each half waits for exactly one peer, so with four cores online it left two
unaccounted for. Every participant now waits for all of them, sized from
`online_count()` - which the primary already knows - so passing it means every online core
was inside one interval - which a single
core cannot produce, since there is no kernel-context preemption to interleave them and
neither side yields. A wall-clock speedup would prove nothing under TCG (it time-slices
the two vCPUs onto host threads), so simultaneity is the available evidence and it is
what is asserted.

**And cells now run in user mode on a secondary core** (docs/SMP.md 10.0, the `smp`
kernel's fourth phase, **all three ISAs**): two cells execute at the unprivileged level
on two cores **at the same instant**, each in its own address space, each trapping back
into its own core's kernel stack. The witness is a shared page in which each cell writes
only its own round counter and the highest peer counter it ever saw - no lock, nothing
to lose - and a nonzero "highest peer seen" means the cell read the peer's progress
*between two of its own rounds*, which one CPU under cooperative dispatch cannot produce
(the first cell runs to completion before the second is entered, so it could only read
0). What had to become per-CPU: the saved kernel context `return_to_kernel` unwinds to
(`KERNEL_CTX`, now one slot per core - indexed by `tp` on RISC-V, MPIDR affinity on
ARM64, reached through `GS_BASE` on x86-64); the portable `CURRENT`/`TOP_CELL`/`EXITED`
(now `PerCpu<usize>`, a compile-time slot 0 on the non-`smp` build); **RISC-V's kernel
`tp`**, which is a saved GPR the cell owns as TLS *and* where the kernel keeps its CPU
index, so the frame gained a `kernel_tp` slot written on the way out and reloaded on the
way in - invisible on the boot CPU, where the wrong answer and the right one are both 0,
which is why it had to be found by reading the trap path; and on x86-64 the four
trap-stub words plus the GDT, TSS and syscall kernel stack, the words reached
`GS`-relative with **no `swapgs`** (nothing in this tree ever gives a cell a GS base -
`arch_prctl(ARCH_SET_GS)` is refused - so `GS_BASE` stays the kernel's in both rings; a
cell reading `%gs:` faults on a supervisor page exactly as it did when the base was 0),
plus the per-core registers no memory change substitutes for - IDTR, GDTR, TR, the
SYSCALL MSRs, CR0/CR4/XCR0 - which the AP trampoline set none of, so a secondary had no
handlers at all and its first exception was a triple fault. Both RISC-V halves were
observed **failing** when reverted. Safe by **partitioning, not locking**: distinct cell
slots, address spaces, kernel stacks and pages, and the cell table read/written at
disjoint indices - the multikernel answer (docs/SCHEDULING.md 1a), not a shortcut.
**And cells are now *placed*, not assigned.** `smp::start_all` brings up **every**
secondary the firmware enumerates (each on its own stack indexed by its own hardware id,
with a probe-the-next-id fallback where EL1 can enumerate nothing - ARM64), so **4 CPUs
come online on all three ISAs**; `smp::place_cells` then publishes a set of runnable
cells that **every core claims from whenever it is free** - work-conserving, balanced by
claim rate, nobody assigned anything in advance (the GEMM block-queue reasoning applied
to cells instead of rows). The proof queues **more cells than cores** (8 on 4, one
deliberately long) and asserts every cell finished on some core carrying its own exit
code, that more than one core claimed work with the counts summing to the queue, and
that **some core claimed a second cell** - which a one-per-core hand-out cannot produce.
Observed on all three ISAs: the core that took the long cell takes exactly 1 and the
rest take 2-3 (reported, never asserted - TCG time-slices the vCPUs onto host threads).
**And every core preempts its own cells.** A core claims a *batch* (2 - one cell has
nothing to preempt *to*) and runs it under **its own** preemption timer, brought up by
that core because every register involved is per-core hardware no trampoline sets:
RISC-V's `stimecmp`/`sie` CSRs; ARM64's own GICv3 **redistributor**
(`GICR_BASE + aff0 * 0x20000` - and the old global "GIC is up" flag covered the CPU
interface too, so a secondary would have had none) plus its CNTV PPI; x86-64's
`IA32_APIC_BASE` enable, TPR and **SVR software-enable** plus the LAPIC timer registers,
while the *discovery* half (APIC-mode probe, IDT gate, one-shot self-test) stays global
work the primary does once. The cells issue **no syscall at all**, so the cooperative
placement round immediately above is the negative control and is asserted to have taken
**zero** preemptions; with slices armed, **344-405 of ~700-820 slices take the CPU on 4
cores at once** on all three ISAs. Two shared-state fixes landed with it: the `preempt`
and `dispatch` counters became relaxed atomics (a `static mut += 1` is a lost update once
every core dispatches), and the native `schedulable` predicate gained an **affinity
test** - a cell belongs to one core (`user::claim_cell`/`cell_on_this_cpu`) so no other
core's pick can see it, with an unclaimed cell visible to everyone, which is exactly the
single-CPU behaviour. It also surfaced a **third instance of the SYSRET-provenance
defect**: `enter_user_first` resumed through `sysret_resume`, which consumes RCX/R11 -
invisible while every frame it saw was fresh or syscall-stopped, fatal the moment a core
re-enters the survivor of a batch through a frame a *timer interrupt* captured. Not a
fault: four cores resumed with two corrupted registers and their bounded loops stopped
terminating. Found by reading the resume path once instrumentation localised the hang;
`enter_user_first` uses `iret_resume` now, and the rule is stated once - **SYSRET is only
for returning from the syscall it was entered by**.
**And an unmodified Linux binary runs as a cell on a secondary**: `chello`, the same
static-glibc C binary `linuxrun` asserts on the primary, runs as a `Personality::Linux`
cell on a secondary with its **exact stdout and exit code asserted**, while a native cell
runs on the primary and the two are held to have overlapped by the rendezvous. That is
one Linux cell at a time - the global mapped-file/pipe/eventfd/pid registries then have
exactly one writer, so the docs/SMP.md 10.2 audit question is not being asked - but the
narrower unknown, whether the Linux syscall path works at all off the boot CPU, is
answered. It needed one more per-core register set, found the same way as the others:
**RISC-V's `sstatus.SUM`** (plus `FS`/`scounteren`), set once by `paging_kernel_init` on
the primary, without which a secondary runs cells fine until the kernel first *touches*
one of their pages and then takes a store page fault at a kernel PC on a correctly-mapped
user page. It is `arch::user_mode_init_this_cpu` now, empty on ARM64/x86-64 (their
equivalents are already adopted per core) so the portable caller need not know which ISAs
need it. `start_all` also exposed a latent race: the single-cell hand-off published its
index with a plain load-then-store, so two secondaries could both take it - one cell, two
cores, one trap frame, presenting as two cores faulting at PC 0 intermittently; it is an
atomic `swap` now. The **personality lock** docs/SMP.md 10.2 names as the first step is
also in place (`linux::plock`: one lock over the whole Linux dispatch plus the
demand-paging entry, **recursive per CPU** - a syscall reaches `fill_fault` through
`uaccess`, so a non-reentrant lock self-deadlocks there - and **not taken at all** on a
single-CPU boot, so every pre-existing kernel's hot path is unchanged). **And FOUR Linux cells run across four cores** (docs/SMP.md 10.0b) - the
docs/SMP.md 10.2 audit's own remaining question, asked. The audit names the Linux
personality's global state (the mapped-file registry, the pipe/eventfd/timerfd registries,
pid allocation, the unix-socket names) as one of six areas, and `linux::plock` covers the
whole dispatch plus the demand-paging entry recursively per CPU - the "one big lock" order
10.2 explicitly allows. Two cells were proven; **many** was not, because a big lock is
exactly the claim that holds for two and fails for N if anything touches a global outside
the locked window. Four Linux cells now go through the same placement queue every other
multi-core phase uses, one per core, each demand-paging its own copy of the same unmodified
static-glibc binary, each synthesizing its own pid, each transcript captured separately and
asserted **exactly**, all four exiting 9 - on **all three ISAs** with all 4 cores taking one.
It widens `place_cells`' contract from "native" to "native **or a Linux cell with no process
tree**". **That phase alone does not prove the lock load-bearing** - forcing `plock` to
return `PGuard::Off` still passes on all three ISAs, because `chello` barely touches the
registries and TCG interleaves coarsely - **but the fixture that closes it now exists**, below.

**And the Node/Bun/Claude-Code load path runs off the boot CPU** (docs/SMP.md 10.0e): the
other Linux multi-core phases run static binaries out of the kernel image, while the real
workloads stream off a live ext4 disk, have `ld.so` map `libc.so.6` with file-backed `mmap`,
and take every page by fault - so "can those run on a secondary" is a question about **that
load path**, askable with `dhello` (the 20 KB dynamic hello `linuxdyn` proves on the primary)
for a fraction of their size. Proven on **all three ISAs**: `dhello` is loaded off the live
`dyn-disk.img` and run as a Linux cell **on a secondary**, overlapping a static `chello` on
the primary, exact transcript and exit 12 asserted, with ~576 block-cache fills *during the
run* - so its interpreter and libc really came off the device on demand from that core.
Exercised off the boot CPU: virtio-blk, the bounded block cache, `ext4plus` path resolution,
`PT_INTERP` + the ELF interpreter, file-backed `MAP_PRIVATE`/`MAP_FIXED`, and demand paging,
with the faults taken on a secondary's trap path using its own kernel stack. It uses
`run_cells_on_both` (a **named** cell to a secondary) rather than `place_cells` (where which
core takes which is a race), and that was not cosmetic: the first version used `place_cells`
and asserted only that the cells landed on *different* cores - the run put the dynamic cell on
the **primary**, so the assertion passed while its own message was false. **And the real Bun runs on a
secondary too** (`linuxbunsmp`): the same binary, disk, JIT authority and preemptive dispatch
as `linuxbun` and the same strict gate - it streams off the live ext4 disk (~9,200 block-cache
fills), brings up JavaScriptCore with its JIT behind the W^X exception, takes **83 preemption
slices on that secondary**, prints exactly `rheo:42` and exits 0 (x86-64 only, as `linuxbun`).
**The prediction written down first was wrong, instructively**: it said this needed *threads of
one Linux cell across cores*, because Bun spawns a worker - it does not, since Bun's contexts
are scheduled cooperatively *within* whichever core runs the cell, exactly as on the primary.
Two things the first run got wrong, both found by reading its output: `run_cell_on_secondary`
reused the 2 s **rendezvous** bound to wait for a whole *program*, and reported "no secondary
came up" for a Bun that had already taken its JIT grant (it has its own 100 s bound now); and
the secondary ran the cell **cooperatively** (`0/24` slices) because a preemption timer is
per-core hardware no trampoline sets - it arms its own now (`83/6243`). **And so do Node.js and Claude Code**
(`linuxnodesmp`, `linuxclaudesmp`) - same construction, `on_secondary` the only difference:
Node (124 MB, V8 + libuv) at ~15,300 block-cache fills and 23 of 9,477 slices prints `rheo:42`
and exits 0; **Claude Code** (275 MB, Bun-compiled) at ~116,300 fills and **1,612 of 61,701
slices** prints exactly `2.1.220 (Claude Code)` and exits 0. Each is its own kernel rather than
a phase, deliberately: the primary-CPU proof is the baseline every claim about these runtimes
rests on, and a boot that runs one somewhere else must not be able to weaken it. Still not
shown: these are the same *cooperative-within-a-core* runtimes they are on the primary, so
running one workload's threads on several cores **at once** needs 10.2's per-cell locking,
which is not built. What the six kernels establish is that the core a workload runs on is no
longer special - not that one workload can use several.

**And a Linux cell FORKS off the boot CPU** (docs/SMP.md 10.0c) - the other half of that
question. `fork` creates a **new cell**, and an unclaimed cell is pickable by every core
(`cell_on_this_cpu` treats `NO_CPU` as pickable, which is what keeps single-core boots
unchanged), so when a Linux cell on core B exits its `linux::proc::reschedule` scan would
find the child core A forked a moment ago - two cores, one trap frame. An **idle** core
cannot reach it (`drain_cells` only enters published cells), so it takes two Linux cells one
of which forks. The fix is the one `cell_on_this_cpu`'s own doc predicted:
`install_forked`/`install_spawned` give the child **its parent's owner** - the same
partitioning applied to a cell that did not exist when the round started, not a wider lock.
Proven on **all three ISAs**: `af_unix` (an unmodified static-glibc
socketpair+fork+bind/listen/connect/accept fixture, so it also drives the global unix-socket
registry and the L6 ring from a secondary) runs on one core while `chello` runs on another,
both exact transcripts and exits asserted, zero double entries, and `user::affinity_skips()`
asserted **nonzero** so a scheduler really was offered another core's cell and declined it -
the positive form, since "no double entry" would pass equally if the race never arose.
**Not proven, and recorded**: that the *child's* inherited owner is what prevented a double
entry - reverting it still passes over five runs, and the refusals counted come from the two
*placed* cells rather than the child, because the window (the peer's exit-time scan landing
between the child's creation and its reaping) is narrow.

**And the registries are proven serialised under load** (docs/SMP.md 10.0d) - what makes
`linux::plock` a *proven* mechanism rather than a present one. Both phases above pass with the
lock removed, and said so; the reason is the workload, since a lock that is never contended is
indistinguishable from no lock. `tests/linux-fixtures/regstress.c` aims at the two registries
whose allocators are **find-a-free-slot-then-claim-it** (`linux/pipe.rs`, `linux/eventfd.rs`),
a shape that races directly - two cores can both find the same free index and both claim it,
leaving two processes holding one object, whose detectable consequence is not a fault but
*someone else's bytes*. So every value it writes is keyed on its own pid and every read is
checked against it: 256 rounds of pipe create/write/read/close and eventfd
create/write/read/close. Two of them run on two **different** cores (asserted, so they really
are inside the allocators together) and each reads back exactly what it wrote, on all three
ISAs - **and the control fires**: with `plock` forced off, a cell prints `regstress FAIL 5`,
five rounds in which it read a byte the *other* cell wrote. Honest about the reach: the global
registries are now shown serialised under genuine concurrent load, but it is still one big lock
over the whole dispatch - the finer per-cell locking docs/SMP.md 10.2 describes, which threads
of *one* Linux cell across cores would need, is not built. All three Linux multi-core phases
live in their own **`linuxsmp`** kernel (the 67th) rather than in `smp`, for a measured reason:
each runs several static-glibc images through a full glibc startup with demand paging, and
adding them to `smp` pushed **riscv64** past the 120 s boot budget - observed, timing out inside
the four-cell phase before the other two ran.

**And two Linux cells now run on two cores at the same time** - the same unmodified
static-glibc binary as both cells, each transcript captured separately (the stdout tap
keys on `user::current_index()`, which is `PerCpu`) and asserted exactly, each exiting 9,
on all three ISAs. Two defects had to go, and the first is recorded because the initial
diagnosis was **wrong**: the phase failed and appeared to fail single-core too, so it was
written up as a personality-state bug - but reproducing it in a kernel with no
secondaries showed two Linux cells install and run serially without a murmur, and the
garbled console that made the single-core run *look* broken was the secondaries. The real
faults were (a) `place_cells` reporting a round finished while a core was still unwinding
inside it, so the caller's next `user::reset()` freed cells under it (a `BUSY` count with
an RAII guard now quiesces every core first), and (b) **the Linux scheduler had no
CPU-affinity test** - `nproc::schedulable` got one when native cells reached secondaries;
`linux::proc`'s three runnable predicates did not, so the primary's exiting cell could
reschedule *into* the cell the secondary was running, presenting as an instruction fetch
at PC 0 in kernel mode on two cores. The lesson is the one this tree keeps relearning:
the first reproduction was in an environment that added its own noise, and reproducing in
the *quietest* environment that can host the bug answered it in one run.
**And a core that runs dry rebalances work out of a peer's claim**: claiming divides
work by arrival and once divided it stays divided, so a core whose cursor is exhausted
takes a cell some peer has **claimed but not started**. One exchange is the whole
protocol - exactly one core can turn a slot's run-mark 0 -> 1 and only that core may
enter the cell, the previous owner discovering the loss when it reaches the slot - so
there is no window in which two cores both enter. With one deliberately long cell among
short ones the steal is **asserted**, not hoped for (a round without it produces the same
exit codes and teaches nothing): 1 rebalanced, busiest core taking 3 of 8, on all three
ISAs.
**And one cell now runs on two cores at once** (docs/SMP.md 10.0a - *vcores*): every phase
above runs **different cells** on different cores, which is real parallelism and is not the
parallelism a *program* has - a Node worker, a strand pool, an FA3 producer/consumer pair
are all one address space that wants several cores, and a cell belonged to one core. The
reason it did is stated in `claim_cell`'s own doc: two cores in one cell would share its
trap frame, its kernel stack and its FP/SIMD save area, none of which is locked. So the fix
is **not a lock** - it makes those three per **vcore** and moves the ownership claim down
with them, at which point the vcore is the unit that is partitioned exactly as the cell was
and the multikernel argument holds one level lower unchanged. `RunCell` carries
`vframe`/`voutcome`/`vcpu` arrays instead of three scalars (slot 0 is the context `install`
builds, so a cell nobody adds a vcore to holds `nvcores == 1` and every pre-vcore path is
byte-for-byte what it was), `CELL_FP` is one area per `(cell, vcore)`, `CUR_VCORE`/
`EXITED_VCORE` are `PerCpu` for the reason `CURRENT` is, the double-entry guard keys on
`cell * MAX_VCORES + vcore`, and `smp::place_vcores` publishes **vcore ids** into the same
queue `place_cells` uses - one drain loop, one claim protocol, one steal protocol, since a
cell index is just the vcore id of its vcore 0. **No new kernel object and no new verb**: a
vcore is an execution context of the Cell object, the shape the Linux personality's contexts
already are, and `install_vcore` is a *launcher* verb because creating a context creates
something the scheduler must own - the cell-facing spelling is the strand runtime asking for
a vcore, which belongs with the process model. Proven by the `smp` kernel on **all three ISAs**: two vcores of **one**
cell go into the placement queue, whichever cores are free claim them, and both are asserted
to exit 0, complete all 64 rounds, run on **different CPUs** (without which the phase passes
with them run back to back - the codes and counts are identical either way) and **each see
the other advance mid-run**, which one CPU cannot produce. Two of the four per-vcore pieces
are proven load-bearing by observed reverts (`vframe[v]` -> `vframe[0]` makes vcore 1 never
finish; `voutcome[v]` -> `voutcome[0]` panics); the other two are **construction requirements
this phase cannot detect**, and that is recorded rather than glossed - a shared kernel stack
**passes on all three ISAs** even with a trap every round, because both cores run the same
short handler and overwrite each other's spills with identical bytes, and a shared FP area is
invisible because each vcore is entered once and exits once on its own register file (the path
that exposes it is preemption of a multi-vcore cell, which is refused). `MAX_VCORES` is 4
because the FP areas are a fixed static (256 KiB of `.bss` on x86-64); funding them through
`mm::kmeta` is what removes the number.

**And a vcore yields to its sibling** (docs/SMP.md 10.0a): a cell with more vcores than
cores is the ordinary case the moment a program asks for eight workers on four cores, and
the first version refused it - the cooperative schedulers pick a *cell* and enter its vcore
0, so `cell_on_this_cpu` refused multi-vcore cells outright rather than enter a context
another core owned. The fix is the predicate one level down: `user::vcore_on_this_cpu(cell,
v)` answers **per vcore** (a vcore belongs to one core; two cores in two *different* vcores
of one cell is the point, not a hazard), `cell_on_this_cpu` reverts to asking about vcore 0
- the right question for a path that enters vcore 0 - and `SYS_YIELD` tries a **sibling
vcore of the same cell first** (round-robin from the running one, so N vcores share a core
evenly), for the reason the Linux preemption path tries a sibling context first: it is the
cheaper move by a wide margin. `user::switch_native_vcore` is the whole of it - **one
address space, so no `activate()` and no TLB consequence**; only the FP/SIMD register file
and the frame change hands, which is the economy of a vcore over a second cell stated as
code. `switch_native_cell` now saves the vcore this CPU is actually *inside* and loads the
target's vcore 0 rather than assuming 0 on both sides (with `from` fixed at 0, a core inside
vcore 1 would write vcore 1's live registers into vcore 0's saved image), and the per-CPU
entry guard became one owner with two callers (`user::enter_vcore`, from `run_inner` and
from `switch_native_vcore`) - **not** from `switch_native_cell`, where marking was tried and
produces a *false* double-entry, because `INSIDE` is written on entry and cleared on return
by `run_inner` while a cross-cell chain sits inside that bracket. The proof is deliberately
**single-core** so the oracle is exact: each round of each vcore is one append to a shared
order vector then one yield, so two vcores must produce a strictly **alternating** 12-marker
vector - which only a yield reaching the sibling context can produce - and reverting the
sibling path gives **6 markers, not 12** (observed). The **owner check** is proven by the
two-core phase instead, which is why its witness now issues a `SYS_YIELD` per round rather
than a counter read: with the two vcores owned by different cores every round asks "is a
sibling enterable" and the answer must be no, and replacing the check with a bare bounds
test makes the guard fire by name (observed). One thing asserted rather than fixed: **the
first vcore to exit unwinds the run**, which is the correct rule for a cell and not yet a
rule for a cell with several vcores - "the cell exits when its *last* vcore exits" is the
missing semantics, and the phase pins vcore 1's flag at 0 so that rule arriving shows up as
a test change instead of silently. 
**And a vcore blocks** (docs/SMP.md 10.0a): `nproc`'s block state was per *cell*, so one
context parking on a timer recorded the wait for all of them - a cell with a runnable sibling
looked blocked and the scheduler idled the machine with work available, the defect the Linux
side already fixed one level up with per-context `pblock`. The fix mirrors the existing
two-phase shape rather than inventing one: `Proc` carries `vblock`/`vparked`/`vwait` arrays
(`vparked` being the per-vcore analogue of `state == Blocked`, kept separate from `vblock`
because `wake_satisfiable` clears the flag while `complete_block` clears the block later with
the woken context's address space active), the cell-level `PState::Blocked` now means
**every** vcore is parked - so a single-vcore cell's transitions are byte-for-byte what they
were, which is why all 66 kernels stayed green - `refresh_deadlines`/`blocked_sources`
iterate parked `(cell, vcore)` pairs rather than gating on the cell's `Blocked` (a cell with
one parked vcore is not blocked and its parked context's deadline still has to be armed - the
arming is what wakes it), `reschedule` picks a `(cell, vcore)` so a woken vcore is re-entered
at *that* context, and `can_reschedule` counts a runnable sibling vcore or one parked on a
waitable source. Proven on **all three ISAs** by the `schedidle` oracle one level down: the
same `user_blocker`/`user_peer` programs as two **vcores of one cell** produce the same exact
order vector **`bSSSSSSSSB`** - blocker parks on 20 ms, sibling takes all 8 rounds strictly
between the two blocker markers, arbiter's one-shot wakes the blocker - and restoring the
per-cell park makes the sibling run **zero** rounds (observed). That phase **found two real
defects**: `next_sibling_vcore` checked ownership but not *parked*, invisible while a vcore
could not block, so a yield entered a sibling parked mid-`SYS_ARM_TIMER` and resumed it at
its syscall return with the return register still holding the syscall **number** (no fault,
no log, `SYS_ARM_TIMER returned 47, want 0`); and `drain_cells` stamped per-vcore ownership
for a whole batch *before* winning any run-mark, so a core holding two vcores of one cell
could enter the sibling while the stealer that took it was already inside - ownership is
stamped where the run-mark is won now, the reasoning `count_claim` beside it already carried,
and the per-CPU entry guard named the pair instead of letting it corrupt downstream. 
**And each vcore has its own queue pair** (docs/SUBSTRATE.md S5, docs/SMP.md 10.0a): a
submission queue is **single-producer**, so two contexts sharing one must serialise their
submissions - and once those contexts run on two cores that serialisation is a cross-core
write to shared ring indices, the cost the io_uring-per-thread shape exists to avoid. So
`RunCell` holds `vqp`/`vqp_va`/`vqp_cap` arrays, `SYS_DOORBELL` drains the ring of the
**calling** vcore, `SYS_QUEUE_INFO` reports the calling vcore's own region and capability,
and `install_vcore` takes the new context's ring beside its frame (slot 0 is what `install`
was handed, so a single-vcore cell is unchanged). The cell-facing shape is the point: a
context does not have to be *told* which ring is its own - it asks, and the answer is per
vcore, so the same binary in two contexts binds two different regions with no code in it
that knows vcores exist. Proven on **all three ISAs** as two separate claims: the rings are
**disjoint** (each vcore reports a different region VA and a different capability id, each
matching what its launcher initialised - reverting `SYS_QUEUE_INFO` to `vqp_va[0]` fails on
the capability, observed), and each ring **completed its own round trip on its own core**
(both vcores go into the placement queue, each submits an `OP_ECHO`, rings its own doorbell
and reaps `STATUS_OK`, and the two are asserted to have run on different CPUs - reverting
the doorbell to `vqp[0]` leaves vcore 1's round trip uncompleted, observed). Together those
say a submission never left its core: there was no shared ring to cross into. Honest: the
ring **overlay** a cell submits through still comes from its launcher, because building one
over a region is `QueuePair::attach`, which lives in kernel `.text` a cell has no mapping
for - so `SYS_QUEUE_INFO` proves per-vcore *reporting* while the round trip proves per-vcore
*servicing*, asserted separately rather than conflated; and `load::map_queue` still places
one ring at `USER_QUEUE_VA`, so a **loaded** cell wanting a second vcore needs
`USER_QUEUE_VA + v * REGION_SIZE` - one line, deliberately unwritten until a loaded cell
asks, since nothing would test it. 
**And the last vcore out ends the cell** (docs/SMP.md 10.0a): the first vcore to exit used to
unwind the run - correct for a cell, unusable for a cell with four vcores, where the first one
finishing kills the other three. `SYS_EXIT` now ends the calling **vcore** and
`SYS_EXIT_GROUP` ends the cell, the process/thread split the Linux personality already has one
level up. A vcore's outcome *is* its liveness (`user::vcore_live` = `voutcome[v].is_none()`), so
every pick already asks, and `all_parked` counts only live vcores - an exited one is neither
parked nor runnable, and counting it as unparked would leave the cell `Runnable` with nothing to
enter. `nproc::retire_vcore` hands the CPU to a sibling **this core may enter** (live, owned
here, and either runnable or parked on a waitable source - the same two-part rule
`can_reschedule` uses) and returns `None` otherwise, unwinding with this vcore's own outcome.
That condition is the whole correctness of it and **both halves were found by the tests**: an
unconditional `reschedule` returns `DEADLOCK_EXIT` when every live sibling belongs to another
core, because a core with nothing to do is not a deadlocked machine; and without
`ensure_tracked` the hand-off finds no `Proc` entry for a cell that has never spawned or
blocked, and ends the run in `DEADLOCK_EXIT` too. The yield phase asserts the rule directly -
**both** vcores reach their exit and the run is ended by vcore **1**, the last one out - and
suppressing the hand-off makes vcore 1 never reach its exit (observed). This is the assertion
the previous slice deliberately pinned at 0 so the rule's arrival would be a test change rather
than a silent one; it fired. `SYS_EXIT_GROUP` takes the pre-existing path unchanged, so nothing
new is claimed for it. 
**And the userspace allocator is multi-core safe** (docs/SMP.md 10.0a) - the prerequisite the
userspace half sits on, and a genuine latent defect rather than a feature. `runtime::Heap` is
the `#[global_allocator]` every `alloc`-using cell and test kernel binds, and it carried a bare
`unsafe impl Sync for Heap` justified by *"single-CPU kernel; no concurrent access to the
allocator"* - true when written, false from the moment two cores ran cells, which is the kind of
**inherited claim that has to be re-checked rather than trusted**. A free list is the worst case
for getting it wrong: every operation reads and writes several links, so two concurrent
allocations hand out **overlapping blocks** and the symptom is not a fault but two owners of one
buffer writing over each other. It is behind `runtime::lock::TicketLock` now - the lock whose
own doc said it was "for the future multi-vcore case" - **unconditionally**, because whether a
structure needs a lock is a property of the structure and not of which cargo features are
enabled (the call `mm::frames` and the NVMe driver already made); an uncontended acquire is two
atomics against a free-list walk that is already several dependent loads. Proven by the `smp`
kernel on **all three ISAs**: each core runs 512 allocate / stamp-every-byte /
read-every-byte-back / free cycles through the global allocator, meeting at a rendezvous first
so the overlap is real - a block handed to both cores means one core's marker lands in the
other's block between the write and the read, caught **directly** rather than by a proxy - with
0 mismatched bytes, 0 pointers outside the region, both cores completing every round, and the
list still serving afterwards. Reverting to the unlocked `UnsafeCell` + `unsafe impl Sync`
produces a **general protection fault inside the allocator** (observed), the same defect in its
unsurvivable form. 
**And strands now run across vcores** (docs/CONCURRENCY.md): `runtime/`'s executor was one
global `Executor` behind a `static mut`, so a cell's strands lived on one vcore however many
the kernel gave it. The obstacle was an **API question, not a port** - `spawn` returns an
`Rc`-based `JoinHandle`, and an `Rc` cannot cross cores, so a queue any vcore drains cannot
hold what `spawn` produces. The split a `Send` bound forces: **`spawn` stays vcore-local**
(same signature, same `Rc` handle, sound because a strand spawned on a vcore stays there - every
existing caller unchanged), and **`spawn_shared` takes a `Send` future and returns no handle**,
both restrictions being one fact, with a caller wanting a result using a channel or an atomic as
a work-stealing pool does anyway. Each vcore's executor is `EXECS[v]`, safe by **partitioning**
rather than by a lock (a vcore belongs to one core at a time, so two cores are two disjoint
elements - the `PerCpu` argument); the runtime cannot know which vcore it is on, having no
register to read, so the embedder supplies an accessor once (`set_vcore_hook`) and unset it is a
constant 0, so every pre-vcore caller is unchanged. The injector is a `TicketLock<VecDeque<_>>`
rather than per-vcore stealing deques, and the doc says why: with a handful of vcores it is not
the structure a many-core deque solves, and whether it becomes one is a measurement there is no
hardware here to take. Proven by `smp` on **all three ISAs** in two sub-phases, because they
prove different things and only one is deterministic - **concurrent**: both cores drain at once,
each of 64 strands asserted to run *exactly* once (a strand delivered twice fails) and the two
take counts asserted to sum to exactly 64 (so every strand came off the shared injector, not a
local queue), with the split **reported, never asserted** (22/42, 37/27, 35/29 - one core
draining first is a legal schedule, and an assertion that can fail on a legal schedule is not a
proof); and **directed**: the primary spawns and does not drain, the secondary asserted to take
*all* 64 with the primary taking none, which is the crossing itself. Reverting `run` so it never
takes from the injector leaves every strand unrun; collapsing the per-vcore executors to one
**hangs**, two cores corrupting one run queue (both observed). 
**And a cell can ask which vcore it is** - **`SYS_VCORE_INFO` (54)**, the session's one new
verb, reporting `(index, count)`, with `librheo::sys::vcore_index` shaped as `fn() -> usize` so
it hands straight to `set_vcore_hook`: the runtime is *told* its index rather than inventing
one, and this is the telling. The **admission audit is written out at its definition** in
`abi/` per ARCHITECTURE.md 6 - it adds **no object** (a vcore is an execution context of the
Cell object, so this is a verb over object 1 exactly as `SYS_QUEUE_INFO` is over the QueuePair
and `SYS_CONNECT` over the channel), it **cannot be a library** (a cell has nothing to compute
its own index from: there is no register that says "you are context 1 of your cell", and it
must not be able to claim a different one since every per-vcore structure keys on it), and it
is **two integers with all policy outside**. Proven by `smp` on **all three ISAs**: the *same
binary* in two contexts of one cell reads indices 0 and 1 and a count of 2 - a per-cell reply
would give both the same and a hardcoded one would give both 0, and reverting the index to a
constant fails by name (observed). 
**And a loaded cell runs the multi-vcore executor** (docs/CONCURRENCY.md) - the assembly of
every piece above. **`librheo-vcore`** is one ELF in two contexts of one address space, each
with its own ring (`load::map_queue_for`, at `load::vcore_queue_va(v)`) and its own user stack
(`load::map_vcore_stack`, below vcore 0's with a one-page guard gap). Both enter at the **same
ELF entry point** - the loader resolves no symbols, and giving a cell two entry points would
make it read a symbol table - so librheo's crt0 branches on `sys::vcore_index()`: the secondary
skips one-time process setup, because re-running `init_heap` would reset the allocator's free
list under a sibling already using it and re-seeding the DRBG would hand two contexts the same
stream. **The cell is not told its role by its launcher; it asks.** Its claim is the
deterministic one: vcore 0 fills the injector and **never drains it**, so every strand that ran
was executed by a context that did not create it - and the cell checks that itself (32 strands
each exactly once, `shared_taken(0) == 0`, `shared_taken(1) == 32`) before ending the cell
`0x42`, with every other code (31..38) naming which check failed. Proven on **all three ISAs**.
Two findings from building it: a secondary can be entered **before the primary has executed one
instruction** (placement publishes every vcore at once and whichever core claims first enters
first - the first version assumed otherwise and the cell never finished), so crt0 gained a
`PRIMARY_READY` flag the secondary *yields* on, bounded; and a secondary returning from `main`
must use `sys::exit_vcore` (`SYS_EXIT`) rather than `sys::exit` (`SYS_EXIT_GROUP`), which would
take its siblings down mid-work. **Honest about this phase's reach**: load-bearing and observed
failing are the per-vcore user stack (sharing one wedges both contexts, code 38 twice) and the
crt0 secondary path (without `PRIMARY_READY` the run does not complete); *not* proven here is
the per-vcore **ring**, since this cell's strands touch only atomics and ring no doorbell so
collapsing both rings to one VA still passes - the ring is proven by the `smp` kernel's own
per-vcore-queue phase instead - nor the `exit_vcore` split, whose effect here is
race-dependent. Still named as not done: a vcore that forks or takes a signal, per-vcore
stealing deques, and a cell that **asks for** its own vcores (the launcher installs them, the
same launcher-mints-authority shape as the queue pair and the W^X exception; a cell-facing
`spawn_vcore` is a separate design question).

**And an address-space switch no longer flushes the TLB** (docs/SUBSTRATE.md pillar 2 /
S2): ARM64 and RISC-V already *tagged* entries with an ASID and then invalidated that very
tag on every `paging_activate`, so the tag bought nothing; x86-64 reloaded CR3 and flushed
every non-global entry. A switch between two address spaces now performs **no** TLB
maintenance at all - the tag is what keeps one cell's translations from answering for
another - and the flush moves to the two places that genuinely need it: a tag handed to a
**new** root (`AddressSpace::new`, once per root) and a batch of **mutations** to an
existing one (`AddressSpace::dirty`, checked in `activate`). On x86-64 that means
`CR4.PCIDE` plus a CR3 whose bit 63 says "keep this PCID's entries", enabled **per core**
(CR4 is per-core hardware) and only where CPUID reports PCID **and** `INVPCID` - both,
because with PCIDE on a plain CR3 load no longer flushes the tag, so without `INVPCID`
nothing could; the latch is read back and `paging_tlb_tagged()` reports what was observed,
so a CPU without it keeps the untagged path and says so (QEMU 8.2's default model here
reports no usable tag, and x86-64 prints that rather than claiming one). **The mutation
half was found by the suite, not by reasoning**: removing the per-switch flush made
`librheotilebattle` run the same tile program twice and get **different** results, because
a cell's heap growth mapped frames the TLB still had stale entries for - the hazard
docs/SMP.md 10.2 names as "`AddressSpace` mutation races a concurrent fault", which the
per-switch flush had been covering by accident. `librheoipc` asserts the exact claim -
**flushes < switches**, which were *equal by construction* before - observing 25 switches
at 4 flushes, all four from real mutations; restoring the per-switch flush makes it fail.

**Two more global allocators are locked** (docs/SMP.md 10.0f): re-reading the shared
`static mut` list after `frames` was locked turned up its twin **`mm::frames_pmem`**
(same bitmap-plus-hint read-modify-write) and **`sched::SYSTEM`**, the machine-wide
admission ledger, which was reached through a `pub fn system() -> &'static mut
Admission` - a `&mut` handed to every caller on every core, over a read-modify-write
whose lost add is precisely the over-commit the ledger exists to prevent. Both are
behind a `SpinLock` now, **unconditionally** rather than `#[cfg(feature = "smp")]`,
and the ledger's `&'static mut` accessor is gone in favour of
`system_admit`/`system_release`/`system_committed_ppm` so the check and the commit
happen under one acquire. The `smp` kernel runs both cores through 4096
admit/sample/release rounds with exact oracles (every admit succeeds, the total
sampled *inside* the hold is always one or two holders' worth, the ledger returns to
0 ppm) - but this is an **honest non-result**: removing the lock and running 400,000
pairs produced zero lost updates, because the critical section is a handful of
instructions and TCG's interleaving is far coarser, so the phase asserts the invariant
while the lock's necessity stays argued from the structure and gated at the lab.

**And the frame allocator is a batch, not a queue** (docs/SMP.md 10.0g): 10.0f closed the
*safety* question for the global allocators; this closes the *scalability* one for the
hottest of them. Four changes, of which the first is worth more than the other three and
would have been worth doing with no lock in sight. **(1) The 4 KiB zeroing left the
critical section.** Every `alloc` set a bitmap bit and three fields and then zeroed 4096
bytes *inside* the lock, so the critical section was ~99% `memset` and a core allocating
waited on another core's `memset` rather than on anything shared. It is safe to move out
for one structural reason - **the bitmap bit is what makes a frame unhandable**, so once
it is set the window before the zeroing is private to the claiming core by construction
rather than by timing. **(2) `alloc_on` takes one acquisition instead of three** (it read
the node's frame range, searched inside it, then fell back to the whole pool, each
separately - and a range read that is not in the same critical section as the search it
bounds is a range that can change under it). **(3) A copy-on-write break takes two
instead of three**: "is this shared" and "give me somewhere to copy it to" are one
decision, now `frames::cow_resolve` -> `Sole`/`Private(dst)`/`NoFrame`, whose `Private`
frame is deliberately **not** zeroed (the caller overwrites all 4096 bytes, so the
no-leak rule is met by the copy) and whose old-frame release is deliberately **not**
folded in - dropping the reference before the copy would let a peer core faulting on the
same page see a count of one, decide it was the sole owner, and write through the frame
this copy is reading, so two is the floor. **(4) Flat combining** (Hendler/Incze/Shavit/
Tzafrir, SPAA 2010): with the `memset` gone the lock is held for a few words, which is
exactly the regime where N cores each taking a short lock pay N cache-line handoffs for
trivial work - so a core publishes to its own 64-byte-aligned slot, one core wins the
*combiner* role, takes the lock once, executes the whole batch with the bitmap in its
cache, and writes each result back. Two shape decisions matter: **the uncontended path
does not publish at all** (a core tries the election first, and winning it - always, on a
single-CPU boot - means doing its own work directly and then draining a pending mask that
is normally zero, so the added cost is two atomics and one relaxed load and there is no
separate "SMP path" to diverge from), and **the batch does not zero** (each core zeroes
its own frame afterwards; a combiner zeroing for the batch would re-serialise exactly what
change 1 unserialised). Executed **exactly once** is enforced by a claim rather than
argued - a publisher may withdraw only by moving its slot from its own opcode straight to
idle, which fails once a combiner has moved it to *busy* - and the withdrawal, which
exists because a publication can land just after the combiner sampled the mask, is
counted so a machine that needs it says so. The boot-time and diagnostic paths
(`init_numa`, `alloc_contig`, `stats`, `used_matches_bitmap`, `node_free`, `node_of`) stay
on the plain lock. Proven by a new `smp` phase on all three ISAs, mirroring the
shared-heap phase because the failure mode has the same shape: each core loops
allocate/stamp-every-byte/read-back/free after a rendezvous, so a frame handed to both
means one core's stamp lands in the other's frame between the write and the read - the
corruption itself, not a proxy - with the counter still matching the bitmap and no frame
leaked; then a **churn** pass (allocate and free, touching nothing) because the verify
pass spends far longer outside the allocator than inside it, across which **140-469
requests were executed by the other core's combiner** (riscv64 315, aarch64 140, x86-64
469) with **1-6 withdrawn** - and the withdrawal count matters more than its size, since
it means the liveness backstop and the `FC_BUSY` claim it interacts with are exercised
rather than dead code, a withdrawal racing a claim being the one interleaving in which a
request could run twice. That number is
**reported, never asserted** (zero is a legal schedule, and TCG interleaves coarsely) and
it stays modest *by design*, which is the point: change 1 left very little window for a
second core to arrive in, so shortening the window removed most of the contention and
combining handles what is left. **The cost is measured, and the reading is honest**: the
bench suite touched none of these paths, so five benches were added, and against the
pre-change tree the layer costs **+11 instructions per operation on x86-64 and +17 on
riscv64** (isolated by `frame_contig1_free`, whose `alloc_contig` half did not change
layer) - +2.8% and +1.6% of a full `alloc`+`free`, because the 4 KiB `memset` dominates,
which is the same fact as change 1. So **on a single-core boot flat combining is a net
loss in instructions**: it buys nothing there and costs 11-17. Two things icount cannot
price cut the other way and are named as lab claims rather than assumed - an atomic RMW
counts as one instruction here and costs far more on real silicon, so the true
single-core cost is *worse*; and the cache-line handoffs the technique removes are
invisible to an emulator with no cache model, so the true contended saving is *better*
than zero by an amount only hardware can report. **The first version cost ~40
instructions and `objdump` said why**: one function held the election, the batch, the
publication and the spin loop, so LLVM allocated seven callee-saved registers for the
cold half and pushed and popped all seven on every fast-path call, with an un-inlined
dispatch beside them - split into an `#[inline(always)]` leaf plus `#[cold]` remainder it
is 11, with no logic changed (docs/ENGINEERING.md 11: a hot path's cost can be dominated
by the cold code sharing its stack frame). Two controls observed firing (the zeroing
deleted -> `4096 nonzero byte(s)`; the combiner running each request twice -> `double
free of <pa>`).
**And the first version of the zeroing oracle passed with the fix deleted** - the
concurrent loop asserted every fresh frame arrives zero, reasoning that a frame one core
stamped and freed comes back to the other dirty, but `alloc` rotates its hint so a freed
frame is not handed out again until the cursor has walked all 131,072 of them and nothing
in a 2,000-round pass is ever recycled; the frame is dirtied **before** it is allocated
now (the hint makes the next one predictable, and a free frame belongs to nobody), with
the prediction asserted rather than assumed. Recorded in docs/ENGINEERING.md 11. Honest:
changes 2 and 3 have **no control of their own** - their observable behaviour is identical
to what they replaced, only the acquisition count changed, so `numa` and `cowfork` are
their gate and no test can distinguish one acquisition from three.

**Honest scope:** preemption is *within* a core's own claim and rebalancing moves only
**unstarted** cells. Migrating a *running* one was **attempted twice and reverted twice**, with four findings
recorded in docs/SMP.md 10.0. The second attempt followed the first's own advice -
instrument the invariant instead of reasoning about the symptom - and that is the part
kept: **the kernel now records which cell each CPU is inside and refuses a second entry**
(`user::double_entries`, asserted by the preemption phase, one store per cell dispatch).
It turned the failure from downstream corruption into `cell 0 entered by CPU 0 while CPU
1 is already inside it`, reproducibly, and exposed a real ordering hole (the request's
cell and destination were two statics, so an owner could hand a cell to the *previous*
request's destination). Packing them reduced the failure rate without eliminating it, so
a fifth thing remains and the mechanism stays out: an intermittently faulting kernel must
not land. The guard's own first version is a finding too - counted per *cell* it
false-positives, because under preemption a batch sibling exits without re-entering
`run_inner`; per **CPU** it names two cores or none. Two Linux cells are proven; *many*, and a Linux cell that forks,
pipes or signals across cores, are not
(the cell/capability/object tables and the Linux per-cell state are still written for
one CPU - the audit in docs/SMP.md 10.2 is the gate). What makes the native path safe is
that a claimed cell is still a *partitioned* cell - one core, one slot, one address
space, one kernel stack - the claim simply made at run time instead of by hand.

**Still honest about what is not wired:** the fixed VA map is still the map - S2' has
only its **ceilings**, which landed early because they were a live defect rather than a
design debt: every per-cell region had a start and no end, so `mmap_file`'s cursor grew
unbounded from 20 GiB toward the cross-cell channel rings at 24 GiB (4 GiB of file
mappings would have placed the next one **on top of the cell's own channel**, silently
replacing the ring two cells talk through) and `SYS_GRANT` walked out of the ISA's user
range instead of refusing. Each region is now bounded by its neighbour, on the cell's own
path and on the peer's in `SYS_GRANT_SHARE`, and `security` proves it with the property
that makes a refusal clean - an over-large grant is refused **and** the next ordinary
grant still lands at the window base, so nothing was consumed. The map's internal *order*
is a compile-time assertion now rather than a comment. **The map is also recorded**: a
per-cell `VaSpace` holds every region a cell is given, and `SYS_MUNMAP` classifies an
address by asking it instead of by which constant range it falls in - load-bearing, not
decoration (removing the anon record makes the legitimate mmap/write/munmap round trip
fail, observed), with records handed back on a whole-region unmap so a churning cell
cannot exhaust its table. That needed the S1' lesson a second time: a first attempt let
each cell's table grow lazily, so its first frame landed inside the per-operation
frame-cost oracles and broke `security`; the tables are funded **once at boot**
(`user::init_layouts`), the same answer S1' gave for the mapped-file registry.
**And the record is the authority on what the kernel owns**: a caller-chosen
`MAP_FIXED` is the one request placement cannot protect against, so it is checked
against the spans the kernel holds - and that check was a second copy of `load.rs`'s
constants in `linux/mem.rs`, kept in step by hand. It asks the cell's recorded layout
now (`user::kernel_owned_overlap`), as an **allow-list over `RegionKind` with no `_`
arm**, because a deny-list answers today's question and defaults a *new* kernel-owned
kind to permitted, silently, at whatever commit adds it. The record had to be
**complete before it could be the authority**: the first attempt broke `linuxproc` at
once, since a Linux cell never maps a queue ring and so never recorded one, so
delegating *lost* a check the constants had - the kernel-owned windows are reserved in
`user::install` for every cell now, mapped or not, which is the truthful statement
about them (those VAs are the kernel's whether or not anything is there yet). Honest:
the refusals are the same two, because the only caller is the Linux `MAP_FIXED` path
and a Linux cell holds no typed grant and no device BAR - the rule changed, not the
behaviour; `mmapx` asserts both spans so a dropped kind cannot hide behind the other,
with the channel half observed failing when its record is removed.
**And placement is now an allocation**: the four regions a cell asks for at run time - a
typed grant, a file mapping, an anonymous `mmap`, and the read-only copy `SYS_GRANT_SHARE`
places in the *peer* - are each a `VaSpace::reserve_in` (first-fit inside the region's
window, guard gap either side, `release_at` rollback on every failure path), retiring
three bump cursors including the **global** anon-mmap one that let one cell's mappings
move another cell's addresses. `security` asserts the property rather than the mechanism,
because a cursor and an allocator agree on the first two answers: first grant at the
window base, second guard-gapped, and after the first is freed the third **reuses its
base** - the only load-bearing one, and exactly what a rising cursor cannot produce (with
the release suppressed it lands at `+0xa000`, observed). Honest: the anon ceiling is
**unproven** - it cannot be reached in one call, because the frame budget refuses any span
big enough to cross the window first, and a first version of that proof passed with the
ceiling deleted and was removed rather than kept as decoration; `reserve_in` is **windowed
rather than whole-space** because the loader's own placements (image, interpreter, stack,
the `.user` window) are still constants and unrecorded, so a global first-fit would
allocate straight through them - recording those is what removes the windows.
**And the user half is each ISA's own now**: `USER_VA_MAX` was `2^38` on all three
because RISC-V Sv39 has the narrowest one, which is a property of the *page-table
format* and so belongs in `arch` (Sv39 is the floor **profile**, not a ceiling the
other two must accept) - and holding the wide ISAs to the narrow one cost something
concrete, since JavaScriptCore's Gigacage is a single 128 GiB `PROT_NONE` reservation,
half the whole Sv39 half, so the Linux `mmap` window had to be squeezed into the 172 GiB
left over and a **second** cage would not have fit at all. It is `arch::USER_VA_TOP` now
and the Linux window follows it (ending 4 GiB below, because the F1 pointer check refuses
a span that *reaches* the ceiling): the largest reservation a cell can take goes 128 GiB
-> **64 TiB on x86-64** (`2^47`) and **128 TiB on ARM64** (`2^48`), riscv64 keeping its
hardware's 128 GiB. Nothing the loader places moved - every fixed region is asserted below
the floor. `mmapx` proves it by **probing** (double a `PROT_NONE` reservation until
`ENOMEM`, assert the Gigacage fits, report the largest that did) against a kernel-side
oracle computed from the same two window constants, because a hardcoded size would now be
right on one ISA and wrong on two. That surfaced a **real defect the old ceiling had been
hiding**: `unmap_range` stepped one 4 KiB page at a time, so unmapping was O(range)
*regardless of what was mapped* - Bun's Gigacage teardown was already 33 million
four-level walks, and against a terabyte-wide window it became a hang (observed as the
probe timing out on x86-64). One conservative per-ISA query fixes it,
`arch::paging_unmapped_span(root, va)`: how many bytes from `va` are *certainly* unmapped
because a table above the leaf level is absent, `0` when only a leaf lookup can answer -
so the portable walker skips an empty gigapage in one step instead of 262,144, and since
it never claims a *mapped* span, a caller that ignored it would still be correct, only
slow. Dispatch is proven for native cells and **off by default** except the
`linuxnode` boot -
enabling it for the *Linux* boots is what the `linuxbun` gate needs - `metrics`
records nothing until a boot enables it, and the per-CPU EEVDF+BORE queue still drives
only the single-core preempt path (multi-core placement is by claim, not by that
queue). The
exit gate is unchanged: `linuxbun` flipping from its accepted partial to `rheo:42`.

**FlashAttention 2 and 3 run** (docs/TILES.md 13): the real exp softmax, filling
the slot the integer attention block explicitly left open. `librheo/src/tile/fmath.rs`
is `exp2f`/`expf` from scratch (range reduction + a degree-6 series, ~40 lines,
stated 2e-7 bound) rather than the `libm` crate - `libm` gives correctly-rounded
`expf` over the whole domain, and a softmax needs neither half of that (its argument
is `x - rowmax`, non-positive and bounded, and the result is immediately divided by a
sum of such results), while what a tile kernel *does* need is a function that inlines
and vectorises rather than an opaque call per element. `librheo/src/tile/attn.rs`
carries **FA2** (the online-softmax loop over K/V blocks - no `Tq x Tk` score matrix
ever exists) and **FA3** (the same arithmetic pipelined over a double-buffered
staging pair, prologue + swap at a fence); both share one `flash_row_resume`
recurrence, which is also the entry point a **paged KV cache** needs (carry
`(m, l, acc)` across non-contiguous pages). Proven by `librheotilebattle` on all
three ISAs, bit-identical: `exp2f`/`expf` within 2e-7 of exact with both ends
saturating; FA2 vs a naive materialise-and-softmax reference (bounded relatively,
because the summation order genuinely differs - measured 0e0 at the test shape);
**FA2 invariant to the block size** across `block_k` 1/7/16/32/64/Tk compared against
the *single-block* result (the load-bearing property - the rescale is what makes
tiling not change the answer, and comparing tilings only against each other would
pass if they were all wrong the same way; worst 4.8e-7); FA3 equal to FA2 **with its
staging-swap count hand-computed** (a pipeline that degenerated to one block would
pass the other checks); every output inside its V column's range (a softmax output is
a convex combination, so this is exact and independent of both implementations); a
paged-KV split accumulation equal to one pass; and the query-row decomposition
matching the whole batch. **And one attention head now runs across every core** (docs/TILES.md 13.4a): the
query-row decomposition is not just a property to assert, it is how attention is
parallelised, so `librheo-fa` is a cell computing FA2 **and** FA3 over one slice of the
rows, and eight of them are handed to the kernel's cell placement - whichever core is
free claims one. On all three ISAs **four cores at once** compute one 32x32 head, and
the assembled result is asserted **bit-identical** to a single cell computing every
row: equality rather than a tolerance, because output row `i` depends only on query row
`i`, so slicing by rows changes no row's arithmetic (unlike slicing the K/V loop). Each
cell also checks FA3 against FA2 on its own slice with the staging-swap count
hand-computed, *inside* the cell running on a core the launcher did not choose. It is a
**cell** and not the kernel work queue the GEMM uses because attention is f32 and the
kernel is deliberately FP-free, while a `.user`-window program's soft-float f32 emits
out-of-line calls into kernel `.text` a cell cannot reach - a loaded ELF cell carries its
own builtins and has neither problem. **And three unlike workloads run at the same instant**: the
cell carries three jobs - attention, a tiled int8 GEMM over its own row slice, and an
**async** job that does no arithmetic at all (eight strands each submitting an `OP_ECHO`
over the cell's own queue pair and parking on the completion) - so the placed queue is
**mixed**: 3 attention cells, 3 GEMM cells and 2 async cells interleaved across four
cores by claim, with *both* assembled compute outputs bit-identical to their single-cell
references and all 16 round trips returning their own value. That is what a separate
proof per workload cannot show however many cores each uses: the f32 softmax path, the
integer GEMM path and the **queue/reactor path** resident on the machine together, none
disturbing another's result - and the queue ABI in particular had only ever been driven
from one core at a time, since every prior async proof ran a single cell. Four controls
observed failing (every cell given the same slice; one bit flipped in the attention
reference; one bit flipped in the GEMM reference; the async strands pointed at `OP_NOP`
so the echo cannot match).
**And the same tile kernels run under the Linux personality** (docs/TILES.md 13.4b): all
of the above is librheo, the *native* userspace library, while Node/Bun/Claude Code are
`Personality::Linux` cells - two substrates with nothing joining them. The tile kernels
are dependency-free Rust, which is already why `kernel/engine.rs` and `bench-core`
`#[path]`-include them, so `tests/linux-fixtures/tilelinux` includes the same three files
(`fmath`/`kernels`/`attn`) and is built as a **static-glibc Linux binary**; `smp` runs it
as a Linux cell and compares its output hashes against the librheo cells' **actual
bytes**, hashed kernel-side over the buffers the rounds above filled, so the expected
transcript is derived rather than copied from a passing run. They agree exactly - GEMM
`23aa217921e5ccb1`, FlashAttention `0a9704e0e8740540`, all three ISAs. That establishes
**the tile programs need nothing librheo provides that the Linux personality cannot** (no
queue pair, no typed grant, no native verb) and that the two substrates agree bit for bit
on the arithmetic. Control observed failing: one input salt changed
in the Linux binary.
**And a tile kernel is reachable by the route a JS runtime's FFI uses** (docs/TILES.md
13.4c): a statically linked program calling the kernels still says nothing about whether
*JavaScript* could, and Node and Bun reach native code exactly one way - `dlopen` +
`dlsym` + an indirect call (`bun:ffi` and N-API addons are both that). So it is answered
rather than assumed: `tests/linux-fixtures/tileso` builds the same `#[path]`-included GEMM
as a **shared library** exporting `tile_gemm_hash`, and `dlopentile.c` opens it **at run
time**, resolves the symbol and calls it - `ld.so` linking from inside a running program
rather than before `main`, which is a step past everything L7 proved. It works on **all
three ISAs**, returning the same `23aa217921e5ccb1`. The probe earned its keep twice, both
times turning a guess into a fact: the first failure's refused-path log named
`/lib/libgcc_s.so.1` (a Rust `cdylib` needs the unwinder even at `panic = "abort"`, so the
library `dlopen` loads needs its *own* dependencies - a missing file, not a missing
mechanism), and the library then failed to link for aarch64 because it had inherited the
static fixtures' `+crt-static -no-pie`, which cannot produce a shared object at all.
**Two findings came out of that work, both kept.** A **writable `/tmp`** is mounted over
the read-only ext4 root in the disk-runtime harness (`ext4plus` is read-only, and the
mount table composing a read-only root with a read-write ramfs is what a mount table is
for). And the **legacy x86-64 `stat`/`lstat`** (numbers 4 and 6) are implemented, routed
to `newfstatat(AT_FDCWD, ...)` - the **third** instance of the two-numbers hazard after
`open` (nr 2) and `readlink`, found by `ENOSYS nr=4` in Bun's trace. How it is proven
matters: **this glibc routes `stat()` through `newfstatat` even on x86-64**, so the first
fixture called `stat()` and *passed with the fix reverted*; the `lstatx` fixture issues
the **raw** syscalls instead, and the programs that use the legacy numbers are precisely
the ones bypassing libc, as Bun's Zig runtime does. **And the fifth hypothesis was the actual defect**:
`/proc/self/exe` was set only by the `execve` *syscall*, so it resolved for a process
another cell had exec'd and returned `ENOENT` for the one the launcher loaded - with the
existing test exercising the working side (docs/ENGINEERING.md 11). `install_cell` now
takes the exe path as an **explicit argument**, since it cannot be derived (an in-memory
image genuinely has none, and `b""` there is the truthful answer). What ended a run of
four wrong guesses was a **diagnostic, not a sixth guess**: `trace_record` now names any
non-`open` syscall returning `ENOENT` (bounded, since library probing produces dozens),
and a refused **`readlink` names its path** the way a refused `open` already did - which
printed `/proc/self/exe` immediately. With that fixed, **Bun runs the tile FFI script as a
real file off the disk**, not just via `-e`.

**And JavaScript on the real Bun calls a tile kernel** (docs/TILES.md 13.4d): the
`linuxbun` gate runs the real Bun binary a second time off the same live ext4 disk,
evaluating JS that opens `/lib/libtileso.so` through **`bun:ffi`**, declares
`tile_gemm_check(u32,u32,u32) -> i32` and calls it, printing `tileffi: gemm 568708273` -
which is `0x23aa217921e5ccb1 & 0x7fff_ffff`, the low 31 bits of the same GEMM hash the
librheo cells, the static `tilelinux` binary and the `dlopentile` C probe produce, so the
value proves the *kernel ran* rather than that a symbol resolved (31 bits because a JS
number is exact only to 2^53). That closes the chain: one tile source compiled into a
librheo cell, a static Linux binary and a shared library, and now **invoked from
JavaScript by a production JS runtime** running as a `Personality::Linux` cell, all four
routes producing the same bits. One measured constraint on the way: passing the JS as a
**file** made Bun call `createFakeTemporaryNodeExecutable`, which writes a stand-in `node`
into a temp dir and failed `error.FileNotFound` - the ext4 driver is **read-only**, so
there is nowhere to write; `-e` does not take that path, and a read-write ext4 mount is
the real fix and is not built. Control observed failing: the library left off the disk
image. Honest: FA3's own producer/consumer
overlap is still **interleaving** within a slice - the parallelism above is data
parallelism *over* the head, and a slice runs on one core, so FA3's wall-clock win over
FA2 needs a second execution context inside the slice and is not built; the inner loops are
scalar (they are the oracle a `tile::simd` vector path is checked against); forward
only (no backward pass, causal mask or dropout); f32 only.

**Frames are NUMA-placed** (docs/SUBSTRATE.md pillar 6 / S6', the `numa` kernel).
The pieces were all present and disconnected: the Inventory recorded which node each
memory region belongs to, `SYS_GRANT`'s fourth argument carried a node hint librheo's
`mem::reserve_on` has always sent, and the allocator served everything from one
rotating search over one pool - the hint's own comment said "recorded but single-node
in QEMU", two claims of which only the second was true, and the second stopped being
true the moment QEMU was asked for two nodes. `mm::frames::init_numa` (called from the
boot sequencer **after** `hw::detect`, since the pool comes up inside `arch::init`
before any firmware table is read) learns which pool *frame indices* sit on which node
- a range per node, not a per-frame id, because the pool is one contiguous span and
the firmware map is already split at node boundaries, so each node's share is
contiguous by construction - and `alloc_on(node)` searches it. `alloc` is untouched,
so every pre-NUMA caller is unchanged and `NODE_ANY` routes straight to it. A
**preference, not a guarantee**: a full node falls back to the pool at large and
`numa_fallbacks()` counts it, because refusing would turn a bandwidth question into an
out-of-memory one (ARCHITECTURE.md 5 has no OOM killer) while a silent misplacement is
a bandwidth cliff the caller thinks it avoided; `NODE_ANY` is not counted, since
nothing was asked for. The `numa` test asserts against an oracle it cannot reach - the
boundary is the first 512 MiB of RAM because that is how the *test* launched QEMU, and
RAM base comes from the one documented relationship (the pool sits 64 MiB into RAM on
every ISA) - so every check compares a PA against that, never against the ranges the
allocator built: the two ranges **partition** the pool (a gap is frames no node can
reach, an overlap frames two nodes both claim, and neither shows up in a successful
allocation), 64 frames land on each node with **zero** fallbacks, and a run-dry node
degrades to its peer with the loss counted exactly once at `node_free`'s exact
run-dry point. Proven on x86-64 (2 nodes from the ACPI **SRAT**, boundary
`0x2000_0000`) and riscv64 (2 nodes from the **device tree**, `0xa000_0000`), each
running node 1 dry after its 16,320 free frames, with the placement assertion observed
failing when `alloc_on` is reverted to `alloc`. **ARM64 skips with a measured
reason**: QEMU hands a bare-ELF `-kernel` boot no device-tree pointer in `x0` on
`virt`, so nothing describes memory nodes - checked, not assumed, since `-dtb`
explicitly does not reach it either - and the single-node path is asserted *unchanged*
so "NUMA landed" never quietly alters a machine that has none. **And a cell's memory is
co-located with the cell**: a cell carries a kernel-stamped **home node** (round-robin
across the nodes the pool holds, so cells spread bandwidth rather than piling on node 0;
inherited by `SYS_SPAWN` and, necessarily, by `fork` - COW starts the child mapping
frames the parent already placed), and three things follow it: its **kernel metadata**
(page tables, capability tables, VA record - `kmeta` allocates on the owner's node and
holds the owner->node map itself, so `mm` never reaches up into `user`), its **typed
grants** (a `SYS_GRANT` naming no node resolves to the cell's own - "no preference" means
"the kernel decides" and the kernel decides locality, as Linux's default policy does),
and **every page it commits** (`commit_range`: anonymous `mmap`, the Linux heap and
stack, demand-page fills, COW copies - the bulk of a cell's memory, so leaving this one
anywhere would make the property false of almost all of it). Proven at the `kmeta` seam
every table growth passes through: two owners given different nodes, every funded frame
of each asserted on its owner's node twice over - against `frames::node_of` and against
the launch-derived boundary, since `node_of` is built from the same ranges `alloc_on`
places against and alone would only show the allocator self-consistent - observed failing
("owner 6 asked for node 1; element 0 landed at 0x84063000, node 0") with `kmeta` reverted
to `frames::alloc`. That proof also found a real API hazard: `node_of` answered "no node"
for any address that was not frame-aligned, because `in_pool` asks about frames rather
than addresses while the addresses a caller holds are interior; it rounds down now, so
"not on any node" cannot quietly mean "not aligned". Two hazards that only
exist once placement is real are handled and **not** reachable on QEMU's contiguous
layout, which is why they are written down rather than left to a proof that cannot see
them: `librheo`'s `Grant::reserve` passed node `0`, harmless while the hint was dropped
and now "pin every default grant to node 0" (it passes `NODE_ANY`); and widening a
node's range across the regions mentioning it is only sound while its slices are
contiguous, so an interleaved machine (node 0, node 1, node 0) would give node 0 a range
swallowing node 1's - `alloc_on(0)` serving node 1's frames while reporting no fallback,
a wrong answer that looks right - which is now detected and answered by knowing
*nothing* (ranges cleared, `alloc_on` degenerating to `alloc`, reason printed) rather
than something false. Honest: the pmem pool has no node of its own, core classes (P/E) are
unmodellable in QEMU, and the cell-facing path is proven at the **kernel seam**
rather than from inside a cell (a cell cannot see a physical address). Latency is
unmeasurable here regardless - QEMU models the topology, not its costs.
**And a core takes work from its own node first** (the CPU half of "vcores follow
memory"): the published runnable set is **grouped by home node** with **one claim cursor
per node**, and a core tries its own before any other - the *same* protocol replicated,
each cursor a single `fetch_add` so exactly one core can obtain each slot, which is the
property that makes two cores entering one cell impossible and the reason a scan was
refused. Work-conserving (a core whose group is dry crosses rather than idles), and with
one node it is the single cursor byte-for-byte. The `smp` kernel now boots with two
memory nodes and its CPUs split across them (every pre-existing phase verified to pass
unchanged under that launch first): **7-8 of 8 cells run on a core of their own memory
node**, with the kernel's counters agreeing **exactly** with the node of the CPU that ran
each cell. **Three attempts, and the first two proved nothing** - asserting `local > 0`
passed with the preference deleted (random claiming already lands ~half locally, so no
ratio can separate "applied" from "looked local"), and the exact version then passed
twice more because the detector shared the `mine` binding with the preference, and then
because it judged crossings by a loop `step` measured from a starting group derived from
the preference itself. Judged from the group actually taken against a freshly-read
`this_node()`, the control fires. The crossing is also counted where a core **wins the
run-mark**, not where it claims: a claim can be lost to a stealer, and counting at claim
time recorded nine claims for eight cells.

**Heterogeneous cores are modelled, discovered and scheduled for** (docs/RESOURCE-GRAPH.md
2.4b, docs/SCHEDULING.md 12): a P-core and an E-core execute the **same** instruction set and
differ in how fast and how efficiently they run it, so the `Cpu` node carries a **class**
(Performance/Efficiency/LowPower/Unknown) and a **capacity** (out of 1024, relative to the
fastest core *on this host*, which is what makes the cluster case expressible) beside its
`IsaSet` and never inside it. The one historical exception is the rule's proof - early Alder
Lake had AVX-512 on P-cores only and Intel disabled it **chip-wide** rather than ship a machine
where a running thread could not be migrated - so a feature some cores lack is a *correctness*
constraint belonging in the per-CPU `IsaSet`, and `sched::hetero` deliberately says nothing about
features. **Only a core can classify itself** (CPUID leaf `0x1A` answers about whoever executed
it), so the boot CPU fills its own class and each secondary fills its own at bring-up, with the
graph learning the result once from the primary at the end of `start_all` - one writer, so the
read path stays lock-free. ARM64 gets **no class by design**: `MIDR_EL1` names the *part*, and a
part->class table is a list of what someone thought of, so what is reported instead is
**divergence** (each core records its model id and a flag rises when two disagree) - an asymmetry
this kernel cannot name, reported as an asymmetry, being strictly better than one reported as
uniformity. Classification of *work* is **observed, not inferred**: Intel Thread Director is
probed and deferred to where present (`HintSource::ThreadDirector`), and where absent the
substitute is not a heuristic, because every relinquish here is an explicit counted transition
(`sched::bore`) - so `Unknown` (never relinquished) and `Compute` (long bursts) take the fastest
core while `Bursty` (short bursts, frequent yields) takes the slowest and gets its latency from
the run queue's ordering instead. The microarchitectural half of Thread Director's signal - IPC,
stalls, vector mix - needs a PMU and is named absent rather than approximated. **Fairness is
deliberately not rescaled by capacity** (that would redefine what fairness means as a side
effect; capacity drives placement, steal direction and statistics, and the EEVDF ordering is
untouched), and a **mismatched steal is counted, never prevented** - unlike the cache-domain
steal preference, which is *refused* in docs/RESOURCE-GRAPH.md 6.3a because an unstarted cell has
no working set to move, where capacity governs how fast it will run for its whole life. **QEMU
models no hybrid part** (no CPUID leaf `0x1A`, no hybrid flag, no `capacity-dmips-mhz` - read out
of its source), so `hwinfo` **asserts the honest absence** on all three ISAs and consumption is
gated on a *declared* asymmetry carrying its own `ClassSource::Declared`; `verify/hetero/`
model-checks the placement over 8 deterministic properties and 20,000 random machines QEMU cannot
be asked for. A uniform machine is unaffected by the **tie rule** rather than by a special case -
a first version's `is_hybrid()` gate in `pick_cpu` was observed to change no answer and was
removed. **And the preference is wired into the multi-core claim**, not only modelled:
`place_cells_classed` publishes a class per cell and `claim_matching_tier` scans for work suiting
the claiming core's tier before the ordinary cursor, safe by the same `PLACE_RUN` exchange
`steal` uses; a core claims **one cell at a time** on a hybrid machine, because a batch is work
held unstarted that may not suit the holder while a core that does suit it idles (restoring the
batch of two makes the phase fail intermittently - observed). The `smp` kernel proves it on all
three ISAs: CPUs 0-1 declared Performance and 2-3 Efficiency, two compute and two bursty cells,
every compute cell asserted on a full-capacity core and every bursty one on a reduced core, all
four claims through the preference, zero tier crossings, and the machine restored to uniform
before the assertions run. One defect on the way: the queue is republished **grouped by home
node**, so slot k is not cell k, and reading the class by slot told the preference the wrong thing
about the cell - looked up through `PLACE_ORIGIN` now, and honestly recorded as proven by the
observation that produced it rather than by a control the phase can reproduce on demand.

**And features are per CPU, with the AVX-512 rule made mechanical** (docs/RESOURCE-GRAPH.md
2.4b): `CpuInfo.features` is read by each core about itself, and the machine advertises the
**intersection** of the cores that have reported, not the union - because a thread can be
migrated, so a feature only some cores have is a promise the machine cannot keep. That is
exactly what Intel did with early Alder Lake's AVX-512, disabling it chip-wide rather than
shipping a migration hazard. The union is kept beside it so "exists somewhere" stays
distinguishable from "safe to advertise", and the graph's **per-CPU `IsaSet`** keeps each core's
real set so a *pinned* placement can still use it - which is the other half of the rule
("restrict placement to those cores, or do not advertise it"). Proven by `hwinfo` on all three
ISAs under a declared divergence: the machine stops advertising the feature, the union still
shows it, and the graph's provider query returns exactly the cores that kept it, so a placement
following the graph cannot walk into a SIGILL. It also asserts that **a core which never started
reports nothing rather than a copy** of the boot CPU's answer (filling it in is the tempting
fabrication), while `smp` asserts the four cores that *do* come up read their own and agree -
the first place a per-core read can be shown to answer about the right core. Three controls
firing; a fourth is recorded as a non-result, because breaking either `set_isa` site alone
changes no answer - the two are one claim and each corrects the other.

**The observability path runs in a real boot** (docs/LOGGING.md, the `preempt` kernel): the
buffered console and `telemetry`'s per-CPU record ring were built with a host model-checker and
**no in-kernel user at all** - the module was referenced by nothing outside its own file, which
is the same category of gap as a scheduler whose asymmetry can never be exercised. `preempt` now
enables buffering for one phase on all three ISAs and asserts against hand-computed numbers: a
buffered write reaches the ring rather than the UART (5 distinct lines, 5 slots), **identical
consecutive lines fold** (8 into 1, 7 folds) so a storm of one repeated message cannot fill the
ring, an overflow is **counted and reported** rather than silently overwriting the middle of a
burst, the flush drains every record, and buffering is off again afterwards so the rest of the
boot is byte-for-byte what it was - which is why it is opt-in, since buffering changes *when*
output appears and would make 210 existing logs incomparable with their own history. Two
controls firing. It also turned up a counter that could never be nonzero: `Rings::bypassed` is
unreachable from the console, because `console::write` asks whether it is buffering *before*
doing any work and returns - the right design, so the counter is documented as belonging to a
direct API caller and the phase asserts that path instead of the impossible one.

**The execution entity is the authority** (docs/EXECUTION-MODEL.md 9, stages E2-E4). Three
facts each used to live in two places, and every one of the five vcore/preemption defects was
one of those agreements being wrong. **E2**: ownership left `RunCell.vcpu[]` and the
entered-guard left its per-CPU array; both are one word on the entity, the entity id **is**
`cell * MAX_VCORES + vcore` so there is no mapping to drift, and `leave_cpu` is keyed on the
**CPU** because a cross-cell hand-off means the entity a core returns from is not the one it
entered. Both halves of the guard are proven load-bearing (removing the exit-side clear leaves
four entities recording a CPU inside them; removing the entry-side one panics by name).
**E3**: `nproc`'s `vparked[]` is gone - `parked()` reads the entity, park/wake write it,
`all_parked` is the table's own implementation - and **I4 becomes checkable for the first
time**, because "parked with no wake source" is a state a personality-side boolean could not
express. **E4**: a context's FP save area is **one funded frame charged to its own cell**
rather than an element of a `MAX_CELLS * MAX_VCORES` static; that static was the entire reason
`MAX_VCORES` was 4 (256 KiB of `.bss` at four contexts, 4 MiB at sixty-four), so it is 16 now
with the `.bss` cost *falling*, and the ceiling is the cell's frame budget. `smp` measures the
per-context frame cost and its return, `verify/entity` checks fund-once / distinct /
release-once. **E4's remainder landed too** (docs/EXECUTION-MODEL.md 9.3): the five
per-vcore arrays beside the FP areas were **704 of `RunCell`'s 840 bytes**, and five arrays
indexed by the same number are one record - vcore 0 inline, the tail a `Funded<Vcore>`
charged to the cell, so a single-context cell allocates nothing and a cell that wants more
pays one page for ~85. The obvious design was **simulated and refused** before anything was
written: one funded table per cell costs 256 KiB of frames to save 21 KiB of `.bss`, because
a `Vcore` is 48 bytes against a 4096-byte frame - the FP areas were worth funding because
each one *is* a frame, and these are not. The tail lives in its own static rather than in
`RunCell` because `RunCell` is `Copy` and a `Funded` descriptor must never be raw-copied (the
S1' scar), which is the shape `linux::thread`'s `FRAMES`/`FPAREAS` already have. Measured:
`RunCell` 840 -> 184 bytes; **the prediction that went with it was wrong and is recorded as
such** - the hot path was expected to get cheaper too, and the icount path lengths are flat,
because the compiler was already reading the fields it needed instead of copying the struct.
The `smp` phase measures the two costs *separately* (first context 1 area + 2 table frames,
each one after it exactly 1), since one batch could not tell "the table is amortised" from
"every context allocates a table". It also removed a latent wrong value nothing could see:
`install_forked` copied the parent's whole `vqp` array while setting `nvcores: 1`, leaving
live pointers into the parent's other rings that nothing could reach and nothing cleared.
**And then `MAX_VCORES` was removed outright** (docs/EXECUTION-MODEL.md 9.4). It bounded
four things, and the largest was hiding in plain sight: `CELL_FP` was still
`[FpArea; MAX_CELLS * MAX_VCORES]` - **the 1 MiB E4 exists to remove, still paid in full**,
kept on as a fallback after the areas became funded. It is a *single* counted area now, and
single deliberately, because sharing one between two live contexts is not a degraded mode -
it is the `SYS_YIELD` FP defect exactly, so no size of fallback table is correct and keeping
256 of them only made the bug rarer. The **id stride** was the real ceiling: `entity_of` was
`cell * MAX_VCORES + vcore`, which E2 rightly called "the identity, not a mapping", but a
derived id is a stride and a stride caps contexts per cell however much is funded - and the
same arithmetic was written a *second* time in `smp`'s placement queue, so the derivation had
already failed at the one job it was doing. Ids are **allocated** now by the one
`EntityTable::create` the Linux side already used and **stored in the context's own record**;
the queue carries entity ids; the reverse direction needs no mapping because the entity
records `(cell, context)` itself; and E3's "two ways of obtaining an id" collapses back to one
(`create_at` and the reserved band deleted). `nproc::Proc`'s two arrays got the same treatment,
656 -> 56 bytes. **Id 0 means "no context"**, load-bearing rather than conventional since a
funded table grows into zeroed frames - and reserving it immediately surfaced a latent defect
**`verify/entity` caught rather than a boot**: `Funded` grows by whole frames and an all-zero
`Entity` is *not* `Entity::EMPTY` (`owner`/`inside` want `u16::MAX`), so a slot grown past but
never written read as "free, owned by CPU 0, entered by CPU 0"; invariant I7 fired on nine
scenarios, and `create` now initialises every slot a growth adds - the same rule as
"write the record, never trust the frame". Measured: per-cell scaffolding `.bss` 21,504 ->
1,408 bytes, `CELL_FP` 1,048,576 -> 4,096 bytes. The **one** surviving per-context bound is a
real resource and says so where it lives - a mapped queue ring needs address space and the
window is 4 GiB (`load::MAX_QUEUE_VCORES`); `MAX_VCORES` was the wrong home for it twice over,
making an address-space question look like a scheduler question and bounding the contexts
*without* rings by the same number. What bounds a cell's contexts now is its own frame budget,
refused cleanly. Proven by `smp` with **25 contexts on one cell** - past where the constant was
- costing 26 frames (2 table frames once, then one area each) and returning all 27, on all
three ISAs, plus an entity round trip in **both** directions replacing "the id decomposes
arithmetically". Four controls observed firing: skipping the grown-slot init fails 9
`verify/entity` scenarios, restoring the stride panics `smp`, not storing the id panics `smp`,
not releasing the funded tail fails the frame oracle by name.
**And E3's Linux half landed with it**: `Thread::state` is gone too, `TState`
surviving only as a *view* computed from the entity, with what stays on the thread being the
**reason** (`pblock`, `fut_addr`) because a wake source's detail belongs to whoever owns the
source while *whether it is waiting* is the scheduler's fact. That half forced a design
question the native side hid (docs/EXECUTION-MODEL.md 9.1): the entity id is **derived**
(`cell * MAX_VCORES + vcore`), which is what removes the mapping E2 exists to delete - but a
derived id is a **stride**, and a stride bounds contexts per cell, where native vcores are
bounded at 16 and Linux threads at `CONTEXT_CEILING` = 1024. Raising the stride is measured
and refused: `Funded::reserve` is dense, so it would allocate **1 MiB** of kernel metadata the
moment the last cell installs, for a table holding a few dozen live entities - a static array
in disguise, which is exactly what E4 just removed. So there are **two ways to obtain an id,
one table and one authority**: native keeps the derived id, a Linux thread gets one
**allocated** by `create` and stored on its `Thread` - `0` meaning "no context", because a
funded table grows into *zeroed* frames so an empty value must be the all-zero pattern - and
the floor is enforced in `EntityTable::init`/`create` rather than by the caller, since an
allocation landing inside a native cell's range would be overwritten by a later `create_at`
with no fault and no log. Two paths became release paths (`release_cell` on a `wait4` reap,
`reset` between runs), which is S1's lesson one level along, and `clone` gained the matching
`-EAGAIN` - a thread that cannot be scheduled must not be created. Proven by `linuxthreads` on
all three ISAs **across** the teardown rather than after it, because the harness resets at the
*start* of a run, so an "all free" assertion after the last one would have passed while
proving nothing; control observed firing. Honest remainders, named rather than implied: **I4
is vacuous in that kernel** (nothing is left parked by then - `verify/entity` exercises it
directly, and forcing every park to `NO_WAKE` fails `condwait` behaviourally rather than
through the assertion); and deleting `MAX_VCORES` outright needs
`vframe`/`vqp`/`vqp_va`/`vqp_cap`/`voutcome` moved onto the entity too - about 11 KiB of
`.bss` at 16 contexts, against the 1 MiB the FP areas alone were, so the constant is a bound
on an array rather than the resource limit it used to be. **And the cell-level Linux state
followed** (docs/EXECUTION-MODEL.md 9.5): `linux::Proc`'s `PState` carried `Runnable` and
`Blocked` as a *cache* of the contexts' states, written by a wake scan over every cell before
every pick, and a stale cache there is a hang (the cell is never picked) or a spin (it is
picked with nothing able to proceed). The enum is `Free | Live | Zombie` now - the lifecycle,
which is the part no entity can answer - and runnability is derived (`Live && (any context
Ready || any parked context satisfiable)`), so the scan and the `state = Blocked` write in
`park_or_switch` are both deleted; there is no window in which the bit and the contexts
disagree because there is no bit. The Ready half scans **that cell's** contexts rather than
calling `EntityTable::all_parked`, which answers the same question by walking every entity in
the machine - the wrong cost for a predicate a pick evaluates per candidate cell. Proven by
the whole Linux suite on all three ISAs *including* `linuxnode`/`linuxbun`/`linuxclaude`,
which is the evidence that counts, since this is the scheduler predicate Node, Bun and Claude
Code are picked by; control fires (dropping the `satisfiable` half deadlocks `linuxproc`).
That work found a **second** defect: a Linux cell held **two entities for one execution
context** - `thread::init_cell` sets context 0's frame to `user::cell_frame(cell)`, so the
cell's vcore 0 and Linux context 0 are the same context, and it allocated an entity beside the
one `install` had already made - invisible while native ids were derived and Linux ids
allocated above them, and a counting discrepancy the moment 9.4 gave both one id space.
Context 0 **adopts** now: one creator (`install`), one releaser (`free_cell`), `detach_entity`
clearing the field without releasing because two owners of one release is how a double free
gets written. Honest **non-result**: the adoption is not load-bearing for the derivation, since
`any_ready` scans the cell rather than calling `all_parked`, so restoring the duplicate breaks
no other phase - `linuxthreads` therefore asserts it *directly* (context 0's entity must equal
the cell's vcore 0) rather than leaving a cleanup nothing checks, and that assertion fires
under the revert.

**The machine can be watched from outside it** (docs/OBSERVABILITY.md 11, the `observe`
kernel, all three ISAs) - the first slice of the observability framework, which is the
**root** and the layout every reader agrees on. The tree could already *prove* things
about itself and could not *watch* itself: every hard defect this session cost a bespoke
diagnostic (Bun's abort took **three wrong diagnoses** and two fully-built mechanisms
before a one-line "print the refused path" answered it), and `kernel/src/trace.rs`'s own
header names the shape - "the lifecycle is not observable, only its endpoints are".
`abi/src/obs.rs` defines the plane once for three separately-compiled readers - the kernel
that writes it, an in-guest collector cell, and a **host tool reading guest physical memory
with no cooperation from the guest** - which is why it is in `rheo-abi` (zero-dep, `no_std`,
no lang items) rather than in the kernel. `kernel/src/obs/root.rs` exports one page-aligned
symbol, `RHEO_OBS_ROOT`, carrying a kernel VA **and a physical address** per published
region plus the tick domain, tick rate, CPU counts and the live window mask. The
load-bearing claim - that an outside reader can walk it from the ELF alone, on bare metal
or under any hypervisor, with no device and no `fw_cfg` - is **verified rather than
argued**: hand-computing the reader's own algorithm (`pa = p_paddr + (vma - p_vaddr)`) from
`readelf` output gives exactly what the guest published on every ISA (`0x47f000` /
`0x404a7000` / `0x8068d000`), two independent computations of one fact agreeing.
**Timestamps are raw ticks, and that is the design's first real constraint**:
`arch::timer_now_ns()` cannot be on an emit path, being a 128-bit multiply *and divide* on
all three ISAs - riscv64 has no 128-bit divide instruction, so it is a call into
`__udivti3`, a software loop, and aarch64 re-reads `cntfrq_el0` and executes an `isb` every
call - so a tracer built on it would cost more than the code it observes, which is the one
thing a tracer must not do; `arch::obs_tick()` is one counter read with **no barrier**
(`rdtsc` / `mrs cntvct_el0` / `rdtime`), losing no ordering because within a CPU order
comes from the sequence number and across CPUs from merging on the tick. Resolution is
**measured, not assumed** - 1 ns/tick on x86-64, 16 ns on ARM64, **100 ns on riscv64** -
and `tick_hz` is published so a reader declines to print a sub-resolution number rather
than inventing one. The event record is **32 bytes** so that with a page-aligned frame it
never straddles a cache line (at 40 it straddles ~40% of the time), and there is
deliberately **no drop counter**: loss is a property of a reader's cursor, so
`[c, head-capacity)` names exactly which events are missing - located rather than counted,
and one fewer atomic RMW on the emit path. Two controls observed firing (the magic not
written -> "no root" rather than decoded garbage; the linear-map mask forgotten in one
place -> the published address is refused as "not anywhere physical memory is", the mistake
that would make a host reader silently decode nothing), and one recorded as a **non-result**
- removing `#[used]` leaves the symbol present on all three ISAs, because `publish` takes
its address and every kernel calls it, so the attribute is kept as insurance and not
claimed as a proven guard. Publishing a field rather than reasoning about it found a real
defect: `smp::online_count()` answers "CPUs SMP bring-up **registered**", and the only
thing that registers the boot CPU is `smp::init`, which exists only under the `smp` feature
- so it returns **0 on every single-CPU boot** while a CPU is executing the call. Honest
scope: this is the root and the layout. Nothing is instrumented yet, no window exists, and
the four planes it will index are in the state docs/OBSERVABILITY.md 11.1 tabulates -
notably `metrics.rs` declares **eight** latency histograms of which only two have a
recorder anywhere and **no boot enables it**, so most of the scheduler-quality work ahead
is wiring and a switch rather than new machinery.

**And the event stream is per-CPU** (docs/OBSERVABILITY.md 11.3, the `observe` and `smp`
kernels, all three ISAs) - the second observability slice, and the one `kernel/src/trace.rs`
had already written down as its own defect: the ring was "one shared buffer with a plain
counter, so it is single-CPU today", with the fix "deliberately not copied here until a
multi-core boot wants to trace". `kernel/src/obs/ring.rs` is one ring per CPU with its own
sequence counter, safe by **partitioning** rather than by hoping - the argument `telemetry`
already made - and `trace.rs` is now a shim keeping every signature and the `@E` format
byte-compatible, because `cargo xtask trace` parses it and `smp` asserts on it (renaming a
module is not a proof, so nothing was renamed). The module is **dependency-free** (the tick
and the CPU index are passed in) so `verify/obs/fuzz.rs` can drive it on the host, which is
the only place the wrap is reachable: a boot emits a few thousand events, and the arithmetic
that matters is at **2^32**, where the recorded sequence number - `head`'s low 32 bits -
recycles, about 71 minutes at one event per microsecond. **Two design decisions were forced
by writing the code**, not by the plan. Funding **cannot** happen on the emit path as
planned: funding allocates, allocation takes `mm::frames`' pool lock, and one of the
recorded windows *traces the allocator* - so a lazily-funding emit could re-enter the
allocator from inside it on a non-recursive lock, which is a deadlock rather than a slow
path, appearing only on the first event a boot ever recorded from that window; `fund` is a
bring-up act now and `emit` never allocates, with a CPU that has no ring counting its
offered emits rather than losing them (and publishing `capacity` **last** - written so a
reader could never see a funded ring pointing at nothing - makes funding self-excluding for
free, since the allocator's own events see `capacity == 0`). And the host tool's gap
detection **had** to change: a sequence number is per-CPU monotone now, so comparing
consecutive lines of the merged stream would report a gap wherever the emitting core
changed - pure noise on any multi-core boot, against that tool's own rule that a diagnostic
which cries wolf is worse than none. `trace::counters()`' second value is **derived** rather
than counted (a ring holds `RING_EVENTS`, so anything past that was overwritten), which
removes an atomic read-modify-write from the hot path and stops conflating "the ring is a
ring" with "a reader lost data". Proven by `observe` on all three ISAs (17 frames - 16 data
plus a directory, as designed - 300 records read back **field-for-field** with real ticks
advancing, the ring wrapped with the surviving window exactly `[total-cap, total)` and the
record before it gone, all 17 frames returned) and by `smp`, where **two cores record 64
events each at the same instant** and three things are asserted per ring rather than as a
total: each ring took exactly its own core's 64, every record is found in the ring of the
core that wrote it *identified by a tag in its own contents*, and each stream's sequence
numbers are consecutive - the property the host tool's loss detection rests on and the one a
shared counter destroys (observed cpu0 and cpu2, 128 total, 17 offered to an unfunded ring
being the secondary's own funding allocations exactly, 34 frames returned). Three controls
observed firing - every CPU forced onto ring 0 gives `the primary's ring took 145 of its 64
events` (= 64 + 64 + 17 in one ring with the other empty; note what it is *not*, since under
TCG nothing was lost and a count of totals would have looked fine - what breaks is
attribution); the slot mask written `n & cap` fails four wrap cases by name; `seq_of` made
zero-based makes a zeroed frame read as a written event - and **one recorded as a
non-result**: `ObsRing::get`'s sequence-number check does not fire, measured rather than
assumed, because sequentially the bounds test subsumes it and it earns its keep only against
a reader racing a live writer on another core, which no built reader is yet and which a
single-threaded host driver cannot produce without aliasing the ring mutably to model a data
race the language forbids.

**And recording is cheap enough to leave compiled in everywhere** (docs/OBSERVABILITY.md
11.4, S2/S2b): a per-window **enable mask** gates 13 windows - one relaxed load, a test
and a branch, **3 instructions** on every disabled site, with the event's arguments never
evaluated (`obs_event!` is a macro so the mask test sits *outside* argument marshalling;
written as a function first, it cost +9 and the plan's own named control caught it) - and
the **enabled** emit went 60 -> **22** instructions by doing what Linux's ring buffer does
(4 packed u64 stores, header word const-folded at the call site) minus what this kernel
does not need (no nesting-safe reserve/commit: the producer is partitioned per CPU and no
emit can interrupt an emit today, stated in `obs/ring.rs` for the design that changes it).
The ring became a **contiguous block** (`frames::alloc_contig`, the allocator whose
absence two ISA scars had recorded) so a slot address is one multiply rather than a page
split, and a host reader's walk is linear. Static-key text patching for the disabled site
is **refused with the cost recorded** (self-modifying kernel text + per-ISA I-cache
protocol to buy back 3 instructions). Measured against the pre-S2 build across the whole
matrix: +3 per new call site on the queue round trips, every untouched path unchanged to
the tick. **And the table under the scheduler state those windows record about was priced
next** (docs/SUBSTRATE.md pillar 1, S2c): `Funded<T>` grew an **inline directory** (first
8 page addresses in the struct - no dependent directory load and no directory frame for
every hot table; three frame oracles moved to the new numbers, the inline/overflow
boundary gained the test it had silently lost) and a **scan API**
(`page_slices`/`iter`, one resolve per page, elements by reference - 10.5 -> 4.9
instructions/element; migrated into the entity table's, VMA list's and thread table's
decision-path scans, while the EEVDF queue deliberately kept its `high_water`-bounded
point loop over `get_ref`, where the same migration would have *regressed* a small
queue), and the **repeated walks were fused**: the page-fault fill asks the record it
already holds (`Vma::file_page`) instead of re-scanning the list, EEVDF `dispatch` takes
pick + eligibility from one walk instead of two identical ones, and `dispatch::pick`
memoizes the personality's runnable predicate (it was evaluated up to three times per
cell per decision, each itself a context-table walk). Whole-matrix bench diff published:
`p5_crosscell_roundtrip` 499 -> 483, against +1/-1 ripples and +3..6 amortized on the
`rng_*` draws from the 64-byte-larger struct - the lessons (optimise the access shape;
two questions about one element set are one walk; a struct's size is an interface, bench
everything) recorded in docs/ENGINEERING.md 11.

**And the machine's live state is a readable block, not a replay** (docs/OBSERVABILITY.md
11.5-11.6, S3/S4): one `ObsCpu` per CPU - a seqlock'd coupled group (state, current
cell/entity/vcore, since-when, the armed deadline in the arbiter's own ns domain, the
receive tier) written only by the owning CPU at transitions it already passes through,
plus 56 monotone counters outside the lock - published as `OBS_SEC_CPU` with a name table
saying which slot means what. **Busy/idle is real measured time**: every transition
charges `now - since` to busy, or to idle only when the park genuinely halted - a spin
charges busy, never laundered - proven by `observe` on all three ISAs (a 20 ms park
charges ~20 ms of idle ticks, judged through the root's published `tick_hz`) and the
seqlock by `verify/obs` on real host threads (4.8M coherent reads racing 3M writes, zero
torn; the bracket-deleted control caught within ~25k reads). A machine-wide `ObsMem`
block is **filled on request** and stamped with when (`refreshed_tick`), never maintained
by the allocators - a mirror-keeping store on the allocation hot path is the cost the
design refuses. **S4 has begun**: the module counter statics migrate onto the plane's
per-CPU slots - `net_rx`'s five (racy `static mut`s the moment a second core ran a
receive wait), `input`'s three, and `idle`'s two, which were a double-report (`idle`'s
own statics plus a gated bump in the snapshot writer - the same halt counted once or
twice depending on the mask; one unconditional counter now, with `observe`'s
zero-while-off assertion *moved deliberately*: the count moves with snapshots off, the
time attribution does not). Every accessor keeps its signature and sums over CPUs, so the
existing suite is the migration's exactness oracle; two controls fire by name, and one
non-result is recorded - `netwait`'s stall-tolerance branch absorbs a misrouted spin-poll
counter, so the control that stands is on an assertion with no tolerance.

**One intermittent test was found and fixed** while gating the above: `rng`'s HID phase
asserted `virtio_input::buffers_clear()` - every DMA buffer zero *after* a drain - and failed
on riscv64 about one run in ten saying a drained event was still in the buffer, when what had
happened was that a **new** keystroke had arrived. A wiped buffer goes straight back to the
device and the injector is still typing, so the question was being asked in the one place it
cannot be answered. The property is now asked of the **drain**: the wipe is **read back**
before the buffer is handed over and only a verified wipe is counted, so
`wiped() == events()` is a statement about memory rather than about timing. Read back
deliberately rather than just counted - an increment beside the wipe would prove nothing,
since deleting the wipe would delete the increment with it; the control (wipe removed) gives
`1 of 2 drained HID event(s) were wiped`. A test that fails on correct behaviour is worse
than no test.

Deferred (documented): cross-host/cluster, PTP/NTS time sync, attested
firmware + real GPU/NPU engines, elastic-grant pressure events, the Verus
proofs, and the hardware-lab performance numbers.

**SMP phase 1** (docs/SMP.md, task #27) closes secondary-core bring-up: **a genuine
second core runs kernel code on all three ISAs**. The portable foundation
(`kernel/src/smp.rs`: a `SpinLock<T>` + a per-CPU registry with `this_cpu()`,
zero-impact on the single-CPU path) is unchanged; what landed is the per-ISA bring-up
and, first, the **root cause that had blocked x86**. The LAPIC was driven only through
the **x2APIC MSR block**, which QEMU 8.2 TCG leaves inert - the docs/ENGINEERING.md 1
case study - and since INIT-SIPI-SIPI is sent through the *interrupt command register*,
x86 SMP and the x86 timer were the **same** defect. The LAPIC driver now supports both
access modes and **picks by observation**: request x2APIC, read `IA32_APIC_BASE` back,
keep it only if `EXTD` latched; otherwise map the **xAPIC MMIO** page uncacheable
(`paging::apic_map_window`, a third fixed window at PML4[386] whose PDPT is stamped into
**every cell root**, so a handler reaches EOI whichever root is active) and require the
register file to answer a write/read-back. Observed under this QEMU: **xAPIC (MMIO)**,
x2APIC correctly declined.

That unlocked four things. (1) A **genuine one-shot timer on x86-64**: `SYS_ARM_TIMER`
halts at `hlt` and really sleeps, `netwait`'s pre-N2h regression no longer skips there,
and the receive wait's `IdleMode::TimerIdle` is a *measured* halt again (21-1771 per run)
rather than the spin N2h had honestly demoted it to. `ktimer` remains the **only** caller
of `arch::timer_*` - the single-owner invariant is untouched. (2) **A real x86-64 AP**: a
position-fixed 16-bit trampoline (`kernel/arch/x86_64/smp.S`) copied to a **verified**
low page (usable RAM per the firmware map, clear of the PVH `hvm_start_info` and the ACPI
RSDP), released by INIT-SIPI-SIPI, climbing real -> protected -> long on the primary's own
`boot_page_tables`. Two triple faults were *observed and fixed* on the way: the AP set
`EFER.LME` but not `NXE`, and kernel PTEs carry NX (a set bit 63 with NXE clear is a
reserved-bit fault, so it died on its first LAPIC read); and swapping in the primary's
`gdt64_ptr` failed because that is the 6-byte 32-bit form while `lgdt` in long mode reads
2 + 8. The first is fixed *structurally* - the primary publishes its own CR4/EFER/CR0 and
the AP adopts them verbatim, so the two cores cannot diverge. (3) **A real ARM64
secondary**: the old verdict "PSCI `CPU_ON`: `smc #0` trapped to EL1" was true of the
*instruction*, not of PSCI - QEMU's `virt` implements PSCI itself and picks the conduit
from the machine config (**hvc** for the plain machine, smc only with
`virtualization=on`), so bring-up now **probes** with `PSCI_VERSION` over each conduit,
both still guarded by a temporary exception vector, and reports what answered (`hvc #0`,
version 1.1); `CPU_ON` then enters an MMU-on trampoline that adopts the primary's
MAIR/TCR/SCTLR verbatim. (4) The **x86-64 UART RX interrupt**, whose poll-only verdict
had rested on the same inert registers - with a working EOI the IO-APIC path
demonstrably re-delivers, so GSI 4 -> vector 0x21 is wired, probed by a 16550-loopback
byte, and `input::interrupt_driven()` is **true on all three ISAs** (x86-64 is now the
*most* complete of the three here: QEMU's 16550 loopback raises the ISA line itself, so
nothing has to be poked into the interrupt controller by hand, unlike the riscv/arm
caveats). No secondary is told its identity - each reads its own hart id / APIC id /
MPIDR - and the `smp` test asserts the shared magic through the cross-core spinlock plus
that the registry slot and the hardware id are **not** the primary's. Everything is
behind the `kernel/smp` cargo feature; the non-smp library is **byte-identical**
(verified). Honest: this is bring-up, **not** preemptive multi-core scheduling - each
secondary runs work the primary hands it and then parks, most of the kernel is not yet
safe to run on two cores concurrently, the runtime stays single-CPU cooperative, and
only **one** secondary is started (one dedicated stack per ISA). Both of those last two
are superseded: a secondary now runs a **cell in user mode**, and `start_all` brings up
every enumerable core on its own stack - see above and docs/SMP.md 10.0. Still deferred: shared
`static mut` state made SMP-safe end to end, per-CPU stacks + a start-all loop, a
per-CPU register instead of the small id->index table, cross-CPU IPIs beyond bring-up, and the **x86-64 NIC RX
interrupt** - the last interrupt source any ISA lacks, now known to need ordinary driver
work (assign the virtio-net BAR, program MSI-X or find the q35 INTx routing) rather than
the platform limit the old wording implied.

The `.user` linker window holds U-mode code (`.user.text`), shared
read-only constants (`.user.rodata`), and per-cell data (`.user.bss`) in
one 2 MiB span; per-cell page tables differ only in the per-cell data
mappings, which is what makes cross-cell isolation MMU-enforced. U-mode
code (the programs in `kernel/src/user_progs.rs`, including the shell) must
be free of out-of-line calls into kernel `.text` (no panics, no bounds
checks, no `core::fmt`, no autovectorized constant pools) since kernel
`.text`/`.rodata` are not mapped in a cell - which is why the test kernels
build `--release` and aarch64 uses the soft-float target.

## Commands

Everything routes through the xtask runner (`xtask/src/main.rs`):

```
cargo xtask check --arch <x86_64|aarch64|riscv64|all>   # type-check the kernel only - the fast loop
cargo xtask build --arch <x86_64|aarch64|riscv64|all>   # cross-compile all kernels
cargo xtask run   --arch riscv64 [--bin lsh]            # boot in QEMU, serial on terminal
cargo xtask test  --arch all                            # boot every test kernel, pass/fail
cargo xtask test  --arch riscv64 --bin schedidle,netwait # boot only these (iterate)
cargo xtask bench --arch all                            # icount path lengths (always release)
cargo xtask verify                                      # host model-check the kernel state machines (seconds, no QEMU)
cargo xtask sizes --arch x86_64 [--bin smp]             # the kernel's largest static allocations, biggest last
cargo xtask trace --arch x86_64 --bin smp [--ledger]    # window a boot's structured trace; --ledger balances per owner
cargo fmt --all                                         # format (CI-gated)
cargo clippy -p xtask -- -D warnings                    # lint host code (CI-gated)
```

**`check` is the inner loop for kernel work.** `build` cross-builds the
`userland` programs, every librheo bin (twice), four separately-featured `net`
cells, the std programs, the coreutils multicall, the columnar dataset, the pmem
backing file and the glibc Linux fixtures *before* it reaches a line of kernel
code - none of which a `kernel/src/` change can affect. `check` runs `cargo
check` on the `kernel` package only, **twice: with and without the `smp`
feature**, because that feature is a separate compilation of the same library
whose per-CPU paths the ordinary build only compiles at the very end. It cannot
catch a link error or a missing fixture, so it does not replace `build`/`test` -
it just makes a compile error surface in seconds instead of minutes.

Kernel clippy needs the build-std flags:

```
cargo clippy -p kernel --target x86_64-unknown-none \
  -Zbuild-std=core,alloc,compiler_builtins \
  -Zbuild-std-features=compiler-builtins-mem -- -D warnings
```

Plain `cargo build` builds only xtask (the kernel needs a bare-metal
`--target`; `default-members` excludes it on purpose). The toolchain is a
pinned nightly from `rust-toolchain.toml` - rustup installs it automatically.
QEMU 8.x system emulators must be installed to run or test.

## Layout

```
docs/         the design documents (the spec - keep code consistent with it)
abi/          rheo-abi: the on-wire user/kernel ABI, defined **once** - syscall
              numbers, queue opcodes/status codes, the ring header + entry
              layouts, and every repr(C) block a syscall writes into a cell.
              no_std, zero deps, **no lang items** (the runtime/ model), so it
              links into a kernel binary and a cell alike; kernel::abi,
              kernel::queue, librheo::sys and libc::sys all re-export it, which
              is what makes divergence a compile error instead of a wrong number
              at runtime (docs/ARCHITECTURE-DEBT.md 3.1). `obs` is a submodule
              rather than more of lib.rs because it has a **third** reader the
              rest of the ABI does not: a host tool walking the telemetry plane
              out of guest physical memory, with no cell and no syscall involved
              (docs/OBSERVABILITY.md 11)
kernel/       the no_std kernel library + boot demo bin
  src/        ISA-independent: boot (the portable boot sequencer - arch::init
              does only console/vectors/page tables, so arch names nothing above
              itself), capability core, queue ABI, cells, mm
              (frames + frames_pmem real-nvdimm allocator + grants; frames carries a
              per-frame refcount so `fork` is copy-on-write - `free` is a decrement,
              `share`/`refs` the COW primitives - **and** the per-NUMA-node frame
              ranges `alloc_on` places against, docs/SUBSTRATE.md pillar 6; the hot
              operations go through **flat combining** rather than each core taking the
              pool lock in turn, and the 4 KiB zeroing is outside the lock entirely -
              docs/SMP.md 10.0g), uaccess (**the one seam kernel code
              touches a cell's memory through**: bounds + presence + copy-on-write
              resolution, so every lazy-mapping feature is one change here rather than a
              ~98-site audit - docs/LINUX-COMPAT.md), time (clock), rng (ChaCha20 DRBG +
              **the multi-source entropy pool**: rng/entropy.rs mixes CPU/device/
              jitter/interrupt/user sources with an absorb that cannot reduce
              entropy and a credit rule that only counts what it can measure,
              rng/jitter.rs is the software-only source, rng/health.rs the
              every-boot power-on self test - docs/TIME-IDENTITY.md 4a),
              event streams,
              sched (reservations + the **system-wide admission ledger**:
              a reservation must fit its cell AND the machine -
              docs/ARCHITECTURE-DEBT.md 2.5; plus bore/vcore - the BORE burst
              score feeding one deadline-ordered EEVDF queue - and **dispatch**,
              the seam where the two reschedulers ask that queue for the order
              while the personality keeps authority over runnability, and
              **preempt**, which takes the CPU from a cell that will not yield:
              docs/SUBSTRATE.md pillar 3), lease, engine, graph, pty, smp
              (per-CPU state + a kernel SpinLock + secondary-core bring-up on
              all three ISAs - docs/SMP.md), input
              (kernel RX ring + the SYS_WAIT_INPUT park-until-input primitive -
              docs/LIBRHEO.md Phase D), idle (the **scheduler idle state**: what
              the kernel does when no cell is runnable but one is blocked on a
              wake source - composes ktimer + net_rx + input, halts where an
              interrupt can wake, reports a genuine deadlock instead of
              panicking - docs/ARCHITECTURE-DEBT.md 2.4), ktimer (the kernel **timer arbiter**:
              the single owner of the per-ISA hardware one-shot - a fixed 5-slot
              deadline registry (RxPoll/RxDeadline/CellSleep/NetTimer/Pacer),
              nearest-deadline arming, and no client's cancel can lose another
              client's deadline - docs/NETSTACK.md 16 Phase N2h), net_rx (the SYS_WAIT_NET
              park-until-frame primitive + the NIC RX interrupt sink + the three
              deadline-honouring wait modes: a NIC-interrupt park, a timer-backed
              idle where only the timer interrupt exists, else a bounded poll -
              docs/NETSTACK.md 16), svc
              (shell/resource syscalls + the **bridge framework**: Bridge<T> +
              FileOps (POSIX files) / SocketOps (N4b remote INET) / NicOps /
              DisplayOps - fn-pointer tables whatever owns the real
              implementation registers at boot, so the kernel stays
              filesystem-free, network-stack-free and **driver-free**: the queue's
              opcode dispatch names no device driver, and a driver cell installs
              into the same slot later - docs/NETSTACK.md 18,
              docs/ARCHITECTURE-DEBT.md 3.2), hw (ACPI/FDT/PCIe
              discovery + the machine Inventory; block BlockDevice trait +
              virtio_blk + **nvme** drivers; virtio_rng randomness
              device driver + **tpm** (TPM 2.0 over FIFO/TIS: the TCG PC Client
              Platform TPM Profile handshake and TPM2_GetRandom, discovered from
              the ACPI TPM2 table / a tcg,tpm-tis-mmio device-tree node / a
              guarded ARM64 candidate probe) + **virtio_input** (HID keyboard/
              mouse events as entropy) - docs/TIME-IDENTITY.md 4a; virtio_net raw-frame NIC driver -
              docs/NETWORKING.md; virtio_gpu 2D display driver -
              docs/DISPLAY.md), elf + load (ELF loader for native
              programs), user run loop (with per-cell syscall
              personalities + the per-cell channel table: one end per client for
              a service cell, docs/NETSTACK.md 17), nproc (native process model:
              SYS_SPAWN/WAIT + SYS_YIELD round-robin yield + the cooperative
              cross-cell scheduler - docs/LIBRHEO.md Phase F, docs/NETSTACK.md 17),
              linux (the Linux personality:
              docs/LINUX-COMPAT.md - incl. the blocking, readiness-computing
              poll/epoll_wait, a real nanosleep, blocking stdin, and eventfd2 as
              a shared counter registry - eventfd.rs), obs (**the observability
              spine**: ring.rs is the **per-CPU event ring** - one ring and one
              sequence counter per core, safe by partitioning, dependency-free so
              `verify/obs/` can drive its wrap on the host; trace.rs is now a shim
              over it keeping the `@E` format. Plus one exported page-aligned root,
              `RHEO_OBS_ROOT`, whose
              section table carries a kernel VA *and a physical address* per
              telemetry region, so a host tool, a hypervisor or a crash dump can
              walk the whole plane from the ELF symbol alone with **zero** guest
              instructions - no device, no MMIO, no fw_cfg, and nothing
              QEMU-specific. It *indexes* the five planes rather than absorbing
              them: text is telemetry.rs, distribution is metrics.rs, and the
              event/counter/snapshot planes live here -
              docs/OBSERVABILITY.md 11), U-mode programs
              (user_progs.rs incl. the lsh shell), abi
  src/arch/   per-ISA Rust modules incl. paging.rs (one dir per ISA)
  arch/       per-ISA assembly (boot, vectors/traps, context switch, user)
  link/       linker scripts per ISA (incl. the .user text/rodata/data window)
tests/        in-QEMU test kernels: cap-invariants, queue-pipeline,
              isolation-hw, security (the syscall-surface hardening audit,
              docs/ENGINEERING.md 12: an unprivileged U-mode cell attempts an
              out-parameter into kernel .bss - refused, canary intact - an absurd
              SYS_MMAP - refused at zero frame cost - and a SYS_MUNMAP of its own
              queue region / a kernel VA / the .user window / the channel + grant
              regions - each refused, ring still serving; plus a control per
              finding so the bound is not a break),
              observe (docs/OBSERVABILITY.md 11: the observability root - the
              compile-time identity a reader checks *before* trusting a byte, the
              published physical address against the kernel's own virt_to_phys,
              every section asserted to carry an address where physical memory
              actually is (which is what catches a forgotten linear-map mask - the
              mistake that makes a host reader silently decode nothing), and the
              tick asserted to be a real advancing counter with a real frequency,
              since a timestamp domain published as zero makes every recorded time
              a lie; then the event plane end to end - the ring funded from the real
              pool, 300 records read back field-for-field with real ticks advancing,
              the ring wrapped with the surviving window located exactly, and every
              frame returned),
              resources, numa (docs/SUBSTRATE.md pillar 6: node-affine frame
              allocation - QEMU launched with two 512 MiB memory nodes, the pool
              asserted partitioned between them at exactly the boundary the launch
              named, 64 frames placed on each node with zero fallbacks, and a
              run-dry node degrading to its peer with the loss counted; x86-64 via
              the ACPI SRAT and riscv64 via the device tree, ARM64 skipping with a
              measured reason - a bare-ELF boot gets no device tree, -dtb included),
              pmem (Phase J: a MemKind::Pmem grant
              backed by a real QEMU nvdimm - x86-64 via the ACPI NFIT; arm/riscv
              skip-with-reason - docs/MEMORY.md 2.1), smp (per-CPU state + kernel spinlock +
              a real secondary core on all three ISAs: SBI HSM on riscv64,
              INIT-SIPI-SIPI + a real-mode AP trampoline on x86-64, PSCI CPU_ON
              over the probed HVC conduit on aarch64; then **all four cores** draining
              one int8-GEMM work queue; then **two cells in user mode on two
              cores at once**, each in its own address space, witnessed by each
              reading the other's progress mid-run; then **start-all + cell
              placement** - 4 CPUs online, 8 runnable cells drained by whichever
              core is free, with a dry core **stealing an unstarted cell** from a
              peer's claim; then **cross-core preemption** - each core preempts
              between the cells it claimed, ~350-400 slices taken on 4 cores at
              once against 0 in the cooperative control round; then **three unlike workloads at once across
              four cores** (one mixed queue of 8 `librheo-fa` cells - 3 computing
              FlashAttention 2+3 over slices of one head, 3 a tiled int8 GEMM, 2 driving
              8 parked strands each over their own queue pair - placed by claim, both
              assembled compute results bit-identical to a single cell computing every
              row and all 16 queue round trips correct); then an
              **node-affine placement** (docs/SUBSTRATE.md pillar 6: two memory nodes with
              the CPUs split across them, the runnable set grouped by home node with one
              claim cursor per node, 7-8 of 8 cells asserted to run on a core of their
              own node and the counters to agree exactly with the core each ran on;
              ARM64 skips - no firmware describes memory there); then **ONE
              CELL ON TWO CORES** - two *vcores* of one cell, one address space,
              claimed by whichever cores were free, each asserted to have seen the
              other advance mid-run, with per-vcore frames and outcomes observed
              load-bearing and the per-vcore kernel stack / FP area named as
              requirements the phase cannot detect; then a **VCORE YIELDING TO ITS
              SIBLING** - two vcores of one cell alternating strictly over 12 rounds
              on ONE core, the oracle only a yield that reaches the sibling context
              can produce; then **A QUEUE PAIR PER VCORE** - two vcores of one cell
              told about their own disjoint rings by `SYS_QUEUE_INFO`, each
              completing its own `OP_ECHO` round trip through its own doorbell on
              its own core; then **TWO CORES IN ONE HEAP** - 512 allocate/stamp/
              verify/free cycles each through `runtime::Heap`'s global allocator,
              0 bytes of either core's block ever holding the other's marker;
              then **TWO CORES IN ONE FRAME ALLOCATOR** - the same shape against
              `mm::frames` (128 alloc/stamp/verify/free rounds each, 0 bytes of
              either core's frame ever holding the other's marker, the used
              counter still matching the bitmap, no frame leaked), then 1024
              churn rounds each with no page touched so the two are contending
              on the bookkeeping alone - 140-469 requests executed by the OTHER
              core's flat-combining combiner, reported and never asserted - plus
              a single-core check that a frame **dirtied while free** comes back
              zeroed, which is the only shape that can catch a missing memset in
              a rotating allocator (docs/SMP.md 10.0g,
              docs/ENGINEERING.md 11); then **STRANDS ACROSS VCORES** - 64 `spawn_shared` strands each
              asserted to run exactly once, drained by two cores at once and then
              spawned on one vcore and executed entirely by another
              (docs/CONCURRENCY.md); then **A CELL ASKING WHICH VCORE IT IS** -
              the same binary in two contexts reading indices 0 and 1 out of
              `SYS_VCORE_INFO`; then **A LOADED CELL RUNNING THE MULTI-VCORE
              EXECUTOR** - `librheo-vcore`, one ELF in two contexts with its own
              ring and stack each, vcore 0 filling the injector and vcore 1
              running all 32 strands (docs/CONCURRENCY.md); then an
              **unmodified static-glibc binary as a Linux cell on a secondary**,
              exact stdout + exit asserted, overlapping a native cell on the
              primary; then **two Linux cells on two cores at once**, each
              transcript captured separately and asserted; then **FOUR LINUX
              CELLS ACROSS FOUR CORES** - the docs/SMP.md 10.2 audit's 'many
              Linux cells' question, each with its own exact transcript and exit,
              with the honest note that removing the personality lock does not
              make it fail; then **A LINUX CELL FORKING OFF THE BOOT CPU** -
              `af_unix` (socketpair+fork+bind/listen/connect/accept) on one core
              beside `chello` on another, both transcripts exact, 0 double
              entries and the affinity test asserted to have refused
              (docs/SMP.md 10.0b/10.0c) - docs/SMP.md 10.0; these three now live
              in their own **linuxsmp** kernel, since together they pushed riscv64
              past the 120 s budget inside `smp`, and they close with **TWO CELLS
              HAMMERING THE GLOBAL REGISTRIES** - `regstress`, 256 rounds each of
              pipe + eventfd create/use/free on two cores, every value keyed on the
              caller's pid, whose control DOES fire: `plock` off gives
              `regstress FAIL 5`, docs/SMP.md 10.0d; and a **DYNAMIC cell off a
              LIVE ext4 DISK on a SECONDARY** - the Node/Bun/Claude-Code load path
              (block device, ext4, PT_INTERP, file-backed mmap, demand paging)
              proven off the boot CPU, docs/SMP.md 10.0e), linuxbunsmp (the REAL
              Bun binary on a SECONDARY core: same disk, JIT and preemption as
              linuxbun, 83 slices taken on that core, `rheo:42` and exit 0 -
              x86-64 only, docs/SMP.md 10.0e), linuxnodesmp + linuxclaudesmp (the
              same for the REAL node and the REAL 275 MB Claude Code binary on a
              secondary - `rheo:42` and `2.1.220 (Claude Code)`, 23 and 1,612
              preemption slices taken on that core, docs/SMP.md 10.0e), shell-smoke, hwinfo, rng, runtime
              (the strand runtime, closing with the **measured** concurrency /
              async / sync phases: 256 strands in flight with every round a
              permutation, 63 I/O ops outstanding at one instant with one
              park+wake each, and 256-way mutex contention with never more than
              one holder - each with an observed negative control),
              posix, blockfs (live virtio-blk disk), nvmefs (the same ext4 image
              behind a **real NVMe controller** - PCIe BAR0 mapped, admin + one I/O
              queue pair brought up, files read through the identical VFS, plus a
              write round trip on the last sector, plus **DRIVERS.md D2 leg 1**:
              an unprivileged cell reads the controller's VS register through a
              launcher-mapped BAR window and one page past it faults), elfrun (load a native
              ELF), posixrun (native program over the POSIX syscalls),
              libcrun (a program linked against rheo-libc), jsonrun (a
              program parsing JSON with rheo-json on-OS), stdrun (a real-std
              program on-OS), librhearun (librheo Phase A: a loaded cell with
              a real mapped queue pair does heap+rng+cap + an async queue
              round-trip, docs/LIBRHEO.md), librheodata (librheo Phase B: the
              mini-DuckDB scan - typed grants + async I/O opcodes + a zero-copy
              columnar scan off a live virtio-blk disk), librheocompute (librheo
              Phase C: parallel map_reduce + a userspace graph submitted to the
              CPU engine + reservation admission), librheoterm (librheo Phase D:
              the interrupt-driven console wakeup + the term byte-stream
              discipline - scripted editing/history/escape, idle-park on all three
              ISAs),
              librheowl (librheo Phase E: the Wayland-class compositor demo -
              two cells share a typed cross-cell queue pair + pass a sealed
              buffer grant zero-copy + a flip completion, checksum-verified),
              coreutils (the coreutils multicall cell, with
              argv + std::fs over the VFS), linuxrun (Personality::Linux:
              bare Linux-ABI programs plus, at L2, unpatched static-glibc
              Rust + C hellos), linuxtools (L3: the unmodified upstream
              uutils/coreutils multicall cell over a ramfs, incl. threaded
              sort), linuxthreads (L4: an unpatched multi-threaded Rust std
              binary - clone/futex/TLS/join), linuxsig (L5: signal delivery -
              async raise, fault->SIGSEGV handler, SIG_DFL terminate, plus
              `sig_fp`: FP/SIMD registers survive a handler that clobbers them -
              eight doubles pinned in *caller-saved* FP registers inside one asm
              block that also contains the faulting store, since the two obvious
              C formulations both passed with the fix deleted),
              linuxproc (L6: fork/execve/wait4/cross-cell pipes - a direct
              multi-process C fixture + the P11 coreutils-suite shell; plus
              `stackx`, which asks for 12 MiB of stack via PT_GNU_STACK and both
              reads it back through RLIMIT_STACK and writes 9280 KiB of it
              through - the loader used to hand every cell a fixed 8 MiB - and
              `sysx`, the seven measured syscalls: eventfd2's full wakeup contract
              (an empty counter is NOT pollable-readable, a dup shares it,
              EFD_SEMAPHORE decrements), a real sysinfo, sched_setscheduler
              refusing real-time with EPERM, close_range, and clone3/rseq refused
              deliberately; and `preemptfork`, two single-context Linux
              processes spinning at once with no syscall between them - the
              only shape that reaches `linux::proc::preempt_cell`, the
              move-to-another-*cell* arm every other preemption proof skips,
              with the same binary under cooperative dispatch as the control
              at 0 cross-cell preemptions),
              linuxclaude (GOAL-CLAUDE: the real 275 MB Claude Code binary - a
              Bun-compiled single-file executable - streamed off a live ext4 disk,
              JIT enabled, preemptive, printing its exact version string and
              exiting 0; `--version` only, since a conversation needs outbound
              TLS from a cell - stated, not implied),
              linuxdyn (L7: an unmodified dynamically-linked glibc C hello over
              PT_INTERP + ld-linux + fd-backed mmap - three ways: loaded direct,
              execve'd from a ramfs VFS, and (GOAL-DISK-2b) execve'd off a real
              ext4 image on a live virtio-blk disk via ext4fs/ext4plus + the block
              cache, streamed on demand), librheoproc (librheo Phase
              F: native spawn/wait + one-shot timer + the lrsh shell + the
              embedded spine-only cell), librheonet (librheo Phase G: raw-frame
              networking - virtio-net driver + net::send/recv/mac, an ARP round
              trip via SLIRP), netwait (rheo-net N2d: true async receive - the NIC
              RX interrupt + SYS_WAIT_NET park; a cell parks on net::recv, wakes on
              SLIRP's ARP reply + a TCP reset, then parks on a deadline, with one
              reactor wakeup per receive and a genuine kernel idle-park on
              riscv64/aarch64; plus N2h: the timer-arbiter conflict regression -
              the pre-N2h lost-deadline pattern reproduced as a false expiry,
              three concurrent deadlines each honoured in order with none lost, a
              cell sleep surviving a full receive wait - and the adaptive
              hot/warm/cold poll escalation, law + observed counters),
              librheogpu (librheo Phase H: a real GPU -
              virtio-gpu 2D driver + display::Scanout present, the create-2d/
              attach/set-scanout/transfer/flush round trip, headless-honest),
              librheoipc (librheo Phase J: symmetric async IPC - two cells
              ping-pong typed messages over the async Sender/Receiver, each recv
              a genuine reactor park), librheopipe (librheo Phase J: a cross-cell
              stdout pipeline - an orchestrator spawns a producer child that
              inherits its channel and streams its output back), netservice
              (rheo-net N4a: the network service cell + concurrent fan-out - one
              service cell serves 3 client cells, each over its own cross-cell
              channel, one strand per client; distinct correct responses + the
              round-robin interleave/in-flight/park-wake witnesses + reaping,
              deterministic core plus a bonus live ARP), linuxnet (rheo-net N4b:
              remote INET for unmodified Linux binaries - an unmodified
              static-glibc C binary does a real DNS round trip to SLIRP's
              resolver 10.0.2.3:53 and a real remote TCP connect over the NIC,
              through the svc::SocketOps bridge + inet_personality.rs; plus the
              `resolve` fixture: glibc's own getaddrinfo over the seeded
              /etc/{nsswitch.conf,hosts,resolv.conf} - a loopback send with no
              listener refused, /etc/hosts resolved deterministically, a live
              public name reported), nethttp
              (rheo-net N5a: HTTP/1.1 + HTTP/2 - the zero-copy h1 codec with its
              22 request-smuggling rejections + the SWAR-vs-scalar scan oracle, an
              h1 client<->server exchange over the in-cell TCP VirtualLink
              (POST+body, chunked, keep-alive, 404), the RFC 7541 Appendix C HPACK
              known-answer vectors incl. Huffman, the h2 connection (preface,
              SETTINGS, concurrent streams, WINDOW_UPDATE-gated body, RST/PING/
              GOAWAY), and one h1 exchange through the TLS 1.3 record layer with
              ALPN; pure compute, live GET skipped-with-reason), nethostcfg
              (rheo-net N4c: host configuration - the DHCP byte oracle + full
              state-machine walk (renewal/rebind/expiry/NAK/DECLINE/RELEASE +
              seven rejections), the hostcfg store read back by dns::Config and
              udp::UdpEndpoint, IPv4 link-local (0.0.0.0 ARP probe, conflict
              re-pick, bounded announce, defend-once-then-yield), mDNS over the
              DNS codec, and the NTP offset/delay KAT as a bounded interval;
              deterministic core plus four duration-bounded live phases (SLIRP's
              real DHCP lease reported, NTP/mDNS skip-with-reason) and the
              per-ISA wait-mode assertion - NIC-interrupt park on riscv64/aarch64,
              timer-backed idle on x86-64),
              preempt (docs/SUBSTRATE.md 15 S3': timer preemption + queue-driven
              dispatch, plus the **scratch-register** property - a cell pins
              sentinels in RCX/R11 inside one asm! block containing its spin loop
              while a sibling yields, so a preempted frame resumed through SYSRET
              (which consumes both) fails deterministically; x86-64 only, the
              other two skip-with-reason since eret/sret consume nothing. The
              preemption itself: two cells run a compute loop that issues NO
              syscall, with the cooperative case asserted as the negative control
              in the same binary - cell 0 unbroken for all 24 rounds and cell 1
              never scheduled, then with dispatch on the shared order vector
              interleaves and the longest run drops to 2-9),
              schedidle (docs/ARCHITECTURE-DEBT.md 2.4, the keystone: the
              scheduler idle state - two cells share one page read-write and each
              appends its own marker to an ordering vector, so the hand-computed
              oracle `bSSSSSSSSB` proves the peer ran all 8 rounds *while* the
              other was blocked on a timer and then on the console, with the
              kernel asserted to have genuinely halted; plus the wedge-free
              refusal to park a receive that can never be satisfied, the deadlock
              classifier, and the docs/ARCHITECTURE-DEBT.md 2.5 system-wide
              admission ledger refusing a second 90%),
              linuxpoll (the Linux half of 2.4: `pollx` - an unmodified
              static-glibc binary - asserts an empty pipe is NOT readable, a
              closed fd is POLLNVAL, both poll and epoll_wait timeouts really
              elapse on the program's own clock, an indefinite poll is woken by a
              forked peer, a 40 ms nanosleep sleeps, and pipe2(O_NONBLOCK) reports
              EAGAIN with no fcntl; `polldead` polls forever on its own empty pipe
              and the run ends with DEADLOCK_EXIT plus a diagnostic naming the
              blocked pid, not a kernel panic),
              nettcpcc (rheo-net N2b + N2e: congestion control - the Reno/CUBIC
              integer cwnd trajectories against their oracles, and BBRv3 as the
              default: the scripted Startup/Drain/ProbeBW/ProbeRTT walk, the
              max-bw + min-RTT filters incl. expiry, the **loss != congestion**
              headline (BBR holds 100% of the link rate through random loss where
              CUBIC falls to 37%), connection-level paced release intervals,
              BBR-vs-Reno loss recovery, the pacer's CPU-reservation admission and
              its refusals, 14 live releases parked on kernel timer-arbiter pacer
              deadlines, and kernel-side the arbiter's continuous-re-arm property -
              40 pacer deadlines with an RTO + a cell sleep outstanding throughout,
              none lost; docs/NETSTACK.md 21),
              gpuhw
              (real-GPU stage 1, docs/GPU-HARDWARE.md 12: PCIe bridge
              recursion + BAR sizing/assignment + capability walk + vendor
              recognition + driving every GPU QEMU models: AMD ati-vga,
              Bochs, Cirrus, VMware, QXL by framebuffer-aperture write+read-
              back and virtio-gpu (behind a root port) by its 2D driver - six
              vendors x86, four arm/riscv; NVIDIA/Intel skip-with-reason - +
              GPU engine registration), iommu (real-GPU stage 2, docs/GPU-HARDWARE.md
              4/12 + BUILD-ORDER step 12: DMA remapping proven with a real
              virtio-blk DMA that is mediated by an identity domain then
              FAULTS when the domain is revoked - x86-64 via VT-d
              (intel-iommu: root/context/second-level + queued invalidation)
              and ARM64 via SMMUv3 (smmuv3: stream table + Context Descriptor
              + LPAE stage-1 + command/event queues); riscv skip-with-reason,
              no QEMU IOMMU model), librheotile (the tile framework,
              docs/TILES.md: TileBuf/TileProgram/CpuExecutor + the graph
              lowering, a tiled int8 GEMM bit-exact vs a naive reference, the
              deterministic TileSim, and the full dtype matrix - F16/Bf16/FP8
              E4M3+E5M2/TF32/int4 round-trips), librheotilebattle (the tile
              battle tier: scaled 7B-class GEMMs, an attention block, paged-KV
              prefix sharing, the columnar reduce, soak + boundary + pipeline
              stress),
              bench-core, and the interactive
              lsh bin. Shared support, all #[path]-included (docs/
              ARCHITECTURE-DEBT.md 5): harness.rs (the `.user`-window cell
              builders **plus** run_elf_cell/run_linux_cell - one native launch
              and one Linux launch, replacing 22 hand-written copies),
              fixture.rs (cell!/linux!/linux_cargo! - the per-ISA
              include_bytes! path, once, which is also what makes the
              docs/TARGET-ARCHITECTURES.md 4.1 cfg exemption auditable),
              console_personality.rs (the console-only FileOps: console_only
              for ENOSYS-everywhere, console_and_empty_fs for the
              ENOENT/EBADF shape - two variants because collapsing them would
              change what a cell that *did* make a file call observes),
              vfs_personality.rs (the real-file FileOps over the POSIX VFS),
              inet_personality.rs (the N4b remote-INET datapath registered as
              svc::SocketOps);
              fixtures/ holds the
              ext4 test image (+ gen-ext4.sh); linux-fixtures/ (incl. tilelinux/ - the
              tile kernels `#[path]`-included from librheo and built as a static-glibc
              Linux binary, so `smp` can assert the two substrates agree bit for bit,
              docs/TILES.md 13.4b; and tileso/ + dlopentile.c - the same
              GEMM as a shared library plus a probe that `dlopen`s it at run time, the
              route a JS runtime's FFI takes, docs/TILES.md 13.4c) holds the
              built-from-source glibc test binaries (rusthello/ + rustthreads/
              + hello.c + sig_{raise,segv,dfl}.c + procdemo/cecho/rsh.c +
              dhello.c + af_unix.c + inet.c + inetremote.c + regstress.c (the
              registry-stress fixture: pipes + eventfds hammered in a loop, every
              value keyed on the caller's pid, so two cells on two cores detect a
              shared slot rather than tolerate one - docs/SMP.md 10.0d); coreutils via cargo
              install, and the L7 ld.so/libc.so.6
              copied from the toolchain - all gitignored)
comparison/   seL4 comparison: methodology, sel4bench script, RESULTS.md; plus
              linux/ - the tuned-Linux (CachyOS-class) comparison: the axis
              measurable today is the **scheduler's ordering decision** (both
              ship EEVDF+BORE), with rheo_sched.rs running the shipped
              sched/{bore,vcore}.rs unedited over a scripted interactive+hogs
              trace and sched_latency.rs asking the host Linux the same question
              in ns - different units, never divided. Every other axis is
              lab-gated and named; no number in the tree says rheo-os is faster
              than Linux (docs/SUBSTRATE.md); plus ethos/ - the Ethos OS design
              comparison (paper-driven; the source needs an authorized key and
              is stated unreachable): Etypes' type-hash-at-the-boundary and
              image-measured identity taken as G-gated debt rows, MinimaLT's
              transport lessons folded into NETSTACK.md N7, paired-OS/Xen
              delegation and per-message kernel type checks refused with reasons
xtask/        build/run/test/bench/verify orchestration (cargo xtask ...)
verify/       host-side model checking of the kernel state machines that are
              integer-only and dependency-free (docs/EXECUTION-MODEL.md 8): each
              driver `#[path]`-includes the shipped kernel source verbatim and shims
              only the storage the kernel funds from frames (the comparison/ rule).
              entity/ drives `sched/entity.rs` - 20,000 sequences x 400 operations
              over 24 entities and 4 CPUs, checking seven invariants after every
              step, with the operations being the edges of EXECUTION-MODEL.md's
              dependency graph so coverage is **asserted** rather than reported.
              Seven invariants, seven controls observed firing - including `steal`
              ignoring `inside`, which IS "migrate a running entity", the capability
              attempted and reverted twice on real hardware and named here in 213
              operations on the first seed. It does not replace `cargo xtask test`:
              it checks state machines, not the trap path, the page tables or the FP
              register file (verify/README.md).
              obs/ drives `obs/ring.rs` - the per-CPU event ring against an
              independent VecDeque oracle at four counter start points, including one
              **crossing 2^32**, where the recorded sequence number recycles (~71
              minutes at one event per microsecond, so a boot never reaches it and
              would ship the arithmetic untested). u64 wrap is deliberately NOT
              tested: 584 years at one event per nanosecond, so the ring makes no such
              claim and asserting it would invent a requirement. Two controls firing
              (slot mask off by one; zero-based sequence numbers), one honest
              non-result recorded (`get`'s seq check cannot fire single-threaded -
              the bounds test subsumes it, and it only earns its keep against a
              reader racing a live writer)
idl/          system IDL + codegen        (future, step 6)
runtime/      strand runtime: heap (alloc), async executor + channel,
              type-level capability rights (BUILD-ORDER step 7)
userland/     native U-mode programs built for a bare target and loaded
              from an ELF (docs/USERLAND.md): hello, iodemo
libc/         rheo-libc: the Rust libc translation layer (crt0, heap +
              allocator, malloc, fd I/O, println) + the libcdemo/jsondemo
              programs
librheo/      the native userspace foundation library (docs/LIBRHEO.md):
              no_std+alloc, mem (grow heap + typed grants/arena/mapping)/rng
              (per-cell DRBG)/cap (typed handles)/rt (strand executor + userland
              queue reactor)/sys (syscall + on-wire queue ABI)/io (async
              File/read_at/write_at/Contract)/store (Dataset)/compute
              (map_reduce/parallel_for/scan strand workers + Engine::info +
              GraphBuilder)/sched (Reservation + lattice-rt Priority/PeriodicTask/
              TimingReport)/tile (the unified tile framework - docs/TILES.md; incl. fmath's
              from-scratch exp2f/expf and attn's FlashAttention 2/3 over one
              shared online-softmax recurrence, TILES.md 13)/term (Phase D
              byte-stream input/edit/render)/ipc
              (Phase E cross-cell Channel + sealed-buffer share + Phase J symmetric
              async Sender/Receiver on the reactor + rheo-net N4a multi-slot
              Channel::open_slot: one end per client for a service cell)/display (Phase E
              Surface/Compositor/InputEvent + Phase H Scanout/Gpu real GPU present
              over OP_GPU_PRESENT - docs/DISPLAY.md)/proc (Phase F spawn/wait/args/
              env + Phase J spawn_piped: a spawned child inherits the parent's
              channel as its stdout pipe + rheo-net N4a spawn_on_channel: a service
              hands each spawned client its own channel slot)/time (Phase F clock + async
              sleep/timeout)/net (Phase G raw-frame
              send/try_recv/mac over OP_NET_* + the N2d parking recv/recv_timeout
              over SYS_WAIT_NET - docs/NETWORKING.md, docs/NETSTACK.md 16) +
              crt0 (feature-gated: default=full, --no-default-features=embedded
              spine) + the librheo-demo (Phase A), librheo-data (Phase B
              mini-DuckDB scan), librheo-compute (Phase C parallel compute + graph
              + QoS), librheo-term (Phase D terminal), librheo-wl (Phase E
              compositor demo), Phase F: librheo-orch (spawn/wait/timer proof),
              lrsh (the librheo-native shell), librheo-echo/librheo-child (native
              coreutils it spawns), librheo-embed (the embedded spine-only cell),
              librheo-fa (three workloads in one binary -
              FlashAttention 2+3, a tiled int8 GEMM, or strands doing async queue round
              trips - so a mixed queue of these placed across cores runs an attention
              head, a GEMM and the reactor at the same instant, docs/TILES.md 13.4a),
              librheo-net (Phase G ARP round trip over virtio-net), librheo-netwait
              (rheo-net N2d parked receive: woken by a real frame, then by a
              deadline), librheo-gpu
              (Phase H virtio-gpu 2D present round trip), librheo-ipc (Phase J
              two-cell async Sender/Receiver ping-pong), and librheo-pipe/
              librheo-pipesrc (Phase J cross-cell stdout pipeline: a spawned
              producer child streams its output to the parent over the channel),
              and librheo-vcore (a cell running the multi-vcore strand executor:
              one ELF in two contexts, told apart only by SYS_VCORE_INFO)
              programs
net/          rheo-net: the greenfield network stack as portable userspace
              (docs/NETSTACK.md, docs/NETWORKING.md) - no_std+alloc, no per-ISA
              code, built for the bare targets as loaded ELF cells over
              librheo's raw-frame path: eth/arp/ip (N1a), udp/icmp/wire (N1b),
              dns caching resolver (N1c), trace (N1e), local fast path (N1d),
              tcp + timer wheel (N2a), cc Reno/CUBIC (N2b), smoltcp_cell +
              shard (N2c), crypto (N3a) + tls (N3b, both feature-gated),
              service (N4a: the service-cell framework - one channel end +
              one strand per client, so one service cell serves many clients
              concurrently), hostcfg + dhcp + zeroconf (link-local + mDNS) + ntp
              (N4c: host configuration - the store the stack reads for its
              address/netmask/gateway/resolvers/search domains, a DHCP client, RFC
              3927 link-local, mDNS over the dns codec unchanged, and an SNTP
              client whose answer is a bounded interval; docs/NETSTACK.md 20),
              bbr + pacer (N2e: BBRv3 as the default congestion control - the
              windowed max-bandwidth + min-RTT filters, round-trip counting, the
              Startup/Drain/ProbeBW/ProbeRTT machine and its gains, a loss response
              that caps in-flight instead of collapsing the window; plus the send
              pacer, a token bucket whose release deadline rides the kernel timer
              arbiter's Pacer slot - pacing is a precondition for BBR, not a knob;
              docs/NETSTACK.md 21),
              and http1 + http2 (N5a: HTTP/1.1 - a zero-copy,
              smuggling-hardened codec + chunked framing + a transport-agnostic
              Client/Server; HTTP/2 - frames, streams, both flow-control levels,
              and HPACK with the RFC-generated Appendix B Huffman table - both in
              the always-compiled half, docs/NETSTACK.md 19). Two postures: `hosted` (default - the
              librheo-driven async endpoints) and the librheo-free **codec**
              posture (`--no-default-features`: eth/ip/arp/udp/tcp/cc/shard/wire
              only), which is what links beside a *kernel* for the N4b
              SocketOps bridge (docs/NETSTACK.md 18).
              Plus the netcore/netl4/netdns/nettrace/netlocal/
              nettcp/nettcpcc/netsmoltcp/netcrypto/nettls/nethttp/nethostcfg
              demo bins and
              netsvc-demo + netsvc-client (the N4a service + its clients)
json/         rheo-json: a dependency-free, zero-copy JSON parser (scalar +
              SSE2 string-scan), no_std, host-tested + benchmarked
              (docs/JSON.md, comparison/json/)
ext4fs/       the disk ext4 driver: an adapter from the `ext4plus` crate
              (Google's read-only ext4-view; no_std; `sync` mode) to
              posix::FileSystem over a posix::BlockSource - kept in its own crate
              so ext4plus's deps stay out of the dependency-free posix. Replaced
              the hand-rolled parser (docs/FILESYSTEMS.md names the crate + deps
              per the no-deps rule). Used by the blockfs/posix test kernels.
services/     system service cells        (future, phase 5)
targets/      rheo-os custom target specs + the std port: rheo_os-*.json,
              patch-std.py (rust-src std patch: heap/stdio/args/env/fs arms),
              std-rheo/ (rheo sys sources + the rheo-rt crt0, the std proof
              program, and the rheo-coreutils multicall cell).
              docs/USERLAND.md M4/M5
```

## Rules

- **The engineering standard.** `docs/ENGINEERING.md` governs *how* work
  lands - evidence, scope language, additivity. Its section 13 checklist
  applies to every slice.
- **Docs first.** A change that adds a kernel object or verb must pass the
  admission rule in `docs/ARCHITECTURE.md` section 6 and be reflected there
  before it lands in code.
- **One native cross-cell switch.** Every path that hands the CPU from one
  native cell to another goes through `user::switch_native_cell`, which
  swaps the FP/SIMD register file as well as the address space. Cells are
  hard-float and the kernel is soft-float, so a bare `switch_to_cell` from a
  native path silently corrupts vector registers (docs/LIBRHEO.md "FP/SIMD
  across the native cross-cell switch").
- **Portability.** Only `kernel/src/arch/` and `kernel/arch/` may differ per
  ISA. A change that needs per-ISA code anywhere else is an architecture bug
  (docs/TARGET-ARCHITECTURES.md 4). Every change must build and boot on all
  three ISAs - run `cargo xtask test --arch all` before pushing.
- **Assembly is exceptional.** Only boot, exception vectors, and the
  context-switch inner loop may be assembly (docs/TOOLING.md 1).
- **Unsafe is concentrated.** Keep `unsafe` in small, audited, documented
  blocks behind safe wrappers; no `unsafe` outside arch/MMIO/DMA code
  without a written justification.
- **No new dependencies casually.** xtask has zero dependencies and the
  kernel has none; keep it that way unless a doc names the crate.
  docs/SUBSTRATE.md 11 formalizes this into tiers: Tier K
  (kernel/abi/posix/xtask) stays zero-dep permanently; Tier S (librheo/net/
  cells) may take named, pinned, vendored-or-hash-locked deps that build on
  all three ISAs; Tier A (apps/fixtures) pins crates.io freely, built from
  source by xtask.
- **CI must stay green** on all three ISAs (.github/workflows/ci.yml). A
  panic in the kernel exits QEMU with a failure code, so CI catches it.

## How benchmarks stay honest

`cargo xtask bench` boots tests/src/bench_core.rs under QEMU
`-icount shift=0` - results are deterministic instruction path lengths,
never wall-clock claims (QEMU has no caches/TLB; hardware gates P1-P12 at
the lab, docs/TOOLING.md 4). The kernel self-calibrates the
tick:instruction ratio each run. Comparisons against seL4 must use the
same QEMU + icount setup on both sides: comparison/README.md.

## How the boot test works

`cargo xtask test` boots every test kernel headless in QEMU with a 120s
timeout and reads the QEMU process exit code (docs/DEVELOPMENT.md 6):
x86-64 uses isa-debug-exit (success exits 33), ARM64 uses semihosting
SYS_EXIT (exits 0), RISC-V uses the sifive_test device (exits 0). Serial
output lands in `target/qemu-<arch>-<bin>.log`.
