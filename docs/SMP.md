# SMP - Per-CPU State, Locking, and Secondary-Core Bring-Up

**Status:** Phase 1 (task #27). Per-CPU state + a kernel spinlock are done and
portable, and **a genuine secondary core now runs kernel code on all three ISAs** -
RISC-V (SBI HSM), x86-64 (a real-mode AP trampoline released by INIT-SIPI-SIPI) and
ARM64 (PSCI `CPU_ON` over the HVC conduit). Getting x86 there first required fixing
the root cause that had blocked it: the local APIC is now driven over **xAPIC MMIO**
with the access mode chosen by probe, which also makes the x86-64 one-shot timer
genuinely interrupt-driven.

This is *proof of a second core running kernel code plus the foundation for real
SMP* - **not** preemptive multi-core scheduling. Each secondary does observable
proof-of-life work and parks; nothing in the kernel is yet safe to run on two cores
concurrently, and the runtime stays single-CPU cooperative
(docs/CONCURRENCY.md). Section 9 has the full accounting. **Section 10 is the
docs-first design for phase 2** (task #132: the SMP-safety audit, per-CPU stacks +
start-all, the preemptive tick, and the EEVDF+BORE per-CPU scheduler with NUMA and
P/E-core placement) - the phase that turns the parked secondaries into the measured
answer to Bun's worker starvation (GOAL-BUN).

This doc pairs with CONCURRENCY.md (which describes the *userspace* strand/vcore
model) - SMP here is the *kernel-level* story: physical cores, per-CPU kernel
state, and starting a second core.

Adds **no kernel object and no syscall verb**: SMP is pure mechanism (per-CPU
state, a spinlock, and an arch bring-up call), so the ARCHITECTURE.md section 6
admission rule does not apply. No new kernel dependency.

## 1. What was blocked, and what changed

CPU *detection* was already done: the machine `Inventory`
(`kernel/src/hw/mod.rs`) counts cores and records NUMA affinity (4 cores on
x86-64 and RISC-V here). What was missing was (a) the portable per-CPU + locking
foundation the kernel needs before more than one core can safely run, and (b)
actually starting a second core.

Three things now exist:

1. **Portable per-CPU state + a kernel spinlock** (`kernel/src/smp.rs`) - real
   groundwork, valuable regardless of how many cores run.
2. **A genuine secondary core on every ISA**, running kernel code through that
   foundation, then parking: RISC-V via SBI HSM `hart_start` (section 3), x86-64
   via a real-mode AP trampoline released by INIT-SIPI-SIPI (section 6), ARM64 via
   PSCI `CPU_ON` over the HVC conduit (section 7).
3. **A working local APIC on x86-64** (section 5), which the SIPI - and the
   one-shot timer, and any IO-APIC-routed line - all depend on.

The earlier phase started only RISC-V and reported a specific blocker on the other
two. Both blockers were real observations with too-narrow attributions; section 4
records what they were and why re-testing them paid.

Everything is **opt-in**, and enforced at the build level: the whole SMP module
and its arch hooks sit behind a `kernel/smp` cargo feature that is **off by
default**. Merely adding a module perturbs LLVM's codegen-unit hashing (it shifts
`.text` by a few hundred bytes), which must not reach kernels that never use SMP,
so the `smp` test kernel is built in its own cargo invocation with the feature on
(the `smp` bin carries `required-features = ["smp"]`, so the main `-p qemu-tests`
build skips it) - mirroring how `librheo-embed` is built separately. Result: the
31 other test kernels link a **byte-identical** `kernel` lib (verified: their
`.text`/`.rodata`/`.bss` sizes are unchanged), so there is no behaviour or timing
change for code that never opts in. The per-CPU primitive also defaults to CPU
index 0.

### The byte-identity property is superseded (docs/SUBSTRATE.md pillar 3)

**As of the Substrate 2 work, the paragraph above describes the bring-up phase,
not the current tree.** The *bring-up half* of `smp.rs` - everything that calls
`arch::smp_*` - is still behind the feature exactly as described. The
**primitives** (`SpinLock`, the generic `PerCpu<T>`, `cpu_index`) are now
compiled unconditionally, so the non-SMP `kernel` lib is no longer byte-identical
to the pre-SMP one.

That was a deliberate trade, and the reasoning is worth keeping: per-CPU-ness is
a property of a *data structure*, not of a build configuration. The subsystems
re-founded on it - the timer arbiter (one hardware one-shot per core), the
metrics histograms, the DRBG roots, the vcore run queues - are correct only if
each core owns its own instance, and had the container expressing that stayed
feature-gated, every one of them would have to be written **twice**: once as a
global `static mut` and once per-CPU. Two implementations of one subsystem is
precisely the shape that produced the FP/SIMD `SYS_YIELD` corruption
(docs/LIBRHEO.md), and §10.2's audit is a list of statics whose ownership
discipline must be stated in exactly one place.

The property that replaces it is stronger and more useful: **enabling the feature
must not change single-CPU behaviour.** Without `smp`, `cpu_index()` is a
compile-time `0`, so every `PerCpu<T>` resolves to slot 0 with no run-time
indexing and every `SpinLock` is an uncontended flag. The guarantee moved from
"the binary is unchanged" to "the semantics are unchanged", which is what
actually matters once per-core state exists.

**One real defect came out of this and is fixed.** `cpu_index()` is the addressing
rule for every per-CPU structure, so it must be **total** - an out-of-range answer
is not a wrong number, it is a wild memory access. x86-64 and ARM64 search a table
of hardware ids and fall back to 0, so both were already safe before bring-up.
RISC-V read `tp` directly, and nothing had set it: SBI leaves whatever the previous
stage put there until `smp_set_this_cpu` runs. That never mattered while the only
caller was `this_cpu()` (reached after bring-up), but the new per-core subsystems
touch per-CPU state during boot, and the `smp` kernel panicked with
`index out of bounds: the len is 64 but the index is 2147790848`. Fixed at both
levels: `kernel/arch/riscv64/boot.S` zeroes `tp` before any Rust runs (the boot
hart genuinely *is* CPU 0 until told otherwise, so the value is correct rather
than garbage that happens to be masked), and `smp::cpu_index()` now bounds
whatever the arch layer reports - the structural backstop, so a future port that
forgets to initialise its identity register gets CPU 0 instead of corruption.

## 2. The portable foundation (`kernel/src/smp.rs`)

**`SpinLock<T>`** - a test-and-test-and-set spinlock. The acquire loop spins on a
relaxed load (sharing the cache line read-only) and only attempts the
`compare_exchange` when the lock looks free; the `Acquire` success / `Release`
unlock pair gives standard critical-section ordering, and `core::hint::spin_loop`
hints the CPU. A guard releases on drop. It is for *short* kernel critical
sections that cross cores; a longer wait belongs on the runtime's async `Mutex`
(CONCURRENCY.md 6).

**Per-CPU registry** - a fixed `[PerCpu; MAX_CPUS]` array (`MAX_CPUS` from the
inventory) indexed by *CPU index*. Each block holds the hardware CPU id (hart id
/ MPIDR affinity / APIC id) and an online flag; a real SMP kernel grows this
block later (per-CPU run queue, current cell, timer state). `this_cpu()` resolves
to the block for the CPU the call runs on via `arch::cpu_index()`.

- **CPU index vs hardware id.** The registry index is *not* the hardware id. The
  boot CPU is registry index 0 (`init`); secondaries take 1, 2, ... as they come
  up (`NEXT_INDEX`). This matters because the boot hart id is often not 0 - QEMU's
  RISC-V `virt` picks the boot hart at random (hart 2 in practice), and the
  started secondary was hart 0. Conflating the two collides slots.
- **Per-CPU identity.** On RISC-V the CPU index lives in `tp` (the thread
  pointer, free in S-mode kernel context - no cell is running in the SMP test,
  and `tp` is only meaningful as user TLS while a cell runs). `cpu_index()` reads
  `tp`; each hart writes its own index as it comes up. On x86-64/ARM64 (no
  secondary runs) `cpu_index()` is a constant 0. Either way the single-CPU path
  reports CPU 0.

**Bring-up driver** - `init()` (register the boot CPU), then `bring_up_one()`
picks a target hardware id (a firmware-enumerated non-boot CPU, else the next id
after the boot CPU), asks the arch layer to start it, and waits on a **bounded**
spin for the secondary to signal itself up. The bound means a non-responsive core
skips-with-reason rather than hanging the primary.

## 3. RISC-V: a real second hart (SBI HSM)

OpenSBI runs in M-mode below the S-mode kernel, so the **SBI HSM** extension is
available: `hart_start(hartid, start_addr, opaque)` (EID `0x48534D` "HSM",
FID 0). This is the tractable bring-up path.

Flow (`kernel/src/arch/riscv64/mod.rs` + `kernel/arch/riscv64/smp.S`):

1. The primary calls `hart_start` for the target hart, passing the physical
   address of the secondary entry trampoline as `start_addr` and **its own
   `satp`** as `opaque` (so the secondary shares the kernel address space and
   page tables).
2. SBI enters `secondary_entry` in S-mode with the MMU off, `a0 = hartid`,
   `a1 = opaque`. The trampoline loads `satp` (paging on). The kernel root keeps
   a low identity of kernel RAM (`l2[2]`, built by `boot.S` for the primary's own
   turn-on), so the instruction right after `csrw satp`, still at its low PC,
   stays mapped - exactly as the primary boot does.
3. It jumps to a high-half continuation via an absolute 64-bit pointer word
   (medany cannot reach a high VA from the low PC), sets `gp` + a dedicated
   secondary stack, and calls `rv_secondary_main(hartid)`.
4. `rv_secondary_main` installs a trap vector (containment) and calls the
   portable `smp::secondary_run`, which claims a registry index, sets its per-CPU
   identity, marks itself online, writes a shared counter **through the
   cross-core `SpinLock`**, signals the primary, and returns to an asm `wfi`
   park.

**A medany detail worth noting:** `secondary_entry` lives in `.boot.text`, linked
identity-low (VMA == LMA), so its link address *is* the physical address SBI
needs. But high kernel code cannot form that low address PC-relatively under
medany (out of ±2 GiB reach), so its physical value is published in a high
`.rodata` word (`SECONDARY_ENTRY_PA`, an absolute `R_RISCV_64` reloc) that the
primary reads.

**Observed proof** (the `smp` test, riscv64): the boot hart is hart 2; the
primary starts hart 0, which comes up as registry CPU index 1, marks itself
online (`online_count() == 2`), and writes `SECONDARY_MARK` (`0x5EC0`) to the
shared counter under the spinlock. The primary reads it back and asserts the
exact value. This is a genuine second core executing kernel code and
synchronising with the first over shared memory.

## 4. What the earlier phase concluded, and why both conclusions fell

Neither ARM64 nor x86-64 ran a second core before this phase. Both made a
genuine, guarded attempt and reported a specific observed blocker, and the `smp`
test skipped-with-reason and passed. Those reports were honest about *what
happened*; both diagnoses turned out to be about the wrong thing.

### ARM64 - "PSCI CPU_ON traps from EL1"

**Observed then:** `PSCI CPU_ON: smc #0 trapped to EL1 (no EL3 firmware in this
QEMU config)`. Perfectly true: the kernel runs at EL1 with no EL2/EL3 (QEMU
`virt`, `secure=off`, `virtualization=off`), so `smc #0` is UNDEFINED there. The
attempt was guarded - a temporary exception vector, interrupts masked - so the
trap was *caught* and reported instead of killing the primary.

**What was wrong:** that is a fact about the **instruction**, not about PSCI.
QEMU's `virt` implements PSCI itself and picks the conduit from the machine
configuration - `hvc` for the plain machine, `smc` only when `virtualization=on`
gives the guest its own EL2. The default configuration answers PSCI on **HVC**,
the one conduit the port never tried. Section 7.

### x86-64 - "needs a 16-bit real-mode AP trampoline below 1 MiB"

**Observed then:** `needs a 16-bit real-mode AP trampoline (INIT-SIPI-SIPI) below
1 MiB; not implemented`. Also true, and the trampoline really did have to be
written (section 6).

**What was missing:** it was only half the blocker. INIT-SIPI-SIPI is *sent
through the local APIC's interrupt command register*, and this port drove the
LAPIC through the x2APIC MSR block, which QEMU's TCG leaves inert - so even with
a trampoline in place the SIPI would have gone nowhere. x86 SMP was blocked by the
same root cause as the x86 timer (docs/ENGINEERING.md 1), which is why section 5
comes first.

**The pattern worth keeping:** in both cases the *report* was accurate and the
*attribution* was too narrow. An honest blocker is still a hypothesis; it is worth
re-testing when something adjacent changes.

## 5. x86-64: the local APIC, over xAPIC MMIO (phase 1)

Everything interrupt-driven on x86-64 - the one-shot timer, the IPIs that release
an application processor, and the EOI any IO-APIC-routed line needs - goes through
the per-CPU **local APIC**. There are two ways to reach its registers, and
`kernel/src/arch/x86_64/mod.rs` now supports **both, selected by probe** rather
than by a feature bit:

| mode | interface | why / why not |
|---|---|---|
| **x2APIC** | the MSR block at `0x800+` | preferred where real: needs no mapping, so it works whichever page-table root is active when an interrupt lands. QEMU's TCG does **not** implement it - x2APIC is absent from QEMU's TCG feature word, so `CPUID.01H:ECX[21]` reads 0 even with `-cpu max`, and the whole MSR block is inert (`EXTD` never latches, writes drop, `TMCCT` reads 0 = "already elapsed") |
| **xAPIC MMIO** | the 4 KiB register page at `0xFEE00000` | QEMU **does** model this under TCG. Needs the page mapped uncacheable, and mapped into *every* page-table root |

`lapic_probe()` runs three steps and keeps only what it observes:

1. Software-enable the APIC (`IA32_APIC_BASE.EN`) - that MSR is architectural
   everywhere; it is the 0x800 *register block* that TCG omits.
2. If CPUID advertises x2APIC, request `EN|EXTD` and **read `IA32_APIC_BASE`
   back**. x2APIC is used only if the bit actually latched.
3. Otherwise map the xAPIC window and check the register file **answers**: write
   the spurious-vector register, read it back out of the device, require a match.
   A dropped MMIO write reads back as 0 or `0xFFFFFFFF`, so this distinguishes a
   modelled APIC from an absent one.

The validated mode is recorded in `ApicMode`, and every accessor (`lapic_read`,
`lapic_write`, and the IPI path) reads the *validated* value - never
CPUID. One set of register-offset constants serves both modes, because the x2APIC
MSR for a register is `0x800 + (offset >> 4)`; only the ICR differs in shape (a
32-bit low/high pair versus one 64-bit MSR) and is wrapped accordingly.

**Observed under QEMU 8.2 TCG, q35, `-cpu max`:** mode **xAPIC (MMIO)**. x2APIC is
advertised as absent and correctly declined. The register file answers, and
`enable_timer_irq`'s probe then sees a **real LVT-timer interrupt arrive** (a
counter incremented only inside the interrupt vector).

### The mapping: a third fixed window, shared into every root

`paging::apic_map_window()` maps physical `0xFEC00000..0xFF000000` (4 MiB: the
IO-APIC page and the local-APIC page) at a fixed kernel VA in **PML4 slot 386**,
disjoint from the pmem window (384) and the PCI-BAR MMIO window (385). Two 2 MiB
supervisor pages, `PCD|PWT` so the default PAT selects entry 3 = **UC** - strongly
uncacheable, which is what a register file needs (invisible under TCG, which models
no caches; stated so the attribute is not mistaken for decoration).

