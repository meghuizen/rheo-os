//! A single-pass, zero-copy recursive-descent parser for RFC 8259 JSON. The
//! scalar core here runs everywhere (including in a cell); the string inner
//! loop goes through `crate::scan`, whose SIMD variant accelerates it on the
//! host. Strings without escapes borrow directly from the input; only escaped
//! strings allocate. Nesting is bounded by `MAX_DEPTH` so untrusted input
//! cannot blow the stack.

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{Error, ErrorKind};
use crate::value::{Number, Value};

/// Maximum object/array nesting. Deeper input is rejected (ErrorKind::Depth).
pub const MAX_DEPTH: usize = 256;

/// Parse a complete JSON document. On success the value borrows from `input`.
pub fn parse(input: &str) -> Result<Value<'_>, Error> {
    parse_bytes(input.as_bytes())
}

/// Parse from raw bytes (must be UTF-8 where strings are read).
pub fn parse_bytes(input: &[u8]) -> Result<Value<'_>, Error> {
    let mut p = Parser { input, pos: 0 };
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.pos != input.len() {
        return Err(p.err(ErrorKind::Trailing));
    }
    Ok(v)
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, kind: ErrorKind) -> Error {
        Error::new(kind, self.pos)
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value<'a>, Error> {
        match self.peek().ok_or_else(|| self.err(ErrorKind::Eof))? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => Ok(Value::String(self.string()?)),
            b't' => self.literal(b"true", Value::Bool(true)),
            b'f' => self.literal(b"false", Value::Bool(false)),
            b'n' => self.literal(b"null", Value::Null),
            b'-' | b'0'..=b'9' => Ok(Value::Number(self.number()?)),
            _ => Err(self.err(ErrorKind::Unexpected)),
        }
    }

    fn literal(&mut self, word: &[u8], v: Value<'a>) -> Result<Value<'a>, Error> {
        if self.input[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(self.err(ErrorKind::Unexpected))
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value<'a>, Error> {
        if depth + 1 > MAX_DEPTH {
            return Err(self.err(ErrorKind::Depth));
        }
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                Some(_) => return Err(self.err(ErrorKind::Unexpected)),
                None => return Err(self.err(ErrorKind::Eof)),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value<'a>, Error> {
        if depth + 1 > MAX_DEPTH {
            return Err(self.err(ErrorKind::Depth));
        }
        self.pos += 1; // '{'
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(members));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err(ErrorKind::Unexpected));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err(ErrorKind::Unexpected));
            }
            self.pos += 1; // ':'
            self.skip_ws();
            let val = self.value(depth + 1)?;
            members.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(members));
                }
                Some(_) => return Err(self.err(ErrorKind::Unexpected)),
                None => return Err(self.err(ErrorKind::Eof)),
            }
        }
    }

    /// Parse a string starting at the opening quote. Borrows the input slice
    /// when there are no escapes; otherwise decodes into an owned `String`.
    fn string(&mut self) -> Result<Cow<'a, str>, Error> {
        self.pos += 1; // opening '"'
        let start = self.pos;
        // Fast scan (SIMD-accelerated on the host): skip to the next quote,
        // backslash, or control byte. No escape until the closing quote means
        // the value borrows the input slice directly (zero copy).
        self.pos += crate::scan::string_event(&self.input[self.pos..]);
        match self.peek() {
            Some(b'"') => {
                let raw = &self.input[start..self.pos];
                self.pos += 1;
                let s = core::str::from_utf8(raw).map_err(|_| self.err(ErrorKind::Unicode))?;
                Ok(Cow::Borrowed(s))
            }
            // An escape: hand off to the owned/decoding path (self.pos is at
            // the backslash, and [start..pos] is the unescaped prefix).
            Some(b'\\') => self.string_escaped(start),
            Some(_) => Err(self.err(ErrorKind::String)), // control byte
            None => Err(self.err(ErrorKind::Eof)),
        }
    }

    /// Slow path: the string contains at least one escape; decode into a
    /// `String`, starting with the already-scanned unescaped prefix.
    fn string_escaped(&mut self, start: usize) -> Result<Cow<'a, str>, Error> {
        let prefix = core::str::from_utf8(&self.input[start..self.pos])
            .map_err(|_| self.err(ErrorKind::Unicode))?;
        let mut out = String::from(prefix);
        loop {
            let b = self.peek().ok_or_else(|| self.err(ErrorKind::Eof))?;
            match b {
                b'"' => {
                    self.pos += 1;
                    return Ok(Cow::Owned(out));
                }
                0x00..=0x1F => return Err(self.err(ErrorKind::String)),
                b'\\' => {
                    self.pos += 1;
                    let e = self.peek().ok_or_else(|| self.err(ErrorKind::Eof))?;
                    self.pos += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => self.unicode_escape(&mut out)?,
                        _ => return Err(self.err(ErrorKind::String)),
                    }
                }
                // A raw UTF-8 byte: copy the whole code point through.
                _ => {
                    let ch_start = self.pos;
                    self.pos += 1;
                    while let Some(c) = self.peek() {
                        if c & 0xC0 == 0x80 {
                            self.pos += 1; // continuation byte
                        } else {
                            break;
                        }
                    }
                    let s = core::str::from_utf8(&self.input[ch_start..self.pos])
                        .map_err(|_| self.err(ErrorKind::Unicode))?;
                    out.push_str(s);
                }
            }
        }
    }

    /// Decode a `\uXXXX` escape (the `\u` already consumed), including a
    /// surrogate pair `𝄞`.
    fn unicode_escape(&mut self, out: &mut String) -> Result<(), Error> {
        let hi = self.hex4()?;
        let cp = if (0xD800..=0xDBFF).contains(&hi) {
            // High surrogate: must be followed by \u low surrogate.
            if self.peek() != Some(b'\\') {
                return Err(self.err(ErrorKind::Unicode));
            }
            self.pos += 1;
            if self.peek() != Some(b'u') {
                return Err(self.err(ErrorKind::Unicode));
            }
            self.pos += 1;
            let lo = self.hex4()?;
            if !(0xDC00..=0xDFFF).contains(&lo) {
                return Err(self.err(ErrorKind::Unicode));
            }
            0x10000 + (((hi - 0xD800) as u32) << 10) + (lo - 0xDC00) as u32
        } else if (0xDC00..=0xDFFF).contains(&hi) {
            return Err(self.err(ErrorKind::Unicode)); // lone low surrogate
        } else {
            hi as u32
        };
        out.push(char::from_u32(cp).ok_or_else(|| self.err(ErrorKind::Unicode))?);
        Ok(())
    }

    fn hex4(&mut self) -> Result<u16, Error> {
        let mut v: u16 = 0;
        for _ in 0..4 {
            let b = self.peek().ok_or_else(|| self.err(ErrorKind::Eof))?;
            let d = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(self.err(ErrorKind::Unicode)),
            };
            v = (v << 4) | d as u16;
            self.pos += 1;
        }
        Ok(v)
    }

    fn number(&mut self) -> Result<Number, Error> {
        let start = self.pos;
        let neg = self.peek() == Some(b'-');
        if neg {
            self.pos += 1;
        }
        // Integer part: single 0, or 1-9 followed by digits.
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err(ErrorKind::Number)),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err(ErrorKind::Number));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err(ErrorKind::Number));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = core::str::from_utf8(&self.input[start..self.pos]).unwrap();
        if !is_float {
            // Try exact integer forms before falling back to float.
            if neg {
                if let Ok(i) = text.parse::<i64>() {
                    return Ok(Number::Signed(i));
                }
            } else if let Ok(u) = text.parse::<u64>() {
                return Ok(Number::Unsigned(u));
            }
        }
        text.parse::<f64>()
            .map(Number::Float)
            .map_err(|_| Error::new(ErrorKind::Number, start))
    }
}
