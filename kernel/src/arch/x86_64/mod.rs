//! x86-64: PVH boot entry, 16550 UART on port 0x3F8, isa-debug-exit,
//! IDT-based traps, rdtsc, and the context-switch stub.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

/// Linux personality ABI (x86-64 legacy syscall table; docs/LINUX-COMPAT.md).
pub mod linux_abi;
mod paging;
use paging::apic_map_window;
pub use paging::{
    PagingRoot, mmio_map_window, paging_activate, paging_activate_kernel, paging_cow_at,
    paging_cow_clear, paging_cow_protect_user, paging_for_each_user_leaf, paging_kernel_init,
    paging_map, paging_map_frame, paging_mapped, paging_new_root, paging_protect,
    paging_unmap_frame, paging_unmapped_span, pmem_map_window,
};

/// `uname` machine string for the Linux personality (docs/LINUX-COMPAT.md L2).
pub const LINUX_UNAME_MACHINE: &str = "x86_64";

/// clone(2) argument order (docs/LINUX-COMPAT.md L4): x86-64 uses the standard
/// order `(flags, stack, parent_tid, child_tid, tls)` (not `CLONE_BACKWARDS`).
pub const CLONE_BACKWARDS: bool = false;

global_asm!(
    include_str!("../../../arch/x86_64/boot.S"),
    options(att_syntax)
);
global_asm!(
    include_str!("../../../arch/x86_64/vectors.S"),
    options(att_syntax)
);
global_asm!(
    include_str!("../../../arch/x86_64/context_switch.S"),
    options(att_syntax)
);
global_asm!(
    include_str!("../../../arch/x86_64/user.S"),
    options(att_syntax)
);

pub const NAME: &str = "x86-64";

/// Physical base of the frame pool: 64 MiB, above the kernel image and
/// within the low-1 GiB identity map (checked in frames::init).
pub const FRAME_POOL_BASE: usize = 0x0400_0000;

/// Exclusive top of this ISA's **user** virtual address range
/// (docs/SUBSTRATE.md pillar 2).
///
/// x86-64 4-level paging gives a 48-bit canonical space split in half, so the
/// low (user) half is `[0, 2^47)` - everything above is the non-canonical hole
/// and then the kernel half. 128 TiB.
///
/// The portable code above `arch` reads this rather than a single shared bound:
/// before pillar 2 every ISA was held to RISC-V Sv39's `2^38` (256 GiB), which
/// is the narrowest of the three, so x86-64 and ARM64 gave up 99.8% of their
/// user space to keep one constant portable. A cell that reserves large spans
/// (a JavaScript engine's pointer cage, a 128 GiB JSC Gigacage, a terabyte file
/// mapping) needs the real ceiling.
pub const USER_VA_TOP: usize = 1 << 47;

/// Kernel linear-map offset (docs/MEMORY.md): the kernel, all MMIO, and the
/// `.user` window run in the top-2 GiB high half (the x86-64 "kernel" code
/// model), so a physical address is reached at `pa | KERNEL_VA_BASE`. The whole
/// low half is left to user programs. The boot trampoline builds this map before
/// any Rust runs, and the kernel is linked at `phys_to_virt(load address)`
/// (link/x86_64.ld). RAM (< 2 GiB with `-m 1G` and the pool at 64 MiB) fits the
/// top-2 GiB window, so `pa | BASE == pa + BASE` for every physical address the
/// kernel touches.
pub const KERNEL_VA_BASE: usize = 0xFFFF_FFFF_8000_0000;

/// Physical address -> kernel virtual address (the high linear map).
#[inline(always)]
pub fn phys_to_virt(pa: usize) -> usize {
    pa | KERNEL_VA_BASE
}

/// Kernel virtual address (high linear map) -> physical address.
#[inline(always)]
pub fn virt_to_phys(va: usize) -> usize {
    va & !KERNEL_VA_BASE
}

// ---------------------------------------------------------------- serial

const COM1: u16 = 0x3F8;

unsafe fn outb(port: u16, value: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") value) };
}

unsafe fn outl(port: u16, value: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") value) };
}

unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { asm!("in al, dx", in("dx") port, out("al") value) };
    value
}

unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe { asm!("in eax, dx", in("dx") port, out("eax") value) };
    value
}

pub fn serial_init() {
    unsafe {
        outb(COM1 + 1, 0x00); // no interrupts
        outb(COM1 + 3, 0x80); // DLAB on
        outb(COM1, 0x01); // divisor 1 = 115200 baud
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8N1, DLAB off
        outb(COM1 + 2, 0xC7); // FIFO on, cleared
    }
}

pub fn serial_write_byte(byte: u8) {
    unsafe {
        // Wait for the transmit holding register to empty (LSR bit 5).
        while inb(COM1 + 5) & 0x20 == 0 {}
        outb(COM1, byte);
    }
}

/// Non-blocking read of one byte from COM1, or None if none pending
/// (LSR bit 0 = data ready).
pub fn serial_read_byte() -> Option<u8> {
    unsafe {
        if inb(COM1 + 5) & 0x01 == 0 {
            None
        } else {
            Some(inb(COM1))
        }
    }
}

// ---------------------------------------------- the local APIC (LAPIC) driver
// docs/SMP.md, docs/NETSTACK.md 16. Everything interrupt-driven on this ISA - the
// one-shot timer, inter-processor interrupts for AP bring-up, and the EOI that any
// IO-APIC-routed line needs - goes through the per-CPU local APIC. There are two
// ways to reach its registers and this port supports **both, selected by probe**
// (docs/ENGINEERING.md 1):
//
// - **x2APIC**: the MSR block at 0x800+. Needs no mapping and works whichever
//   page-table root is active, so it is preferred where it exists. QEMU's TCG does
//   **not** implement it (`CPUID.01H:ECX[21]` reads 0 with `-cpu max`, because
//   x2APIC is absent from QEMU's TCG feature word), and QEMU then treats the whole
//   0x800 MSR block as inert: `EXTD` never latches in `IA32_APIC_BASE`, register
//   writes are dropped, and `TMCCT` reads **0** - which reads as "the one-shot
//   already elapsed", so every deadline was satisfied instantly. That defect is the
//   case study in docs/ENGINEERING.md 1.
// - **xAPIC MMIO**: the 4 KiB register page at `0xFEE00000`, which QEMU *does*
//   model under TCG. It needs that page mapped uncacheable into the kernel root
//   **and every cell root** (an interrupt handler must reach EOI whichever root is
//   active) - `paging::apic_map_window`.
//
// [`lapic_probe`] enables x2APIC, reads `IA32_APIC_BASE` back, and keeps that mode
// only if `EXTD` actually latched; otherwise it falls back to xAPIC MMIO and
// verifies the register file responds by writing the spurious-vector register and
// reading the value back out of the device. The mode that survived is recorded in
// [`APIC_MODE`], and every accessor below reads the *validated* mode - never CPUID.

/// `IA32_APIC_BASE` (Intel SDM vol 3, 11.4.4): bit 11 = global enable (xAPIC),
/// bit 10 = x2APIC mode (EXTD), bits 12+ = the MMIO page's physical base.
const MSR_APIC_BASE: u32 = 0x1B;
const APIC_BASE_EN: u64 = 1 << 11;
const APIC_BASE_EXTD: u64 = 1 << 10;

/// LAPIC register offsets in the xAPIC MMIO page (Intel SDM vol 3, table 11-1).
/// The x2APIC MSR for the same register is `0x800 + (offset >> 4)`, which is what
/// makes one set of constants serve both access modes.
/// (The ICR pair, which only AP bring-up needs, is defined with the `smp`
/// feature's code below so a non-SMP build carries nothing unused.)
const LAPIC_ID: usize = 0x020;
const LAPIC_EOI: usize = 0x0B0;
const LAPIC_TPR: usize = 0x080;
const LAPIC_SVR: usize = 0x0F0;
const LAPIC_LVT_TIMER: usize = 0x320;
const LAPIC_TMICT: usize = 0x380;
const LAPIC_TMCCT: usize = 0x390;
const LAPIC_TDCR: usize = 0x3E0;

/// Offset of the local APIC inside the mapped APIC window (the window starts at
/// the IO-APIC's `0xFEC00000`; the local APIC is 2 MiB above it).
const LAPIC_WINDOW_OFFSET: usize = 0xFEE0_0000 - 0xFEC0_0000;

/// How the local APIC is reachable **as observed at bring-up**, not as advertised.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum ApicMode {
    /// No usable local APIC found: every APIC-driven capability stays off.
    None,
    /// The x2APIC MSR block latched (`EXTD` set and read back).
    X2Apic,
    /// The legacy xAPIC MMIO page responds to a write/read-back.
    XApic,
}

impl ApicMode {
    pub fn name(self) -> &'static str {
        match self {
            ApicMode::None => "none",
            ApicMode::X2Apic => "x2APIC (MSR)",
            ApicMode::XApic => "xAPIC (MMIO)",
        }
    }
}

static mut APIC_MODE: ApicMode = ApicMode::None;
/// Kernel VA of the mapped local APIC register page (xAPIC mode only).
static mut LAPIC_VA: usize = 0;

/// Chosen interrupt vectors: LAPIC timer 0x20, UART RX 0x21, LAPIC spurious 0xFF
/// (all above the 32 CPU-exception vectors).
const VEC_TIMER: usize = 0x20;
const VEC_UART: usize = 0x21;
const VEC_SPURIOUS: usize = 0xFF;

static mut TIMER_ENABLED: bool = false;

/// The access mode the bring-up probe validated.
pub fn apic_mode() -> ApicMode {
    // SAFETY: single CPU; set once at bring-up before any secondary or cell runs.
    unsafe { *core::ptr::addr_of!(APIC_MODE) }
}

/// Read a local APIC register by its xAPIC offset, through whichever access mode
/// the probe validated. Returns 0 when no APIC is usable.
fn lapic_read(reg: usize) -> u32 {
    match apic_mode() {
        // SAFETY: a validated x2APIC MSR read.
        ApicMode::X2Apic => unsafe { paging_rdmsr(0x800 + (reg >> 4) as u32) as u32 },
        // SAFETY: `LAPIC_VA` is the mapped, uncacheable register page; `reg` is one
        // of the aligned offsets named above, inside its 4 KiB.
        ApicMode::XApic => unsafe {
            ((*core::ptr::addr_of!(LAPIC_VA) + reg) as *const u32).read_volatile()
        },
        ApicMode::None => 0,
    }
}

/// Write a local APIC register by its xAPIC offset, through the validated mode.
/// A no-op when no APIC is usable, so a fallback path never faults.
fn lapic_write(reg: usize, value: u32) {
    match apic_mode() {
        // SAFETY: a validated x2APIC MSR write.
        ApicMode::X2Apic => unsafe { paging_wrmsr(0x800 + (reg >> 4) as u32, value as u64) },
        // SAFETY: as `lapic_read`.
        ApicMode::XApic => unsafe {
            ((*core::ptr::addr_of!(LAPIC_VA) + reg) as *mut u32).write_volatile(value);
        },
        ApicMode::None => {}
    }
}

unsafe extern "C" {
    fn timer_irq_stub();
    fn uart_irq_stub();
    fn spurious_irq_stub();
}

// ------------------------------------------------- UART RX over the IO-APIC
// docs/SMP.md 8, docs/LIBRHEO.md Phase D. COM1's ISA IRQ 4 is routed by the
// emulated IO-APIC to a LAPIC vector. This was documented as poll-only on the
// grounds that "under QEMU TCG + kernel-irqchip=split the LAPIC's ISR/IRR are not
// modeled (they read 0) and an IO-APIC-routed line does not reliably re-trigger".
// That diagnosis was made **through the inert x2APIC MSR block** (see the LAPIC
// section above): with no working EOI the first interrupt would indeed be the last,
// because the in-service bit is never cleared. With the APIC reached over xAPIC
// MMIO the whole chain is testable, so bring-up now **probes it end to end** and
// reports what happened.

/// IO-APIC register window (Intel ICH/82093AA): an index register and a data
/// window. Offsets are into the mapped APIC window, whose base *is* the IO-APIC.
const IOAPIC_REGSEL: usize = 0x00;
const IOAPIC_IOWIN: usize = 0x10;
/// Redirection-table entry `n` occupies index `0x10 + 2n` (low) and `+1` (high).
const IOAPIC_REDTBL: u32 = 0x10;
/// COM1 is ISA IRQ 4, and q35 wires GSI 4 to IO-APIC pin 4 (`gsi_handler` sends
/// every ISA line to both the i8259 - masked here - and the IO-APIC). The ACPI
/// MADT's interrupt-source-override table is **not** consulted; the only override
/// QEMU emits is IRQ 0 -> GSI 2, which does not affect this line. Stated because
/// it is an assumption, not a discovery.
const UART_GSI: u32 = 4;

