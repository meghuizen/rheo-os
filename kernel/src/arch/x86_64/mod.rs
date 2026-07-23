//! x86-64: PVH boot entry, 16550 UART on port 0x3F8, isa-debug-exit.

use core::arch::{asm, global_asm};

global_asm!(
    include_str!("../../../arch/x86_64/boot.S"),
    options(att_syntax)
);

pub const NAME: &str = "x86-64";

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
