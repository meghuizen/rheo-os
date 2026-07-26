//! RISC-V 64: QEMU virt machine, 16550 UART at 0x1000_0000, sifive_test
//! exit, stvec traps, rdcycle, and the context-switch stub.
//! Runs in S-mode on top of OpenSBI (DEVELOPMENT.md 4).

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

/// Linux personality ABI (asm-generic table, shared with ARM64;
/// docs/LINUX-COMPAT.md).
#[path = "../linux_abi_generic.rs"]
pub mod linux_abi;
mod paging;
pub use paging::{
    PagingRoot, paging_activate, paging_activate_kernel, paging_for_each_user_leaf,
    paging_kernel_init, paging_map, paging_map_frame, paging_new_root, paging_protect,
    paging_unmap_frame,
};
pub use paging::{mmio_map_window, pmem_map_window};

/// `uname` machine string for the Linux personality (docs/LINUX-COMPAT.md L2).
pub const LINUX_UNAME_MACHINE: &str = "riscv64";

/// clone(2) argument order (docs/LINUX-COMPAT.md L4): RISC-V selects
/// `CLONE_BACKWARDS`, so the raw order is `(flags, stack, parent_tid, tls,
/// child_tid)` - `tls` and `child_tid` are swapped relative to x86-64 (glibc's
/// riscv `clone.S` passes tls in a3, child_tid in a4).
pub const CLONE_BACKWARDS: bool = true;

global_asm!(include_str!("../../../arch/riscv64/boot.S"));
global_asm!(include_str!("../../../arch/riscv64/traps.S"));
global_asm!(include_str!("../../../arch/riscv64/context_switch.S"));

pub const NAME: &str = "RISC-V 64";

/// Physical base of the frame pool: 64 MiB into RAM, well above the kernel
/// image (checked against __kernel_end in frames::init).
pub const FRAME_POOL_BASE: usize = 0x8400_0000;

/// Kernel linear-map offset (docs/MEMORY.md): the kernel, all MMIO, and the
/// `.user` window run in the Sv39 high canonical half, so a physical address
/// is reached at `pa | KERNEL_VA_BASE`. The whole low half is left to user
/// programs. The boot trampoline builds this map before any Rust runs, and the
/// kernel is linked at `phys_to_virt(load address)` (link/riscv64.ld). Base of
/// the 39-bit sign-extended high half (top-half level-2 indices 256..511).
pub const KERNEL_VA_BASE: usize = 0xFFFF_FFC0_0000_0000;

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

// MMIO the kernel touches while a cell root is active (the serial UART for
// cell stdout/stdin) must be reachable in that root; the kernel runs high, so
// its base is a high linear-map VA (mapped supervisor in every root). Device
// MMIO used only at boot (test-exit device, virtio, PCIe ECAM) is likewise
// reached high for uniformity.
const UART_BASE: usize = 0x1000_0000 | KERNEL_VA_BASE;
const UART_THR: *mut u8 = UART_BASE as *mut u8; // transmit holding
const UART_LSR: *mut u8 = (UART_BASE + 5) as *mut u8; // line status
const LSR_THRE: u8 = 1 << 5; // transmit holding register empty

pub fn serial_init() {
    // QEMU's 16550 is usable as-is for TX; real init comes with the driver.
}

pub fn serial_write_byte(byte: u8) {
    unsafe {
        while UART_LSR.read_volatile() & LSR_THRE == 0 {}
        UART_THR.write_volatile(byte);
    }
}

const UART_RBR: *mut u8 = UART_BASE as *mut u8; // receive buffer (= THR)
const LSR_DR: u8 = 1 << 0; // data ready

/// Non-blocking read of one byte from the UART, or None if none pending.
pub fn serial_read_byte() -> Option<u8> {
    unsafe {
        if UART_LSR.read_volatile() & LSR_DR == 0 {
            None
        } else {
            Some(UART_RBR.read_volatile())
        }
    }
}

// -------------------------------------------- console input wakeup seam
// docs/LIBRHEO.md Phase D: the kernel's first interrupt. QEMU riscv `virt`
// with `aia=aplic-imsic` delivers the 16550 UART's IRQ (source 10) through the
// **AIA**: the S-mode APLIC (`aplic@d000000`, MSI-delivery mode) raises an MSI
// into the S-mode IMSIC (`imsics@28000000`, one interrupt file per hart, reached
// through the S-mode indirect CSRs siselect/sireg/stopei), which drives
// `sip.SEIP`. We enable the UART's RX-data interrupt (IER bit 0) and take the
// S external interrupt (`scause` = interrupt | 9) in the kernel trap handler,
// where it drains the UART into the kernel RX ring (kernel/src/input.rs).
//
// Interrupts fire only in kernel context: cells run with `sstatus.SIE` clear
// (a U-mode SEI would look like a fault), and we set SIE only briefly, inside
// `idle_wait`/`uart_inject_and_wait`, to service a pending SEI after `wfi` woke
// on it. That makes `SYS_WAIT_INPUT` a genuine 0%-CPU park.

// AIA S-mode indirect CSRs (Ssaia): siselect=0x150, sireg=0x151, stopei=0x15c
// (written as numeric literals in asm so an older assembler without AIA CSR
// names still accepts them).
/// `scause` for a supervisor external interrupt: interrupt bit | cause 9.
const SCAUSE_S_EXT: u64 = (1 << 63) | 9;

