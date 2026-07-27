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
(docs/CONCURRENCY.md). Section 9 has the full accounting.

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

### What is *not* fixed by this

ARM64's `discover` still reports only the boot CPU. That was previously attributed to
PSCI being unusable, which is now disproved; the real reason is that QEMU's `virt` places
**no firmware table** for a bare ELF (x0 arrives as 0, no DTB in guest RAM), while x86
reads the ACPI MADT and RISC-V the device tree. PSCI is not an enumeration API - `CPU_ON`
starts a CPU you already name. Probing `PSCI_AFFINITY_INFO` over candidate affinities
*would* be a genuine enumeration path and is a documented follow-on; it is not done here
because it would move the PSCI helper out of the `smp` cargo feature and change every
kernel's inventory. The bring-up driver meanwhile synthesises the target id (`boot + 1`)
for exactly this case, which is why a genuine attempt was possible at all.

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
- A genuinely interrupt-driven one-shot timer on all three ISAs, verified at
  bring-up by an interrupt the kernel took (section 5).

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
- ARM64 CPU **enumeration** (probing `PSCI_AFFINITY_INFO`), so the inventory reports
  more than the boot CPU - section 7.
- Cross-CPU IPIs for anything beyond bring-up (the natural next users are a per-CPU
  timer arbiter and a remote-TLB shootdown; docs/NETSTACK.md 16 notes the arbiter
  shape).
