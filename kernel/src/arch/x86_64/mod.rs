//! x86-64: PVH boot entry, 16550 UART on port 0x3F8, isa-debug-exit,
//! IDT-based traps, rdtsc, and the context-switch stub.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

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

pub const NAME: &str = "x86-64";

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

const IDT_ENTRIES: usize = 32; // CPU exceptions only, for now

static mut IDT: [IdtEntry; IDT_ENTRIES] = [IdtEntry {
    offset_low: 0,
    selector: 0,
    ist_and_flags: 0,
    offset_mid: 0,
    offset_high: 0,
    reserved: 0,
}; IDT_ENTRIES];

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

unsafe extern "C" {
    // Table of the 32 stub addresses, emitted by vectors.S.
    static VECTOR_STUBS: [u64; IDT_ENTRIES];
}

pub fn trap_init() {
    unsafe {
        let idt = &mut *core::ptr::addr_of_mut!(IDT);
        for (i, entry) in idt.iter_mut().enumerate() {
            let handler = VECTOR_STUBS[i];
            *entry = IdtEntry {
                offset_low: handler as u16,
                selector: 0x08,        // boot GDT 64-bit code segment
                ist_and_flags: 0x8E00, // present, interrupt gate, DPL 0
                offset_mid: (handler >> 16) as u16,
                offset_high: (handler >> 32) as u32,
                reserved: 0,
            };
        }
        let pointer = IdtPointer {
            limit: (core::mem::size_of::<IdtEntry>() * IDT_ENTRIES - 1) as u16,
            base: core::ptr::addr_of!(IDT) as u64,
        };
        asm!("lidt [{}]", in(reg) &pointer);
    }
}

static DOORBELLS: AtomicU64 = AtomicU64::new(0);

/// Called from the common stub in vectors.S. Vector 3 (breakpoint) is the
/// doorbell stand-in and returns; everything else is fatal.
#[unsafe(no_mangle)]
extern "C" fn x86_trap_handler(vector: u64, error_code: u64, rip: u64) {
    if vector == 3 {
        DOORBELLS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    crate::println!("TRAP: vector {vector} error {error_code:#x} at rip {rip:#x}");
    exit(super::ExitCode::Failure);
}

/// One kernel-entry round trip via int3 (the doorbell measurement floor).
pub fn doorbell_trap() {
    unsafe { asm!("int3") };
}

pub fn doorbell_count() -> u64 {
    DOORBELLS.load(Ordering::Relaxed)
}

// -------------------------------------------------------------- counters

pub fn cycles() -> u64 {
    let lo: u32;
    let hi: u32;
    // lfence keeps rdtsc from being reordered around the measured code.
    unsafe { asm!("lfence", "rdtsc", out("eax") lo, out("edx") hi) };
    ((hi as u64) << 32) | lo as u64
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
