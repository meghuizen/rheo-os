//! Minimal serial console: just enough for `println!` during bring-up.
//! No locking yet - only the boot CPU runs at this stage. The real logging
//! design (per-strand ring buffers, lazy formatting) is in docs/LOGGING.md.

use core::fmt;

struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // Serial consoles expect "\r\n" line endings.
            if byte == b'\n' {
                crate::arch::serial_write_byte(b'\r');
            }
            crate::arch::serial_write_byte(byte);
        }
        Ok(())
    }
}

pub fn write(args: fmt::Arguments) {
    let _ = fmt::Write::write_fmt(&mut Console, args);
}

macro_rules! print {
    ($($arg:tt)*) => { $crate::console::write(format_args!($($arg)*)) };
}

macro_rules! println {
    () => { print!("\n") };
    ($($arg:tt)*) => { print!("{}\n", format_args!($($arg)*)) };
}
