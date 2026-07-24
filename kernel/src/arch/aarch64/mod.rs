//! ARM64: QEMU virt machine, PL011 UART at 0x0900_0000, semihosting exit,
//! VBAR_EL1 traps, cntvct_el0, and the context-switch stub.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

mod paging;
pub use paging::{
    PagingRoot, paging_activate, paging_activate_kernel, paging_kernel_init, paging_map,
    paging_new_root,
};

global_asm!(include_str!("../../../arch/aarch64/boot.S"));
global_asm!(include_str!("../../../arch/aarch64/vectors.S"));
global_asm!(include_str!("../../../arch/aarch64/context_switch.S"));

pub const NAME: &str = "ARM64";

/// Physical base of the frame pool: 64 MiB into RAM, above the kernel
/// image (checked against __kernel_end in frames::init).
pub const FRAME_POOL_BASE: usize = 0x4400_0000;

// ---------------------------------------------------------------- serial

const PL011_BASE: usize = 0x0900_0000;
const PL011_DR: *mut u32 = PL011_BASE as *mut u32; // data register
const PL011_FR: *mut u32 = (PL011_BASE + 0x18) as *mut u32; // flag register
const FR_TXFF: u32 = 1 << 5; // transmit FIFO full

pub fn serial_init() {
    // QEMU's PL011 is usable as-is for TX; real init comes with the driver.
}

pub fn serial_write_byte(byte: u8) {
    unsafe {
        while PL011_FR.read_volatile() & FR_TXFF != 0 {}
        PL011_DR.write_volatile(byte as u32);
    }
}

const FR_RXFE: u32 = 1 << 4; // receive FIFO empty

/// Non-blocking read of one byte from the PL011, or None if none pending.
pub fn serial_read_byte() -> Option<u8> {
    unsafe {
        if PL011_FR.read_volatile() & FR_RXFE != 0 {
            None
        } else {
            Some(PL011_DR.read_volatile() as u8)
        }
    }
}

// ----------------------------------------------------------------- traps

unsafe extern "C" {
    static vector_table: u8;
}

pub fn trap_init() {
    unsafe {
        asm!(
            "msr vbar_el1, {0}",
            "isb",
            in(reg) core::ptr::addr_of!(vector_table),
        );
    }
}

static DOORBELLS: AtomicU64 = AtomicU64::new(0);

const EC_SVC64: u64 = 0x15;

