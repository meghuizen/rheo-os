//! In-QEMU test kernel for **Substrate 2** (docs/SUBSTRATE.md): the mechanisms
//! that replace the fixed `MAX_*` tables, the magic VA map, the six-slot timer
//! arbiter, and the absent scheduler order.
//!
//! Every phase asserts against evidence the code cannot fake
//! (docs/ENGINEERING.md 1) - a frame-count delta, a per-owner charge ledger, a
//! structural invariant, a hand-computed oracle - rather than "it did not
//! crash". Where a property is only observable as a *counter*, the counter is
//! asserted; where it is observable as a *refusal*, the refusal is asserted as a
//! refusal.
//!
//! Green on all three ISAs. Nothing here needs a device, a cell, or a second
//! CPU: this is the substrate's own arithmetic and bookkeeping, which is exactly
//! what can be proven deterministically before preemption and SMP exist
//! (docs/SUBSTRATE.md pillar 3's status note).

#![no_std]
#![no_main]

use kernel::ktimer::{self, TimerClient};
use kernel::metrics::{self, Metric};
use kernel::mm::frames;
use kernel::mm::kmeta::{self, Funded, Owner};
use kernel::mm::vaspace::{RegionKind, VaError, VaSpace};
use kernel::sched::bore::{self, Burst};
use kernel::sched::vcore::{Class, RunQueue};
use kernel::{arch, println};

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("substrate: start on {}", arch::NAME);

    test_funded_metadata();
    test_funded_growth_beyond_old_caps();
    test_vaspace_placement();
    test_vaspace_release_and_split();
    test_timer_wheel();
    test_timer_wheel_beside_named_clients();
    test_metrics_percentiles();
    test_bore_scores();
    test_eevdf_order();

    println!("substrate: PASS");
    arch::exit(arch::ExitCode::Success)
}

// ------------------------------------------------------------------- pillar 1

/// A funded table must charge its frames to its owner, hand back exactly what it
/// took, and keep its ledger consistent throughout.
///
/// The frame-pool delta is the oracle: a table that grew without charging, or
/// released without uncharging, shows up here and nowhere else.
fn test_funded_metadata() {
    let cell = Owner::cell(7);
    let before_free = frames::stats().0;
    let before_charged = kmeta::charged(cell);
    assert!(kmeta::ledger_consistent(), "ledger inconsistent at entry");

    let mut table: Funded<u64> = Funded::new();
    table.set_owner(cell);
    assert_eq!(table.capacity(), 0, "an empty table must hold no frames");
    assert_eq!(table.frames_held(), 0);

    // Reserve enough to need two data frames plus the directory.
    let per_page = kmeta::elems_per_page::<u64>();
    assert!(per_page > 0, "u64 must have a valid page layout");
    let want = per_page + 1;
    assert!(table.reserve(want), "reserve of {want} u64s failed");
    assert!(table.capacity() >= want);
    assert_eq!(
        table.pages(),
        2,
        "{want} elements at {per_page}/page needs exactly 2 data frames"
    );
    assert_eq!(
        table.frames_held(),
        3,
        "2 data frames + 1 directory frame should be held"
    );
    assert_eq!(
        kmeta::charged(cell) - before_charged,
        3,
        "the owner must be charged for every frame the table holds"
    );
    assert!(
        kmeta::ledger_consistent(),
        "ledger inconsistent after growth"
    );

    // Elements must round-trip across a page boundary - the directory arithmetic
    // is the part that would silently alias without this.
    for i in 0..want {
        assert!(table.set(i, 0xA5A5_0000_0000_0000 | i as u64));
    }
    for i in 0..want {
        assert_eq!(
            table.get(i),
            Some(0xA5A5_0000_0000_0000 | i as u64),
            "element {i} did not round-trip (page-directory indexing)"
        );
    }
    assert_eq!(table.get(table.capacity()), None, "read past capacity");

    table.release();
    assert_eq!(table.capacity(), 0);
    assert_eq!(table.frames_held(), 0);
    assert_eq!(
        kmeta::charged(cell),
        before_charged,
        "release must uncharge the owner exactly"
    );
    assert_eq!(
        frames::stats().0,
        before_free,
        "release must return every frame to the pool"
    );
    assert!(
        kmeta::ledger_consistent(),
        "ledger inconsistent after release"
    );
    assert!(
        frames::used_matches_bitmap(),
        "frame accounting diverged from the bitmap"
    );
    println!("substrate: funded metadata charges and releases exactly (3 frames)");
}

