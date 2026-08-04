// A model-checking fuzzer over the **shipped** observability event ring
// (docs/OBSERVABILITY.md 11, kernel/src/obs/ring.rs), included verbatim.
//
// WHY A FUZZER AND NOT A BOOT TEST. Every interesting case in a ring is a boundary,
// and a boot reaches none of them. `head` is a free-running u64: crossing it takes
// 2^64 events, so the arithmetic around the wrap would ship untested forever, and an
// untested wrap in a ring is how a long-lived kernel starts handing out other
// events' records. A full ring, a slot recycled under a reader, and a reader whose
// cursor has fallen further behind than the whole ring are all reachable in
// milliseconds here and effectively unreachable in QEMU.
//
// THE ORACLE IS INDEPENDENT. A `VecDeque` of what was pushed, trimmed to the ring's
// capacity, and the ring must agree with it on every step. Deliberately not computed
// from the ring's own `head`/`capacity`: the `entity` fuzzer's first I5 check asked
// the code under test whether work existed and passed while stranding work, because
// both sides agreed on a wrong answer (verify/README.md).
//
// WHAT IT CANNOT DO, said up front. This checks one ring's arithmetic. It does not
// check that a CPU writes only its own ring (that is partitioning, enforced by
// `PerCpu` and proven by the in-QEMU multi-core phase), nor the release/acquire
// ordering between a writer and a cross-CPU reader (a host process running one
// thread cannot observe it), nor the tick source. Those stay with the boot tests.
//
// One consequence is worth naming rather than leaving to be rediscovered:
// `ObsRing::get`'s **sequence-number check does not fire here**, and that was
// measured, not assumed - removing it entirely leaves every case below passing.
// Sequentially the bounds test subsumes it, because a slot cannot be recycled
// between a bounds check and a read when nothing else is running. It earns its keep
// only against a reader racing a live writer on another core, which is exactly the
// thing a single-threaded host driver cannot produce, and simulating it would mean
// aliasing the ring mutably to model a data race the language forbids - a test whose
// green is worth nothing. The check is documented in `ring.rs` as reasoned rather
// than proven.
//
// Run it with `cargo xtask verify`.

use std::collections::VecDeque;

// ------------------------------------------------------------- host storage
//
// `ring.rs` is fully dependency-free now: it adopts a caller-allocated contiguous
// block and computes physical addresses through a caller-supplied closure. So the
// "shim" is one leaked, zeroed allocation - no `Funded` model at all, which is the
// payoff of the contiguous-ring architecture (docs/OBSERVABILITY.md 11.4) landing
// on the fuzzer too: less pretending, more of the shipped code under test.

/// A zeroed block the size the ring wants, leaked so its address is stable for the
/// process's life (a fuzzer run allocates a handful).
///
/// A `Vec<ObsEvent>` and not a `Vec<u64>`, because the ring requires its block
/// aligned to the record (align 32 - the never-straddles-a-cache-line property) and
/// the first version handed it a merely 8-aligned block: the debug-assertions build
/// refused at the `read_volatile`, which is exactly the class of contract this
/// driver exists to hold the shipped code - and its callers - to.
fn ring_block() -> usize {
    let v: Vec<ObsEvent> = vec![ObsEvent::default(); RING_EVENTS];
    Box::leak(v.into_boxed_slice()).as_ptr() as usize
}

// The ABI module is dependency-free and `no_std`, so it includes as-is.
#[path = "../../abi/src/obs.rs"]
pub mod obs_abi;

mod abi {
    pub use crate::obs_abi as obs;
}

// `ring.rs` names `crate::abi::obs` and `crate::mm::kmeta`, both shimmed above.
#[path = "../../kernel/src/obs/ring.rs"]
mod ring;