/// Base of the mapped IO-APIC register page, or 0 if the APIC window is not up.
static mut IOAPIC_VA: usize = 0;
static mut UART_ENABLED: bool = false;
/// UART RX interrupts observed, incremented **only** inside the interrupt vector -
/// the unfakeable evidence the bring-up probe rests on.
static UART_FIRES: AtomicU64 = AtomicU64::new(0);
/// While set, the handler drains the UART byte and **discards** it, so the
/// bring-up probe's own byte never reaches the console ring.
static mut UART_PROBING: bool = false;

/// SAFETY: `IOAPIC_VA` must be the mapped IO-APIC page.
unsafe fn ioapic_write(index: u32, value: u32) {
    unsafe {
        let base = *core::ptr::addr_of!(IOAPIC_VA);
        ((base + IOAPIC_REGSEL) as *mut u32).write_volatile(index);
        ((base + IOAPIC_IOWIN) as *mut u32).write_volatile(value);
    }
}

/// Whether the UART RX interrupt is wired **and verified to fire** (false = the
/// honest poll path). Set by [`enable_uart_rx_irq`] only after the probe below has
/// seen a real interrupt arrive through the IO-APIC.
pub fn uart_irq_enabled() -> bool {
    // SAFETY: single CPU; set once at bring-up.
    unsafe { *core::ptr::addr_of!(UART_ENABLED) }
}

/// Bring up the UART RX interrupt (IO-APIC GSI 4 -> LAPIC vector 0x21) **and
/// verify it end to end**. Called only by the Phase D test, so no other kernel is
/// affected.
///
/// The probe is the point (docs/ENGINEERING.md 1). It puts the 16550 into loopback
/// and writes a byte, which QEMU's serial model delivers into the receive FIFO and
/// raises the ISA line for - so a successful probe exercises the *entire* chain the
/// real path uses: device line -> IO-APIC redirection entry -> LAPIC -> IDT vector
/// -> handler -> EOI. `UART_ENABLED` is set only if [`UART_FIRES`], which nothing
/// but the interrupt vector touches, actually moved.
pub fn enable_uart_rx_irq() {
    lapic_probe();
    if apic_mode() == ApicMode::None {
        crate::println!(
            "x86-64: no usable local APIC, so no UART RX interrupt - console input polls"
        );
        return;
    }
    // The APIC window's base is the IO-APIC page; the local APIC sits 2 MiB in.
    // SAFETY: single CPU at bring-up; `apic_map_window` is idempotent.
    unsafe { *core::ptr::addr_of_mut!(IOAPIC_VA) = apic_map_window() };
    set_idt_gate(VEC_UART, uart_irq_stub as *const () as u64);

    // SAFETY: kernel context; IO-APIC MMIO + 16550 port I/O.
    unsafe {
        // Redirection entry: vector 0x21, fixed delivery, physical destination,
        // active-high, edge-triggered, unmasked; destination = this CPU's APIC id.
        ioapic_write(IOAPIC_REDTBL + 2 * UART_GSI + 1, lapic_id() << 24);
        ioapic_write(IOAPIC_REDTBL + 2 * UART_GSI, VEC_UART as u32);
        // 16550: OUT2 gates the IRQ (QEMU's model checks it), ERBFI enables the
        // received-data-available interrupt.
        outb(COM1 + 4, 0x08); // MCR: OUT2
        outb(COM1 + 1, 0x01); // IER: ERBFI
    }

    // --- the self-test: does a received byte actually raise the interrupt? ---
    UART_FIRES.store(0, Ordering::Relaxed);
    // SAFETY: kernel context; the probe byte is discarded by the handler.
    let usable = unsafe {
        *core::ptr::addr_of_mut!(UART_PROBING) = true;
        let t0 = cycles();
        outb(COM1 + 4, 0x18); // MCR: OUT2 + LOOP
        outb(COM1, b'\0'); // received into the RX FIFO by loopback
        outb(COM1 + 4, 0x08); // drop loopback
        asm!("sti");
        while UART_FIRES.load(Ordering::Relaxed) == 0
            && ticks_to_ns(cycles().wrapping_sub(t0)) < PROBE_WINDOW_NS
        {
            core::hint::spin_loop();
        }
        asm!("cli");
        *core::ptr::addr_of_mut!(UART_PROBING) = false;
        UART_FIRES.load(Ordering::Relaxed) > 0
    };
    // SAFETY: single CPU; set once before any cell runs.
    unsafe { *core::ptr::addr_of_mut!(UART_ENABLED) = usable };
    if usable {
        crate::println!(
            "x86-64: UART RX interrupt verified over the IO-APIC (GSI {UART_GSI} -> vector \
             {VEC_UART:#x}, {} EOI) - a real interrupt arrived",
            apic_mode().name()
        );
    } else {
        // Leave the line masked so a half-working route cannot surprise a later
        // `sti` (the timer's idle path enables interrupts).
        // SAFETY: kernel context; mask the redirection entry again.
        unsafe {
            ioapic_write(IOAPIC_REDTBL + 2 * UART_GSI, (1 << 16) | VEC_UART as u32);
            outb(COM1 + 1, 0x00); // IER: no interrupts
        }
        crate::println!(
            "x86-64: UART RX interrupt did not arrive through the IO-APIC (APIC access: {}) - \
             console input polls, reported not claimed",
            apic_mode().name()
        );
    }
}

/// The UART RX interrupt handler (called from `uart_irq_stub`): drain every byte
/// the 16550 has into the portable RX ring, then EOI. Draining in a loop matters
/// for an edge-triggered line - a byte that arrived while the vector was being
/// taken would otherwise sit in the FIFO with no new edge to announce it.
#[unsafe(no_mangle)]
extern "C" fn x86_uart_irq() {
    UART_FIRES.fetch_add(1, Ordering::Relaxed);
    // SAFETY: single CPU; a plain static read.
    let probing = unsafe { *core::ptr::addr_of!(UART_PROBING) };
    while let Some(b) = serial_read_byte() {
        if !probing {
            crate::input::rx_push(b);
        }
    }
    lapic_write(LAPIC_EOI, 0);
}

/// Halt until the UART RX interrupt delivers a byte (only called when
/// [`uart_irq_enabled`]): the standard enable-halt-disable idle idiom, so this is a
/// genuine 0%-CPU park.
pub fn idle_wait() {
    // SAFETY: kernel context; the standard enable-halt-disable idle idiom.
    unsafe { asm!("sti; hlt; cli", options(nomem, nostack)) };
}

/// Deliver a scripted byte through the **real** UART RX interrupt, halting at `hlt`
/// until it is taken - the same path a live keystroke takes. Used by the
/// deterministic Phase D test.
///
/// The byte goes in by 16550 loopback, which is a genuine receive: QEMU's serial
/// model puts it in the receive FIFO and raises the ISA line, so the IO-APIC and
/// the LAPIC do the same work they do for a typed character. (RISC-V and ARM64 have
/// to raise their interrupt-controller input directly here, because QEMU's
/// 16550/PL011 loopback does not drive those controllers' lines; on x86 it does,
/// which is what makes this the more complete of the three.)
pub fn uart_inject_and_wait(b: u8) {
    // SAFETY: kernel context; 16550 port I/O + the unmask/halt/mask idiom.
    unsafe {
        outb(COM1 + 4, 0x18); // MCR: OUT2 + LOOP
        outb(COM1, b); // received into the RX FIFO
        outb(COM1 + 4, 0x08); // drop loopback (the byte stays in the FIFO)
        asm!("sti; hlt; cli", options(nomem, nostack));
    }
}

/// Whether the virtio-net RX interrupt is wired - **false on x86-64** (honest), and
/// the one interrupt source this ISA still lacks (docs/SMP.md 8).
///
/// The NIC here is virtio-*pci* driven entirely through PCI config space (the
/// `VIRTIO_PCI_CAP_PCI_CFG` tunnel, because PVH boot has no firmware to program
/// BARs), so **no BAR is assigned** to hold an MSI-X table. That is the whole of the
/// remaining gap, and it is driver work rather than a platform limit:
///
/// - the old second half of this justification - "legacy INTx would ride an IOAPIC
///   path that does not re-deliver reliably under QEMU TCG" - is **disproved**: the
///   UART RX line above runs through exactly that path, verified end to end. The
///   original observation had been made through the inert x2APIC MSR block, where a
///   missing EOI genuinely makes the first interrupt the last;
/// - and BAR assignment is not impossible either - `hw::assign_pci_bars` +
///   `arch::mmio_map_window` already assign and map real BARs for the GPU work
///   (docs/GPU-HARDWARE.md 12).
///
/// What is left is: assign the virtio-net BAR, program its MSI-X table (or discover
/// the q35 INTx routing), and wire the vector. Not attempted here - a claim about
/// this line has to be earned by a probe like the UART's, not by inheriting one.
///
/// Meanwhile the **timer** interrupt is genuine, so `SYS_WAIT_NET` does not spin: it
/// takes the timer-backed idle mode (`net_rx::IdleMode::TimerIdle`) - poll the
/// receive queue, halt at `hlt` for one timer slice, re-poll - a real halt at a low
/// duty cycle, honoured against the caller's deadline. It is reported as
/// timer-backed, never as NIC-interrupt-driven (docs/NETSTACK.md 16 has the per-ISA
/// table).
pub fn net_irq_enabled() -> bool {
    false
}

/// Whether a NIC interrupt is pending - never on x86-64 (no NIC IRQ wired).
pub fn net_irq_pending() -> bool {
    false
}

/// Bring up the virtio-net RX interrupt - not wired on x86-64 (see
/// [`net_irq_enabled`]). Returns false, so the portable wait picks its next-best
/// mode (the timer-backed idle) rather than claiming a NIC park.
pub fn enable_virtio_net_irq(_slot: usize) -> bool {
    false
}

/// Whether the LAPIC timer interrupt is wired **and verified to fire** (false =
/// the honest cooperative/poll path). Set by [`enable_timer_irq`] only after the
/// bring-up self-test below has seen a real one-shot interrupt arrive.
pub fn timer_irq_enabled() -> bool {
    // SAFETY: single CPU; set once before any cell runs.
    unsafe { *core::ptr::addr_of!(TIMER_ENABLED) }
}

/// Bring the local APIC up **and establish which access mode actually works**,
/// once. Idempotent; every APIC user calls it first.
///
/// The order matters and each step is verified from the other side
/// (docs/ENGINEERING.md 1):
///
/// 1. Software-enable the APIC (`IA32_APIC_BASE.EN`). The MSR itself is real on
///    every x86-64 - it is the *0x800 register block* that TCG omits.
/// 2. If CPUID advertises x2APIC, request `EXTD` and **read the MSR back**. Only
///    if the bit latched is x2APIC used; a dropped write leaves the bit clear and
///    the probe moves on rather than driving an inert register file.
/// 3. Otherwise map the xAPIC MMIO page (uncacheable, into the kernel root and,
///    via the shared PML4 entry, every cell root) and check the register file
///    responds: write the spurious-vector register, read it back from the device,
///    and require the value to match. A dropped MMIO write reads back as 0 or
///    `0xFFFFFFFF`, so this distinguishes a modelled APIC from an absent one.
///
/// After a mode is settled the shared setup runs through the validated accessors:
/// TPR = 0 (accept every priority), the spurious vector installed and the APIC
/// software-enabled in the SVR.
fn lapic_probe() {
    if apic_mode() != ApicMode::None {
        return;
    }
    // SAFETY: kernel context; `IA32_APIC_BASE` is architectural on every x86-64.
    let base = unsafe {
        let base = paging_rdmsr(MSR_APIC_BASE);
        paging_wrmsr(MSR_APIC_BASE, base | APIC_BASE_EN);
        base
    };

    // Step 2: x2APIC, only if the mode bit genuinely latches.
    let advertises_x2apic = (core::arch::x86_64::__cpuid_count(1, 0).ecx >> 21) & 1 == 1;
    if advertises_x2apic {
        // SAFETY: kernel context. The SDM requires xAPIC (EN) before x2APIC
        // (EN|EXTD); a direct disabled -> x2APIC transition is illegal.
        let latched = unsafe {
            paging_wrmsr(MSR_APIC_BASE, base | APIC_BASE_EN | APIC_BASE_EXTD);
            paging_rdmsr(MSR_APIC_BASE) & APIC_BASE_EXTD != 0
        };
        if latched {
            // SAFETY: single CPU, bring-up.
            unsafe { *core::ptr::addr_of_mut!(APIC_MODE) = ApicMode::X2Apic };
        }
    }

    // Step 3: xAPIC MMIO, verified by a write/read-back through the device.
    if apic_mode() == ApicMode::None {
        let va = apic_map_window() + LAPIC_WINDOW_OFFSET;
        // SAFETY: single CPU, bring-up; the accessors below need the VA first.
        unsafe {
            *core::ptr::addr_of_mut!(LAPIC_VA) = va;
            *core::ptr::addr_of_mut!(APIC_MODE) = ApicMode::XApic;
        }
        let probe = 0x100 | VEC_SPURIOUS as u32;
        lapic_write(LAPIC_SVR, probe);
        if lapic_read(LAPIC_SVR) != probe {
            // The page is mapped but nothing behind it answers: retract the claim.
            // SAFETY: single CPU, bring-up.
            unsafe { *core::ptr::addr_of_mut!(APIC_MODE) = ApicMode::None };
            return;
        }
    }

    // Shared setup, through whichever accessor won.
    lapic_write(LAPIC_TPR, 0); // accept interrupts of every priority
    lapic_write(LAPIC_SVR, 0x100 | VEC_SPURIOUS as u32); // software-enable + vector
    set_idt_gate(VEC_SPURIOUS, spurious_irq_stub as *const () as u64);
}

