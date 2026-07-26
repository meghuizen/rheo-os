//! x86-64: PVH boot entry, 16550 UART on port 0x3F8, isa-debug-exit,
//! IDT-based traps, rdtsc, and the context-switch stub.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

/// Linux personality ABI (x86-64 legacy syscall table; docs/LINUX-COMPAT.md).
pub mod linux_abi;
mod paging;
pub use paging::{
    PagingRoot, paging_activate, paging_activate_kernel, paging_for_each_user_leaf,
    paging_kernel_init, paging_map, paging_map_frame, paging_new_root, paging_protect,
    paging_unmap_frame,
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

// ----------------------------------- console input + timer interrupt seam
// docs/LIBRHEO.md Phase D/F. **Timer: interrupt-driven** (the kernel's second
// interrupt). q35's per-CPU LAPIC, driven in **x2APIC** mode (MSR access, 0x800+
// - no MMIO mapping, and EOI works regardless of which page-table root is active
// during the interrupt), delivers a one-shot **LVT timer** on vector 0x20. Cells
// run with IF clear (their TrapFrame RFLAGS has no IF); the kernel sets IF only
// inside the `sti; hlt` idle idiom, so `SYS_ARM_TIMER` is a genuine 0%-CPU park.
//
// **UART RX: poll** (honest). q35 routes COM1's ISA IRQ4 through the emulated
// IOAPIC, but under QEMU's TCG + `kernel-irqchip=split` the LAPIC's ISR/IRR are
// not modeled (they read 0) and IPIs are not delivered, so an IOAPIC-routed line
// delivers the first byte but does not reliably re-trigger, and the self-IPI the
// RISC-V port uses as its deterministic-test analog does not fire at all. Rather
// than fake it, x86-64 keeps the poll path (kernel/src/input.rs). The GICv3-style
// per-source ack the RISC-V AIA gives is what makes RISC-V's UART RX reliable;
// the honest x86-64 result is timer-only. Opt-in (`enable_timer_irq`, called only
// by the Phase F test), so no other kernel is affected.

/// x2APIC MSRs (Intel SDM vol 3): APIC base, spurious-vector, EOI, and the
/// LVT-timer trio.
const MSR_APIC_BASE: u32 = 0x1B;
const MSR_X2APIC_SVR: u32 = 0x80F;
const MSR_X2APIC_EOI: u32 = 0x80B;
const MSR_X2APIC_LVT_TIMER: u32 = 0x832;
const MSR_X2APIC_TIMER_INIT: u32 = 0x838;
const MSR_X2APIC_TIMER_CUR: u32 = 0x839;
const MSR_X2APIC_TIMER_DIV: u32 = 0x83E;

/// Chosen interrupt vectors: LAPIC timer 0x20, LAPIC spurious 0xFF (above the 32
/// CPU-exception vectors).
const VEC_TIMER: usize = 0x20;
const VEC_SPURIOUS: usize = 0xFF;

static mut TIMER_ENABLED: bool = false;

unsafe extern "C" {
    fn timer_irq_stub();
    fn spurious_irq_stub();
}

/// Whether the UART RX interrupt is wired (false = poll path). x86-64 polls (see
/// the seam comment: the QEMU TCG IOAPIC/LAPIC does not re-deliver reliably).
pub fn uart_irq_enabled() -> bool {
    false
}
/// Bring up the UART RX interrupt - x86-64 stays on the poll path (see the seam
/// comment). Called only by the Phase D test.
pub fn enable_uart_rx_irq() {}
/// Halt until the UART RX interrupt (only called when `uart_irq_enabled`, i.e.
/// never on x86-64).
pub fn idle_wait() {}
/// Deliver a scripted byte through the UART RX interrupt (only called when
/// `uart_irq_enabled`, i.e. never on x86-64).
pub fn uart_inject_and_wait(_b: u8) {}

/// Whether the LAPIC timer interrupt is wired (false = busy-wait path).
pub fn timer_irq_enabled() -> bool {
    // SAFETY: single CPU; set once before any cell runs.
    unsafe { *core::ptr::addr_of!(TIMER_ENABLED) }
}

/// Enable the LAPIC in x2APIC mode (MSR access) + set the spurious vector. Shared
/// bring-up for the timer path.
fn lapic_init() {
    // SAFETY: kernel context; plain MSR writes.
    unsafe {
        // Enable x2APIC: IA32_APIC_BASE |= EN (11) | EXTD (10).
        let base = paging_rdmsr(MSR_APIC_BASE) | (1 << 11) | (1 << 10);
        paging_wrmsr(MSR_APIC_BASE, base);
        // Software-enable the LAPIC + set the spurious vector (SVR bit 8 + 0xFF).
        paging_wrmsr(MSR_X2APIC_SVR, 0x100 | VEC_SPURIOUS as u64);
        set_idt_gate(VEC_SPURIOUS, spurious_irq_stub as *const () as u64);
    }
}

/// Bring up the LAPIC one-shot timer interrupt (vector 0x20). Called only by the
/// Phase F timer test.
pub fn enable_timer_irq() {
    lapic_init();
    set_idt_gate(VEC_TIMER, timer_irq_stub as *const () as u64);
    // SAFETY: kernel context; x2APIC enabled by lapic_init.
    unsafe {
        // Divide config = 1 (bits: 0b1011 -> divide by 1).
        paging_wrmsr(MSR_X2APIC_TIMER_DIV, 0b1011);
        // LVT timer: vector 0x20, one-shot (bits 17-18 = 0), unmasked.
        paging_wrmsr(MSR_X2APIC_LVT_TIMER, VEC_TIMER as u64);
        paging_wrmsr(MSR_X2APIC_TIMER_INIT, 0); // disarmed until timer_wait
        *core::ptr::addr_of_mut!(TIMER_ENABLED) = true;
    }
}

/// The LAPIC timer interrupt handler (called from `timer_irq_stub`): the
/// one-shot has fired, so just EOI; the waiter in `timer_wait` observes the
/// elapsed deadline and returns.
#[unsafe(no_mangle)]
extern "C" fn x86_timer_irq() {
    // SAFETY: kernel context; x2APIC EOI is a plain MSR write.
    unsafe { paging_wrmsr(MSR_X2APIC_EOI, 0) };
}

/// Arm the LAPIC one-shot timer for `deadline_ns` from now and halt at `hlt`
/// until it fires (a genuine 0%-CPU park). The LAPIC timer counts the APIC bus
/// clock; we calibrate its rate against the TSC (`cycles()` + `ticks_to_ns`)
/// once, then convert. Called only when [`timer_irq_enabled`].
pub fn timer_wait(deadline_ns: u64) {
    let count = lapic_timer_count(deadline_ns);
    // SAFETY: kernel context; x2APIC timer MSRs, one-shot, IF via the idle idiom.
    unsafe {
        paging_wrmsr(MSR_X2APIC_TIMER_INIT, count as u64); // arm (starts counting down)
        // The one-shot fires once (current count hits 0); wake hlt and EOI. Loop
        // guards against an unrelated interrupt waking hlt early.
        while paging_rdmsr(MSR_X2APIC_TIMER_CUR) != 0 {
            asm!("sti; hlt; cli", options(nomem, nostack));
        }
        paging_wrmsr(MSR_X2APIC_TIMER_INIT, 0); // disarm
    }
}

/// Calibrate the LAPIC timer's tick rate against the TSC and return the initial
/// count for `deadline_ns`. Done once (the ratio is stable under QEMU), cached.
fn lapic_timer_count(deadline_ns: u64) -> u32 {
    static CAL_PPN: AtomicU64 = AtomicU64::new(0); // LAPIC ticks per 1e6 ns, *1024
    let mut ppn = CAL_PPN.load(Ordering::Relaxed);
    if ppn == 0 {
        // Run the LAPIC timer for a known TSC span and count its ticks.
        // SAFETY: kernel context; timer MSRs + TSC.
        unsafe {
            paging_wrmsr(MSR_X2APIC_TIMER_INIT, 0xFFFF_FFFF);
            let tsc0 = cycles();
            let lc0 = paging_rdmsr(MSR_X2APIC_TIMER_CUR);
            // Busy-spin a bounded TSC interval (~calibration window).
            while cycles().wrapping_sub(tsc0) < 2_000_000 {
                core::hint::spin_loop();
            }
            let lc1 = paging_rdmsr(MSR_X2APIC_TIMER_CUR);
            let tsc1 = cycles();
            paging_wrmsr(MSR_X2APIC_TIMER_INIT, 0);
            let lapic_ticks = lc0.wrapping_sub(lc1); // counts down
            let ns = ticks_to_ns(tsc1.wrapping_sub(tsc0)).max(1);
            // LAPIC ticks per ns, scaled by 1<<20 for integer precision.
            ppn = (((lapic_ticks as u128) << 20) / ns as u128) as u64;
            if ppn == 0 {
                ppn = 1;
            }
            CAL_PPN.store(ppn, Ordering::Relaxed);
        }
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
        let pointer = IdtPointer {
            limit: (core::mem::size_of::<IdtEntry>() * IDT_ENTRIES - 1) as u16,
            base: core::ptr::addr_of!(IDT) as u64,
        };
        asm!("lidt [{}]", in(reg) &pointer);
    }
    mask_legacy_pic();
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
// docs/SMP.md, task #27. x86-64 AP bring-up is **not implemented here** and is
// honestly reported as blocked: an application processor starts in 16-bit
// real mode and must be released with an INIT-SIPI-SIPI sequence pointing at a
// trampoline placed below 1 MiB, which then switches to long mode and joins the
// kernel. PVH boot hands us no low real-mode memory or firmware to stage that
// trampoline, and building one cleanly is out of scope for this phase (it must
// not destabilise the single-core boot every other kernel depends on). CPU
// *detection* is done - ACPI MADT already counts the APs in the inventory - so
// the honest deliverable is the count plus a documented blocker. `smp` skips x86
// with this reason and still passes.

/// This CPU's index. x86-64 does not bring up secondaries here, so only the boot
/// processor ever asks - always CPU 0.
#[cfg(feature = "smp")]
pub fn cpu_index() -> usize {
    0
}

/// No-op: no per-CPU identity register is established (single-core path).
#[cfg(feature = "smp")]
pub fn smp_set_this_cpu(_index: usize) {}

/// The bootstrap processor's hardware id (APIC id 0 on this QEMU q35 config).
#[cfg(feature = "smp")]
pub fn boot_cpu_hw_id() -> u32 {
    0
}

/// Report the x86-64 AP-bring-up blocker (docs/SMP.md). No AP is started.
#[cfg(feature = "smp")]
pub fn smp_start_secondary(_hw_id: u32) -> Result<(), &'static str> {
    Err("needs a 16-bit real-mode AP trampoline (INIT-SIPI-SIPI) below 1 MiB; not implemented")
}

/// Feature names; bit i corresponds to index i in CpuReport.features.
pub fn cpu_feature_names() -> &'static [&'static str] {
    &[
        "sse", "sse2", "sse3", "ssse3", "sse4.1", "sse4.2", "avx", "avx2", "avx512f", "aes", "sha",
        "rdrand", "rdseed", "xsave", "fsgsbase", "nx", "pcid", "pdpe1gb", "x2apic",
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
        // Reserved bit 1 only; IF stays *clear*. The kernel has no interrupt
        // handlers yet (the preemption doorbell is future work, CONCURRENCY.md
        // 4) and under PVH there is no firmware to remap the 8259 PIC, so a
        // legacy IRQ (the PIT's IRQ0) would arrive on vector 0x08 and be
        // mistaken for a #DF. Cells are cooperative and trap-driven, so
        // masking interrupts in U-mode is correct until real IRQ handling
        // lands. The PIC is also masked at boot (see mask_legacy_pic).
        rflags: 0x002,
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

/// Save the live U-mode FP/SIMD state (SSE: XMM0-15 + MXCSR + x87) into a
/// 512-byte 16-aligned area, for a cooperative context switch between two
/// threads of one cell (docs/LINUX-COMPAT.md L4). The kernel is soft-float, so
/// the registers still hold the trapped thread's values.
///
/// # Safety
/// `area` must point to at least 512 writable, 16-byte-aligned bytes.
pub unsafe fn save_user_fp(area: *mut u8) {
    unsafe { asm!("fxsave [{p}]", p = in(reg) area, options(nostack)) };
}

/// Restore U-mode FP/SIMD state saved by [`save_user_fp`].
///
/// # Safety
/// `area` must point to a valid 512-byte FXSAVE image (16-aligned).
pub unsafe fn restore_user_fp(area: *const u8) {
    unsafe { asm!("fxrstor [{p}]", p = in(reg) area, options(nostack, readonly)) };
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

// Single CPU: the syscall stub reaches these as plain globals, no GS.
#[unsafe(no_mangle)]
static mut KERNEL_RSP: u64 = 0;
#[unsafe(no_mangle)]
static mut USER_RSP_SCRATCH: u64 = 0;
#[unsafe(no_mangle)]
static mut CUR_FRAME: u64 = 0;
#[unsafe(no_mangle)]
static mut KERNEL_CTX: u64 = 0;

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

static mut TSS: Tss = Tss {
    reserved0: 0,
    rsp: [0; 3],
    reserved1: 0,
    ist: [0; 7],
    reserved2: 0,
    reserved3: 0,
    iomap_base: 0,
};

// GDT: null, kernel code64, kernel data, user data, user code64, TSS (2).
static mut GDT: [u64; 7] = [0; 7];

#[repr(C, packed)]
struct DescPtr {
    limit: u16,
    base: u64,
}

static mut SYSCALL_KSTACK: [u8; 64 * 1024] = [0; 64 * 1024];

/// Set up ring 3: a full GDT with user segments and a TSS, then the
/// SYSCALL/SYSRET MSRs. Called from paging_kernel_init.
pub(super) fn user_init() {
    unsafe {
        let kstack_top = core::ptr::addr_of!(SYSCALL_KSTACK) as u64 + (64 * 1024);
        *core::ptr::addr_of_mut!(KERNEL_RSP) = kstack_top;

        let tss = &mut *core::ptr::addr_of_mut!(TSS);
        tss.rsp[0] = kstack_top; // ring 3 -> ring 0 fault stack
        tss.iomap_base = core::mem::size_of::<Tss>() as u16;

        let gdt = &mut *core::ptr::addr_of_mut!(GDT);
        gdt[0] = 0;
        gdt[1] = 0x00AF_9A00_0000_FFFF; // 0x08 kernel code64
        gdt[2] = 0x00CF_9200_0000_FFFF; // 0x10 kernel data
        gdt[3] = 0x00CF_F200_0000_FFFF; // 0x18 user data (DPL3)
        gdt[4] = 0x00AF_FA00_0000_FFFF; // 0x20 user code64 (DPL3)
        let tss_base = core::ptr::addr_of!(TSS) as u64;
        let tss_limit = (core::mem::size_of::<Tss>() - 1) as u64;
        gdt[5] = tss_limit
            | ((tss_base & 0xFF_FFFF) << 16)
            | (0x89u64 << 40)
            | (((tss_limit >> 16) & 0xF) << 48)
            | (((tss_base >> 24) & 0xFF) << 56);
        gdt[6] = tss_base >> 32;

        let gdt_ptr = DescPtr {
            limit: (core::mem::size_of::<[u64; 7]>() - 1) as u16,
            base: core::ptr::addr_of!(GDT) as u64,
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
        // variants are the x86-64 baseline and fault without this. AVX
        // (XSAVE/XCR0) is intentionally not enabled: QEMU's default CPU does
        // not expose it, so glibc's ifunc resolver stays on SSE2. No FP state
        // save/restore yet (one ring-3 context per cell; kernel is soft-float).
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        cr0 = (cr0 | (1 << 1)) & !(1 << 2) & !(1 << 3);
        asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack));
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        cr4 |= (1 << 9) | (1 << 10);
        asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));

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
    ((ticks as u128 * 1_000_000_000) / hz as u128) as u64
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
