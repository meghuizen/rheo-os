//! RISC-V 64: QEMU virt machine, 16550 UART at 0x1000_0000, sifive_test exit.
//! Runs in S-mode on top of OpenSBI (DEVELOPMENT.md 4).

use core::arch::{asm, global_asm};

global_asm!(include_str!("../../../arch/riscv64/boot.S"));

pub const NAME: &str = "RISC-V 64";

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