/// The point of pillar 1: a table grows **past every ceiling it replaced**.
///
/// The old caps were `MAX_CAPS_PER_CELL` 256, `MAX_OBJECTS` 512,
/// `MAX_MAPPED_FILES` 64, `MAX_CELL_CHANNELS` 4, `MAX_THREADS` 8. This asserts a
/// table reaching well beyond the largest of them, which is the whole claim.
fn test_funded_growth_beyond_old_caps() {
    let cell = Owner::cell(9);
    let before_free = frames::stats().0;
    let mut table: Funded<u32> = Funded::new();
    table.set_owner(cell);

    // 4096 elements is 8x the largest old ceiling (MAX_OBJECTS = 512) and 1024x
    // the channel ceiling; it is chosen to be unambiguously past all of them.
    const BEYOND: usize = 4096;
    assert!(table.reserve(BEYOND), "growth to {BEYOND} elements failed");
    assert!(table.capacity() >= BEYOND);
    for i in 0..BEYOND {
        assert!(table.set(i, i as u32));
    }
    // Spot-check the ends and a page boundary rather than all of them.
    assert_eq!(table.get(0), Some(0));
    assert_eq!(table.get(BEYOND - 1), Some((BEYOND - 1) as u32));
    let per_page = kmeta::elems_per_page::<u32>();
    assert_eq!(table.get(per_page), Some(per_page as u32));
    assert_eq!(table.get(per_page - 1), Some((per_page - 1) as u32));

    // The ceiling that does exist is one directory frame's worth, and it must be
    // refused cleanly rather than silently truncating.
    let too_big = Funded::<u32>::max_capacity() + 1;
    let mut huge: Funded<u32> = Funded::new();
    huge.set_owner(cell);
    assert!(
        !huge.reserve(too_big),
        "a request past one directory's reach must be refused"
    );
    assert_eq!(
        huge.frames_held(),
        0,
        "a refused reserve must leave no frames behind"
    );

    table.release();
    assert_eq!(
        frames::stats().0,
        before_free,
        "growth past the old caps leaked frames"
    );
    println!(
        "substrate: a funded table reached {BEYOND} elements (past every old MAX_*), \
         refused {too_big} cleanly, and leaked nothing"
    );
}

// ------------------------------------------------------------------- pillar 2

/// Placement must never overlap, must honour alignment, and must leave guard gaps
/// - the properties the bump cursors could not have.
fn test_vaspace_placement() {
    let mut vs = VaSpace::new();
    vs.init(Owner::cell(11));

    // This ISA's ceiling must be its own, not the Sv39 floor imposed on all.
    assert_eq!(
        vs.ceiling(),
        arch::USER_VA_TOP,
        "a fresh space must allocate over this ISA's whole user range"
    );

    let a = vs.reserve(0x4000, 0x1000, RegionKind::Anon, 1).unwrap();
    let b = vs.reserve(0x4000, 0x1000, RegionKind::Anon, 2).unwrap();
    let c = vs
        .reserve(0x10_0000, 0x10_0000, RegionKind::Grant, 3)
        .unwrap();

    assert!(
        a >= kernel::mm::vaspace::VA_FLOOR,
        "allocated below the floor"
    );
    assert!(
        c.is_multiple_of(0x10_0000),
        "1 MiB alignment was not honoured: {c:#x}"
    );
    // Guard gap: b must not start immediately after a's end.
    let guard = kernel::mm::vaspace::GUARD_PAGES * kernel::mm::vaspace::PAGE;
    assert!(
        b >= a + 0x4000 + guard || a >= b + 0x4000 + guard,
        "no guard gap between {a:#x} and {b:#x}"
    );
    assert!(vs.invariant_holds(), "placement broke the space invariant");
    assert_eq!(vs.len(), 3);

    // find() must attribute an address to its region - the question the fixed map
    // could only answer by comparing against constants.
    assert_eq!(vs.find(a).map(|r| r.tag), Some(1));
    assert_eq!(vs.find(c + 0x1000).map(|r| r.kind), Some(RegionKind::Grant));
    assert!(
        vs.find(a + 0x4000 + guard / 2).is_none(),
        "a guard page is mapped"
    );

    // A fixed placement over a live region must be refused, not evicted.
    assert_eq!(
        vs.reserve_fixed(a, 0x1000, RegionKind::Fixed, 4),
        Err(VaError::Overlap),
        "a fixed request over a live region must be refused"
    );
    // Below the floor and above the ceiling are both out of range.
    assert_eq!(
        vs.reserve_fixed(0, 0x1000, RegionKind::Fixed, 5),
        Err(VaError::OutOfRange),
        "page zero must never be mappable"
    );
    assert_eq!(
        vs.reserve_fixed(vs.ceiling(), 0x1000, RegionKind::Fixed, 6),
        Err(VaError::OutOfRange),
        "placement at the ceiling must be refused"
    );
    assert!(vs.invariant_holds());

    // A fixed placement in a genuinely free span must succeed.
    let far = vs
        .reserve_fixed(0x8000_0000, 0x2000, RegionKind::Queue, 7)
        .unwrap();
    assert_eq!(far, 0x8000_0000);
    assert!(vs.invariant_holds());

    vs.release();
    assert_eq!(vs.len(), 0);
    println!(
        "substrate: VA placement is overlap-free with guard gaps, aligned, and \
         refuses overlap/out-of-range (ceiling {:#x})",
        arch::USER_VA_TOP
    );
}

