//! The **entropy input pool** (docs/TIME-IDENTITY.md 4a).
//!
//! Everything that can contribute unpredictability - a CPU instruction, a
//! hardware RNG device, the arrival time of a network frame or a disk
//! completion, a program writing to `/dev/urandom` - feeds this one pool. The
//! pool then re-keys each core's output DRBG. The output algorithm does not
//! change: it stays the ChaCha20 fast-key-erasure DRBG in [`super`], because
//! that is what makes reading random bytes cheap.
//!
//! # Why a pool at all
//!
//! Before this, seeding read the CPU's hardware RNG directly and used nothing
//! else. That is fine on x86-64 (RDSEED) and ARM64 (RNDR) and gives **nothing**
//! on RISC-V, whose `seed` CSR needs an M-mode grant we do not have - so that
//! ISA fell back to a cycle-counter loop that is deterministic under QEMU. A
//! pool lets any number of sources contribute, so an ISA without a CPU
//! instruction can be seeded by a device instead.
//!
//! # The one property that matters: absorbing never loses entropy
//!
//! A pool that many sources write into has an obvious attack: a source an
//! attacker controls floods it and washes out the good entropy already there.
//! The mixing step is chosen so that cannot happen. For each 32-byte chunk `C`
//! of input, with `K` the current pool key:
//!
//! ```text
//! K_new = ChaCha20_block(key = K, nonce = seq++)[0..32]  XOR  C
//! ```
//!
//! Read it in the two directions an attacker can come from:
//!
//! - **The attacker chose `C` but does not know `K`.** Then
//!   `ChaCha20_block(K, ..)` is unpredictable to them, and XORing a value they
//!   *do* know with a value they do not leaves it unpredictable. The pool is no
//!   weaker than it was.
//! - **The attacker knows `K` (the pool was compromised) but `C` carries real
//!   entropy.** Then the first term is known and `K_new` is as unpredictable as
//!   `C`. The pool *recovers*.
//!
//! So absorbing is non-decreasing in both directions, which is what "seeding
//! must not reduce entropy" means. (Formally it is non-decreasing up to the
//! collision loss of a random function on 256 bits, which is negligible - the
//! same caveat Linux's pool carries.)
//!
//! # Credit: mixing is not the same as counting
//!
//! Every source is *mixed*. Only some are *counted*. A source contributes
//! `credit_bits` towards the 256 bits needed before the pool will re-key a
//! root DRBG, and a source we cannot measure contributes **zero** - it is still
//! mixed (it can only help), it just does not move the counter. That is the
//! honest position: this kernel has no entropy estimator, and inventing one
//! would let a predictable source declare the pool ready.
//!
//! | Source | Credited | Why |
//! |---|---|---|
//! | [`Source::Cpu`] | full | RDSEED / RNDR, after the SP 800-90B health tests |
//! | [`Source::Device`] | full | a dedicated RNG device (virtio-rng, a TPM/TRNG chip) |
//! | [`Source::Jitter`] | 1 bit/sample | CPU execution-time jitter, **only** if its own health tests pass |
//! | [`Source::Firmware`] | full | `/chosen/rng-seed`; a lying bootloader already loaded the kernel |
//! | [`Source::Interrupt`] | none | NIC / NVMe / disk / UART arrival times - real but unmeasured |
//! | [`Source::User`] | none | writes to `/dev/urandom`; exactly Linux's rule |
//! | [`Source::Boot`] | none | the cycle-counter floor, deterministic under emulation |
//!
//! # The pool cannot run out
//!
//! Two different things could be called "exhaustion", and only one of them is
//! real:
//!
//! - **The pool state.** It cannot be exhausted. Extracting a seed runs the
//!   state through ChaCha20 and keeps the *first* half as the new state, so the
//!   state is refreshed, not consumed. An attacker who learns an extracted seed
//!   learns nothing about the state that produced it and nothing about the next
//!   one. This is what lets a read of random bytes always succeed - there is no
//!   blocking `/dev/random` here and there never needs to be.
//! - **The credit counter.** This can reach zero, and all that means is "no
//!   fresh entropy has arrived since the last re-key". It never makes the RNG
//!   weaker; it only stops a *reseed* from happening.
//!
//! Two guards keep the second from mattering:
//!
//! 1. [`seeded`] is **sticky**. Once the pool has ever held a full
//!    [`CREDIT_TARGET`], it is seeded for the rest of the boot. Nothing can put
//!    it back into an unseeded state, so no later condition can degrade a root
//!    DRBG that was properly keyed.
//! 2. [`replenish`] tops the pool up from every source that can be asked (a
//!    randomness device, the CPU instruction, and the software jitter source)
//!    rather than waiting for one to volunteer. [`pump`] calls it whenever
//!    credit is below [`LOW_WATER`], so a machine with any source at all keeps
//!    a reserve without anyone scheduling it.
//!
//! A machine with **no** source (no CPU instruction, no device, and jitter that
//! fails its health tests, which is every emulated boot) never becomes
//! `seeded`, and says so through [`super::SeedSource::Fallback`]. That is a
//! report, not a failure mode: the DRBG still runs, it just does not claim to
//! have been properly keyed.
//!
//! # Performance
//!
//! The read path (`getrandom`, a TLS nonce) never touches this module: it reads
//! the calling core's own root DRBG with no lock, exactly as before.
//!
//! There are two write paths, split so that an interrupt handler never waits
//! for a lock:
//!
//! - [`absorb_fast`] is what a device interrupt calls. It is one atomic XOR
//!   plus one atomic increment into **this core's own** scratch words. No lock,
//!   no ChaCha20, no allocation.
//! - [`absorb`] is what a thread calls. It takes the pool lock and runs
//!   ChaCha20 over the input.
//!
//! [`pump`] moves this core's scratch words into the pool and re-keys the root
//! if enough credit has accumulated. It runs in thread context, so the pool
//! lock is never taken with interrupts owning it - which is why the split
//! exists at all.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

