//! The PTY bridge (docs/SHELL.md 1). The design has no kernel TTY
//! subsystem: a PTY is a pair of native queues plus a cooked line
//! discipline. In this emulation-first build the "terminal" is the serial
//! console (standing in for the latte-term cell, which needs a
//! compositor), and the kernel bridges it to the shell cell:
//!
//! - input: keystrokes (serial RX, or a pre-seeded script for headless
//!   tests) -> line discipline -> a completed line to the shell
//! - output: bytes from the shell -> serial
//!
//! The line discipline is cooked: it echoes, handles backspace and
//! Ctrl-C/Ctrl-D, and returns one line at a time. Raw mode (per-keystroke)
//! and the resize/signal control queue (docs/SHELL.md) are future work.

use crate::{arch, input};

/// Install a scripted input source (for deterministic headless tests). The
/// script is played as if typed; end of script is end of input.
///
/// Forwards to [`input::install_script`]: this module used to hold a second,
/// byte-identical `Source` enum with its own `static mut` and its own cursor
/// (docs/ARCHITECTURE-DEBT.md 3.6). There is one scripted-input source in the
/// kernel now. The **live** paths stay separate on purpose - see the note on
/// `input::Source`.
pub fn install_script(script: &'static [u8]) {
    input::install_script(script);
}

/// Write one byte to the console.
pub fn put_byte(b: u8) {
    arch::serial_write_byte(b);
}

/// Write a byte slice to the console.
pub fn write(bytes: &[u8]) {
    for &b in bytes {
        put_byte(b);
    }
}

fn echo_str(s: &str) {
    write(s.as_bytes());
}

/// Fetch the next raw input byte. Blocks (polls) on the serial source;
/// returns None at end of a script source.
fn next_byte() -> Option<u8> {
    // Scripted input comes from the one shared cursor; the live path is this
    // module's own (a blocking poll inside the cooked read, which is not what
    // `input`'s interrupt-fed ring does).
    if input::scripted() {
        return input::script_next_byte();
    }
    loop {
        if let Some(b) = arch::serial_read_byte() {
            return Some(b);
        }
        core::hint::spin_loop();
    }
}

/// Result of reading a line.
pub enum Line {
    /// A completed line of `len` bytes (already written into the caller's
    /// buffer). The trailing newline is not included.
    Read(usize),
    /// End of input (script exhausted / Ctrl-D on an empty line).
    Eof,
}

/// Read one cooked line into `buf` (capacity `cap`), echoing as it goes.
/// Backspace edits, Ctrl-C cancels the current line (returns an empty
/// line), Ctrl-D on an empty line is EOF.
///
/// # Safety
/// `buf` must point at `cap` writable bytes (the shell's line buffer,
/// mapped into the kernel's identity map).
pub unsafe fn read_line(buf: *mut u8, cap: usize) -> Line {
    let mut len = 0usize;
    loop {
        let Some(b) = next_byte() else {
            return if len == 0 { Line::Eof } else { Line::Read(len) };
        };
        match b {
            b'\r' | b'\n' => {
                echo_str("\r\n");
                return Line::Read(len);
            }
            0x7f | 0x08 => {
                if len > 0 {
                    len -= 1;
                    echo_str("\x08 \x08");
                }
            }
            0x03 => {
                // Ctrl-C: abandon the line.
                echo_str("^C\r\n");
                return Line::Read(0);
            }
            0x04 => {
                // Ctrl-D: EOF if the line is empty, otherwise ignored.
                if len == 0 {
                    return Line::Eof;
                }
            }
            0x20..=0x7e if len < cap => {
                // SAFETY: len < cap and buf has cap writable bytes.
                unsafe { buf.add(len).write(b) };
                len += 1;
                put_byte(b);
            }
            _ => {}
        }
    }
}