/// Program **this core's** LAPIC timer registers.
///
/// The LAPIC register file is per core, so a secondary that wants a preemption slice
/// must program its own - the primary's writes reach only the primary's
/// (docs/SMP.md 10.0). Deliberately *only* the per-core registers: the mode probe,
/// the IDT gate and the one-shot self-test in [`enable_timer_irq`] are global work
/// the primary does once, and running them on four cores concurrently raced on the
/// shared IDT and interleaved four copies of the probe's own console line.
pub fn enable_timer_irq_this_cpu() {
    let mode = apic_mode();
    if mode == ApicMode::None {
        return;
    }
    // **This core's own LAPIC has to be enabled first.** `IA32_APIC_BASE`, the task
    // priority register and the spurious-vector register are all per core, and the AP
    // trampoline sets none of them - it adopts the primary's CR0/CR4/EFER and nothing
    // else. Without the SVR software-enable bit the core's LAPIC delivers nothing at
    // all, so its timer is armed into a register file that is switched off: no
    // interrupt, no preemption, and no error to say why (docs/SMP.md 10.0). The
    // *discovery* of which access mode works stays global - it is a property of the
    // machine - so only the enabling is repeated here.
    // SAFETY: kernel context; `IA32_APIC_BASE` is architectural on every x86-64.
    unsafe {
        let base = paging_rdmsr(MSR_APIC_BASE) | APIC_BASE_EN;
        let base = if mode == ApicMode::X2Apic {
            base | APIC_BASE_EXTD
        } else {
            base
        };
        paging_wrmsr(MSR_APIC_BASE, base);
    }
    lapic_write(LAPIC_TPR, 0); // accept interrupts of every priority
    lapic_write(LAPIC_SVR, 0x100 | VEC_SPURIOUS as u32); // software-enable + vector
    // Divide config = 1 (bits: 0b1011 -> divide by 1).
    lapic_write(LAPIC_TDCR, 0b1011);
    // LVT timer: vector 0x20, one-shot (bits 17-18 = 0), unmasked.
    lapic_write(LAPIC_LVT_TIMER, VEC_TIMER as u32);
    lapic_write(LAPIC_TMICT, 0); // disarmed until the arbiter arms it
}

/// LAPIC timer interrupts observed (incremented by the handler). Used by the
/// bring-up self-test, so the claim "this ISA has a timer interrupt" rests on an
/// interrupt the kernel actually took.
static TIMER_FIRES: AtomicU64 = AtomicU64::new(0);

/// Bring up the LAPIC one-shot timer interrupt (vector 0x20), **and verify it**.
/// Called only by the kernels that arm a deadline.
///
/// The verification is the point (docs/ENGINEERING.md 1, rheo-net N2h). The
/// original port drove the timer through the x2APIC MSR block only, which QEMU's
/// TCG leaves inert: `TMCCT` read **0**, and 0 means "the one-shot elapsed", so
/// every deadline on this ISA was reported as already expired - a `SYS_ARM_TIMER`
/// sleep that did not sleep, while `timer_irq_enabled()` still claimed a hardware
/// timer. [`lapic_probe`] now reaches the same registers over the **xAPIC MMIO**
/// page when x2APIC is absent, and this function still refuses to claim the
/// capability on anything but an interrupt it actually took: arm a one-shot,
/// briefly unmask, and require [`TIMER_FIRES`] - incremented *only* inside the
/// interrupt vector - to move. `TIMER_ENABLED` is set only then; otherwise the
/// portable code falls back honestly (a cooperative deadline check for
/// `SYS_ARM_TIMER`, a bounded poll for a receive wait) instead of pretending to
/// park.
/// The control-register bits ring 3 relies on, for **this core** - nothing extra.
///
/// The counterpart of RISC-V's `sstatus.SUM`/`FS` block. On x86-64 the equivalents
/// live in CR0/CR4/XCR0, which `user_init` programs per core (a secondary calls it
/// from `x86_secondary_main`), and the AP trampoline adopts the primary's CR0/CR4/EFER
/// before that. SMAP - the SUM analogue - is not enabled in CR4, so a kernel access to
/// a user page needs no window. Deliberately empty rather than absent: the portable
/// caller (`smp::secondary_run`) must not have to know which ISAs need it
/// (docs/SMP.md 10.0).
pub fn user_mode_init_this_cpu() {}

pub fn enable_timer_irq() {
    lapic_probe();
    set_idt_gate(VEC_TIMER, timer_irq_stub as *const () as u64);
    enable_timer_irq_this_cpu();

    // --- the self-test: does a one-shot actually fire? ---
    let usable = if apic_mode() == ApicMode::None {
        false
    } else {
        TIMER_FIRES.store(0, Ordering::Relaxed);
        let t0 = cycles();
        lapic_write(LAPIC_TMICT, PROBE_COUNT);
        // SAFETY: kernel context; the standard unmask/wait/mask idiom. Bounded by
        // wall time, so a timer that never fires costs one short window at boot.
        unsafe { asm!("sti") };
        while TIMER_FIRES.load(Ordering::Relaxed) == 0
            && ticks_to_ns(cycles().wrapping_sub(t0)) < PROBE_WINDOW_NS
        {
            core::hint::spin_loop();
        }
        // SAFETY: kernel context; re-mask.
        unsafe { asm!("cli") };
        lapic_write(LAPIC_TMICT, 0);
        TIMER_FIRES.load(Ordering::Relaxed) > 0
    };
    // SAFETY: single CPU; set once before any cell runs.
    unsafe { *core::ptr::addr_of_mut!(TIMER_ENABLED) = usable };
    if usable {
        // Calibrate the LAPIC tick rate **now**, not on first use. The
        // calibration busy-spins a fixed TSC window, and doing that inside the
        // arbiter's first `timer_arm` burned the whole of a short deadline before
        // the hardware was even armed - so the first `sleep` on a fresh kernel
        // never reached a park and reported no idle. One line, but the class of
        // bug is docs/ENGINEERING.md 1 again: bring-up cost masquerading as an
        // elapsed deadline. Calibrating at bring-up puts that cost where it
        // belongs and leaves the arm path free of it.
        lapic_timer_count(1_000_000);
        lapic_write(LAPIC_TMICT, 0);
        crate::println!(
            "x86-64: LAPIC one-shot timer verified over {} - a real interrupt arrived",
            apic_mode().name()
        );
    } else {
        crate::println!(
            "x86-64: LAPIC one-shot timer unavailable (APIC access: {}), no timer interrupt \
             on this machine - deadlines fall back to the cooperative/poll path",
            apic_mode().name()
        );
    }
}

/// Initial count for the bring-up probe, and the wall-clock window it waits for
/// the interrupt in. The count is small enough to fire promptly at any plausible
/// APIC clock; the window is the honest upper bound on boot cost when it never does.
const PROBE_COUNT: u32 = 1 << 16;
const PROBE_WINDOW_NS: u64 = 20_000_000; // 20 ms

/// The LAPIC timer interrupt handler (called from `timer_irq_stub`): the
/// one-shot has fired, so record it (the bring-up self-test reads the count) and
/// EOI; the arbiter (`ktimer::service`) observes the elapsed deadline and returns.
#[unsafe(no_mangle)]
extern "C" fn x86_timer_irq() {
    TIMER_FIRES.fetch_add(1, Ordering::Relaxed);
    lapic_write(LAPIC_EOI, 0);
}

/// Monotonic now in nanoseconds **in the timer's own domain** - the TSC, which is
/// what [`timer_arm`]'s LAPIC count is calibrated against. The timer arbiter
/// (`kernel/src/ktimer.rs`) compares its deadlines against this.
pub fn timer_now_ns() -> u64 {
    ticks_to_ns(cycles())
}

/// Halt the CPU until an interrupt fires - the LAPIC one-shot the arbiter armed
/// (the standard enable-halt-disable idle idiom). Called only by
/// `kernel/src/ktimer.rs`, which owns the hardware one-shot (docs/NETSTACK.md 16);
/// it halts only with a deadline armed, since on this ISA nothing else is wired to
/// wake `hlt`.
pub fn timer_park() {
    // SAFETY: kernel context; the standard enable-halt-disable idle idiom.
    unsafe { asm!("sti; hlt; cli", options(nomem, nostack)) };
}

/// Arm the LAPIC one-shot timer for `deadline_ns` from now, without waiting. The
/// LAPIC timer counts the APIC bus clock; its rate is calibrated against the TSC
/// (`cycles()` + `ticks_to_ns`) once, then converted.
///
/// **Private mechanism of the timer arbiter** (`kernel/src/ktimer.rs`), the
/// kernel's single owner of the one-shot: a subsystem that arms this directly
/// cancels whatever another subsystem armed (docs/NETSTACK.md 16). Register a
/// deadline with the arbiter instead.
pub fn timer_arm(deadline_ns: u64) {
    let count = lapic_timer_count(deadline_ns);
    lapic_write(LAPIC_TMICT, count);
}

/// Whether the armed one-shot has fired (its current count reached 0).
pub fn timer_expired() -> bool {
    lapic_read(LAPIC_TMCCT) == 0
}

/// Disarm the LAPIC one-shot timer.
pub fn timer_disarm() {
    lapic_write(LAPIC_TMICT, 0);
}

/// Calibrate the LAPIC timer's tick rate against the TSC and return the initial
/// count for `deadline_ns`. Done once (the ratio is stable under QEMU), cached.
fn lapic_timer_count(deadline_ns: u64) -> u32 {
    static CAL_PPN: AtomicU64 = AtomicU64::new(0); // LAPIC ticks per ns, << 20
    let mut ppn = CAL_PPN.load(Ordering::Relaxed);
    if ppn == 0 {
        // Run the LAPIC timer for a known TSC span and count its ticks.
        lapic_write(LAPIC_TMICT, 0xFFFF_FFFF);
        let tsc0 = cycles();
        let lc0 = lapic_read(LAPIC_TMCCT);
        // Busy-spin a bounded TSC interval (~calibration window).
        while cycles().wrapping_sub(tsc0) < 2_000_000 {
            core::hint::spin_loop();
        }
        let lc1 = lapic_read(LAPIC_TMCCT);
        let tsc1 = cycles();
        lapic_write(LAPIC_TMICT, 0);
        let lapic_ticks = lc0.wrapping_sub(lc1) as u64; // counts down
        let ns = ticks_to_ns(tsc1.wrapping_sub(tsc0)).max(1);
        // LAPIC ticks per ns, scaled by 1<<20 for integer precision.
        ppn = (((lapic_ticks as u128) << 20) / ns as u128) as u64;
        if ppn == 0 {
            ppn = 1;
        }
        CAL_PPN.store(ppn, Ordering::Relaxed);
    }
    let count = ((deadline_ns as u128 * ppn as u128) >> 20).max(1);
    count.min(0xFFFF_FFFF) as u32
}

// ----------------------------------------------------------------- traps

/// One 16-byte interrupt gate. Layout per the Intel SDM.
#[repr(C)]
#[derive(Copy, Clone)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist_and_flags: u16,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

