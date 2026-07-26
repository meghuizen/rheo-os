# SMP - Per-CPU State, Locking, and Secondary-Core Bring-Up

**Status:** Phase I (task #27). Per-CPU state + a kernel spinlock are done and
portable; a RISC-V secondary hart genuinely runs kernel code; ARM64 and x86-64
make an honest attempt and document a specific blocker. This is *proof of a
second core running kernel code plus the foundation for real SMP* - not
preemptive multi-core scheduling (still deferred; the runtime is single-CPU
cooperative, docs/CONCURRENCY.md).

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
(INIT-SIPI-SIPI) below 1 MiB; not implemented`. This mirrors how the project
already documents the x86 UART RX line as honestly poll-only under QEMU's TCG
split-irqchip - an honest per-ISA asymmetry, not a fake.

## 5. What is proven vs deferred

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