/// S-mode APLIC MMIO base (`aplic@d000000`), reached through the high linear map.
const APLIC_S_BASE: usize = 0x0d00_0000 | KERNEL_VA_BASE;
/// The 16550 UART's APLIC interrupt source number (device tree: serial IRQ 10).
const UART_SOURCE: usize = 10;
/// The IMSIC external interrupt identity we route the UART MSI to.
const UART_EIID: u32 = 10;
/// The 16550 IER (offset 1) and MCR (offset 4).
const UART_IER: *mut u8 = (0x1000_0000 | KERNEL_VA_BASE) as *mut u8;
const UART_MCR: *mut u8 = ((0x1000_0000 + 4) | KERNEL_VA_BASE) as *mut u8;

static mut IRQ_ENABLED: bool = false;

/// Whether the UART RX interrupt is wired on this ISA (set by
/// [`enable_uart_rx_irq`]). While false the portable input path polls.
pub fn uart_irq_enabled() -> bool {
    // SAFETY: single CPU; set once before any cell runs.
    unsafe { *core::ptr::addr_of!(IRQ_ENABLED) }
}

/// Bring up the UART RX interrupt through the AIA (APLIC-S + IMSIC-S) and the
/// 16550 IER, and enable S external interrupts. Idempotent-ish; call once before
/// running a cell that waits on console input. On a machine without the AIA (or
/// where M-mode did not delegate it) the CSR writes would trap - this is called
/// only by the Phase D test path, so no other kernel is affected.
pub fn enable_uart_rx_irq() {
    let hart = boot_hartid();
    unsafe {
        // --- IMSIC S-file (via indirect CSRs): enable delivery, no threshold,
        // enable the UART's EIID. ---
        imsic_write(0x70, 1); // eidelivery = 1
        imsic_write(0x72, 0); // eithreshold = 0 (deliver all enabled)
        imsic_write(0xC0, 1 << UART_EIID); // eie0 |= bit(EIID) (EIID < 64)

        // --- APLIC S-domain (MSI-delivery mode) ---
        let aplic = APLIC_S_BASE as *mut u32;
        aplic.write_volatile((1 << 8) | (1 << 2)); // domaincfg: IE=1, DM=1 (MSI)
        // sourcecfg[UART_SOURCE] = 6 (SM = Level1, active-high).
        aplic
            .byte_add(0x0004 + (UART_SOURCE - 1) * 4)
            .write_volatile(6);
        // target[UART_SOURCE] = (hart << 18) | EIID  (MSI to this hart's IMSIC).
        aplic
            .byte_add(0x3000 + UART_SOURCE * 4)
            .write_volatile(((hart as u32) << 18) | UART_EIID);
        // setienum = UART_SOURCE (enable the source).
        aplic.byte_add(0x1EDC).write_volatile(UART_SOURCE as u32);

        // --- 16550: OUT2 (gate IRQ on) + enable RX-data-available interrupt. ---
        UART_MCR.write_volatile(0x08); // OUT2
        UART_IER.write_volatile(0x01); // ERBFI: received-data-available interrupt

        // --- Enable S external interrupts (sie.SEIE, bit 9); keep sstatus.SIE
        // clear so the SEI is taken only when we ask (in idle_wait). ---
        asm!("csrrs x0, sie, {0}", in(reg) 1u64 << 9);

        *core::ptr::addr_of_mut!(IRQ_ENABLED) = true;
    }
}

/// Write `val` to the IMSIC S-file register selected by `sel` (via siselect/sireg).
///
/// # Safety
/// The AIA S-mode CSRs must be accessible (Ssaia + M-mode delegation).
unsafe fn imsic_write(sel: u64, val: u64) {
    unsafe {
        asm!(
            "csrw 0x150, {s}", // siselect
            "csrw 0x151, {v}", // sireg
            s = in(reg) sel, v = in(reg) val,
        );
    }
}

/// Drain the UART RX FIFO into the kernel ring and claim the interrupt in the
/// IMSIC S-file. Called from the kernel trap handler on a supervisor external
/// interrupt. Draining first (level low) then claiming avoids a re-assert.
fn handle_uart_irq() {
    // Drain the UART.
    while let Some(b) = serial_read_byte() {
        crate::input::rx_push(b);
    }
    // Claim the top external interrupt (a write to stopei clears its pending bit).
    // SAFETY: stopei is an S-mode AIA CSR, accessible once the AIA is up.
    unsafe {
        let mut top: u64;
        loop {
            asm!("csrr {0}, 0x15c", out(reg) top); // read stopei (no clear)
            if top == 0 {
                break;
            }
            asm!("csrw 0x15c, x0"); // write stopei -> claim/clear the top
            // Drain anything that arrived meanwhile.
            while let Some(b) = serial_read_byte() {
                crate::input::rx_push(b);
            }
        }
    }
}

/// Halt until the UART RX interrupt fires (a genuine 0%-CPU park). `wfi` wakes on
/// a pending enabled interrupt even with `sstatus.SIE` clear; we then briefly
/// enable SIE so the pending SEI is taken and serviced by the trap handler.
pub fn idle_wait() {
    // SAFETY: kernel context; SIE toggled around a single instruction.
    unsafe {
        asm!("wfi");
        asm!("csrrsi x0, sstatus, 2"); // set SIE -> pending SEI taken here
        asm!("csrrci x0, sstatus, 2"); // clear SIE
    }
}

// ------------------------------------------------- timer interrupt (Sstc)
// docs/LIBRHEO.md Phase F: the kernel's second interrupt. QEMU `virt` with
// `-cpu max` implements **Sstc**, so S-mode can arm its own timer by writing
// the `stimecmp` CSR (0x14D); when the `time` counter reaches it the machine
// raises `sip.STIP` (`scause` = interrupt | 5). We enable `sie.STIE` (bit 5)
// and, like the UART path, keep `sstatus.SIE` clear - `wfi` still wakes on the
// pending enabled interrupt, and we set SIE only briefly to service it. That
// makes `SYS_ARM_TIMER` a genuine 0%-CPU park (POWER.md: armed only when a real
// deadline exists). OpenSBI leaves `menvcfg.STCE` set on this QEMU config, so
// the S-mode `stimecmp` write does not trap; this is opt-in (`enable_timer_irq`,
// called only by the Phase F timer test), so no other kernel is affected.

