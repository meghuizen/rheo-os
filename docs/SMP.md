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
- **Start-all: all of RISC-V's cores** - four online at once (boot + three
  secondaries, matching QEMU's `-smp 4`), the first slice of §10.3/§10.7-step-2. Each
  secondary claims a distinct registry slot and hardware id (1, 2, 3). Bring-up is
  **sequential**
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
- ARM64 CPU **enumeration** (probing `PSCI_AFFINITY_INFO`), so the inventory reports
  more than the boot CPU - section 7.
- Cross-CPU IPIs for anything beyond bring-up (the natural next users are a per-CPU
  timer arbiter and a remote-TLB shootdown; docs/NETSTACK.md 16 notes the arbiter
  shape).
- The **x86-64 NIC RX interrupt** - the last interrupt source any ISA is missing.
  Section 8 records why the old justification no longer holds and what is actually
  left to do.

## 10. Phase 2 design: preemptive multi-core scheduling (task #132)

**Status: design only.** Nothing in this section is implemented. It is written
docs-first (ARCHITECTURE.md 6 discipline for a change this large) so the work lands
in reviewable slices against a fixed plan, and so the single-CPU cooperative path
stays byte-identical until each prerequisite is genuinely safe.

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

### 10.2 The gate: an SMP-safety audit of shared `static mut` state

The bring-up parks each secondary precisely because the kernel's mutable statics are
written with no lock and read on the assumption of one CPU. Feeding a secondary
runnable work before this is done is a data race, not a scheduler. So the **first
deliverable is an audit, not a scheduler**: enumerate every `static mut` a second core
could touch, and give each one an explicit discipline - a lock, per-CPU partitioning,
or a single-owner core. The known set, from this tree:

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

### 10.3 Per-CPU infrastructure

- **Per-CPU stacks**: today each ISA has one dedicated secondary stack (section 9).
  Start-all needs one kernel stack per core, plus a per-CPU trap/IST stack on x86-64.
- **A per-CPU register**, replacing the `cpu_index()` id->index search: x86-64
  `GS_BASE` + `swapgs` at kernel entry, ARM64 `tpidr_el1`, RISC-V a per-hart `sscratch`
  slot. `this_cpu()` becomes a single register read.
- **Start-all**: iterate the discovered CPU set - ACPI MADT on x86-64, `PSCI_AFFINITY_INFO`
  enumeration on ARM64 (section 7 defers exactly this), the device-tree `cpus` node on
  RISC-V - and bring each up with its own stack via the section 5-7 paths, unchanged.

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