use super::{chacha, wipe};
use crate::arch;
use crate::smp::{MAX_CPUS, SpinLock, cpu_index};

/// Bits of credited entropy the pool wants before it will re-key a root DRBG.
/// 256 = the key width; asking for less would mean re-keying with a key that is
/// weaker than the key it replaces.
pub const CREDIT_TARGET: u32 = 256;

/// Credit below which [`pump`] goes and asks the sources for more instead of
/// waiting for one to volunteer. Half a seed: high enough that a reseed is
/// usually ready when it is wanted, low enough that a quiet machine is not
/// running the jitter gatherer constantly.
pub const LOW_WATER: u32 = CREDIT_TARGET / 2;

/// Most bytes absorbed from one `/dev/urandom` write. A program is free to
/// write more - the rest is accepted and dropped rather than hashed, so a
/// process cannot make the kernel do unbounded ChaCha20 work by writing a large
/// buffer. It costs nothing real: the credit for user writes is zero anyway.
pub const MAX_USER_ABSORB: usize = 1024;

/// Where a contribution came from. Used for the credit rule above and so a boot
/// can *report* what actually seeded it rather than assert it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Source {
    /// A CPU instruction: RDSEED/RDRAND, RNDR, the Zkr `seed` CSR.
    Cpu,
    /// A hardware RNG device: virtio-rng, a TRNG or TPM chip.
    Device,
    /// The software-only CPU execution-time jitter source ([`super::jitter`]),
    /// health-tested before any of it is counted.
    Jitter,
    /// The **firmware boot seed** - `/chosen/rng-seed` in the device tree, which
    /// a bootloader or hypervisor fills from its own randomness. The only source
    /// available before any device is up.
    Firmware,
    /// Timing of a device event: NIC receive, NVMe/disk completion, UART byte.
    Interrupt,
    /// A program writing to `/dev/urandom`.
    User,
    /// The boot-time cycle-counter floor.
    Boot,
}