/// `scause` for a supervisor timer interrupt: interrupt bit | cause 5.
const SCAUSE_S_TIMER: u64 = (1 << 63) | 5;
/// The QEMU `virt` timebase (the `time` CSR / `stimecmp` run at 10 MHz).
const TIMEBASE_HZ: u64 = 10_000_000;

static mut TIMER_ENABLED: bool = false;

/// Whether the S-mode timer interrupt (Sstc) is wired on this ISA (set by
/// [`enable_timer_irq`]). While false the portable timer path busy-waits.
pub fn timer_irq_enabled() -> bool {
    // SAFETY: single CPU; set once before any cell runs.
    unsafe { *core::ptr::addr_of!(TIMER_ENABLED) }
}

/// Read the `time` CSR (0xC01): the S-mode wall counter at `TIMEBASE_HZ`.
#[inline(always)]
fn rdtime() -> u64 {
    let t: u64;
    // SAFETY: `time` is a read-only counter CSR, readable in S-mode here.
    unsafe { asm!("csrr {0}, time", out(reg) t) };
    t
}

/// Enable the S-mode timer interrupt (Sstc): push `stimecmp` far out (so nothing
/// is pending) and set `sie.STIE`. Idempotent; call once before a cell that arms
/// a timer. If Sstc were absent the `stimecmp` write would trap - this is called
/// only by the Phase F timer test, so no other kernel is affected.
pub fn enable_timer_irq() {
    // SAFETY: kernel context; Sstc is present on `-cpu max` with menvcfg.STCE set.
    unsafe {
        asm!("csrw 0x14d, {0}", in(reg) u64::MAX); // stimecmp = never
        asm!("csrrs x0, sie, {0}", in(reg) 1u64 << 5); // sie.STIE
        *core::ptr::addr_of_mut!(TIMER_ENABLED) = true;
    }
}

/// Arm the S-mode timer for `deadline_ns` from now and halt at `wfi` until it
/// fires (a genuine 0%-CPU park). The timer runs in the `time`/`stimecmp` domain
/// (10 MHz), distinct from `cycles()` (the retired-instruction counter), so the
/// deadline is converted here. Called only when [`timer_irq_enabled`].
pub fn timer_wait(deadline_ns: u64) {
    let delta = ((deadline_ns as u128 * TIMEBASE_HZ as u128) / 1_000_000_000) as u64;
    let target = rdtime().wrapping_add(delta.max(1));
    // SAFETY: kernel context; `stimecmp` is writable (Sstc), SIE toggled around
    // a single serviced interrupt.
    unsafe {
        asm!("csrw 0x14d, {0}", in(reg) target); // stimecmp = deadline
        while rdtime() < target {
            asm!("wfi"); // wakes on the pending STI (SIE clear, not yet taken)
            asm!("csrrsi x0, sstatus, 2"); // take + service it (handler disarms)
            asm!("csrrci x0, sstatus, 2"); // clear SIE
        }
        asm!("csrw 0x14d, {0}", in(reg) u64::MAX); // disarm
    }
}

/// Deliver a scripted byte through the real UART RX interrupt (16550 loopback),
/// halting at `wfi` until the interrupt is taken - the same path a live keystroke
/// takes. Used by the deterministic Phase D test.
pub fn uart_inject_and_wait(b: u8) {
    const UART_THR: *mut u8 = (0x1000_0000 | KERNEL_VA_BASE) as *mut u8;
    // Deliver the byte into the UART's RX FIFO by 16550 loopback (a genuine
    // receive - the handler reads it back out), then raise the UART's MSI in the
    // S-mode IMSIC and `wfi` until the resulting S external interrupt is taken -
    // the same interrupt a live keystroke raises through the APLIC. QEMU's 16550
    // loopback does not drive the APLIC input line, so we raise the MSI directly;
    // it is exactly the MSI the configured S-APLIC (source 10 -> this hart, EIID
    // 10) would send (docs/LIBRHEO.md Phase D).
    let imsic = ((0x2800_0000usize + boot_hartid() * 0x1000) | KERNEL_VA_BASE) as *mut u32;
    // SAFETY: kernel context; the UART + IMSIC MMIO are mapped high.
    unsafe {
        UART_MCR.write_volatile(0x18); // OUT2 + LOOP
        UART_THR.write_volatile(b); // byte received into the RX FIFO
        UART_MCR.write_volatile(0x08); // drop loopback (byte stays in FIFO)
        imsic.write_volatile(UART_EIID); // seteipnum <- EIID (the APLIC's MSI); SEIP asserts
        asm!("wfi"); // wakes on the pending SEI (SIE clear, not yet taken)
        asm!("csrrsi x0, sstatus, 2"); // take + service it (handler drains b)
        asm!("csrrci x0, sstatus, 2"); // clear SIE
    }
}

// ----------------------------------------------------------------- traps

unsafe extern "C" {
    fn trap_vector();
}

pub fn trap_init() {
    unsafe {
        // Direct mode: all traps to one handler (address is 4-aligned).
        asm!("csrw stvec, {0}", in(reg) trap_vector as *const ());
    }
}

static DOORBELLS: AtomicU64 = AtomicU64::new(0);

const SCAUSE_BREAKPOINT: u64 = 3;