// The snapshot plane's seqlock (docs/OBSERVABILITY.md 11, S3), also dependency-free.
// This is the one piece of the plane whose interesting case a boot test cannot
// reach at all: the writer's bracket is ~6 stores, QEMU TCG interleaves far more
// coarsely than that, and the in-kernel reader runs on the same CPU as the writer.
// Real host threads are where a reader can actually catch a writer mid-update.
#[path = "../../kernel/src/obs/cpu.rs"]
mod cpu;

use abi::obs::{ObsCpu, ObsEvent};
use ring::{ObsRing, RING_EVENTS, seq_of};

/// A cheap deterministic PRNG so a failing run is reproducible without a dependency
/// (the `json/src/scan.rs` convention).
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 33
}

/// What event `n` should contain, so a misplaced or stale record is detectable from
/// its **contents** and not only from a count. Keyed on the event number, which is
/// the analogue of `regstress.c` keying every value on its own pid.
fn expect(n: u64) -> ObsEvent {
    ObsEvent {
        tick: n.wrapping_mul(7).wrapping_add(1),
        a: n.wrapping_mul(0x9E37_79B9),
        b: !n,
        seq: seq_of(n),
        owner: (n % 61) as u16,
        window: (n % 14) as u8,
        kind: (n % 7) as u8,
    }
}

fn push_expected(r: &mut ObsRing, n: u64) {
    let e = expect(n);
    r.push(e.tick, e.window, e.kind, e.owner, e.a, e.b);
}

// ------------------------------------------------------------------ the model

/// Push into one ring against a `VecDeque` oracle, with `head` started at `start` so
/// the wrap boundary is reachable.
fn ring_model(seed: u64, start: u64, steps: usize) -> Result<(), String> {
    let mut r = ObsRing::new();
    r.fund(3, ring_block(), |va| va & 0x0000_ffff_ffff_ffff);
    if !r.funded() {
        return Err("fund did not adopt the block".into());
    }
    let cap = r.capacity() as u64;
    if cap != RING_EVENTS as u64 {
        return Err(format!("funded capacity {cap}, want {RING_EVENTS}"));
    }
    r.seek_for_test(start);

    // The oracle: event numbers still readable, oldest first.
    let mut model: VecDeque<u64> = VecDeque::new();
    let mut st = seed;
    let mut n = start;

    // Fill the ring before asserting anything, when the counter was moved.
    //
    // `held` is `min(head, capacity)`, which is exactly right for every ring the
    // kernel builds - one funded at head 0 and counted up. Seeking is a fiction this
    // fuzzer creates to reach the wrap, and immediately after a seek the ring claims
    // to hold `capacity` events of which none were ever pushed. That is the fiction's
    // fault, not the ring's, so it is removed here rather than papered over with a
    // base offset in the published header - a field no kernel path would ever set to
    // anything but zero, existing only to make a test's pretence self-consistent.
    if start != 0 {
        for _ in 0..cap {
            push_expected(&mut r, n);
            model.push_back(n);
            n = n.wrapping_add(1);
        }
    }

    for _ in 0..steps {
        // A burst distribution that includes 0, 1, exactly the capacity and more
        // than the capacity, because those are the boundaries and a uniform draw
        // over 1..100 misses all of them.
        let burst = match lcg(&mut st) % 12 {
            0 => 0,
            1 => 1,
            2 => RING_EVENTS as u64,
            3 => RING_EVENTS as u64 + 1,
            4 => RING_EVENTS as u64 * 2 + 3,
            _ => lcg(&mut st) % 97,
        };
        for _ in 0..burst {
            push_expected(&mut r, n);
            model.push_back(n);
            if model.len() > cap as usize {
                model.pop_front();
            }
            n = n.wrapping_add(1);
        }

        // 1. The ring agrees with the oracle about *which* events survive.
        if r.written() != n {
            return Err(format!("written {} != {n}", r.written()));
        }
        if r.held() != model.len() {
            return Err(format!("held {} != model {}", r.held(), model.len()));
        }
        if r.oldest() != *model.front().unwrap_or(&n) {
            return Err(format!(
                "oldest {} != model {}",
                r.oldest(),
                model.front().copied().unwrap_or(n)
            ));
        }

        // 2. Every surviving event reads back **byte for byte**. A count alone would
        //    pass on a ring that kept the right number of the wrong records, which is
        //    exactly what an off-by-one in the slot mask produces.
        for &want_n in &model {
            let got = r
                .get(want_n)
                .ok_or_else(|| format!("event {want_n} is gone but the model holds it"))?;
            let want = expect(want_n);
            if got != want {
                return Err(format!("event {want_n}: got {got:?} want {want:?}"));
            }
        }

        // 3. Everything the oracle does **not** hold must be refused, in both
        //    directions: an event already overwritten, and one not yet written. This
        //    is the half that catches a bounds check written as `<=`, and the half a
        //    "walk the survivors" test cannot see.
        if let Some(&front) = model.front() {
            if front > start && r.get(front - 1).is_some() {
                return Err(format!(
                    "event {} was overwritten but reads back",
                    front - 1
                ));
            }
        }
        if r.get(n).is_some() {
            return Err(format!("event {n} has not been written but reads back"));
        }
        if n > 0 && r.get(n.wrapping_add(1_000)).is_some() {
            return Err("an event far in the future reads back".into());
        }
    }
    Ok(())
}

