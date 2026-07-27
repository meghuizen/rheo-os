//! HPACK - HTTP/2 header compression (RFC 7541) - docs/NETSTACK.md §19.
//! From scratch, no dependency: the static table, a size-bounded dynamic table
//! with eviction, the prefix-integer representation, string literals with
//! optional Huffman coding ([`super::huffman`]), and an encoder + decoder.
//!
//! Correctness is pinned by the **RFC 7541 Appendix C** known-answer tests: the
//! `nethttp` proof decodes the RFC's own hex for C.2.1-C.2.4, C.3.1-C.3.3 and
//! C.4.1-C.4.3 into exactly the RFC's header lists, **and** re-encodes those
//! header lists into exactly the RFC's hex, checking the dynamic table's reported
//! size against the RFC's stated sizes (55 / 57 / 110 / 164) at each step.
//!
//! ## The parts that bite, and how they are handled
//! - **Indexing is 1-based and split**: index `1..=61` is the static table, index
//!   `62..` is the dynamic table counted from the **newest** entry. One helper
//!   ([`Table::get`]) owns that arithmetic so it cannot drift between encoder and
//!   decoder.
//! - **Entry size is `name.len() + value.len() + 32`** (RFC 7541 §4.1), not the
//!   wire size. Getting this wrong makes two peers evict at different moments and
//!   desynchronises every later index.
//! - **A dynamic table size update** (`001xxxxx`) may only appear at the start of
//!   a header block and may not exceed the decoder's advertised maximum; both are
//!   enforced.
//! - **Never-indexed** fields (`0001xxxx`) can be *emitted* with
//!   [`Mode::NeverIndex`], which is the representation to use for secrets. Honest
//!   limit: the **decoder does not surface the never-indexed bit** to its caller,
//!   so an intermediary built on this cannot yet guarantee it re-emits such a
//!   field never-indexed (a documented deferral, docs/NETSTACK.md §19).
//! - Integer decoding is bounded: a continuation run longer than would overflow a
//!   `usize` is [`HpackError::IntegerOverflow`], not a wrap.

use alloc::vec::Vec;

use super::huffman;

/// Every way an HPACK block can be rejected. All are `COMPRESSION_ERROR` at the
/// connection level (RFC 7541 §4.1 - HPACK errors are not recoverable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HpackError {
    /// The block ended mid-representation.
    Truncated,
    /// An index of 0, or one past the end of static + dynamic.
    BadIndex,
    /// A prefix integer whose continuation bytes would overflow.
    IntegerOverflow,
    /// A Huffman-coded string that is malformed, badly padded, or contains EOS.
    Huffman,
    /// A dynamic table size update larger than the decoder allows, or one that
    /// appeared after a header representation rather than at the block start.
    BadTableSizeUpdate,
    /// The decoded header list exceeded the decoder's field-count cap.
    TooManyFields,
}

/// The RFC 7541 Appendix A static table, indices 1..=61 (this array is 0-based, so
/// entry `i` here is HPACK index `i + 1`). Generated from the RFC text.
pub const STATIC_TABLE: [(&[u8], &[u8]); 61] = [
    (b":authority", b""),
    (b":method", b"GET"),
    (b":method", b"POST"),
    (b":path", b"/"),
    (b":path", b"/index.html"),
    (b":scheme", b"http"),
    (b":scheme", b"https"),
    (b":status", b"200"),
    (b":status", b"204"),
    (b":status", b"206"),
    (b":status", b"304"),
    (b":status", b"400"),
    (b":status", b"404"),
    (b":status", b"500"),
    (b"accept-charset", b""),
    (b"accept-encoding", b"gzip, deflate"),
    (b"accept-language", b""),
    (b"accept-ranges", b""),
    (b"accept", b""),
    (b"access-control-allow-origin", b""),
    (b"age", b""),
    (b"allow", b""),
    (b"authorization", b""),
    (b"cache-control", b""),
    (b"content-disposition", b""),
    (b"content-encoding", b""),
    (b"content-language", b""),
    (b"content-length", b""),
    (b"content-location", b""),
    (b"content-range", b""),
    (b"content-type", b""),
    (b"cookie", b""),
    (b"date", b""),
    (b"etag", b""),
    (b"expect", b""),
    (b"expires", b""),
    (b"from", b""),
    (b"host", b""),
    (b"if-match", b""),
    (b"if-modified-since", b""),
    (b"if-none-match", b""),
    (b"if-range", b""),
    (b"if-unmodified-since", b""),
    (b"last-modified", b""),
    (b"link", b""),
    (b"location", b""),
    (b"max-forwards", b""),
    (b"proxy-authenticate", b""),
    (b"proxy-authorization", b""),
    (b"range", b""),
    (b"referer", b""),
    (b"refresh", b""),
    (b"retry-after", b""),
    (b"server", b""),
    (b"set-cookie", b""),
    (b"strict-transport-security", b""),
    (b"transfer-encoding", b""),
    (b"user-agent", b""),
    (b"vary", b""),
    (b"via", b""),
    (b"www-authenticate", b""),
];