/// Called from trap.S with (scause, sepc, stval); returns the sepc to
/// resume at. Breakpoint (ebreak, delegated to S-mode by OpenSBI) is the
/// doorbell stand-in; everything else is fatal.
#[unsafe(no_mangle)]
extern "C" fn riscv_trap_handler(scause: u64, sepc: u64, stval: u64) -> u64 {
    if scause == SCAUSE_S_EXT {
        // The kernel's first hardware interrupt (docs/LIBRHEO.md Phase D): the
        // UART RX line, delivered via the AIA. Drain it and resume where we were.
        handle_uart_irq();
        return sepc;
    }
    if scause == SCAUSE_S_TIMER {
        // The kernel's second interrupt (docs/LIBRHEO.md Phase F): the S-mode
        // timer (Sstc). Disarm by pushing stimecmp far out (clears STIP); the
        // waiter in `timer_wait` observes the elapsed deadline and returns.
        unsafe { asm!("csrw 0x14d, {0}", in(reg) u64::MAX) };
        return sepc;
    }
    if scause == SCAUSE_BREAKPOINT {
        DOORBELLS.fetch_add(1, Ordering::Relaxed);
        // Skip the ebreak: 4 bytes for the full encoding, 2 for c.ebreak.
        let insn = unsafe { (sepc as *const u16).read_volatile() };
        return if insn & 0b11 == 0b11 {
            sepc + 4
        } else {
            sepc + 2
        };
    }
    crate::println!("TRAP: scause {scause:#x} at sepc {sepc:#x}, stval {stval:#x}");
    exit(super::ExitCode::Failure)
}

/// One kernel-entry round trip via ebreak (the doorbell measurement floor).
pub fn doorbell_trap() {
    unsafe { asm!("ebreak") };
}

pub fn doorbell_count() -> u64 {
    DOORBELLS.load(Ordering::Relaxed)
}

// ----------------------------------------------------- hardware discovery

unsafe extern "C" {
    static BOOT_DTB: u64;
    static BOOT_HARTID: u64;
}

/// The device-tree blob pointer OpenSBI passed in a1.
pub fn boot_firmware_ptr() -> usize {
    unsafe { core::ptr::addr_of!(BOOT_DTB).read() as usize }
}

/// Discover the machine from the device tree. OpenSBI passes a *physical* DTB
/// pointer; the kernel runs high, so it is read through the high linear map.
pub fn discover(inv: &mut crate::hw::Inventory) {
    let dtb = boot_firmware_ptr();
    if dtb != 0 {
        inv.firmware = crate::hw::Firmware::DeviceTree;
        crate::hw::fdt::parse(phys_to_virt(dtb), inv);
    }
}

/// Boot hart id (a0), for SMP.
#[allow(dead_code)]
pub fn boot_hartid() -> usize {
    unsafe { core::ptr::addr_of!(BOOT_HARTID).read() as usize }
}

// ------------------------------------------------------------------- SMP
// docs/SMP.md, task #27. RISC-V is the ISA where a genuine second core runs
// kernel code here: OpenSBI runs in M-mode below the S-mode kernel, so the SBI
// HSM `hart_start` ecall is available to start a secondary hart. The secondary
// enters `secondary_entry` (arch/riscv64/smp.S) with the MMU off, loads the
// shared kernel satp, and calls `rv_secondary_main` below. Gated behind the
// `smp` feature so the non-SMP kernels link a byte-identical `kernel` lib
// (adding it perturbs codegen-unit hashing); only the `smp` test enables it.

#[cfg(feature = "smp")]
global_asm!(include_str!("../../../arch/riscv64/smp.S"));

#[cfg(feature = "smp")]
unsafe extern "C" {
    /// Physical (low LMA) address of the secondary entry trampoline, published
    /// as a high .rodata word by smp.S (an absolute reloc the high-half kernel
    /// can read - it cannot form the low address PC-relatively under medany).
    static SECONDARY_ENTRY_PA: u64;
}

/// The per-hart CPU index for this hart. The kernel keeps it in `tp` (the thread
/// pointer, free in S-mode kernel context - no cell runs in the SMP test, and
/// `tp` is only meaningful as user TLS while a cell runs). Defaults to 0 until
/// [`smp_set_this_cpu`] establishes an identity, so the single-CPU path always
/// reports CPU 0.
#[cfg(feature = "smp")]
pub fn cpu_index() -> usize {
    let v: usize;
    // SAFETY: reads `tp`, a GPR; no memory effect.
    unsafe { asm!("mv {0}, tp", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Record this hart's CPU index in `tp`. Called once per hart as it comes up
/// (the primary in `smp::init`, a secondary in `rv_secondary_main`).
#[cfg(feature = "smp")]
pub fn smp_set_this_cpu(index: usize) {
    // SAFETY: writes `tp`, a GPR; safe in kernel context (no cell running).
    unsafe { asm!("mv tp, {0}", in(reg) index, options(nomem, nostack, preserves_flags)) };
}

/// The boot hart id, as the portable CPU hardware id.
#[cfg(feature = "smp")]
pub fn boot_cpu_hw_id() -> u32 {
    boot_hartid() as u32
}

/// SBI HSM `hart_start(hartid, start_addr, opaque)` (EID 0x48534D "HSM", FID 0).
/// Returns the SBI error code (0 = success). The started hart begins at
/// `start_addr` in S-mode with the MMU off, `a0 = hartid`, `a1 = opaque`.
#[cfg(feature = "smp")]
fn sbi_hart_start(hartid: usize, start_addr: usize, opaque: usize) -> isize {
    let error: isize;
    // SAFETY: an ecall to OpenSBI (M-mode) with the HSM calling convention; the
    // clobbered a-registers are declared, ra is preserved by the SBI ABI.
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") hartid => error,
            inlateout("a1") start_addr => _,
            in("a2") opaque,
            in("a6") 0usize,          // FID 0 = hart_start
            in("a7") 0x0048_534Dusize, // EID "HSM"
            options(nostack),
        );
    }
    error
}

/// Start one secondary hart running kernel code (docs/SMP.md). Hands it the
/// kernel's own satp (so it shares the address space) via the SBI `opaque` arg
/// and the low physical entry trampoline as `start_addr`. Returns Ok if SBI
/// accepted the start; the caller waits for the hart to actually come online.
#[cfg(feature = "smp")]
pub fn smp_start_secondary(hw_id: u32) -> Result<(), &'static str> {
    let satp: usize;
    // SAFETY: reads satp, an S-mode CSR.
    unsafe { asm!("csrr {0}, satp", out(reg) satp, options(nomem, nostack)) };
    let entry = unsafe { core::ptr::addr_of!(SECONDARY_ENTRY_PA).read() } as usize;
    match sbi_hart_start(hw_id as usize, entry, satp) {
        0 => Ok(()),
        -2 => Err("SBI_ERR_NOT_SUPPORTED (no HSM extension)"),
        -3 => Err("SBI_ERR_INVALID_PARAM"),
        -6 => Err("SBI_ERR_ALREADY_AVAILABLE (hart already running)"),
        _ => Err("SBI hart_start failed"),
    }
}

