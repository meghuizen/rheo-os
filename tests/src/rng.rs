//! In-QEMU test kernel for the cryptographic RNG (docs/TIME-IDENTITY.md 4).
//! Verifies the ChaCha20 core against the RFC 8439 test vector, the DRBG's
//! determinism, independence, reseed, and statistical sanity, and then the
//! credited multi-source entropy pool: the hard seeding gate, the branchless
//! health tests, the conservative jitter estimator, the per-source credit
//! ledger, and the live sources on this machine (hwrng / firmware seed /
//! virtio-rng).

#![no_std]
#![no_main]

use kernel::rng::{self, Drbg, chacha, pool};
use kernel::{arch, hw, println};

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("rng: start on {}", arch::NAME);

    test_chacha_rfc_vector();
    test_determinism();
    test_independence();
    test_reseed();
    test_next_u64_matches_fill();
    test_statistical_sanity();
    test_pool_gate();
    test_health_tests();
    test_jitter_estimator();
    test_live_sources();

    println!("rng: PASS");
    arch::exit(arch::ExitCode::Success)
}

/// RFC 8439 section 2.3.2: key = 00..1f, nonce = 000000090000004a00000000,
/// counter = 1. The keystream block is a published fixed vector.
fn test_chacha_rfc_vector() {
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = i as u8;
    }
    let nonce = [0u8, 0, 0, 9, 0, 0, 0, 0x4a, 0, 0, 0, 0];
    let mut out = [0u8; 64];
    chacha::block(&key, 1, &nonce, &mut out);

    const EXPECT: [u8; 64] = [
        0x10, 0xf1, 0xe7, 0xe4, 0xd1, 0x3b, 0x59, 0x15, 0x50, 0x0f, 0xdd, 0x1f, 0xa3, 0x20, 0x71,
        0xc4, 0xc7, 0xd1, 0xf4, 0xc7, 0x33, 0xc0, 0x68, 0x03, 0x04, 0x22, 0xaa, 0x9a, 0xc3, 0xd4,
        0x6c, 0x4e, 0xd2, 0x82, 0x64, 0x46, 0x07, 0x9f, 0xaa, 0x09, 0x14, 0xc2, 0xd7, 0x05, 0xd9,
        0x8b, 0x02, 0xa2, 0xb5, 0x12, 0x9c, 0xd1, 0xde, 0x16, 0x4e, 0xb9, 0xcb, 0xd0, 0x83, 0xe8,
        0xa2, 0x50, 0x3c, 0x4e,
    ];
    assert!(
        out == EXPECT,
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

/// The hard gate on a scratch pool: no key below the threshold, a key at
/// it, and the credit ledger/source mask must track absorbs exactly.
fn test_pool_gate() {
    let mut p = pool::EntropyPool::new();
    assert!(!p.ready(), "fresh pool claims ready");
    assert!(p.squeeze_key().is_none(), "unseeded pool handed out a key");

    // 128 credited bits: mixed, recorded, still gated.
    p.absorb(pool::Source::HwRng, &[0xAAu8; 16], 128);
    assert!(p.credited_bits() == 128, "credit ledger wrong");
    assert!(p.sources() & 1 != 0, "source bit not recorded");
    assert!(p.squeeze_key().is_none(), "gate leaked at 128 bits");

    // Zero-credit material mixes but never counts.
    p.absorb(pool::Source::Event, &[0x55u8; 64], 0);
    assert!(p.credited_bits() == 128, "zero-credit absorb was credited");

    // Crossing the threshold opens the gate; squeezing ratchets the pool
    // (two squeezes differ - forward secrecy of the pool key).
    p.absorb(pool::Source::FirmwareSeed, &[0x77u8; 16], 128);
    assert!(p.ready(), "256 credited bits but not ready");
    let k1 = p.squeeze_key().expect("ready pool refused to squeeze");
    let k2 = p.squeeze_key().expect("second squeeze refused");
    assert!(k1 != k2, "pool key did not ratchet between squeezes");
    println!("rng: pool hard gate + credit ledger OK");
}

/// The branchless SP 800-90B-style health tests must reject stuck and
/// repeating hwrng output and accept varied words.
fn test_health_tests() {
    assert!(!pool::health_ok(&[0u64; 16]), "all-zero passed health");
    assert!(!pool::health_ok(&[u64::MAX; 16]), "all-ones passed health");
    let mut rep = [0x1234_5678_9ABC_DEF0u64; 16];
    rep[0] = 1; // still 15 consecutive repeats
    assert!(!pool::health_ok(&rep), "repeating words passed health");
    // Varied words from the (deterministic) DRBG pass.
    let mut d = Drbg::from_seed(42);
    let mut varied = [0u64; 16];
    for v in varied.iter_mut() {
        *v = d.next_u64();
    }
    assert!(pool::health_ok(&varied), "varied words failed health");
    println!("rng: branchless health tests OK");
}

/// The jitter estimator must credit 0 for constant timing and stay under
/// its conservative bound (1/4 bit per sample) for noisy input.
fn test_jitter_estimator() {
    assert!(
        pool::estimate_jitter_bits(&[100u64; 64]) == 0,
        "constant deltas earned jitter credit"
    );
    // Fully noisy synthetic deltas: at most len/4 bits by construction.
    let mut d = Drbg::from_seed(7);
    let mut noisy = [0u64; 64];
    for v in noisy.iter_mut() {
        *v = d.next_u64();
    }
    let bits = pool::estimate_jitter_bits(&noisy);
    assert!(bits <= 16, "jitter credit above the 1/4-bit bound: {bits}");
    // Live gather into a scratch pool must respect the same cap and must
    // report ~0 under deterministic icount (cycles advance uniformly).
    let mut p = pool::EntropyPool::new();
    let live = pool::gather_jitter(&mut p, 4);
    assert!(live <= 64, "live jitter overcredited: {live}");
    println!("rng: jitter estimator conservative OK (live rounds credited {live} bits)");
}

/// The machine must be seeded through real sources, and each present
/// source must behave: hwrng varies, virtio-rng delivers, reseed mixes.
fn test_live_sources() {
    let r = rng::seed_report();
    println!(
        "rng: seed report: seeded={} credited={} bits, sources={:#06b}",
        r.seeded, r.credited_bits, r.sources
    );
    for (i, name) in pool::SOURCE_NAMES.iter().enumerate() {
        if r.sources & (1 << i) != 0 {
            println!("rng:   source[{i}] {name} contributed");
        }
    }

    // Every QEMU test machine has at least one credited source: RDRAND
    // (x86), RNDR (arm64), or the firmware seed / virtio-rng (riscv).
    assert!(r.seeded, "no credited entropy source seeded this machine");
    assert!(
        r.credited_bits >= pool::THRESHOLD_BITS,
        "seeded below threshold?"
    );
    assert!(
        rng::derive_cell_drbg().is_some(),
        "seeded root refused a derive"
    );

    if arch::has_hwrng() {
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
        println!("rng: hwrng live OK ({})", arch::hwrng_name());
    }

    // virtio-rng: probe again directly; where the device exists it must
    // deliver bytes that are not all identical.
    if let Some(dev) = hw::virtio_rng::probe() {
        let mut a = [0u8; 32];
        let n = dev.fill(&mut a);
        assert!(n > 0, "virtio-rng present but delivered 0 bytes");
        let first = a[0];
        assert!(
            a[..n].iter().any(|&b| b != first),
            "virtio-rng bytes all identical"
        );
        println!("rng: virtio-rng live OK ({n} bytes)");
    } else {
        println!("rng: no virtio-rng device on this machine");
    }

    if let Some(seed) = hw::fdt::rng_seed() {
        println!("rng: firmware rng-seed present ({} bytes)", seed.len());
    }

    // Continuous reseed with a live source must mix credited entropy.
    assert!(
        rng::reseed_root(),
        "reseed mixed no credited entropy despite live sources"
    );
    println!("rng: multi-source seeding OK");
}
