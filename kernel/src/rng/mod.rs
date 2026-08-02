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
pub mod entropy;
pub mod health;
pub mod jitter;

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
            wipe(&mut blk);
        }
        // Fast key erasure rule 1: the first 32 bytes become the new key, so
        // the old key (and all past output) can never be recovered from the
        // state that remains.
        self.key.copy_from_slice(&ks[..32]);
        self.buf.copy_from_slice(&ks[32..32 + OUT]);
        self.pos = 0;
        // The whole local keystream, not just the new key: the rest of it is
        // the output buffer's contents, which rule 2 below exists to erase.
        wipe(&mut ks);
    }

    /// Fill `dst` with random bytes.
    ///
    /// **Fast key erasure rule 2** (cr.yp.to 2017.07.23): a byte is erased from
    /// the buffer as it is handed out, so capturing the DRBG state later reveals
    /// nothing about output already delivered. Rule 1 (re-key every refill) is in
    /// [`Drbg::refill`]; without rule 2 as well, up to `OUT` bytes of delivered
    /// output sat in `buf` until the next refill.
    ///
    /// Honest limit: `Drbg` is `Copy`, so a caller that copied the struct leaves a
    /// stale image these wipes cannot reach. The wipe covers the state this
    /// generator owns.
    pub fn fill_bytes(&mut self, dst: &mut [u8]) {
        let mut i = 0;
        while i < dst.len() {
            if self.pos == OUT {
                self.refill();
            }
            let n = core::cmp::min(dst.len() - i, OUT - self.pos);
            dst[i..i + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            wipe(&mut self.buf[self.pos..self.pos + n]);
            self.pos += n;
            i += n;
        }
    }

    /// How many bytes of the current buffer have been handed out and erased.
    /// A hook for the proof kernel, which cannot see `buf` from outside.
    #[doc(hidden)]
    pub fn spent_is_erased(&self) -> bool {
        self.buf[..self.pos].iter().all(|&b| b == 0)
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
        // The buffer is abandoned here (pos = OUT forces a refill), so its
        // undelivered tail must be erased rather than left behind - rule 2
        // applied to output that will now never be handed out.
        wipe(&mut self.buf);
        wipe(&mut blk);
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

/// Overwrite a buffer so the compiler may not remove the store.
///
/// Word-wide where it can be: a per-byte volatile loop measured about twice as
/// slow on bulk draws, and erasure is a property of the bytes being gone, not of
/// how many stores it took.
pub(crate) fn wipe(b: &mut [u8]) {
    let (head, words, tail) = unsafe { b.align_to_mut::<u64>() };
    for x in head.iter_mut() {
        // SAFETY: a plain write through a valid reference; volatile only so it
        // is not optimised away.
        unsafe { core::ptr::write_volatile(x, 0) };
    }
    for w in words.iter_mut() {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile(w, 0) };
    }
    for x in tail.iter_mut() {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile(x, 0) };
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
    /// Seeded from the CPU's hardware RNG instruction, health tests passed.
    Hwrng,
    /// Seeded from a hardware RNG **device** (virtio-rng, a TRNG chip) through
    /// the entropy pool. Distinct from [`SeedSource::Hwrng`] so a boot report
    /// says which one actually paid for the seed - on RISC-V there is no CPU
    /// instruction available to S-mode, so a device is the only real source.
    Device,
    /// Seeded from the firmware boot seed (`/chosen/rng-seed`), the only source
    /// available before a device is up. Device-tree platforms only.
    Firmware,
    /// Seeded from the software-only CPU execution-time jitter source, which
    /// passed its own health tests. The fallback for a machine with no
    /// randomness hardware at all - and a real source, not the old floor,
    /// because it is only counted when the timing variation is measured.
    Jitter,
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
///
/// Everything goes through the entropy pool now: the boot floor is mixed in
/// uncredited, the CPU's hardware RNG is health-tested and mixed in credited,
/// and the root is keyed from whatever the pool then holds. The reported
/// [`SeedSource`] is decided by whether the pool had *credited* entropy - so a
/// machine with no real source still gets a keyed root, and still says so.
pub fn init() {
    // The cycle-counter floor. Always mixed (it can only help), never counted:
    // under QEMU `-icount` it is deterministic, so counting it would let an
    // emulated boot claim it was properly seeded.
    let mut floor = [0u8; 32];
    fallback_key(&mut floor);
    entropy::absorb(entropy::Source::Boot, &floor, 0);
    wipe(&mut floor);

    // The firmware boot seed, where the platform has one. First because it is
    // free - the bytes are already in hand from device-tree discovery - and
    // because it is the only source that exists before a device is up.
    if let Some(seed) = crate::hw::fdt::rng_seed() {
        entropy::absorb(
            entropy::Source::Firmware,
            seed,
            (seed.len() as u32).saturating_mul(8),
        );
    }

    // **Every** source, not just enough of them. This is the one key where
    // stopping early would let whichever source answered first decide the whole
    // machine's starting state on its own (docs/TIME-IDENTITY.md 4a).
    let fed = feed_cpu_hwrng();
    entropy::seed_from_all();

    let (mut key, credited) = entropy::take_seed();
    let src = if credited {
        credited_source().unwrap_or(SeedSource::Hwrng)
    } else if fed == Feed::Rejected {
        SeedSource::HwrngRejected
    } else {
        SeedSource::Fallback
    };
    // SAFETY: short setup on this CPU's own state.
    unsafe {
        *root() = Drbg::from_key(key);
        *SOURCES.this_mut() = src;
    }
    wipe(&mut key);
}

/// Seed a **secondary** CPU's root, called on that CPU as it comes up.
///
/// Keys from `parent` (an already-seeded root, normally the boot CPU's) by
/// derivation rather than by copy - fast key erasure means the derived key does
/// not reveal the parent's state and, critically, the parent's own state advances,
/// so no two cores can be handed the same key. Then folds in whatever the pool
/// can offer this core - its own hardware RNG reading, plus anything a device
/// has contributed since - which is why the reported [`SeedSource`] is per-core.
///
/// Returns the seed source recorded for this CPU.
pub fn init_secondary(parent: &mut Drbg) -> SeedSource {
    let derived = parent.derive();
    // SAFETY: short setup on this CPU's own state.
    unsafe {
        *root() = derived;
    }
    let fed = feed_cpu_hwrng();
    let src = if entropy::pump() {
        credited_source().unwrap_or(SeedSource::Hwrng)
    } else if fed == Feed::Rejected {
        SeedSource::HwrngRejected
    } else {
        // No credited entropy of its own, but the derived key is genuinely
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

/// Re-key **this CPU's** root from a 256-bit pool extract. The one place the
/// entropy pool is allowed to touch a root DRBG.
pub(crate) fn reseed_this_root(seed: &[u8; 32]) {
    // SAFETY: short use of this CPU's own root.
    unsafe {
        root().reseed(seed);
    }
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
    maybe_pump();
    // SAFETY: short use of this CPU's own root.
    unsafe { root().derive() }
}

/// How many derivations a core makes before it drains its interrupt scratch
/// into the pool and re-keys if the pool is full.
const PUMP_EVERY: u32 = 1024;

/// How many derivations a core makes before it re-keys its root from the pool
/// **whether or not** the pool has credited entropy.
///
/// This bounds how long a compromised root stays compromised
/// (docs/TIME-IDENTITY.md 4a). Fast key erasure already means an attacker who
/// captures the state learns nothing about *past* output; this is the other
/// direction - recovery - and without it a machine whose sources have all gone
/// quiet would keep a captured key for the rest of the boot.
///
/// A larger multiple of [`PUMP_EVERY`] than 1 because a re-key that mixes only
/// uncredited input is a *chance* of recovery, not a guarantee, and it costs a
/// pool lock: often enough to bound the window, rarely enough to be free.
const REKEY_EVERY: u32 = 16 * PUMP_EVERY;

/// Per-core countdown to the next [`entropy::pump`]. Plain, not atomic: a core
/// only ever touches its own slot, and the worst a lost update could do is
/// delay a reseed.
static PUMP_TICK: crate::smp::PerCpu<u32> = crate::smp::PerCpu::new(0);
/// Per-core countdown to the next unconditional re-key. See [`REKEY_EVERY`].
static REKEY_TICK: crate::smp::PerCpu<u32> = crate::smp::PerCpu::new(0);

/// Drive continuous reseeding off the *consume* path rather than a timer.
///
/// Entropy that a device interrupt put in this core's scratch has to reach the
/// pool somehow, and the natural moment is when randomness is actually being
/// used. The cost on the hot path is an increment and a compare; the pool lock
/// is touched once every [`PUMP_EVERY`] derivations, not per byte.
#[inline]
fn maybe_pump() {
    // SAFETY: this CPU's own slot, read and written in one straight line.
    let tick = unsafe { PUMP_TICK.this_mut() };
    *tick += 1;
    if *tick >= PUMP_EVERY {
        *tick = 0;
        entropy::pump();
    }
    // SAFETY: as above.
    let rk = unsafe { REKEY_TICK.this_mut() };
    *rk += 1;
    if *rk >= REKEY_EVERY {
        *rk = 0;
        rekey_root_unconditional();
    }
}

/// Re-key this core's root from the pool even when the pool has no *credited*
/// entropy to offer, and count it.
///
/// The point is recovery from a state compromise. Everything that has happened
/// since the last re-key - every device interrupt, every HID arrival, every
/// `/dev/urandom` write, every cycle counter read - has been absorbed into the
/// pool, and none of it is counted because none of it can be measured. That is
/// the right rule for deciding whether a machine is *seeded*; it is the wrong
/// rule for deciding whether to move a key an attacker may already hold.
///
/// Honest about the strength: this is a **chance** of recovery, not a guarantee.
/// On a machine whose every input is predictable to the attacker it changes the
/// key to another one they can compute. On any machine with real activity it
/// closes the window. [`rekeys`] counts them so the claim is measurable.
fn rekey_root_unconditional() {
    let (mut seed, _credited) = entropy::take_seed();
    reseed_this_root(&seed);
    wipe(&mut seed);
    REKEYS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

static REKEYS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How many times a root has been re-keyed by the bounded-lifetime rule.
pub fn rekeys() -> u64 {
    REKEYS.load(core::sync::atomic::Ordering::Relaxed)
}

/// Pull fresh entropy from every source that has any - the CPU's hardware RNG
/// plus this core's accumulated device-interrupt scratch - and re-key **this
/// CPU's** root if that adds up to a full seed. Safe to call periodically.
/// Returns true if the root was actually re-keyed.
pub fn reseed_root() -> bool {
    feed_cpu_hwrng();
    entropy::pump()
}

/// Feed a hardware RNG **device** (virtio-rng, a TRNG or TPM chip) into the
/// pool. The driver supplies bytes it read from the device; they are credited
/// in full, the same trust the CPU instruction gets, because a device whose
/// whole purpose is randomness is not a guess about a side effect.
///
/// This is the entry point that lets an ISA with no usable CPU instruction -
/// RISC-V, whose `seed` CSR needs an M-mode grant - be properly seeded.
pub fn feed_device(bytes: &[u8]) {
    entropy::absorb(
        entropy::Source::Device,
        bytes,
        (bytes.len() as u32).saturating_mul(8),
    );
}

/// Feed the timing of a device event into this core's scratch, from an
/// interrupt handler. Two atomic operations; no lock, no ChaCha20. Never
/// credited - see the table in [`entropy`].
#[inline]
pub fn feed_interrupt(a: u64, b: u64) {
    entropy::absorb_fast(a, b);
}

/// Feed the **arrival of a HID event** - a keystroke, a mouse move - into this
/// core's scratch.
///
/// # This is not a keylogger, by construction
///
/// The parameter is a **sequence number, not the event**. There is no way for a
/// caller to pass a key code, a character or a coordinate through this function,
/// because the signature does not carry one - which is a property a reviewer can
/// check in one line, rather than a promise about what callers do.
///
/// That costs nothing, because what carries the unpredictability is **when** a
/// key was pressed, not which one: the cycle counter [`entropy::absorb_fast`]
/// reads is the entropy, and a key code is at most a few bits of highly skewed,
/// highly guessable text. Mixing keystroke content would put what a person typed
/// into kernel state for a source that is credited **zero** anyway - all cost, no
/// benefit.
///
/// Named apart from [`feed_interrupt`] even though both land in the same
/// uncredited scratch, because they are different claims: a device completion is
/// a machine's own timing, while a HID event is a *person*, which is the classic
/// source Linux has collected since its first `/dev/random`. A reader asking
/// "does this OS collect input entropy" should find the answer by name.
///
/// Mixed, never credited: this kernel cannot tell a typist from an auto-repeat.
#[inline]
pub fn feed_hid(seq: u64) {
    entropy::absorb_fast(seq, 0);
}

/// Feed bytes a program wrote to `/dev/urandom`. Mixed, never credited -
/// exactly Linux's rule, and for the same reason: the kernel cannot tell
/// whether a program wrote real entropy or a constant.
pub fn feed_user(bytes: &[u8]) {
    let n = core::cmp::min(bytes.len(), entropy::MAX_USER_ABSORB);
    entropy::absorb(entropy::Source::User, &bytes[..n], 0);
}

/// What a read of the CPU's hardware RNG produced.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Feed {
    /// No hardware RNG on this ISA, or it returned nothing.
    Absent,
    /// Read and health-tested; mixed in and counted.
    Credited,
    /// Read but failed the health tests; mixed in **uncredited**. Mixing a
    /// suspect source can only help; counting it could let a stuck source
    /// declare the pool ready, which is the one thing that must not happen.
    Rejected,
}

/// Bits credited per health-tested 64-bit hardware word. A hardware RNG that
/// passes its health tests is treated as full entropy, the same default Linux
/// uses (`random.trust_cpu=1`).
const BITS_PER_HW_WORD: u32 = 64;

/// Read this CPU's hardware RNG and feed it to the pool.
fn feed_cpu_hwrng() -> Feed {
    let mut words = [0u64; 8];
    let got = gather_hwrng(&mut words);
    if got == 0 {
        return Feed::Absent;
    }
    let mut bytes = [0u8; 64];
    for i in 0..got {
        bytes[i * 8..i * 8 + 8].copy_from_slice(&words[i].to_le_bytes());
    }
    let ok = got >= 4 && health_ok(&words[..got]);
    let credit = if ok { got as u32 * BITS_PER_HW_WORD } else { 0 };
    entropy::absorb(entropy::Source::Cpu, &bytes[..got * 8], credit);
    wipe(&mut bytes);
    for w in words.iter_mut() {
        // SAFETY: plain write through a valid reference, volatile so it stays.
        unsafe { core::ptr::write_volatile(w, 0) };
    }
    if ok { Feed::Credited } else { Feed::Rejected }
}

/// Which credited source the pool has been paid by, if any. Cumulative over the
/// boot: it answers "has a real source ever contributed", which is exactly the
/// question a seed-source report is asking.
fn credited_source() -> Option<SeedSource> {
    let c = entropy::counters();
    if c.credited[entropy::Source::Cpu.index()] > 0 {
        Some(SeedSource::Hwrng)
    } else if c.credited[entropy::Source::Device.index()] > 0 {
        Some(SeedSource::Device)
    } else if c.credited[entropy::Source::Firmware.index()] > 0 {
        Some(SeedSource::Firmware)
    } else if c.credited[entropy::Source::Jitter.index()] > 0 {
        Some(SeedSource::Jitter)
    } else {
        None
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
