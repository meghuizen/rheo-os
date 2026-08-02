//! In-QEMU test kernel for the cryptographic RNG (docs/TIME-IDENTITY.md 4,
//! 4a). Verifies the ChaCha20 core against the RFC 8439 test vector, then the
//! DRBG's determinism, independence, reseed, and statistical sanity; then the
//! **entropy pool** - that a source an attacker controls cannot make the pool
//! claim to be seeded, that the pool cannot be exhausted, that uncredited input
//! is still mixed - the **software jitter source**, and the **boot health
//! check**. Closes with the seed source this machine actually reached, which
//! with a randomness device attached must be a real one on all three ISAs.

#![no_std]
#![no_main]

use kernel::rng::{self, Drbg, SeedSource, entropy, health, jitter};
use kernel::{arch, println};

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("rng: start on {}", arch::NAME);

    test_chacha_rfc_vector();
    test_determinism();
    test_independence();
    test_reseed();
    test_next_u64_matches_fill();
    test_statistical_sanity();
    test_boot_health();
    // Before the phases below, which deliberately reset the pool: this one is
    // about the state the *boot* reached.
    test_hwrng_and_seed_source();
    test_pool_flood_cannot_seed();
    test_pool_mixes_uncredited_input();
    test_pool_cannot_be_exhausted();
    test_jitter_source();

    println!("rng: PASS");
    arch::exit(arch::ExitCode::Success)
}

/// The ChaCha20 known-answer test. The vector lives in `rng::health` because
/// **every boot** runs it as a power-on self test now, not just this kernel -
/// so this asserts the same function the boot asserts, in one place.
fn test_chacha_rfc_vector() {
    assert!(
        health::chacha_kat(),
        "ChaCha20 block does not match RFC 8439 vector"
    );
    println!("rng: chacha20 RFC 8439 vector OK");
}

/// The same key always yields the same stream (a DRBG, not true random).
fn test_determinism() {
    let key = [0x5au8; 32];
    let mut a = Drbg::from_key(key);
    let mut b = Drbg::from_key(key);
    let mut ba = [0u8; 300]; // spans more than one refill
    let mut bb = [0u8; 300];
    a.fill_bytes(&mut ba);
    b.fill_bytes(&mut bb);
    assert!(ba == bb, "same key produced different streams");
    println!("rng: determinism OK");
}

/// Distinct seeds and derived children are independent.
fn test_independence() {
    let mut d1 = Drbg::from_seed(1);
    let mut d2 = Drbg::from_seed(2);
    let x1 = d1.next_u64();
    assert!(d1.next_u64() != x1, "DRBG repeated a value");
    assert!(d2.next_u64() != x1, "distinct seeds collided");
    let mut child = d1.derive();
    assert!(
        child.next_u64() != d1.next_u64(),
        "child stream not independent of parent"
    );
    println!("rng: independence OK");
}

/// Reseeding changes the stream (fresh entropy actually mixes in).
fn test_reseed() {
    let key = [0x11u8; 32];
    let mut a = Drbg::from_key(key);
    let mut b = Drbg::from_key(key);
    b.reseed(&[0x22u8; 32]);
    assert!(a.next_u64() != b.next_u64(), "reseed did not change stream");
    println!("rng: reseed OK");
}

/// next_u64 is little-endian over the same stream fill_bytes produces.
fn test_next_u64_matches_fill() {
    let key = [0x7cu8; 32];
    let mut a = Drbg::from_key(key);
    let mut b = Drbg::from_key(key);
    let mut bytes = [0u8; 64];
    b.fill_bytes(&mut bytes);
    for i in 0..8 {
        let v = a.next_u64();
        let mut chunk = [0u8; 8];
        chunk.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
        assert!(
            v == u64::from_le_bytes(chunk),
            "next_u64 disagrees with fill_bytes"
        );
    }
    println!("rng: next_u64/fill_bytes consistency OK");
}

