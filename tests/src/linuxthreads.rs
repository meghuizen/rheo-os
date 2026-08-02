//! In-QEMU test kernel for Linux-personality milestone L4 (docs/LINUX-COMPAT.md
//! L4): an **unpatched multi-threaded Rust `std` binary** runs as a
//! `Personality::Linux` cell with multiple execution contexts scheduled
//! cooperatively at syscall boundaries.
//!
//! The fixture (`tests/linux-fixtures/rustthreads`) spawns four `std::thread`s
//! that share an `Arc<AtomicUsize>`, a `Mutex<u64>`, and an `mpsc` channel,
//! then joins them - exercising clone + futex + per-thread TLS + the CHILD_-
//! CLEARTID join handshake end to end. Its output is scheduling-independent, so
//! the exact stdout and exit code are asserted on all three ISAs.
//!
//! Built static-glibc ET_EXEC by `cargo xtask` (`build_linux_fixtures`); no
//! binary lives in git. `include_bytes!`d below.

#![no_std]
#![no_main]

use kernel::ktimer;
use kernel::linux::{self};
use kernel::user::Outcome;
use kernel::{arch, println};

#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;

/// `include_bytes!` the static-glibc multi-threaded Rust fixture (L4), built by
/// `xtask::build_linux_fixtures` for the ISA's `*-unknown-linux-gnu` target.
static RUSTTHREADS: &[u8] = fixture::linux_cargo!("rustthreads");

/// The **many-context** fixture (docs/SUBSTRATE.md pillar 1): 12 simultaneously
/// live threads, where the pre-migration fixed context array allowed 7 besides
/// main. Built by `xtask::build_linux_fixtures` like the 4-thread one above.
static MANYTHREADS: &[u8] = fixture::linux_cargo!("manythreads");

/// The static-glibc `pthread_cond_timedwait` fixture (built by
/// `xtask::build_linux_fixtures`), for the futex-timeout phase below.
static CONDWAIT: &[u8] = fixture::linux!("condwait");

// -- stdout capture, wired to the Linux personality's stdout tap --
const CAP_MAX: usize = 8 * 1024;
static mut STDOUT_CAP: [u8; CAP_MAX] = [0; CAP_MAX];
static mut STDOUT_LEN: usize = 0;

fn tap(bytes: &[u8]) {
    // SAFETY: single-threaded; the tap is called only during a cell run.
    unsafe {
        for &b in bytes {
            if STDOUT_LEN < CAP_MAX {
                STDOUT_CAP[STDOUT_LEN] = b;
                STDOUT_LEN += 1;
            }
        }
    }
}

fn captured() -> &'static [u8] {
    unsafe { &STDOUT_CAP[..STDOUT_LEN] }
}