Unlike those two windows it must be reachable from **every** root, because an
interrupt can land while a cell root is active and the handler has to write EOI. So
the window's PML4 entry is recorded once (`APIC_PML4E`) and `paging_new_root`
stamps that **same** entry into each cell root: one shared PDPT, no extra frame per
cell. A kernel that never calls `apic_map_window` leaves PML4[386] zero and its
page tables are byte-for-byte unchanged.

### What this unlocked immediately

- **A genuine one-shot timer on x86-64.** `arch::timer_arm`/`timer_expired`/
  `timer_disarm`/`timer_park` are real, `timer_irq_enabled()` is true, and the
  `ktimer` arbiter halts at `hlt`. `SYS_ARM_TIMER` really sleeps; `librheoproc` now
  asserts the idle-park on this ISA too, and `nettcpcc`'s 40 continuously re-armed
  pacer deadlines are genuine hardware arms. The **single-owner invariant** is
  untouched: `ktimer` remains the only caller of `arch::timer_*`.
- **A measured, not claimed, `IdleMode::TimerIdle`** for the network receive wait -
  21 halts on a bounded wait, 253 timer-slice halts in `netwait`'s escalation
  phase, 1771 in `nethostcfg` (docs/NETSTACK.md 16).
- **`netwait`'s pre-N2h regression phase no longer skips on x86-64.**
- **A working ICR**, which is what AP bring-up needs.

One further defect fell out of it, of exactly the docs/ENGINEERING.md 1 class: the
LAPIC tick-rate calibration busy-spins a fixed TSC window, and it ran **lazily
inside the arbiter's first `timer_arm`** - so on a fresh kernel the first `sleep`'s
entire deadline was consumed by bring-up cost before the hardware was armed, and
the park never happened. Calibration now runs in `enable_timer_irq`, where that
cost belongs.

## 6. x86-64: a real application processor (INIT-SIPI-SIPI)

With a working interrupt command register, the remaining half of the old blocker
is the trampoline. An AP leaves reset in **16-bit real mode** and a SIPI carries
an 8-bit vector it turns into `CS:IP = (vector << 8):0` - so it starts executing
at physical `vector << 12`, necessarily **below 1 MiB**. The kernel is loaded at
1 MiB and above and PVH boot provides no firmware and no low staging area, so the
kernel stages the trampoline itself.

### The trampoline (`kernel/arch/x86_64/smp.S`)

**Position-fixed, not position-independent.** It is assembled as if based at
`AP_TRAMPOLINE_PA` and every intra-trampoline absolute reference is written
`AP_BASE + (label - ap_trampoline)`. That is what makes it copy-safe: the only
addresses it forms are those constants, 32-bit absolute immediates/addresses of
*kernel* symbols (which a copy does not move), and `movabs` of 64-bit kernel VAs.
Nothing is PC-relative, so the kernel patches nothing after the copy. The assembly
publishes the base it was built for (`AP_TRAMPOLINE_BASE`) and Rust asserts the two
agree, because a silent disagreement would land the AP nowhere.

**Mode ladder:** real 16 -> `lgdt` + `CR0.PE` -> protected 32 -> `CR4`, `CR3`,
`EFER`, `CR0.PG` -> compatibility -> far jump to a 64-bit descriptor -> long 64.
`CR3` is the primary's own `boot_page_tables`, so the AP shares the kernel address
space from its first paged instruction; `PML4[0]` there is the low identity map the
primary's own MMU turn-on left behind, which is exactly what keeps the trampoline's
low PC valid across `mov %cr0` (boot.S relies on the same thing).

**The AP's mode is the primary's mode, by construction.** Rather than hand-picking
bits, the primary publishes its own `CR4`, `EFER` and `CR0` into `.boot.bss` slots
(identity low, so the 32-bit stage reads them with paging off) and the AP loads them
verbatim. This is not tidiness, and the reason is a bug that was actually hit: the
kernel's page tables carry **NX** on real entries - the APIC register window among
them - and with `EFER.NXE` clear a set bit 63 is a **reserved-bit page fault**. An AP
that merely set `LME` therefore triple-faulted on its **first LAPIC read**. Copying
the control registers makes that class of divergence impossible instead of
enumerable.

**One GDT, laid out compatibly.** The trampoline carries its own GDT (the ladder
needs a 32-bit code descriptor, which the boot GDT has no reason to contain), but
puts the 64-bit code descriptor at **0x08** and data at **0x10**, matching boot.S,
with the trampoline-only 32-bit descriptor parked at 0x20. That lets the AP keep this
GDT for the rest of its life: the kernel's IDT gates all name selector 0x08, so an
exception on the AP resolves to a 64-bit code descriptor and reaches the kernel
handler rather than faulting while faulting. The first attempt instead swapped in the
primary's `gdt64_ptr` - which is the **6-byte** 32-bit form, while `lgdt` in long mode
reads 2 + 8 bytes, so it loaded a garbage base and `#GP`'d. Observed as a triple
fault; fixed by not needing the swap. The AP keeps running off the low page forever,
which is safe because the frame allocator's pool starts at 64 MiB and can never hand
that page out.

### Choosing the low page, and verifying it

`AP_TRAMPOLINE_PA = 0x8000`: 4 KiB aligned, below 1 MiB, above the real-mode IVT/BDA
and below the ACPI staging area. Because the kernel *chooses* the page rather than
being handed one, `smp_start_secondary` **checks** it rather than commenting on it -
stamping a trampoline over firmware data would be a silent memory corruption, not a
failed boot. Three checks: the firmware memory map must call the page usable RAM, and
neither the PVH `hvm_start_info` block nor the ACPI RSDP it points at may fall inside
it. Any of them failing is a clean `StartError::Blocked` with the reason.

### The sequence, and the identity

INIT (level-triggered assert) -> 10 ms -> INIT de-assert -> SIPI -> 200 us -> SIPI,
all through `lapic_send_ipi`, with the ICR's delivery-status bit polled between sends.
Every wait is bounded by wall time, so a non-responsive AP surfaces as the portable
driver's `StartError::Timeout` rather than a wedged primary.

The AP's `x86_secondary_main` takes **no argument**: it reads its own APIC id out of
its own local APIC (QEMU maps the APIC page per-CPU, as real hardware does), so the
identity in the registry comes from the hardware and not from something the primary
told it.

**Observed** (the `smp` test, x86_64): `secondary CPU 1 (hw id 1) ran kernel code -
online=2, shared=0x5ec0`. A real second core, in kernel code, synchronising with the
first through the cross-core spinlock.

### Per-CPU identity without a dedicated register

x86-64 has no register free for a per-CPU pointer the way RISC-V has `tp`: `FS`
carries a Linux cell's TLS base, and `GS`/`KERNEL_GS_BASE` are reserved for the
eventual `swapgs` per-CPU block, which is a change to the syscall entry path and does
not belong in a bring-up phase. So `cpu_index()` resolves this CPU's **own** APIC id
against a small fixed table each CPU fills as it registers. It costs one LAPIC read
and is on no hot path (only code that opted into SMP reaches `smp::this_cpu`), and it
falls back to 0 - the boot CPU - for an unregistered CPU, which is exactly the
single-CPU answer. ARM64 does the same with `MPIDR_EL1` affinity, because `tpidr_el1`
is already the owner of the current `TrapFrame` pointer in its vector code.

## 7. ARM64: a real secondary core (PSCI over the *right* conduit)

The old report said `smc #0` trapped to EL1, and it did. But QEMU's `virt` implements
PSCI itself, and `machvirt_init` picks the conduit from the machine configuration:
**HVC** for the plain machine, SMC when `virtualization=on` hands the guest an EL2 of
its own (which would want HVC for itself). The default configuration - the one this
tree boots - answers PSCI on HVC.

So bring-up now **probes instead of assuming** (docs/ENGINEERING.md 1):
`psci_call_guarded(conduit, fnid, ...)` issues either `hvc #0` or `smc #0`, and Rust
calls `PSCI_VERSION` over each in turn, keeping whichever returns without trapping and
reports a sane version. Both instructions stay guarded by the temporary exception
vector, so a machine that answers on neither reports skip-with-reason rather than
dying. The conduit and version that *answered* are printed, not the machine type that
implies them.

**Observed** (the `smp` test, aarch64): `PSCI answered on hvc #0 - version 1.1
(probed, not assumed)`, then `secondary CPU 1 (hw id 1) ran kernel code - online=2,
shared=0x5ec0`.

### The secondary entry (`kernel/arch/aarch64/smp.S`)

PSCI `CPU_ON` enters the secondary at EL1 with the **MMU off** at the physical address
the primary passed. The entry lives in `.boot.text`, which is linked identity-low
(VMA == LMA), so the PA handed to PSCI is the label's own address and `adrp` works
because the PC *is* the link address - no copy and no relocation. (The x86 trampoline
has to be copied only because a SIPI vector cannot name an address above 1 MiB.)

It adopts the primary's translation regime **verbatim** - `MAIR`, `TCR` and `SCTLR` as
the primary is running them, published in `AP_SYSREGS` - for the same reason the x86 AP
copies control registers: a divergence there cost a triple fault. `TTBR1` is the
kernel's high linear map and `TTBR0` the low identity map the primary's own MMU turn-on
built, which keeps the low PC mapped across `msr sctlr_el1`. It then jumps to a high-VA
continuation, sets its own stack, installs the kernel's real `vector_table` (containment:
a fault on this core reaches the kernel handler), and calls
`aarch64_secondary_main`, which reads its identity from its own `MPIDR_EL1`.

### ARM64 CPU enumeration - **now done**, and the deferral was wrong twice

This section used to say ARM64's `discover` reports only the boot CPU, and gave two
reasons. Both were wrong, and the cost was not a missing feature but a wrong answer,
so they are kept here rather than quietly replaced.

The first: "PSCI is not an enumeration API - `CPU_ON` starts a CPU you already name."
True of `CPU_ON`, and not of **`AFFINITY_INFO`**, which answers ON/OFF/ON_PENDING for
an affinity the platform implements and INVALID_PARAMETERS for one it does not. Asking
it about each candidate affinity *is* enumeration. (What is genuinely true is that
QEMU's `virt` places no firmware table for a bare ELF: x0 arrives as 0 and no DTB is in
guest RAM, which is why the device-tree path RISC-V uses does not apply. `discover`
tries a DTB first anyway, for the boot modes that do pass one, and falls through.)

The second: "it would move the PSCI helper out of the `smp` cargo feature and change
every kernel's inventory." That is a description of the fix, offered as a reason
against it. How many CPUs a machine has is a property of the hardware, not of a build
configuration - the same reasoning this port already applies to `mpidr_aff0`, in the
same file. The guarded PSCI call now lives in `arch/aarch64/psci.S`, always compiled;
only the secondary trampoline is behind `smp`.

**What the deferral cost.** The NVMe driver sized its per-core queue pairs from
`inventory().ncpus`, so on a four-core ARM64 boot cores 1..3 had no queue of their own
and silently shared core 0's. It did not present as an error: the same sector read back
different bytes on successive reads, no fault and no log (docs/SUBSTRATE.md S5). A field
that is constant is a field that lies, and this one was consulted by a driver that had
every right to believe it.

ARM64 now reports `firmware=Psci cpus=4` in the **non-SMP** build, and `start_all`'s
probe-the-next-id fallback - written for exactly this gap - no longer fires on any ISA.
It stays, because it is a genuine attempt whose answer is observed, and a machine whose
firmware enumerates nothing is still a machine this kernel should try to start.

## 8. Re-examining the x86-64 device interrupts

Both x86-64 device-interrupt verdicts in this tree - the UART RX line and the NIC RX
line - rested, wholly or partly, on the inert APIC. With a working one they had to be
re-tested rather than inherited.

### UART RX: the old verdict was wrong, and it now works

**The old verdict** (docs/LIBRHEO.md Phase D): "q35 routes COM1's ISA IRQ 4 through
the emulated IO-APIC, but under QEMU TCG + `kernel-irqchip=split` the LAPIC's ISR/IRR
are not modeled (they read 0) and an IO-APIC-routed line delivers the first byte but
does not reliably re-trigger."

**Why it was wrong:** every one of those observations was made through the x2APIC MSR
block. With no working EOI the first interrupt genuinely *is* the last - the in-service
bit is never cleared, so the LAPIC never accepts another interrupt at that priority.
"Does not re-deliver" was a symptom of the missing EOI, not a property of the IO-APIC.

**What is there now:** `enable_uart_rx_irq` programs the IO-APIC redirection entry for
GSI 4 (vector 0x21, fixed delivery, physical destination = the boot CPU's APIC id,
active-high, edge-triggered, unmasked), enables the 16550's OUT2 gate and its
received-data-available interrupt, installs the vector, and the handler drains the
whole FIFO before EOI'ing the LAPIC. Draining in a loop matters for an edge-triggered
line: a byte that arrived while the vector was being taken would otherwise sit in the
FIFO with no new edge to announce it.

**And it is probed, not asserted.** Bring-up puts the 16550 in loopback and writes a
byte, then briefly unmasks and requires a counter that *only the interrupt vector*
touches to move. `uart_irq_enabled()` is set only then; otherwise the redirection entry
is re-masked, the device interrupt disabled, and the poll path kept - reported, never
claimed. The probe's own byte is discarded by the handler so it cannot reach the console
ring.

**Observed** (`librheoterm`, x86_64): `UART RX interrupt verified over the IO-APIC
(GSI 4 -> vector 0x21, xAPIC (MMIO) EOI) - a real interrupt arrived`, then `input mode:
interrupt-driven (WFI idle)` and `idle-park proven (kernel idled at WFI, woke on UART
IRQ)`. So `input::interrupt_driven()` is now **true on all three ISAs**.

**One respect in which x86-64 is now the *best* of the three.** RISC-V and ARM64 both
carry a documented QEMU caveat here: their loopback does not drive the
interrupt-controller input line, so the deterministic test raises the controller line
directly (the RISC-V IMSIC MSI, `GICD_ISPENDR` for the ARM SPI). On x86-64 QEMU's 16550
loopback both delivers the byte into the receive FIFO **and** raises the ISA line, so
the test exercises the entire device -> IO-APIC -> LAPIC -> vector -> EOI path with
nothing poked by hand.

### NIC RX: the gap is narrower than the old wording, and is still a gap

**The old verdict** (docs/NETSTACK.md 16): the virtio-net NIC is driven entirely
through the `VIRTIO_PCI_CAP_PCI_CFG` config tunnel because PVH boot has no firmware to
program BARs, "so there is no mapped BAR to hold an MSI-X table, and legacy INTx would
ride the same IOAPIC path that does not re-deliver reliably".

**What re-examination establishes:**

- the **second** clause is **disproved**. That IOAPIC path demonstrably re-delivers -
  the UART RX line runs through it, verified end to end. Any future INTx attempt is no
  longer blocked by this reasoning.
- the **first** clause is no longer an impossibility either. BAR assignment and mapping
  already exist and are already used: `hw::assign_pci_bars` programs BARs from a per-ISA
  host-bridge window and `arch::mmio_map_window` maps them, which is how the AMD GPU's
  framebuffer aperture is driven (docs/GPU-HARDWARE.md 12). The virtio-net driver
  *chooses* not to need a BAR, which is a driver decision, not a platform limit.