impl Source {
    /// How many sources there are; the width of the per-source counters.
    pub const COUNT: usize = 7;
    /// Index into the per-source counters.
    pub fn index(self) -> usize {
        match self {
            Source::Cpu => 0,
            Source::Device => 1,
            Source::Jitter => 2,
            Source::Firmware => 3,
            Source::Interrupt => 4,
            Source::User => 5,
            Source::Boot => 6,
        }
    }
    /// Human-readable, for the boot report.
    pub fn name(self) -> &'static str {
        match self {
            Source::Cpu => "cpu",
            Source::Device => "device",
            Source::Jitter => "jitter",
            Source::Firmware => "firmware",
            Source::Interrupt => "interrupt",
            Source::User => "user",
            Source::Boot => "boot",
        }
    }
    /// Whether a contribution from this source may count towards
    /// [`CREDIT_TARGET`]. See the table in the module docs.
    ///
    /// [`Source::Jitter`] is creditable, but it credits itself **zero** unless
    /// its own health tests pass - which they do not on an emulated machine
    /// with a deterministic cycle counter. Being on this list is permission to
    /// count, not a claim to have something worth counting.
    ///
    /// [`Source::Firmware`] is credited for the reason Linux credits it: a
    /// bootloader that lied about the seed had already loaded the kernel, so it
    /// could have compromised the boot far more directly. Trusting it adds no
    /// attacker who was not already inside.
    pub fn creditable(self) -> bool {
        matches!(
            self,
            Source::Cpu | Source::Device | Source::Jitter | Source::Firmware
        )
    }
}

// ----------------------------------------------------------------- the pool

struct Pool {
    /// The pool's 256-bit state. Not an output key: it is only ever used to
    /// produce a re-key for a root DRBG, and it re-keys itself when it does.
    key: [u8; 32],
    /// Absorb counter, used as the ChaCha20 nonce so no two absorbs under the
    /// same key can produce the same block.
    seq: u64,
    /// Credited bits held, saturating at [`CREDIT_TARGET`].
    credit: u32,
    /// Set once the pool has ever reached [`CREDIT_TARGET`], and never cleared.
    /// See "The pool cannot run out" in the module docs.
    seeded: bool,
}

impl Pool {
    const fn new() -> Pool {
        Pool {
            key: [0; 32],
            seq: 0,
            credit: 0,
            seeded: false,
        }
    }

    /// Mix one 32-byte chunk in. See the module docs for why this cannot lose
    /// entropy.
    fn absorb_chunk(&mut self, chunk: &[u8; 32]) {
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&self.seq.to_le_bytes());
        self.seq = self.seq.wrapping_add(1);
        let mut blk = [0u8; 64];
        chacha::block(&self.key, 0, &nonce, &mut blk);
        for i in 0..32 {
            self.key[i] = blk[i] ^ chunk[i];
        }
        wipe(&mut blk);
    }

    /// Mix an arbitrary-length buffer in, 32 bytes at a time. A short final
    /// chunk is zero-padded, which is safe: padding is a constant, and mixing
    /// in a constant cannot reduce entropy by the argument above.
    fn absorb_bytes(&mut self, bytes: &[u8]) {
        let mut off = 0;
        while off < bytes.len() {
            let n = core::cmp::min(32, bytes.len() - off);
            let mut chunk = [0u8; 32];
            chunk[..n].copy_from_slice(&bytes[off..off + n]);
            self.absorb_chunk(&chunk);
            wipe(&mut chunk);
            off += n;
        }
    }

    /// Squeeze a 256-bit re-key out and erase the state that produced it, so a
    /// later compromise of the pool cannot reproduce a key already handed out
    /// (the same fast-key-erasure rule the output DRBG uses).
    fn extract(&mut self) -> [u8; 32] {
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&self.seq.to_le_bytes());
        self.seq = self.seq.wrapping_add(1);
        let mut blk = [0u8; 64];
        chacha::block(&self.key, 0, &nonce, &mut blk);
        // First half re-keys the pool, second half is the output.
        self.key.copy_from_slice(&blk[..32]);
        let mut out = [0u8; 32];
        out.copy_from_slice(&blk[32..]);
        wipe(&mut blk);
        out
    }
}

static POOL: SpinLock<Pool> = SpinLock::new(Pool::new());

// ------------------------------------------------- per-core interrupt scratch

