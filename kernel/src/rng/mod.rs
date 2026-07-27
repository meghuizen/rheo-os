//! Cryptographic randomness (docs/TIME-IDENTITY.md 4, ARCHITECTURE.md 3
//! object 9). A ChaCha20 DRBG with fast key erasure replaces the old
//! SplitMix64 placeholder:
//!
//! - **Cryptographically strong**: ChaCha20 keystream (rng::chacha, verified
//!   against the RFC 8439 vector), the same primitive Linux's CRNG uses.
//! - **Forward secret**: every refill consumes the first 32 keystream bytes
//!   to re-key, so recovering the current state never reveals past output
//!   (fast key erasure, as in Linux `get_random_bytes` and BoringSSL).
//! - **Non-blocking**: generation always returns from the buffered keystream;
//!   only *seeding* touches the entropy source, and even that is bounded-
//!   retry with a fallback - there is no blocking "entropy pool" (the doc's
//!   "seeding is the only critical moment").
//! - **Hardware-seeded when available**: the root DRBG is seeded from the
//!   per-ISA hardware RNG (x86 RDSEED/RDRAND, ARM64 RNDR) after SP 800-90B
//!   style health tests; where no usable hwrng exists (RISC-V S-mode has no
//!   access to the Zkr seed CSR here) it falls back to a documented floor.
//! - **Per-cell streams**: each cell gets its own DRBG derived from the root,
//!   so a cell reads random bytes as a library call over its own state - no
//!   shared pool, no cross-cell side channel, no syscall on the fast path.

pub mod chacha;

use crate::arch;

/// Output bytes handed out between refills. Each refill produces 32 bytes to
/// re-key plus this many output bytes of keystream.
const OUT: usize = 256;
/// Keystream bytes per refill: 32 (rekey) + OUT, rounded up to whole blocks.
const KS_BLOCKS: usize = (32 + OUT).div_ceil(64); // 5 blocks = 320 bytes
const KS_BYTES: usize = KS_BLOCKS * 64;

/// A ChaCha20 deterministic random bit generator with fast key erasure.
#[derive(Copy, Clone)]
pub struct Drbg {
    key: [u8; 32],
    nonce: [u8; 12],
    buf: [u8; OUT],
    /// Next unused byte in `buf`; `OUT` means the buffer is spent.
    pos: usize,
}

impl Drbg {
    /// A zero DRBG for static initialisation; seed before use.
    pub const ZERO: Drbg = Drbg {
        key: [0; 32],
        nonce: [0; 12],
        buf: [0; OUT],
        pos: OUT,
    };

    /// Seed from a full 256-bit key - the strong constructor.
    pub fn from_key(key: [u8; 32]) -> Drbg {
        Drbg {
            key,
            nonce: [0; 12],
            buf: [0; OUT],
            pos: OUT,
        }
    }

    /// Seed from 64 bits. A compatibility shim (tests, weak fallbacks) that
    /// spreads the value across the key with SplitMix64 diffusion; it is NOT
    /// a substitute for `from_key` fed by real entropy.
    pub fn from_seed(seed: u64) -> Drbg {
        let mut key = [0u8; 32];
        let mut s = seed;
        let mut i = 0;
        while i < 4 {
            s = splitmix(s);
            key[i * 8..i * 8 + 8].copy_from_slice(&s.to_le_bytes());
            i += 1;
        }
        Drbg::from_key(key)
    }

    /// Refill `buf`, re-keying from the first 32 keystream bytes.
    fn refill(&mut self) {
        let mut ks = [0u8; KS_BYTES];
        let mut ctr = 0u32;
        let mut off = 0;
        while off < KS_BYTES {
            let mut blk = [0u8; 64];
            chacha::block(&self.key, ctr, &self.nonce, &mut blk);
            ks[off..off + 64].copy_from_slice(&blk);
            ctr += 1;
            off += 64;
        }
        // Fast key erasure: the first 32 bytes become the new key, so the
        // old key (and all past output) can never be recovered from the
        // state that remains.
        self.key.copy_from_slice(&ks[..32]);
        self.buf.copy_from_slice(&ks[32..32 + OUT]);
        self.pos = 0;
        // Wipe the local keystream copy of the new key.
        for b in ks[..32].iter_mut() {
            unsafe { core::ptr::write_volatile(b, 0) };
        }
    }