/// A ring the pool refused must record nothing, count every offered emit, and stay
/// usable as a reader (answering `None`) rather than misbehaving.
fn unfunded_model() -> Result<(), String> {
    // The pool refusing IS the caller never calling `fund` - allocation lives with
    // the caller now, so an unfunded ring is simply one that was never given a
    // block, and the ring's own job is to count what it was offered meanwhile.
    let mut r = ObsRing::new();
    if r.funded() {
        return Err("a ring never given a block reports itself funded".into());
    }
    for i in 0..50 {
        push_expected(&mut r, i);
    }
    if r.written() != 0 {
        return Err(format!("unfunded ring wrote {} events", r.written()));
    }
    if r.unfunded_emits() != 50 {
        return Err(format!(
            "unfunded ring counted {} of 50 offered emits - 'the window was on but \
             this CPU had no memory' must be distinguishable from 'nothing happened'",
            r.unfunded_emits()
        ));
    }
    if r.get(0).is_some() {
        return Err("an unfunded ring returned an event".into());
    }
    Ok(())
}

/// A funded ring released must give everything back and behave like a fresh one.
fn release_model() -> Result<(), String> {
    let mut r = ObsRing::new();
    r.fund(0, ring_block(), |va| va);
    for i in 0..(RING_EVENTS as u64 + 7) {
        push_expected(&mut r, i);
    }
    if r.get(RING_EVENTS as u64).is_none() {
        return Err("a written event is missing before release".into());
    }
    r.release();
    if r.funded() || r.written() != 0 || r.held() != 0 {
        return Err("a released ring is not empty".into());
    }
    if r.get(0).is_some() {
        return Err("a released ring returned an event".into());
    }
    // And it can be funded again - the between-runs path.
    r.fund(0, ring_block(), |va| va);
    if !r.funded() {
        return Err("re-funding a released ring refused".into());
    }
    push_expected(&mut r, 0);
    if r.get(0).is_none() {
        return Err("a re-funded ring dropped its first event".into());
    }
    Ok(())
}

/// A zeroed slot must not read as event 0.
///
/// The ring is backed by freshly allocated frames, which arrive zeroed, and a zeroed
/// record has `seq == 0`. If sequence numbers were zero-based, every untouched slot
/// of a partly-filled ring would answer as a legitimate event - so `seq_of` is
/// one-based, and this is the check that says so.
fn zero_slot_model() -> Result<(), String> {
    if seq_of(0) == 0 {
        return Err("seq_of(0) is 0, so a zeroed frame reads as a written event".into());
    }
    let mut r = ObsRing::new();
    r.fund(0, ring_block(), |va| va);
    push_expected(&mut r, 0);
    // Slot 1 has never been written and is still zero. `written` is 1, so `get(1)`
    // must be refused on the bounds - and `get` of a *stale* generation must be
    // refused on the sequence number, which the wrap phase above exercises.
    if r.get(1).is_some() {
        return Err("an untouched zeroed slot read back as an event".into());
    }
    Ok(())
}

