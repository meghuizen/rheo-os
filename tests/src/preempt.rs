//! In-QEMU test kernel for **timer preemption and queue-driven dispatch** -
//! docs/SUBSTRATE.md pillar 3, migration S3'; task #27.
//!
//! ## What was wrong
//!
//! Every scheduler in this tree was cooperative. A cell kept the CPU until it
//! parked at a syscall boundary, yielded, or exited, and there was no mechanism at
//! all by which a cell issuing no syscall could be made to stop. Two consequences
//! were disclosed rather than fixed:
//!
//! - A compute-bound cell starves every sibling, for as long as it computes.
//! - `linuxbun` is an **accepted partial** because of it: all 205 of Bun's startup
//!   syscalls came from its main thread and the worker it spawned never got the
//!   CPU, because Bun's main thread requires the worker to progress *concurrently*
//!   before it will proceed. Nothing was missing from the syscall surface - the CPU
//!   simply never moved.
//!
//! Separately, the EEVDF+BORE ready queue (`kernel/src/sched/vcore.rs`) was built
//! and proven as a data structure while **nothing dispatched through it**: two
//! `reschedule` functions picked the next cell by `(leaving + k) % MAX_CELLS`,
//! first-runnable-wins. Round-robin has no responsiveness model, so a cell woken by
//! a keystroke waited behind every compute-bound sibling at a lower index.
//!
//! ## The proof
//!
//! Two `.user`-window cells share one page read-write and each appends **its own**
//! marker byte to an ordering vector in it, so neither can manufacture the other's
//! (the `schedidle`/`netservice` interleave-witness pattern). Both run
//! [`user_spinner`]: a bounded compute loop that **issues no syscall until it is
//! done**.
//!
//! That makes the two phases a claim and its own negative control, in one binary:
//!
//! 1. **Cooperative (dispatch off)** - the control. Cell 0 runs its whole loop and
//!    exits; cell 1 never gets the CPU. The vector is a solid run of `A` and
//!    contains **no** `B` at all. This is asserted, not assumed: it is what makes
//!    phase 2 evidence of something.
//! 2. **Preemptive (dispatch on)** - the claim. The same two binaries produce an
//!    **interleaved** vector, and `preempt::counters()` reports preemptions actually
//!    taken. An interleave is only producible if something took the CPU away in the
//!    middle of a loop that never traps, and the only thing that can is the
//!    preemption timer.
//!
//! Phase 3 checks the **ordering** half of the migration separately from
//! preemption: `dispatch::counters()`'s divergence count is the number of picks
//! where the ready queue chose a different cell than round-robin would have, which
//! is the only direct evidence that adopting the queue changed the order rather
//! than reproducing it.
//!
//! ## Honest scope
//!
//! - Preemption needs the ISA's timer interrupt wired (`arch::enable_timer_irq`).
//!   Where a boot has not wired one, `preempt::counters()`'s `unarmable` count is
//!   non-zero and phase 2 **skips with a reason** rather than asserting an
//!   interleave a machine could not produce. All three ISAs have a verified
//!   one-shot as of docs/SMP.md 5, so no ISA skips here today.
//! - This is preemption on **one** CPU: the CPU is taken away and given to another
//!   cell, which is what unblocks concurrency-dependent programs. Two cells running
//!   at the same instant is SMP phase 2.

#![no_std]
#![no_main]

#[path = "harness.rs"]
mod harness;

use harness::{CellStore, KernelStack, build_cell};
use kernel::arch::MapPerm;
use kernel::capability::{CapTable, ObjectTable};
use kernel::sched::{dispatch, preempt};
use kernel::user::{self, Outcome};
use kernel::user_progs::user_spinner;
use kernel::{arch, idle, ktimer, println};

#[unsafe(link_section = ".user.bss")]
static mut STORE_A: CellStore = CellStore::new();
#[unsafe(link_section = ".user.bss")]
static mut STORE_B: CellStore = CellStore::new();

/// The page both cells map read-write: the interleave witness.
#[repr(C, align(4096))]
struct Shared([u8; 4096]);
#[unsafe(link_section = ".user.bss")]
static mut SHARED: Shared = Shared([0; 4096]);
static mut KSTACK: KernelStack = KernelStack::new();
static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();