/// Output is close to unbiased: over a large buffer the set-bit fraction is
/// near 1/2 and every byte value appears. Not a substitute for a real test
/// suite (Dieharder/PractRand), but it catches a broken generator.
fn test_statistical_sanity() {
    let mut d = Drbg::from_seed(0xDEAD_BEEF);
    let mut ones: u64 = 0;
    let mut seen = [false; 256];
    let mut nseen = 0u32;
    let mut buf = [0u8; 1024];
    let total_bytes = 64 * 1024u64;
    let rounds = total_bytes / buf.len() as u64;
    for _ in 0..rounds {
        d.fill_bytes(&mut buf);
        for &b in buf.iter() {
            ones += b.count_ones() as u64;
            if !seen[b as usize] {
                seen[b as usize] = true;
                nseen += 1;
            }
        }
    }
    let total_bits = total_bytes * 8;
    // Expect ~50% set. Allow a generous +/-2% window (true fraction here is
    // deterministic per seed, so this is a fixed pass/fail, not flaky).
    let lo = total_bits * 48 / 100;
    let hi = total_bits * 52 / 100;
    assert!(
        ones > lo && ones < hi,
        "bit balance off: {ones} set of {total_bits}"
    );
    assert!(nseen == 256, "only {nseen}/256 byte values appeared");
    println!("rng: statistical sanity OK ({ones} set bits of {total_bits}, {nseen}/256 bytes)");
}

/// The boot-time health check ran, and its three integrity tests passed. This
/// is the *same* check `boot::init` ran before this kernel's first line - it is
/// re-run here so the assertion is visible, and because the report carries the
/// pool numbers the phases below reason about.
fn test_boot_health() {
    let r = health::report();
    assert!(r.kat_ok, "boot health: ChaCha20 KAT failed");
    assert!(r.crngt_ok, "boot health: continuous test failed");
    assert!(r.window_ok, "boot health: output window test failed");
    println!(
        "rng: boot health OK (kat/crngt/window), pool seeded={} credit={}",
        r.seeded, r.credit
    );
}

/// **A source an attacker controls cannot make the pool claim to be seeded.**
///
/// This is the property the whole credit scheme exists for. A program writing
/// to `/dev/urandom` is `Source::User`, which is mixed and never counted - so a
/// megabyte of chosen bytes moves the credit counter by exactly zero, and the
/// pool stays unseeded. Then one credited source seeds it in one call.
fn test_pool_flood_cannot_seed() {
    entropy::reset();
    assert!(!entropy::seeded(), "pool seeded straight after a reset");

    // 1 MiB of attacker-chosen bytes, in the biggest chunks the write path
    // accepts, each claiming far more entropy than it could hold.
    let flood = [0x41u8; 512];
    for _ in 0..2048 {
        entropy::absorb(entropy::Source::User, &flood, u32::MAX);
    }
    let c = entropy::counters();
    assert!(
        c.credit == 0,
        "user writes credited {} bits (must be zero)",
        c.credit
    );
    assert!(
        !entropy::seeded(),
        "a flood of chosen bytes seeded the pool"
    );
    assert!(
        c.bytes[entropy::Source::User.index()] == 1024 * 1024,
        "flood was not mixed: {} bytes recorded",
        c.bytes[entropy::Source::User.index()]
    );

    // One credited source, one call: 32 bytes at 8 bits each is exactly the
    // 256-bit target.
    let good = [0x5au8; 32];
    entropy::absorb(entropy::Source::Cpu, &good, 256);
    assert!(
        entropy::seeded(),
        "a full credited source did not seed the pool"
    );
    assert!(entropy::ready(), "credited to target but not ready");

    // And the flood cannot push it *over* or reset it either.
    for _ in 0..64 {
        entropy::absorb(entropy::Source::User, &flood, u32::MAX);
    }
    assert!(
        entropy::counters().credit == entropy::CREDIT_TARGET,
        "credit moved past the target"
    );
    println!("rng: flood cannot seed (1 MiB chosen bytes = 0 bits credited) OK");
}

