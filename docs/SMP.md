# SMP - Per-CPU State, Locking, and Secondary-Core Bring-Up

**Status:** Phase I (task #27). Per-CPU state + a kernel spinlock are done and
portable; a RISC-V secondary hart genuinely runs kernel code. **Phase 1** (section
5 onward) fixed the root cause that blocked x86: the local APIC is now driven over
**xAPIC MMIO** with the access mode chosen by probe, which makes the x86-64 one-shot
timer genuinely interrupt-driven and gives the kernel a working interrupt command
register. This is *proof of a second core running kernel code plus the foundation
for real SMP* - not preemptive multi-core scheduling (still deferred; the runtime is
single-CPU cooperative, docs/CONCURRENCY.md).

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
actually starting a second core. Starting cores is blocked to different degrees
per ISA (see section 4); the tractable one is RISC-V.

Two things now exist:

1. **Portable per-CPU state + a kernel spinlock** (`kernel/src/smp.rs`) - real
   groundwork, valuable regardless of how many cores run.
2. **A genuine RISC-V secondary hart** running kernel code through that
   foundation, then parking.

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

## 4. ARM64 and x86-64: honest attempt + documented blocker

Neither ISA runs a second core here. Both make a genuine attempt and report a
specific, observed blocker; the `smp` test skips-with-reason and still PASSES
(the same pattern as `librheonet`/`librheogpu` skipping when a device is absent).

### ARM64 - PSCI CPU_ON traps from EL1

The kernel runs at EL1 with **no EL2/EL3** (QEMU `virt`, `secure=off`,
`virtualization=off`). The bring-up call `smp_start_secondary` issues a genuine
PSCI `CPU_ON` (`smc #0`, function id `0xC4000003`), but **guards** it: it
temporarily installs its own exception vector, masks interrupts, executes the
SMC, and restores (`kernel/arch/aarch64/smp.S`). With no EL3 to service it, the
SMC is UNDEFINED at EL1 and **traps back into EL1** - the guard catches the trap,
returns a sentinel, and the primary survives (without the guard the trap would
hit the kernel's fatal sync handler and fail the test).

**Observed** (the `smp` test, aarch64): `PSCI CPU_ON: smc #0 trapped to EL1 (no
EL3 firmware in this QEMU config)`. This empirically confirms the long-standing
note that PSCI is unusable from EL1 in this config - which is also why ARM64's
`discover` reports only the boot CPU (PSCI is the only CPU-enumeration path and
it needs EL3). A real ARM secondary needs an EL3 PSCI provider (firmware) plus a
shared-page-table MMU-on secondary trampoline; both are out of scope here.

### x86-64 - needs a real-mode AP trampoline

CPU detection is done (ACPI MADT enumerates the 4 LAPICs into the inventory), so
the primary has real AP ids to target. But an application processor starts in
**16-bit real mode** and must be released with an INIT-SIPI-SIPI sequence
pointing at a trampoline placed **below 1 MiB**, which then switches to long mode
and joins the kernel. PVH boot hands the kernel no low real-mode staging area or
firmware to build that trampoline, and doing it cleanly (without destabilising
the single-core boot every other kernel depends on) is out of scope for this
phase.

**Observed** (the `smp` test, x86_64): `needs a 16-bit real-mode AP trampoline
(INIT-SIPI-SIPI) below 1 MiB; not implemented`.

**This was only half the blocker.** INIT-SIPI-SIPI is *sent through the local
APIC's interrupt command register*, and this port drove the LAPIC through the
x2APIC MSR block, which QEMU's TCG leaves inert - so even with a trampoline in
place the SIPI would have gone nowhere. x86 SMP was blocked by the same root cause
as the x86 timer (docs/ENGINEERING.md 1). Section 5 fixes that root cause first.

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

## 9. What is proven vs deferred

**Proven**
- A portable `SpinLock<T>` and a per-CPU registry with `this_cpu()`, zero-impact
  on the single-CPU path.
- A genuine RISC-V secondary hart running kernel code, synchronising with the
  primary over the cross-core spinlock and per-CPU registry, then parking.
- Genuine, guarded bring-up attempts on ARM64 and x86-64 with empirically
  observed blockers.

**Deferred**
- Preemptive multi-core scheduling (the runtime stays single-CPU cooperative;
  CONCURRENCY.md). The RISC-V secondary does proof-of-life work and parks; it is
  not yet fed runnable cells.
- Making the shared kernel `static mut` state SMP-safe end to end (only the SMP
  test's own shared state is locked today).
- ARM64: an EL3 PSCI provider + a shared-page-table ARM secondary trampoline.
- x86-64: the 16-bit real-mode AP trampoline (INIT-SIPI-SIPI).
