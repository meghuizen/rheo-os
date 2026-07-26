//! The credited multi-source entropy pool behind the root DRBG
//! (docs/TIME-IDENTITY.md 4). Design rules, in order:
//!
//! - **Many sources, one pool.** Every available source is mixed in (mixing
//!   can never reduce the pool's entropy); each source earns *credit*
//!   separately and conservatively. This is Linux's post-5.18 stance rather
//!   than trust-one-instruction: a compromised or absent source degrades the
//!   credit ledger, never the pool.
//! - **Hard gate.** Nothing downstream may draw until `THRESHOLD_BITS` of
//!   credit has accumulated. There is no weak fallback that silently counts.
//! - **Conservative credit.** Hardware RNG words are credited only after the
//!   branchless SP 800-90B-style health tests pass; jitter is credited at
//!   1/4 bit per noisy sample (well under the ~1 bit 90B would allow); event
//!   timing is mixed at zero credit.
//! - **No secret-dependent branches.** The mixing and health-test paths are
//!   written with constant-time compares (`ct_eq64`) so neither branch
//!   predictors nor timing leak the material being examined. Loop bounds
//!   depend only on public lengths.
//!
//! The conditioner is ChaCha20-based (the one primitive this kernel ships):
//! each 32-byte chunk of input is folded into the 256-bit pool key by
//! XOR-ing it with a keystream block keyed by the current pool state - the
//! same shape as `Drbg::reseed`. This is a documented ad-hoc sponge, not a
//! NIST-listed conditioning function; what makes it sound is that ChaCha20
//! acts as the PRF and every input byte influences the key.

use super::chacha;
use crate::arch;

/// Credit required before the pool will hand out a key (256-bit strength
/// with margin; the "hard getrandom-blocking guarantee").
pub const THRESHOLD_BITS: u32 = 256;

/// Cap on the ledger so it cannot wrap (credit beyond this is meaningless).
const CREDIT_CAP: u32 = 4096;

/// Entropy sources, as bit positions in `sources()` / `SeedReport::sources`.
/// `Tpm` and `BoardTrng` are reserved slots for drivers that do not exist
/// yet (docs/TIME-IDENTITY.md 4 deferred list).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Source {
    HwRng = 0,
    FirmwareSeed = 1,
    VirtioRng = 2,
    Jitter = 3,
    Event = 4,
    Tpm = 5,
    BoardTrng = 6,
}

/// Human-readable names, index-matched to the `Source` bit positions.
pub const SOURCE_NAMES: [&str; 7] = [
    "hwrng",
    "fw-seed",
    "virtio-rng",
    "jitter",
    "event",
    "tpm",
    "board-trng",
];

/// The pool: a 256-bit key absorbing input, plus the credit ledger.
pub struct EntropyPool {
    key: [u8; 32],
    credited: u32,
    sources: u32,
}

impl Default for EntropyPool {
    fn default() -> EntropyPool {
        EntropyPool::new()
    }
}

impl EntropyPool {
    pub const fn new() -> EntropyPool {
        EntropyPool {
            key: [0; 32],
            credited: 0,
            sources: 0,
        }
    }

    /// Fold one 32-byte chunk into the pool key (ChaCha20 PRF + XOR).
    fn mix32(&mut self, chunk: &[u8; 32]) {
        let mut n = [0u8; 12];
        n.copy_from_slice(&chunk[..12]);
        let ctr = u32::from_le_bytes([chunk[12], chunk[13], chunk[14], chunk[15]]);
        let mut blk = [0u8; 64];
        chacha::block(&self.key, ctr, &n, &mut blk);
        let mut i = 0;
        while i < 32 {
            self.key[i] = blk[i] ^ chunk[i];
            i += 1;
        }
        // Wipe the keystream copy (it determines the new key).
        for b in blk.iter_mut() {
            unsafe { core::ptr::write_volatile(b, 0) };
        }
    }

    /// Absorb `data`, crediting `credit_bits` to the ledger and recording
    /// the source. Zero-credit absorbs are welcome (mixing never hurts).
    /// A domain-separation header chunk (source id + length) goes in first
    /// so concatenation across sources is unambiguous.
    pub fn absorb(&mut self, src: Source, data: &[u8], credit_bits: u32) {
        let mut hdr = [0u8; 32];
        hdr[0] = 0x52; // 'R', domain tag
        hdr[1] = src as u8;
        hdr[2..10].copy_from_slice(&(data.len() as u64).to_le_bytes());
        self.mix32(&hdr);

        let mut off = 0;
        while off < data.len() {
            let n = core::cmp::min(32, data.len() - off);
            let mut chunk = [0u8; 32];
            chunk[..n].copy_from_slice(&data[off..off + n]);
            self.mix32(&chunk);
            off += n;
        }
        self.credited = core::cmp::min(self.credited.saturating_add(credit_bits), CREDIT_CAP);
        self.sources |= 1 << (src as u32);
    }