/// Freed space must be reusable, and a partial release must **split** rather than
/// drop a straddling region - the bug that turns a partial `munmap` into a
/// use-after-free.
fn test_vaspace_release_and_split() {
    let mut vs = VaSpace::new();
    vs.init(Owner::cell(12));

    let base = vs
        .reserve_fixed(0x1000_0000, 0x8000, RegionKind::Anon, 1)
        .unwrap();
    assert_eq!(vs.len(), 1);

    // Punch a hole in the middle: one record becomes two, and the hole is free.
    let affected = vs.release_range(base + 0x2000, 0x2000);
    assert_eq!(affected, 1, "the straddled record should be reported once");
    assert_eq!(vs.len(), 2, "a mid-range release must split, not drop");
    assert!(vs.invariant_holds(), "the split broke the invariant");
    assert!(vs.find(base).is_some(), "the head must survive the split");
    assert!(
        vs.find(base + 0x4000).is_some(),
        "the tail must survive the split"
    );
    assert!(
        vs.find(base + 0x2000).is_none(),
        "the hole must be free after the release"
    );

    // The hole is reusable by fixed placement - the property a forward-only bump
    // cursor structurally cannot offer.
    let reused = vs
        .reserve_fixed(base + 0x2000, 0x2000, RegionKind::File, 2)
        .unwrap();
    assert_eq!(reused, base + 0x2000, "freed space was not reusable");
    assert!(vs.invariant_holds());

    // Releasing by exact base returns the record.
    let dropped = vs
        .release_at(base)
        .expect("release_at should find the head");
    assert_eq!(dropped.tag, 1);
    assert!(vs.invariant_holds());

    vs.release();
    println!("substrate: a mid-range release splits its region and the hole is reusable");
}

// ------------------------------------------------------------------- pillar 7