/// The per-entry overhead HPACK charges (RFC 7541 §4.1).
pub const ENTRY_OVERHEAD: usize = 32;
/// The default dynamic table capacity (RFC 9113 SETTINGS_HEADER_TABLE_SIZE default).
pub const DEFAULT_TABLE_SIZE: usize = 4096;
/// A cap on the number of fields one decoded block may carry.
pub const MAX_FIELDS: usize = 128;

/// A decoded header list: owned `(name, value)` pairs in wire order.
pub type HeaderList = Vec<(Vec<u8>, Vec<u8>)>;

/// One dynamic table entry (owned - it must outlive the block that inserted it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

impl Entry {
    fn size(&self) -> usize {
        self.name.len() + self.value.len() + ENTRY_OVERHEAD
    }
}

/// The HPACK indexing context: the fixed static table plus a FIFO dynamic table
/// bounded by `capacity` bytes. `entries[0]` is the **newest** entry, which is what
/// makes dynamic index `62` mean "most recently added".
#[derive(Debug, Clone)]
pub struct Table {
    entries: Vec<Entry>,
    size: usize,
    capacity: usize,
}

impl Default for Table {
    fn default() -> Table {
        Table::new(DEFAULT_TABLE_SIZE)
    }
}

impl Table {
    pub fn new(capacity: usize) -> Table {
        Table {
            entries: Vec::new(),
            size: 0,
            capacity,
        }
    }

    /// The dynamic table's current size in HPACK accounting bytes - the value the
    /// RFC 7541 Appendix C examples print as "Table size".
    pub fn size(&self) -> usize {
        self.size
    }

    /// The current capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of dynamic entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Set the capacity, evicting from the oldest end until the table fits.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        self.evict();
    }

    fn evict(&mut self) {
        while self.size > self.capacity {
            match self.entries.pop() {
                Some(e) => self.size -= e.size(),
                None => {
                    self.size = 0;
                    break;
                }
            }
        }
    }

    /// Insert a new entry at the newest end. An entry larger than the whole
    /// capacity empties the table and is not inserted (RFC 7541 §4.4).
    pub fn insert(&mut self, name: &[u8], value: &[u8]) {
        let e = Entry {
            name: name.to_vec(),
            value: value.to_vec(),
        };
        let sz = e.size();
        if sz > self.capacity {
            self.entries.clear();
            self.size = 0;
            return;
        }
        self.entries.insert(0, e);
        self.size += sz;
        self.evict();
    }

    /// Look up a 1-based HPACK index across static then dynamic.
    pub fn get(&self, index: usize) -> Option<(&[u8], &[u8])> {
        if index == 0 {
            return None;
        }
        if index <= STATIC_TABLE.len() {
            let (n, v) = STATIC_TABLE[index - 1];
            return Some((n, v));
        }
        let d = index - STATIC_TABLE.len() - 1;
        self.entries
            .get(d)
            .map(|e| (e.name.as_slice(), e.value.as_slice()))
    }

    /// The lowest index whose (name, value) both match, else the lowest index
    /// whose name matches, else `None`. Static entries are searched first so the
    /// encoder prefers them - which is what reproduces the RFC's own encodings.
    pub fn find(&self, name: &[u8], value: &[u8]) -> (Option<usize>, Option<usize>) {
        let mut name_only = None;
        for (i, (n, v)) in STATIC_TABLE.iter().enumerate() {
            if *n == name {
                if *v == value {
                    return (Some(i + 1), None);
                }
                if name_only.is_none() {
                    name_only = Some(i + 1);
                }
            }
        }
        for (d, e) in self.entries.iter().enumerate() {
            let idx = STATIC_TABLE.len() + 1 + d;
            if e.name == name {
                if e.value == value {
                    return (Some(idx), None);
                }
                if name_only.is_none() {
                    name_only = Some(idx);
                }
            }
        }
        (None, name_only)
    }
}