/// Uncredited input is still **mixed**. Not counting a source is not the same
/// as dropping it - a `/dev/urandom` write that changed nothing would be the
/// stub-reporting-success shape docs/ENGINEERING.md 7 rejects.
///
/// The oracle: reset, absorb X, extract; reset, absorb Y, extract. Two
/// different inputs must give two different extracts, and both must differ from
/// the extract of an empty pool.
fn test_pool_mixes_uncredited_input() {
    entropy::reset();
    let (empty, _) = entropy::take_seed();

    entropy::reset();
    entropy::absorb(entropy::Source::User, b"input-x", 0);
    let (x, _) = entropy::take_seed();

    entropy::reset();
    entropy::absorb(entropy::Source::User, b"input-y", 0);
    let (y, _) = entropy::take_seed();

    assert!(x != empty, "an uncredited write did not change the pool");
    assert!(x != y, "two different writes produced the same pool state");
    println!("rng: uncredited input is still mixed OK");
}

/// **The pool cannot be exhausted.** Extraction re-keys the state rather than
/// consuming it, so a seed is always available; and `seeded` is sticky, so
/// draining the credit counter can never put a properly-keyed machine back into
/// the unseeded state.
fn test_pool_cannot_be_exhausted() {
    entropy::reset();
    entropy::absorb(entropy::Source::Cpu, &[0x11u8; 32], 256);
    assert!(entropy::seeded(), "setup: pool not seeded");

    // The first take spends the credit; every later take still returns a seed.
    let mut prev = [0u8; 32];
    let mut distinct = 0;
    for i in 0..64 {
        let (seed, credited) = entropy::take_seed();
        assert!(
            credited == (i == 0),
            "take {i}: credited={credited}, expected {}",
            i == 0
        );
        assert!(
            entropy::seeded(),
            "take {i}: pool stopped reporting itself seeded"
        );
        if seed != prev {
            distinct += 1;
        }
        prev = seed;
    }
    assert!(
        distinct == 64,
        "extraction repeated: {distinct}/64 distinct"
    );

    // And a top-up brings credit back on a machine that has any source. If the
    // machine has none, credit stays zero - reported, not asserted, because
    // that is a property of the machine and not of this code.
    let credit = entropy::replenish();
    println!("rng: pool cannot be exhausted OK (64 distinct extracts, replenish -> {credit} bits)");
}

/// The **software jitter source**: the fallback for a machine with no
/// randomness hardware at all.
///
/// What is asserted is that it never credits without evidence. Whether it finds
/// jitter is a property of the machine - and under QEMU `-icount` there is
/// genuinely none, since the same work really does take the same number of
/// cycles - so the result is reported and its *self-consistency* is what fails
/// the test if it is wrong.
fn test_jitter_source() {
    entropy::reset();
    let r = jitter::gather();
    println!(
        "rng: jitter samples={} distinct={} longest_run={} credited={} bits{}{}",
        r.samples,
        r.distinct,
        r.longest_run,
        r.credited_bits,
        if r.reason.is_empty() { "" } else { " - " },
        r.reason
    );
    assert!(r.samples == jitter::SAMPLES as u32, "sample count wrong");
    assert!(
        r.distinct >= 1 && r.distinct <= r.samples,
        "distinct out of range"
    );
    if r.credited_bits > 0 {
        // Credited: then every health test it claims to have passed must hold.
        assert!(r.reason.is_empty(), "credited with a rejection reason");
        assert!(
            r.distinct >= jitter::MIN_DISTINCT_FOR_CREDIT,
            "credited {} bits on only {} distinct deltas",
            r.credited_bits,
            r.distinct
        );
        assert!(
            r.longest_run <= jitter::MAX_RUN,
            "credited {} bits with a run of {} identical deltas",
            r.credited_bits,
            r.longest_run
        );
        assert!(
            r.credited_bits <= r.distinct,
            "credited more bits than distinct values observed"
        );
        assert!(entropy::counters().credited[entropy::Source::Jitter.index()] > 0);
    } else {
        // Not credited: then it must say why, and the pool must be untouched
        // in credit while still having been mixed.
        assert!(!r.reason.is_empty(), "refused with no reason given");
        assert!(
            entropy::counters().credit == 0,
            "jitter refused but the pool gained credit"
        );
        assert!(
            entropy::counters().bytes[entropy::Source::Jitter.index()] > 0,
            "jitter refused AND did not mix - it must always mix"
        );
    }
}