/// Called from the "current EL, SPx, synchronous" vector. SVC is the
/// doorbell stand-in and returns (ELR already points past the svc);
/// every other exception class is fatal.
#[unsafe(no_mangle)]
extern "C" fn aarch64_sync_handler(esr: u64, elr: u64) {
    if (esr >> 26) & 0x3F == EC_SVC64 {
        DOORBELLS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    crate::println!("TRAP: sync exception, esr {esr:#x} at elr {elr:#x}");
    exit(super::ExitCode::Failure);
}

/// Called for the 15 vector slots that should never fire at this stage.
#[unsafe(no_mangle)]
extern "C" fn aarch64_fatal_handler(slot: u64, esr: u64, elr: u64) -> ! {
    crate::println!("TRAP: unexpected vector slot {slot}, esr {esr:#x} at elr {elr:#x}");
    exit(super::ExitCode::Failure)
}

/// One kernel-entry round trip via svc (the doorbell measurement floor).
pub fn doorbell_trap() {
    unsafe { asm!("svc #0") };
}

pub fn doorbell_count() -> u64 {
    DOORBELLS.load(Ordering::Relaxed)
}

// -------------------------------------------------------------- user mode

/// Saved EL0 register state. Layout matches the offsets in vectors.S:
/// x0..x30, then SP_EL0, ELR_EL1, SPSR_EL1, the kernel sp, and padding to
/// a 16-byte multiple.
#[repr(C)]
pub struct TrapFrame {
    regs: [u64; 31],
    sp_el0: u64,
    elr: u64,
    spsr: u64,
    kernel_sp: u64,
    _pad: u64,
}

const REG_X0: usize = 0; // first argument / return value
const REG_X8: usize = 8; // syscall number

pub fn trapframe_new(entry: usize, user_sp: usize, arg: usize, kernel_sp: usize) -> TrapFrame {
    let mut regs = [0u64; 31];
    regs[REG_X0] = arg as u64;
    TrapFrame {
        regs,
        sp_el0: user_sp as u64,
        elr: entry as u64,
        spsr: 0, // EL0t, interrupts unmasked (none are enabled)
        kernel_sp: kernel_sp as u64,
        _pad: 0,
    }
}

pub fn decode_syscall(frame: &TrapFrame) -> (u64, u64) {
    (frame.regs[REG_X8], frame.regs[REG_X0])
}

pub fn set_syscall_ret(frame: &mut TrapFrame, value: u64) {
    frame.regs[REG_X0] = value;
}

unsafe extern "C" {
    pub fn enter_user_first(frame: *mut TrapFrame);
    fn return_to_kernel_asm() -> !;
}

pub fn return_to_kernel() -> ! {
    // SAFETY: only called while a cell is running (inside enter_user_first).
    unsafe { return_to_kernel_asm() }
}

/// Called from vectors.S on every EL0 trap. SVC (ELR already points past
/// the instruction) is a syscall; any other exception class is a fault.
#[unsafe(no_mangle)]
extern "C" fn aarch64_user_trap(esr: u64, far: u64, frame: *mut TrapFrame) -> *mut TrapFrame {
    let kind = if (esr >> 26) & 0x3F == EC_SVC64 {
        super::TrapKind::Syscall
    } else {
        super::TrapKind::Fault
    };
    let resume = crate::user::on_user_trap(kind, far as usize, frame);
    if resume.is_null() {
        return_to_kernel();
    }
    resume
}

// -------------------------------------------------------------- counters

pub fn cycles() -> u64 {
    let value: u64;
    // isb keeps cntvct from being reordered around the measured code.
    unsafe { asm!("isb", "mrs {0}, cntvct_el0", out(reg) value) };
    value
}

/// Calibration loop with a known instruction count: exactly 2
/// instructions per iteration (subs + b.ne). Benchmarks use it to convert
/// counter ticks into approximate instruction counts under QEMU -icount.
pub fn spin_loop(iters: u64) {
    if iters == 0 {
        return;
    }
    let mut n = iters;
    unsafe {
        asm!(
            "2:",
            "subs {0}, {0}, #1",
            "b.ne 2b",
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
/// Frame layout must match context_switch.S: x19-x28, x29, then x30
/// (the return address), 96 bytes total.
///
/// # Safety
/// `stack_top` must be the 16-aligned top of a stack of adequate size.
pub unsafe fn context_init(stack_top: *mut u8, entry: extern "C" fn() -> !) -> super::Context {
    unsafe {
        let sp = stack_top.sub(96) as *mut u64;
        for i in 0..11 {
            sp.add(i).write(0); // x19..x28, x29
        }
        sp.add(11).write(entry as usize as u64); // x30: return address
        super::Context { sp: sp as usize }
    }
}

// ------------------------------------------------------------------ exit

/// Semihosting SYS_EXIT (DEVELOPMENT.md 6). QEMU must run with
/// `-semihosting-config enable=on,target=native`; it exits with our code.
pub fn exit(code: super::ExitCode) -> ! {
    const ADP_STOPPED_APPLICATION_EXIT: u64 = 0x20026;
    let status: u64 = match code {
        super::ExitCode::Success => 0,
        super::ExitCode::Failure => 1,
    };
    let block: [u64; 2] = [ADP_STOPPED_APPLICATION_EXIT, status];
    unsafe {
        asm!(
            "hlt #0xF000",
            in("w0") 0x18u32, // SYS_EXIT
            in("x1") block.as_ptr(),
        );
    }
    // Only reached without semihosting (e.g. interactive run).
    loop {
        unsafe { asm!("wfe") };
    }
}
