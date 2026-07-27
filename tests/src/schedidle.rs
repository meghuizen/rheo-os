//! In-QEMU test kernel for the **scheduler idle state** - the keystone slice
//! (docs/ARCHITECTURE-DEBT.md 2.4, docs/CONCURRENCY.md 1, docs/IO.md 1).
//!
//! ## What was wrong
//!
//! `IO.md` 1 and `CONCURRENCY.md` 1 say blocking does not exist below the library
//! level, and `SCHEDULING.md` 3 says scheduler activations are viable here *because*
//! no blocking syscall exists. Three verbs contradicted that: `SYS_ARM_TIMER`,
//! `SYS_WAIT_INPUT` and `SYS_WAIT_NET` each waited **inside the trap**, in kernel
//! context, never consulting the scheduler - so one cell's `sleep` idled the whole
//! machine while its siblings were runnable. And `reschedule` **panicked** when
//! nothing was runnable, so "every cell is waiting for the outside world" - the
//! normal steady state of a server - was not an expressible state.
//!
//! ## The proof
//!
//! Two `.user`-window cells share one page read-write. Each appends its own marker
//! byte to an **ordering vector** in that page, so neither can manufacture the
//! other's - the `netservice` interleave-witness pattern (docs/ENGINEERING.md 1).
//! Cell 0 (`user_blocker`) appends `b`, performs one blocking wait, appends `B`.
//! Cell 1 (`user_peer`) appends `S` and yields, N times, then parks on a deadline
//! far beyond the blocker's so the run ends on the blocker's **wake**, not on the
//! peer's exit.
//!
//! The oracle is hand-computed and exact: `b S S S S S S S S B` for N = 8. Every
//! `S` between `b` and `B` is a round the peer ran **while the blocker was blocked**.
//!
//! **Pre-fix, the same binaries produce `b B` with no `S` at all** - the blocker
//! waits out its whole deadline in the trap, appends `B`, and exits, ending the run
//! before the peer ever gets the CPU. That is the observation this phase is built
//! around, and it was confirmed by running these cells against the old code path
//! (see the module comment in `kernel/src/nproc.rs`).
//!
//! Phases, one per wake source:
//! 1. **timer** - `SYS_ARM_TIMER`; the wake is the arbiter's one-shot, and on an ISA
//!    with a wired timer interrupt the scheduler is asserted to have genuinely
//!    **halted** (`idle::did_idle()`), not spun.
//! 2. **console** - `SYS_WAIT_INPUT`; the wake is a scripted byte delivered through
//!    the real UART RX path, and the byte the blocker received is asserted.
//! 3. **network** - `SYS_WAIT_NET` with a deadline, on a machine with no NIC. The
//!    wait must keep its in-trap path there (parking on a frame that can never
//!    arrive would wedge), so this phase asserts the **refusal to park** rather than
//!    an interleave, and says so: the frame-arrival wake is proven by `netwait`.
//! 4. **the deadlock classifier** - the exact decision the run loop makes:
//!    `nproc::wake_sources()` is `0` once nothing is blocked, and a mask with no
//!    waitable bit is the state that now prints a diagnostic naming each blocked cell
//!    instead of panicking. (`linuxpoll` reaches the terminal branch itself, with a
//!    process whose `poll` nothing can ever satisfy.)
//! 5. **the system-wide admission ledger** (docs/ARCHITECTURE-DEBT.md 2.5) - a second
//!    90% reservation is refused as over-commit, while each *cell's* own controller
//!    would have accepted it. That gap was the defect: sixteen cells at 90% each all
//!    succeeded.

#![no_std]
#![no_main]

#[path = "harness.rs"]
mod harness;

use harness::{CellStore, KernelStack, build_cell};
use kernel::arch::MapPerm;
use kernel::capability::{CapTable, ObjectTable};
use kernel::sched::{self, Admission};
use kernel::user::{self, Outcome};
use kernel::user_progs::{
    BLOCK_CONSOLE, BLOCK_NET, BLOCK_TIMER, ORDER_IO_OFF, user_blocker, user_peer,
};
use kernel::{arch, idle, input, ktimer, net_rx, nproc, println, time};

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

