// A model-checking fuzzer over the **shipped** kernel telemetry rings
// (docs/LOGGING.md, kernel/src/telemetry.rs), included verbatim.
//
// WHY A FUZZER AND NOT A BOOT TEST. The ring is free-running counters masked into a slot
// array, so every interesting case is a wrap-around: `head`/`tail` crossing `u32::MAX`,
// a full ring, a ring emptied and refilled across the boundary, a drain interleaved with
// pushes. A boot test emits a few hundred records and never reaches any of them - it would
// pass on an implementation that is wrong after four billion messages, which is a number a
// long-lived kernel reaches. Here the counters can be *started* near the boundary and
// crossed in milliseconds.
//
// The oracle is an independent model - a `VecDeque` of what was pushed and not dropped -
// and the ring must agree with it on every step. Deliberately not computed from the ring's
// own fields: the `entity` fuzzer's first I5 check asked the code under test whether work
// existed, and passed while stranding work, because both sides agreed on a wrong answer
// (verify/README.md).
//
// Run it with `cargo xtask verify`.

use std::collections::VecDeque;

#[path = "../../kernel/src/telemetry.rs"]
mod telemetry;

use telemetry::{Level, Record, Ring, Rings, MAX_RING_CPUS, PAYLOAD_MAX, SLOTS};

/// A cheap deterministic PRNG so a failing run is reproducible without a dependency
/// (the `json/src/scan.rs` convention).
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 33
}

/// What a record's payload should be for a given sequence number, so a torn or misplaced
/// record is detectable from its contents rather than only from its count.
///
/// Keyed on the sequence number AND the CPU, which is the property a per-CPU ring has to
/// have: CPU 1's bytes must never appear in CPU 0's ring. That is the kernel-side analogue
/// of `regstress.c` keying every value on its own pid (docs/SMP.md 10.0d).
fn payload_for(cpu: u16, seq: u64, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    for i in 0..len {
        v.push((cpu as u64).wrapping_mul(0x9E37).wrapping_add(seq).wrapping_add(i as u64) as u8);
    }
    v
}

// ------------------------------------------------------------------ single-ring model