/// The secondary hart's Rust entry, called from smp.S with `a0 = hartid` once it
/// is running high-half kernel code on the shared address space. Establishes its
/// per-CPU identity, runs the portable bring-up proof, then returns to the asm
/// `wfi` park. Never returns to normal scheduling - this is a proof of life, not
/// preemptive SMP scheduling (docs/SMP.md).
#[cfg(feature = "smp")]
#[unsafe(no_mangle)]
extern "C" fn rv_secondary_main(hartid: usize) {
    // Install a trap vector so a stray fault is contained rather than jumping to
    // the reset stvec (0). The secondary should not trap: it does integer work
    // on shared memory and parks. secondary_run claims this CPU's registry index
    // and sets its per-CPU identity (tp).
    trap_init();
    crate::smp::secondary_run(hartid as u32);
}

/// Feature names; bit i in CpuReport.features corresponds to index i.
pub fn cpu_feature_names() -> &'static [&'static str] {
    &[
        "rv64", "M", "A", "F", "D", "C", "V", "Zicsr", "Zifencei", "Zba", "Zbb", "Zbs",
    ]
}

/// Decode CPU features from the device-tree "riscv,isa" string (misa is an
/// M-mode CSR and traps in S-mode, so the firmware string is the source).
pub fn cpu_report(_inv: &crate::hw::Inventory) -> crate::hw::CpuReport {
    let mut report = crate::hw::CpuReport::EMPTY;
    report.vendor[..5].copy_from_slice(b"riscv");
    let isa = crate::hw::fdt::riscv_isa();
    // Base extensions are the single letters after "rv64", before any '_'.
    let base = isa.get(4..).unwrap_or("").split('_').next().unwrap_or("");
    for (i, name) in cpu_feature_names().iter().enumerate() {
        let present = if name.len() == 1 {
            let c = name.as_bytes()[0].to_ascii_lowercase();
            base.as_bytes().contains(&c)
        } else {
            contains_ci(isa, name)
        };
        if present {
            report.features |= 1 << i;
        }
    }
    report
}

/// Case-insensitive substring search (no allocation).
fn contains_ci(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() || n.len() > h.len() {
        return false;
    }
    for i in 0..=(h.len() - n.len()) {
        if h[i..i + n.len()].eq_ignore_ascii_case(n) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------- virtio-mmio slots
// QEMU riscv `virt`: 8 virtio-mmio transports at 0x1000_1000, stride 0x1000
// (within the 0..1 GiB MMIO gigapage the kernel maps high).
pub const VIRTIO_MMIO_BASE: usize = 0x1000_1000 | KERNEL_VA_BASE;
pub const VIRTIO_MMIO_STRIDE: usize = 0x1000;
pub const VIRTIO_MMIO_COUNT: usize = 8;

// ----------------------------------------------------- hardware RNG

/// No usable hardware RNG here. The scalar-crypto entropy source (Zkr, the
/// `seed` CSR at 0x015) is an M-mode CSR; S-mode access must be granted by
/// M-mode via mseccfg.sseed, which this OpenSBI/QEMU configuration does not
/// enable, so reading it would trap. A real RISC-V board with Zkr and the
/// mseccfg grant (or an SBI entropy call) would return true here. The root
/// DRBG falls back accordingly (rng::SeedSource::Fallback).
pub fn has_hwrng() -> bool {
    false
}

pub fn hwrng_name() -> &'static str {
    "none (Zkr seed CSR needs M-mode mseccfg grant)"
}

pub fn hwrng_u64() -> Option<u64> {
    None
}

/// PCI config read through the ECAM window (RISC-V has no config ports).
pub fn pci_cfg_read32(ecam: u64, bus: u8, dev: u8, func: u8, off: u16) -> u32 {
    let a = ecam
        + ((bus as u64) << 20)
        + ((dev as u64) << 15)
        + ((func as u64) << 12)
        + (off as u64 & 0xFFC);
    unsafe { (phys_to_virt(a as usize) as *const u32).read_volatile() }
}

/// PCI config write through the ECAM window (RISC-V has no config ports).
pub fn pci_cfg_write32(ecam: u64, bus: u8, dev: u8, func: u8, off: u16, val: u32) {
    let a = ecam
        + ((bus as u64) << 20)
        + ((dev as u64) << 15)
        + ((func as u64) << 12)
        + (off as u64 & 0xFFC);
    unsafe { (phys_to_virt(a as usize) as *mut u32).write_volatile(val) }
}

/// The host bridge's 32-bit MMIO window for BAR assignment
/// (docs/GPU-HARDWARE.md 3). QEMU `virt` puts PCIe MMIO at
/// 0x4000_0000..0x8000_0000.
pub fn pci_mmio_window() -> (u64, u64) {
    (0x4000_0000, 0x4000_0000)
}

// -------------------------------------------------------------- user mode

/// Saved U-mode register state. Layout matches the offsets in traps.S:
/// `regs[i]` is xi (regs[0]/x0 unused, regs[2] is the user sp), then sepc,
/// then the kernel sp to load on trap entry.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct TrapFrame {
    regs: [u64; 32],
    sepc: u64,
    kernel_sp: u64,
}