// A peer-only wait is not waitable: that pair *is* the deadlock condition the run
// loop tests, and it is a structural property of the masks, so it is asserted at
// compile time rather than pretended to be a runtime observation.
const _: () = assert!(idle::PEER & idle::WAITABLE == 0);
const _: () = assert!(idle::WAITABLE & idle::TIMER != 0);

/// Rounds the peer cell runs while the blocker is parked.
const ROUNDS: u64 = 8;
/// The blocker's deadline (timer + network phases). 20 ms: long enough that the
/// pre-fix in-trap wait provably covered the peer's whole run, short enough for the
/// 120 s boot budget.
const BLOCK_NS: u64 = 20_000_000;
/// The peer's parking deadline - an order of magnitude beyond the blocker's, so the
/// blocker is always the nearer deadline and therefore the waking one.
const PEER_PARK_NS: u64 = 400_000_000;

/// A scripted console byte for phase 2. One byte is enough: the point is that it
/// arrives while the blocker is parked and the peer has run.
static SCRIPT: &[u8] = b"Z";

/// Reset the shared page and return its kernel VA (== its user VA - it is in the
/// `.user` window, identity mapped).
fn reset_shared() -> u64 {
    // SAFETY: single-threaded kernel, between runs.
    unsafe {
        *core::ptr::addr_of_mut!(SHARED) = Shared([0; 4096]);
        core::ptr::addr_of!(SHARED) as u64
    }
}

/// The order vector written so far.
fn order() -> &'static [u8] {
    // SAFETY: single-threaded; byte 0 is the cursor, bytes 1.. the vector.
    unsafe {
        let p = core::ptr::addr_of!(SHARED) as *const u8;
        let n = p.read() as usize;
        core::slice::from_raw_parts(p.add(1), n)
    }
}

/// The byte the blocker's `SYS_WAIT_INPUT`/`SYS_WAIT_NET` landed in the shared
/// page's scratch area.
fn io_byte(i: usize) -> u8 {
    // SAFETY: single-threaded; `ORDER_IO_OFF + i` is inside the page.
    unsafe {
        (core::ptr::addr_of!(SHARED) as *const u8)
            .add(ORDER_IO_OFF + i)
            .read()
    }
}

/// Build and run the two cells: cell 0 blocks in `mode`, cell 1 runs `ROUNDS`
/// yielding rounds and then parks. Returns `(outcome, blocker_ret, peer_rounds)`.
fn run_pair(mode: u64, arg: u64) -> (Outcome, u64, u64) {
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
            user_blocker,
            mode,
            arg,
        );
        let (mut aspace_b, _ob, mut frame_b) =
            build_cell(&mut *b, objects, caps, kernel_sp, 2, user_peer, 0, ROUNDS);
        // The witness page, read-write in both cells - the only memory they share.
        aspace_a.map_user_range(shared as usize, 4096, MapPerm::UserRw);
        aspace_b.map_user_range(shared as usize, 4096, MapPerm::UserRw);
        // `ticks` carries the witness VA in (both programs read it as an input);
        // `qp_addr` carries the peer's parking deadline (neither uses a queue).
        (*a).params.ticks = shared;
        (*b).params.ticks = shared;
        (*b).params.qp_addr = PEER_PARK_NS;

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
        (outcome, (*a).params.ops, (*b).params.ops)
    }
}

