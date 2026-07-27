//! In-QEMU test kernel for rheo-net Phase N2b (docs/NETSTACK.md §11 congestion
//! control): the two from-scratch congestion controllers, **Reno** and **CUBIC**
//! (`net::cc`), over the N2a `CongestionControl` seam. A cell loaded from the
//! `nettcpcc-demo` ELF drives **deterministic integer cwnd trajectories** - slow
//! start, AIMD, fast retransmit / fast recovery, RTO collapse, and the CUBIC `W(t)`
//! shape - each pinned against a precomputed oracle, plus a real `Connection<Reno>`
//! fast-retransmit-before-RTO scenario over the in-cell virtual link. It exits
//! `0x42` only if every trajectory matches, so the exit code is the proof.
//!
//! Like `nettcp`, this needs **no netdev**: the proof is entirely in-cell (the CC
//! math + the loopback `VirtualLink`), so a live peer is not required (a live TCP
//! handshake to SLIRP is skipped-with-reason - SLIRP has no TCP responder). The
//! kernel is untouched: `net::cc` + `net::tcp` are portable userspace over the
//! existing reactor ABI. A minimal console `FileOps` backs the cell's `println!`.

#![no_std]
#![no_main]

extern crate alloc;

use core::ptr::addr_of_mut;

use kernel::svc::{self};
use kernel::user::Outcome;
use kernel::{arch, println};

#[path = "console_personality.rs"]
mod console_personality;
#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;

static DEMO: &[u8] = fixture::cell!("nettcpcc-demo");

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