/// What actually seeded this machine.
///
/// The launch attaches a **randomness device** on all three ISAs
/// (docs/TIME-IDENTITY.md 4a), so the headline assertion is the one that was
/// not true before: **every ISA reaches a real seed**, not just the two with a
/// CPU instruction. RISC-V could not before - its `seed` CSR needs an M-mode
/// grant this firmware does not give - and that hole is what the device closes.
fn test_hwrng_and_seed_source() {
    let src = rng::seed_source();
    let c = entropy::counters();
    println!(
        "rng: cpu-hwrng={} present={} rng-device={} seed_source={:?} seeded={}",
        arch::hwrng_name(),
        arch::has_hwrng(),
        kernel::hw::virtio_rng::present(),
        src,
        c.seeded
    );
    println!(
        "rng: credited bits cpu={} device={} jitter={} (uncredited bytes interrupt={} user={} boot={})",
        c.credited[entropy::Source::Cpu.index()],
        c.credited[entropy::Source::Device.index()],
        c.credited[entropy::Source::Jitter.index()],
        c.bytes[entropy::Source::Interrupt.index()],
        c.bytes[entropy::Source::User.index()],
        c.bytes[entropy::Source::Boot.index()],
    );

    assert!(
        kernel::hw::virtio_rng::present(),
        "the launch attaches a virtio-rng device but the driver did not find one"
    );
    assert!(
        c.seeded,
        "a randomness device is present but the pool never reached a full seed"
    );
    assert!(
        matches!(src, SeedSource::Hwrng | SeedSource::Device),
        "seeded, but the recorded source is {src:?}"
    );
    assert!(
        c.credited[entropy::Source::Device.index()] > 0,
        "the randomness device contributed no credited bits"
    );

    if arch::has_hwrng() {
        // The CPU instruction is asked first, so on an ISA that has one it is
        // what paid for the seed.
        assert!(
            src == SeedSource::Hwrng,
            "CPU hwrng present but seed source is {src:?}"
        );
        let mut samples = [0u64; 16];
        let mut got = 0;
        for s in samples.iter_mut() {
            if let Some(v) = arch::hwrng_u64() {
                *s = v;
                got += 1;
            }
        }
        assert!(got >= 8, "hwrng present but produced too few words");
        let mut all_equal = true;
        for i in 1..got {
            if samples[i] != samples[0] {
                all_equal = false;
            }
        }
        assert!(!all_equal, "hwrng stuck: all words identical");
        assert!(
            c.credited[entropy::Source::Cpu.index()] > 0,
            "CPU hwrng present but credited nothing"
        );
        // Continuous reseed must succeed with a live source.
        assert!(rng::reseed_root(), "root reseed failed with a live source");
        println!("rng: CPU hwrng live + device present + root seeded OK");
    } else {
        // No CPU instruction: the device is the only credited source, and it
        // is what the report must name. This is the RISC-V case.
        assert!(
            src == SeedSource::Device,
            "no CPU hwrng and a device present, but source is {src:?}"
        );
        assert!(
            c.credited[entropy::Source::Cpu.index()] == 0,
            "no CPU hwrng but CPU credited bits"
        );
        assert!(rng::reseed_root(), "root reseed failed with a live device");
        println!("rng: no CPU hwrng - randomness DEVICE seeded this machine OK");
    }
}
