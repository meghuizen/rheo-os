//! The renderer (docs/LIBRHEO.md Phase D): a buffered, high-performance output
//! layer. Editing produces many small writes (a keystroke repaints the line);
//! batching them into one buffer and flushing with a single `write` keeps the TX
//! path bounded - "submit N, one flush" - instead of a syscall per byte. Line
//! repaint erases to end-of-line (CSI K) so a shorter line clears leftover
//! characters (a minimal-diff repaint), and the cursor is positioned with an
//! absolute-column CSI. ANSI escapes are emitted here, in userland.

use alloc::string::String;
use alloc::vec::Vec;

use crate::sys;

/// A batched console renderer for a single editable line plus free-form output.
pub struct Renderer {
    out: Vec<u8>,
    prompt: String,
}

impl Renderer {
    pub fn new(prompt: &str) -> Renderer {
        Renderer {
            out: Vec::new(),
            prompt: String::from(prompt),
        }
    }

    /// Repaint the editable line: carriage-return, the prompt, the line text,
    /// erase to end of line, then position the cursor after `cursor` characters.
    /// Batched into one buffer and flushed with a single write.
    pub fn paint(&mut self, line: &str, cursor: usize) {
        self.out.clear();
        self.out.push(b'\r');
        // Disjoint field borrows: `out` (mut) and `prompt` (shared).
        self.out.extend_from_slice(self.prompt.as_bytes());
        self.out.extend_from_slice(line.as_bytes());
        self.out.extend_from_slice(b"\x1b[K"); // erase to end of line
        // Position: CR to column 1, then move right prompt_width + cursor.
        let col = self.prompt.chars().count() + cursor;
        self.out.push(b'\r');
        if col > 0 {
            self.out.extend_from_slice(b"\x1b[");
            push_uint(&mut self.out, col as u64);
            self.out.push(b'C'); // cursor forward `col` columns
        }
        self.flush();
    }

    /// Move to a fresh line (after committing input).
    pub fn newline(&mut self) {
        self.out.clear();
        self.out.extend_from_slice(b"\r\n");
        self.flush();
    }

    /// Print arbitrary text (program output between prompts).
    pub fn print(&mut self, s: &str) {
        self.out.clear();
        self.out.extend_from_slice(s.as_bytes());
        self.flush();
    }

    /// Write the batched buffer to stdout in one bounded loop (not per byte).
    fn flush(&mut self) {
        let mut off = 0;
        while off < self.out.len() {
            let n = sys::write(
                1,
                self.out[off..].as_ptr() as u64,
                (self.out.len() - off) as u64,
            );
            if n <= 0 {
                break;
            }
            off += n as usize;
        }
        self.out.clear();
    }
}

/// Append the decimal digits of `v` to `out` (no allocation, no `core::fmt`).
fn push_uint(out: &mut Vec<u8>, v: u64) {
    if v == 0 {
        out.push(b'0');
        return;
    }
    let mut digits = [0u8; 20];
    let mut i = 0;
    let mut n = v;
    while n > 0 {
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        out.push(digits[i]);
    }
}