// ---------------------------------------------------------------------------
// Prefix integers (RFC 7541 §5.1)
// ---------------------------------------------------------------------------

/// Encode `value` with an `n`-bit prefix, OR-ing `prefix_bits` into the high bits
/// of the first octet. Proven against RFC 7541 C.1.1-C.1.3.
pub fn encode_int(out: &mut Vec<u8>, n: u32, prefix_bits: u8, value: usize) {
    let max = (1usize << n) - 1;
    if value < max {
        out.push(prefix_bits | value as u8);
        return;
    }
    out.push(prefix_bits | max as u8);
    let mut v = value - max;
    while v >= 128 {
        out.push(((v % 128) + 128) as u8);
        v /= 128;
    }
    out.push(v as u8);
}

/// Decode an `n`-bit-prefix integer at `*pos`, advancing `*pos`.
pub fn decode_int(buf: &[u8], pos: &mut usize, n: u32) -> Result<usize, HpackError> {
    let first = *buf.get(*pos).ok_or(HpackError::Truncated)?;
    *pos += 1;
    let max = (1usize << n) - 1;
    let mut value = (first as usize) & max;
    if value < max {
        return Ok(value);
    }
    let mut shift = 0u32;
    loop {
        let b = *buf.get(*pos).ok_or(HpackError::Truncated)?;
        *pos += 1;
        // 7 fresh bits per continuation octet; bail before a usize would wrap.
        if shift >= usize::BITS {
            return Err(HpackError::IntegerOverflow);
        }
        let add = ((b & 0x7f) as usize)
            .checked_shl(shift)
            .ok_or(HpackError::IntegerOverflow)?;
        value = value.checked_add(add).ok_or(HpackError::IntegerOverflow)?;
        if b & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

// ---------------------------------------------------------------------------
// String literals (RFC 7541 §5.2)
// ---------------------------------------------------------------------------

/// Encode a string literal, Huffman coding it when `huffman` is set **and** the
/// coded form is strictly shorter (the standard encoder rule; for every RFC 7541
/// Appendix C.4 value it is shorter, so this reproduces the RFC bytes exactly).
pub fn encode_string(out: &mut Vec<u8>, s: &[u8], huffman_allowed: bool) {
    if huffman_allowed && huffman::encoded_len(s) < s.len() {
        let coded = huffman::encode(s);
        encode_int(out, 7, 0x80, coded.len());
        out.extend_from_slice(&coded);
    } else {
        encode_int(out, 7, 0x00, s.len());
        out.extend_from_slice(s);
    }
}

/// Decode a string literal at `*pos`, advancing `*pos`.
pub fn decode_string(buf: &[u8], pos: &mut usize) -> Result<Vec<u8>, HpackError> {
    let huff = (*buf.get(*pos).ok_or(HpackError::Truncated)? & 0x80) != 0;
    let len = decode_int(buf, pos, 7)?;
    let end = pos.checked_add(len).ok_or(HpackError::IntegerOverflow)?;
    if end > buf.len() {
        return Err(HpackError::Truncated);
    }
    let raw = &buf[*pos..end];
    *pos = end;
    if huff {
        huffman::decode(raw)
    } else {
        Ok(raw.to_vec())
    }
}

// ---------------------------------------------------------------------------
// Encoder / decoder
// ---------------------------------------------------------------------------

/// How one field should be represented on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Literal **with** incremental indexing (`01xxxxxx`), or a plain indexed
    /// field if the whole (name, value) pair is already in a table.
    Indexed,
    /// Literal **without** indexing (`0000xxxx`) - not added to the table.
    NoIndex,
    /// Literal **never** indexed (`0001xxxx`) - not added, and an intermediary
    /// must keep it that way. The representation for secrets.
    NeverIndex,
}

/// The HPACK encoder: one per connection direction (its table must stay in step
/// with the peer's decoder table).
pub struct Encoder {
    table: Table,
    huffman: bool,
}

impl Encoder {
    /// A new encoder. `huffman` selects whether string literals are Huffman coded.
    pub fn new(capacity: usize, huffman: bool) -> Encoder {
        Encoder {
            table: Table::new(capacity),
            huffman,
        }
    }

    /// The encoder's dynamic table (for the Appendix C size assertions).
    pub fn table(&self) -> &Table {
        &self.table
    }

    /// Emit a dynamic table size update and apply it.
    pub fn emit_table_size_update(&mut self, out: &mut Vec<u8>, capacity: usize) {
        encode_int(out, 5, 0x20, capacity);
        self.table.set_capacity(capacity);
    }

    /// Encode a header list, all fields with [`Mode::Indexed`].
    pub fn encode(&mut self, fields: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (n, v) in fields {
            self.encode_field(&mut out, n, v, Mode::Indexed);
        }
        out
    }

    /// Encode a header list with a per-field representation choice.
    pub fn encode_with_modes(&mut self, fields: &[(&[u8], &[u8], Mode)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (n, v, m) in fields {
            self.encode_field(&mut out, n, v, *m);
        }
        out
    }

    /// Encode one field into `out`.
    pub fn encode_field(&mut self, out: &mut Vec<u8>, name: &[u8], value: &[u8], mode: Mode) {
        let (full, name_only) = self.table.find(name, value);
        if let (Some(idx), Mode::Indexed) = (full, mode) {
            // Indexed Header Field: `1xxxxxxx`.
            encode_int(out, 7, 0x80, idx);
            return;
        }
        let (prefix_len, prefix_bits) = match mode {
            Mode::Indexed => (6u32, 0x40u8),
            Mode::NoIndex => (4, 0x00),
            Mode::NeverIndex => (4, 0x10),
        };
        match name_only.or(full) {
            Some(idx) => encode_int(out, prefix_len, prefix_bits, idx),
            None => {
                encode_int(out, prefix_len, prefix_bits, 0);
                encode_string(out, name, self.huffman);
            }
        }
        encode_string(out, value, self.huffman);
        if mode == Mode::Indexed {
            self.table.insert(name, value);
        }
    }
}

/// The HPACK decoder: one per connection direction.
pub struct Decoder {
    table: Table,
    max_capacity: usize,
}

impl Decoder {
    pub fn new(max_capacity: usize) -> Decoder {
        Decoder {
            table: Table::new(max_capacity),
            max_capacity,
        }
    }

    /// The decoder's dynamic table (for the Appendix C size assertions).
    pub fn table(&self) -> &Table {
        &self.table
    }

    /// Decode a complete header block into owned `(name, value)` pairs. The
    /// dynamic table is mutated exactly as the block directs, so a sequence of
    /// blocks on one connection decodes correctly (RFC 7541 Appendix C.3/C.4).
    pub fn decode(&mut self, buf: &[u8]) -> Result<HeaderList, HpackError> {
        let mut out: HeaderList = Vec::new();
        let mut pos = 0usize;
        // A dynamic table size update is only legal before the first field.
        let mut updates_allowed = true;
        while pos < buf.len() {
            let b = buf[pos];
            if b & 0x80 != 0 {
                // Indexed Header Field.
                let idx = decode_int(buf, &mut pos, 7)?;
                let (n, v) = self.table.get(idx).ok_or(HpackError::BadIndex)?;
                out.push((n.to_vec(), v.to_vec()));
                updates_allowed = false;
            } else if b & 0xc0 == 0x40 {
                // Literal with incremental indexing.
                let idx = decode_int(buf, &mut pos, 6)?;
                let (name, value) = self.literal(buf, &mut pos, idx)?;
                self.table.insert(&name, &value);
                out.push((name, value));
                updates_allowed = false;
            } else if b & 0xe0 == 0x20 {
                // Dynamic table size update.
                if !updates_allowed {
                    return Err(HpackError::BadTableSizeUpdate);
                }
                let cap = decode_int(buf, &mut pos, 5)?;
                if cap > self.max_capacity {
                    return Err(HpackError::BadTableSizeUpdate);
                }
                self.table.set_capacity(cap);
            } else {
                // Literal without indexing (0000) or never indexed (0001).
                let idx = decode_int(buf, &mut pos, 4)?;
                let (name, value) = self.literal(buf, &mut pos, idx)?;
                out.push((name, value));
                updates_allowed = false;
            }
            if out.len() > MAX_FIELDS {
                return Err(HpackError::TooManyFields);
            }
        }
        Ok(out)
    }

    /// The shared tail of every literal representation: an indexed name (`idx` non
    /// zero) or a literal name, then a literal value.
    fn literal(
        &self,
        buf: &[u8],
        pos: &mut usize,
        idx: usize,
    ) -> Result<(Vec<u8>, Vec<u8>), HpackError> {
        let name = if idx == 0 {
            decode_string(buf, pos)?
        } else {
            self.table.get(idx).ok_or(HpackError::BadIndex)?.0.to_vec()
        };
        let value = decode_string(buf, pos)?;
        Ok((name, value))
    }
}