const REG_SP: usize = 2;
const REG_TP: usize = 4; // thread pointer (TLS); a saved GPR, so per-context
const REG_A0: usize = 10; // first argument / return value
const REG_A7: usize = 17; // syscall number
const SCAUSE_ECALL_U: u64 = 8;

/// Build a fresh frame that enters `entry` in U-mode with stack `user_sp`
/// and `arg` in a0. `kernel_sp` is the stack the trap handler runs on.
pub fn trapframe_new(entry: usize, user_sp: usize, arg: usize, kernel_sp: usize) -> TrapFrame {
    let mut regs = [0u64; 32];
    regs[REG_SP] = user_sp as u64;
    regs[REG_A0] = arg as u64;
    TrapFrame {
        regs,
        sepc: entry as u64,
        kernel_sp: kernel_sp as u64,
    }
}

/// (syscall number in a7, arguments a0..a5 = x10..x15).
pub fn decode_syscall(frame: &TrapFrame) -> (u64, [u64; 6]) {
    (
        frame.regs[REG_A7],
        [
            frame.regs[REG_A0],
            frame.regs[REG_A0 + 1],
            frame.regs[REG_A0 + 2],
            frame.regs[REG_A0 + 3],
            frame.regs[REG_A0 + 4],
            frame.regs[REG_A0 + 5],
        ],
    )
}

pub fn set_syscall_ret(frame: &mut TrapFrame, value: u64) {
    frame.regs[REG_A0] = value;
}

/// A zeroed frame, for static per-context storage (docs/LINUX-COMPAT.md L4).
pub const fn trapframe_zeroed() -> TrapFrame {
    TrapFrame {
        regs: [0; 32],
        sepc: 0,
        kernel_sp: 0,
    }
}

/// Build a thread child's frame from the cloning parent's (docs/LINUX-COMPAT.md
/// L4): same code/return point (`sepc`, already advanced past the parent's
/// `ecall`) and kernel stack, a new user stack, `a0 = 0` so `clone` returns 0
/// in the child, and the child's TLS in `tp` (a saved GPR, restored on resume).
pub fn clone_child_frame(parent: &TrapFrame, child_sp: u64, tls: u64) -> TrapFrame {
    let mut f = *parent;
    f.regs[REG_SP] = child_sp;
    f.regs[REG_A0] = 0;
    f.regs[REG_TP] = tls;
    f
}

/// Save the live U-mode FP state (f0-f31 + fcsr) into `area`, for a cooperative
/// context switch between two threads of one cell (docs/LINUX-COMPAT.md L4).
/// The kernel runs with sstatus.FS enabled and is soft-float, so the registers
/// still hold the trapped thread's values.
///
/// # Safety
/// `area` must point to at least 264 writable, 8-byte-aligned bytes.
pub unsafe fn save_user_fp(area: *mut u8) {
    unsafe {
        asm!(
            "fsd f0, 0({b})", "fsd f1, 8({b})", "fsd f2, 16({b})", "fsd f3, 24({b})",
            "fsd f4, 32({b})", "fsd f5, 40({b})", "fsd f6, 48({b})", "fsd f7, 56({b})",
            "fsd f8, 64({b})", "fsd f9, 72({b})", "fsd f10, 80({b})", "fsd f11, 88({b})",
            "fsd f12, 96({b})", "fsd f13, 104({b})", "fsd f14, 112({b})", "fsd f15, 120({b})",
            "fsd f16, 128({b})", "fsd f17, 136({b})", "fsd f18, 144({b})", "fsd f19, 152({b})",
            "fsd f20, 160({b})", "fsd f21, 168({b})", "fsd f22, 176({b})", "fsd f23, 184({b})",
            "fsd f24, 192({b})", "fsd f25, 200({b})", "fsd f26, 208({b})", "fsd f27, 216({b})",
            "fsd f28, 224({b})", "fsd f29, 232({b})", "fsd f30, 240({b})", "fsd f31, 248({b})",
            "frcsr {t}", "sd {t}, 256({b})",
            b = in(reg) area, t = out(reg) _, options(nostack),
        );
    }
}

/// Restore U-mode FP state saved by [`save_user_fp`].
///
/// # Safety
/// `area` must point to a valid 264-byte image written by `save_user_fp`.
pub unsafe fn restore_user_fp(area: *const u8) {
    unsafe {
        asm!(
            "fld f0, 0({b})", "fld f1, 8({b})", "fld f2, 16({b})", "fld f3, 24({b})",
            "fld f4, 32({b})", "fld f5, 40({b})", "fld f6, 48({b})", "fld f7, 56({b})",
            "fld f8, 64({b})", "fld f9, 72({b})", "fld f10, 80({b})", "fld f11, 88({b})",
            "fld f12, 96({b})", "fld f13, 104({b})", "fld f14, 112({b})", "fld f15, 120({b})",
            "fld f16, 128({b})", "fld f17, 136({b})", "fld f18, 144({b})", "fld f19, 152({b})",
            "fld f20, 160({b})", "fld f21, 168({b})", "fld f22, 176({b})", "fld f23, 184({b})",
            "fld f24, 192({b})", "fld f25, 200({b})", "fld f26, 208({b})", "fld f27, 216({b})",
            "fld f28, 224({b})", "fld f29, 232({b})", "fld f30, 240({b})", "fld f31, 248({b})",
            "ld {t}, 256({b})", "fscsr {t}",
            b = in(reg) area, t = out(reg) _, options(nostack, readonly),
        );
    }
}

