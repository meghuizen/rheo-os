//! In-QEMU test kernel for the cryptographic RNG (docs/TIME-IDENTITY.md 4).
//! Verifies the ChaCha20 core against the RFC 8439 test vector, then the
//! DRBG's determinism, independence, reseed, and statistical sanity, and
//! reports the hardware-RNG seed source discovered on this machine.

#![no_std]
#![no_main]

use kernel::abi::ShellIo;
use kernel::rng::{self, Drbg, SeedSource, chacha};
use kernel::{arch, println, user_progs};

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
    test_umode_urng();
    test_hwrng_and_seed_source();

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

static mut TEST_IO: ShellIo = ShellIo::ZERO;

/// The U-mode per-cell DRBG (the shell's `rand` path) is a library call over
/// ShellIo state. Verify its fast-key-erasure stream matches an independent
/// ChaCha20 computation. Called here in kernel mode; the shell runs the same
/// code in U-mode (shell-smoke exercises that path).
fn test_umode_urng() {
    let key = [0x33u8; 32];
    let io = unsafe { &mut *core::ptr::addr_of_mut!(TEST_IO) };
    io.rng_key = key;
    io.rng_pos = 32; // spent -> first draw re-keys

    // First 8 stream bytes = bytes 32..40 of block(key, counter=0, nonce=0).
    let mut blk = [0u8; 64];
    chacha::block(&key, 0, &[0u8; 12], &mut blk);
    let mut e = [0u8; 8];
    e.copy_from_slice(&blk[32..40]);
    let expected = u64::from_le_bytes(e);

    // SAFETY: TEST_IO is a live, seeded ShellIo static.
    let (got, a, b) = unsafe {
        (
            user_progs::urng_next_u64(core::ptr::addr_of_mut!(TEST_IO)),
            user_progs::urng_next_u64(core::ptr::addr_of_mut!(TEST_IO)),
            user_progs::urng_next_u64(core::ptr::addr_of_mut!(TEST_IO)),
        )
    };
    assert!(got == expected, "U-mode DRBG stream mismatch");
    assert!(a != b && a != got, "U-mode DRBG repeated a value");
    println!("rng: U-mode per-cell library-call DRBG OK");
}

/// Report the hardware RNG and root seed source. When a hwrng is present its
/// words must vary (not stuck); the root must then be hardware-seeded.
fn test_hwrng_and_seed_source() {
    let src = rng::seed_source();
    println!(
        "rng: hwrng={} present={} root_seed_source={:?}",
        arch::hwrng_name(),
        arch::has_hwrng(),
        src
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
        assert!(
            src == SeedSource::Hwrng,
            "hwrng present but root not hardware-seeded ({src:?})"
        );
        // Continuous reseed must succeed with a live source.
        assert!(rng::reseed_root(), "root reseed from hwrng failed");
        println!("rng: hwrng live + root hardware-seeded OK");
    } else {
        assert!(
            src == SeedSource::Fallback,
            "no hwrng but seed source is {src:?}"
        );
        println!("rng: no hwrng, documented fallback seed OK");
    }
}
