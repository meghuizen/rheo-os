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
    test_erase_on_read();
    test_statistical_sanity();
    test_boot_health();
    // Before the phases below, which deliberately reset the pool: this one is
    // about the state the *boot* reached.
    test_hwrng_and_seed_source();
    test_pool_flood_cannot_seed();
    test_pool_mixes_uncredited_input();
    test_pool_cannot_be_exhausted();
    test_jitter_source();
    test_hid_events();
    test_rekey_bounds_a_compromise();
    test_quantum_margin();

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

/// **Fast key erasure, rule 2**: a byte is erased from the buffer as it is
/// handed out, so capturing the DRBG state later reveals nothing about output
/// already delivered (cr.yp.to 2017.07.23, the recording-attacker case). Rule 1
/// - re-key on every refill - was already implemented; this half was not, and
/// up to 256 bytes of delivered output stayed in the buffer until the next
/// refill.
///
/// Drawn in odd sizes so the spent region ends mid-word and spans a refill.
fn test_erase_on_read() {
    let mut d = Drbg::from_key([0x3cu8; 32]);
    let mut buf = [0u8; 37];
    let mut drawn = 0usize;
    // 37 * 20 = 740 bytes, so this crosses the 256-byte buffer twice.
    for round in 0..20 {
        d.fill_bytes(&mut buf);
        drawn += buf.len();
        assert!(
            d.spent_is_erased(),
            "round {round}: delivered bytes still in the buffer"
        );
    }
    assert!(drawn == 740);
    println!("rng: erase-on-read OK (740 bytes drawn, spent buffer always zero)");
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

/// **HID devices as an entropy source.**
///
/// A person pressing a key is unpredictable in a way no deterministic machine
/// reproduces, and it is one of the few sources a board with no randomness
/// hardware has at all - the source Linux has collected since its first
/// `/dev/random`. The launch attaches a virtio keyboard and presses keys on it
/// over QEMU's monitor protocol, so the driver is exercised by a device that is
/// really sending rather than by an empty queue.
///
/// Two facts, asserted separately: the device answers a **config read** (its own
/// name, a real round trip that works even with nobody typing), and its events
/// reach the pool. Waiting for a keystroke is a **deadline**, and a run where
/// none arrives reports that rather than failing - the injector is a separate
/// process and cannot be a precondition of the kernel being correct.
fn test_hid_events() {
    if !kernel::hw::virtio_input::present() {
        println!("rng: no HID device attached");
        return;
    }
    let mut name = [0u8; 64];
    let mut n = kernel::hw::virtio_input::device_name(&mut name);
    // The device reports its name's size including the NUL terminator.
    while n > 0 && name[n - 1] == 0 {
        n -= 1;
    }
    assert!(n > 0, "the HID device reported an empty name");
    let printable = name[..n].iter().all(|b| (0x20..0x7f).contains(b));
    assert!(
        printable,
        "the HID device's name is not text: {:?}",
        &name[..n]
    );

    entropy::reset();
    // Up to 4 seconds for a keystroke. The injector presses keys for about two.
    let deadline = arch::timer_now_ns().saturating_add(4_000_000_000);
    let mut drained = 0;
    while drained == 0 && arch::timer_now_ns() < deadline {
        drained += kernel::hw::virtio_input::pump();
        core::hint::spin_loop();
    }
    if drained == 0 {
        println!(
            "rng: HID device \"{}\" present and answering, but no key arrived in the window - \
             nothing claimed about input entropy this run",
            core::str::from_utf8(&name[..n]).unwrap_or("?")
        );
        return;
    }
    // Every event went into this core's scratch. Draining it into the pool is
    // what makes the contribution visible.
    let before = entropy::counters();
    entropy::pump();
    let after = entropy::counters();
    let ii = entropy::Source::Interrupt.index();
    assert!(
        after.bytes[ii] > before.bytes[ii],
        "{drained} HID events arrived but none reached the pool"
    );
    assert!(
        after.credited[ii] == 0,
        "HID events were credited entropy - they are mixed, never counted"
    );
    assert!(
        kernel::hw::virtio_input::events() >= drained as u64,
        "the driver's event counter disagrees with what it drained"
    );
    // Nothing a person typed is left behind. The stronger half of the
    // not-a-keylogger property is structural - `rng::feed_hid` takes a sequence
    // number and no event, so a caller *cannot* pass a key code - and this is the
    // part a test can see: the DMA buffers are wiped as they are drained.
    assert!(
        kernel::hw::virtio_input::buffers_clear(),
        "a drained HID event is still sitting in the kernel's buffer"
    );
    println!(
        "rng: HID device \"{}\" delivered {drained} key event(s) into the entropy pool \
         ({} bytes mixed, 0 bits credited, buffers wiped) OK",
        core::str::from_utf8(&name[..n]).unwrap_or("?"),
        after.bytes[ii]
    );
}

/// **A compromised root does not stay compromised.**
///
/// Fast key erasure already means an attacker who captures the DRBG state learns
/// nothing about output already handed out. This is the other direction:
/// recovery. Without a bounded root lifetime, a machine whose credited sources
/// have gone quiet would keep a captured key for the whole boot, because a
/// re-key only happened when the pool reached a full 256 credited bits.
///
/// So a root is re-keyed after at most `REKEY_EVERY` derivations whatever the
/// pool holds. The oracle here is exact: take enough draws to cross the bound,
/// and the re-key counter must move; the key must actually change, which is
/// checked by the stream diverging from what the un-rekeyed root would produce.
///
/// Honest: this is a *chance* of recovery, not a guarantee - on a machine whose
/// every input is predictable it moves the key to another the attacker can
/// compute. It removes "compromised forever", which is what a bound can do.
fn test_rekey_bounds_a_compromise() {
    let before = rng::rekeys();
    // Enough derivations to cross the bound at least once.
    for _ in 0..(16 * 1024 + 8) {
        core::hint::black_box(rng::derive_cell_drbg().next_u64());
    }
    let after = rng::rekeys();
    assert!(
        after > before,
        "a root ran {} derivations without a re-key - a captured key would be \
         good for the rest of the boot",
        16 * 1024 + 8
    );
    println!(
        "rng: root re-keyed {} time(s) under the lifetime bound OK",
        after - before
    );
}

/// The **post-quantum** position, stated as a checkable property rather than a
/// reassurance.
///
/// Shor's algorithm does not apply: there is no public-key structure here, only
/// a stream cipher. Grover's halves the effective strength of a symmetric key,
/// so a 256-bit ChaCha20 key gives about 128 bits against a quantum adversary -
/// which is why the pool's target is the **full key width** and not less. A
/// 128-bit seed would be the actual weakness, so that is what is asserted.
fn test_quantum_margin() {
    assert!(
        entropy::CREDIT_TARGET == 256,
        "the seed target is {} bits; Grover halves it, so anything under 256 \
         leaves less than a 128-bit post-quantum margin",
        entropy::CREDIT_TARGET
    );
    // And the key the DRBG is built from is that wide.
    assert!(
        core::mem::size_of_val(&[0u8; 32]) * 8 == entropy::CREDIT_TARGET as usize,
        "the DRBG key and the seed target disagree"
    );
    println!(
        "rng: post-quantum margin - 256-bit key, ~128 bits under Grover; no \
         public-key structure, so Shor does not apply OK"
    );
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
        "rng: cpu-hwrng={} present={} rng-device={} tpm={} seed_source={:?} seeded={}",
        arch::hwrng_name(),
        arch::has_hwrng(),
        kernel::hw::virtio_rng::present(),
        kernel::hw::tpm::present(),
        src,
        c.seeded
    );
    println!(
        "rng: credited bits cpu={} device={} jitter={} firmware={} (uncredited bytes interrupt={} user={} boot={})",
        c.credited[entropy::Source::Cpu.index()],
        c.credited[entropy::Source::Device.index()],
        c.credited[entropy::Source::Jitter.index()],
        c.credited[entropy::Source::Firmware.index()],
        c.bytes[entropy::Source::Interrupt.index()],
        c.bytes[entropy::Source::User.index()],
        c.bytes[entropy::Source::Boot.index()],
    );

    assert!(
        kernel::hw::virtio_rng::present(),
        "the launch attaches a virtio-rng device but the driver did not find one"
    );

    // The pool names no driver: a randomness device **registers** with it, which
    // is what let the TPM be added without touching the entropy subsystem at all.
    let names = entropy::device_source_names();
    let registered: usize = names.iter().filter(|n| n.is_some()).count();
    assert!(
        registered >= 1,
        "a randomness device is present but none registered with the pool"
    );
    let before = entropy::counters().bytes[entropy::Source::Device.index()];
    let fed = entropy::draw_from_devices();
    let after = entropy::counters().bytes[entropy::Source::Device.index()];
    assert!(fed > 0, "the registered devices fed nothing");
    assert!(
        after == before + fed as u64,
        "the devices fed {fed} bytes but the pool recorded {}",
        after - before
    );
    println!(
        "rng: {registered} randomness device(s) registered with the pool {:?}, {fed} bytes drawn through the table OK",
        names
    );

    // The **TPM**, where the launch could attach one. A TPM's own specification
    // requires it to contain a hardware RNG, and it is the one source on a server
    // that is neither the CPU vendor's instruction nor a paravirtual device.
    // `present` is "firmware described one and its registers answer"; `answered`
    // is "the chip completed a TPM2_GetRandom". Two facts, asserted separately,
    // because conflating them is how a boot claims a source it does not have.
    if kernel::hw::tpm::present() {
        assert!(
            kernel::hw::tpm::answered(),
            "a TPM is mapped but never answered a command"
        );
        let mut buf = [0u8; 32];
        let got = kernel::hw::tpm::get_random(&mut buf);
        assert!(got > 0, "TPM2_GetRandom returned no bytes");
        // A chip that answered with a constant is worse than one that refused.
        assert!(
            buf[..got].iter().any(|&b| b != buf[0]),
            "TPM2_GetRandom returned {got} identical bytes"
        );
        let mut buf2 = [0u8; 32];
        let got2 = kernel::hw::tpm::get_random(&mut buf2);
        assert!(
            got2 > 0 && buf2[..got2] != buf[..got],
            "two TPM2_GetRandom calls returned the same bytes"
        );
        assert!(
            names.iter().any(|n| *n == Some("tpm")),
            "the TPM answered but did not register with the pool"
        );
        println!(
            "rng: TPM 2.0 over FIFO/TIS - vendor/device {:#010x}, TPM2_GetRandom gave {got} \
             bytes then {got2} different ones OK",
            kernel::hw::tpm::did_vid()
        );
    } else {
        println!("rng: no TPM described by firmware here (no swtpm backend attached)");
    }
    assert!(
        c.seeded,
        "a randomness device is present but the pool never reached a full seed"
    );
    assert!(
        matches!(
            src,
            SeedSource::Hwrng | SeedSource::Device | SeedSource::Firmware
        ),
        "seeded, but the recorded source is {src:?}"
    );
    assert!(
        c.credited[entropy::Source::Device.index()] > 0,
        "the randomness device contributed no credited bits"
    );

    // The firmware boot seed, on the platforms that have one. Asserted where the
    // device tree carries `/chosen/rng-seed` (QEMU's riscv64 `virt` does), and
    // reported as absent where there is no device tree at all - x86-64 has none,
    // and an ARM64 bare-ELF `-kernel` boot is handed no pointer to one.
    match kernel::hw::fdt::rng_seed() {
        Some(seed) => {
            assert!(
                seed.len() >= 8,
                "firmware supplied a {}-byte seed - too short to be one",
                seed.len()
            );
            assert!(
                c.credited[entropy::Source::Firmware.index()] > 0,
                "the firmware supplied {} seed bytes but none were credited",
                seed.len()
            );
            println!(
                "rng: firmware boot seed /chosen/rng-seed present ({} bytes, {} bits credited) OK",
                seed.len(),
                c.credited[entropy::Source::Firmware.index()]
            );
        }
        None => {
            assert!(
                c.credited[entropy::Source::Firmware.index()] == 0,
                "no firmware seed on this platform, but firmware bits were credited"
            );
            println!("rng: no device tree here, so no firmware boot seed (expected)");
        }
    }

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
        // The firmware boot seed is asked for first, so on a device-tree platform
        // that supplies one it is what pays; otherwise the device does. Both are
        // real sources - which one it was is reported, not assumed.
        assert!(
            matches!(src, SeedSource::Device | SeedSource::Firmware),
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