    /// Fill `dst` with random bytes.
    pub fn fill_bytes(&mut self, dst: &mut [u8]) {
        let mut i = 0;
        while i < dst.len() {
            if self.pos == OUT {
                self.refill();
            }
            let n = core::cmp::min(dst.len() - i, OUT - self.pos);
            dst[i..i + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            i += n;
        }
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill_bytes(&mut b);
        u64::from_le_bytes(b)
    }

    /// Fold 256 bits of fresh entropy into the state (SP 800-90A reseed
    /// shape): mix the new seed and the current key through one ChaCha20
    /// block, then force a refill under the new key.
    pub fn reseed(&mut self, seed: &[u8; 32]) {
        let mut n = [0u8; 12];
        n.copy_from_slice(&seed[..12]);
        let ctr = u32::from_le_bytes([seed[12], seed[13], seed[14], seed[15]]);
        let mut blk = [0u8; 64];
        chacha::block(&self.key, ctr, &n, &mut blk);
        let mut i = 0;
        while i < 32 {
            self.key[i] = blk[i] ^ seed[i];
            i += 1;
        }
        self.pos = OUT;
    }

    /// Derive an independent child DRBG. Per-cell streams are derived, never
    /// shared, so one cell's state tells you nothing about a sibling's.
    pub fn derive(&mut self) -> Drbg {
        let mut k = [0u8; 32];
        self.fill_bytes(&mut k);
        Drbg::from_key(k)
    }
}

fn splitmix(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ------------------------------------------------------- root seeding

/// Where the root DRBG's seed came from (reported by the `rand` tooling and
/// the boot attestation story - a host must be able to say what seeded it).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SeedSource {
    /// Seeded from the hardware RNG, health tests passed.
    Hwrng,
    /// Hardware RNG present but failed health/liveness - fell back.
    HwrngRejected,
    /// No usable hardware RNG; documented weak floor.
    Fallback,
    /// Not yet seeded.
    None,
}

/// **Per-CPU root DRBGs** (docs/SUBSTRATE.md 10a).
///
/// One root per core rather than one for the machine, for the same reason the
/// timer arbiter is per-core: `getrandom` is on the hot path of real programs
/// (every TLS nonce, every hash-map seed, glibc's own startup), and a single
/// global root would make it the one place every core serialises. A per-core root
/// needs no lock at all - the multikernel discipline of partitioning instead of
/// synchronising (docs/SCHEDULING.md 1a).
///
/// **This is safe only because of how the roots are seeded.** Each core's root is
/// keyed independently: [`init`] seeds CPU 0 from the hardware RNG after the
/// SP 800-90B health tests, and [`init_secondary`] keys each further core by
/// *deriving* from an already-seeded root (fast key erasure, so the parent's state
/// is not recoverable from the child's) and then folding in whatever fresh hardware
/// entropy that core can read itself. Two cores therefore never produce the same
/// stream, which is the property a naive "copy the root to every core" would
/// silently destroy - and it would destroy it invisibly, because two identical
/// ChaCha streams look perfectly random in isolation.
static ROOTS: crate::smp::PerCpu<Drbg> = crate::smp::PerCpu::new(Drbg::ZERO);
/// How each core's root was seeded. Per-core because the answer can genuinely
/// differ: a secondary may read the hardware RNG successfully where the primary
/// did not, or the reverse.
static SOURCES: crate::smp::PerCpu<SeedSource> = crate::smp::PerCpu::new(SeedSource::None);

/// This CPU's root DRBG.
///
/// # Safety
/// The reference must not outlive the caller's critical section, and no second
/// reference may be taken while it lives. A core touches only its own root, so
/// there is no cross-core obligation.
#[inline]
#[allow(clippy::mut_from_ref)]
unsafe fn root() -> &'static mut Drbg {
    // SAFETY: this CPU's own slot; the intra-CPU obligation is discharged at each
    // call site below (short, straight-line uses).
    unsafe { ROOTS.this_mut() }
}

