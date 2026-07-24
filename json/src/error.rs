//! Parse errors: a reason plus the byte offset where parsing stopped.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrorKind {
    /// The input ended while a value/string/token was still expected.
    Eof,
    /// A byte that cannot appear here.
    Unexpected,
    /// A malformed number.
    Number,
    /// A malformed string (bad escape, control char, or invalid UTF-8).
    String,
    /// A `\u` escape or raw bytes that are not valid Unicode/UTF-8.
    Unicode,
    /// Nesting deeper than the parser's fixed limit.
    Depth,
    /// Extra non-whitespace bytes after a complete top-level value.
    Trailing,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    pub kind: ErrorKind,
    /// Byte offset into the input where the error was detected.
    pub offset: usize,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, offset: usize) -> Error {
        Error { kind, offset }
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "json: {:?} at byte {}", self.kind, self.offset)
    }
}