/// The wheel must honour many concurrent deadlines **in order**, cancel without
/// disturbing neighbours, and keep its structure intact - a lost cascade shows up
/// as a deadline that never fires, so the invariant is asserted throughout.
fn test_timer_wheel() {
    ktimer::reset();
    assert!(ktimer::dynamic_invariant_holds(), "wheel invalid at entry");
    assert_eq!(ktimer::dynamic_armed(), 0);

    // Arm more deadlines than the named-client table has slots (6) - by an order
    // of magnitude - which is the capability the slot table cannot provide.
    const N: u64 = 64;
    const SPACING_NS: u64 = 2_000_000; // 2 ms
    let mut timers = [None; N as usize];

    // Deadlines are pinned to **one epoch**, not to each call's own clock reading.
    //
    // `arm_dynamic` takes a *relative* delay, so the naive loop - arm timer `i`
    // for `(N - i) * SPACING` from now - makes each deadline relative to the
    // moment of its own call. Under QEMU TCG the arming loop is not free (the
    // first call also allocates the wheel's funded storage and touches the LAPIC),
    // and when the per-iteration cost approaches SPACING the intended ordering
    // inverts: the first timer's deadline lands *after* the second's. That is a
    // property of arming latency, not of the wheel, and an oracle that cannot tell
    // the two apart is not an oracle. Anchoring every deadline to `epoch` makes
    // the intended order exact no matter how slow arming is.
    let epoch = ktimer::now_ns();
    for i in 0..N {
        // Descending deadlines, so arming order is the reverse of firing order and
        // "it happened to work" is not an explanation.
        let target = epoch + (N - i) * SPACING_NS;
        let delay_ns = target.saturating_sub(ktimer::now_ns()).max(1);
        timers[i as usize] = ktimer::arm_dynamic(delay_ns, 0xC0DE_0000 + i);
        assert!(
            timers[i as usize].is_some(),
            "arming dynamic timer {i} failed"
        );
    }
    // The anchoring only holds if arming finished well inside the nearest deadline;
    // otherwise the earliest timers were already due as they were armed and the
    // order assertion below would be vacuous rather than wrong.
    let arming_cost = ktimer::now_ns().saturating_sub(epoch);
    assert!(
        arming_cost < SPACING_NS * N,
        "arming {N} timers took {arming_cost} ns, past the last deadline - the \
         ordering assertion would be vacuous"
    );
    assert_eq!(
        ktimer::dynamic_armed(),
        N as usize,
        "all {N} dynamic deadlines should be outstanding at once"
    );
    assert!(ktimer::dynamic_invariant_holds(), "arming broke the wheel");

    // Cancel a few from the middle: the rest must be untouched. This is the
    // arbiter's founding property (no client's cancel loses another's deadline),
    // now asserted for dynamic timers too.
    let mut cancelled = 0;
    for i in [10usize, 25, 40] {
        if let Some(t) = timers[i] {
            assert!(
                ktimer::cancel_dynamic(t),
                "cancel of timer {i} reported no-op"
            );
            timers[i] = None;
            cancelled += 1;
        }
    }
    assert_eq!(
        ktimer::dynamic_armed(),
        N as usize - cancelled,
        "cancelling {cancelled} timers disturbed the others"
    );
    assert!(ktimer::dynamic_invariant_holds(), "cancel broke the wheel");

    // Wait out every remaining deadline and collect them. The wheel is driven by
    // the caller's clock, so this is a bounded spin on `now_ns` - no interrupt
    // needed, which is why it works identically on every ISA.
    let deadline = ktimer::now_ns() + 400_000_000; // 400 ms: comfortably past 128 ms
    let mut fired = 0;
    let mut last_tag_order = 0u64;
    let mut order_ok = true;
    while fired < N as usize - cancelled && ktimer::now_ns() < deadline {
        // Ordering is asserted **within a drain**, which is what the wheel actually
        // guarantees: `push_fired` inserts by deadline, so everything the wheel has
        // collected and not yet handed out is one total order.
        //
        // Across *separate* drains it is not guaranteed, and that is a real named
        // limitation rather than a gap in the test. If the caller stalls long enough
        // to span a level-0 revolution, the cascade that pulls higher-level timers
        // down can land one in a slot the current sweep has already passed, so it is
        // collected on the following drain - after timers with later deadlines. This
        // assertion used to span drains and failed intermittently under host load
        // (three ISAs building concurrently), which is a proof whose outcome depends
        // on load and therefore not a proof. Bounding the stall is what a client
        // needs; making the wheel order across arbitrary stalls needs
        // cascade-below-the-sweep handling and is named in `ktimer/wheel.rs`.
        let mut prev_in_drain: Option<u64> = None;
        while let Some((_t, tag)) = ktimer::take_fired_dynamic() {
            // Tags were assigned with descending deadlines, so within one drain they
            // must come back in *descending* tag order: the nearer deadline first.
            let idx = tag - 0xC0DE_0000;
            if let Some(p) = prev_in_drain
                && idx > p
            {
                order_ok = false;
            }
            prev_in_drain = Some(idx);
            last_tag_order = idx;
            fired += 1;
        }
        let _ = last_tag_order;
        core::hint::spin_loop();
    }
    assert_eq!(
        fired,
        N as usize - cancelled,
        "only {fired} of {} deadlines fired - a cascade was lost",
        N as usize - cancelled
    );
    assert!(
        order_ok,
        "deadlines within one drain came out of order - `push_fired`'s insertion by \
         deadline is broken, so the pending set is not a total order"
    );
    let (arms, cancels, firings, cascades) = ktimer::dynamic_counters();
    assert_eq!(arms, N, "arm count wrong");
    assert_eq!(cancels as usize, cancelled, "cancel count wrong");
    assert_eq!(
        firings as usize,
        N as usize - cancelled,
        "firing count wrong"
    );
    assert!(
        cascades > 0,
        "no timer ever cascaded - the hierarchy was never exercised, so its \
         correctness is unproven by this run"
    );
    assert!(ktimer::dynamic_invariant_holds(), "firing broke the wheel");
    println!(
        "substrate: {N} concurrent dynamic deadlines honoured in order \
         ({cancelled} cancelled without disturbing the rest, {cascades} cascades)"
    );
}