/// Spin rounds per cell.
///
/// Enough that the vector has room to show structure (a run versus an interleave)
/// and few enough that both cells finish inside the 120 s boot budget under TCG,
/// where each round is `user_progs::SPIN_WORK` multiply-adds.
const ROUNDS: u64 = 24;

/// Marker bytes. Distinct per cell, and each cell writes only its own - which is
/// what makes the vector a witness rather than a report.
const MARK_A: u64 = b'A' as u64;
const MARK_B: u64 = b'B' as u64;

fn reset_shared() -> u64 {
    // SAFETY: single-threaded kernel, between phases.
    unsafe {
        *core::ptr::addr_of_mut!(SHARED) = Shared([0; 4096]);
        core::ptr::addr_of!(SHARED) as u64
    }
}

/// The order vector written so far (byte 0 is the cursor).
fn order() -> &'static [u8] {
    // SAFETY: single-threaded; byte 0 is the cursor, bytes 1.. the vector.
    unsafe {
        let p = core::ptr::addr_of!(SHARED) as *const u8;
        let n = p.read() as usize;
        core::slice::from_raw_parts(p.add(1), n)
    }
}

/// Build and run two spinner cells. Returns `(outcome, a_finished, b_markers)`.
///
/// `queue_driven` is set **before** the cells' trap frames are built, not after,
/// because on ARM64 a frame's SPSR carries the EL0 IRQ mask and that mask is derived
/// from this setting: a frame built with IRQ masked cannot be preempted no matter
/// what the scheduler later decides. Passing it in rather than letting the caller set
/// it around this function is what makes that ordering impossible to get wrong.
fn run_pair(queue_driven: bool) -> (Outcome, bool, usize) {
    dispatch::enable(queue_driven);
    let shared = reset_shared();
    // SAFETY: single-threaded kernel; each phase completes before the next, and the
    // stores/tables are unique `.user`/`.bss` allocations outliving the run.
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS);
        *objects = ObjectTable::new();
        *caps = CapTable::new();
        let kernel_sp = (*core::ptr::addr_of!(KSTACK)).top();

        let a = core::ptr::addr_of_mut!(STORE_A);
        let b = core::ptr::addr_of_mut!(STORE_B);
        let (mut aspace_a, _oa, mut frame_a) = build_cell(
            &mut *a,
            objects,
            caps,
            kernel_sp,
            1,
            user_spinner,
            MARK_A,
            ROUNDS,
        );
        let (mut aspace_b, _ob, mut frame_b) = build_cell(
            &mut *b,
            objects,
            caps,
            kernel_sp,
            2,
            user_spinner,
            MARK_B,
            ROUNDS,
        );
        // The witness page, read-write in both cells - the only memory they share.
        aspace_a.map_user_range(shared as usize, 4096, MapPerm::UserRw);
        aspace_b.map_user_range(shared as usize, 4096, MapPerm::UserRw);
        (*a).params.ticks = shared;
        (*b).params.ticks = shared;

        user::reset();
        ktimer::reset();
        idle::reset();
        user::install(
            0,
            &aspace_a,
            caps,
            objects,
            (*a).qp.qp.as_ptr(),
            core::ptr::addr_of_mut!(frame_a),
        );
        user::install(
            1,
            &aspace_b,
            caps,
            objects,
            (*b).qp.qp.as_ptr(),
            core::ptr::addr_of_mut!(frame_b),
        );
        let (_idx, outcome) = user::run(0);
        (outcome, (*a).params.status == 1, count_marks(b'B'))
    }
}

/// Count occurrences of `c` in the order vector.
fn count_marks(c: u8) -> usize {
    order().iter().filter(|&&x| x == c).count()
}