/// Assert the exact interleave oracle: `b` then `ROUNDS` peer markers then `B`.
fn assert_interleave(what: &str) {
    let ord = order();
    assert_eq!(
        ord.first(),
        Some(&b'b'),
        "{what}: order vector does not start with the blocker's pre-wait marker: {ord:?}"
    );
    assert_eq!(
        ord.last(),
        Some(&b'B'),
        "{what}: order vector does not end with the blocker's post-wait marker: {ord:?}"
    );
    let peers = ord.iter().filter(|&&c| c == b'S').count() as u64;
    assert_eq!(
        peers, ROUNDS,
        "{what}: peer ran {peers} of {ROUNDS} rounds while the blocker was blocked ({ord:?})"
    );
    // Every peer round must land strictly between the two blocker markers - which is
    // what "the other cell ran while this one was blocked" means, and exactly what
    // the pre-fix in-trap wait could not produce.
    let first_s = ord.iter().position(|&c| c == b'S').unwrap();
    let last_s = ord.iter().rposition(|&c| c == b'S').unwrap();
    assert!(
        first_s > 0 && last_s < ord.len() - 1,
        "{what}: peer rounds are not inside the blocker's wait ({ord:?})"
    );
    println!(
        "schedidle: {what}: order {} - the peer ran all {ROUNDS} rounds inside the block OK",
        core::str::from_utf8(ord).unwrap_or("?")
    );
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("schedidle: start on {}", arch::NAME);
    // Both interrupt paths this proof can idle on, opt-in exactly as the Phase D/F
    // kernels do (so the other 57 kernels are untouched).
    arch::enable_timer_irq();
    arch::enable_uart_rx_irq();

    // ---- phase 1: a timer block reschedules, and the scheduler idles ----
    input::reset();
    let (outcome, ret, rounds) = run_pair(BLOCK_TIMER, BLOCK_NS);
    assert_eq!(
        outcome,
        Outcome::Exited(0),
        "timer phase: the blocker did not exit cleanly"
    );
    assert_eq!(ret, 0, "timer phase: SYS_ARM_TIMER returned {ret}, want 0");
    assert_eq!(rounds, ROUNDS, "timer phase: peer round counter");
    assert_interleave("timer");
    // The wake source was the arbiter's one-shot, in its own slot.
    assert!(
        ktimer::registrations(ktimer::TimerClient::CellSleep) >= 2,
        "timer phase: expected both cells' sleeps in the CellSleep slot, saw {}",
        ktimer::registrations(ktimer::TimerClient::CellSleep)
    );
    if arch::timer_irq_enabled() {
        assert!(
            idle::did_idle(),
            "timer phase: the scheduler never halted (halts={}, spins={})",
            idle::halts(),
            idle::spins()
        );
        assert!(
            time::timer_did_idle(),
            "timer phase: a sleep that parked in the scheduler must still count as an idle"
        );
        println!(
            "schedidle: timer: scheduler idled genuinely - {} halt(s), {} bounded-poll iteration(s)",
            idle::halts(),
            idle::spins()
        );
    } else {
        println!(
            "schedidle: timer: no hardware one-shot on this build - the deadline was honoured in \
             software ({} bounded-poll iteration(s), NOT an idle)",
            idle::spins()
        );
    }

    // ---- phase 2: a console block reschedules, and a real byte wakes it ----
    input::reset();
    input::install_script(SCRIPT);
    let (outcome, ret, rounds) = run_pair(BLOCK_CONSOLE, 0);
    assert_eq!(
        outcome,
        Outcome::Exited(0),
        "console phase: the blocker did not exit cleanly"
    );
    assert_eq!(
        ret, 1,
        "console phase: SYS_WAIT_INPUT returned {ret} bytes, want 1"
    );
    assert_eq!(io_byte(0), b'Z', "console phase: wrong byte delivered");
    assert_eq!(rounds, ROUNDS, "console phase: peer round counter");
    assert_interleave("console");
    println!(
        "schedidle: console: byte 'Z' delivered to the parked cell ({}), {} halt(s)",
        if input::interrupt_driven() {
            "UART RX interrupt"
        } else {
            "polled UART - honest, not an idle"
        },
        idle::halts()
    );

    // ---- phase 3: a network block with no NIC keeps its in-trap wait ----
    //
    // Honest scope. Parking a cell on a frame that can never arrive would wedge the
    // machine, and the in-trap `wait_frame` is where the backstops live (it answers 0
    // at once with no NIC, and bounds an indefinite wait with `POLL_BUDGET`). So
    // `block_net` deliberately refuses to park in exactly those two cases, and this
    // phase asserts the refusal. The *frame-arrival* wake through the same
    // `Block::Net` path is proven by `netwait`, which has a real NIC.
    assert!(
        !net_rx::nic_present(),
        "network phase: this kernel is booted without a netdev on purpose"
    );
    input::reset();
    let (outcome, ret, rounds) = run_pair(BLOCK_NET, BLOCK_NS);
    assert_eq!(
        outcome,
        Outcome::Exited(0),
        "network phase: the blocker did not exit cleanly"
    );
    assert_eq!(
        ret, 0,
        "network phase: SYS_WAIT_NET with no NIC must report 0 bytes, got {ret}"
    );
    let ord = order();
    assert_eq!(
        &ord[..2],
        b"bB",
        "network phase: with no NIC the wait must not park at all ({ord:?})"
    );
    assert_eq!(
        rounds, 0,
        "network phase: the peer must not have run before the (non-parking) wait returned"
    );
    println!(
        "schedidle: network: SYS_WAIT_NET with no NIC refused to park and answered 0 - the \
         wedge-free path, order {} OK",
        core::str::from_utf8(ord).unwrap_or("?")
    );

    // ---- phase 4: the deadlock classifier ----
    //
    // The run loop's decision is: nothing runnable, and `wake_sources()` has no
    // waitable bit -> print which cell is blocked on what and end the run with
    // `DEADLOCK_EXIT`, instead of `panic!`. The classifier is asserted here directly;
    // reaching the terminal branch needs a block whose source can never fire, which
    // the native verbs now refuse to create (phase 3), so it is code-reviewed and
    // reachable-by-construction rather than exercised - stated plainly.
    assert_eq!(
        nproc::wake_sources(),
        0,
        "phase 4: no cell is blocked after the run, so the classifier must report no source"
    );
    assert_eq!(idle::describe(0), "nothing");
    assert_eq!(idle::describe(idle::TIMER), "timer");
    assert_eq!(idle::describe(idle::NET), "net");
    assert_eq!(idle::describe(idle::CONSOLE), "console");
    assert_eq!(idle::describe(idle::PEER), "peer");
    assert_eq!(idle::describe(idle::TIMER | idle::NET), "several");
    println!("schedidle: deadlock classifier: peer-only waits are not waitable OK");

    // ---- phase 5: the system-wide admission ledger (ARCHITECTURE-DEBT.md 2.5) ----
    //
    // Admission used to be tested only against the *calling cell's* controller, so
    // sixteen cells each admitting 90% all succeeded - 1440% of one CPU admitted,
    // nothing refused. The hand-computed oracle: a budget of 9 in a period of 10 is
    // 900,000 ppm, so the first fits (900,000 <= 1,000,000) and a second cannot
    // (1,800,000 > 1,000,000) - while each cell's *own* controller, being empty,
    // would accept it, which is precisely the over-commit that used to slip through.
    sched::reset_system();
    let mut cell_a = Admission::new();
    let mut cell_b = Admission::new();
    let a_sys = sched::system()
        .admit(9, 10, 10)
        .expect("first 90% must be admitted");
    let a_own = cell_a
        .admit(9, 10, 10)
        .expect("cell A's own controller admits it");
    assert_eq!(a_sys.util_ppm(), 900_000, "90% of a period is 900,000 ppm");
    assert_eq!(sched::system().committed_ppm(), 900_000);
    // Cell B's own controller accepts - it knows nothing of cell A. The machine's
    // does not. That gap *was* the defect.
    assert!(
        cell_b.admit(9, 10, 10).is_ok(),
        "a fresh per-cell controller must still accept 90% - the per-cell check is \
         not what refuses over-commit"
    );
    assert!(
        matches!(
            sched::system().admit(9, 10, 10),
            Err(sched::AdmitError::Overcommit)
        ),
        "the system ledger must refuse a second 90% (it committed {} ppm)",
        sched::system().committed_ppm()
    );
    assert_eq!(
        sched::system().committed_ppm(),
        900_000,
        "a refused admission must leave the ledger unchanged"
    );
    // And a release gives the capacity back, so the ledger does not leak.
    cell_a.release(&a_own);
    sched::system().release(&a_sys);
    assert_eq!(sched::system().committed_ppm(), 0);
    assert!(
        sched::system().admit(9, 10, 10).is_ok(),
        "released capacity must be reusable"
    );
    sched::reset_system();
    println!(
        "schedidle: system admission ledger: 90% admitted, a second 90% REFUSED as \
         over-commit (each cell's own controller would have accepted both), and a \
         release returns the capacity OK"
    );

    println!("schedidle: PASS");
    arch::exit(arch::ExitCode::Success)
}