**What is therefore honestly true:** x86-64 still has **no NIC RX interrupt**, and the
remaining work is ordinary driver work - assign the virtio-net BAR, program its MSI-X
table (or discover the q35 INTx routing), wire the vector - not a QEMU or platform
blocker. It is **not attempted here**: a claim about this line has to be earned by a
probe of its own, exactly like the UART's, and inheriting one from the UART's success
would be the same mistake in the other direction. `net_rx::interrupt_driven()` stays
false on x86-64, and the receive wait keeps `IdleMode::TimerIdle` - a real `hlt`
between polls, with the halts counted.

### Interrupt tally after this phase

| source | riscv64 | aarch64 | x86-64 |
|---|---|---|---|
| **UART RX** (console) | AIA: APLIC-S -> IMSIC -> `sip.SEIP` | GICv3 SPI 33 | **IO-APIC GSI 4 -> LAPIC vector 0x21** (new) |
| **one-shot timer** | Sstc `stimecmp` | CNTV via GICv3 | **LAPIC LVT one-shot over xAPIC MMIO** (new) |
| **NIC RX** | APLIC-S `1+slot` -> IMSIC | GICv3 SPI `16+slot` | **none** - the one remaining gap |

Every entry is verified at bring-up by an interrupt the kernel took, on a counter only
the interrupt vector writes.

## 9. What is proven vs deferred

**Proven**
- A portable `SpinLock<T>` and a per-CPU registry with `this_cpu()`, zero-impact
  on the single-CPU path.
- **A genuine secondary core on all three ISAs**, each running kernel code,
  claiming a registry slot with an identity it read from *its own* hardware
  (hart id / APIC id / MPIDR), writing the shared magic through the cross-core
  spinlock, and parking. The primary reads the value back and asserts it, plus
  that the slot is not its own and the recorded hardware id is not its own - three
  things a primary looping through the same code could not produce.
- A **probed**, not assumed, x86-64 APIC access mode and ARM64 PSCI conduit; both
  print what answered.
- A genuinely interrupt-driven one-shot timer **and** UART RX line on all three ISAs,
  each verified at bring-up by an interrupt the kernel actually took (sections 5, 8).
- **The `SpinLock` provides real mutual exclusion under two-core contention**, on all
  three ISAs. The primary and the secondary rendezvous, then each increments one shared
  counter `CONTENTION_ITERS` (20 000) times **concurrently**, every increment under the
  lock; the primary asserts the sum is **exactly 40 000**. This is strictly stronger than
  the single-cross-core-write proof, which passes even with a lock that provides no
  exclusion (there is no concurrent writer): a lock that failed to serialise the
  read-modify-write would lose updates and fall short. This is the primitive the
  phase-2 kernel-wide locks (§10.2) rest on, proven before they are built on it.
- **Start-all: multiple secondaries on all three ISAs** - RISC-V four cores at once
  (boot + three secondaries, matching QEMU's `-smp 4`), ARM64 and x86-64 three each
  (boot + two), the first slice of §10.3/§10.7-step-2. Each secondary claims a distinct
  registry slot and hardware id. The **same portable mechanism** on every ISA: the
  trampoline loads its stack from a shared `secondary_sp` word (RISC-V `la`+`ld`, ARM64
  `adrp`+`ldr`, x86-64 `movabs`+`mov` in long mode) that the primary sets - with a
  release barrier - to the right stack top before each start (`hart_start` / PSCI
  `CPU_ON` / SIPI), via the arch hooks `smp_secondary_count` / `smp_prepare_secondary`.
  Bring-up is **sequential**
  (the primary waits for secondary N online before releasing N+1), so the per-CPU stack
  hand-off is race-free: each secondary loads its own stack top from a shared
  `secondary_sp` word the primary sets before each `hart_start` (arch hook
  `smp_prepare_secondary`; the trampoline reads it instead of a hardcoded label). The
  second core must claim a **distinct** registry slot and a hardware id that is neither
  the boot CPU's nor the first secondary's - unfakeable. ARM64/x86-64 keep one secondary
  for now (`smp_secondary_count() == 1`, a no-op `smp_prepare_secondary`), so their
  bring-up is byte-for-byte unchanged; the multi-stack hand-off is done on RISC-V first
  and generalises to them next. The contention proof (above) still runs only against the
  first secondary, so its exact-sum assertion is independent of core count.

**Deferred**
- Preemptive multi-core scheduling (the runtime stays single-CPU cooperative;
  CONCURRENCY.md). Each secondary does proof-of-life work and parks; none is yet
  fed runnable cells. **This is the whole of task #27's remaining substance** - the
  bring-up is the prerequisite, not the scheduler.
- Making the shared kernel `static mut` state SMP-safe end to end (only the SMP
  test's own shared state is locked today). Nothing in the kernel is safe to run on
  two cores concurrently yet, which is why the secondary parks rather than being fed
  work.
- More than **one** secondary: each ISA's bring-up has a single dedicated secondary
  stack, and the driver starts one core. Per-CPU stacks and a start-all loop are
  mechanical once the shared state is safe.
- A per-CPU register on x86-64/ARM64 (`swapgs` / freeing `tpidr_el1`), instead of
  the small hardware-id -> index table `cpu_index()` searches today.
- Cross-CPU IPIs for anything beyond bring-up (the natural next users are a per-CPU
  timer arbiter and a remote-TLB shootdown; docs/NETSTACK.md 16 notes the arbiter
  shape).
- The **x86-64 NIC RX interrupt** - the last interrupt source any ISA is missing.
  Section 8 records why the old justification no longer holds and what is actually
  left to do.

## 10. Phase 2 design: preemptive multi-core scheduling (task #132)

**Status: partly built.** Three prerequisites have landed and are proven; the
scheduler itself has not.

- **Timer preemption exists on all three ISAs** (docs/SUBSTRATE.md 15, S3', the
  `preempt` kernel). A cell that issues no syscall can be taken off the CPU, and a
  Linux cell's sibling *context* is preferred over another cell. That closes the
  single-core half of task #27.
- **The frame allocator is SMP-safe, and two cores do real work at the same time**
  (the `smp` kernel, all three ISAs). `frames`' bitmap, reference counts, used counter
  and search hint are one data structure with four fields, and every operation touches
  several of them, so they are now behind a `SpinLock` - unconditionally, not behind
  the `smp` feature, because locking is a property of the data structure rather than of
  a build configuration (the lesson that produced the `SYS_YIELD` FP defect: state
  whose safety depends on which features are enabled gets written twice and diverges).
  On top of that, **every online core drains a shared work queue**: an int8 GEMM's output
  rows are split into blocks and each core claims blocks from a single `fetch_add` cursor
  until it is exhausted, with the result asserted **bit-identical** to a single-core
  oracle. It was two cores for a while, and not by design: the job was `take()`n out of
  its slot by the first secondary to see it, so the phase was primary-plus-one while the
  other cores sat in their idle loops beside undrained blocks. The job is published by
  **generation** now (each core drains a round once) and the primary waits for *the
  queue* - every block accounted to the core that did it - rather than for one
  secondary's completion flag, which it cannot use once the number of participants is
  unknown. Simultaneity is likewise an **N-way barrier** sized from `online_count()`
  rather than the two-way rendezvous, which could only ever witness a pair: 4 of 4 cores
  take a nonzero share on all three ISAs (4/3/6/3, varying run to run), and restoring
  the `take()` makes the assertion fail. Claiming rather than pre-assigning is what makes the split a *result*: with a
  static half-and-half division the faster core finishes early and idles, and the
  per-core counts prove nothing because they were decided in advance. Here they vary run
  to run (8/8, 9/7) and both are asserted nonzero and asserted to sum to the queue - a
  run where either is zero drained serially and would still have produced the right
  answer, which is exactly why correctness alone is not the load-sharing evidence.

  The parallelism is proven by a **rendezvous**, not by timing: each core publishes a
  flag and waits for the other's, and neither writes its flag after passing, so both
  passing means both executed inside one interval - which a single core cannot produce,
  since there is no kernel-context preemption to interleave them and neither side
  yields. A wall-clock speedup would prove nothing under TCG, which time-slices the two
  vCPUs onto host threads; simultaneity is the available evidence and it is what is
  asserted.
- **Cells run in user mode on secondary cores, are placed on whichever core is free,
  and are preempted there** (the `smp` kernel, all three ISAs) - section 10.0. Every
  enumerable secondary is started, two cells are proven running at the unprivileged
  level on two cores at the same instant, a queue of 8 runnable cells is drained by 4
  cores that each claim from it, each core then preempts between the cells it claimed
  (344-405 slices taken on 4 cores at once, against 0 in the cooperative control round),
  and **two unmodified static-glibc binaries run as Linux cells on two cores at the same
  time**, each transcript asserted exactly, and a core that runs dry **rebalances an
  unstarted cell out of a peer's claim**. Not yet the whole scheduler: nothing migrates a
  cell that is already *running*, and the per-CPU EEVDF+BORE queue does not participate
  in placement.

### 10.0 Cells run in user mode on a secondary core - built

**Done on all three ISAs** (the `smp` kernel's fourth phase). Two cells run **in user
mode, on two cores, at the same instant** - each in its own address space, each dropping
to the ISA's unprivileged level and trapping back into its own core's kernel stack. This
is the capability section 10 was missing: bring-up proves a core executes, the GEMM phase
proves two cores compute in kernel context, and this proves a *cell* runs on a core that
is not the boot CPU.

**The witness is a page, not a timing.** Each cell owns two words in one shared page: its
own round counter, and the highest peer counter it has ever seen. It writes only its own
two words and reads only the peer's counter, so there is no lock and no update to lose -
which matters here in a way it does not in `preempt` or `schedidle`, where the shared
order vector is safe precisely because only one CPU is ever executing. A nonzero "highest
peer seen" means this cell read the peer's progress **between two of its own rounds**. On
one CPU under cooperative dispatch the first cell runs to completion before the second is
entered, so it could only ever read 0. Both directions nonzero is therefore not
producible by one core. Dispatch is deliberately left **off** for the phase: this is
about two cores, not about preemption (which `preempt` owns), and a slice firing here
could hand one core's cell to the other core's scheduler.

**What had to become per-CPU, and what did not.** The trap path is already per-core *in
hardware* on two ISAs - RISC-V keeps the current frame in `sscratch` and its vectors in
`stvec`, ARM64 in `TPIDR_EL1` and `VBAR_EL1`, all per-core CSRs/system registers. What
was global:

- **The saved kernel context** each ISA's `return_to_kernel` unwinds to (`KERNEL_CTX`).
  Now one slot per core on all three: indexed by `tp` on RISC-V, by MPIDR affinity 0 on
  ARM64, and reached through `GS_BASE` on x86-64. Reverting RISC-V's to a single slot was
  observed to make the secondary's cell never finish.
- **The portable "which cell is running" state** - `user::CURRENT`, `TOP_CELL` and
  `EXITED` - now `PerCpu<usize>`. With `cpu_index()` a compile-time 0 on the non-`smp`
  build, that substitution changes nothing there.
- **RISC-V's kernel `tp`.** `tp` is a *saved GPR* the cell owns as its TLS pointer **and**
  where the kernel keeps its CPU index, so without saving it the kernel would run every
  trap handler reading the wrong CPU's per-CPU state. The frame gained a `kernel_tp`
  slot, written on the way out to U-mode and reloaded on the way in. On the boot CPU this
  is invisible, because the wrong answer and the right one are both 0 - which is exactly
  why it had to be found by reading the trap path rather than by testing on one core.
  Reverting it was observed to make the secondary's cell never finish.
- **x86-64's four trap-stub words** (`CUR_FRAME`, `KERNEL_RSP`, `USER_RSP_SCRATCH`,
  `KERNEL_CTX`), plus its **GDT, TSS and syscall kernel stack**. All are per-CPU now. The
  four words are reached `GS`-relative, and **there is no `swapgs`**: nothing in this tree
  ever gives a cell a GS base (`arch_prctl(ARCH_SET_GS)` is refused `-EINVAL`, and both
  other ISAs carry TLS elsewhere), so `GS_BASE` points at the core's own block in kernel
  and user mode alike. A cell that did read `%gs:` would fault on a supervisor-only page -
  which is what it already did when the base was 0 and the address was unmapped. This is
  the cheap half of the arrangement section 10.3 anticipated; adopting `swapgs` later is
  two instructions at each ring boundary and nothing else here. Indexing by `cpu_index()`
  was rejected for the reason the earlier plan gave: it reads the LAPIC over MMIO, which
  does not belong in front of every syscall.
- **x86-64's per-core registers that no memory change can substitute for**: IDTR, GDTR,
  TR, the SYSCALL MSRs, CR0/CR4/XCR0 and `GS_BASE`. The AP trampoline set none of them, so
  a secondary had no interrupt handlers at all and its first exception was a triple fault.
  The secondary now loads the *primary's* IDT (the table is shared - the vectors are the
  same code on every core - only the register is its own) and runs `user_init` for itself.

**Safe by partitioning, not by locking.** The two cells hold distinct cell slots,
distinct address spaces, distinct kernel stacks and distinct pages; the only structure
they share is the cell table, which `run` reads and `finish` writes at *disjoint
indices*. That is the multikernel answer this document commits to (SCHEDULING.md 1a)
rather than a shortcut, and it is why no lock appears on this path.

**Then placement: every core is started, and cells are claimed rather than assigned.**
Handing one *named* cell to *the* secondary is a placement decision made by hand, which
is the decision a scheduler is supposed to make. So `smp::start_all` brings up **every**
secondary the firmware enumerates - each on its own stack, indexed by its own hardware id
(hart id / MPIDR affinity / initial APIC id), with a bounded wait per core and a
probe-the-next-id fallback where the firmware enumerates nothing from EL1 (ARM64, the
same synthesis `bring_up_one` already used) - and `smp::place_cells` publishes a set of
runnable cells that **every core claims from whenever it is free**. Nobody is assigned
anything in advance. It is work-conserving (no core idles while the queue is non-empty)
and self-balancing by claim rate rather than by prediction, which is the GEMM
block-queue reasoning applied to cells instead of rows.

The proof puts **more cells than cores** in the queue (8 on 4), with one deliberately
long cell and seven short ones, and asserts three things: every cell finished on some
core and says which cell it was (a distinct exit code - the only thing that ties a
completed run back to a cell when the caller did not choose where it went); more than
one core claimed work and the per-core counts sum to the queue (a run where one core
took everything produces identical exit codes and teaches nothing, which is why
correctness is not the placement evidence); and **some core claimed a second cell**,
which a one-per-core hand-out cannot produce. Observed on all three ISAs with 4 CPUs
online: the core that took the long cell takes exactly 1 and the rest take 2-3 - the
ratio is reported, never asserted, because TCG time-slices the vCPUs onto host threads
and the split is a property of that scheduling, not of ours.

**One cell, one core, checked.** The kernel records which cell each CPU is inside and
refuses a second entry (`user::double_entries`, asserted by the preemption phase). This
is the invariant every other multi-core claim rests on, and it is instrumented rather
than inferred from the absence of a crash: every defect on this path has surfaced as
corruption somewhere else entirely - a core executing a data symbol, an instruction
fetch at 0 - which says nothing about where the second entry happened.

**And a claim no longer runs to completion: every core preempts its own cells.** A core
claims a *batch* (`smp::CLAIM_BATCH` = 2 - one cell has nothing to preempt *to*) and runs
it under **its own** preemption timer. Everything that needs is per-core hardware no
bring-up trampoline sets, so each core brings up its own
(`arch::enable_timer_irq_this_cpu`): the RISC-V `stimecmp`/`sie` CSRs; on ARM64 this
core's GICv3 **redistributor** (the frame is per core at `GICR_BASE + aff0 * 0x20000`,
and the old global "GIC is up" flag covered the CPU interface too, so a secondary would
have had none at all) and its CNTV PPI; on x86-64 this core's `IA32_APIC_BASE` enable,
TPR and **SVR software-enable** plus the LAPIC timer registers - while the *discovery*
half (the APIC mode probe, the IDT gate, the one-shot self-test) stays global work the
primary does once, because four cores racing through it wrote one shared IDT and printed
four interleaved copies of the probe's own line.