/// Bytes reserved per cell for a saved U-mode FP image (f0-f31 + fcsr = 264
/// bytes; rounded up for alignment/headroom). Vector (RVV) state is not enabled.
pub const FP_AREA_LEN: usize = 512;

/// Initialize a cell's FP save area to a clean state (all f-regs and fcsr zero -
/// the reset default; a zeroed fcsr masks nothing but RISC-V has no trapping FP
/// exceptions, so zero is a valid clean image). Explicit for re-install.
///
/// # Safety
/// `area` must point to at least `FP_AREA_LEN` writable bytes.
pub unsafe fn fp_area_init(area: *mut u8) {
    unsafe { core::ptr::write_bytes(area, 0, FP_AREA_LEN) };
}

/// Portable `SIMD_*` tier mask a cell reads (docs/TILES.md 4). RISC-V cells use
/// scalar F/D (hard-float baseline); the vector extension (RVV) is not enabled,
/// so there is no SIMD tier to advertise - the tile executor runs scalar.
pub fn fp_simd_tiers() -> u64 {
    0
}

/// x86-only `arch_prctl` TLS hook (docs/LINUX-COMPAT.md L1). Unreachable on
/// RISC-V: the asm-generic table has no `arch_prctl` number, and U-mode sets
/// its own `tp` (a saved GPR), so glibc never asks the kernel. Present only
/// so the portable personality dispatch compiles on every ISA.
pub fn set_user_fs_base(_addr: u64) {}
pub fn user_fs_base() -> u64 {
    0
}

// ---------------------------------------------------- signal frame (L5)

/// User VA of the injected `rt_sigreturn` trampoline page (docs/LINUX-COMPAT.md
/// L5). Like ARM64, RISC-V has no SA_RESTORER path (the restorer normally comes
/// from the vDSO), so a 2-instruction page is mapped into every Linux cell and
/// the handler's `ra` is pointed at it. A free low page, below any load base.
pub const SIGTRAMP_VA: usize = 0x2000;

/// The asm-generic kernel `struct sigaction` has no `sa_restorer` field (the
/// restorer comes from the injected trampoline, not the caller); `sa_mask`
/// follows `sa_flags` directly (docs/LINUX-COMPAT.md L5).
pub const SIGACTION_HAS_RESTORER: bool = false;

/// The `rt_sigreturn` trampoline: `li a7, 139 (rt_sigreturn); ecall`.
/// Encoded little-endian (addi a7,x0,139 = 0x08B00893; ecall = 0x00000073).
pub fn sig_tramp_code() -> &'static [u8] {
    &[0x93, 0x08, 0xB0, 0x08, 0x73, 0x00, 0x00, 0x00]
}

/// The interrupted user stack pointer (for building a signal frame, L5).
pub fn user_sp(frame: &TrapFrame) -> u64 {
    frame.regs[REG_SP]
}

/// The kernel stack pointer saved in `frame` (loaded on trap entry). `execve`
/// reuses it when building the new image's entry frame (docs/LINUX-COMPAT.md L6).
pub fn trapframe_kernel_sp(frame: &TrapFrame) -> usize {
    frame.kernel_sp as usize
}

// rt_sigframe layout on the user stack: { siginfo(128); ucontext }.
const INFO_OFF: u64 = 0;
const UC_OFF: u64 = 128;
const UC_SIGMASK_OFF: u64 = 40; // uc_sigmask within the ucontext
const MC_OFF: u64 = UC_OFF + 176; // uc_mcontext (sc_regs) within the frame
const MC_PC: u64 = MC_OFF; // sc_regs[0] = pc
const MC_REGS: u64 = MC_OFF; // sc_regs[i] = x[i] at MC_REGS + 8*i (i = 1..31)
const FRAME_RESERVE: u64 = 1024;

/// Build a Linux `rt_sigframe` on the user stack and rewrite `frame` to enter
/// the handler (docs/LINUX-COMPAT.md L5). The cell's address space is active
/// (trap context), so user VAs are written directly.
///
/// # Safety
/// `spec.stack_top` must be a valid, writable user stack VA in the active cell.
pub fn setup_rt_frame(frame: &mut TrapFrame, spec: &super::SigFrameSpec) {
    let base = (spec.stack_top - FRAME_RESERVE) & !0xF; // 16-aligned SP
    // SAFETY: [base, base+FRAME_RESERVE) is writable user stack in the active cell.
    unsafe {
        let w = |off: u64, v: u64| ((base + off) as *mut u64).write(v);
        // siginfo: si_signo, si_errno, si_code, si_addr.
        ((base + INFO_OFF) as *mut i32).write(spec.signo as i32);
        ((base + INFO_OFF + 4) as *mut i32).write(0);
        ((base + INFO_OFF + 8) as *mut i32).write(spec.si_code);
        w(INFO_OFF + 16, spec.si_addr);
        w(UC_OFF, 0); // uc_flags
        w(UC_OFF + 8, 0); // uc_link
        w(UC_OFF + UC_SIGMASK_OFF, spec.saved_mask);
        w(MC_PC, frame.sepc); // sc_regs[0] = pc
        for i in 1..32usize {
            w(MC_REGS + (i as u64) * 8, frame.regs[i]);
        }
    }
    frame.regs[REG_A0] = spec.signo as u64; // a0: signo
    frame.regs[REG_A0 + 1] = base + INFO_OFF; // a1: siginfo*
    frame.regs[REG_A0 + 2] = base + UC_OFF; // a2: ucontext*
    frame.regs[1] = SIGTRAMP_VA as u64; // ra -> rt_sigreturn trampoline
    frame.regs[REG_SP] = base;
    frame.sepc = spec.handler;
}