const VECTOR_COUNT: usize = 32; // CPU exception stubs emitted by vectors.S
/// Full 256-entry IDT so hardware-interrupt vectors (the LAPIC timer at 0x20, the
/// spurious vector 0xFF) can be installed alongside the 32 CPU-exception gates
/// (docs/LIBRHEO.md Phase F). Entries past the exceptions stay not-present until
/// `set_idt_gate` fills them.
const IDT_ENTRIES: usize = 256;

const IDT_EMPTY: IdtEntry = IdtEntry {
    offset_low: 0,
    selector: 0,
    ist_and_flags: 0,
    offset_mid: 0,
    offset_high: 0,
    reserved: 0,
};

static mut IDT: [IdtEntry; IDT_ENTRIES] = [IDT_EMPTY; IDT_ENTRIES];

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

unsafe extern "C" {
    // Table of the 32 exception stub addresses, emitted by vectors.S.
    static VECTOR_STUBS: [u64; VECTOR_COUNT];
}

/// Build one present interrupt gate (DPL 0, IF cleared on entry) for `handler`.
fn idt_gate(handler: u64) -> IdtEntry {
    IdtEntry {
        offset_low: handler as u16,
        selector: 0x08,        // boot GDT 64-bit code segment
        ist_and_flags: 0x8E00, // present, interrupt gate, DPL 0
        offset_mid: (handler >> 16) as u16,
        offset_high: (handler >> 32) as u32,
        reserved: 0,
    }
}

/// Install an interrupt gate for `vector` (used by the interrupt bring-up to add
/// the UART RX / timer / spurious vectors after the exception gates are set).
fn set_idt_gate(vector: usize, handler: u64) {
    // SAFETY: single CPU; the IDT is uniquely owned and already loaded (a live
    // edit of a not-present slot is safe - no interrupt targets it until the
    // controller is programmed to).
    unsafe {
        (*core::ptr::addr_of_mut!(IDT))[vector] = idt_gate(handler);
    }
}

pub fn trap_init() {
    unsafe {
        let idt = &mut *core::ptr::addr_of_mut!(IDT);
        for (i, entry) in idt.iter_mut().take(VECTOR_COUNT).enumerate() {
            *entry = idt_gate(VECTOR_STUBS[i]);
        }
        load_idt();
    }
    mask_legacy_pic();
}

/// Point this CPU's IDTR at the (shared) IDT.
///
/// # Safety
/// The IDT must already be filled in - this only loads the register.
unsafe fn load_idt() {
    let pointer = IdtPointer {
        limit: (core::mem::size_of::<IdtEntry>() * IDT_ENTRIES - 1) as u16,
        base: core::ptr::addr_of!(IDT) as u64,
    };
    // SAFETY: a well-formed descriptor over a filled table.
    unsafe { asm!("lidt [{}]", in(reg) &pointer) };
}

/// A secondary core adopts the primary's IDT. The *table* is shared - the vectors
/// are the same code on every core - but IDTR is a per-core register, so a core
/// that never runs `lidt` has no handlers at all and its first exception is a
/// triple fault.
#[cfg(feature = "smp")]
fn secondary_trap_init() {
    // SAFETY: the primary filled the IDT in `trap_init` before starting this core.
    unsafe { load_idt() };
}

/// Mask every line on the legacy 8259 PIC. Under PVH there is no firmware to
/// remap or disable it, and the kernel uses no legacy IRQs (the serial line is
/// polled), so an unmasked PIT IRQ0 would be delivered on vector 0x08 - the
/// #DF slot. Mask master and slave so nothing is delivered.
fn mask_legacy_pic() {
    // SAFETY: writing the PIC OCW1 mask registers; no memory effects.
    unsafe {
        outb(0xA1, 0xFF); // slave data
        outb(0x21, 0xFF); // master data
    }
}

static DOORBELLS: AtomicU64 = AtomicU64::new(0);

/// Called from the common stub in vectors.S with the interrupt-frame CS so
/// the RPL distinguishes a kernel trap from a user fault. Vector 3
/// (breakpoint) is the in-kernel doorbell self-test and returns; a fault
/// from ring 3 is recorded and unwinds; anything else is fatal.
#[unsafe(no_mangle)]
extern "C" fn x86_trap_handler(vector: u64, error_code: u64, rip: u64, _cs: u64) {
    if vector == 3 {
        DOORBELLS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // The LAPIC one-shot, taken in **ring 0** - the arbiter's own `timer_park`
    // (`sti; hlt; cli`) is where that happens, and it is the pre-preemption
    // behaviour unchanged: record the fire, EOI, resume. The arbiter observes the
    // elapsed deadline itself when the halt returns.
    if vector as usize == VEC_TIMER {
        x86_timer_irq();
        return;
    }
    // Ring-3 faults are handled by `x86_fault_trap` (via the .Lfault_from_user
    // path in vectors.S), which builds the full TrapFrame the signal machinery
    // needs. Reaching here means a kernel-mode exception: fatal.
    crate::println!("TRAP: vector {vector} error {error_code:#x} at rip {rip:#x}");
    exit(super::ExitCode::Failure);
}

/// Map a CPU exception vector to a portable fault cause (docs/LINUX-COMPAT.md
/// L5). #PF/#GP/#SS/#NP are SIGSEGV; #UD is SIGILL; #DE is SIGFPE; #AC is SIGBUS.
fn fault_cause(vector: u64) -> super::FaultCause {
    match vector {
        6 => super::FaultCause::Ill,  // #UD invalid opcode
        0 => super::FaultCause::Fpe,  // #DE divide error
        17 => super::FaultCause::Bus, // #AC alignment check
        _ => super::FaultCause::Segv, // #PF/#GP/#SS/#NP and the rest
    }
}

/// Called from the .Lfault_from_user path in vectors.S after a ring-3 fault has
/// been captured into the cell's TrapFrame. Returns the frame to resume (a
/// Linux signal handler entry, rewritten in place) or null to unwind and
/// terminate the cell. `fault_addr` is CR2 for a page fault, else the faulting
/// RIP (computed in asm).
#[unsafe(no_mangle)]
extern "C" fn x86_fault_trap(
    vector: u64,
    fault_addr: u64,
    frame: *mut TrapFrame,
) -> *mut TrapFrame {
    // The LAPIC one-shot, taken in **ring 3**: an interrupt, not a fault. EOI it,
    // record that a preemption slice elapsed, and let the portable scheduler decide
    // whether the CPU moves (docs/SUBSTRATE.md pillar 3). Returning `frame`
    // unchanged - which is what happens when nothing else is runnable - resumes the
    // cell at exactly the interrupted instruction, because `common_trap` routes a
    // ring-3 return through IRET.
    if vector as usize == VEC_TIMER {
        x86_timer_irq();
        crate::sched::preempt::note();
        return crate::user::on_user_interrupt(frame);
    }
    crate::user::on_user_trap(
        super::TrapKind::Fault,
        fault_cause(vector),
        fault_addr as usize,
        frame,
    )
}

/// One kernel-entry round trip via int3 (the doorbell measurement floor).
pub fn doorbell_trap() {
    unsafe { asm!("int3") };
}

pub fn doorbell_count() -> u64 {
    DOORBELLS.load(Ordering::Relaxed)
}

// ----------------------------------------------------- hardware discovery

unsafe extern "C" {
    static BOOT_INFO: u64;
}

/// The PVH hvm_start_info pointer QEMU passed in ebx. `BOOT_INFO` lives in the
/// identity-mapped-low `.boot.bss` (the 32-bit trampoline wrote it before
/// paging), so its symbol address equals its physical address; the kernel runs
/// high, so it is read through the high linear map.
pub fn boot_firmware_ptr() -> usize {
    let pa = core::ptr::addr_of!(BOOT_INFO) as usize;
    unsafe { (phys_to_virt(pa) as *const u64).read() as usize }
}

/// Discover the machine via ACPI (RSDP handed over by the PVH start info).
pub fn discover(inv: &mut crate::hw::Inventory) {
    inv.firmware = crate::hw::Firmware::Acpi;
    crate::hw::acpi::parse(boot_firmware_ptr(), inv);
}

// ------------------------------------------------------------------- SMP
// docs/SMP.md 6, task #27. **Real AP bring-up.** An application processor leaves
// reset in 16-bit real mode and is released by an INIT-SIPI-SIPI sequence through
// the local APIC's interrupt command register, with the SIPI's 8-bit vector
// naming a page **below 1 MiB** to start executing at. Two things had to exist
// first: a working ICR (the whole point of the xAPIC MMIO work above - the x2APIC
// MSR block this port used is inert under QEMU TCG, so a SIPI written there went
// nowhere), and a real-mode trampoline in low memory, which PVH boot gives no
// firmware to stage, so the kernel stages it itself
// (`kernel/arch/x86_64/smp.S`, copied to [`AP_TRAMPOLINE_PA`]).
//
// Everything here is `#[cfg(feature = "smp")]`, so the 55 kernels that never opt
// in link an unchanged library - including the trampoline's assembly.

#[cfg(feature = "smp")]
global_asm!(
    include_str!("../../../arch/x86_64/smp.S"),
    options(att_syntax)
);

/// Physical page the AP trampoline is copied to and started at. Constraints: 4
/// KiB aligned and **below 1 MiB** (a SIPI vector is 8 bits: the AP starts at
/// `vector << 12`), in RAM, and clear of anything firmware left in low memory.
/// 0x8000 sits above the real-mode IVT/BDA and below the PVH start-info and ACPI
/// staging area; [`smp_start_secondary`] **verifies** all of that at runtime
/// rather than assuming it.
#[cfg(feature = "smp")]
const AP_TRAMPOLINE_PA: usize = 0x8000;

#[cfg(feature = "smp")]
unsafe extern "C" {
    /// First byte of the trampoline, in `.boot.text` (VMA == LMA, so its symbol
    /// address is its physical address).
    static ap_trampoline: u8;
    static ap_trampoline_end: u8;
    /// The base address the trampoline was **assembled for**, published by the
    /// assembly so Rust can cross-check it against [`AP_TRAMPOLINE_PA`]. The
    /// trampoline is position-fixed, so a silent disagreement between the two
    /// constants would jump the AP into nowhere.
    static AP_TRAMPOLINE_BASE: u64;
    /// Slots in `.boot.bss` (identity low, so readable by the trampoline's
    /// 32-bit stage with paging off) where the primary publishes its own control
    /// registers for the AP to adopt verbatim - see the header of smp.S.
    static AP_CR4: u64;
    static AP_EFER: u64;
    static AP_CR0: u64;
}

/// Publish this (primary) CPU's `CR4`, `EFER` and `CR0` for the AP trampoline, so
/// the secondary comes up in **exactly** the primary's mode rather than in a
/// hand-picked approximation of it.
///
/// This is not tidiness. The kernel's page tables carry NX on real entries - the
/// APIC register window among them - and with `EFER.NXE` clear a set bit 63 is a
/// **reserved-bit** page fault, so an AP that merely set `LME` triple-faulted on
/// its first LAPIC read (observed, not theorised). Copying the control registers
/// makes that whole class of divergence impossible rather than enumerable.
#[cfg(feature = "smp")]
fn publish_ap_mode() {
    // SAFETY: kernel context; reads of this CPU's own control registers.
    let (cr4, cr0) = unsafe {
        let cr4: u64;
        let cr0: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        (cr4, cr0)
    };
    // SAFETY: kernel context; EFER is architectural.
    let efer = unsafe { paging_rdmsr(0xC000_0080) };
    // The three slots live in `.boot.bss`, whose symbol address is its physical
    // address; the kernel runs high, so they are written through the linear map.
    // SAFETY: single CPU at bring-up; three aligned 8-byte writes to kernel BSS.
    unsafe {
        let w = |sym: usize, v: u64| {
            (phys_to_virt(sym) as *mut u64).write(v);
        };
        w(core::ptr::addr_of!(AP_CR4) as usize, cr4);
        w(core::ptr::addr_of!(AP_EFER) as usize, efer);
        w(core::ptr::addr_of!(AP_CR0) as usize, cr0);
    }
}

/// LAPIC registers only AP bring-up needs (see the register table above).
#[cfg(feature = "smp")]
const LAPIC_ICR_LOW: usize = 0x300;
#[cfg(feature = "smp")]
const LAPIC_ICR_HIGH: usize = 0x310;

/// This CPU's local APIC id, read **from the local APIC** - each CPU's own
/// register file answers at the same address (QEMU maps the APIC page per-CPU, as
/// real hardware does), so this is a genuine hardware identity and not something
/// the primary told the secondary. In xAPIC the id is in bits 24-31.
fn lapic_id() -> u32 {
    match apic_mode() {
        ApicMode::XApic => lapic_read(LAPIC_ID) >> 24,
        ApicMode::X2Apic => lapic_read(LAPIC_ID),
        ApicMode::None => 0,
    }
}

/// Send an inter-processor interrupt to APIC id `dest`. `icr_low` carries the
/// vector plus the delivery mode / level / trigger bits.
///
/// The ICR is the one register whose two access modes differ in shape: xAPIC has
/// a 32-bit low/high pair where the **high half must be written first** (writing
/// the low half is what issues the IPI), while x2APIC has a single 64-bit MSR.
#[cfg(feature = "smp")]
fn lapic_send_ipi(dest: u32, icr_low: u32) {
    match apic_mode() {
        // SAFETY: a validated x2APIC MSR write; the ICR is MSR 0x830, 64-bit.
        ApicMode::X2Apic => unsafe {
            paging_wrmsr(0x830, ((dest as u64) << 32) | icr_low as u64);
        },
        ApicMode::XApic => {
            lapic_write(LAPIC_ICR_HIGH, dest << 24);
            lapic_write(LAPIC_ICR_LOW, icr_low);
        }
        ApicMode::None => {}
    }
}

/// Wait (bounded) for a sent IPI to be accepted: the ICR's Delivery Status bit
/// (12) clears. x2APIC has no delivery-status bit (its MSR write is synchronous),
/// so this only polls in xAPIC mode. Bounded by wall time, so a wedged APIC costs
/// one short window instead of hanging the primary.
#[cfg(feature = "smp")]
fn lapic_ipi_wait() {
    if apic_mode() != ApicMode::XApic {
        return;
    }
    let t0 = cycles();
    while lapic_read(LAPIC_ICR_LOW) & (1 << 12) != 0 {
        if ticks_to_ns(cycles().wrapping_sub(t0)) > 10_000_000 {
            return; // 10 ms; the caller's own bounded wait decides the outcome
        }
        core::hint::spin_loop();
    }
}

/// Busy-wait `ns` nanoseconds of TSC time. Used for the two delays the
/// INIT-SIPI-SIPI sequence architecturally requires. Deliberately **not** the
/// timer arbiter: bring-up runs before any deadline client exists, and touching
/// `arch::timer_*` here would break the single-owner invariant
/// (docs/ENGINEERING.md 3).
#[cfg(feature = "smp")]
fn delay_ns(ns: u64) {
    let t0 = cycles();
    while ticks_to_ns(cycles().wrapping_sub(t0)) < ns {
        core::hint::spin_loop();
    }
}

/// APIC id -> CPU registry index, filled by each CPU as it establishes its
/// identity. `u32::MAX` = unused. Indexed by CPU index, so slot 0 is the boot
/// CPU's APIC id.
#[cfg(feature = "smp")]
static mut APIC_ID_OF_CPU: [u32; crate::hw::MAX_CPUS] = [u32::MAX; crate::hw::MAX_CPUS];

/// This CPU's registry index, resolved from its **own** local APIC id against the
/// table each CPU filled in [`smp_set_this_cpu`].
///
/// x86-64 has no free general register to dedicate to a per-CPU pointer the way
/// RISC-V has `tp`: `FS` carries a Linux cell's TLS base and `GS`/`KERNEL_GS_BASE`
/// are reserved for the eventual `swapgs` per-CPU block, which is a change to the
/// syscall entry path and does not belong in a bring-up phase. A search over a
/// tiny fixed table is honest and costs a LAPIC read; it is on no hot path (the
/// single-CPU kernels never call it, since `smp::this_cpu` is only reached from
/// code that opted into SMP). Falls back to 0 - the boot CPU - if this CPU has not
/// registered, which is exactly the pre-bring-up single-CPU answer.
#[cfg(feature = "smp")]
pub fn cpu_index() -> usize {
    let me = lapic_id();
    // SAFETY: single writer per slot (each CPU writes only its own), and the
    // write happens-before the reads that matter (the secondary sets its slot
    // before marking itself online).
    let table = unsafe { *core::ptr::addr_of!(APIC_ID_OF_CPU) };
    for (i, &id) in table.iter().enumerate() {
        if id == me {
            return i;
        }
    }
    0
}

/// Record that the CPU running this call is registry index `index`, by writing
/// its own APIC id into that slot. Runs on the CPU it describes.
#[cfg(feature = "smp")]
pub fn smp_set_this_cpu(index: usize) {
    let me = lapic_id();
    // SAFETY: single CPU writes this slot, and no other CPU reads it before the
    // secondary signals itself up.
    unsafe {
        (*core::ptr::addr_of_mut!(APIC_ID_OF_CPU))[index] = me;
    }
}

/// The bootstrap processor's hardware id: its **local APIC id, read from the
/// hardware** (it is 0 on this QEMU q35 config, but that is an observation, not
/// an assumption). Requires the APIC to be up, so it brings it up.
#[cfg(feature = "smp")]
pub fn boot_cpu_hw_id() -> u32 {
    lapic_probe();
    lapic_id()
}

/// Whether the low page `[AP_TRAMPOLINE_PA, +4 KiB)` is safe to overwrite: RAM
/// per the firmware memory map, and clear of the two structures firmware left in
/// low memory that the kernel still reads - the PVH `hvm_start_info` block and
/// the ACPI RSDP it points at. Returns the reason it is not, or `None`.
///
/// This is a check rather than a comment because the page is chosen by the kernel
/// (PVH gives no low staging area to be handed one), and stamping a trampoline
/// over firmware data would be a silent memory corruption, not a failed boot.
#[cfg(feature = "smp")]
fn ap_page_conflict() -> Option<&'static str> {
    let lo = AP_TRAMPOLINE_PA as u64;
    let hi = lo + 4096;
    let inv = crate::hw::inventory();
    let mut in_ram = false;
    for i in 0..inv.nmem {
        let r = inv.mem[i];
        if r.kind == crate::hw::MemKind::Ram && r.base <= lo && lo + 4096 <= r.base + r.len {
            in_ram = true;
        }
    }
    if !in_ram {
        return Some("the chosen low trampoline page is not usable RAM in the firmware memory map");
    }
    let info = boot_firmware_ptr() as u64;
    if info >= lo && info < hi {
        return Some("the chosen low trampoline page holds the PVH hvm_start_info block");
    }
    // The RSDP address is the 5th u32 of hvm_start_info (magic, version, flags,
    // nr_modules, modlist_paddr(8), cmdline_paddr(8), rsdp_paddr(8)); read it
    // through the linear map rather than re-parsing ACPI.
    if info != 0 {
        // SAFETY: the PVH block QEMU wrote is in low RAM, reached through the
        // kernel's linear map; `rsdp_paddr` is at offset 32 per the PVH ABI.
        let rsdp = unsafe { ((phys_to_virt(info as usize) + 32) as *const u64).read() };
        if rsdp >= lo && rsdp < hi {
            return Some("the chosen low trampoline page holds the ACPI RSDP");
        }
    }
    None
}