fn main() {
    let mut failures = 0usize;

    println!("== observability ring: deterministic properties ==");
    for (name, res) in [
        ("a pool refusal is counted, not lost", unfunded_model()),
        ("release returns everything and re-funds", release_model()),
        ("a zeroed slot is not event 0", zero_slot_model()),
    ] {
        match res {
            Ok(()) => println!("  ok   {name}"),
            Err(e) => {
                println!("  FAIL {name}: {e}");
                failures += 1;
            }
        }
    }

    // The wrap boundaries, which are the whole reason this file exists.
    //
    // `head` is a free-running u64 and the recorded sequence number is its low 32
    // bits, so the boundary that matters is **2^32**: four billion events is about
    // 71 minutes at one event per microsecond, which a long-lived kernel reaches,
    // and it is where a stale slot could carry a sequence number that looks current.
    // Reached by *starting* the counter near it.
    //
    // `u64` wrap is deliberately not tested, and that is a statement about the
    // design rather than a gap in the fuzzer: at one event per nanosecond it is 584
    // years, so the ring does not claim to survive it and asserting that it does
    // would be inventing a requirement.
    println!("== observability ring: wrap boundaries ==");
    let starts: [(&str, u64); 4] = [
        ("from zero", 0),
        ("mid-range", 1 << 40),
        ("one ring below u32::MAX", u32::MAX as u64 - RING_EVENTS as u64),
        ("across u32::MAX", u32::MAX as u64 - 3),
    ];
    for (name, start) in starts {
        let mut bad = 0usize;
        for seed in 0..40u64 {
            if let Err(e) = ring_model(seed, start, 40) {
                if bad == 0 {
                    println!("  FAIL {name}: seed {seed}: {e}");
                }
                bad += 1;
            }
        }
        if bad == 0 {
            println!("  ok   {name} (40 seeds x 40 bursts)");
        } else {
            failures += bad;
        }
    }

    println!("== snapshot plane: the seqlock ==");
    for (name, res) in [
        ("busy/idle arithmetic is exact", seqlock_arithmetic()),
        ("a racing reader never sees a torn group", seqlock_race()),
        ("CONTROL: an unbracketed writer IS caught torn", seqlock_torn_control()),
    ] {
        match res {
            Ok(msg) => println!("  ok   {name}{msg}"),
            Err(e) => {
                println!("  FAIL {name}: {e}");
                failures += 1;
            }
        }
    }

    if failures == 0 {
        println!("obs fuzz: PASS");
    } else {
        println!("obs fuzz: FAIL ({failures} failure(s))");
        std::process::exit(1);
    }
}

// ------------------------------------------------------------ the seqlock model
//
// The writer publishes only **self-consistent** tuples: at step k the group is
// `(state, cell, entity, vcore, since) = (k%5, k, k+1, k+2, k)`. A reader that ever
// sees `entity != cell+1` (or any other relation broken) has caught a group that
// existed at no instant - the torn triple docs/OBSERVABILITY.md 11's plan names as
// exactly what the seqlock exists to make impossible. The invariant is on the
// *contents*, so it needs no shared state with the writer (the regstress.c idea).

/// Whether one reading satisfies the writer's invariant (or is the initial zero
/// group, which the reader can legitimately catch before the first write).
fn coherent(s: &cpu::CpuSnap) -> bool {
    if s.since_tick == 0 {
        return s.cell == 0 && s.entity == 0 && s.vcore == 0 && s.state == 0;
    }
    let k32 = s.since_tick as u32;
    s.cell == k32
        && s.entity == k32.wrapping_add(1)
        && s.vcore == k32.wrapping_add(2)
        && s.state == (s.since_tick % 5) as u32
}