/// Restore a `TrapFrame` saved by [`setup_rt_frame`] on `rt_sigreturn` and
/// return the saved signal mask. On entry the handler's SP (frame base) is in
/// the saved `x2` (the trampoline does not move it).
pub fn restore_rt_frame(frame: &mut TrapFrame) -> u64 {
    let base = frame.regs[REG_SP];
    // SAFETY: `base` is the frame VA in the active cell, written by setup_rt_frame.
    unsafe {
        frame.sepc = ((base + MC_PC) as *const u64).read();
        for i in 1..32usize {
            frame.regs[i] = ((base + MC_REGS + (i as u64) * 8) as *const u64).read();
        }
        ((base + UC_OFF + UC_SIGMASK_OFF) as *const u64).read()
    }
}

unsafe extern "C" {
    /// Enter U-mode with `frame`, saving kernel state for return_to_kernel.
    pub fn enter_user_first(frame: *mut TrapFrame);
    /// Unwind back out of enter_user_first. Diverges.
    fn return_to_kernel_asm() -> !;
}

/// Leave U-mode and resume the kernel run loop (see enter_user_first).
pub fn return_to_kernel() -> ! {
    // SAFETY: only called while a cell is running, i.e. inside the
    // dynamic extent of an enter_user_first call.
    unsafe { return_to_kernel_asm() }
}

/// Called from traps.S on every U-mode trap. Advances past the ecall for
/// syscalls, then hands off to the portable dispatcher, which returns the
/// frame to resume (or diverges via return_to_kernel).
#[unsafe(no_mangle)]
extern "C" fn riscv_user_trap(scause: u64, stval: u64, frame: *mut TrapFrame) -> *mut TrapFrame {
    let kind = if scause == SCAUSE_ECALL_U {
        // Resume after the 4-byte ecall.
        unsafe { (*frame).sepc += 4 };
        super::TrapKind::Syscall
    } else {
        super::TrapKind::Fault
    };
    let resume = crate::user::on_user_trap(kind, fault_cause(scause), stval as usize, frame);
    if resume.is_null() {
        return_to_kernel();
    }
    resume
}

/// Map an S-mode trap `scause` to a portable fault cause (docs/LINUX-COMPAT.md
/// L5). Access/page faults are SIGSEGV; misaligned access SIGBUS; illegal
/// instruction SIGILL.
fn fault_cause(scause: u64) -> super::FaultCause {
    match scause {
        1 | 5 | 7 | 12 | 13 | 15 => super::FaultCause::Segv, // access / page faults
        0 | 4 | 6 => super::FaultCause::Bus,                 // misaligned fetch/load/store
        2 => super::FaultCause::Ill,                         // illegal instruction
        _ => super::FaultCause::Segv,
    }
}

// -------------------------------------------------------------- counters

pub fn cycles() -> u64 {
    let value: u64;
    unsafe { asm!("csrr {0}, cycle", out(reg) value) };
    value
}

/// Convert `cycles()` to nanoseconds for the Linux personality's
/// `clock_gettime` (docs/LINUX-COMPAT.md L2). `cycles()` reads the `cycle`
/// CSR (retired cycles); QEMU virt exposes a 10 MHz timebase for the separate
/// `time` CSR. These are different counters, so this is an approximation -
/// accuracy is irrelevant for glibc's coarse clock probes on the fixtures.
pub fn ticks_to_ns(ticks: u64) -> u64 {
    const TIMEBASE_HZ: u64 = 10_000_000;
    ((ticks as u128 * 1_000_000_000) / TIMEBASE_HZ as u128) as u64
}

/// Calibration loop with a known instruction count: exactly 2
/// instructions per iteration (addi + bnez). Benchmarks use it to convert
/// counter ticks into approximate instruction counts under QEMU -icount.
pub fn spin_loop(iters: u64) {
    if iters == 0 {
        return;
    }
    let mut n = iters;
    unsafe {
        asm!(
            "2:",
            "addi {0}, {0}, -1",
            "bnez {0}, 2b",
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
/// Frame layout must match context_switch.S: ra, then s0-s11,
/// 112 bytes total (16-aligned).
///
/// # Safety
/// `stack_top` must be the 16-aligned top of a stack of adequate size.
pub unsafe fn context_init(stack_top: *mut u8, entry: extern "C" fn() -> !) -> super::Context {
    unsafe {
        let sp = stack_top.sub(112) as *mut u64;
        sp.write(entry as usize as u64); // ra: return address
        for i in 1..13 {
            sp.add(i).write(0); // s0..s11
        }
        super::Context { sp: sp as usize }
    }
}

// ------------------------------------------------------------------ exit

/// sifive_test device at 0x10_0000: 0x5555 = pass (QEMU exits 0),
/// (code << 16) | 0x3333 = fail (QEMU exits with the code).
pub fn exit(code: super::ExitCode) -> ! {
    const TEST_DEVICE: *mut u32 = (0x10_0000 | KERNEL_VA_BASE) as *mut u32;
    let value: u32 = match code {
        super::ExitCode::Success => 0x5555,
        super::ExitCode::Failure => (1 << 16) | 0x3333,
    };
    unsafe {
        TEST_DEVICE.write_volatile(value);
    }
    // Only reached without the test device.
    loop {
        unsafe { asm!("wfi") };
    }
}
