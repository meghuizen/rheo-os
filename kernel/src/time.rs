//! Clock and entropy objects (docs/ARCHITECTURE.md 3 object 9,
//! docs/TIME-IDENTITY.md). Single-host scope: a monotonic clock read from
//! the per-ISA cycle counter, a wall clock expressed as a bounded interval
//! (no PTP/NTS sync yet, so the error bound is deliberately large and
//! honest), and a root DRBG feeding per-cell DRBGs.
//!
//! What is real here: monotonic ordering, the interval-clock *shape*
//! (every wall read is [t-e, t+e], never a bare instant), and
//! deterministic per-cell random streams derived from a root seed. What is
//! deferred: hardware time sync, entropy from a real TRNG, reseed-on-
//! restore (there is no checkpoint/restore yet).

use crate::arch;
use core::sync::atomic::{AtomicU64, Ordering};

static BOOT_TICKS: AtomicU64 = AtomicU64::new(0);

/// Record the boot instant. Called once during kernel init.
pub fn init() {
    BOOT_TICKS.store(arch::cycles(), Ordering::Relaxed);
    root_drbg_init();
}

/// Monotonic counter reading (raw ticks; per-ISA meaning, see
/// arch::cycles). Never goes backwards on a single core.
pub fn monotonic() -> u64 {
    arch::cycles()
}

/// Ticks elapsed since boot.
pub fn uptime_ticks() -> u64 {
    arch::cycles().wrapping_sub(BOOT_TICKS.load(Ordering::Relaxed))
}

/// A wall-clock reading as a bounded interval [center-e, center+e]
/// (docs/ARCHITECTURE.md 4.5). Without a synced time source the center is
/// "ticks since boot" and the error bound is the whole interval - the API
/// forces callers to see uncertainty rather than trust a fake instant.
#[derive(Copy, Clone, Debug)]
pub struct Interval {
    pub center: u64,
    pub error: u64,
}

pub fn wall() -> Interval {
    let t = uptime_ticks();
    Interval {
        center: t,
        // Unsynced: the true error is unbounded; report the reading itself
        // as the bound so no caller mistakes this for a precise clock.
        error: t,
    }
}

// ------------------------------------------------------------- entropy

/// A deterministic random bit generator (SplitMix64). Small, fast, and
/// good enough for per-cell streams; a real deployment swaps the seed
/// source for a hardware TRNG and adds health tests (TIME-IDENTITY.md).
#[derive(Copy, Clone)]
pub struct Drbg {
    state: u64,
}

impl Drbg {
    /// A zero-valued DRBG for static initialisation; reseed before use.
    pub const ZERO: Drbg = Drbg { state: 0 };

    pub fn from_seed(seed: u64) -> Drbg {
        Drbg { state: seed }
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Derive a child DRBG (per-cell streams are derived, not shared).
    pub fn derive(&mut self) -> Drbg {
        Drbg::from_seed(self.next_u64())
    }
}

static ROOT_DRBG: AtomicU64 = AtomicU64::new(0);

fn root_drbg_init() {
    // Seed source: a fixed constant mixed with the boot cycle count. This
    // is weak entropy (documented); the point is the derivation structure.
    let seed = 0x1234_5678_9ABC_DEF0 ^ arch::cycles().rotate_left(17);
    ROOT_DRBG.store(seed | 1, Ordering::Relaxed);
}

/// Mint a fresh per-cell DRBG derived from the root.
pub fn derive_cell_drbg() -> Drbg {
    let mut root = Drbg::from_seed(ROOT_DRBG.load(Ordering::Relaxed));
    let child = root.derive();
    ROOT_DRBG.store(root.next_u64() | 1, Ordering::Relaxed);
    child
}