/// Deterministic single-thread checks: the first interval is dropped, every later
/// one lands in exactly one of busy/idle, and their sum is exact.
fn seqlock_arithmetic() -> Result<String, String> {
    let c = Box::leak(Box::new(ObsCpu::new())) as *mut ObsCpu;
    const STEPS: u64 = 10_000;
    for k in 1..=STEPS {
        // now = 3k, so every interval is 3 ticks; odd steps charge idle.
        // SAFETY: single thread owns the block.
        unsafe { cpu::transition(c, (k % 5) as u32, k as u32, 0, 0, 3 * k, k % 2 == 1) };
    }
    // SAFETY: live leaked block, single thread here.
    let (busy, idle) = unsafe { (cpu::counter(c, cpu::CTR_BUSY_TICKS), cpu::counter(c, cpu::CTR_IDLE_TICKS)) };
    // Steps 2..=STEPS each charge 3 ticks; step 1's interval is dropped (no stamp
    // existed before it). Odd k charges idle: of 2..=10000 the odd steps are
    // 3,5,..,9999 = 4999 of them, the even 2,4,..,10000 = 5000 - not "half each",
    // which is what this oracle's first version said and the counters refuted.
    let want_idle = 3 * (STEPS / 2 - 1);
    let want_busy = 3 * (STEPS / 2);
    if busy != want_busy || idle != want_idle {
        return Err(format!(
            "busy {busy} idle {idle}, want {want_busy}/{want_idle} - an interval was \
             lost, double-charged, or the first one was not dropped"
        ));
    }
    // `set_aux` touches only what it was given: the deadline lands, and the group
    // still reads exactly as the last `transition` left it (this writer's tuples
    // are not the race writer's, so the check is against the known final values,
    // not `coherent`).
    // SAFETY: as above.
    unsafe { cpu::set_aux(c, Some(777), None) };
    // SAFETY: live leaked block, single thread here.
    let s = unsafe { cpu::read(c as *const ObsCpu) }.ok_or("group unreadable single-threaded")?;
    if s.timer_deadline_ns != 777
        || s.cell != STEPS as u32
        || s.since_tick != 3 * STEPS
        || s.state != (STEPS % 5) as u32
        || s.net_tier != 0
    {
        return Err(format!(
            "set_aux broke the group: deadline {} cell {} since {} state {} tier {}",
            s.timer_deadline_ns, s.cell, s.since_tick, s.state, s.net_tier
        ));
    }
    Ok(format!(" (busy {want_busy} / idle {want_idle} over {STEPS} steps, exact)"))
}