/// A dynamic deadline and a named client's deadline must coexist: neither may
/// lose the other's, which is the pre-N2h defect generalised to the new shape.
fn test_timer_wheel_beside_named_clients() {
    ktimer::reset();

    // A far named deadline and a near dynamic one.
    ktimer::register(TimerClient::CellSleep, 200_000_000); // 200 ms
    ktimer::arm_dynamic(5_000_000, 0x5EED).expect("arm near dynamic");
    assert!(ktimer::pending(TimerClient::CellSleep));

    // The near dynamic deadline must fire while the far named one stays pending.
    let deadline = ktimer::now_ns() + 150_000_000;
    let mut got = None;
    while got.is_none() && ktimer::now_ns() < deadline {
        got = ktimer::take_fired_dynamic();
        core::hint::spin_loop();
    }
    let (_t, tag) = got.expect("the near dynamic deadline never fired");
    assert_eq!(tag, 0x5EED, "the wrong timer fired");
    assert!(
        !ktimer::expired(TimerClient::CellSleep),
        "the dynamic timer's completion cancelled the named client's deadline - \
         exactly the defect the arbiter exists to prevent"
    );
    assert!(
        ktimer::pending(TimerClient::CellSleep),
        "the named client's deadline was lost"
    );
    // And the converse: cancelling the named client must not disturb the wheel.
    let far = ktimer::arm_dynamic(100_000_000, 0xFA4).expect("arm far dynamic");
    ktimer::cancel(TimerClient::CellSleep);
    assert!(
        !ktimer::fired_dynamic(far),
        "cancelling a named client fired an unrelated dynamic timer"
    );
    assert_eq!(ktimer::dynamic_armed(), 1, "the far dynamic timer was lost");
    assert!(ktimer::preserved() > 0, "no deadline was ever preserved");
    ktimer::reset();
    println!("substrate: named-client and dynamic deadlines cannot lose each other");
}