/// Seed **this CPU's** root DRBG. Called once during boot on the primary, before
/// any cell runs.
pub fn init() {
    let mut key = [0u8; 32];
    let src = gather_seed(&mut key);
    // SAFETY: short setup on this CPU's own state.
    unsafe {
        *root() = Drbg::from_key(key);
        *SOURCES.this_mut() = src;
    }
    for b in key.iter_mut() {
        unsafe { core::ptr::write_volatile(b, 0) };
    }
}

/// Seed a **secondary** CPU's root, called on that CPU as it comes up.
///
/// Keys from `parent` (an already-seeded root, normally the boot CPU's) by
/// derivation rather than by copy - fast key erasure means the derived key does
/// not reveal the parent's state and, critically, the parent's own state advances,
/// so no two cores can be handed the same key. Then folds in this core's own
/// hardware entropy if it passes the health tests, which is why the reported
/// [`SeedSource`] is per-core.
///
/// Returns the seed source recorded for this CPU.
pub fn init_secondary(parent: &mut Drbg) -> SeedSource {
    let derived = parent.derive();
    // SAFETY: short setup on this CPU's own state.
    unsafe {
        *root() = derived;
    }
    let mut pool = [0u64; 8];
    let got = gather_hwrng(&mut pool);
    let src = if got >= 4 && health_ok(&pool[..got]) {
        let mut chunk = [0u8; 32];
        fold(&pool[..got], &mut chunk);
        // SAFETY: as above.
        unsafe {
            root().reseed(&chunk);
        }
        for b in chunk.iter_mut() {
            unsafe { core::ptr::write_volatile(b, 0) };
        }
        SeedSource::Hwrng
    } else if got > 0 {
        SeedSource::HwrngRejected
    } else {
        // No hardware entropy of its own, but the derived key is genuinely
        // independent of every other core's, so this is not the documented weak
        // floor - it is inherited strength. Reported as such.
        SeedSource::Fallback
    };
    // SAFETY: as above.
    unsafe {
        *SOURCES.this_mut() = src;
    }
    src
}

/// Take a derived DRBG from this CPU's root, for seeding another core (see
/// [`init_secondary`]). Kept separate from [`derive_cell_drbg`] so that "seed a
/// CPU" and "seed a cell" are distinguishable at the call site.
pub fn derive_for_cpu() -> Drbg {
    // SAFETY: short use of this CPU's own root.
    unsafe { root().derive() }
}

/// How **this CPU's** root DRBG was seeded.
pub fn seed_source() -> SeedSource {
    *SOURCES.this()
}

/// How CPU `cpu`'s root was seeded, for a boot report that names every core.
///
/// # Safety
/// An aggregation read; see [`crate::smp::PerCpu::get`].
pub unsafe fn seed_source_of(cpu: usize) -> SeedSource {
    // SAFETY: delegated to the caller.
    unsafe { *SOURCES.get(cpu) }
}

/// Mint a fresh per-cell DRBG derived from **this CPU's** root.
///
/// Every `SYS_RANDOM`/`getrandom`/`/dev/urandom` read goes through here, so
/// kernel-served randomness holds no per-cell state that could go stale or be
/// duplicated by a `fork` - the duplication hazard is confined to a cell's *own*
/// userspace DRBG, which is what `MADV_WIPEONFORK` exists to handle
/// (docs/SUBSTRATE.md 10a).
pub fn derive_cell_drbg() -> Drbg {
    // SAFETY: short use of this CPU's own root.
    unsafe { root().derive() }
}

/// Pull fresh entropy from the hardware RNG (if any) and fold it into **this
/// CPU's** root. Continuous reseeding; safe to call periodically. Returns true if
/// hardware entropy was actually mixed in.
pub fn reseed_root() -> bool {
    let mut pool = [0u64; 8];
    let got = gather_hwrng(&mut pool);
    if got >= 4 && health_ok(&pool[..got]) {
        let mut chunk = [0u8; 32];
        fold(&pool[..got], &mut chunk);
        // SAFETY: short use of this CPU's own root.
        unsafe {
            root().reseed(&chunk);
        }
        true
    } else {
        false
    }
}

