//! Cryptographic randomness (docs/TIME-IDENTITY.md 4, ARCHITECTURE.md 3
//! object 9). A ChaCha20 DRBG with fast key erasure over a **credited
//! multi-source entropy pool** with a **hard seeding gate**:
//!
//! - **Cryptographically strong**: ChaCha20 keystream (rng::chacha, verified
//!   against the RFC 8439 vector), the same primitive Linux's CRNG uses.
//! - **Forward secret**: every refill consumes the first 32 keystream bytes
//!   to re-key, so recovering the current state never reveals past output
//!   (fast key erasure, as in Linux `get_random_bytes` and BoringSSL).
//! - **Multi-source, credited** (rng::pool): the hardware RNG (RDSEED /
//!   RDRAND / RNDR, vetted by branchless SP 800-90B-style health tests),
//!   the firmware boot seed (`/chosen/rng-seed` from the device tree), a
//!   virtio-rng device (hw::virtio_rng, all three ISAs), timing jitter
//!   credited conservatively (1/4 bit per noisy delta - honestly ~0 under
//!   deterministic QEMU icount), and uncredited event timing. Every present
//!   source is mixed; credit is the gate.
//! - **Hard gate, no weak fallback**: until 256 credited bits arrive the
//!   root does not exist, `derive_cell_drbg` returns None, and consumers
//!   refuse rather than mint weak keys (BOOT.md 4's attestation stance,
//!   enforced locally). Seeding is the only blocking moment; after it,
//!   generation always returns from buffered keystream.
//! - **Per-cell streams**: each cell gets its own DRBG derived from the
//!   root, so a cell reads random bytes as a library call over its own
//!   state - no shared pool, no cross-cell side channel, no syscall on the
//!   fast path.
//! - **Branchless hot paths**: the ChaCha core, the pool conditioner, the
//!   health tests, and the jitter estimator contain no secret-dependent
//!   branches (constant indices, `ct_eq64` masks); `fill_bytes` branches
//!   only on public buffer positions.

pub mod chacha;
pub mod pool;

use crate::arch;
use pool::{EntropyPool, Source};

/// Output bytes handed out between refills. Each refill produces 32 bytes to
/// re-key plus this many output bytes of keystream.
const OUT: usize = 256;
/// Keystream bytes per refill: 32 (rekey) + OUT, rounded up to whole blocks.
const KS_BLOCKS: usize = (32 + OUT).div_ceil(64); // 5 blocks = 320 bytes
const KS_BYTES: usize = KS_BLOCKS * 64;

/// Root draws between opportunistic reseeds (derives are rare, so this
/// mostly matters for long-lived hosts; SP 800-90A's reseed-interval idea
/// applied to the root).
const ROOT_RESEED_INTERVAL: u64 = 512;

/// A ChaCha20 deterministic random bit generator with fast key erasure,
/// per Bernstein's construction (cr.yp.to blog, 2017.07.23) - the same
/// flow Linux's per-CPU CRNG adopted. The two rules that make it
/// forward-secure against later state capture:
///
/// 1. **The key is overwritten first**: every refill re-keys from the
///    leading keystream bytes, so the key that produced past output never
///    survives the refill that used it.
/// 2. **Delivered bytes are erased on the way out**: `fill_bytes` wipes
///    each output byte from the buffer as it is copied, so the state only
///    ever holds *future* output. Capturing the DRBG reveals nothing about
///    bytes already handed to a caller.
///
/// Honest limit: `Drbg` is `Copy`, and Rust moves/copies can leave stale
/// bitwise copies on the stack that these wipes cannot reach; the wipes
/// cover the long-lived state, which is what a later compromise reads.
/// The 256-byte batch keeps per-cell state small (djb's example batches
/// 736 bytes; the batch size only trades throughput, not security).
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

    /// Seed from 64 bits. A compatibility shim (tests, benchmarks) that
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
        // Wipe the whole local keystream copy (it holds the new key and a
        // duplicate of the buffer).
        wipe(&mut ks);
    }

    /// Fill `dst` with random bytes, erasing each byte from the buffer as
    /// it is delivered (rule 2 above): the state never retains output a
    /// caller has already received.
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
        // Wipe the keystream temporary and the abandoned buffer tail
        // (never-delivered output of the old key).
        wipe(&mut blk);
        wipe(&mut self.buf);
        self.pos = OUT;
    }

    /// Test hook: true if every already-delivered byte has been wiped from
    /// the buffer. State inspection exists only so the `rng` test kernel
    /// can prove the erase-on-read rule; never use it to read state.
    #[doc(hidden)]
    pub fn spent_bytes_erased(&self) -> bool {
        self.buf[..self.pos].iter().all(|&b| b == 0)
    }

    /// Derive an independent child DRBG. Per-cell streams are derived, never
    /// shared, so one cell's state tells you nothing about a sibling's.
    pub fn derive(&mut self) -> Drbg {
        let mut k = [0u8; 32];
        self.fill_bytes(&mut k);
        Drbg::from_key(k)
    }
}