/// Start the application processor with APIC id `hw_id`: stage the real-mode
/// trampoline in low memory, then release the AP with INIT-SIPI-SIPI. Returns
/// `Ok(())` once the SIPI has been sent (the portable `smp::bring_up_one` then
/// waits, bounded, for the AP to run kernel code and mark itself online), or a
/// reason a start was not attempted.
///
/// Nothing here can hang: both delays and the ICR delivery-status poll are
/// bounded by wall time, and a non-responsive AP surfaces as the caller's
/// `StartError::Timeout` rather than a wedged primary.
#[cfg(feature = "smp")]
pub fn smp_start_secondary(hw_id: u32) -> Result<(), &'static str> {
    lapic_probe();
    if apic_mode() == ApicMode::None {
        return Err("no usable local APIC (neither x2APIC nor the xAPIC MMIO page responded)");
    }
    // The trampoline is position-fixed: it was assembled for one base address,
    // and jumping an AP at a different one would land it in nowhere. Cross-check
    // the assembly's own view against ours before touching anything.
    // SAFETY: a read of a link-time constant in `.rodata`.
    let assembled_for = unsafe { *core::ptr::addr_of!(AP_TRAMPOLINE_BASE) };
    if assembled_for != AP_TRAMPOLINE_PA as u64 {
        return Err("AP trampoline base disagrees between smp.S and mod.rs");
    }
    if let Some(reason) = ap_page_conflict() {
        return Err(reason);
    }
    publish_ap_mode();

    // Copy the trampoline from `.boot.text` (identity low, so its symbol address
    // is its physical address) to the chosen low page. Both ends are touched
    // through the kernel's linear map, so this works whichever root is active.
    let src_pa = core::ptr::addr_of!(ap_trampoline) as usize;
    let end_pa = core::ptr::addr_of!(ap_trampoline_end) as usize;
    let len = end_pa - src_pa;
    if len == 0 || len > 4096 {
        return Err("AP trampoline does not fit in one low page");
    }
    // SAFETY: `src` is the trampoline's own bytes and `dst` is the low page
    // verified above to be RAM the firmware is not using; both are inside the
    // kernel's linear map of physical 0-2 GiB and the regions do not overlap
    // (the trampoline is linked at 1 MiB, the destination below it).
    unsafe {
        core::ptr::copy_nonoverlapping(
            phys_to_virt(src_pa) as *const u8,
            phys_to_virt(AP_TRAMPOLINE_PA) as *mut u8,
            len,
        );
    }

    // INIT-SIPI-SIPI (Intel MP spec / SDM 9.4.4). Delivery modes: INIT = 5,
    // Start-Up = 6; bit 14 = level assert, bit 15 = level triggered.
    const INIT_ASSERT: u32 = (1 << 15) | (1 << 14) | (5 << 8);
    const INIT_DEASSERT: u32 = (1 << 15) | (5 << 8);
    let sipi: u32 = (6 << 8) | (AP_TRAMPOLINE_PA >> 12) as u32;

    lapic_send_ipi(hw_id, INIT_ASSERT);
    lapic_ipi_wait();
    delay_ns(10_000_000); // 10 ms, the spec's post-INIT wait
    lapic_send_ipi(hw_id, INIT_DEASSERT);
    lapic_ipi_wait();
    for _ in 0..2 {
        lapic_send_ipi(hw_id, sipi);
        lapic_ipi_wait();
        delay_ns(200_000); // 200 us between the two SIPIs
    }
    Ok(())
}

/// Where the AP trampoline hands control to Rust, on the secondary CPU, in long
/// mode on the **primary's page tables** with its own stack. It hands the
/// portable driver this CPU's hardware identity - read from its own local APIC,
/// not passed in - and parks when that returns.
#[cfg(feature = "smp")]
#[unsafe(no_mangle)]
extern "C" fn x86_secondary_main() -> ! {
    // Everything a CPU needs before it can host ring 3 is **per-CPU register
    // state**, and the AP trampoline set none of it: IDTR, GDTR, TR, the
    // SYSCALL MSRs, CR0/CR4/XCR0 and GS_BASE all live in the core, not in
    // memory (docs/SMP.md 10.0). The tables' *contents* are the primary's -
    // only the descriptor a core loads and the block GS points at are its own.
    secondary_trap_init();
    user_init();
    crate::smp::secondary_run(lapic_id());
    loop {
        // SAFETY: kernel context on a parked secondary; interrupts are masked
        // (the AP has never enabled them), so this halts until an NMI/INIT.
        unsafe { asm!("cli; hlt", options(nomem, nostack)) };
    }
}

/// Feature names; bit i corresponds to index i in CpuReport.features.
pub fn cpu_feature_names() -> &'static [&'static str] {
    &[
        "sse",
        "sse2",
        "sse3",
        "ssse3",
        "sse4.1",
        "sse4.2",
        "avx",
        "avx2",
        "avx512f",
        "aes",
        "sha",
        "rdrand",
        "rdseed",
        "xsave",
        "fsgsbase",
        "nx",
        "pcid",
        "pdpe1gb",
        "x2apic",
        "avx512vnni",
    ]
}