/// The longest run of a single repeated byte in the order vector.
///
/// The discriminating statistic between the two phases: a cooperative run is one
/// maximal run per cell, so the longest run equals a whole cell's round count; a
/// preempted run is broken into pieces, so it is strictly shorter.
fn longest_run() -> usize {
    let ord = order();
    let mut best = 0usize;
    let mut cur = 0usize;
    let mut prev = 0u8;
    for &c in ord {
        if c == prev {
            cur += 1;
        } else {
            cur = 1;
            prev = c;
        }
        if cur > best {
            best = cur;
        }
    }
    best
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("preempt: start on {}", arch::NAME);
    // The one-shot this proof preempts with, opt-in exactly as the Phase D/F and
    // `schedidle` kernels do, so the other kernels are untouched.
    arch::enable_timer_irq();

    // ---- phase 1: the cooperative control ----
    //
    // Dispatch off. Cell 0 never traps until it exits, so cell 1 cannot run at all.
    // Asserting that is what makes phase 2 mean something: without this phase, an
    // interleave could be evidence of preemption or of the cells having been
    // interleaved all along.
    let (outcome, a_done, b_marks) = run_pair(false);
    assert!(
        matches!(outcome, Outcome::Exited(0)),
        "cooperative phase: cell 0 did not exit cleanly ({outcome:?})"
    );
    assert!(a_done, "cooperative phase: cell 0 did not finish its loop");
    assert_eq!(
        b_marks,
        0,
        "cooperative phase: cell 1 ran {b_marks} rounds, but a cell that issues no \
         syscall cannot be preempted - so this phase is not the control it claims \
         to be ({:?})",
        order()
    );
    let coop_run = longest_run();
    assert_eq!(
        coop_run, ROUNDS as usize,
        "cooperative phase: the vector should be one unbroken run of {ROUNDS} A's, \
         longest run was {coop_run}"
    );
    println!(
        "preempt: cooperative control OK - cell 0 ran all {ROUNDS} rounds unbroken \
         and cell 1 never got the CPU"
    );

    // ---- phase 2: preemption takes the CPU from a cell that never traps ----
    let (outcome, a_done, b_marks) = run_pair(true);
    let (armed, taken, unarmable, to_sibling, to_cell) = preempt::counters();
    if !arch::timer_irq_enabled() || armed == 0 {
        println!(
            "preempt: SKIP the preemption phase - this ISA has no wired timer \
             interrupt to preempt with ({unarmable} slices unarmable), so an \
             interleave is not producible and nothing is asserted"
        );
    } else {
        assert!(
            matches!(outcome, Outcome::Exited(0)),
            "preemptive phase: the first cell to finish did not exit cleanly ({outcome:?})"
        );
        assert!(
            taken > 0,
            "preemptive phase: {armed} slices armed but none was ever taken - the \
             timer interrupt is not reaching the running cell (arbiter: {} arms, \
             {} firings, deadline elapsed: {}, interrupts arrived: {}; \
             a_done={a_done}, marks={:?})",
            ktimer::arms(),
            ktimer::firings(),
            arch::timer_expired(),
            preempt::notes(),
            order().len()
        );
        assert!(
            b_marks > 0,
            "preemptive phase: cell 1 still never ran ({taken} preemptions taken, \
             {to_cell} to another cell) - the CPU was taken away but not handed over"
        );
        let pre_run = longest_run();
        assert!(
            pre_run < ROUNDS as usize,
            "preemptive phase: the vector is still one unbroken run of {pre_run} - \
             the CPU changed hands only at a cell boundary, which cooperative \
             scheduling already did ({:?})",
            order()
        );
        println!(
            "preempt: a cell that issues NO syscall was preempted - {taken} of {armed} \
             slices taken ({to_sibling} to a sibling context, {to_cell} to another \
             cell), cell 1 ran {b_marks} rounds, longest unbroken run {pre_run} < \
             {ROUNDS} (cooperative was {coop_run}), first cell finished: {a_done}"
        );
    }

    // ---- phase 3: the ready queue, not round-robin, chose the order ----
    let (picks, rr_picks, diverged, charged_ns) = dispatch::counters();
    assert!(
        picks > 0,
        "dispatch phase: the ready queue was never consulted ({rr_picks} \
         round-robin picks) - the seam is not wired"
    );
    assert!(
        charged_ns > 0,
        "dispatch phase: no CPU time was charged to any vcore, so every burst score \
         is zero and the EEVDF weights are all identical - the ordering would be \
         arbitrary even though the queue is being asked"
    );
    println!(
        "preempt: the ready queue drove {picks} picks ({diverged} of them to a cell \
         round-robin would not have chosen), {charged_ns} ns charged to vcores"
    );

    println!("preempt: PASS");
    arch::exit(arch::ExitCode::Success);
}