/// Volatile-wipe a byte region (key/output erasure). Volatile so the
/// compiler cannot elide the "dead" stores; u64-wide where alignment
/// allows so bulk erasure does not dominate the draw path (a per-byte
/// volatile loop costs ~8x more and cannot vectorise).
pub(crate) fn wipe(bytes: &mut [u8]) {
    let mut p = bytes.as_mut_ptr();
    let mut n = bytes.len();
    // SAFETY: p..p+n is the caller's exclusive slice; u64 stores are only
    // issued on 8-byte-aligned addresses fully inside it.
    unsafe {
        while n > 0 && (p as usize) & 7 != 0 {
            core::ptr::write_volatile(p, 0);
            p = p.add(1);
            n -= 1;
        }
        while n >= 8 {
            core::ptr::write_volatile(p as *mut u64, 0);
            p = p.add(8);
            n -= 8;
        }
        while n > 0 {
            core::ptr::write_volatile(p, 0);
            p = p.add(1);
            n -= 1;
        }
    }
}

fn splitmix(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ------------------------------------------------------- root management

/// How the root stands: credited bits, contributing sources, and whether
/// the hard gate has been passed. This is what the boot attestation story
/// reports - a host must be able to say what seeded it.
#[derive(Copy, Clone, Debug)]
pub struct SeedReport {
    pub seeded: bool,
    pub credited_bits: u32,
    /// Bitmask over `pool::SOURCE_NAMES`.
    pub sources: u32,
}

static mut POOL: EntropyPool = EntropyPool::new();
static mut ROOT: Drbg = Drbg::ZERO;
static mut SEEDED: bool = false;
static mut ROOT_DRAWS: u64 = 0;
static mut VIRTIO: Option<crate::hw::virtio_rng::VirtioRng> = None;

fn pool_mut() -> &'static mut EntropyPool {
    // SAFETY: single-vcore kernel (SMP bring-up is future work).
    unsafe { &mut *core::ptr::addr_of_mut!(POOL) }
}

/// Seed the root DRBG from every available source. Called once during
/// arch::init, after hw discovery (the firmware tables and PCI bus must be
/// up) and before any cell runs.
pub fn init() {
    let p = pool_mut();

    // 1. Hardware RNG instruction, health-tested, full credit on pass.
    absorb_hwrng(p, 64);

    // 2. Firmware boot seed (`/chosen/rng-seed`): what real firmware and
    //    QEMU both provide on device-tree platforms. Full credit, capped.
    if let Some(seed) = crate::hw::fdt::rng_seed() {
        let credit = core::cmp::min(seed.len() as u32 * 8, pool::THRESHOLD_BITS);
        p.absorb(Source::FirmwareSeed, seed, credit);
    }

    // 3. virtio-rng: host-fed entropy on any of the three ISAs' QEMU
    //    machines (and cloud guests). Full credit per delivered byte.
    let dev = crate::hw::virtio_rng::probe();
    if let Some(ref d) = dev {
        absorb_virtio(p, d);
    }
    // SAFETY: single-vcore init.
    unsafe { *core::ptr::addr_of_mut!(VIRTIO) = dev };

    // 4. Timing jitter, conservatively credited (honest zero under icount).
    pool::gather_jitter(p, 32);

    // 5. Always-mixed, never-credited: the cycle counter and any early
    //    event timing. Mixing cannot reduce the pool; only credit gates.
    p.absorb(Source::Event, &arch::cycles().to_le_bytes(), 0);
    pool::drain_events(p);

    try_instantiate();
    let r = seed_report();
    if !r.seeded {
        crate::println!(
            "rng: UNSEEDED - {} of {} credited bits; refusing to serve random bytes",
            r.credited_bits,
            pool::THRESHOLD_BITS
        );
    }
}