    /// Credited entropy bits gathered so far.
    pub fn credited_bits(&self) -> u32 {
        self.credited
    }

    /// Bitmask of sources that have contributed (credited or not).
    pub fn sources(&self) -> u32 {
        self.sources
    }

    /// Has the hard gate been passed?
    pub fn ready(&self) -> bool {
        self.credited >= THRESHOLD_BITS
    }

    /// Squeeze a 256-bit key out, ratcheting the pool forward (the pool key
    /// is replaced by fresh keystream, so the squeezed key cannot be
    /// recovered from the pool afterwards). Returns None before the gate.
    pub fn squeeze_key(&mut self) -> Option<[u8; 32]> {
        if !self.ready() {
            return None;
        }
        let nonce = [0xA5u8; 12]; // distinct domain from mix32 inputs
        let mut blk = [0u8; 64];
        chacha::block(&self.key, u32::MAX, &nonce, &mut blk);
        self.key.copy_from_slice(&blk[..32]);
        let mut out = [0u8; 32];
        out.copy_from_slice(&blk[32..]);
        for b in blk.iter_mut() {
            unsafe { core::ptr::write_volatile(b, 0) };
        }
        Some(out)
    }
}

// ------------------------------------------------ constant-time helpers

/// 1 if a == b else 0, with no data-dependent branch.
#[inline]
pub fn ct_eq64(a: u64, b: u64) -> u64 {
    let x = a ^ b;
    // x | -x has the sign bit set iff x != 0.
    1 ^ ((x | x.wrapping_neg()) >> 63)
}

// ------------------------------------------------ hwrng health tests

/// SP 800-90B-style health checks on full-entropy 64-bit words, written
/// branchless: Repetition Count (no consecutive repeats), Adaptive
/// Proportion (no value over-represented in the window), and stuck-at
/// (bits vary). The counts accumulate with `ct_eq64`; only the final
/// verdict branches, on aggregates, never on the raw values.
pub fn health_ok(s: &[u64]) -> bool {
    if s.len() < 2 {
        return false;
    }
    // Repetition Count Test.
    let mut repeats = 0u64;
    let mut i = 1;
    while i < s.len() {
        repeats += ct_eq64(s[i], s[i - 1]);
        i += 1;
    }
    // Adaptive Proportion Test: with full-entropy 64-bit words even two
    // matches in the window are astronomically unlikely; cap occurrences.
    let cutoff = (s.len() as u64 / 8).max(2);
    let mut max_count = 0u64;
    let mut i = 0;
    while i < s.len() {
        let mut c = 0u64;
        let mut j = 0;
        while j < s.len() {
            c += ct_eq64(s[j], s[i]);
            j += 1;
        }
        // max_count = max(max_count, c), branchless.
        let gt = 1 ^ ct_eq64(c.wrapping_sub(max_count) >> 63, 1); // 1 if c >= max_count
        max_count = gt * c + (1 - gt) * max_count;
        i += 1;
    }
    // Stuck-at: no bit constant across the window.
    let mut orv = 0u64;
    let mut andv = u64::MAX;
    for &v in s {
        orv |= v;
        andv &= v;
    }
    repeats == 0 && max_count <= cutoff && orv != 0 && andv != u64::MAX
}

// ------------------------------------------------ jitter entropy

/// Conservative credit estimate for a window of raw timing deltas:
/// delta-of-delta, low nibble must be neither 0x0 nor 0xF to count as
/// noisy, 1/4 bit credited per noisy sample (SP 800-90B would allow ~1).
/// Branchless per sample. Under QEMU `-icount` deltas are constant, the
/// noisy count is 0, and the credit is honestly 0.
pub fn estimate_jitter_bits(deltas: &[u64]) -> u32 {
    if deltas.len() < 2 {
        return 0;
    }
    let mut noisy = 0u64;
    let mut i = 1;
    while i < deltas.len() {
        let d2 = deltas[i].wrapping_sub(deltas[i - 1]);
        let low = d2 & 0xF;
        let ne0 = 1 ^ ct_eq64(low, 0x0);
        let nef = 1 ^ ct_eq64(low, 0xF);
        noisy += ne0 & nef;
        i += 1;
    }
    (noisy / 4) as u32
}

