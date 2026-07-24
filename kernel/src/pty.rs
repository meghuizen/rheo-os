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

use crate::arch;

/// Where the line discipline reads keystrokes from.
enum Source {
    /// Live serial console (interactive `cargo xtask run --bin lsh`).
    Serial,
    /// A fixed script consumed byte by byte (headless tests). The usize is
    /// the read cursor.
    Script(&'static [u8], usize),
}

static mut SOURCE: Source = Source::Serial;

/// Install a scripted input source (for deterministic headless tests).
/// The script is played as if typed; end of script is end of input.
pub fn install_script(script: &'static [u8]) {
    unsafe {
        *core::ptr::addr_of_mut!(SOURCE) = Source::Script(script, 0);
    }
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
    // SAFETY: single CPU, synchronous; SOURCE is only touched here and in
    // install_script (before any cell runs).
    let src = unsafe { &mut *core::ptr::addr_of_mut!(SOURCE) };
    match src {
        Source::Serial => loop {
            if let Some(b) = arch::serial_read_byte() {
                return Some(b);
            }
            core::hint::spin_loop();
        },
        Source::Script(data, pos) => {
            if *pos >= data.len() {
                None
            } else {
                let b = data[*pos];
                *pos += 1;
                Some(b)
            }
        }
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