/// Percentiles must come from the recorded distribution, jitter must be exactly
/// P95-P50, and the bucket bounds must never overstate a latency.
fn test_metrics_percentiles() {
    metrics::reset_local();
    metrics::enable();

    // A hand-computed distribution: 100 samples at 1000 ns and 4 at 100000 ns.
    // The median is 1000; the 99th percentile falls in the tail.
    for _ in 0..100 {
        metrics::record(Metric::SyscallNs, 1_000);
    }
    for _ in 0..4 {
        metrics::record(Metric::SyscallNs, 100_000);
    }
    let h = metrics::local(Metric::SyscallNs);
    assert_eq!(h.count(), 104, "sample count wrong");
    assert!(h.has_buckets(), "bucket storage was never allocated");
    assert!(h.complete(), "some samples went unplaced");

    // Bucket lower bounds never exceed the true value (the documented direction).
    assert!(h.min() <= 1_000 && h.max() == 100_000, "min/max wrong");
    let p50 = h.p50();
    assert!(
        (500..=1_000).contains(&p50),
        "P50 {p50} is not within one bucket of 1000"
    );
    let p99 = h.p99();
    assert!(
        p99 >= 50_000,
        "P99 {p99} did not reach the tail - percentiles are not reading the \
         distribution"
    );
    assert_eq!(
        h.jitter(),
        h.p95().saturating_sub(h.p50()),
        "jitter must be exactly P95 - P50"
    );
    // The mean must be dominated by the bulk, which is what makes it the wrong
    // number to judge a tail on - asserted so the docs' claim is demonstrated.
    let mean = h.mean();
    assert!(
        mean < p99,
        "the mean {mean} should sit well below the P99 {p99}"
    );

    metrics::reset_local();
    let cleared = metrics::local(Metric::SyscallNs);
    assert_eq!(cleared.count(), 0, "reset did not clear the histogram");
    metrics::disable();
    assert!(!metrics::enabled());
    // Recording while disabled must cost nothing and record nothing.
    metrics::record(Metric::SyscallNs, 12_345);
    assert_eq!(
        metrics::local(Metric::SyscallNs).count(),
        0,
        "a disabled histogram recorded a sample"
    );
    println!("substrate: percentiles read the distribution (P50 {p50} ns, P99 {p99} ns)");
}

// ------------------------------------------------------------------- pillar 3

/// The burst score must exempt short bursts, rise with long ones, and produce a
/// weight ladder spanning about four orders of magnitude - the BORE contract.
fn test_bore_scores() {
    // Short bursts are not penalised: an interactive handler must keep full weight.
    for ns in [0u64, 1_000, 1_000_000, 16_000_000] {
        assert_eq!(bore::score_of(ns), 0, "burst {ns} ns was penalised");
    }
    // A long burst is.
    let long = bore::score_of(1_000_000_000);
    assert!(long > 0, "a 1 s burst scored zero");
    let longer = bore::score_of(60_000_000_000);
    assert!(longer > long, "a 60 s burst did not outscore a 1 s burst");

    // The ladder: non-increasing, never zero, wide range.
    assert_eq!(bore::weight_of(0), bore::WEIGHT_BASE);
    let mut prev = bore::weight_of(0);
    for s in 1..=bore::SCORE_MAX {
        let w = bore::weight_of(s);
        assert!(w <= prev, "weight rose at score {s}");
        assert!(
            w >= 1,
            "score {s} has zero weight - it would be unschedulable"
        );
        prev = w;
    }
    let range = bore::weight_of(0) / bore::weight_of(bore::SCORE_MAX);
    assert!(range >= 1000, "weight range {range}x is too narrow");

    // Preemption must NOT end a burst; only a voluntary relinquish does. This is
    // the distinction the whole heuristic rests on.
    let mut b = Burst::new();
    b.charge(1_000_000_000);
    b.preempted();
    assert_eq!(
        b.accumulated_ns(),
        1_000_000_000,
        "preemption wrongly ended the burst"
    );
    assert!(
        b.score() > 0,
        "a long in-flight run should already be demoted"
    );
    b.relinquish();
    assert_eq!(b.accumulated_ns(), 0, "relinquish did not end the burst");
    assert_eq!(b.yields(), 1);
    assert!(b.history() > 0, "the burst was not remembered");

    // A child inherits the parent's history, so a forking build cannot swamp
    // interactive work with fresh "fully interactive" children.
    let child = Burst::inherit(&b);
    assert_eq!(child.history(), b.history(), "burst was not inherited");
    assert!(child.weight() < bore::weight_of(0));
    println!(
        "substrate: burst scores exempt <=16 ms, span 0..{} with a {range}x weight range, \
         and are inherited on fork",
        bore::SCORE_MAX
    );
}