/// Push and pop one ring against a `VecDeque` oracle, with `head`/`tail` started at
/// `start` so the wrap boundary is reachable.
fn ring_model(seed: u64, start: u32, steps: usize) -> Result<(), String> {
    let mut ring = Ring::EMPTY;
    // Start both counters at `start`: an empty ring at an arbitrary point in the counter
    // space, which is what a long-lived kernel's ring actually is.
    ring.seek_for_test(start, start);
    let mut model: VecDeque<Vec<u8>> = VecDeque::new();
    let mut st = seed;
    let mut seq = 0u64;
    let mut pushed = 0u32;
    let mut dropped = 0u32;

    for _ in 0..steps {
        if lcg(&mut st) % 100 < 60 {
            // A length distribution that includes 0, the exact capacity, and over-capacity,
            // because those three are the boundaries and a uniform draw over 1..96 misses
            // all of them.
            let len = match lcg(&mut st) % 10 {
                0 => 0,
                1 => PAYLOAD_MAX,
                2 => PAYLOAD_MAX + 17,
                _ => (lcg(&mut st) as usize) % PAYLOAD_MAX,
            };
            let body = payload_for(0, seq, len);
            let ok = ring.push(Level::Info, 0, seq, &body);
            let kept = len.min(PAYLOAD_MAX);
            if ok {
                if model.len() >= SLOTS {
                    return Err(format!("accepted a push into a full ring at seq {seq}"));
                }
                model.push_back(body[..kept].to_vec());
                pushed += 1;
            } else {
                if model.len() < SLOTS {
                    return Err(format!(
                        "dropped a push at seq {seq} with only {} of {SLOTS} slots used",
                        model.len()
                    ));
                }
                dropped += 1;
            }
            seq += 1;
        } else {
            match (ring.pop(), model.pop_front()) {
                (Some(rec), Some(want)) => {
                    if rec.bytes() != &want[..] {
                        return Err(format!(
                            "popped {} bytes, expected {} - a record was torn or misplaced",
                            rec.bytes().len(),
                            want.len()
                        ));
                    }
                }
                (None, None) => {}
                (Some(_), None) => return Err("ring produced a record the model did not".into()),
                (None, Some(_)) => return Err("ring is empty and the model is not".into()),
            }
        }
        if ring.pending() != model.len() {
            return Err(format!(
                "pending {} vs model {} - the counters and the contents disagree",
                ring.pending(),
                model.len()
            ));
        }
    }

    let (w, d) = ring.counters();
    if w != pushed || d != dropped {
        return Err(format!(
            "counters ({w} written, {d} dropped) disagree with the model ({pushed}, {dropped})"
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------ merge model

/// Several CPUs push with interleaved timestamps; the drain must come out in
/// non-decreasing timestamp order and every record must reach the right consumer with its
/// own CPU's bytes.
fn merge_model(seed: u64, cpus: u16, steps: usize) -> Result<(), String> {
    let mut rings = Rings::EMPTY;
    rings.set_buffered(true);
    let mut st = seed;
    let mut expect: Vec<(u64, u16, Vec<u8>)> = Vec::new();
    let mut seq = 0u64;

    for _ in 0..steps {
        let cpu = (lcg(&mut st) % cpus as u64) as u16;
        // Timestamps deliberately NOT monotonic per CPU on their own - they are drawn from
        // a shared increasing clock with jitter, which is what several cores reading one
        // counter actually looks like.
        let ts = seq * 10 + lcg(&mut st) % 7;
        let len = (lcg(&mut st) as usize) % 32;
        let body = payload_for(cpu, seq, len);
        if rings.push(Level::Info, cpu, ts, &body) {
            expect.push((ts, cpu, body));
        }
        seq += 1;
    }

    // The oracle: sorted by timestamp, ties by CPU - the order `pop_oldest` documents.
    // Computed here from what was pushed, never read back out of the rings.
    expect.sort_by_key(|(ts, cpu, _)| (*ts, *cpu));

    let mut last_ts = 0u64;
    for (i, (ts, cpu, body)) in expect.iter().enumerate() {
        let Some(rec) = rings.pop_oldest() else {
            return Err(format!("drain ran dry after {i} of {} records", expect.len()));
        };
        if rec.ts_ns < last_ts {
            return Err(format!(
                "drain went backwards in time: {} after {last_ts}",
                rec.ts_ns
            ));
        }
        last_ts = rec.ts_ns;
        if rec.ts_ns != *ts || rec.cpu != *cpu {
            return Err(format!(
                "record {i}: got (ts {}, cpu {}), expected (ts {ts}, cpu {cpu}) - the merge \
                 order is not timestamp-then-CPU",
                rec.ts_ns, rec.cpu
            ));
        }
        if rec.bytes() != &body[..] {
            return Err(format!(
                "record {i} from CPU {cpu} carries another producer's bytes"
            ));
        }
    }
    if rings.pop_oldest().is_some() {
        return Err("drain produced more records than were pushed".into());
    }
    Ok(())
}

/// Buffering off must record nothing and count every offer, so "nothing was logged" and
/// "logging was never enabled" are distinguishable. The `Ret(0)` lesson: a path that
/// reports success while doing nothing is the defect docs/ENGINEERING.md 7 names.
fn bypass_model() -> Result<(), String> {
    let mut rings = Rings::EMPTY;
    for i in 0..10u64 {
        if rings.push(Level::Error, 0, i, b"x") {
            return Err("a push succeeded while buffering was off".into());
        }
    }
    let (w, d, bypassed) = rings.counters();
    if w != 0 || d != 0 {
        return Err(format!("buffering off yet {w} written, {d} dropped"));
    }
    if bypassed != 10 {
        return Err(format!("{bypassed} offers counted, expected 10"));
    }
    Ok(())
}

/// A level above the threshold is not recorded, and is **not** counted as a drop - a
/// filtered record was never offered to the ring, and conflating the two would make a
/// quiet boot look like an overflowing one.
fn threshold_model() -> Result<(), String> {
    let mut rings = Rings::EMPTY;
    rings.set_buffered(true);
    rings.set_threshold(Level::Warn);
    if !rings.push(Level::Error, 0, 1, b"e") {
        return Err("Error was filtered at threshold Warn".into());
    }
    if !rings.push(Level::Warn, 0, 2, b"w") {
        return Err("Warn was filtered at threshold Warn".into());
    }
    if rings.push(Level::Info, 0, 3, b"i") {
        return Err("Info was recorded at threshold Warn".into());
    }
    if rings.push(Level::Trace, 0, 4, b"t") {
        return Err("Trace was recorded at threshold Warn".into());
    }
    let (w, d, _) = rings.counters();
    if w != 2 {
        return Err(format!("{w} records written, expected 2"));
    }
    if d != 0 {
        return Err(format!("{d} counted as dropped - a filtered record is not a drop"));
    }
    Ok(())
}

/// A CPU index beyond the ring array is counted, not silently discarded: "this machine has
/// more CPUs than the rings were sized for" has to be visible in the numbers.
fn overflow_cpu_model() -> Result<(), String> {
    let mut rings = Rings::EMPTY;
    rings.set_buffered(true);
    if rings.push(Level::Info, MAX_RING_CPUS as u16, 1, b"x") {
        return Err("a push from an out-of-range CPU was accepted".into());
    }
    let (_, d, _) = rings.counters();
    if d != 1 {
        return Err(format!("{d} drops recorded for an out-of-range CPU, expected 1"));
    }
    Ok(())
}

/// An over-long record keeps its head and says so. Truncation that is not marked is a
/// short-looking line indistinguishable from a genuinely short one.
fn truncation_model() -> Result<(), String> {
    let mut rings = Rings::EMPTY;
    rings.set_buffered(true);
    let long = vec![b'z'; PAYLOAD_MAX + 40];
    rings.push(Level::Info, 0, 1, &long);
    let rec: Record = rings.pop_oldest().ok_or("nothing recorded")?;
    if !rec.truncated {
        return Err("an over-long record was not marked truncated".into());
    }
    if rec.bytes().len() != PAYLOAD_MAX {
        return Err(format!("kept {} bytes, expected {PAYLOAD_MAX}", rec.bytes().len()));
    }
    // And the head, not the tail: the identifying text of a kernel message is at the front.
    if rec.bytes()[0] != b'z' {
        return Err("truncation kept the wrong end".into());
    }
    Ok(())
}

fn main() {
    let mut failures = 0usize;

    println!("== telemetry ring: deterministic properties ==");
    for (name, r) in [
        ("buffering off records nothing and counts offers", bypass_model()),
        ("a level above the threshold is filtered, not dropped", threshold_model()),
        ("an out-of-range CPU is counted", overflow_cpu_model()),
        ("an over-long record keeps its head and is marked", truncation_model()),
    ] {
        match r {
            Ok(()) => println!("  ok   {name}"),
            Err(e) => {
                println!("  FAIL {name}: {e}");
                failures += 1;
            }
        }
    }

    // The wrap boundary is the point of this driver, so it is *aimed at*, not hoped for:
    // one start value is `u32::MAX - 8`, so the very first pushes cross it.
    println!("== telemetry ring: push/pop against a model, across the counter wrap ==");
    const STEPS: usize = 4_000;
    for (label, start) in [("from 0", 0u32), ("across u32::MAX", u32::MAX - 8)] {
        let mut bad = 0;
        for run in 0..2_000u64 {
            if let Err(e) = ring_model(0x51ED ^ run.wrapping_mul(0x9E3779B9), start, STEPS) {
                if bad == 0 {
                    println!("  FAIL {label}, seed {run}: {e}");
                }
                bad += 1;
            }
        }
        if bad == 0 {
            println!("  ok   {label}: 2000 runs x {STEPS} operations");
        } else {
            failures += 1;
        }
    }

    println!("== telemetry ring: the per-CPU merge ==");
    let mut bad = 0;
    for run in 0..2_000u64 {
        // Widths deliberately include 1 (the merge degenerating to one ring) and the full
        // array, because a merge that only works for several rings and a merge that only
        // works for one are different bugs.
        let cpus = 1 + (run % MAX_RING_CPUS as u64) as u16;
        if let Err(e) = merge_model(0xB0B ^ run.wrapping_mul(0x100_0001B3), cpus, SLOTS) {
            if bad == 0 {
                println!("  FAIL {cpus} CPUs, seed {run}: {e}");
            }
            bad += 1;
        }
    }
    if bad == 0 {
        println!("  ok   2000 runs, 1..{MAX_RING_CPUS} CPUs, ordered by timestamp then CPU");
    } else {
        failures += 1;
    }

    if failures > 0 {
        println!("telemetry fuzz: FAIL ({failures} properties)");
        std::process::exit(1);
    }
    println!("telemetry fuzz: PASS");
}