/// Collect raw hwrng samples into `pool`; returns how many were obtained.
fn gather_hwrng(pool: &mut [u64]) -> usize {
    if !arch::has_hwrng() {
        return 0;
    }
    let mut got = 0;
    for slot in pool.iter_mut() {
        match arch::hwrng_u64() {
            Some(v) => {
                *slot = v;
                got += 1;
            }
            None => break,
        }
    }
    got
}

/// Build a 256-bit seed key. Prefers the hardware RNG; validates it with
/// SP 800-90B style health tests before trusting it.
fn gather_seed(key: &mut [u8; 32]) -> SeedSource {
    if arch::has_hwrng() {
        let mut pool = [0u64; 64];
        let got = gather_hwrng(&mut pool);
        if got >= 32 && health_ok(&pool[..got]) {
            fold(&pool[..got], key);
            return SeedSource::Hwrng;
        }
        fallback_key(key);
        return SeedSource::HwrngRejected;
    }
    fallback_key(key);
    SeedSource::Fallback
}

/// SP 800-90B style health checks on full-entropy 64-bit words: Repetition
/// Count Test (no consecutive repeats), Adaptive Proportion Test (no value
/// over-represented in the window), and a stuck-at check (bits vary).
fn health_ok(s: &[u64]) -> bool {
    // Repetition Count Test.
    let mut i = 1;
    while i < s.len() {
        if s[i] == s[i - 1] {
            return false;
        }
        i += 1;
    }
    // Adaptive Proportion Test: with 64-bit full-entropy words even two
    // matches in the window is astronomically unlikely, so cap occurrences.
    let cutoff = (s.len() / 8).max(2);
    let mut i = 0;
    while i < s.len() {
        let mut c = 0;
        let mut j = 0;
        while j < s.len() {
            if s[j] == s[i] {
                c += 1;
            }
            j += 1;
        }
        if c > cutoff {
            return false;
        }
        i += 1;
    }
    // Stuck-at: no bit is constant across the whole window.
    let mut orv = 0u64;
    let mut andv = u64::MAX;
    for &v in s {
        orv |= v;
        andv &= v;
    }
    orv != 0 && andv != u64::MAX
}

/// Absorb `pool` entropy into a 256-bit key by ChaCha20 diffusion: seed a
/// working DRBG from the first words, then reseed it with the rest so every
/// sample influences the output, and squeeze out 32 bytes.
fn fold(pool: &[u64], key: &mut [u8; 32]) {
    let mut k = [0u8; 32];
    let mut i = 0;
    while i < 4 {
        let v = pool[i % pool.len()];
        k[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        i += 1;
    }
    let mut d = Drbg::from_key(k);
    let mut idx = 0;
    while idx < pool.len() {
        let mut chunk = [0u8; 32];
        let mut j = 0;
        while j < 4 {
            let v = pool[(idx + j) % pool.len()];
            chunk[j * 8..j * 8 + 8].copy_from_slice(&v.to_le_bytes());
            j += 1;
        }
        d.reseed(&chunk);
        idx += 4;
    }
    d.fill_bytes(key);
}

/// Last-resort seed with no hardware RNG. Mixes the cycle counter through a
/// timing loop. NOTE: under QEMU -icount this is deterministic (no real
/// jitter), so it is a structural floor, not a source of real entropy - a
/// board without hwrng needs a genuine jitter/TRNG source (TIME-IDENTITY.md).
fn fallback_key(key: &mut [u8; 32]) {
    let mut acc = 0x9E37_79B9_7F4A_7C15u64 ^ arch::cycles();
    let mut d = Drbg::from_seed(acc);
    let mut r = 0;
    while r < 64 {
        acc ^= arch::cycles().rotate_left((acc & 63) as u32);
        let mut chunk = [0u8; 32];
        let mut dd = Drbg::from_seed(acc);
        dd.fill_bytes(&mut chunk);
        d.reseed(&chunk);
        r += 1;
    }
    d.fill_bytes(key);
}