/// Decode CPU vendor + features via CPUID.
pub fn cpu_report(_inv: &crate::hw::Inventory) -> crate::hw::CpuReport {
    use core::arch::x86_64::__cpuid_count;
    let mut report = crate::hw::CpuReport::EMPTY;
    let v = __cpuid_count(0, 0);
    // Vendor string is ebx, edx, ecx (12 bytes).
    report.vendor[0..4].copy_from_slice(&v.ebx.to_le_bytes());
    report.vendor[4..8].copy_from_slice(&v.edx.to_le_bytes());
    report.vendor[8..12].copy_from_slice(&v.ecx.to_le_bytes());

    let l1 = __cpuid_count(1, 0);
    let l7 = __cpuid_count(7, 0);
    let le = __cpuid_count(0x8000_0001, 0);

    let mut set = |bit: u32, on: bool| {
        if on {
            report.features |= 1 << bit;
        }
    };
    set(0, l1.edx & (1 << 25) != 0); // sse
    set(1, l1.edx & (1 << 26) != 0); // sse2
    set(2, l1.ecx & (1 << 0) != 0); // sse3
    set(3, l1.ecx & (1 << 9) != 0); // ssse3
    set(4, l1.ecx & (1 << 19) != 0); // sse4.1
    set(5, l1.ecx & (1 << 20) != 0); // sse4.2
    set(6, l1.ecx & (1 << 28) != 0); // avx
    set(7, l7.ebx & (1 << 5) != 0); // avx2
    set(8, l7.ebx & (1 << 16) != 0); // avx512f
    set(9, l1.ecx & (1 << 25) != 0); // aes
    set(10, l7.ebx & (1 << 29) != 0); // sha
    set(11, l1.ecx & (1 << 30) != 0); // rdrand
    set(12, l7.ebx & (1 << 18) != 0); // rdseed
    set(13, l1.ecx & (1 << 26) != 0); // xsave
    set(14, l7.ebx & (1 << 0) != 0); // fsgsbase
    set(15, le.edx & (1 << 20) != 0); // nx
    set(16, l1.ecx & (1 << 17) != 0); // pcid
    set(17, le.edx & (1 << 26) != 0); // 1 GiB pages
    set(18, l1.ecx & (1 << 21) != 0); // x2apic
    set(19, l7.ecx & (1 << 11) != 0); // avx512_vnni
    report
}

// ---------------------------------------------------- virtio-mmio slots
// q35 has no virtio-mmio transport (virtio is PCIe here). virtio-blk on x86
// needs the virtio-pci transport - a follow-on - so there are no slots to
// scan and `virtio_blk::probe` finds nothing.
pub const VIRTIO_MMIO_BASE: usize = 0;
pub const VIRTIO_MMIO_STRIDE: usize = 0;
pub const VIRTIO_MMIO_COUNT: usize = 0;

// ----------------------------------------------------- hardware RNG

/// True if the CPU has RDSEED or RDRAND.
pub fn has_hwrng() -> bool {
    use core::arch::x86_64::__cpuid_count;
    let l1 = __cpuid_count(1, 0);
    let l7 = __cpuid_count(7, 0);
    l1.ecx & (1 << 30) != 0 || l7.ebx & (1 << 18) != 0
}

pub fn hwrng_name() -> &'static str {
    use core::arch::x86_64::__cpuid_count;
    if __cpuid_count(7, 0).ebx & (1 << 18) != 0 {
        "RDSEED"
    } else if __cpuid_count(1, 0).ecx & (1 << 30) != 0 {
        "RDRAND"
    } else {
        "none"
    }
}

/// One 64-bit hardware random word, or None if no source produced one within
/// the bounded retry budget (never blocks). Prefers RDSEED (a true entropy
/// source) over RDRAND (a reseeded DRBG).
pub fn hwrng_u64() -> Option<u64> {
    use core::arch::x86_64::__cpuid_count;
    if __cpuid_count(7, 0).ebx & (1 << 18) != 0 {
        for _ in 0..64 {
            let (v, ok): (u64, u8);
            // SAFETY: RDSEED is present (checked above); it writes a GPR and
            // sets CF=1 on success, CF=0 when no entropy is ready.
            unsafe {
                asm!("rdseed {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok,
                     options(nomem, nostack));
            }
            if ok != 0 {
                return Some(v);
            }
            core::hint::spin_loop();
        }
    }
    if __cpuid_count(1, 0).ecx & (1 << 30) != 0 {
        for _ in 0..64 {
            let (v, ok): (u64, u8);
            // SAFETY: RDRAND is present (checked above).
            unsafe {
                asm!("rdrand {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok,
                     options(nomem, nostack));
            }
            if ok != 0 {
                return Some(v);
            }
            core::hint::spin_loop();
        }
    }
    None
}

/// PCI config read via the CF8/CFC I/O ports (mechanism #1). The ECAM base
/// is unused on x86 - the ports reach bus 0 without any MMIO mapping.
pub fn pci_cfg_read32(_ecam: u64, bus: u8, dev: u8, func: u8, off: u16) -> u32 {
    let addr = 0x8000_0000u32
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | (off as u32 & 0xFC);
    unsafe {
        outl(0xCF8, addr);
        inl(0xCFC)
    }
}

/// PCI config write via the CF8/CFC I/O ports. `off` is DWORD-aligned (the
/// low two bits are ignored), matching how the virtio-pci capabilities are
/// laid out (every field is DWORD-aligned).
pub fn pci_cfg_write32(_ecam: u64, bus: u8, dev: u8, func: u8, off: u16, val: u32) {
    let addr = 0x8000_0000u32
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | (off as u32 & 0xFC);
    unsafe {
        outl(0xCF8, addr);
        outl(0xCFC, val);
    }
}

/// The host bridge's 32-bit MMIO window for BAR assignment
/// (docs/GPU-HARDWARE.md 3). On q35 the PCI hole below 4 GiB spans
/// ~0xB000_0000..0xFEC0_0000; 0xE000_0000..0xF000_0000 sits safely inside
/// it, above the 0xB000_0000 ECAM and below the LAPIC/IOAPIC pages.
pub fn pci_mmio_window() -> (u64, u64) {
    (0xE000_0000, 0x1000_0000)
}

// -------------------------------------------------------------- user mode

/// Saved ring-3 register state. Layout matches the offsets in user.S.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct TrapFrame {
    rax: u64,
    rbx: u64,
    rcx: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    rbp: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rip: u64,
    rflags: u64,
    rsp: u64,
    kernel_sp: u64,
    _pad: u64,
}

pub fn trapframe_new(entry: usize, user_sp: usize, arg: usize, kernel_sp: usize) -> TrapFrame {
    TrapFrame {
        rax: 0,
        rbx: 0,
        rcx: 0,
        rdx: 0,
        rsi: 0,
        rdi: arg as u64, // first argument
        rbp: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
        rip: entry as u64,
        // Reserved bit 1, plus IF (bit 9) **only when the scheduler needs to be able
        // to take the CPU away from this cell** (docs/SUBSTRATE.md pillar 3).
        //
        // IF clear was right while every scheduler was cooperative, and for a reason
        // beyond tidiness: under PVH there is no firmware to remap the 8259 PIC, so a
        // legacy IRQ (the PIT's IRQ0) would arrive on vector 0x08 and be mistaken for
        // a #DF. That hazard is handled - the PIC is masked at boot (`mask_legacy_pic`)
        // and the LAPIC drives its own vectors - so the remaining consequence of a
        // clear IF was simply that the preemption timer could not be delivered to a
        // running cell, which is why nothing could stop one.
        //
        // Read at frame-construction time rather than flipped later, so a frame built
        // for a cooperative boot keeps the pre-migration bits exactly.
        rflags: if crate::sched::dispatch::enabled() {
            0x202
        } else {
            0x002
        },
        rsp: user_sp as u64,
        kernel_sp: kernel_sp as u64,
        _pad: 0,
    }
}

/// A zeroed frame, for static per-context storage (docs/LINUX-COMPAT.md L4).
/// Not runnable as-is; `clone_child_frame` fills one from a parent.
pub const fn trapframe_zeroed() -> TrapFrame {
    TrapFrame {
        rax: 0,
        rbx: 0,
        rcx: 0,
        rdx: 0,
        rsi: 0,
        rdi: 0,
        rbp: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
        rip: 0,
        rflags: 0,
        rsp: 0,
        kernel_sp: 0,
        _pad: 0,
    }
}

/// Build a thread child's frame from the cloning parent's (docs/LINUX-COMPAT.md
/// L4): same code/return point (`rip` = the post-`syscall` RIP the parent
/// saved), same kernel stack, a new user stack, and `rax = 0` so `clone`
/// returns 0 in the child. The TLS base is programmed separately (x86-64 keeps
/// it in the FS_BASE MSR, reloaded per context switch), so `tls` is unused here.
pub fn clone_child_frame(parent: &TrapFrame, child_sp: u64, _tls: u64) -> TrapFrame {
    let mut f = *parent;
    f.rsp = child_sp;
    f.rax = 0;
    f
}

/// The XSAVE component mask (an XCR0 value) the kernel enabled **and validated**
/// at boot, or 0 when XSAVE/AVX is unavailable (then FXSAVE/SSE is used). Set
/// once in `paging_kernel_init`, before any cell or thread runs; read on every
/// FP save/restore. This is the "adapt to the hardware, fall back gracefully"
/// hinge (docs/TILES.md 4): on a CPU with AVX-512 it saves the ZMM state, on an
/// SSE-only CPU it is 0 and the FXSAVE path runs.
static mut XSAVE_MASK: u64 = 0;

/// The validated XSAVE mask (0 = FXSAVE/SSE only). Also the honest report of
/// which FP/SIMD widths the kernel turned on for U-mode.
pub fn fp_xsave_mask() -> u64 {
    // SAFETY: written once at boot before any cell/thread runs; single CPU.
    unsafe { *core::ptr::addr_of!(XSAVE_MASK) }
}

/// Portable `SIMD_*` tier mask a cell reads (docs/TILES.md 4): the widths the
/// kernel **enabled and validated**, not merely what CPUID claims. A tier is
/// reported only if CPUID has it AND its XSAVE state component is in the boot
/// mask - so a cell never dispatches to a path whose registers the kernel would
/// not save across a cell switch.
pub fn fp_simd_tiers() -> u64 {
    use crate::abi::{SIMD_AVX2, SIMD_AVX512F, SIMD_AVX512VNNI, SIMD_SSE2};
    let mask = fp_xsave_mask();
    let avx_ok = mask & (1 << 2) != 0; // XCR0.AVX (YMM state saved)
    let avx512_ok = mask & ((1 << 5) | (1 << 6) | (1 << 7)) == (1 << 5) | (1 << 6) | (1 << 7);
    let l7 = core::arch::x86_64::__cpuid_count(7, 0);
    let mut t = SIMD_SSE2; // the hard-float x86 baseline
    if avx_ok && l7.ebx & (1 << 5) != 0 {
        t |= SIMD_AVX2;
    }
    if avx512_ok && l7.ebx & (1 << 16) != 0 {
        t |= SIMD_AVX512F;
        if l7.ecx & (1 << 11) != 0 {
            t |= SIMD_AVX512VNNI;
        }
    }
    t
}

/// Save the live U-mode FP/SIMD state for a context switch (docs/LINUX-COMPAT.md
/// L4, docs/TILES.md 4). Uses XSAVE with the validated mask when AVX/AVX-512 is
/// enabled (saving YMM/ZMM), else the 512-byte FXSAVE image (SSE). The kernel is
/// soft-float, so the registers still hold the switched-away context's values.
///
/// # Safety
/// `area` must point to at least `FP_AREA_LEN` writable bytes, 64-byte aligned
/// for the XSAVE path (16 suffices for FXSAVE).
pub unsafe fn save_user_fp(area: *mut u8) {
    let mask = fp_xsave_mask();
    unsafe {
        if mask != 0 {
            asm!("xsave [{p}]", p = in(reg) area,
                 in("eax") mask as u32, in("edx") (mask >> 32) as u32, options(nostack));
        } else {
            asm!("fxsave [{p}]", p = in(reg) area, options(nostack));
        }
    }
}

/// Restore U-mode FP/SIMD state saved by [`save_user_fp`] (matching XSAVE/FXSAVE
/// per the validated mask).
///
/// # Safety
/// `area` must point to a valid image written by `save_user_fp` (or a clean one
/// from `fp_area_init`), same alignment rules.
pub unsafe fn restore_user_fp(area: *const u8) {
    let mask = fp_xsave_mask();
    unsafe {
        if mask != 0 {
            asm!("xrstor [{p}]", p = in(reg) area,
                 in("eax") mask as u32, in("edx") (mask >> 32) as u32, options(nostack, readonly));
        } else {
            asm!("fxrstor [{p}]", p = in(reg) area, options(nostack, readonly));
        }
    }
}