/// **The pacer on the timer arbiter, under continuous re-arm** (docs/NETSTACK.md 21,
/// rheo-net N2e). Kernel-side, deterministic, all three ISAs, no NIC traffic.
///
/// N2h built the arbiter because the hardware has exactly one one-shot deadline and
/// two subsystems were arming it directly, each cancelling the other's deadline on
/// its way out. It reserved a slot for the BBR pacer as "the requester that will make
/// this fatal rather than latent", because a paced flow re-arms a deadline after
/// **every segment**, for the life of the flow.
///
/// This is that client: 40 back-to-back pacer deadlines while a cell sleep and a
/// network (RTO) deadline stay outstanding the whole time. Every one of the 40
/// completions must leave both of the others armed - and then they must still fire at
/// their own times, in order.
fn pacer_arbiter_phase() {
    use kernel::ktimer::{self, TimerClient};

    ktimer::reset();
    // 40 nominally-200-us releases are ~8 ms of *guest* time, so both long
    // deadlines are sized an order of magnitude above that. This is not slack for
    // its own sake: the boot tests run without `-icount`, so the guest's clock is
    // host wall-clock, and a loaded host can deschedule QEMU for milliseconds at a
    // time. At the original 20 ms the number of releases that landed inside the RTO
    // was decided by host load - observed as few as 10 of 40 in a full matrix run,
    // tripping the `checked >= PACES / 2` guard below while every per-release
    // assertion still held. The property being proven is unchanged and the
    // assertions are untouched; only the window is now wide enough that emulator
    // timing noise cannot decide the outcome (docs/ENGINEERING.md 10).
    let d_net: u64 = 200_000_000; // 200 ms: a TCP RTO
    let d_sleep: u64 = 400_000_000; // 400 ms: the cell's own sleep
    let pace_ns: u64 = 200_000; // 200 us: one paced segment
    const PACES: u64 = 40;

    let t0 = ktimer::now_ns();
    ktimer::register(TimerClient::NetTimer, d_net);
    ktimer::register(TimerClient::CellSleep, d_sleep);

    // The two long deadlines must survive every pacer release - but "survive" means
    // "is not marked due before its own time", which is the arbiter's guarantee. It
    // is not "is still pending after 40 halts", because under emulation those halts
    // can overshoot: QEMU is an ordinary host process and a loaded host can
    // deschedule it for milliseconds, so 40 nominally-200-us releases may genuinely
    // run past the RTO, at which point marking it due is *correct*. Checking
    // against each client's own deadline asserts the property instead of a timing
    // coincidence (docs/ENGINEERING.md 4, 10); a *lost* deadline never becomes due at
    // any elapsed time, and `preserved()` below counts the survivals directly.
    let mut checked = 0u64;
    for i in 0..PACES {
        ktimer::register(TimerClient::Pacer, pace_ns);
        while !ktimer::expired(TimerClient::Pacer) {
            if !ktimer::park(false) {
                arch::spin_loop(1);
            }
        }
        let elapsed = ktimer::now_ns().wrapping_sub(t0);
        if elapsed < d_net {
            assert!(
                ktimer::pending(TimerClient::NetTimer) && !ktimer::expired(TimerClient::NetTimer),
                "pacer release {i} lost the network deadline"
            );
            checked += 1;
        }
        if elapsed < d_sleep {
            assert!(
                ktimer::pending(TimerClient::CellSleep) && !ktimer::expired(TimerClient::CellSleep),
                "pacer release {i} lost the cell-sleep deadline"
            );
        }
    }
    // The point of the phase is that a *continuously re-armed* client cannot destroy
    // another's deadline, so the run is only meaningful if many releases happened
    // while the RTO was genuinely outstanding.
    assert!(
        checked >= PACES / 2,
        "only {checked} of {PACES} pacer releases landed inside the RTO - the \
         continuous-re-arm property was not exercised"
    );
    let paced_span = ktimer::now_ns().wrapping_sub(t0);
    assert!(
        paced_span >= PACES * pace_ns,
        "40 x 200 us of pacing took only {paced_span} ns - the deadlines did not hold"
    );
    assert!(
        ktimer::registrations(TimerClient::Pacer) == PACES,
        "expected {PACES} pacer registrations, got {}",
        ktimer::registrations(TimerClient::Pacer)
    );
    assert!(
        ktimer::preserved() >= PACES,
        "the arbiter never preserved another client's deadline across a pacer release"
    );
    ktimer::cancel(TimerClient::Pacer);

    // And the two deadlines still fire at their own times, in order.
    while !ktimer::expired(TimerClient::NetTimer) {
        if !ktimer::park(false) {
            arch::spin_loop(1);
        }
    }
    let at_net = ktimer::now_ns().wrapping_sub(t0);
    assert!(
        at_net >= d_net,
        "network deadline fired early ({at_net} ns)"
    );
    assert!(
        ktimer::pending(TimerClient::CellSleep),
        "the network deadline's completion cancelled the cell sleep"
    );
    ktimer::cancel(TimerClient::NetTimer);
    while !ktimer::expired(TimerClient::CellSleep) {
        if !ktimer::park(false) {
            arch::spin_loop(1);
        }
    }
    let at_sleep = ktimer::now_ns().wrapping_sub(t0);
    assert!(
        at_sleep >= d_sleep && at_sleep > at_net,
        "cell sleep fired early or out of order ({at_sleep} ns)"
    );
    ktimer::cancel(TimerClient::CellSleep);
    assert!(
        ktimer::nearest_ns().is_none(),
        "a deadline is still armed after every client was released"
    );
    println!(
        "nettcpcc: pacer slot re-armed {} times over {} us with a 200 ms RTO and a 400 ms \
         sleep outstanding throughout - none lost ({} preserved, {} arms, {} halts); \
         the RTO then fired at {} us and the sleep at {} us",
        PACES,
        paced_span / 1_000,
        ktimer::preserved(),
        ktimer::arms(),
        ktimer::parks(),
        at_net / 1_000,
        at_sleep / 1_000
    );
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("nettcpcc: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    // The pacer's deadlines are real: bring up the per-ISA timer interrupt so
    // `SYS_ARM_TIMER` parks at wfi/hlt through the arbiter rather than falling back
    // to the cooperative deadline check (docs/LIBRHEO.md Phase F, NETSTACK.md 21).
    arch::enable_timer_irq();

    // Kernel-side: the pacer slot under continuous re-arm, beside two other
    // outstanding deadlines (rheo-net N2e over the N2h arbiter).
    pacer_arbiter_phase();
    let pacer_regs_before = kernel::ktimer::registrations(kernel::ktimer::TimerClient::Pacer);

    svc::init();
    svc::set_file_ops(console_personality::console_and_empty_fs());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(DEMO, "nettcpcc-demo") };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "nettcpcc-demo exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "nettcpcc: Reno (slow-start/AIMD/fast-retransmit/RTO) + CUBIC W(t) shape \
                 + integration dup-ACK/RTO + BBRv3 (startup/drain/probe-bw/probe-rtt, \
                 the two filters, loss != congestion) + the pacer, exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("nettcpcc-demo faulted at {addr:#x}"),
    }

    // The cell's pacer went through the arbiter's **pacer** slot, once per paced
    // release (14 of its 16 releases; the first two fit the burst allowance). This is
    // the kernel's own count, not the cell's.
    let pacer_regs = kernel::ktimer::registrations(kernel::ktimer::TimerClient::Pacer);
    let from_cell = pacer_regs - pacer_regs_before;
    assert!(
        from_cell >= 14,
        "the cell's pacer registered {from_cell} deadlines in the arbiter's pacer slot, \
         expected at least 14"
    );
    if arch::timer_irq_enabled() {
        assert!(
            kernel::time::timer_did_idle(),
            "the timer interrupt is wired but no pacing deadline ever halted the CPU"
        );
        println!(
            "nettcpcc: the cell's pacer registered {from_cell} deadlines in the arbiter's \
             pacer slot, each a genuine wfi/hlt idle-park ({} halts total)",
            kernel::ktimer::parks()
        );
    } else {
        println!(
            "nettcpcc: the cell's pacer registered {from_cell} deadlines in the arbiter's \
             pacer slot; no verified hardware one-shot on this kernel, so each was an \
             honest cooperative deadline check rather than an idle park"
        );
    }

    println!("nettcpcc: PASS");
    arch::exit(arch::ExitCode::Success)
}