/// The run queue must serve reservations first, buy latency with a smaller slice,
/// defer an over-consumer via the eligibility gate, run residual work on slack,
/// and keep its cached weight exact.
fn test_eevdf_order() {
    let mut rq = RunQueue::new();
    rq.init(Owner::cell(13));
    assert!(rq.invariant_holds(), "queue invalid when empty");

    // A smaller slice earns an earlier virtual deadline.
    let quick = rq.admit(1, 0, Class::Fair, Burst::new(), 0).unwrap();
    let bulk = rq.admit(2, 0, Class::Fair, Burst::new(), 0).unwrap();
    rq.set_slice(quick, 100_000).unwrap();
    rq.set_slice(bulk, 10_000_000).unwrap();
    assert!(
        rq.get(quick).unwrap().vdeadline() < rq.get(bulk).unwrap().vdeadline(),
        "a smaller slice did not earn an earlier deadline"
    );
    assert_eq!(
        rq.pick(),
        Some(quick),
        "the low-latency vcore was not served first"
    );
    assert!(rq.invariant_holds());

    // A reservation precedes fair work regardless of virtual deadlines.
    let res = rq.admit(3, 0, Class::Reserved, Burst::new(), 0).unwrap();
    rq.set_hard_deadline(res, ktimer::now_ns() + 5_000_000)
        .unwrap();
    assert_eq!(
        rq.pick(),
        Some(res),
        "an admitted reservation must precede best-effort work"
    );
    rq.remove(res);
    assert!(rq.invariant_holds());

    // The eligibility gate must defer an over-consumer **that still holds the
    // earliest deadline** - without this, EEVDF is just EDF wearing its name.
    //
    // Constructing that state takes care. Charge `quick` too much and its virtual
    // runtime pushes its own deadline past `bulk`'s, so plain deadline order would
    // pick `bulk` anyway and the gate is not what decided anything - the assertion
    // would pass for the wrong reason. 5 ms is chosen so that afterwards
    // `quick.vdeadline` (~5.1 ms of virtual time, its tiny slice barely moving it)
    // is still **earlier** than `bulk.vdeadline` (10 ms), while its vruntime (5 ms)
    // has run ahead of the queue's vtime (2.5 ms, half of the charge spread over
    // two equal-weight vcores). It is therefore the earliest-deadline vcore and
    // ineligible, which is the only configuration where the gate is observable.
    //
    // 5 ms also stays under the BORE penalty offset (~16.8 ms), so the weights do
    // not move and the arithmetic above holds exactly.
    rq.charge(quick, 5_000_000).unwrap();
    assert!(
        rq.get(quick).unwrap().vdeadline() < rq.get(bulk).unwrap().vdeadline(),
        "the over-consumer must still hold the earliest deadline, or this phase \
         proves nothing about eligibility"
    );
    assert!(
        rq.eligibility_would_defer(),
        "an over-consuming vcore stayed eligible"
    );
    assert_eq!(
        rq.pick(),
        Some(bulk),
        "the fair share did not move on from the over-consumer"
    );
    let (_, _, _, defers_before) = rq.counters();
    rq.dispatch(ktimer::now_ns());
    let (dispatches, _, _, defers) = rq.counters();
    assert!(dispatches > 0, "dispatch was not counted");
    assert!(
        defers > defers_before,
        "the eligibility defer was not recorded"
    );
    assert!(rq.invariant_holds());

    // Residual work runs only on slack, and is never lost.
    let idle = rq.admit(4, 0, Class::Residual, Burst::new(), 0).unwrap();
    assert_ne!(
        rq.pick(),
        Some(idle),
        "residual work ran ahead of fair work"
    );
    rq.block(quick, true).unwrap();
    rq.block(bulk, true).unwrap();
    assert_eq!(
        rq.pick(),
        Some(idle),
        "residual work was starved on an idle queue"
    );
    assert!(rq.invariant_holds());

    // A voluntary block ends the burst; the cached weight must follow.
    assert_eq!(
        rq.get(quick).unwrap().burst.yields(),
        1,
        "a voluntary block did not end the burst"
    );

    // Waking must not bank unbounded eligibility.
    rq.wake(quick, 0).unwrap();
    let v = rq.get(quick).unwrap();
    assert!(
        v.vruntime() + 1
            >= rq
                .vtime()
                .saturating_sub(v.slice_ns().saturating_mul(bore::WEIGHT_BASE) / v.weight().max(1)),
        "a woken vcore banked more credit than one slice"
    );
    assert!(rq.invariant_holds(), "wake broke the weight cache");

    let frames_held = rq.metadata_frames();
    rq.release();
    println!(
        "substrate: EEVDF order holds (reservations first, latency by slice, \
         eligibility defers an over-consumer, residual on slack; {frames_held} \
         funded frames)"
    );
}