/// Bytes reserved per cell for a saved U-mode FP/SIMD image. Sized for the
/// widest x86 save format (an XSAVE area with AVX-512 is ~2.5 KiB); the current
/// FXSAVE image uses the first 512 bytes. 64-aligned when the holder aligns it.
pub const FP_AREA_LEN: usize = 4096;

/// Initialize a cell's FP save area to a clean FXSAVE image: x87 control word
/// 0x037F, MXCSR 0x1F80 (all exceptions masked, round-to-nearest even), every
/// register zero. A freshly-spawned cell has never had its FP state saved, so
/// its area would otherwise be all zeros - and `fxrstor` of a zero area loads
/// MXCSR=0, which *unmasks* every SIMD exception and faults on the first FP op.
/// This writes the ABI-default state a process expects at entry instead.
///
/// # Safety
/// `area` must point to at least `FP_AREA_LEN` writable, 16-byte-aligned bytes.
pub unsafe fn fp_area_init(area: *mut u8) {
    unsafe {
        core::ptr::write_bytes(area, 0, FP_AREA_LEN);
        // FXSAVE layout: FCW at offset 0 (u16), MXCSR at offset 24 (u32).
        (area as *mut u16).write(0x037F);
        (area.add(24) as *mut u32).write(0x1F80);
    }
}

/// (syscall number in rax, arguments a0..a5). Linux-style argument
/// registers: rdi, rsi, rdx, r10, r8, r9 (r10 not rcx, which `syscall`
/// clobbers).
pub fn decode_syscall(frame: &TrapFrame) -> (u64, [u64; 6]) {
    (
        frame.rax,
        [
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ],
    )
}

pub fn set_syscall_ret(frame: &mut TrapFrame, value: u64) {
    frame.rax = value;
}

/// Set the U-mode thread-pointer base for the Linux personality's
/// `arch_prctl(ARCH_SET_FS)` (docs/LINUX-COMPAT.md L1). x86-64 keeps the TLS
/// base in the FS_BASE MSR, which ring 3 cannot write without FSGSBASE, so
/// glibc asks the kernel. The kernel never uses FS, so programming the MSR
/// once at glibc startup persists across this cell's syscalls. (Per-context
/// reload on a cross-cell switch arrives with threads, L4.)
pub fn set_user_fs_base(addr: u64) {
    // SAFETY: a plain MSR write; FS is unused by the kernel.
    unsafe { paging_wrmsr(0xC000_0100, addr) }
}

/// Read back the FS_BASE MSR for `arch_prctl(ARCH_GET_FS)`.
pub fn user_fs_base() -> u64 {
    // SAFETY: a plain MSR read.
    unsafe { paging_rdmsr(0xC000_0100) }
}

// ---------------------------------------------------- signal frame (L5)

/// x86-64 uses the caller-supplied `sa_restorer` (glibc always passes one), so
/// no kernel trampoline page is injected. `SIGTRAMP_VA` is unused here (0) and
/// `sig_tramp_code` is empty; the ARM64/RISC-V ports inject a 2-instruction
/// `rt_sigreturn` page instead (docs/LINUX-COMPAT.md L5).
pub const SIGTRAMP_VA: usize = 0;

/// The x86-64 kernel `struct sigaction` carries an `sa_restorer` field (glibc
/// supplies one), so the personality reads/writes it (docs/LINUX-COMPAT.md L5).
pub const SIGACTION_HAS_RESTORER: bool = true;

/// No injected trampoline on x86-64 (see `SIGTRAMP_VA`).
pub fn sig_tramp_code() -> &'static [u8] {
    &[]
}

/// The interrupted user stack pointer (for building a signal frame, L5).
pub fn user_sp(frame: &TrapFrame) -> u64 {
    frame.rsp
}

/// The kernel stack pointer saved in `frame` (loaded on trap entry). `execve`
/// reuses it when building the new image's entry frame (docs/LINUX-COMPAT.md L6).
pub fn trapframe_kernel_sp(frame: &TrapFrame) -> usize {
    frame.kernel_sp as usize
}

// rt_sigframe layout on the user stack (docs/LINUX-COMPAT.md L5):
//   base+0   pretcode (restorer)
//   base+8   ucontext { uc_flags, uc_link, uc_stack[24], uc_mcontext[256],
//                       uc_sigmask(8) }
//   base+320 siginfo (128)
// The mcontext GPR offsets match Linux `struct sigcontext_64`, so a
// SA_SIGINFO handler that inspects `uc_mcontext.gregs` sees the real layout.
const UC_OFF: u64 = 8;
const MC_OFF: u64 = UC_OFF + 40; // uc_mcontext within the frame
const MASK_OFF: u64 = MC_OFF + 256; // uc_sigmask within the frame
const INFO_OFF: u64 = MASK_OFF + 8; // siginfo within the frame
const FRAME_RESERVE: u64 = 512;

// sigcontext_64 GPR offsets (relative to MC_OFF).
const SC_R8: u64 = 0;
const SC_RDI: u64 = 64;
const SC_RSI: u64 = 72;
const SC_RBP: u64 = 80;
const SC_RBX: u64 = 88;
const SC_RDX: u64 = 96;
const SC_RAX: u64 = 104;
const SC_RCX: u64 = 112;
const SC_RSP: u64 = 120;
const SC_RIP: u64 = 128;
const SC_EFLAGS: u64 = 136;

/// Build a Linux `rt_sigframe` on the user stack and rewrite `frame` to enter
/// the handler (docs/LINUX-COMPAT.md L5). The cell's address space is active
/// (trap context), so the user VAs are written directly.
///
/// # Safety
/// `spec.stack_top` must be a valid, writable user stack VA in the active cell.
pub fn setup_rt_frame(frame: &mut TrapFrame, spec: &super::SigFrameSpec) {
    // 16-align, then bias by -8 so the handler sees rsp % 16 == 8 (the
    // post-`call` alignment the SysV ABI requires at function entry).
    let base = ((spec.stack_top - FRAME_RESERVE) & !0xF) - 8;
    // Offset of the mcontext relative to `base` (the `w` closure adds `base`).
    let mc = MC_OFF;
    // SAFETY: [base, base+FRAME_RESERVE) is writable user stack in the active cell.
    unsafe {
        let w = |off: u64, v: u64| ((base + off) as *mut u64).write(v);
        w(0, spec.restorer); // pretcode
        // ucontext header (uc_flags/uc_link/uc_stack) left zeroed.
        w(UC_OFF, 0);
        w(UC_OFF + 8, 0);
        // mcontext GPRs, from the interrupted frame.
        w(mc + SC_R8, frame.r8);
        w(mc + SC_R8 + 8, frame.r9);
        w(mc + SC_R8 + 16, frame.r10);
        w(mc + SC_R8 + 24, frame.r11);
        w(mc + SC_R8 + 32, frame.r12);
        w(mc + SC_R8 + 40, frame.r13);
        w(mc + SC_R8 + 48, frame.r14);
        w(mc + SC_R8 + 56, frame.r15);
        w(mc + SC_RDI, frame.rdi);
        w(mc + SC_RSI, frame.rsi);
        w(mc + SC_RBP, frame.rbp);
        w(mc + SC_RBX, frame.rbx);
        w(mc + SC_RDX, frame.rdx);
        w(mc + SC_RAX, frame.rax);
        w(mc + SC_RCX, frame.rcx);
        w(mc + SC_RSP, frame.rsp);
        w(mc + SC_RIP, frame.rip);
        w(mc + SC_EFLAGS, frame.rflags);
        w(MASK_OFF, spec.saved_mask);
        // siginfo: si_signo, si_errno, si_code, then si_addr.
        ((base + INFO_OFF) as *mut i32).write(spec.signo as i32);
        ((base + INFO_OFF + 4) as *mut i32).write(0);
        ((base + INFO_OFF + 8) as *mut i32).write(spec.si_code);
        ((base + INFO_OFF + 16) as *mut u64).write(spec.si_addr);
    }
    frame.rsp = base;
    frame.rip = spec.handler;
    frame.rdi = spec.signo as u64; // arg0: signo
    frame.rsi = base + INFO_OFF; // arg1: siginfo*
    frame.rdx = base + UC_OFF; // arg2: ucontext*
    frame.rax = 0;
}

/// Restore a `TrapFrame` saved by [`setup_rt_frame`] on `rt_sigreturn`
/// (docs/LINUX-COMPAT.md L5) and return the saved signal mask. At the
/// `rt_sigreturn` syscall the user rsp points at the ucontext (the handler
/// `ret`ed past the pretcode), so the mcontext is at `rsp + 40`.
pub fn restore_rt_frame(frame: &mut TrapFrame) -> u64 {
    let uc = frame.rsp;
    let mc = uc + 40;
    // SAFETY: `uc` is the ucontext VA in the active cell, written by setup_rt_frame.
    unsafe {
        let r = |off: u64| ((mc + off) as *const u64).read();
        frame.r8 = r(SC_R8);
        frame.r9 = r(SC_R8 + 8);
        frame.r10 = r(SC_R8 + 16);
        frame.r11 = r(SC_R8 + 24);
        frame.r12 = r(SC_R8 + 32);
        frame.r13 = r(SC_R8 + 40);
        frame.r14 = r(SC_R8 + 48);
        frame.r15 = r(SC_R8 + 56);
        frame.rdi = r(SC_RDI);
        frame.rsi = r(SC_RSI);
        frame.rbp = r(SC_RBP);
        frame.rbx = r(SC_RBX);
        frame.rdx = r(SC_RDX);
        frame.rax = r(SC_RAX);
        frame.rcx = r(SC_RCX);
        frame.rsp = r(SC_RSP);
        frame.rip = r(SC_RIP);
        frame.rflags = r(SC_EFLAGS);
        ((uc + (MASK_OFF - UC_OFF)) as *const u64).read()
    }
}

unsafe extern "C" {
    pub fn enter_user_first(frame: *mut TrapFrame);
    fn return_to_kernel_asm() -> !;
}

pub fn return_to_kernel() -> ! {
    // SAFETY: only called while a cell is running (inside enter_user_first).
    unsafe { return_to_kernel_asm() }
}

/// Called from user.S on a SYSCALL. Returns the frame to resume, or null
/// to unwind (the stub jumps to return_to_kernel_asm on null).
#[unsafe(no_mangle)]
extern "C" fn x86_user_trap(kind: u64, fault_addr: u64, frame: *mut TrapFrame) -> *mut TrapFrame {
    let k = if kind == 0 {
        super::TrapKind::Syscall
    } else {
        super::TrapKind::Fault
    };
    // The syscall path never faults; FaultCause is a placeholder here (only
    // read when kind == Fault, which comes through `x86_fault_trap`).
    crate::user::on_user_trap(k, super::FaultCause::Segv, fault_addr as usize, frame)
}

/// How many CPUs can hold ring-3 state at once.
///
/// The four words the trap stubs touch, the GDT, the TSS and the syscall kernel
/// stack are all **per-CPU** - two cores running cells would otherwise scribble on
/// each other's saved user stack pointer and current-frame pointer, which is not a
/// fault but a wrong register file (docs/SMP.md 10.0). A power of two, so the slot
/// is a mask on the CPU's own APIC id and an id outside the range aliases slot 0
/// rather than running off the array. Matches `KERNEL_CTX_SLOTS` on the other two
/// ISAs; lifting it is part of the start-all-cores loop, not of this phase.
const CPU_SLOTS: usize = 8;

/// This CPU's slot, read from its **own** local APIC - no table, no argument, and
/// nothing to keep live across ring 3. The assembly stubs reach the same slot
/// through `GS_BASE`, which is why the two must agree; `user_init` is the single
/// place that programs it.
fn cpu_slot() -> usize {
    (lapic_id() as usize) & (CPU_SLOTS - 1)
}

/// The words the trap stubs reach `GS`-relative. Offsets are fixed by
/// `kernel/arch/x86_64/user.S` (`CPU_*`), so the order here is load-bearing.
///
/// `GS_BASE` points at this CPU's block **in kernel and in user mode alike**, and
/// there is deliberately no `swapgs`: nothing in this tree ever gives a cell a GS
/// base (`arch_prctl(ARCH_SET_GS)` is refused `-EINVAL`, and both other ISAs carry
/// TLS elsewhere), so the register is the kernel's to keep. A cell that did read
/// `%gs:` would fault on a supervisor-only page - which is what it already did when
/// the base was 0 and the address was unmapped. Adopting `swapgs` later is a change
/// to two instructions at each ring boundary and to nothing else here.
#[repr(C, align(64))]
struct CpuArea {
    user_rsp_scratch: u64, // +0
    kernel_rsp: u64,       // +8
    cur_frame: u64,        // +16
    kernel_ctx: u64,       // +24
}

