//! **Boot-time RNG health check** (docs/TIME-IDENTITY.md 4a).
//!
//! Every boot runs this once, right after the root DRBG is keyed. It asks three
//! integrity questions and reports two facts.
//!
//! # The three integrity questions
//!
//! These are about the *machinery*, not about the machine, so a failure is a
//! kernel-integrity failure and the boot stops. Nothing downstream is safe if
//! the generator is broken - and silently producing weak keys is worse than not
//! booting, which is the whole reason FIPS 140 requires a power-on self test.
//!
//! 1. **Known-answer test.** ChaCha20 must reproduce the RFC 8439 section 2.3.2
//!    block exactly. This catches a miscompiled or mis-linked primitive, which
//!    is not hypothetical: this tree has already been bitten once by an LLVM
//!    miscompile in crypto code (docs/NETSTACK.md, the N3a AES note).
//! 2. **Continuous test.** Two consecutive outputs of the live root DRBG must
//!    differ. This is FIPS 140-2's CRNGT, and it catches a generator that has
//!    got stuck.
//! 3. **Window test.** A window of live output must pass the same SP 800-90B
//!    style checks the hardware samples pass: no repeats, no value dominating,
//!    no bit constant across the window.
//!
//! # The two facts
//!
//! These are about the *machine* and are reported, never asserted, because a
//! machine genuinely may have no entropy source:
//!
//! - whether the entropy pool is [`super::entropy::seeded`], and
//! - which source paid for it, with the credited bits per source.
//!
//! An emulated boot with a deterministic cycle counter and no randomness device
//! reaches here unseeded, and that is a true statement about that machine.
//!
//! # Why the healthy path is silent
//!
//! The check runs on every boot; it prints only when something is wrong. A line
//! on every boot would change all ~210 recorded logs to say the same thing every
//! time, which is noise, and the failure path is a panic naming the test - which
//! is louder than a log line could be. A boot that wants the numbers asks for
//! [`report`] and prints them itself, which is what the `rng` test kernel does.

use super::{SeedSource, entropy};

/// What the check found.
#[derive(Copy, Clone)]
pub struct Report {
    /// ChaCha20 reproduces the RFC 8439 vector.
    pub kat_ok: bool,
    /// Two consecutive live outputs differ (FIPS 140-2 CRNGT).
    pub crngt_ok: bool,
    /// A window of live output passes the SP 800-90B style checks.
    pub window_ok: bool,
    /// The entropy pool has held a full credited seed at some point.
    pub seeded: bool,
    /// Credited bits the pool holds right now.
    pub credit: u32,
    /// How this core's root was seeded.
    pub source: SeedSource,
    /// Credited bits accepted per [`entropy::Source`], in index order.
    pub credited: [u64; entropy::Source::COUNT],
}

impl Report {
    /// Whether every *integrity* test passed. Deliberately does not include
    /// [`Report::seeded`]: a machine with no entropy source is not a broken
    /// kernel.
    pub fn integrity_ok(&self) -> bool {
        self.kat_ok && self.crngt_ok && self.window_ok
    }
}

/// The RFC 8439 section 2.3.2 known-answer test: key = 00..1f, nonce =
/// 000000090000004a00000000, counter = 1. Published, so it is an oracle this
/// code cannot influence.
pub fn chacha_kat() -> bool {
    let mut key = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        key[i] = i as u8;
        i += 1;
    }
    let nonce = [0u8, 0, 0, 9, 0, 0, 0, 0x4a, 0, 0, 0, 0];
    let mut out = [0u8; 64];
    super::chacha::block(&key, 1, &nonce, &mut out);
    out == EXPECT
}

const EXPECT: [u8; 64] = [
    0x10, 0xf1, 0xe7, 0xe4, 0xd1, 0x3b, 0x59, 0x15, 0x50, 0x0f, 0xdd, 0x1f, 0xa3, 0x20, 0x71, 0xc4,
    0xc7, 0xd1, 0xf4, 0xc7, 0x33, 0xc0, 0x68, 0x03, 0x04, 0x22, 0xaa, 0x9a, 0xc3, 0xd4, 0x6c, 0x4e,
    0xd2, 0x82, 0x64, 0x46, 0x07, 0x9f, 0xaa, 0x09, 0x14, 0xc2, 0xd7, 0x05, 0xd9, 0x8b, 0x02, 0xa2,
    0xb5, 0x12, 0x9c, 0xd1, 0xde, 0x16, 0x4e, 0xb9, 0xcb, 0xd0, 0x83, 0xe8, 0xa2, 0x50, 0x3c, 0x4e,
];

/// Run the check and return what it found. Does not panic; [`check`] is the
/// caller that decides a failure ends the boot.
pub fn report() -> Report {
    // Test the *live* generator, not a fresh one seeded here: a fresh DRBG
    // would prove the algorithm works and say nothing about the state this
    // machine will actually use.
    let mut d = super::derive_cell_drbg();
    let a = d.next_u64();
    let b = d.next_u64();

    let mut window = [0u64; 64];
    for w in window.iter_mut() {
        *w = d.next_u64();
    }

    let c = entropy::counters();
    Report {
        kat_ok: chacha_kat(),
        crngt_ok: a != b,
        window_ok: super::health_ok(&window),
        seeded: c.seeded,
        credit: c.credit,
        source: super::seed_source(),
        credited: c.credited,
    }
}

/// The boot-time check. Silent when healthy; panics with the failing test's
/// name when the generator itself is broken.
pub fn check() -> Report {
    let r = report();
    assert!(
        r.kat_ok,
        "rng health: ChaCha20 does not match the RFC 8439 vector"
    );
    assert!(
        r.crngt_ok,
        "rng health: the root DRBG returned the same value twice"
    );
    assert!(
        r.window_ok,
        "rng health: a window of DRBG output failed the SP 800-90B checks"
    );
    r
}