The cells issue **no syscall at all** until they exit, so the evidence is unambiguous:
under cooperative scheduling the number of preemptions taken is exactly zero - there is
no other moment at which the CPU could change hands - and the cooperative placement
round immediately above is asserted to be exactly that. With slices armed, **344-405 of
~700-820 slices take the CPU on 4 cores at once** on all three ISAs.

Two shared-state fixes had to land with it. The `preempt` and `dispatch` counters were
`static mut` `+= 1`, which is a lost update rather than a race that "cannot interleave"
once every core dispatches; they are relaxed atomics now (docs/SMP.md 10.2's rule: a
lock, a partition, or an atomic - never nothing). And the native scheduler's
`schedulable` predicate gained the affinity test: a cell belongs to one core
(`user::claim_cell` / `cell_on_this_cpu`), so no other core's pick can see it. An
unclaimed cell is visible to everyone, which is exactly the single-CPU behaviour, so a
boot that never claims anything is unchanged.

**A third instance of the SYSRET-provenance defect surfaced here, and it is why the rule
is now stated once.** `enter_user_first` resumed through `sysret_resume`, which takes RIP
from RCX and RFLAGS from R11. That was invisible while the only frames it saw were freshly
built or last stopped at a syscall. A core running two cells re-enters the survivor
through `enter_user_first` after the other exits - and that frame was captured by a timer
interrupt, with live RCX/R11. The symptom was not a fault: all four cores resumed their
second cell with two corrupted registers and its bounded loop stopped terminating. It was
found by *reading* the resume path after the instrumentation localised the hang to
"re-entering the second cell of a batch", not by guessing at the symptom. `enter_user_first`
resumes via `iret_resume` now, and the rule is: **SYSRET is only ever for returning from
the syscall it was entered by** - the syscall fast path keeps it; nothing else may.

**And an unmodified Linux binary runs as a cell on a secondary.** Every cell run off the
boot CPU above is **native** - the tree's own ABI, one context, no fd table, no VMA list,
no signal state. The Linux personality keeps far more per-cell state and a few genuinely
global registries beside it, and 10.2 names auditing those as the gate for running Linux
cells on several cores. This takes the part that does **not** need that audit: *one*
Linux cell, on one core, at a time - the global registries have exactly one writer, so
the question the audit exists to answer is not being asked. The narrower question, which
was genuinely unknown, is whether the Linux syscall path works at all off the boot CPU.

It does. `chello` - the same **unmodified static-glibc C binary** `linuxrun` asserts on
the primary - runs as a `Personality::Linux` cell on a secondary with its **exact stdout
and exit code asserted**, while a native cell runs on the primary and the two are held to
have overlapped by the same rendezvous the two-cells phase uses. Asserting the exact
transcript is what makes it a claim about the personality rather than about a stub:
glibc's startup runs `arch_prctl`/`set_tid_address`/`brk`/`readlink` and a demand-paged
image before it reaches `main`.

It needed one more per-core register set, found the way the others were: **RISC-V's
`sstatus.SUM`** (plus `FS` and `scounteren`), which `paging_kernel_init` set once on the
primary. Without it a secondary runs cells perfectly until the kernel first *touches* one
of their pages - the first `uaccess` copy in a syscall - and then takes a store page fault
at a kernel PC on a correctly-mapped user page. That is now
`arch::user_mode_init_this_cpu`, called from `smp::secondary_run`, and it is deliberately
an empty function on ARM64 and x86-64 (their equivalents are `CPACR_EL1`, adopted by the
PSCI entry, and CR0/CR4/XCR0, programmed per core by `user_init`) so the portable caller
does not have to know which ISAs need it.

The `start_all` change also exposed a latent race in the single-cell hand-off:
`run_cells_on_both` published a cell index with a plain load-then-store, which two
secondaries could both read before either cleared it - one cell, two cores, one trap
frame. With one secondary the two were equivalent; with three it presented as two cores
faulting at PC 0 at the same instant, intermittently. It is an atomic `swap` now, the same
exclusivity the placement queue's `fetch_add` already had.

**The personality lock is in place; contention is not proven.** Running Linux cells on
*several* cores at once needs the personality's global tables protected, and 10.2 names
the first step as a **big kernel lock, finer locks proven in later**. That lock exists
now (`linux::plock`): one lock over the whole Linux dispatch plus the demand-paging
entry, **recursive per CPU** (a syscall reaches `fill_fault` through `uaccess`, so the
second entry happens inside the first on the same core - a non-reentrant lock
self-deadlocks there, which is what the first version did), and **not taken at all**
while only one CPU is online, so every pre-existing kernel's hot path is byte-for-byte
what it was. Coarse on purpose: there is exactly one place a Linux syscall enters the
personality, so "every global it touches is protected" is a property of one line rather
than of a list a new registry can be added to without noticing.

**And two Linux cells now run on two cores at the same time.** The same unmodified
static-glibc binary runs as *both* cells, each transcript captured separately and
asserted exactly, each exiting 9 - on all three ISAs. That needed the stdout tap to
become per cell (it keys on `user::current_index()`, which is `PerCpu`, so each core
writes only the slot of the cell it is running; with one shared buffer the two
transcripts interleave and a test that cannot tell them apart cannot show that both ran
correctly).

Getting there cost two defects, and the first is worth recording because the initial
diagnosis was wrong. The phase failed, and the failure looked like it reproduced when
the two cells were run one after the other on a single core - so it was written up as a
personality-state bug. Reproducing it in a kernel with **no secondaries** (`linuxrun`)
showed two Linux cells install and run serially without a murmur. The garbled console
that made the single-core run *look* broken was coming from the secondaries, and the
real faults were two:

- **`place_cells` reported a round finished while cores were still in it.**
  `PLACE_DONE` says every cell finished; it does not say every core has stopped
  *touching* them. The core that completed the last cell is still unwinding its address
  space, and the caller - which returns the instant the count is reached - goes on to
  `user::reset()` the cell table out from under it. A `BUSY` count with an RAII guard
  now quiesces every core before the round is reported done.
- **The Linux scheduler had no CPU-affinity test.** `nproc::schedulable` got one when
  native cells started running on secondaries; `linux::proc`'s three runnable
  predicates did not. So when the primary's Linux cell exited, its reschedule could
  pick the cell the *secondary* was running - two cores in one cell, one trap frame,
  one kernel stack. It presented as an instruction fetch at PC 0 in kernel mode on two
  cores at once. All three predicates now test `user::cell_on_this_cpu`, which is
  constant-true on every single-core boot.

The lesson is the one this tree keeps relearning: the first reproduction was in an
environment that added its own noise, and the "single core reproduces it" conclusion
drawn from it sent the diagnosis in the wrong direction. Reproducing in the *quietest*
environment that can host the bug came first the second time, and answered it in one
run.

**And a core that runs dry rebalances work out of a peer's claim.** Claiming divides
work by arrival, and once divided it stays divided: a core that drew a long cell *and* a
short one finishes late while another idles. So a core whose cursor is exhausted looks
for a cell some peer has **claimed but not started** and takes it. The protocol is one
exchange - exactly one core can turn a slot's run-mark from 0 to 1, and only that core
may enter the cell; the previous owner discovers the loss when it reaches the slot and
finds the mark set. No message, no lock, and no window in which two cores could both
enter. With one deliberately long cell among short ones the steal is **asserted**, not
hoped for: a round in which it did not happen produces the same exit codes and teaches
nothing. Observed on all three ISAs: 1 cell rebalanced, the busiest core taking 3 of 8.

A cell that is already **running** is not stealable. That is the remaining gap, and it
was **attempted and rejected** rather than deferred untried - the record is below,
because two wrong fixes are more useful to the next attempt than the absence of one.

#### Rejected: migrating a running cell (attempted, reverted)

The mechanism is small and most of it worked. A cell's whole context is already per
*cell* rather than per core - its `TrapFrame`, its FP save area and its kernel stack all
travel with it - and the only per-core piece, the saved kernel context a run unwinds
into, is left behind because the losing core unwinds normally and the gaining core
enters afresh. So the design was: a dry core sets a request flag; the owner sees it at
its next preemption point (where the cell's state is already saved and the CPU is in
ordinary kernel context), unwinds its `run` with the cell released instead of finished;
the asking core resumes it with `user::run`, which restores a frame captured anywhere -
the property `enter_user_first` gained when it stopped using SYSRET. It needed one new
unwind reason, added as `user::run_or_migrate` returning `Option<Outcome>` rather than a
third `Outcome` variant, because `Outcome` is matched exhaustively at ~140 sites that
all mean "the cell is finished".

It worked, and then failed roughly two runs in five with a core executing a data symbol.
It was attempted **twice**, and the second attempt followed the first one's own advice:
instrument the invariant rather than reason about the symptom. That was the right move
and it is the part worth keeping. Four findings, in the order they were established:

1. **Publish the release after the unwind, not inside the trap.** Real: the releasing
   core is still on the cell's kernel stack while it is inside `migrate_out`. Necessary,
   not sufficient.
2. **Hand the cell straight to the named requester rather than clearing its owner.**
   Also real - an unowned cell is one every core's scheduler will pick, so clearing it
   opens a window for a *third* core. Also not sufficient. At this point the reasoning
   had been wrong twice, so the attempt was reverted with a note to instrument next.
3. **The instrumentation has to be per CPU, not per cell.** A first guard counted
   entries per *cell* and produced false positives immediately, because under preemption
   a batch sibling exits without ever passing through `run_inner`, so the exit
   decrements a counter the entry never incremented. Asking "does another CPU already
   report this cell" is immune to that: it names two cores or it names none. With that,
   the failure stopped being a corrupted stack somewhere downstream and became
   `cell 0 entered by CPU 0 while CPU 1 is already inside it`, reproducibly.
4. **The request must be published whole.** The cell and the destination core were two
   statics - the cell set by a compare-exchange, the destination stored straight after -
   so between them the owner could see the request and hand the cell to whatever
   destination the *previous* request had left. Packing both into one word fixed that
   ordering hole. It reduced the failure rate and did not eliminate it.

So a fifth thing remains. The honest state is unchanged: **an intermittently faulting
kernel must not land**, and this needs a designed hand-off protocol rather than another
patch. What is kept is finding 3 - the per-CPU entry guard is now permanent
(`user::double_entries`), asserted by the preemption phase, and costs one store per cell
dispatch. It is the tool the next attempt needs, already in place and already proven to
turn this class of defect from downstream corruption into a named pair of cores.

**Honest scope.** Preemption is *within* a core's own claim, and rebalancing moves only
**unstarted** cells. Nothing migrates a *running* cell (attempted; see above), and there
is no priority across
cores - the per-CPU EEVDF+BORE queue orders each core's own cells and does not
participate in the placement decision. Two Linux cells are proven; *many* is not, and neither
is a Linux cell that forks, pipes or signals across cores - those reach the global
registries in patterns the two-`chello` case does not exercise, and the big lock is
correct for them by construction but unproven. What makes all of it safe is unchanged
and is the reason it could land first: a claimed cell is still a *partitioned* cell (one
core, one slot, one address space, one kernel stack), the claim simply being made at run
time instead of by hand.

### 10.0a One cell on two cores - vcores - built

> The vcore work below landed in slices, and five defects came out of it (this section
> records four of them; §10.2a records the fifth). They share one cause: an execution
> context has three representations in this kernel and the agreement between them is
> maintained by hand. **docs/EXECUTION-MODEL.md** is the top-down design that removes the
> cause - one entity table, the dependency graph drawn, the invariants stated, the use
> cases simulated, and a host fuzzer over the state machine. Read it before extending
> anything here.

Everything in 10.0 runs **different cells** on different cores. That is real
parallelism, and it is not the parallelism a *program* has: a Node worker, a strand
pool, a FlashAttention 3 producer/consumer pair are all one address space that wants
several cores. A cell belonged to one core, so the answer to "can my program use the
machine" was **no**, however many cores were online.

The reason a cell was one core is stated in `claim_cell`'s own doc: two cores in one cell
would share its trap frame, its kernel stack and its FP/SIMD save area, none of which is
locked. So the fix is not a lock. It is to make those three per **vcore** and move the
ownership claim down with them, at which point the vcore is the unit that is partitioned
- exactly as the cell was - and the multikernel argument holds one level lower, unchanged
(docs/SCHEDULING.md 1a, docs/SUBSTRATE.md pillar 3).

**The mechanism** (`kernel/src/user.rs`, no new kernel object and no new verb - a vcore is
an execution context of the Cell object, the same shape the Linux personality's contexts
already are):

- `RunCell` holds `vframe`/`voutcome`/`vcpu` arrays of `MAX_VCORES` (4) instead of three
  scalars, plus `nvcores`. Slot 0 is the context `install` builds, so a cell nobody adds
  a vcore to holds `nvcores == 1` and every pre-vcore path is what it was.
- `CELL_FP` is one area per `(cell, vcore)` rather than per cell, and `FP_SWAPS` became a
  relaxed atomic - two cores now restore FP for two vcores at the same instant, and a
  `static mut +=` loses counts (the fix the preempt/dispatch counters already took).
- `CUR_VCORE`/`EXITED_VCORE` are `PerCpu`, for the reason `CURRENT` is: two cores inside
  two vcores of one cell are inside the *same* cell index, so the cell alone cannot say
  which frame or which FP area a trap belongs to.
- The double-entry guard keys on `cell * MAX_VCORES + vcore`. Two cores in one *cell* is
  now the point; two cores in one *vcore* is still the corruption it catches.
- `install_vcore(idx, frame)` is a **launcher** verb, not a syscall: creating a context is
  creating something the scheduler must own, and the cell-facing spelling of it is the
  strand runtime asking for a vcore, which belongs with the rest of the process model.
  The kernel mechanism is proven before anything is exposed, as every capability here was.
- `smp::place_vcores` publishes **vcore ids** into the same queue `place_cells` uses, so
  there is one drain loop, one claim protocol, one steal protocol - a cell index is simply
  the vcore id of its vcore 0 (compose before extending).

**A vcore yields to its sibling** - the named next step, now built. A cell with more
vcores than cores is the ordinary case the moment a program asks for eight workers on four
cores, and the first version of this refused it: the cooperative schedulers pick a *cell*
and enter its vcore 0, so `cell_on_this_cpu` refused multi-vcore cells outright rather than
enter a context another core owned. The fix is the predicate one level down:

- `user::vcore_on_this_cpu(cell, v)` answers per vcore - a vcore belongs to one core, an
  unclaimed one is enterable by any, and two cores in two *different* vcores of one cell is
  the point rather than a hazard. `cell_on_this_cpu` reverts to asking about vcore 0, which
  is the right question for a path that enters vcore 0, multi-vcore cell or not.
- `SYS_YIELD` tries a **sibling vcore of the same cell first** (`nproc::next_sibling_vcore`,
  round-robin from the running one so N vcores share a core evenly), for the reason the
  Linux preemption path tries a sibling context first: it is the cheaper move by a wide
  margin. `user::switch_native_vcore` is the whole of it - one address space, so **no
  `activate()` and no TLB consequence**; only the FP/SIMD register file and the frame change
  hands. That is the economy of a vcore over a second cell, stated as code.
