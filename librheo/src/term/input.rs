//! The input decoder: a raw console byte stream -> typed [`Key`] events
//! (docs/LIBRHEO.md Phase D, docs/SHELL.md 1). Handles CSI (`ESC [ ...`) and SS3
//! (`ESC O ...`) escape sequences for the editing keys, UTF-8 codepoints, and
//! control characters. Escape/keymap decoding is deliberately **userland**, not
//! kernel (SHELL.md 1). `next_key().await` parks on the input completion via the
//! reactor (`rt::read_console`), so an idle shell costs no CPU where the UART RX
//! interrupt is wired, and polls otherwise.

use crate::rt;

/// A decoded key press. The typed layer over the raw byte stream (which stays
/// the primary API - `rt::read_console`). Grapheme clustering beyond a single
/// codepoint is out of scope (documented); `Char` carries one Unicode scalar.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Key {
    /// A printable character (one Unicode scalar value).
    Char(char),
    /// Enter / Return (CR or LF).
    Enter,
    /// Backspace (DEL 0x7f or BS 0x08).
    Backspace,
    /// Tab.
    Tab,
    /// A bare Escape key press.
    Esc,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Delete,
    Insert,
    PageUp,
    PageDown,
    /// Function key F1..F12.
    F(u8),
    /// A control character, e.g. `Ctrl('c')` for `^C` (a..z).
    Ctrl(char),
    /// An unrecognised byte or sequence.
    Unknown,
}

/// A streaming key decoder over the raw console. Holds a small byte buffer, refills
/// it from `rt::read_console` when the parser needs more, and yields one [`Key`]
/// per `next_key`. Byte-oriented so a multi-byte escape or UTF-8 sequence that
/// spans two reads is reassembled correctly.
pub struct KeyReader {
    buf: [u8; 64],
    len: usize, // valid bytes in buf
    pos: usize, // parse cursor within buf
    eof: bool,
}

impl Default for KeyReader {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyReader {
    pub fn new() -> KeyReader {
        KeyReader {
            buf: [0; 64],
            len: 0,
            pos: 0,
            eof: false,
        }
    }

    /// Decode the next key, parking until input is available. Returns `None` at
    /// end of input (the source closed / a test script exhausted).
    pub async fn next_key(&mut self) -> Option<Key> {
        loop {
            if let Some(k) = self.try_parse() {
                return Some(k);
            }
            if self.eof {
                return None;
            }
            self.fill().await;
        }
    }

    /// Compact consumed bytes and read more from the console (parking).
    async fn fill(&mut self) {
        if self.pos > 0 {
            self.buf.copy_within(self.pos..self.len, 0);
            self.len -= self.pos;
            self.pos = 0;
        }
        if self.len >= self.buf.len() {
            return; // buffer full; the parser must consume before we read more
        }
        // SAFETY: `buf[len..]` is `buf.len()-len` writable bytes that outlive the
        // await (the buffer is owned by this reader, pinned in the strand).
        let ptr = unsafe { self.buf.as_mut_ptr().add(self.len) };
        let n = rt::read_console(ptr, self.buf.len() - self.len).await;
        if n == 0 {
            self.eof = true;
        } else {
            self.len += n;
        }
    }

    /// Try to parse one key from `buf[pos..len]`. Returns `None` if more bytes
    /// are needed (an incomplete escape / UTF-8 sequence) and the source is not
    /// yet at EOF.
    fn try_parse(&mut self) -> Option<Key> {
        if self.pos >= self.len {
            return None;
        }
        let b = self.buf[self.pos];
        match b {
            0x1b => self.parse_escape(),
            b'\r' | b'\n' => {
                self.pos += 1;
                Some(Key::Enter)
            }
            0x7f | 0x08 => {
                self.pos += 1;
                Some(Key::Backspace)
            }
            b'\t' => {
                self.pos += 1;
                Some(Key::Tab)
            }
            // Other control chars 0x01..=0x1a -> Ctrl('a'..'z') (BS/Tab/CR/LF
            // already handled above).
            0x01..=0x1a => {
                self.pos += 1;
                Some(Key::Ctrl((b'a' + (b - 1)) as char))
            }
            0x20..=0x7e => {
                self.pos += 1;
                Some(Key::Char(b as char))
            }
            0x80..=0xff => self.parse_utf8(),
            _ => {
                self.pos += 1;
                Some(Key::Unknown)
            }
        }
    }