/// One core's scratch words, written by interrupt handlers on that core.
///
/// Four words rather than one so a burst of events carries more state than a
/// single 64-bit value could; one atomic RMW per event rather than four so the
/// cost stays near a counter bump. `hits` selects which word an event lands in,
/// spreading a burst across all four.
struct Fast {
    acc: [AtomicU64; 4],
    hits: AtomicU32,
}

impl Fast {
    const fn new() -> Fast {
        Fast {
            acc: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            hits: AtomicU32::new(0),
        }
    }
}

static FAST: [Fast; MAX_CPUS] = [const { Fast::new() }; MAX_CPUS];

// ------------------------------------------------------------------ counters

/// What the pool has seen, for the boot report and the proof kernel. Counters
/// only - a reader learns *how much* came from where, never a byte of it.
#[derive(Copy, Clone, Default)]
pub struct Counters {
    /// Bytes mixed in, per [`Source`], in [`Source::index`] order.
    pub bytes: [u64; Source::COUNT],
    /// Contributions accepted, per source.
    pub hits: [u64; Source::COUNT],
    /// Credited bits accepted, per source (before saturation at the target).
    /// This is what says *which* source actually paid for a seed.
    pub credited: [u64; Source::COUNT],
    /// Credited bits held right now.
    pub credit: u32,
    /// Whether the pool has ever held a full seed. Sticky - see the module
    /// docs; this is the anti-exhaustion guarantee.
    pub seeded: bool,
    /// Times the pool re-keyed a root DRBG.
    pub reseeds: u64,
    /// Times a core's interrupt scratch was drained into the pool.
    pub drains: u64,
}

static BYTES: [AtomicU64; Source::COUNT] = [const { AtomicU64::new(0) }; Source::COUNT];
static HITS: [AtomicU64; Source::COUNT] = [const { AtomicU64::new(0) }; Source::COUNT];
static CREDITED: [AtomicU64; Source::COUNT] = [const { AtomicU64::new(0) }; Source::COUNT];
static RESEEDS: AtomicU64 = AtomicU64::new(0);
static DRAINS: AtomicU64 = AtomicU64::new(0);

/// A snapshot of the counters.
pub fn counters() -> Counters {
    let p = POOL.lock();
    let (credit, seeded) = (p.credit, p.seeded);
    drop(p);
    let mut c = Counters {
        credit,
        seeded,
        reseeds: RESEEDS.load(Relaxed),
        drains: DRAINS.load(Relaxed),
        ..Default::default()
    };
    for i in 0..Source::COUNT {
        c.bytes[i] = BYTES[i].load(Relaxed);
        c.hits[i] = HITS[i].load(Relaxed);
        c.credited[i] = CREDITED[i].load(Relaxed);
    }
    c
}

/// Reset the pool and its counters. For a proof kernel that needs a known
/// starting point; not part of normal boot.
pub fn reset() {
    let mut p = POOL.lock();
    p.key = [0; 32];
    p.seq = 0;
    p.credit = 0;
    p.seeded = false;
    drop(p);
    for i in 0..Source::COUNT {
        BYTES[i].store(0, Relaxed);
        HITS[i].store(0, Relaxed);
        CREDITED[i].store(0, Relaxed);
    }
    RESEEDS.store(0, Relaxed);
    DRAINS.store(0, Relaxed);
    for f in FAST.iter() {
        for a in f.acc.iter() {
            a.store(0, Relaxed);
        }
        f.hits.store(0, Relaxed);
    }
}

// ------------------------------------------------------------------- the API

/// Mix `bytes` into the pool and claim `credit_bits` of entropy for them.
///
/// The claim is capped three ways: at the source's own rule (a source that is
/// not [`Source::creditable`] gets zero however much it asks for), at eight
/// bits per byte supplied, and at [`CREDIT_TARGET`] in total. Mixing always
/// happens regardless of credit.
///
/// Must be called in thread context: it takes the pool lock. An interrupt
/// handler calls [`absorb_fast`] instead.
pub fn absorb(src: Source, bytes: &[u8], credit_bits: u32) {
    if bytes.is_empty() {
        return;
    }
    let i = src.index();
    BYTES[i].fetch_add(bytes.len() as u64, Relaxed);
    HITS[i].fetch_add(1, Relaxed);

    let claim = if src.creditable() {
        let per_byte = (bytes.len() as u64).saturating_mul(8).min(u32::MAX as u64) as u32;
        credit_bits.min(per_byte)
    } else {
        0
    };

    let mut p = POOL.lock();
    p.absorb_bytes(bytes);
    if claim > 0 {
        p.credit = p.credit.saturating_add(claim).min(CREDIT_TARGET);
        if p.credit >= CREDIT_TARGET {
            p.seeded = true;
        }
        CREDITED[i].fetch_add(claim as u64, Relaxed);
    }
}