fn run(image: &[u8], argv: &[&[u8]]) -> Outcome {
    // SAFETY: single-threaded init; the harness's statics outlive the run.
    unsafe { harness::run_linux_cell(image, argv) }
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("linuxthreads: start on {}", arch::NAME);
    println!(
        "linuxthreads: loaded multi-threaded Rust std ({} bytes)",
        RUSTTHREADS.len()
    );

    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run(RUSTTHREADS, &[b"rustthreads"]);
    linux::set_stdout_tap(None);

    let want_out = b"threads 4 total 1550 channel 1550\n";
    match outcome {
        Outcome::Exited(code) => {
            assert!(code == 4, "rustthreads exited {code}, expected 4");
            let got = captured();
            assert!(
                got == want_out,
                "rustthreads stdout mismatch:\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_out),
            );
        }
        Outcome::Faulted(addr) => panic!("rustthreads faulted at {addr:#x}"),
    }

    println!("linuxthreads: OK (clone + futex + TLS + join)");

    // --- more contexts than the old fixed ceiling (docs/SUBSTRATE.md pillar 1).
    //
    // The context tables are funded now, so a cell's thread count is bounded by its
    // frame budget rather than by an array dimension. This asserts the consequence
    // rather than the mechanism: 12 threads, all live at once (they rendezvous on a
    // barrier before any of them finishes), which the previous `MAX_THREADS = 8`
    // could not have served - the 8th `pthread_create` returned EAGAIN.
    //
    // The sum is hand-computed: sum over id in 1..=12 of triangular(id*10), i.e.
    // sum of (id*10)(id*10+1)/2 = 32890. Order-independent, so stdout is exact.
    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let many = run(MANYTHREADS, &[b"manythreads"]);
    linux::set_stdout_tap(None);

    let want_many = b"contexts 12 total 32890 channel 32890\n";
    match many {
        Outcome::Exited(code) => {
            assert!(
                code == 12,
                "manythreads exited {code}, expected 12 - a context beyond the \
                 old ceiling failed to start"
            );
            let got = captured();
            assert!(
                got == want_many,
                "manythreads stdout mismatch:\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_many),
            );
        }
        Outcome::Faulted(addr) => panic!("manythreads faulted at {addr:#x}"),
    }
    println!("linuxthreads: OK (12 simultaneous contexts - past the old 8-slot array)");

    // --- the futex **timeout** (docs/LINUX-COMPAT.md L4, the `futex` row).
    // `pthread_cond_timedwait` on a condvar nobody signals, in a process with no
    // other thread: the only thing that can end the wait is its own deadline. The
    // timeout used to be ignored, and with nothing else runnable the kernel
    // answered 0 ("you were woken"), so glibc looped forever.
    //
    // The kernel arms the deadline through the timer arbiter's own slot, so the
    // wait is a genuine halt where the timer interrupt is wired - enabled here,
    // opt-in as everywhere else, and reported rather than claimed.
    ktimer::reset();
    arch::enable_timer_irq();
    let parks_before = ktimer::parks();
    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run(CONDWAIT, &[b"condwait"]);
    linux::set_stdout_tap(None);
    let want_cond: &[u8] =
        b"condwait: realtime timedout\ncondwait: monotonic timedout\ncondwait OK\n";
    match outcome {
        Outcome::Exited(code) => {
            let got = captured();
            assert!(
                got == want_cond,
                "condwait stdout mismatch:\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_cond),
            );
            assert!(code == 0, "condwait exited {code}, expected 0");
        }
        Outcome::Faulted(addr) => panic!("condwait faulted at {addr:#x}"),
    }
    // The kernel's futex-timeout mechanism must have been exercised - the wait was
    // honoured against a **real deadline**, one of exactly two correct outcomes,
    // never the ignored-timeout bug (which would hang to the boot timeout). Either
    // the deadline was in the future and the wait parked on the arbiter's futex slot
    // (a registered deadline), OR it was already in the past when the syscall ran and
    // `-ETIMEDOUT` was returned immediately (`immediate_timeouts`).
    //
    // The floor is **1**, not 2, and which of the two counters carries it is not
    // load-invariant (task #162). The *first* wait always reaches the kernel deadline
    // path: modern glibc passes the absolute deadline straight to FUTEX_WAIT_BITSET
    // with no userspace pre-check, and `cur == val` on a never-signalled condvar, so
    // it blocks (register) or its deadline has already elapsed (immediate). The
    // *second* wait may instead time out in glibc's own recheck after a kernel EAGAIN
    // (the condvar's internal word changed), contributing nothing - so requiring 2,
    // or requiring `regs > 0` specifically, was wrongly red under parallel load. What
    // the bug cannot produce, and this does, is at least one deadline-honoured return
    // plus the exact-transcript/exit-0/`deadlock_waits() == 0` evidence below.
    let regs = ktimer::registrations(ktimer::TimerClient::FutexWait);
    let immediate = kernel::linux::thread::immediate_timeouts();
    assert!(
        regs + immediate >= 1,
        "condwait timed out with no deadline honoured by the kernel futex mechanism \
         ({regs} arbiter registrations + {immediate} already-elapsed immediates) - \
         the wait was not honoured the way it claims to be"
    );
    let parks = ktimer::parks() - parks_before;
    println!(
        "linuxthreads: futex timeout OK - two pthread_cond_timedwait waits on a \
         never-signalled condvar returned ETIMEDOUT no earlier than their own \
         deadlines ({regs} arbiter futex deadlines + {immediate} already-elapsed \
         immediates, {parks} genuine CPU halts; an already-elapsed deadline honours \
         the timeout immediately without parking, routine under parallel load)"
    );
    assert!(
        kernel::linux::thread::deadlock_waits() == 0,
        "condwait hit the unsatisfiable-futex path - the timeout was not honoured"
    );

    // --- E3, the Linux half: a context's state lives on its entity
    //
    // `Thread::state` is gone. A Linux context is `Ready` or `Blocked` because its **entity**
    // says so, the same authority the native vcores use (docs/EXECUTION-MODEL.md 9), and what
    // remains on the thread is the *reason* - `pblock` and `fut_addr` - because a wake source's
    // detail belongs to whoever owns the source.
    //
    // The id is **allocated and stored**, not derived, and that was the decision this stage
    // forced: it used to be `cell * MAX_VCORES + vcore`, but a Linux cell's contexts are
    // bounded by its frame budget rather than by that stride, and widening the stride to reach
    // `CONTEXT_CEILING` costs 1 MiB of dense funded metadata - measured and refused
    // (docs/EXECUTION-MODEL.md 9.1). E4's second stage then removed the stride for **native**
    // contexts too, so there is now one allocator and one way to obtain an id, and the
    // reserved band this phase used to check for is gone with it (9.4).
    //
    // This phase runs after 4 threads, 12 threads, a rayon-threaded sort and two condvar
    // timeouts have all created, parked, woken and exited contexts. Three properties:
    //
    //  1. The table's own invariants hold - including I3, an entered entity is owned by the core
    //     inside it. I4 (**parked with no wake source**) is checked too, but honestly: by the
    //     time the phase runs nothing is left parked, so that loop is vacuous *here* and the
    //     invariant's real exercise is `verify/entity`, which drives park/wake directly. What
    //     this kernel does show about the wake source is behavioural - forcing every park to
    //     `NO_WAKE` makes `condwait` fail with "the futex facility returned an unexpected error
    //     code", because an unsatisfiable park is refused rather than hung (observed).
    //  2. Every context that ran holds a **nonzero** id. Zero is the "no context" value, and
    //     it is load-bearing rather than a convention: an id is stored in a funded table, a
    //     funded table grows into zeroed frames, so the value a fresh slot reads must be the
    //     one meaning empty. A collision with a native context is no longer expressible -
    //     there is one allocator.
    //  3. The teardown hands every entity back. This is measured across the teardown rather than
    //     after it, because the harness resets at the *start* of a run, not the end: the last
    //     cell's contexts are still live here, which is exactly what makes the check
    //     non-vacuous - a run with nothing left to release would pass an "all free" assertion
    //     while proving nothing. So the phase counts before, calls the teardown, and counts
    //     again. A slot-handback path that is not a release path is the S1' leak.
    //
    //     The two numbers are computed from **different structures**: how many contexts hold
    //     an entity is counted on the thread side, and how many entities are live is counted
    //     on the table side, so the assertion compares two independent answers rather than
    //     asking one structure about itself. That became necessary rather than merely nicer
    //     when the derived-id band went away: native vcores and Linux contexts allocate from
    //     one table now, so "which of these are Linux" is no longer an arithmetic question
    //     (docs/EXECUTION-MODEL.md 9.4) - and a Linux cell holds two entities, its native
    //     vcore 0 and its context 0, which is why an "all free" count would be wrong here
    //     even with the leak fixed.
    {
        use kernel::sched::entity;

        // Count the live Linux contexts *before* the teardown, and check the invariants while
        // there is still something in the table to check.
        // SAFETY: between runs, with no core inside a cell.
        let t = unsafe { entity::table() };
        assert!(
            t.check().is_none(),
            "the entity table violates an invariant after the thread phases: {:?}",
            t.check()
        );
        /// Live entities, counted on the table side.
        fn live_entities(t: &entity::EntityTable) -> usize {
            (0..t.capacity())
                .filter_map(|id| t.get(id))
                .filter(|e| e.live())
                .count()
        }
        let mut inside = 0usize;
        for id in 0..t.capacity() {
            let Some(e) = t.get(id) else { continue };
            if e.state == entity::State::Free {
                continue;
            }
            assert_ne!(id, 0, "id 0 was allocated - it is the `no context` value");
            if e.inside != u16::MAX {
                inside += 1;
            }
            if e.state == entity::State::Parked {
                assert_ne!(
                    e.wake,
                    entity::NO_WAKE,
                    "entity {id} is parked with no wake source - I4"
                );
            }
        }
        assert_eq!(
            inside, 0,
            "{inside} entities still record a CPU inside them"
        );

        // **One context, one entity.** A Linux cell's context 0 and its native vcore 0 are the
        // same execution context - `thread::init_cell` sets context 0's frame to
        // `user::cell_frame(cell)` - and context 0 used to *allocate* an entity beside the one
        // `user::install` had already made for that vcore. Two names for one thing, which is
        // the defect shape this whole section exists to remove, and it was invisible while
        // native ids were derived and Linux ids allocated above them: they simply lived in
        // different ranges (docs/EXECUTION-MODEL.md 9.4). It cost an entity and an FP frame
        // per Linux cell and it leaked the entity, since the adopted-vs-owned split means
        // `detach_entity` does not release context 0's.
        //
        // Asserted directly, because it is not otherwise load-bearing: the runnability
        // derivation below scans this cell's contexts rather than calling
        // `EntityTable::all_parked`, so restoring the duplicate does not fail any other
        // phase. A cleanup nothing checks is a cleanup that comes back.
        for cell in 0..kernel::user::MAX_CELLS {
            let c0 = kernel::linux::thread::context0_entity(cell);
            if c0 == 0 {
                continue;
            }
            assert_eq!(
                c0,
                kernel::user::entity_of(cell, 0),
                "cell {cell}: Linux context 0 holds entity {c0} but the cell's vcore 0 is \
                 entity {} - two entities naming one execution context",
                kernel::user::entity_of(cell, 0)
            );
        }

        // The thread side's answers, and the table side's, taken before the teardown.
        //
        // **Two numbers on the thread side, because a context 0 is adopted, not owned.** Its
        // entity is the cell's own vcore 0 (`user::install` created it; `user::free_cell`
        // releases it), so the thread teardown clears the field without releasing - one
        // creator, one releaser. Asserting the table dropped by *every* context would demand
        // a release this side must not perform, which is the double free the split exists to
        // prevent (docs/EXECUTION-MODEL.md 9.4).
        let ctx = kernel::linux::thread::live_entities();
        let owned = kernel::linux::thread::owned_entities();
        let before = live_entities(t);
        assert!(
            ctx > 0,
            "no Linux context holds an entity before the teardown - the checks below would \
             be testing nothing"
        );

        kernel::linux::thread::reset();

        // SAFETY: as above.
        let t = unsafe { entity::table() };
        let after = live_entities(t);
        let released = before - after;
        // What the teardown owed the table: exactly its own contexts.
        assert_eq!(
            released, owned,
            "the teardown released {released} entities but owned {owned} - a context that \
             never handed its entity back is the S1' leak, and one that handed back an \
             adopted id would be a double free"
        );
        // And what it owed every context, adopted or owned: a cleared field. This half is
        // never vacuous - a single-context cell still has one to clear - and a stale id is
        // not merely untidy, it names an entity `create` may since have handed to another
        // cell.
        assert_eq!(
            kernel::linux::thread::live_entities(),
            0,
            "a Linux context still holds an entity after the teardown"
        );
        println!(
            "linuxthreads: E3 - A LINUX CONTEXT'S STATE IS ITS ENTITY'S - `Thread::state` is \
             gone; Ready and Blocked are read from the entity and park/wake write it, with the \
             *reason* (pblock, fut_addr) staying on the thread because a source's detail belongs \
             to its owner. After 4 threads, 12 threads, a rayon sort and two condvar timeouts, \
             the table holds every invariant it can check (I4, parked-with-no-source, is checked \
             but vacuous here - nothing is left parked; verify/entity exercises it) \
             and the teardown clears all {ctx} context fields - the half that is never \
             vacuous, since a stale id names an entity `create` may since have given to \
             another cell. Context 0 ADOPTS the cell's vcore-0 entity rather than holding a \
             second name for the same execution context (one creator, one releaser), and the \
             thread-owned count here is {owned} because every worker detached at its own \
             exit - so the release half is stated, not exercised, and lives in \
             verify/entity's fund-once/release-once scenario.\n             Measured measured across it rather than after because the harness resets at \
             the start of a run - and counted on the thread side against the table side, since \
             one allocator means which of these entities are Linux is no longer arithmetic. Ids are ALLOCATED rather than computed, because a stride is a \
             ceiling on contexts per cell and widening it to reach `CONTEXT_CEILING` costs 1 MiB \
             of dense metadata - measured and refused; native contexts allocate from the same \
             one table now, so there is a single way to name a context \
             (docs/EXECUTION-MODEL.md 9.1, 9.4) OK"
        );
    }

    println!("linuxthreads: PASS");
    arch::exit(arch::ExitCode::Success)
}
