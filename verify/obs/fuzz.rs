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

// --------------------------------------------------------------- host shims
//
// `Funded<T>` is a page-directory-backed table charged to a cell's frame budget
// (kernel/src/mm/kmeta.rs). The methods below are every one `ring.rs` calls; a new
// one appearing upstream is a compile error here rather than a silent divergence.
// This is the same shim `verify/entity/fuzz.rs` uses, for the same reason: a host
// process has no frame pool.

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Owner(u16);
impl Owner {
    pub const KERNEL: Owner = Owner(u16::MAX);
}

/// How many elements of `T` a 4 KiB frame holds - the real function's answer, since
/// the ring publishes it and a wrong value would be a wrong published layout.
pub const fn elems_per_page<T>() -> usize {
    let size = std::mem::size_of::<T>();
    if size == 0 || size > 4096 { 0 } else { 4096 / size }
}

/// Whether the shimmed pool will allow growth.
///
/// A property of the *pool*, kept here rather than as a test-only method on the ring:
/// the kernel's `reserve` can refuse because the frame pool is exhausted, and a shim
/// that always succeeds would never exercise the ring's "not funded, count the emit"
/// path. Modelling that as a knob on the code under test would be the fuzzer
/// changing the thing it is checking.
static ALLOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn set_pool_allows(v: bool) {
    ALLOW.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub struct Funded<T: Copy> {
    slots: Vec<T>,
}

impl<T: Copy> Funded<T> {
    pub const fn new() -> Funded<T> {
        Funded { slots: Vec::new() }
    }
    pub fn set_owner(&mut self, _owner: Owner) {}
    pub fn reserve(&mut self, want: usize) -> bool {
        if !ALLOW.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        while self.slots.len() < want {
            // The kernel grows into freshly allocated frames, which arrive zeroed -
            // `mm::kmeta`'s stated contract on `T`. Reproducing it keeps the shim
            // faithful to what the code under test sees, and it is what makes the
            // "a zero sequence number means never written" rule testable.
            self.slots.push(unsafe { std::mem::zeroed() });
        }
        true
    }
    pub fn release(&mut self) {
        self.slots.clear();
    }
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }
    pub fn pages(&self) -> usize {
        self.slots.len().div_ceil(elems_per_page::<T>().max(1))
    }
    pub fn dir_va(&self) -> usize {
        // A plausible non-zero kernel VA; the ring only publishes it.
        if self.slots.is_empty() {
            0
        } else {
            0xffff_0000_0000_1000
        }
    }
    pub fn get(&self, index: usize) -> Option<T> {
        self.slots.get(index).copied()
    }
    pub fn set(&mut self, index: usize, value: T) -> bool {
        match self.slots.get_mut(index) {
            Some(s) => {
                *s = value;
                true
            }
            None => false,
        }
    }
}

mod mm {
    pub mod kmeta {
        pub use crate::{Funded, Owner, elems_per_page};
    }
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

use abi::obs::ObsEvent;
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
    if !r.fund(3, |va| va & 0x0000_ffff_ffff_ffff) {
        return Err("fund refused with an allowing shim".into());
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
    let mut r = ObsRing::new();
    set_pool_allows(false);
    let funded = r.fund(1, |va| va);
    set_pool_allows(true);
    if funded {
        return Err("fund succeeded against a refusing pool".into());
    }
    if r.funded() {
        return Err("a ring the pool refused reports itself funded".into());
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
    r.fund(0, |va| va);
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
    if !r.fund(0, |va| va) {
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
    r.fund(0, |va| va);
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

    if failures == 0 {
        println!("obs fuzz: PASS");
    } else {
        println!("obs fuzz: FAIL ({failures} failure(s))");
        std::process::exit(1);
    }
}