    /// `buf[pos] == ESC`. A CSI (`[`) or SS3 (`O`) introducer starts a sequence;
    /// a lone ESC is the Escape key (only resolved once no more bytes will come).
    fn parse_escape(&mut self) -> Option<Key> {
        if self.len - self.pos < 2 {
            if self.eof {
                self.pos += 1;
                return Some(Key::Esc);
            }
            return None; // wait for the introducer
        }
        match self.buf[self.pos + 1] {
            b'[' => self.parse_csi(),
            b'O' => self.parse_ss3(),
            _ => {
                self.pos += 1;
                Some(Key::Esc)
            }
        }
    }

    /// `ESC [ params final`, where `final` is a byte in `0x40..=0x7e`.
    fn parse_csi(&mut self) -> Option<Key> {
        let start = self.pos + 2;
        let mut i = start;
        while i < self.len {
            let c = self.buf[i];
            if (0x40..=0x7e).contains(&c) {
                let key = csi_key(&self.buf[start..i], c);
                self.pos = i + 1;
                return Some(key);
            }
            i += 1;
        }
        if self.eof {
            self.pos = self.len;
            return Some(Key::Unknown);
        }
        None
    }

    /// `ESC O final` - the SS3 form (application-mode arrows, F1..F4).
    fn parse_ss3(&mut self) -> Option<Key> {
        if self.len - self.pos < 3 {
            if self.eof {
                self.pos = self.len;
                return Some(Key::Unknown);
            }
            return None;
        }
        let c = self.buf[self.pos + 2];
        self.pos += 3;
        Some(match c {
            b'A' => Key::Up,
            b'B' => Key::Down,
            b'C' => Key::Right,
            b'D' => Key::Left,
            b'H' => Key::Home,
            b'F' => Key::End,
            b'P' => Key::F(1),
            b'Q' => Key::F(2),
            b'R' => Key::F(3),
            b'S' => Key::F(4),
            _ => Key::Unknown,
        })
    }

    /// A UTF-8 codepoint starting at `buf[pos]` (lead byte >= 0x80).
    fn parse_utf8(&mut self) -> Option<Key> {
        let b0 = self.buf[self.pos];
        let need = if b0 >= 0xf0 {
            4
        } else if b0 >= 0xe0 {
            3
        } else if b0 >= 0xc0 {
            2
        } else {
            1 // stray continuation byte
        };
        if need == 1 {
            self.pos += 1;
            return Some(Key::Unknown);
        }
        if self.len - self.pos < need {
            if self.eof {
                self.pos += 1;
                return Some(Key::Unknown);
            }
            return None;
        }
        let ch = core::str::from_utf8(&self.buf[self.pos..self.pos + need])
            .ok()
            .and_then(|s| s.chars().next());
        self.pos += need;
        Some(match ch {
            Some(c) => Key::Char(c),
            None => Key::Unknown,
        })
    }
}

/// Map a CSI `params` + `final` byte to a [`Key`].
fn csi_key(params: &[u8], final_b: u8) -> Key {
    match final_b {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'~' => match params {
            b"1" | b"7" => Key::Home,
            b"4" | b"8" => Key::End,
            b"2" => Key::Insert,
            b"3" => Key::Delete,
            b"5" => Key::PageUp,
            b"6" => Key::PageDown,
            b"15" => Key::F(5),
            b"17" => Key::F(6),
            b"18" => Key::F(7),
            b"19" => Key::F(8),
            b"20" => Key::F(9),
            b"21" => Key::F(10),
            b"23" => Key::F(11),
            b"24" => Key::F(12),
            _ => Key::Unknown,
        },
        _ => Key::Unknown,
    }
}