static mut CPU_AREAS: [CpuArea; CPU_SLOTS] = [const {
    CpuArea {
        user_rsp_scratch: 0,
        kernel_rsp: 0,
        cur_frame: 0,
        kernel_ctx: 0,
    }
}; CPU_SLOTS];

// ---------------------------------------------------------- GDT/TSS/MSRs

#[repr(C, packed)]
struct Tss {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

static mut TSS: [Tss; CPU_SLOTS] = [const {
    Tss {
        reserved0: 0,
        rsp: [0; 3],
        reserved1: 0,
        ist: [0; 7],
        reserved2: 0,
        reserved3: 0,
        iomap_base: 0,
    }
}; CPU_SLOTS];

// GDT: null, kernel code64, kernel data, user data, user code64, TSS (2).
// Per CPU, because the TSS descriptor in it names that CPU's own TSS - and because
// `ltr` marks the descriptor busy, so two cores loading one descriptor is a fault.
static mut GDT: [[u64; 7]; CPU_SLOTS] = [[0; 7]; CPU_SLOTS];

#[repr(C, packed)]
struct DescPtr {
    limit: u16,
    base: u64,
}

// The SYSCALL dispatch runs on this stack. Unlike a hardware interrupt/
// exception (which the CPU auto-aligns to 16 bytes when loading RSP from the
// TSS), SYSCALL does not touch RSP, so KERNEL_RSP must itself be 16-byte
// aligned or the SysV ABI is violated and SSE spills in the Rust dispatch
// (core::fmt) corrupt. A bare `[u8; _]` is only align-1, so its top address
// was 16-aligned only by luck of the .bss layout - any code motion shifting
// it to an odd offset re-triggered the corruption. Force the alignment.
#[repr(align(16))]
struct SyscallKStack([u8; 64 * 1024]);
static mut SYSCALL_KSTACK: [SyscallKStack; CPU_SLOTS] =
    [const { SyscallKStack([0; 64 * 1024]) }; CPU_SLOTS];

/// Set up ring 3: a full GDT with user segments and a TSS, then the
/// SYSCALL/SYSRET MSRs. Called from paging_kernel_init.
pub(super) fn user_init() {
    let slot = cpu_slot();
    unsafe {
        // GS_BASE first: every stub below reaches this CPU's block through it, and
        // nothing between here and the first ring-3 entry may run without it.
        let area = core::ptr::addr_of_mut!(CPU_AREAS[slot]);
        paging_wrmsr(0xC000_0101, area as u64);

        let kstack_top = core::ptr::addr_of!(SYSCALL_KSTACK[slot].0) as u64 + (64 * 1024);
        (*area).kernel_rsp = kstack_top;

        let tss = &mut *core::ptr::addr_of_mut!(TSS[slot]);
        tss.rsp[0] = kstack_top; // ring 3 -> ring 0 fault stack
        tss.iomap_base = core::mem::size_of::<Tss>() as u16;

        let gdt = &mut *core::ptr::addr_of_mut!(GDT[slot]);
        gdt[0] = 0;
        gdt[1] = 0x00AF_9A00_0000_FFFF; // 0x08 kernel code64
        gdt[2] = 0x00CF_9200_0000_FFFF; // 0x10 kernel data
        gdt[3] = 0x00CF_F200_0000_FFFF; // 0x18 user data (DPL3)
        gdt[4] = 0x00AF_FA00_0000_FFFF; // 0x20 user code64 (DPL3)
        let tss_base = core::ptr::addr_of!(TSS[slot]) as u64;
        let tss_limit = (core::mem::size_of::<Tss>() - 1) as u64;
        gdt[5] = tss_limit
            | ((tss_base & 0xFF_FFFF) << 16)
            | (0x89u64 << 40)
            | (((tss_limit >> 16) & 0xF) << 48)
            | (((tss_base >> 24) & 0xFF) << 56);
        gdt[6] = tss_base >> 32;

        let gdt_ptr = DescPtr {
            limit: (core::mem::size_of::<[u64; 7]>() - 1) as u16,
            base: core::ptr::addr_of!(GDT[slot]) as u64,
        };
        // Load the GDT, reload CS via a far return, set the data segments,
        // and load the task register.
        asm!(
            "lgdt [{ptr}]",
            "push 0x08",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ax, 0x10",
            "mov ds, ax",
            "mov es, ax",
            "mov ss, ax",
            "mov ax, 0x28",
            "ltr ax",
            ptr = in(reg) &gdt_ptr,
            tmp = out(reg) _,
            out("ax") _,
        );

        // Enable SSE for U-mode (docs/LINUX-COMPAT.md L1): CR0.MP set,
        // CR0.EM clear (no x87 emulation), CR0.TS clear; CR4.OSFXSR (fxsave
        // area valid) + CR4.OSXMMEXCPT. glibc's SSE2 `memcpy`/`str*` ifunc
        // variants are the x86-64 baseline and fault without this. AVX/AVX-512
        // are enabled just below when CPUID reports them (docs/TILES.md 4); FP
        // state is saved/restored across cell switches (the kernel is
        // soft-float, so ring-3 owns the FP/vector registers).
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        cr0 = (cr0 | (1 << 1)) & !(1 << 2) & !(1 << 3);
        asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack));
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        cr4 |= (1 << 9) | (1 << 10);
        asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));

        // Enable AVX/AVX-512 for U-mode when the hardware has it (docs/TILES.md
        // 4): a hard-float cell runtime-dispatches to AVX2/AVX-512/VNNI tile
        // kernels, whose wider register state the kernel then saves/restores
        // across cell switches with XSAVE. Gated on CPUID, so a CPU without
        // XSAVE/AVX keeps the FXSAVE/SSE path (graceful fallback); the kernel
        // itself stays soft-float and only enables the feature for ring 3. A
        // boot health check reads XCR0 back and records only the bits that
        // actually stuck as the save/restore mask - honest about what the
        // hardware (or hypervisor) really honored.
        let l1 = core::arch::x86_64::__cpuid_count(1, 0);
        let l7 = core::arch::x86_64::__cpuid_count(7, 0);
        let have_xsave = l1.ecx & (1 << 26) != 0;
        let have_avx = l1.ecx & (1 << 28) != 0;
        let have_avx512 = l7.ebx & (1 << 16) != 0;
        if have_xsave && have_avx {
            // CR4.OSXSAVE (bit 18): enable XSAVE and XGETBV/XSETBV.
            cr4 |= 1 << 18;
            asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));
            // XCR0 = x87 (0) | SSE (1) | AVX (2), plus the three AVX-512 state
            // components (opmask 5, ZMM_Hi256 6, Hi16_ZMM 7) when present.
            let mut want: u64 = 0b111;
            if have_avx512 {
                want |= (1 << 5) | (1 << 6) | (1 << 7);
            }
            asm!("xsetbv", in("ecx") 0, in("eax") want as u32, in("edx") (want >> 32) as u32,
                 options(nomem, nostack));
            // Read XCR0 back and keep only the bits that took, so a component the
            // platform silently dropped never leads to a mismatched xrstor.
            let (lo, hi): (u32, u32);
            asm!("xgetbv", in("ecx") 0, out("eax") lo, out("edx") hi, options(nomem, nostack));
            let got = ((hi as u64) << 32) | lo as u64;
            *core::ptr::addr_of_mut!(XSAVE_MASK) = got & want;
        }

        // EFER.SCE (enable SYSCALL); NXE was set in paging_kernel_init.
        let efer = paging_rdmsr(0xC000_0080) | 1;
        paging_wrmsr(0xC000_0080, efer);
        // STAR: SYSCALL loads CS=0x08/SS=0x10; SYSRET base 0x10 -> user
        // SS=0x18, CS=0x20.
        paging_wrmsr(0xC000_0081, (0x10u64 << 48) | (0x08u64 << 32));
        // LSTAR: the SYSCALL entry point.
        paging_wrmsr(0xC000_0082, syscall_entry as *const () as u64);
        // SFMASK: clear IF and DF on entry.
        paging_wrmsr(0xC000_0084, 0x600);
    }
}

unsafe extern "C" {
    fn syscall_entry();
}

unsafe fn paging_rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nostack));
    }
    ((hi as u64) << 32) | lo as u64
}

unsafe fn paging_wrmsr(msr: u32, value: u64) {
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack),
        );
    }
}

// -------------------------------------------------------------- counters

pub fn cycles() -> u64 {
    let lo: u32;
    let hi: u32;
    // lfence keeps rdtsc from being reordered around the measured code.
    unsafe { asm!("lfence", "rdtsc", out("eax") lo, out("edx") hi) };
    ((hi as u64) << 32) | lo as u64
}

/// Convert `cycles()` (TSC ticks) to nanoseconds for the Linux personality's
/// `clock_gettime` (docs/LINUX-COMPAT.md L2). Uses CPUID leaf 0x16 (processor
/// base frequency in MHz) when the CPU reports it; otherwise a documented
/// 1 GHz assumption. glibc reads this only for coarse timing, so exact TSC
/// frequency is not load-bearing for the fixtures.
pub fn ticks_to_ns(ticks: u64) -> u64 {
    ((ticks as u128 * 1_000_000_000) / tsc_hz() as u128) as u64
}

/// The TSC frequency in Hz, read once from CPUID leaf 0x16 and cached. Cached
/// because the timer arbiter converts a TSC reading to nanoseconds on every
/// deadline check, including inside the receive path's bounded busy-poll tier
/// (docs/NETSTACK.md 16), and CPUID is a VM exit / serialising instruction.
fn tsc_hz() -> u64 {
    static TSC_HZ: AtomicU64 = AtomicU64::new(0);
    let cached = TSC_HZ.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    use core::arch::x86_64::__cpuid_count;
    let max_leaf = __cpuid_count(0, 0).eax;
    let mhz = if max_leaf >= 0x16 {
        __cpuid_count(0x16, 0).eax as u64
    } else {
        0
    };
    let hz = if mhz != 0 {
        mhz * 1_000_000
    } else {
        1_000_000_000
    };
    TSC_HZ.store(hz, Ordering::Relaxed);
    hz
}

/// Calibration loop with a known instruction count: exactly 2
/// instructions per iteration (dec + jnz). Benchmarks use it to convert
/// counter ticks into approximate instruction counts under QEMU -icount.
pub fn spin_loop(iters: u64) {
    if iters == 0 {
        return;
    }
    let mut n = iters;
    unsafe {
        asm!(
            "2:",
            "dec {0}",
            "jnz 2b",
            inout(reg) n,
            options(nomem, nostack),
        )
    };
    let _ = n;
}

// -------------------------------------------------------- context switch

unsafe extern "C" {
    fn context_switch_asm(old_sp: *mut usize, new_sp: *const usize);
}

/// Switch from the current context (saved into `old`) to `new`.
///
/// # Safety
/// `new` must have been produced by `context_init` or a prior switch, and
/// its stack must still be alive.
pub unsafe fn context_switch(old: &mut super::Context, new: &super::Context) {
    unsafe { context_switch_asm(&mut old.sp, &new.sp) };
}

/// Prime a fresh stack so the first switch into it enters `entry`.
/// Frame layout must match context_switch.S: 6 callee-saved registers,
/// then the return address; an extra 8 bytes keeps the SysV entry
/// alignment (rsp % 16 == 8 at function entry).
///
/// # Safety
/// `stack_top` must be the 16-aligned top of a stack of adequate size.
pub unsafe fn context_init(stack_top: *mut u8, entry: extern "C" fn() -> !) -> super::Context {
    unsafe {
        let sp = stack_top.sub(64) as *mut u64;
        for i in 0..6 {
            sp.add(i).write(0); // r15, r14, r13, r12, rbx, rbp
        }
        sp.add(6).write(entry as usize as u64); // return address
        super::Context { sp: sp as usize }
    }
}

// ------------------------------------------------------------------ exit

/// isa-debug-exit at port 0xF4: QEMU exits with (value << 1) | 1,
/// so Success -> 33 and Failure -> 35. The xtask harness knows these.
pub fn exit(code: super::ExitCode) -> ! {
    let value: u32 = match code {
        super::ExitCode::Success => 0x10,
        super::ExitCode::Failure => 0x11,
    };
    unsafe {
        outl(0xF4, value);
    }
    // Only reached without the exit device (e.g. interactive run).
    loop {
        unsafe { asm!("hlt") };
    }
}
