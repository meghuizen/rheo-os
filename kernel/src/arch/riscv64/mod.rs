//! RISC-V 64: QEMU virt machine, 16550 UART at 0x1000_0000, sifive_test
//! exit, stvec traps, rdcycle, and the context-switch stub.
//! Runs in S-mode on top of OpenSBI (DEVELOPMENT.md 4).

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

global_asm!(include_str!("../../../arch/riscv64/boot.S"));
global_asm!(include_str!("../../../arch/riscv64/trap.S"));
global_asm!(include_str!("../../../arch/riscv64/context_switch.S"));

pub const NAME: &str = "RISC-V 64";

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

// -------------------------------------------------------------- counters

pub fn cycles() -> u64 {
    let value: u64;
    unsafe { asm!("csrr {0}, cycle", out(reg) value) };
    value
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
