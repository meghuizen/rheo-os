//! The HPACK Huffman code (RFC 7541 Appendix B) - docs/NETSTACK.md §19.
//!
//! ## The table is generated, never transcribed
//! The 257-row table below was produced mechanically from the authoritative RFC
//! 7541 text (fetched and blob-hash cross-checked against several independent
//! mirrors, then parsed row by row). Hand-typing 257 codes is exactly how a
//! Huffman implementation acquires a silent bug in one rare symbol, so it was not
//! done. The generator also asserted three properties of the extracted table
//! before emitting it: every code fits in its stated bit length, the code is
//! **prefix-free**, and within each bit length the codes are **consecutive**
//! integers (i.e. it is canonical) - which is what makes the decoder below
//! correct and table-small.
//!
//! ## Canonical decoding
//! Because codes of one length are consecutive, a decoder needs no tree: read one
//! bit at a time into `(code, len)`, and at each length check whether `code` falls
//! in that length's range `[FIRST_CODE[len], FIRST_CODE[len] + CODE_COUNT[len])`.
//! If so the symbol is `SORTED_SYMS[FIRST_INDEX[len] + code - FIRST_CODE[len]]`.
//! Lengths run 5..=30 bits, so at most 30 iterations per symbol.
//!
//! ## Padding rules (security-relevant)
//! A Huffman-coded string is padded to a byte boundary with the **most
//! significant bits of the EOS code**, which are all ones (RFC 7541 §5.2). The
//! decoder therefore rejects: padding longer than 7 bits, padding that is not all
//! ones, and any actual decoded EOS symbol. Accepting those would let two peers
//! disagree about a header's value - the HPACK analogue of a smuggling desync.

use alloc::vec::Vec;

use super::hpack::HpackError;

include!("huffman_table.rs");

/// The EOS symbol index (RFC 7541 Appendix B): never a real decoded byte.
const EOS: u16 = 256;

/// The number of bytes `data` occupies when Huffman coded.
pub fn encoded_len(data: &[u8]) -> usize {
    let bits: usize = data.iter().map(|&b| HUFF_LEN[b as usize] as usize).sum();
    bits.div_ceil(8)
}

/// Huffman-encode `data`, padding the final byte with EOS's leading one bits.
pub fn encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(encoded_len(data));
    // A 64-bit accumulator holds up to 30 fresh bits plus <8 carried bits.
    let mut acc: u64 = 0;
    let mut nbits: u32 = 0;
    for &b in data {
        let code = HUFF_CODE[b as usize] as u64;
        let len = HUFF_LEN[b as usize] as u32;
        acc = (acc << len) | code;
        nbits += len;
        while nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    if nbits > 0 {
        let pad = 8 - nbits;
        // Pad with ones (the EOS prefix).
        let byte = ((acc << pad) as u8) | ((1u8 << pad) - 1);
        out.push(byte);
    }
    out
}

/// Huffman-decode `data`, enforcing the padding rules above.
pub fn decode(data: &[u8]) -> Result<Vec<u8>, HpackError> {
    let mut out = Vec::with_capacity(data.len() * 8 / 5);
    let mut code: u32 = 0;
    let mut len: u32 = 0;
    for &byte in data {
        for bit in (0..8).rev() {
            code = (code << 1) | ((byte >> bit) as u32 & 1);
            len += 1;
            if len > MAX_CODE_LEN {
                return Err(HpackError::Huffman);
            }
            if len < MIN_CODE_LEN {
                continue;
            }
            let l = len as usize;
            let count = CODE_COUNT[l] as u32;
            if count == 0 {
                continue;
            }
            let first = FIRST_CODE[l];
            if code >= first && code - first < count {
                let sym = SORTED_SYMS[FIRST_INDEX[l] as usize + (code - first) as usize];
                if sym == EOS {
                    // A literal EOS in the data stream is a decoding error.
                    return Err(HpackError::Huffman);
                }
                out.push(sym as u8);
                code = 0;
                len = 0;
            }
        }
    }
    // Whatever is left must be <8 bits of all-ones EOS padding.
    if len >= 8 {
        return Err(HpackError::Huffman);
    }
    if len > 0 && code != (1u32 << len) - 1 {
        return Err(HpackError::Huffman);
    }
    Ok(out)
}