/// Mix a device event into **this core's** scratch words, from an interrupt
/// handler.
///
/// Two atomic read-modify-writes and a cycle-counter read; no lock, so a
/// handler can never wait on a thread that holds the pool. Credited zero, per
/// the module's table. `a` and `b` are whatever the caller has that varies -
/// typically a descriptor index and a length, or a received byte.
#[inline]
pub fn absorb_fast(a: u64, b: u64) {
    let f = &FAST[cpu_index()];
    let n = f.hits.fetch_add(1, Relaxed);
    let t = arch::cycles();
    // The rotate by the hit count keeps repeated identical events from
    // cancelling each other out under XOR.
    let mixed = t ^ a.rotate_left(n & 63) ^ b.rotate_left((n >> 6) & 63);
    f.acc[(n & 3) as usize].fetch_xor(mixed, Relaxed);
}

/// Take a 256-bit seed out of the pool **unconditionally**, and say whether it
/// was paid for with credited entropy.
///
/// Unconditional because a root DRBG has to be keyed with *something* at boot,
/// and a pool holding only uncredited input is still strictly better than the
/// bare cycle counter it replaced. The boolean is what stops that being an
/// overclaim: a caller reports [`super::SeedSource::Fallback`] when it is
/// false, so the boot log says exactly what happened.
///
/// Thread context only.
pub fn take_seed() -> ([u8; 32], bool) {
    drain_this_cpu();
    let mut p = POOL.lock();
    let credited = p.credit >= CREDIT_TARGET;
    let seed = p.extract();
    if credited {
        p.credit = 0;
    }
    (seed, credited)
}

/// Move this core's interrupt scratch into the pool, then re-key this core's
/// root DRBG if the pool holds a full [`CREDIT_TARGET`].
///
/// Returns true if the root was re-keyed. Thread context only.
pub fn pump() -> bool {
    drain_this_cpu();
    let held = POOL.lock().credit;
    if held < LOW_WATER {
        replenish();
    }
    if !ready() {
        return false;
    }
    let (mut seed, credited) = take_seed();
    if !credited {
        wipe(&mut seed);
        return false;
    }
    super::reseed_this_root(&seed);
    wipe(&mut seed);
    RESEEDS.fetch_add(1, Relaxed);
    true
}

/// Whether the pool currently holds a full seed's worth of credited entropy -
/// that is, whether a re-key is available *right now*.
pub fn ready() -> bool {
    POOL.lock().credit >= CREDIT_TARGET
}

/// Whether the pool has **ever** held a full seed. Sticky: once true it stays
/// true for the boot. This is the question "was this machine properly seeded",
/// which is not the same as [`ready`] and must never be answered by it.
pub fn seeded() -> bool {
    POOL.lock().seeded
}

// ------------------------------------------------- registered device sources

/// A randomness **device** the pool can ask for more bytes.
///
/// Returns how many bytes it fed (through [`super::feed_device`]); zero if the
/// device had nothing. The pool never sees the bytes - a driver reads its own
/// device and hands them over, and the pool decides mixing and credit. One owner
/// per decision.
pub type DeviceSource = fn() -> usize;

/// How many randomness devices can register. Four is more than any machine here
/// has; a fifth is refused with a printed reason rather than silently dropped.
const MAX_DEVICE_SOURCES: usize = 4;