- `switch_native_cell` now saves the vcore this CPU is actually *inside* and loads the
  target's vcore 0, rather than assuming 0 on both sides - with `from` fixed at 0, a core
  inside vcore 1 would write vcore 1's live registers into vcore 0's saved image.
- The per-CPU entry guard gained a second caller and one owner: `user::enter_vcore` is
  reached from `run_inner` and from `switch_native_vcore`, since the intra-cell switch is a
  new way to become "inside" a vcore and a new path is where a guard gets forgotten.
  **Not** from `switch_native_cell`: marking there was tried and produces a *false*
  double-entry during placement, because `INSIDE` is written on entry and cleared on return
  by `run_inner` while a cross-cell chain sits inside that bracket, and a batch sibling can
  exit without passing back through it (the looseness 10.0 already records).

**The proof** (the `smp` kernel's vcore-yield phase, all three ISAs) is deliberately
**single-core**: both vcores are left unclaimed and run on the primary, which is what makes
the oracle exact. Each round of each vcore is one append to a shared order vector then one
yield, so two vcores must produce a strictly **alternating** 12-marker vector - an
alternation only a yield that reaches the sibling context can produce, not one that comes
back to the caller (a run of one marker) and not one that goes to another cell (there is
none). Reverting the sibling path gives **6 markers, not 12** - vcore 0 ran alone -
observed.

The **owner check** is proven by the two-core phase, which is why its witness program now
issues a `SYS_YIELD` per round rather than a bare counter read: with the two vcores owned by
different cores, every round asks "is a sibling enterable" and the answer must be no.
Replacing `vcore_on_this_cpu` with a bare bounds test makes the entry guard fire by name -
`cell 0 vcore 1 entered by CPU 2 while CPU 1 is already inside it` - observed.

**The last vcore out ends the cell** - the rule that phase used to assert was *missing*.
The first vcore to exit unwound the run, because `finish` records an outcome and returns the
null frame the trampoline reads as "unwind"; correct for a cell, and unusable for a cell with
four vcores, where the first one finishing would kill the other three. `SYS_EXIT` now ends the
calling **vcore** and `SYS_EXIT_GROUP` ends the cell - the process/thread split the Linux
personality already has one level up. A vcore's outcome *is* its liveness
(`user::vcore_live` = `voutcome[v].is_none()`), so every pick already asks; `all_parked` counts
only live vcores, since an exited one is neither parked nor runnable and counting it as
unparked would leave the cell `Runnable` with nothing to enter.

`nproc::retire_vcore` hands the CPU to a sibling **this core may enter** - live, owned here,
and either runnable or parked on a waitable source, the same two-part rule `can_reschedule`
uses - and returns `None` otherwise, which unwinds with this vcore's own outcome. That
condition is the whole correctness of it, and both halves were found by the tests: an
unconditional `reschedule` returns `DEADLOCK_EXIT` when every live sibling belongs to another
core (the two-cores-one-cell phase), because a core with nothing to do is not a deadlocked
machine; and without `ensure_tracked` the hand-off finds no `Proc` entry at all for a cell that
has never spawned or blocked, and ends the run in `DEADLOCK_EXIT` too (the yield phase).

The yield phase now asserts the rule directly: **both** vcores reach their exit, and the run is
ended by vcore **1** - the last one out, not the first. Suppressing the hand-off makes vcore 1
never reach its exit, observed. `SYS_EXIT_GROUP` takes the pre-existing path unchanged, so
nothing new is claimed for it.

**A vcore blocks** - the next rung after that, also built. `nproc`'s block state was per
*cell*, so one context parking on a timer recorded the wait for all of them: a cell with a
runnable sibling looked blocked and the scheduler idled the machine with work available. It
is the defect the Linux side already fixed one level up with per-context `pblock`
(docs/LINUX-COMPAT.md), and the fix here mirrors the existing two-phase shape rather than
inventing one:

- `Proc` carries `vblock`/`vparked`/`vwait` arrays instead of `block`/`wait_for`. `vparked`
  is the per-vcore analogue of `state == Blocked` and is kept separate from `vblock` for the
  same reason the cell-level pair is: `wake_satisfiable` clears the parked flag while
  `complete_block` clears the block later, with the woken context's address space active.
- The cell-level `PState::Blocked` now means **every** vcore is parked. For a single-vcore
  cell, parking its one vcore parks them all, so the transitions are what they were - which
  is why all 66 kernels stayed green through the change.
- `satisfiable`, `sources_of`, `block_name` and `complete_block` take a vcore.
  `refresh_deadlines` and `blocked_sources` iterate **parked `(cell, vcore)` pairs** rather
  than gating on the cell's `Blocked`, because a cell with one parked vcore and one runnable
  is not blocked and its parked context's deadline still has to be armed - the arming is
  what wakes it.
- `reschedule` picks a `(cell, vcore)`: a cell that parked one context and left a sibling
  runnable is re-entered at the sibling, and a cell whose parked vcore was just woken is
  re-entered at *that* vcore, which is what completes its syscall in its own address space.
  `switch_native_cell_vcore` exists for exactly that.
- `can_reschedule` counts a runnable **sibling vcore** - or one parked on a waitable source,
  the same two-part rule the cell branch uses. Without it a single multi-vcore cell falls
  through to "is another *cell* runnable", takes the in-trap wait, and its sibling never
  runs: the block would be per cell in effect even with the state per vcore.

**The proof** is the `schedidle` oracle one level down, on all three ISAs: the same
`user_blocker` and `user_peer` programs, now as two **vcores of one cell**, produce the same
exact order vector **`bSSSSSSSSB`** - the blocker parks on a 20 ms deadline, the sibling
takes all 8 of its rounds strictly between the two blocker markers, and the arbiter's
one-shot wakes the blocker. Restoring the per-cell park (`state = Blocked` unconditionally)
makes the sibling run **zero** rounds, observed.

That phase also **found a real defect in the yield built one commit earlier**:
`next_sibling_vcore` checked ownership but not *parked*, which was invisible while a vcore
could not block. A yield then entered a sibling parked mid-`SYS_ARM_TIMER`, resuming it at
its syscall return with the return register still holding the syscall **number** - no fault
and no log, just a wrong answer from a wait that had not finished (`SYS_ARM_TIMER returned
47, want 0`). And a second, in the placement path: `drain_cells` stamped per-vcore ownership
for a whole batch *before* winning any run-mark, so a core holding two vcores of one cell
could enter the sibling from inside the first while the stealer that took it was already
there. Ownership is stamped where the run-mark is won now - the reasoning `count_claim`
beside it already carried - and the per-CPU entry guard named the pair rather than letting it
corrupt downstream.

**A queue pair per vcore** (docs/SUBSTRATE.md S5) - the third rung, unblocked by the
second. A submission queue is **single-producer**: two contexts sharing one must serialise
their submissions, and once those contexts run on two cores that serialisation is a
cross-core write to shared ring indices - the cost the io_uring-per-thread shape exists to
avoid. So `RunCell` holds `vqp`/`vqp_va`/`vqp_cap` arrays, `SYS_DOORBELL` drains the ring of
the **calling** vcore, `SYS_QUEUE_INFO` reports the calling vcore's own region and
capability, and `install_vcore` takes the new context's ring alongside its frame. Slot 0 is
what `install` was handed, so a single-vcore cell is unchanged.

The cell-facing shape is the point: a context does not have to be *told* which ring is its
own. It asks, and the answer is per vcore - so the same binary in two contexts binds two
different regions with no code in it that knows vcores exist.

**The proof** (the `smp` kernel's per-vcore-queue phase, all three ISAs) makes two separate
claims. The rings are **disjoint**: each vcore reports a different region VA and a different
capability id, each matching what its launcher initialised - a per-cell ring reports one VA
twice, and reverting `SYS_QUEUE_INFO` to `vqp_va[0]`/`vqp_cap[0]` fails on the capability
(observed). And each ring **completed its own round trip on its own core**: both vcores go
into the placement queue, each submits an `OP_ECHO`, rings its own doorbell and reaps
`STATUS_OK`, and the two are asserted to have run on different CPUs - reverting the doorbell
to `vqp[0]` leaves vcore 1's round trip uncompleted (observed). Together those say a
submission never left its core: there was no shared ring for it to cross into.

Honest: the ring **overlay** a cell submits through still comes from its launcher, because
building one over a region is `QueuePair::attach`, which lives in kernel `.text` that a cell
has no mapping for - so `SYS_QUEUE_INFO` proves per-vcore *reporting* while the round trip
proves per-vcore *servicing*, and the two are asserted separately rather than conflated. And
`load::map_queue` still places one ring at `USER_QUEUE_VA`: a **loaded** cell asking for a
second vcore needs `USER_QUEUE_VA + v * REGION_SIZE`, which is one line and is deliberately
not written until a loaded cell asks, since nothing would test it.

**The userspace allocator is multi-core safe** - the prerequisite the userspace half sits on,
and a genuine latent defect rather than a feature. `runtime::Heap` is the `#[global_allocator]`
every `alloc`-using cell and test kernel binds, and it carried a bare `unsafe impl Sync for
Heap` justified by *"single-CPU kernel; no concurrent access to the allocator"*. That was true
when it was written and false from the moment two cores ran cells - an inherited claim, which
is the kind that has to be re-checked rather than trusted (docs/ENGINEERING.md 1). A free list
is the worst case for getting it wrong: every operation reads and writes several links, so two
concurrent allocations hand out **overlapping blocks**, and the symptom is not a fault but two
owners of one buffer writing over each other.

It is behind `runtime::lock::TicketLock` now - the lock whose own doc said it was "for the
future multi-vcore case" - **unconditionally**, because whether a structure needs a lock is a
property of the structure and not of which cargo features are enabled, the call `mm::frames`
and the NVMe driver already made. An uncontended acquire is two atomics against a free-list
walk that is already several dependent loads.

Proven by the `smp` kernel's shared-heap phase on all three ISAs: each core runs 512
allocate / stamp-every-byte / read-every-byte-back / free cycles through the global allocator,
meeting at a rendezvous first so the overlap is real. A block handed to both cores means one
core's marker lands in the other's block between the write and the read, which the read-back
catches **directly** - the corruption itself, not a proxy for it. Asserted: 0 mismatched bytes,
0 pointers outside the heap region, both cores completing all their rounds, and the free list
still serving afterwards. Reverting to the unlocked `UnsafeCell` + `unsafe impl Sync` produces
a **general protection fault inside the allocator** (x86-64 vector 13) - a harder failure than
the mismatch counter was built to report, which is the same defect in its unsurvivable form.

**Still not done** and named: a vcore that forks or takes a signal, the loaded-cell ring
placement just named, and a cell that **asks for** its own vcores. A loaded cell now *runs* the
multi-vcore executor: `librheo-vcore`, one ELF in two contexts with its own ring
(`load::map_queue_for`) and stack (`load::map_vcore_stack`) each, entered at the same ELF entry
and told apart only by `SYS_VCORE_INFO`, with vcore 0 filling the injector and vcore 1 running
all 32 strands (docs/CONCURRENCY.md). What is left is that the *launcher* installs the vcores -
the same launcher-mints-authority shape as the queue pair and the W^X exception - and a
cell-facing `spawn_vcore` is a separate design question.

**The proof** (the `smp` kernel's two-vcore phase, all three ISAs): two vcores of **one**
cell go into the placement queue and whichever cores are free claim them. Both are
asserted to exit 0, to complete all 64 rounds, to have run on **different CPUs** - without
which the phase would pass with them run back to back on one core, since the exit codes
and round counts are identical either way - and to have **each seen the other advance
mid-run**, using 10.0's witness for 10.0's reason: a nonzero "highest peer counter seen"
means this vcore read the peer's progress between two of its own rounds, which one CPU
cannot produce, because there is no kernel-context preemption to interleave them and
neither vcore yields. Observed: vcore 0 on CPU 1, vcore 1 on CPU 2 (and every other
pairing across runs), each seeing the other in the 50-64 range.

**Honest scope - two pieces proven, two required but not detectable here.** Reverting
`vframe[v]` to `vframe[0]` makes vcore 1 never finish, and reverting `voutcome[v]` to
`voutcome[0]` panics on the missing outcome; both observed. The other two are construction
requirements this phase cannot see, which is worth recording rather than glossing:

- A **per-vcore kernel stack** is required because ARM64 and RISC-V load the trap stack
  out of the frame (`ld sp, TF_KSP(sp)`). Giving both vcores one stack was tried and
  **passes on all three ISAs**, even after `user_copair` was extended to trap every round
  precisely to make the two cores' use of it overlap: both cores run the *same* short
  handler, so each overwrites the other's saved return address and spills with identical
  bytes. Detecting it needs a handler whose stack contents differ per core.
- A **per-vcore FP save area** matters when a vcore is stopped and resumed. Each vcore
  here is entered once and exits once, on its own core with its own register file, so
  nothing reloads a saved image and sharing one area would be invisible. The path that
  exposes it is preemption of a multi-vcore cell, which is not built, so that proof arrives
  with that capability, not before. (The *yield* path does swap it, and correctly, but
  within one core there is no second register file for a wrong image to come from.)

Also honest: `MAX_VCORES` is 4 because the FP areas are a fixed static
(`MAX_CELLS * MAX_VCORES`, 4 KiB each on x86-64 = 256 KiB of `.bss`); funding them out of
the owning cell's budget through `mm::kmeta` - the mechanism S1' already built - is what
removes the number. Two vcores are proven, and so is a yield between them; a vcore that
blocks, forks or takes a signal is not, and per-vcore queue pairs (docs/SUBSTRATE.md S5)
are the next rung now that there is something to give them to.

### 10.0b Many Linux cells - the 10.2 audit's own question, asked

10.2 makes an audit the gate on feeding secondaries real work, and names the Linux
personality's global state as one of its six areas: the mapped-file registry, the
pipe/eventfd/timerfd registries, pid allocation, the unix-socket names. `linux::plock`
covers the whole Linux dispatch plus the demand-paging entry, recursively per CPU, so the
discipline in place is "one big lock" - the seL4 order 10.2 explicitly allows.

Two Linux cells were proven above. The remaining question was **many**, because a big lock
is exactly the kind of claim that holds for two and fails for N if anything touches a
global outside the locked window. It is now asked: **four** Linux cells go through the
same placement queue every other multi-core phase uses, one per core, each demand-paging
its own copy of the same unmodified static-glibc binary, each synthesizing its own pid,
each transcript captured separately and asserted **exactly**, all four exiting 9. It
passes on all three ISAs with all 4 cores taking one.

That widens `place_cells`' documented contract from "native" to "native, **or a Linux cell
with no process tree**": such a cell's exit reaches `linux::proc`, which with no children
ends the run exactly as a native cell's does. A Linux cell that **forks, pipes or signals
across cores** is a different question and is still not asked.

**This phase alone does not prove the lock is load-bearing** - tested, not assumed. Forcing
`plock` to return `PGuard::Off`, so nothing serialises the personality at all, and it still
passes on all three ISAs: `chello` is a hello-world that barely touches the global
registries, and TCG interleaves coarsely. So its own claim is exactly "N Linux cells across
N cores produce N correct transcripts". **The fixture that closes it now exists** - see
10.0d.

All three Linux multi-core phases live in their own **`linuxsmp`** kernel rather than in
`smp`, for a measured reason: each runs several static-glibc images through a full glibc
startup with demand paging, and adding them to `smp` pushed **riscv64** past the 120 s
boot-test budget - observed, timing out inside the four-cell phase before the other two
ran. One kernel per concern is the tree's shape anyway; this is what forced it.

### 10.0c A Linux cell that forks off the boot CPU

The other half of 10.2's Linux question. `fork` creates a **new cell**, and a cell nobody
has claimed is pickable by every core - `user::cell_on_this_cpu` treats `NO_CPU` as
pickable, which is exactly what keeps single-core boots unchanged. So when a Linux cell on
core B exits, its `linux::proc::reschedule` scans for a runnable cell and would find the
child core A forked a moment ago: two cores, one cell, one trap frame.

An **idle** core cannot reach it - `drain_cells` only enters cells the caller published, and
a forked child is not in the queue - so it takes **two** Linux cells, one of which forks.
That is why this is its own phase.

The fix is the one `cell_on_this_cpu`'s own doc predicted while no boot reached this state:
`install_forked` and `install_spawned` give the child **its parent's owner**. Not a wider
lock - the same partitioning discipline, applied to a cell that did not exist when the round
started. The stale "honest limitation" note in that predicate is replaced by what is now
true.

**Proven** (the `smp` kernel, all three ISAs): `af_unix` - an unmodified static-glibc
`socketpair` + `fork` + bind/listen/connect/accept fixture, so it drives the global
unix-socket registry and the L6 cross-cell ring from a secondary - runs on one core while
`chello` runs on another, both exact transcripts asserted, both exit codes asserted, zero
double entries; and `user::affinity_skips()` is asserted **nonzero**, so a scheduler really
was offered a cell belonging to another core and declined it. That last assertion is the
point of adding the counter: an absence is weak evidence, and "no double entry" would pass
equally if the race never arose.

**Not proven**, and recorded rather than dressed up: that the *child's* inherited owner is
what prevented a double entry. Reverting the fork path to leave the child unclaimed still
passes - five runs - and the refusals counted come from the two *placed* cells rather than
from the child. The window is narrow: the peer's exit-time scan has to land between the
child's creation and its reaping. So the inheritance closes a real window that this phase
cannot make happen on demand.

### 10.0d The registries under load - what makes `plock` testable

10.0b and 10.0c both pass with the personality lock removed, and said so. The reason is the
workload: `chello` and `af_unix` start glibc, do a little, and exit, so they barely touch the
global tables. A lock that is never contended is indistinguishable from no lock.

`tests/linux-fixtures/regstress.c` is the missing workload. It aims at the two registries
whose allocators are **find-a-free-slot-then-claim-it** (`linux/pipe.rs`, `linux/eventfd.rs`)
- a shape that races directly, because two cores can both find the same free index and both
claim it, leaving two processes holding one object. The detectable consequence is not a fault
but *someone else's bytes*, so every value it writes is derived from its own pid and every
read is checked against it: 256 rounds of pipe create/write/read/close and eventfd
create/write/read/close, one line of output (`regstress OK`, or `regstress FAIL <n>`).

**Proven** (`linuxsmp`, all three ISAs): two of these run on two different cores at once -
asserted to be different cores, so they really are inside the allocators together - and each
reads back exactly what it wrote, with its own exact transcript and exit 0.

**And the control fires.** With `plock` forced to `PGuard::Off`, a cell prints
`regstress FAIL 5` - five rounds in which it read a byte the *other* cell had written. That
is the corruption named at the top of this section, produced on demand, and it is what makes
`linux::plock` a proven mechanism rather than a present one.

What that licenses, and what it does not: the personality's global registries are now shown
serialised under genuine concurrent load. It is still one big lock, so it serialises the
whole dispatch rather than only the tables - the finer per-cell locking 10.2 describes, which
is what threads of *one* Linux cell on several cores would need, is not built.

### 10.0e The Node/Bun/Claude Code load path, off the boot CPU

The three phases above run static-glibc binaries out of the kernel image. The real workloads
do none of that: `node`, `bun` and `claude` stream off a live ext4 disk, their `ld.so` maps
`libc.so.6` and friends with file-backed `mmap`, and every page arrives by fault. So "can
those run on a secondary" is really a question about **that load path**, and it can be asked
with `dhello` - the same 20 KB dynamic hello `linuxdyn` proves on the primary - for a fraction
of their size and time.

**Proven** (`linuxsmp`, all three ISAs): `dhello` is loaded off the live `dyn-disk.img` and run
as a Linux cell **on a secondary**, overlapping a static `chello` on the primary, with its
exact transcript and exit 12 asserted and ~576 block-cache fills *during the run* - so its
interpreter and libc really came off the device, on demand, from that core. Exercised off the
boot CPU: the virtio-blk driver, the bounded block cache, `ext4plus` path resolution,
`PT_INTERP` + the ELF interpreter, file-backed `MAP_PRIVATE`/`MAP_FIXED`, and demand paging -
with the faults taken on a secondary's trap path using that core's own kernel stack.

It uses `run_cells_on_both`, which hands a **named** cell to a secondary, rather than
`place_cells`, where which core takes which is a race. That distinction was not cosmetic: the
first version used `place_cells` and asserted only that the two cells landed on *different*
cores, and the run put the dynamic cell on the **primary** - the assertion passed while the
claim in its own message was false. "The dynamic cell ran off the boot CPU" has to be the
deterministic form.

**And the real Bun runs there too** - `linuxbunsmp`, the same binary, disk, JIT authority
and preemptive dispatch as `linuxbun`, held to the same strict gate: it streams off the live
ext4 disk (~9,200 block-cache fills), brings up JavaScriptCore with its JIT behind the W^X
exception, takes **83 preemption slices on that secondary**, evaluates
`console.log("rheo:"+(40+2))`, prints exactly `rheo:42` and exits 0. x86-64 only, as
`linuxbun` - there is no arm64/riscv64 bun build here, so those ISAs skip with a reason.

**The prediction written down before running it was wrong, and instructively.** It said this
would need *threads of one Linux cell across cores* - the finer per-cell locking 10.2
describes - because Bun spawns a worker. It does not: Bun's contexts are scheduled
cooperatively *within* whichever core runs the cell, exactly as on the primary, so nothing
about them has to change for the cell to sit on a secondary. Parallel execution of those
contexts is a separate capability and this needs none of it. Worth recording because the
wrong version was a plausible story, and the cheap experiment settled it (ENGINEERING.md 1).

Two things the first run got wrong, both found by reading its own output rather than by
reasoning:

- `run_cell_on_secondary` reused `RV_TIMEOUT_NS` (2 s) to wait for the cell. That bound
  answers "did a secondary arrive"; waiting for a *program* is a different magnitude, and
  Bun had already brought JSC up and taken its JIT grant when the wait gave up and reported
  "no secondary came up". It has its own `CELL_RUN_TIMEOUT_NS` (100 s, under the harness's
  120 s) so a genuine hang still reports here with a reason.
- The secondary ran the cell **cooperatively**: `0/24` slices taken, while the primary's
  identical boot was preemptive. A preemption timer is per-core hardware no trampoline sets,
  so the secondary now arms its own when the publisher asks for one - `83/6243` slices after.

**And so do Node.js and Claude Code** - `linuxnodesmp` and `linuxclaudesmp`, the same
construction: same binary, same disk, same JIT authority, same preemptive dispatch, same
strict gate, `on_secondary` the only difference. Observed:

| runtime | size | block-cache fills | preemption slices on the secondary | result |
|---|---|---|---|---|
| Bun (JSC) | 99 MB | ~9,200 | 83 of 6,243 | `rheo:42`, exit 0 |
| Node.js (V8 + libuv) | 124 MB | ~15,300 | 23 of 9,477 | `rheo:42`, exit 0 |
| Claude Code (Bun-compiled) | 275 MB | ~116,300 | 1,612 of 61,701 | `2.1.220 (Claude Code)`, exit 0 |

Each is its own kernel rather than a phase, deliberately: the primary-CPU proof is the
baseline every claim about these runtimes rests on, and a boot that runs one somewhere else
must not be able to weaken it. Six kernels, six independent results.

**Honest about what is still not shown.** These are the same *cooperative-within-a-core*
runtimes they are on the primary: their contexts are scheduled inside whichever core runs the
cell, and running them on several cores **at once** needs the per-cell locking of 10.2, which
is not built. What these six kernels establish is that the core a workload runs on is no
longer special - not that one workload can use several.

### 10.0f Two more global allocators - the pmem pool and the admission ledger

The 10.2 audit names shared `static mut` state as the gate, and `frames` was
locked first because it is the one every path touches. Re-reading the list
afterwards turned up two more that are just as global and were missed:

- **`mm::frames_pmem`** - the persistent-memory allocator. Its bitmap and search
  hint are read and written together by one operation, so two cores could both
  see a bit clear and both claim the frame. Identical in structure to `frames`,
  which is why it was easy to overlook: the DDR pool got the attention and its
  twin did not.
- **`sched::SYSTEM`** - the machine-wide admission ledger
  (docs/ARCHITECTURE-DEBT.md 2.5). It was reached through
  `pub fn system() -> &'static mut Admission`, which hands a `&mut` to every
  caller on every core, and `admit` is a read-modify-write on the committed
  total. A lost add lets the machine admit past 100%, which is the exact defect
  the ledger exists to prevent; a lost subtract leaks utilisation until nothing
  can be admitted again.

Both are behind a `SpinLock` now, and **unconditionally** rather than
`#[cfg(feature = "smp")]`: whether a structure needs a lock is a property of the
structure, not of which cargo features are enabled - the call `frames`,
`runtime::Heap` and the NVMe driver already made, and the lesson the `SYS_YIELD`
FP defect taught, that state whose safety depends on a build configuration gets
written twice and diverges. The `&'static mut` accessor is gone; the ledger is
reached only through `system_admit` / `system_release` / `system_committed_ppm`,
so the check and the commit happen under one acquire.

`frames_pmem::free` also gained the double-free assertion the DDR pool has, since
a clear bit there is precisely what an unserialised alloc handing one frame to
two cores produces.

**The proof, and its honest limit.** `smp`'s new phase runs both cores through
admit / sample-the-machine-total / release, 4096 rounds each, with three exact
oracles: every admit succeeds (two cores at 10% can never reach 100%, so a
refusal means upward drift), the total sampled *while this core holds its own
reservation* is always one or two holders' worth, and the ledger returns to
exactly 0 ppm.

It does **not** demonstrate the lock is load-bearing. Removing it and running
**400,000** admit/release pairs across two cores produced zero lost updates: the
critical section is a handful of instructions and TCG's interleaving is far
coarser than that, so the window is never hit under emulation. The phase asserts
the invariant continuously under genuine two-core traffic; the lock's necessity
is argued from the structure - the same shape as `frames`, whose lock the
four-core GEMM phase *does* exercise - and confirming it is a lab claim on real
hardware, where out-of-order execution and true concurrency make the window
reachable. `frames_pmem` gets the same treatment for the same reason, and with no
phase of its own: an nvdimm is x86-64-only here, and adding one to the `smp`
launch would perturb the tree's most assertion-dense kernel to buy a control that
would not fire either.

### 10.0g The frame allocator, made a batch - flat combining

10.0f closes the *safety* question for the global allocators: every one of them is
behind a lock. This closes the *scalability* one for the hottest of them. The two
are different problems and the second only exists because the first was answered:
`mm::frames` is correct on N cores and it is also the one structure every path
touches, so it is where N cores queue.

Five changes, in the order they matter. The first is worth far more than the
others and would have been worth doing with no lock in sight; the last is about the
bitmap search itself rather than about getting to it, and is the largest single win
of the group.

**1. The 4 KiB zeroing left the critical section.** Every `alloc` set a bitmap
bit, three fields, and then zeroed 4096 bytes - *inside* the lock. That is a
handful of instructions of bookkeeping against 4096 bytes of stores, so the
critical section was ~99% `memset` and every core allocating waited on another
core's `memset` rather than on anything shared. It is safe to move out for one
reason: **the bitmap bit is what makes a frame unhandable.** Once it is set, no
scan can return that frame, so the window between the claim and the zeroing is
private to the claiming core by construction, not by timing. `alloc`, `alloc_on`
and `alloc_contig` now claim under the lock and zero after it.

**2. `alloc_on` takes one acquisition instead of three.** It read the node's
frame range (acquire), searched inside it (acquire), and fell back to the whole
pool (acquire). A range read that is not in the same critical section as the
search it bounds is a range that can change under it, so this was a correctness
tidy as much as a cost one.

**3. A copy-on-write break takes two instead of three.** `cow_fault` asked
`refs(pa)` (acquire), then `alloc()` (acquire), then `free(pa)` (acquire).
"Is this shared" and "give me somewhere to copy it to" are **one** decision, and
asking them separately means the first answer can be stale by the time the second
is served - so they are now `frames::cow_resolve`, returning
`Sole` / `Private(dst)` / `NoFrame`. The `Private` frame is deliberately **not**
zeroed: the caller overwrites all 4096 bytes, so a `memset` first is 4 KiB of
stores nothing reads, and the "never leak previous contents" rule is met by the
copy. The old frame's release is **not** folded in, and that is a correctness
requirement rather than laziness: dropping the reference before the copy would let
a peer core faulting on the same page see a count of one, conclude it was the sole
owner, and write through the very frame this copy is reading. Two acquisitions is
the floor for this operation.

**4. Flat combining** (Hendler, Incze, Shavit, Tzafrir, *Flat Combining and the
Synchronization-Parallelism Tradeoff*, SPAA 2010). With the `memset` gone the lock
is held for a few words, which is exactly the regime the technique is for: N cores
each taking a short lock pay N cache-line handoffs of the lock word plus N handoffs
of every line the section touches, and the work itself is trivial by comparison.
So a core publishes its request to its **own** 64-byte-aligned slot, one core wins
the *combiner* role, takes the lock once, executes the whole batch while the bitmap
stays in its cache, and writes each result back; the others spin on a line nobody
else writes.

Three things about the shape here rather than in the paper, and the first two are
what make it nearly free when nobody is contending:

- **The combiner is whoever holds the lock.** The paper gives the role its own flag
  because a combiner there may hold it across several lock acquisitions; here a
  batch is one critical section, so a separate flag is a second atomic claim over
  exclusivity the lock already provides. `SpinLock::try_lock` *is* the election.
  That deleted a static and two atomics per operation.
- **The wire form is slow-path only.** A request only has to become `(op, arg)`
  words if it is going to be published, so `fc_run` takes the operation twice -
  once as a closure returning its own type, for this core, and once encoded, for a
  peer's slot. The fast path never encodes or decodes, which is what keeps `alloc`'s
  `Option<usize>` from round-tripping through a `u64` on the path that never left
  this core.
- **The batch does not zero.** Each requesting core zeroes its own frame after its
  request completes. A combiner zeroing on behalf of the batch would re-serialise
  precisely what change 1 unserialised.

So the cost added to an uncontended operation is **one relaxed load** - the
`FC_PENDING` bitmask, which exists precisely so that a combiner with nothing to do
reads one word rather than scanning `MAX_CPUS` cache lines - on top of the lock the
pre-combining code already took. There is no separate "SMP path" to diverge from,
which is the mistake docs/SUBSTRATE.md pillar 3 records as the cause of the
`SYS_YIELD` FP defect.

One consequence is stated because it is a real if rare liveness cost: the lock's
*other* holders - the diagnostics, the boot paths - do not drain, so a request
published against one of those waits out its spin bound and is withdrawn rather
than served. It is counted, and the retry then takes the lock itself.

Executed **exactly once** is the invariant, and it is enforced by a claim rather
than argued: a publisher may withdraw its request only by moving its slot from its
own opcode straight to idle, which fails once a combiner has moved it to *busy*.
The withdrawal exists because a publication can land just after the combiner
sampled the mask, which no protocol without a second handshake avoids; it is a
liveness backstop, and it is counted so a machine that needs it says so.

The boot-time and diagnostic paths (`init_numa`, `alloc_contig`, `stats`,
`used_matches_bitmap`, `node_free`, `node_of`) deliberately stay on the plain
lock: they allocate nothing or run once, so a batch would add machinery to a path
with nothing to batch against.

**5. The search is a word at a time, not a bit at a time.** The three changes above
are about *reaching* the bitmap; this one is about the work once you are there, and it
is the largest of the five by a wide margin. The search was a per-frame loop - index a
word, shift, mask, test - so its cost grew with how many frames were **allocated**. On
the unrestricted path the rotating hint usually points straight at a free frame and it
never showed; on the NUMA path it did, because `alloc_on` restarts at the node's `lo`
on *every call*, so a node at 90% paid ~59,000 iterations per allocation and running
one dry - which the `numa` kernel does deliberately - is quadratic in the node's size.

`mm::bitmap` turns each 64 allocated frames into one load, one compare and one branch,
finding the free bit with a single `trailing_zeros`. Same answer; **63x fewer steps**
through a full region. The pmem pool got the same treatment, for 10.0f's reason: it is
the same scan, and its frame count is the one that is *not* a multiple of 64, since it
comes from whatever size the NFIT reports. `alloc_contig`'s run search is deliberately
left bit-at-a-time and says so where it lives: a run cannot skip an all-free word
without carrying a partial run across the skip, and it runs a handful of times per
boot, so the win would be unmeasurable and the risk would not.

It is **its own dependency-free module**, and that is the whole safety argument. This
is bit arithmetic with four boundary conditions the old form did not have - the first
word's low bits, the last word's high bits, both at once in a single-word range, and a
range whose end is not a multiple of 64 - and each is a case where being wrong is
*silent*: a missed free bit is a spurious out-of-memory on a machine with free memory,
and a bit returned from outside `[lo, hi)` is a frame on the wrong NUMA node that
`alloc_on` reports as correctly placed. So the functions take a plain `&[u64]` and no
kernel state, which lets `verify/bitmap/` include them verbatim and drive **683,792
cases** against a bit-at-a-time reference on the host: 16 hand-computed boundaries,
320,000 random `find_in`/`find_from` across densities from empty to full, 32,000
random `find_run`, and every 8-bit map *exhaustively*. Two controls fire, the second
by exactly the name that matters - "returned N outside [lo, hi) - on the NUMA path
this is a frame on another node reported as placed".

The cost is reported there rather than in `cargo xtask bench`, because **the bench
suite cannot see this change**: the benches allocate from a nearly-empty pool where
both algorithms stop on the first candidate, so "the benches are unchanged" would be
true and beside the point. Wall clock was tried too and is not claimed - the `numa`
boot went 11.6 s to 10.7 s, one sample of a boot doing far more than the run-dry
phase, under an emulator. The step count is what this container can defend.

**The proof** is a new `smp` phase, mirroring the shared-heap phase because the
failure mode is the same shape. Each core, in a loop: allocate a frame, stamp
every byte with its own marker, read every byte back, free it - so a frame handed
to both cores means one core's stamp lands in the other's frame between the write
and the read, which is the corruption itself rather than a proxy for it. The two
meet at a rendezvous first, and the counter is asserted to still match the bitmap
and no frame to have leaked. Then the same two cores run a **churn** pass -
allocate and free, touching nothing - because the verify pass spends far longer
outside the allocator than inside it and so measures the correctness, not the
combining. Across both passes **140-469 requests were
executed by the other core's combiner** (riscv64 315, aarch64 140, x86-64 469),
with **1-6 withdrawn**.

That withdrawal count matters more than its size: it means the liveness backstop
is *exercised* rather than dead code, and so is the `FC_BUSY` claim it interacts
with - a withdrawal racing a claim is the one interleaving in which a request could
be executed twice, and it is reached on every ISA on every run.

That number is **reported, never asserted**. Zero is a legal schedule (one core
running its whole share before the other starts), and TCG interleaves coarsely, so
an assertion on it could fail on a correct machine. It also stays modest *by
design*, which is the point rather than a disappointment: after change 1 there is
very little window left for a second core to arrive in. Shortening the window
removed most of the contention; combining handles what is left.

**What it costs, measured, because the whole case for change 4 is "cheap when
uncontended" and that had to be a number.** The bench suite touched none of these
paths, so five benches were added (`frame_alloc_free`, `frame_alloc_on_free`,
`frame_contig1_free`, `frame_share_free`, `frame_cow_resolve_sole`), icount
instructions per operation, against the pre-change tree:

Three columns, because the number moved twice and the reason each time is the
lesson. **A** is the pre-change tree. **B** is flat combining written as one
function. **C** is the shipped form: an `#[inline(always)]` leaf fast path,
`try_lock` as the election, and the wire encoding confined to the slow path.

| bench | x86-64 A -> B -> C | riscv64 A -> B -> C |
|---|---|---|
| `frame_alloc_free` | 652 -> 731 -> **656** | 1728 -> 1756 -> **1743** |
| `frame_alloc_on_free` | 689 -> 739 -> **679** | 1778 -> 1776 -> **1755** |
| `frame_contig1_free` | 883 -> 920 -> **886** | 2009 -> 2026 -> **2020** |
| `frame_share_free` (2 ops) | 94 -> 173 -> **100** | 123 -> 156 -> **138** |
| `frame_cow_resolve_sole` | 40 -> 85 -> **46** | 57 -> 85 -> **66** |

`frame_contig1_free` is the clean isolation: `alloc_contig` did not change layer,
so its delta is `free` alone going through the batch - **+3 instructions on x86-64,
+11 on riscv64**. `frame_share_free` is two such operations and agrees.

**And `frame_alloc_on_free` is now faster than before any of this** - 10
instructions on x86-64, 23 on riscv64 - because change 2 collapsed three lock
acquisitions into one, and only once the combining layer got down to ~3 did that
win stop being buried by it.

**Read against `frame_alloc_free`, the layer is +0.6% and +0.9%**, because the 4 KiB
`memset` dominates the operation - which is the same fact as change 1: the thing
the combining layer is measured against is precisely what used to be inside the
lock.

**The honest reading: on a single-core boot the combining layer is still a small
net loss** - ~3 instructions on x86-64, ~11 on riscv64 - since it buys nothing
where there is nobody to batch with. What changed is that it is now small enough to
be dominated by change 2's win on the NUMA path and invisible against the `memset`
on the others, rather than being a real tax.

Two things icount cannot price, both stated as lab claims rather than assumed: an
atomic RMW counts as one instruction here and costs far more on real silicon (the
shipped form's advantage is larger than the table shows, since it removed two of
them per operation); and the cache-line handoffs the technique removes are invisible
to an emulator with no cache model, so the contended saving is better than zero by
an amount only hardware can report. What is defensible from this container is the
shape - a leaf fast path adding one relaxed load to a lock that was already taken -
not a wall-clock speedup.

**Column B cost ~40 instructions per operation, and `objdump` said why** rather
than reasoning about it. One function held the election, the batch, the publication
and the spin loop, so LLVM allocated seven callee-saved registers for the cold half
and pushed and popped all seven on every fast-path call, and the `call fc_execute`
beside them kept `op` a runtime value so the six-way dispatch ran too. Splitting it
into an `#[inline(always)]` leaf with `fc_slow` and `fc_drain` both `#[cold]` took
it to ~11; folding the election into `try_lock` and confining the wire encoding to
the slow path took it to ~3. Recorded in docs/ENGINEERING.md 11: a hot path's cost
can be dominated by the *cold* code sharing its stack frame - and the second half
of that story is that once the frame was cheap, two atomics and a `u64` round trip
were suddenly the whole remaining cost, which is only visible once you are counting.

**Controls, both observed firing.** Deleting the zeroing makes the frame-zeroing
check report `4096 nonzero byte(s)`; making the combiner execute each request
twice trips `double free of <pa>` inside `release`.

**And the first version of the zeroing oracle passed with the fix deleted** -
recorded in docs/ENGINEERING.md 11, because the reason is not obvious and the
same mistake is available to anyone testing an allocator. Changes 2 and 3 have
**no control of their own** and that is stated rather than papered over: their
observable behaviour is identical to what they replaced - only the number of
acquisitions changed - so the existing `numa` (node placement and fallback
counting, exactly) and `cowfork` (2406 pages shared, 0 copied) phases are their
gate. There is no test that can distinguish one acquisition from three.

### 10.1 The measured motivation (not a wish)

The cooperative single-CPU scheduler switches to another context **only when the
current one blocks at a syscall boundary** (kernel/src/nproc.rs, linux/thread.rs).
That is correct for syscall-driven concurrency - Node.js completes because its main
thread blocks on `epoll_wait` for an eventfd a worker writes (docs/LINUX-COMPAT.md,
per-context blocking). It is **not** correct for a program that requires a sibling to
make progress *before* it ever blocks. The real Bun binary is the measured case
(GOAL-BUN, the `linuxbun` partial): it spawned a worker via `clone3` and then
`abort()`ed, and instrumentation showed **all 205 of its syscalls issued from the
main thread - the worker never got the CPU**. No syscall was missing; the load path
(streaming, demand paging, dynamic linking, the 128 GiB Gigacage, the event loop) all
worked. The frontier is that a second runnable context has no core to run on. That is
this phase, and it is the single largest lever named by the productivity goal:
multicore throughput, per-core tuning, memory locality, and "high performance
bun/Claude Code" all sit behind it.

### 10.1a Run it against a newer emulator: what QEMU 11 found

QEMU 11.0.3 was built from source in this container (three softmmu targets, libslirp built
alongside because QEMU 7.2+ externalised it and every rheo-net proof uses `-netdev user`)
and the x86-64 suite run against it. The reason for doing it is section 5's own thesis:
this kernel **observes** the LAPIC access mode rather than assuming it, and QEMU 8.2's TCG
reports no x2APIC - so *the x2APIC path had never once executed*. A fallback that is always
taken is a fallback that has never been tested against the thing it falls back from.

Predicted from reading QEMU 11's source before running it: `CPUID_EXT_X2APIC` and
`CPUID_EXT_PCID` are in `TCG_EXT_FEATURES` and `INVPCID` is in the leaf-7 word (8.2 had
none of the three), and `hw/riscv/riscv-iommu-pci.c` is a real device. FRED and AVX-512 are
**still absent from TCG** and are KVM-only or unmodelled (docs/CPU-FEATURES.md 1.3).

**It found a real defect immediately, and the shape is one this file has recorded three
times already.** `lapic_probe` latched `EXTD` on the boot CPU for the first time, so
`apic_mode()` became `X2Apic` machine-wide - and the first secondary took a **#GP at
`RDMSR 0x802`** before it could report anything. The cause: `IA32_APIC_BASE` is a
**per-CPU** MSR, the AP trampoline carries CR0, CR4 and EFER but not that, so a secondary
in x2APIC mode starts with `EXTD` clear. And the very first thing a secondary does is ask
which CPU it is - which on this ISA *is* a LAPIC access (`lapic_id()`, which in x2APIC mode
is `RDMSR 0x802`). So the fault lands before the core has an identity to name itself with.

That is the fourth instance of one pattern: **per-CPU register state that no trampoline
sets** - after the LAPIC software-enable, CR4/EFER, and the SYSCALL MSRs. It was
*unreachable* on 8.2 rather than absent, because the xAPIC MMIO path has no per-core enable
to forget: the register file is memory, and memory is shared. `lapic_adopt_this_cpu()` now
runs as the AP's first Rust action, before `secondary_trap_init` and before anything asks
for a CPU index, and 4 CPUs come online on QEMU 11.

**Still failing under QEMU 11, and not yet diagnosed:** the 4-core GEMM barrier phase
reports that not all four online cores met inside one interval, where it passes on 8.2. It
is a *different* failure from the one above - every phase up to and including two cells in
user mode on two cores passes - and it is recorded here as an open question rather than
guessed at, because the last four defects in this branch were each settled by a counter
rather than by a story.

**What this licenses, stated narrowly.** The x2APIC fix is verified in both directions: the
#GP is gone on QEMU 11, and `smp`, `preempt`, `librheoipc` and `netwait` still pass on
QEMU 8.2 (where the new function is a no-op by construction, since the mode is never
`X2Apic` there). The full 210-boot matrix under QEMU 11 has **not** been run, so QEMU 8.2
remains the reference emulator for every claim in this repository until it has.

### 10.2 The gate: an SMP-safety audit of shared `static mut` state

The bring-up parks each secondary precisely because the kernel's mutable statics are
written with no lock and read on the assumption of one CPU. Feeding a secondary
runnable work before this is done is a data race, not a scheduler. So the **first
deliverable is an audit, not a scheduler**: enumerate every `static mut` a second core
could touch, and give each one an explicit discipline - a lock, per-CPU partitioning,
or a single-owner core. The known set, from this tree (the Linux-personality half is
enumerated statically and classified in §10.2a below; the rest is still a plan):

- **`mm::frames` / `frames_pmem`** - the frame allocator + per-frame refcount (COW).
  A global `SpinLock` initially; a per-CPU magazine (free-list cache) later for the
  hot path, refilled/drained under the global lock (the standard slab-magazine shape).
- **Page tables** - per-cell root, but `AddressSpace` mutation (map/unmap/protect,
  COW privatisation) races a concurrent fault on the same cell. Per-address-space lock;
  a remote-TLB shootdown IPI (section 9 names it) for unmap/protect that another core
  may have cached.
- **`capability` object/cap tables, `user` cell table** - a per-cell lock, or the
  seL4 discipline of a big-kernel-lock first and finer locks proven in later.
- **The Linux personality per-cell state** (`linux::*`: fd table, VMAs, signal
  dispositions, thread table, futex waiter lists) - all indexed by cell; a per-cell
  lock makes threads of one cell safe across cores, which is exactly what Bun needs.
- **`ktimer`** (the single hardware one-shot owner), **`net_rx`**, **`input`** - each
  becomes **per-CPU**: every core has its own timer arbiter over its own local timer,
  and RX interrupts are steered to a core. This is the natural home for the per-CPU
  timer the section-9 IPI note anticipated.
- **`idle`, `sched` (the admission ledger)** - the run queue and the system-wide
  reservation ledger; the ledger stays global under a lock, the run queue goes
  per-CPU (below).

The rule (docs/ENGINEERING.md 3, one owner per shared resource) is the whole design:
each static gets exactly one owner or one lock, and the single-CPU build compiles the
locks out (a `SpinLock` on one core is an uncontended flag), so the cooperative path
is unchanged and stays the proof-of-correctness baseline.

#### 10.2a The Linux-personality half of the audit, performed

The list above is a plan; this is the audit itself for the half that gates Bun. Every
mutable static in `kernel/src/linux/` is enumerated below with the class it belongs to
and the discipline that class needs. It is written down because the alternative is a
list rediscovered each time a slice touches the personality - and because the classes,
not the individual names, are what decides how much locking is enough.

**Class A - per-cell, indexed by cell.** One row per cell, and no cell's row is read or
written by another cell's syscall:

| File | Static | Holds |
|---|---|---|
| `mod.rs` | `LINUX_STATE` | fd table, cwd, brk/mmap bookkeeping, VMA list |
| `proc.rs` | `PROCS`, `ASPACE`, `POLLSET` | process state, address space, the one pollset per cell |
| `thread.rs` | `THREADS`, `FRAMES`, `FPAREAS`, `CUR_THREAD`, `NEXT_TID` | execution contexts, their trap frames and FP areas |
| `signal.rs` | `ACTIONS`, `CTXS` | dispositions, per-context signal state |

Class A is **safe by partitioning while one cell runs on one core** - which is exactly
what `user::claim_cell` and the affinity tests in `nproc::schedulable` /
`linux::proc`'s three runnable predicates enforce today, and why two Linux cells on two
cores is proven (section 10.0). It stops being safe the moment *two contexts of one
cell* run on two cores, because then two cores index the same row. That is the slice
Bun's worker wants, and the discipline it needs is a **per-cell lock** - one per
`LINUX_STATE` row, taken by the syscall path and by `fill_fault`, which is finer than
`plock` and is what removes the "syscalls serialise machine-wide" limitation.

**Class B - genuinely global, one copy for the machine.** Reached by any cell's
syscall, so two cells on two cores reach them concurrently:

| File | Static | Holds |
|---|---|---|
| `filemap.rs` | `TBL` (`Funded`) | the refcounted mapped-file registry |
| `pipe.rs` | `PIPES` | cross-cell pipe rings (L6) |
| `eventfd.rs` | `TBL` | eventfd counters (the counter is here, not in the fd) |
| `timerfd.rs` | `TBL` | armed timerfds |
| `epoll.rs` | `EPOLLS_TBL` | epoll sets |
| `unixsock.rs` | `LISTENERS_TBL` | the AF_UNIX name registry |
| `inetsock.rs` | `LISTENERS_TBL`, `DGRAMS`, `EPHEMERAL` | loopback INET endpoints, the ephemeral-port counter |
| `proc.rs` | `NEXT_PID` | the pid counter |

Class B is what `plock` exists for, and it is the reason the coarse lock was the right
first step: a per-cell lock does **not** cover these, so the finer-locking slice must
give each Class B table its own lock at the same time it splits Class A - or keep
`plock` for exactly these and take it *inside* the per-cell lock. Either is sound; the
second is smaller and is the recommended order, because a Class B table is touched a
few times per syscall while Class A is touched on every one.

**Class C - global scratch buffers.** The sharpest hazard in the file set, and the one
a reader does not expect, because these are not state - they are staging areas used
*within* one syscall:

| File | Static | Used by |
|---|---|---|
| `mod.rs` | `MAPS_SCRATCH` (8 KiB) | rendering `/proc/self/maps` at open |
| `proc.rs` | `EXEC_STR` (16 KiB), `EXEC_PATH` | staging `execve`'s argv/envp/path out of the old address space |
| `stack.rs` | `LAST_AUXV`, `LAST_AUXV_LEN` | the auxv `/proc/self/auxv` serves |
| `signal.rs` | `CTX_SCRATCH` | the fallback when a context has no funded signal slot |
| `thread.rs` | `FP_SCRATCH` | the fallback FP save area |

Two cores in one of these interleave *mid-syscall*, and the result is not a lost update
but a **mixed buffer** - one cell's `execve` running with fragments of another's argv, or
a `maps` render containing another cell's lines. No fault, no log; the failure looks like
the program misbehaving. `plock` covers all five today. When it is split, each must become
either per-CPU (the shape `PDEPTH` already uses) or a stack local - and per-CPU is the
honest answer for the two large ones, since 16 KiB on the kernel stack is not available.

**Class D - already per-CPU or atomic.** `mod.rs`'s `PLOCK` (atomic) and `PDEPTH`
(`PerCpu`) - the lock's own state, correct by construction.

Futex waiter state is not a table of its own: a context's `fut_addr` and its wait
deadline are fields of its row in `THREADS`, so it is Class A and needs the per-cell
lock, nothing more. `vma.rs`, `fd.rs`, `errno.rs` and `dirent.rs` hold **no** mutable
statics - their state lives inside `LINUX_STATE`, which is what makes them Class A by
containment rather than by their own discipline. That completes the file set.

**Class E - diagnostic counters, lost updates only.** `mod.rs` `TRACE`, `TRACE_AT`,
`ENOENT_LOGGED`, `STDOUT_TAP`; `mem.rs` `FAULTS`, `FAULTS_MMAP`; `thread.rs`
`DEADLOCK_WAITS`, `IMMEDIATE_TIMEOUTS`. A race here costs a miscount or an interleaved
trace line, never correctness - but a test that *asserts* one of these counts is reading
a racy value, so any such assertion must either run single-core or the counter must
become a relaxed atomic first (the fix `preempt`'s counters already took, section 10.0).
`STDOUT_TAP` is a write-once function pointer set by a test before a run and read per
write; the tap the `smp` kernel installs keys its capture buffer on
`user::current_index()`, which is `PerCpu`, which is what lets two cells' transcripts be
captured separately on two cores.

**What was outside the lock, and now is not.** `plock` brackets `linux::handle` (the
whole syscall dispatch) and `linux::fill_fault` (the demand-paging entry, which is why
the lock is recursive per CPU: a syscall reaches `fill_fault` through `uaccess`). Three
further paths reach personality state from trap context, and the audit found all three
**outside** it:

- `linux::thread::preempt_context` and `linux::proc::preempt_cell`, from
  `user::on_user_interrupt` (Class A: `THREADS`, `CUR_THREAD`, `PROCS`).
- `linux::deliver_fault` and `linux::proc::exit_signaled`, from `user::on_user_trap`
  (Class A: `ACTIONS`, `CTXS`, `PROCS`).
- `linux::dup_state` / `install_cell` / `exec_reinit` / `reap` / `reset`, from the
  loader and the run loop (Classes A and B).

The first two now take `plock` themselves (`user.rs`, both sites, under the recursive
guard the syscall path uses), which is why `linux::plock` and `PGuard` are `pub`. The
third set runs on the primary between rounds with no core inside a cell, and is left
alone rather than bracketed for a concurrency it does not have.

They had been **unreachable, not safe** - every multi-core Linux phase ran with dispatch
**off**, so no slice fired and neither call was ever made. That was a property of the
proofs, not of the code, so a phase was written to execute them: `linuxsmp`'s
`test_preempted_threads_two_cores` runs two *multi-threaded* cells (the `rustthreads`
fixture `linuxthreads` asserts) on two cores under each core's own preemption timer, and
asserts both exact transcripts, both exit codes, the overlap, no double entry, and that
preemptions were genuinely taken - 47 of 322 slices on x86-64, 27 of 150 on riscv64, every
one of them into a **sibling context**, which is `linux::thread::preempt_context` executing
from trap context on two cores at once. Three things it does not claim:

- `linux::proc::preempt_cell` is not exercised *here*. A 4-thread cell always has a ready
  sibling, so the first arm always answers; executing the second needs a single-context
  cell that outlives its slice, and no fixture in this kernel is one. It is proven
  elsewhere, single-core: `linuxproc`'s `preemptfork` phase forks and spins on **both**
  sides, so neither cell has a sibling to move to and 392-930 preemptions go to the other
  *cell* on all three ISAs, against 0 in the cooperative control
  (docs/ARCHITECTURE-DEBT.md 7.6). What stays unclaimed is that arm on **two cores at
  once**, which is what this kernel would have to show.
- The *locking* has no deterministic negative control, because removing it leaves a race
  rather than a failure (docs/ENGINEERING.md 7 - reasoned and reviewed, not proven by
  revert).
- ARM64 took no preemption here at all, which was a real gap in the model rather than a
  property of that ISA - **fixed, see below**.

**And it found the gap that stage E5 closed - three findings, all measured.** The phase
first reported ARM64 taking **0** preemptions across two whole programs where x86-64 took
47 and riscv64 27 on the same binary, and the honest first version therefore gated on
whether a timer interrupt arrived and claimed nothing where none did. Chasing it produced
three facts, none of which was the first guess:

1. **A Linux syscall returning to its own context did not re-arm a slice.** A slice was
   armed at first entry, at a cell-level reschedule, and by the preemption path - not on
   an ordinary syscall return. So a cell whose contexts are scheduled *within* the cell,
   which is Node's and Bun's shape, got one slice and nothing armed another if it did not
   fire. `user::on_user_trap` is now a thin wrapper over `on_user_trap_inner`'s eight
   return paths and `sched::dispatch::rearm_remaining` arms there - one site, the
   reduction the FP/SIMD swap already got. It arms the slice's **remainder**, not a fresh
   slice: a full slice per return would let a cell syscalling every 100 us push its
   deadline out forever, which is the starvation this prevents wearing the costume of a
   fix.
2. **`dispatch::running` looked the vcore up and never admitted it.** The only thing that
   admitted a vcore was `pick`'s `sync_runnable`, so a cell that never reached a
   cell-level reschedule was never in the queue - the running record stayed empty, and the
   CPU-time charge, the burst score *and* the new re-arm all silently did nothing. Found
   by a counter, not by reasoning: `dispatch::rearm_counters` exists to distinguish "the
   site is never reached" from "the site is reached and declines", which an unchanged
   `armed` count cannot, and it reported the site reached **472** times and declining all
   472. "The vcore this CPU is running is in the queue" is an invariant now, established
   where the CPU starts running it.
3. **An ordering rule, not a defect.** On ARM64 a cell's SPSR carries its IRQ mask and
   `trapframe_new` derives it from `dispatch::enabled()`, so enabling dispatch *after*
   building the frames gives a cell running at EL0 with IRQ masked: 474 slices armed, 0
   interrupts taken. x86-64 and riscv64 read their mask at the same point, so it is one
   rule rather than a per-ISA workaround - enable dispatch before `trapframe_new`.

ARM64 now arms 819 slices, takes 156 timer interrupts and 55 preemptions; all three ISAs
preempt. The escape is narrowed to the one honest case, "this ISA has no slice to arm",
and the rest is asserted - after E5 the chain is a property of the kernel rather than of
the workload, so a wired one-shot with hundreds of deadlines against it must fire.

**It found one defect that does have a control.** `smp::run_cells_on_both` published two
cells and claimed neither. An unclaimed cell is visible to every core's scheduler - correct
on a single-CPU boot, and correct while dispatch was off - but with a slice firing, the
peer's `preempt_cell` scan saw this core's cell as runnable and switched into it: two cores,
one trap frame, one kernel stack. It presented as an instruction fetch at address 0 on both
cores, immediately and on every run. Each core now claims the cell it is about to enter -
the secondary in its work loop, because which core wins the published cell is not known to
the publisher - and reverting that reproduces the fault. It is the same lesson §10.0 records
for `place_cells`: the claim is the whole multi-core safety argument, so a path that enters a
cell without making one is unsafe the moment anything else can pick it.

### 10.3 Per-CPU infrastructure

- **Per-CPU stacks**: today each ISA has one dedicated secondary stack (section 9).
  Start-all needs one kernel stack per core, plus a per-CPU trap/IST stack on x86-64.
- **A per-CPU register**, replacing the `cpu_index()` id->index search: x86-64
  `GS_BASE` + `swapgs` at kernel entry, ARM64 `tpidr_el1`, RISC-V a per-hart `sscratch`
  slot. `this_cpu()` becomes a single register read.
- **Start-all**: **done** (`smp::start_all`, section 10.0) - it iterates the discovered
  CPU set and brings each up with its own stack via the section 5-7 paths, unchanged,
  falling back to probing the next hardware ids where the firmware enumerates nothing.
  Four CPUs come online on all three ISAs. ARM64 `PSCI_AFFINITY_INFO` **enumeration** is
  done too (section 7), so the inventory is now populated on every ISA and the fallback
  no longer fires anywhere - it stays as a genuine attempt for a machine whose firmware
  enumerates nothing.

### 10.4 The preemptive tick

Every core already has a genuinely interrupt-driven one-shot timer (sections 5-8, all
three ISAs). Preemption is that timer arming a **scheduler tick**: a context that has
not yielded by the end of its slice is preempted - its `TrapFrame` saved, the next
runnable context on that core resumed. This is the one mechanism the cooperative model
lacks (docs/CONCURRENCY.md, task #27's "a spinning thread starves siblings"), and it is
what lets Bun's worker run: on a second core immediately, or - even on one core - by
preempting the main thread so the worker is scheduled before main reaches its abort
check.

### 10.5 The scheduler itself (EEVDF + BORE, integer-only)

The policy is already researched and written down: docs/SCHEDULING.md 11 records the
CachyOS production stack - **EEVDF** (virtual deadline = eligible time + slice/weight)
as the base, with **BORE**'s burst-time penalty layered on top, and the load-bearing
insight that BORE's score is an **integer bit-length/log2** computation with **no
FPU** - which matters because the kernel is soft-float (CLAUDE.md, the hard/soft-float
split). So the scheduler math needs no floating point and no new dependency.

- **Per-CPU run queues** with **work-stealing**: an idle core steals from a busy
  core's queue tail. No global run-queue lock on the hot path.
- **NUMA locality** (goal: "memory locality"): the machine `Inventory` already carries
  SRAT/DT NUMA topology (CLAUDE.md, hw discovery). A cell's frames are allocated on a
  home node; its threads prefer cores on that node; stealing across nodes is a last
  resort and is charged a documented penalty. This is placement, not new mechanism.
- **P/E/LP-core awareness** (goal: "tuned to performance/energy/low-power cores"):
  extend the inventory's per-CPU descriptor with a `CpuClass` (from CPUID leaf 0x1A on
  x86-64 hybrid parts, the DT `capacity-dmips-mhz` on ARM64). Latency-sensitive strands
  and reservation-admitted work prefer P-cores; background/batch prefers E/LP-cores; the
  EEVDF weight and the core class compose (a low-weight task on an E-core is the energy
  path). Enumeration first (report the classes in `hwinfo`), placement second.

### 10.6 Composition, not extension

This adds **no kernel object and no syscall verb** - like phase 1, it is pure
mechanism. The run loop generalises the existing cooperative cross-cell switch
(`user::switch_native_cell`, still the one FP-swapping native switch); the timer
arbiter becomes per-CPU; the scheduler reuses ktimer, the interrupt-driven timers, and
the SCHEDULING.md policy. Reservations (object 7) finally gain **runtime enforcement**:
today admission is real but the guarantee is "admitted, not yet scheduled" (CLAUDE.md,
Phase C honesty) because there is one cooperative CPU; a preemptive per-CPU scheduler
is what makes an admitted budget an enforced one.

### 10.7 Ordering and the proof

Land it in slices, each keeping the single-CPU path byte-identical and green:

1. The 10.2 audit + locks (single-CPU: locks compile to uncontended flags; the whole
   existing suite must stay green - that is the regression gate).
2. Per-CPU stacks + register + start-all (two cores up, still only the primary fed
   work - phase-1 proof extended to N cores).
3. The preemptive tick on the primary (a spinning cooperative thread is now preempted -
   a direct test: a compute-bound sibling no longer starves a second).
4. Per-CPU run queues + work-stealing + NUMA/class placement.

The headline proof is an SMP scheduling test where **two cells genuinely run on two
cores at once** - a shared page incremented from both with an ordering witness that is
impossible under cooperative single-CPU interleaving - and the honest end-to-end proof
is **`linuxbun` flipping from the exit-134 partial to `rheo:42` + exit 0**, because the
worker that "never got the CPU" now has one. Both are the measurable definitions of
done for this phase; neither is claimed until observed.