/// Instantiate (or leave) the root according to the pool gate.
fn try_instantiate() {
    // SAFETY: single-vcore kernel.
    unsafe {
        if *core::ptr::addr_of!(SEEDED) {
            return;
        }
        if let Some(key) = pool_mut().squeeze_key() {
            *core::ptr::addr_of_mut!(ROOT) = Drbg::from_key(key);
            *core::ptr::addr_of_mut!(SEEDED) = true;
        }
    }
}

/// Pull hwrng words through the health tests into the pool. Words are
/// credited at full width only when the whole batch passes; a failing
/// batch is still mixed, at zero credit.
fn absorb_hwrng(p: &mut EntropyPool, want_words: usize) -> bool {
    if !arch::has_hwrng() {
        return false;
    }
    let mut pool_words = [0u64; 64];
    let want = core::cmp::min(want_words, 64);
    let mut got = 0;
    while got < want {
        match arch::hwrng_u64() {
            Some(v) => {
                pool_words[got] = v;
                got += 1;
            }
            None => break,
        }
    }
    if got < 8 {
        return false;
    }
    let ok = pool::health_ok(&pool_words[..got]);
    let mut bytes = [0u8; 64 * 8];
    for (i, w) in pool_words[..got].iter().enumerate() {
        bytes[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    let credit = if ok { got as u32 * 64 } else { 0 };
    p.absorb(Source::HwRng, &bytes[..got * 8], credit);
    ok
}

/// Pull one buffer from virtio-rng into the pool at full credit.
fn absorb_virtio(p: &mut EntropyPool, d: &crate::hw::virtio_rng::VirtioRng) -> bool {
    let mut buf = [0u8; 64];
    let n = d.fill(&mut buf);
    if n == 0 {
        return false;
    }
    p.absorb(Source::VirtioRng, &buf[..n], n as u32 * 8);
    true
}

/// The current seed report (see `SeedReport`).
pub fn seed_report() -> SeedReport {
    // SAFETY: single-vcore kernel.
    let p = pool_mut();
    SeedReport {
        seeded: unsafe { *core::ptr::addr_of!(SEEDED) },
        credited_bits: p.credited_bits(),
        sources: p.sources(),
    }
}

/// Has the hard gate been passed?
pub fn is_seeded() -> bool {
    // SAFETY: single-vcore kernel.
    unsafe { *core::ptr::addr_of!(SEEDED) }
}

/// Stir an event timestamp into the fast pool (see pool::mix_event).
/// Re-exported so I/O paths depend on `rng`, not on pool internals.
#[inline]
pub fn mix_event(v: u64) {
    pool::mix_event(v);
}

/// Mint a fresh per-cell DRBG derived from the root, or None while the
/// hard gate holds. Derives count as root draws and trigger opportunistic
/// reseeds so long-lived hosts keep folding fresh entropy in.
pub fn derive_cell_drbg() -> Option<Drbg> {
    // SAFETY: single-vcore kernel.
    unsafe {
        if !*core::ptr::addr_of!(SEEDED) {
            // A source may have come alive since init (e.g. events).
            reseed_root();
            if !*core::ptr::addr_of!(SEEDED) {
                return None;
            }
        }
        let draws = core::ptr::addr_of_mut!(ROOT_DRAWS);
        *draws += 1;
        if (*draws).is_multiple_of(ROOT_RESEED_INTERVAL) {
            reseed_root();
        }
        Some((*core::ptr::addr_of_mut!(ROOT)).derive())
    }
}

/// Gather fresh entropy from every live source and fold it into the pool
/// (and the root, if seeded); instantiates the root if the gate is newly
/// passed. Returns true if any *credited* entropy was mixed in.
pub fn reseed_root() -> bool {
    let p = pool_mut();

    // Each helper reports whether it mixed *credited* entropy; the ledger
    // itself is capped, so "fresh credit arrived" must be tracked per call.
    let mut credited = absorb_hwrng(p, 8);
    // SAFETY: single-vcore kernel; VIRTIO is set once during init.
    if let Some(d) = unsafe { (*core::ptr::addr_of!(VIRTIO)).as_ref() } {
        credited |= absorb_virtio(p, d);
    }
    credited |= pool::jitter_once(p) > 0;
    pool::drain_events(p);
    try_instantiate();
    // SAFETY: single-vcore kernel.
    unsafe {
        if *core::ptr::addr_of!(SEEDED)
            && let Some(key) = pool_mut().squeeze_key()
        {
            (*core::ptr::addr_of_mut!(ROOT)).reseed(&key);
        }
    }
    credited
}