static mut DEVICE_SOURCES: [Option<(&'static str, DeviceSource)>; MAX_DEVICE_SOURCES] =
    [None; MAX_DEVICE_SOURCES];

/// Register a randomness device with the pool.
///
/// **This is why the pool does not name a driver.** It used to call
/// `hw::virtio_rng::refill()` by name, which put a device driver's name inside
/// the entropy subsystem - the shape `svc::Bridge` exists to avoid, and the same
/// reason the queue's opcode dispatch names no driver. A TPM, a board TRNG or an
/// I/O device with a randomness function is then a *driver* that registers here,
/// not a change to the pool.
///
/// Called from boot, before any secondary starts.
pub fn register_device_source(name: &'static str, f: DeviceSource) -> bool {
    // SAFETY: boot, single-threaded, before any cell or secondary runs.
    let table = unsafe { &mut *core::ptr::addr_of_mut!(DEVICE_SOURCES) };
    for slot in table.iter_mut() {
        if slot.is_none() {
            *slot = Some((name, f));
            return true;
        }
    }
    crate::println!("rng: no free source slot for {name} - not registered");
    false
}

/// The names of the registered randomness devices, for the boot report.
pub fn device_source_names() -> [Option<&'static str>; MAX_DEVICE_SOURCES] {
    // SAFETY: written at boot, read-only after.
    let table = unsafe { &*core::ptr::addr_of!(DEVICE_SOURCES) };
    let mut out = [None; MAX_DEVICE_SOURCES];
    for (o, s) in out.iter_mut().zip(table.iter()) {
        *o = s.map(|(n, _)| n);
    }
    out
}

/// Ask every registered randomness device for bytes. Returns the total fed.
///
/// Every device is asked, not just the first: a machine with two of them has two
/// independent sources, and taking only one would waste the other for no reason.
pub fn draw_from_devices() -> usize {
    // SAFETY: the table is written at boot and read-only after; the function
    // pointers are called here in thread context.
    let table = unsafe { &*core::ptr::addr_of!(DEVICE_SOURCES) };
    let mut total = 0;
    for (_, f) in table.iter().flatten() {
        total += f();
    }
    total
}

/// Ask every source that can be asked for more entropy.
///
/// The order is cheapest-and-best first: the CPU instruction, then a randomness
/// device, then - only if those two left the pool short **and the machine has
/// never been seeded** - the software jitter source. Returns the credit held
/// afterwards.
///
/// The jitter source is gated on `!seeded` deliberately. It is the expensive one
/// (it spends time in order to measure time), and its job is to get a machine
/// with no randomness hardware to its *first* seed. Once a machine is seeded,
/// running it on every top-up would be a large recurring cost for a source that,
/// by then, is the weakest one available. Thread context only.
pub fn replenish() -> u32 {
    super::feed_cpu_hwrng();
    if !ready() {
        draw_from_devices();
    }
    if !ready() && !seeded() {
        super::jitter::gather();
    }
    POOL.lock().credit
}

/// Ask **every** source, whether or not the pool is already full, and return the
/// credit held afterwards.
///
/// The difference from [`replenish`] is the early exits, and it matters exactly
/// once - at boot, for the key every root DRBG starts from. `replenish` stops as
/// soon as the counter is satisfied, which means the *first* source to answer
/// decides the initial key on its own. That is the one place where a single
/// backdoored source would own the machine, so the first seed mixes the CPU
/// instruction **and** every randomness device **and** the jitter source, and an
/// attacker has to have compromised all of them rather than any one.
///
/// It cannot go wrong in the other direction: absorbing more can never reduce
/// entropy (see the module docs), and the credit counter saturates.
pub fn seed_from_all() -> u32 {
    super::feed_cpu_hwrng();
    draw_from_devices();
    super::jitter::gather();
    POOL.lock().credit
}

/// Take this core's scratch words (leaving them zero) and mix them in.
fn drain_this_cpu() {
    let f = &FAST[cpu_index()];
    if f.hits.swap(0, Relaxed) == 0 {
        return;
    }
    let mut chunk = [0u8; 32];
    for i in 0..4 {
        let v = f.acc[i].swap(0, Relaxed);
        chunk[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    DRAINS.fetch_add(1, Relaxed);
    absorb(Source::Interrupt, &chunk, 0);
    wipe(&mut chunk);
}