/// One jitter round: time a data-dependent ALU/memory loop with the cycle
/// counter, absorb the raw deltas, and return the credited bits.
const JITTER_WINDOW: usize = 64;
/// Per-round credit cap - even a wildly noisy counter earns at most this.
const JITTER_ROUND_CAP: u32 = 16;

fn jitter_round(pool: &mut EntropyPool, salt: u64) -> u32 {
    let mut deltas = [0u64; JITTER_WINDOW];
    let mut table = [0u64; 16];
    let mut acc = salt | 1;
    let mut prev = arch::cycles();
    let mut i = 0;
    while i < JITTER_WINDOW {
        // Variable work: dependent multiply-xor chain plus a table touch,
        // so the timing depends on pipeline/cache state, not a fixed path.
        acc = acc
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(i as u64);
        acc ^= acc >> 29;
        let idx = (acc & 0xF) as usize;
        table[idx] = table[idx].wrapping_add(acc);
        let now = arch::cycles();
        deltas[i] = now.wrapping_sub(prev);
        prev = now;
        i += 1;
    }
    let credit = core::cmp::min(estimate_jitter_bits(&deltas), JITTER_ROUND_CAP);
    let mut bytes = [0u8; JITTER_WINDOW * 8];
    let mut i = 0;
    while i < JITTER_WINDOW {
        bytes[i * 8..i * 8 + 8].copy_from_slice(&deltas[i].to_le_bytes());
        i += 1;
    }
    pool.absorb(Source::Jitter, &bytes, credit);
    credit
}

/// One unconditional jitter round (for reseeding an already-ready pool).
/// Returns the credited bits.
pub fn jitter_once(pool: &mut EntropyPool) -> u32 {
    jitter_round(pool, arch::cycles())
}

/// Gather jitter until the pool is ready or `max_rounds` is exhausted.
/// Returns total credited bits from this gather.
pub fn gather_jitter(pool: &mut EntropyPool, max_rounds: u32) -> u32 {
    let mut total = 0u32;
    let mut r = 0;
    while r < max_rounds && !pool.ready() {
        total = total.saturating_add(jitter_round(pool, arch::cycles() ^ ((r as u64) << 32)));
        r += 1;
    }
    total
}

// ------------------------------------------------ event fast-mix

/// A tiny lock-free-ish accumulator for event timing (PTY input arrival,
/// virtio completions). `mix_event` must stay a handful of instructions -
/// it sits on I/O paths - so it only stirs four words; the words are folded
/// into the real pool (at zero credit) whenever the root reseeds.
static mut FAST: [u64; 4] = [0; 4];
static mut FAST_EVENTS: u64 = 0;

/// Stir an event timestamp (or any value) into the fast pool. Cheap,
/// branchless, safe to call from any kernel I/O path.
#[inline]
pub fn mix_event(v: u64) {
    // SAFETY: single-vcore kernel; a torn stir would still only stir.
    unsafe {
        let f = &mut *core::ptr::addr_of_mut!(FAST);
        let n = *core::ptr::addr_of!(FAST_EVENTS);
        let i = (n & 3) as usize;
        f[i] = f[i].rotate_left(19).wrapping_add(v ^ 0x9E37_79B9_7F4A_7C15);
        *core::ptr::addr_of_mut!(FAST_EVENTS) = n.wrapping_add(1);
    }
}

/// Drain the fast pool into `pool` (zero credit - event timing is mixed
/// for defense in depth, never counted). Returns how many events had
/// arrived since the last drain.
pub fn drain_events(pool: &mut EntropyPool) -> u64 {
    // SAFETY: single-vcore kernel.
    unsafe {
        let n = *core::ptr::addr_of!(FAST_EVENTS);
        if n == 0 {
            return 0;
        }
        let f = &*core::ptr::addr_of!(FAST);
        let mut bytes = [0u8; 40];
        for (i, w) in f.iter().enumerate() {
            bytes[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        bytes[32..40].copy_from_slice(&n.to_le_bytes());
        pool.absorb(Source::Event, &bytes, 0);
        *core::ptr::addr_of_mut!(FAST_EVENTS) = 0;
        n
    }
}
