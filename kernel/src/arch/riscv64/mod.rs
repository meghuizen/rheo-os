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
    PagingRoot, paging_activate, paging_activate_kernel, paging_kernel_init, paging_map,
    paging_map_frame, paging_new_root, paging_protect, paging_unmap_frame,
};

/// `uname` machine string for the Linux personality (docs/LINUX-COMPAT.md L2).
pub const LINUX_UNAME_MACHINE: &str = "riscv64";

global_asm!(include_str!("../../../arch/riscv64/boot.S"));
global_asm!(include_str!("../../../arch/riscv64/traps.S"));
global_asm!(include_str!("../../../arch/riscv64/context_switch.S"));

pub const NAME: &str = "RISC-V 64";

/// Physical base of the frame pool: 64 MiB into RAM, well above the kernel
/// image (checked against __kernel_end in frames::init).
pub const FRAME_POOL_BASE: usize = 0x8400_0000;

/// Physical <-> kernel virtual address. RISC-V keeps the kernel identity-
/// mapped for now (the higher-half move is a separate step), so these are the
/// identity - the portable `phys_to_virt` seam (mm/frames, load, hw DMA) is a
/// no-op here.
#[inline(always)]
pub fn phys_to_virt(pa: usize) -> usize {
    pa
}
#[inline(always)]
pub fn virt_to_phys(va: usize) -> usize {
    va
}

// ---------------------------------------------------------------- serial

const UART_BASE: usize = 0x1000_0000;
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

/// Discover the machine from the device tree.
pub fn discover(inv: &mut crate::hw::Inventory) {
    let dtb = boot_firmware_ptr();
    if dtb != 0 {
        inv.firmware = crate::hw::Firmware::DeviceTree;
        crate::hw::fdt::parse(dtb, inv);
    }
}

/// Boot hart id (a0), for SMP.
#[allow(dead_code)]
pub fn boot_hartid() -> usize {
    unsafe { core::ptr::addr_of!(BOOT_HARTID).read() as usize }
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
// (within the 0..1 GiB MMIO gigapage the kernel maps).
pub const VIRTIO_MMIO_BASE: usize = 0x1000_1000;
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
    unsafe { (a as *const u32).read_volatile() }
}

/// PCI config write through the ECAM window (RISC-V has no config ports).
pub fn pci_cfg_write32(ecam: u64, bus: u8, dev: u8, func: u8, off: u16, val: u32) {
    let a = ecam
        + ((bus as u64) << 20)
        + ((dev as u64) << 15)
        + ((func as u64) << 12)
        + (off as u64 & 0xFFC);
    unsafe { (a as *mut u32).write_volatile(val) }
}

// -------------------------------------------------------------- user mode

/// Saved U-mode register state. Layout matches the offsets in traps.S:
/// `regs[i]` is xi (regs[0]/x0 unused, regs[2] is the user sp), then sepc,
/// then the kernel sp to load on trap entry.
#[repr(C)]
pub struct TrapFrame {
    regs: [u64; 32],
    sepc: u64,
    kernel_sp: u64,
}

const REG_SP: usize = 2;
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

/// x86-only `arch_prctl` TLS hook (docs/LINUX-COMPAT.md L1). Unreachable on
/// RISC-V: the asm-generic table has no `arch_prctl` number, and U-mode sets
/// its own `tp` (a saved GPR), so glibc never asks the kernel. Present only
/// so the portable personality dispatch compiles on every ISA.
pub fn set_user_fs_base(_addr: u64) {}
pub fn user_fs_base() -> u64 {
    0
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
    let resume = crate::user::on_user_trap(kind, stval as usize, frame);
    if resume.is_null() {
        return_to_kernel();
    }
    resume
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
    const TEST_DEVICE: *mut u32 = 0x10_0000 as *mut u32;
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