/// A writer thread hammers `transition` while this thread reads: every successful
/// read must be coherent. This is the only place in the tree the retry loop is
/// reachable - the boot tests' reader always runs on the writer's own CPU.
fn seqlock_race() -> Result<String, String> {
    if std::thread::available_parallelism().map_or(1, |n| n.get()) < 2 {
        return Ok(" (skipped: single-CPU host, no real race to produce)".into());
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    let c_addr = Box::leak(Box::new(ObsCpu::new())) as *mut ObsCpu as usize;
    let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));

    const WRITES: u64 = 3_000_000;
    let writer = std::thread::spawn(move || {
        let c = c_addr as *mut ObsCpu;
        for k in 1..=WRITES {
            // SAFETY: the one writer, as the kernel's owning CPU is.
            unsafe {
                cpu::transition(
                    c,
                    (k % 5) as u32,
                    k as u32,
                    (k as u32).wrapping_add(1),
                    (k as u32).wrapping_add(2),
                    k,
                    k % 2 == 0,
                )
            };
        }
        stop.store(true, Ordering::Release);
    });

    let c = c_addr as *const ObsCpu;
    let (mut reads, mut gaveup, mut torn) = (0u64, 0u64, 0u64);
    while !stop.load(Ordering::Acquire) {
        // SAFETY: live leaked block; racing the writer is the point.
        match unsafe { cpu::read(c) } {
            Some(s) => {
                reads += 1;
                if !coherent(&s) {
                    torn += 1;
                    if torn == 1 {
                        eprintln!(
                            "  torn: state {} cell {} entity {} vcore {} since {}",
                            s.state, s.cell, s.entity, s.vcore, s.since_tick
                        );
                    }
                }
            }
            // A writer held the bracket across every retry - legal under this
            // hammering, counted so a protocol that never lets a read through
            // would still be visible.
            None => gaveup += 1,
        }
    }
    writer.join().map_err(|_| "writer panicked")?;
    if torn != 0 {
        return Err(format!("{torn} torn group(s) in {reads} reads"));
    }
    if reads == 0 {
        return Err(format!("no read ever succeeded ({gaveup} gave up)"));
    }
    // The post-race accounting is still exact: single writer, so busy + idle is
    // every interval after the first, whatever the reader was doing.
    // SAFETY: live leaked block; the writer has joined.
    let total =
        unsafe { cpu::counter(c, cpu::CTR_BUSY_TICKS) + cpu::counter(c, cpu::CTR_IDLE_TICKS) };
    if total != WRITES - 1 {
        return Err(format!(
            "busy+idle {total}, want {} - the race corrupted the counters",
            WRITES - 1
        ));
    }
    Ok(format!(" ({reads} coherent reads beside {WRITES} writes, {gaveup} retries exhausted)"))
}

/// The negative control: the same field writes with the bracket deleted must be
/// *caught* by the same reader - otherwise the race test above proves only that
/// nobody looked. Firing requires a real interleaving, so a bounded budget and an
/// honest skip on a single-CPU host.
fn seqlock_torn_control() -> Result<String, String> {
    if std::thread::available_parallelism().map_or(1, |n| n.get()) < 2 {
        return Ok(" (skipped: single-CPU host, no real race to produce)".into());
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    let c_addr = Box::leak(Box::new(ObsCpu::new())) as *mut ObsCpu as usize;
    let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));

    let writer = std::thread::spawn(move || {
        let c = c_addr as *mut ObsCpu;
        let mut k = 0u64;
        while !stop.load(Ordering::Acquire) {
            k += 1;
            // The bracket deleted: the same stores `transition` makes, raw.
            // SAFETY: same single-writer contract; the *reader* is what races.
            unsafe {
                core::ptr::write_volatile(&raw mut (*c).state, (k % 5) as u32);
                core::ptr::write_volatile(&raw mut (*c).cur_cell, k as u32);
                core::ptr::write_volatile(&raw mut (*c).cur_entity, (k as u32).wrapping_add(1));
                core::ptr::write_volatile(&raw mut (*c).cur_vcore, (k as u32).wrapping_add(2));
                core::ptr::write_volatile(&raw mut (*c).since_tick, k);
            }
        }
    });

    let c = c_addr as *const ObsCpu;
    const BUDGET: u64 = 50_000_000;
    let mut torn_at = None;
    for i in 0..BUDGET {
        // SAFETY: live leaked block; racing the broken writer is the point.
        if let Some(s) = unsafe { cpu::read(c) }
            && s.since_tick != 0
            && !coherent(&s)
        {
            torn_at = Some(i);
            break;
        }
    }
    stop.store(true, Ordering::Release);
    writer.join().map_err(|_| "writer panicked")?;
    match torn_at {
        Some(i) => Ok(format!(" (torn group observed after {i} reads)")),
        None => Err(format!(
            "an unbracketed writer was never caught in {BUDGET} reads - the reader's \
             invariant detects nothing, so the race test's green is worth nothing"
        )),
    }
}
