//! In-QEMU test kernel for rheo-net **N2d** (docs/NETSTACK.md, the async-receive
//! path): **true async receive** - a NIC RX interrupt plus a park-until-frame
//! primitive, so a cell waiting for a packet costs no CPU instead of re-polling.
//!
//! The cell (`librheo-netwait`) drains the receive queue, spawns a witness strand,
//! sends a broadcast ARP request for the SLIRP gateway `10.0.2.2`, then parks in
//! `net::recv`. SLIRP's ARP reply is the wake event - a genuine received frame, on
//! a real virtio-net device, network-free and deterministic (the same proof shape
//! as `librheonet`, now taken through the blocking path).
//!
//! What this kernel asserts:
//!
//! - **the cell's own checks**, via its exit code `0x42`: the frame really is the
//!   gateway's ARP reply; the witness strand ran while the receiver was parked (so
//!   the receive suspended rather than holding the vcore); and the reactor recorded
//!   **exactly one wakeup per parked receive** - one park + one wake, never N
//!   re-polls (the no-spin property);
//! - **a genuine NIC interrupt** (`net_rx::irq_count() > 0`) on the ISAs where the
//!   RX interrupt is wired - it cannot be faked, the count is only incremented from
//!   the ISA's interrupt vector;
//! - **the idle-park** (`net_rx::did_idle()`) when the wait actually had to halt -
//!   the kernel stopped the CPU at WFI and the NIC's interrupt woke it.
//!
//! Per-ISA honesty (docs/NETSTACK.md has the table): RISC-V and ARM64 drive the
//! virtio-mmio device's interrupt line (AIA APLIC->IMSIC / GICv3 SPI). x86-64's NIC
//! is virtio-*pci* driven through the PCI config tunnel with no mapped BAR and no
//! usable IOAPIC line under QEMU TCG, so there the kernel wait falls back to a
//! bounded poll: the cell still parks once, but the CPU spins - reported, never
//! claimed as an idle.
//!
//! It also carries the **N2h** proof, kernel-side and before the cell runs
//! (docs/NETSTACK.md 16, Phase N2h): the **timer arbiter's** no-lost-deadline property
//! (including the pre-N2h conflict reproduced with the raw `arch::timer_*` primitives)
//! and the **adaptive receive-poll escalation** (hot busy-poll, then warm slices, then
//! cold slices), asserted both as a pure function and as observed counters.

#![no_std]
#![no_main]

extern crate alloc;

use core::ptr::addr_of_mut;

use kernel::hw::virtio_net;
use kernel::svc::{self};
use kernel::user::Outcome;
use kernel::{arch, net_rx, println};

#[path = "console_personality.rs"]
mod console_personality;
#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;

static DEMO: &[u8] = fixture::cell!("librheo-netwait");

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

/// Block until `client`'s arbiter deadline elapses: halt where a verified hardware
/// one-shot can wake the CPU, spin where there is none. The arbiter honours the
/// deadline in software either way, which is what makes this portable.
fn wait_deadline(client: kernel::ktimer::TimerClient) {
    use kernel::ktimer;
    while !ktimer::expired(client) {
        if !ktimer::park(false) {
            arch::spin_loop(1);
        }
    }
}

/// Assert that `client`'s deadline is still outstanding - **the arbiter's actual
/// guarantee**, which is that a deadline is never marked due before its own time,
/// not that a wake is prompt.
///
/// The distinction matters under emulation (docs/ENGINEERING.md 4, 10). QEMU is an
/// ordinary host process, so the host can deschedule it for milliseconds: a genuine
/// halt waiting on the 1 ms deadline may not be re-entered until after the 5 ms one
/// is *legitimately* due, at which point `service()` is right to mark it and a bare
/// `pending()` assertion would fail on a property the arbiter never promised. So the
/// check applies only while `elapsed < deadline`, and the overshoot is reported
/// rather than asserted away. Every no-loss assertion in this phase - the ones the
/// N2h defect actually broke - is unaffected: a *lost* deadline never becomes due at
/// all, at any elapsed time, which parts (A) and (C) and `preserved()` pin down.
fn still_pending(client: kernel::ktimer::TimerClient, elapsed: u64, deadline: u64, what: &str) {
    use kernel::ktimer;
    if elapsed < deadline {
        assert!(
            !ktimer::expired(client) && ktimer::pending(client),
            "{what}"
        );
    } else {
        println!(
            "netwait: the wake overshot to {} us, past this client's own {} us deadline - \
             \"{what}\" is vacuous this run and is skipped, not weakened",
            elapsed / 1_000,
            deadline / 1_000
        );
    }
}

/// **rheo-net N2h, the conflict regression test.** Kernel-side, deterministic, and
/// run on all three ISAs: it needs no NIC traffic.
///
/// Three parts:
///
/// - **(A)** reproduces the pre-N2h defect with the raw `arch::timer_*` primitives -
///   the pattern `net_rx` and `time::arm_timer` both used. Two requesters, and the
///   inner one's completion destroys the outer one's deadline. (This is the only
///   place in the tree that still arms the hardware directly; every kernel subsystem
///   now goes through the arbiter.)
/// - **(B)** the same two deadlines plus a third through the arbiter: the nearest
///   fires first, the others survive its completion and each fires at its **own**
///   time, in order, none lost.
/// - **(C)** the production shape: a cell's sleep deadline outstanding **across** a
///   full `net_rx` receive wait (which registers and cancels a receive deadline and
///   poll slices of its own). Pre-N2h the receive wait's exit disarmed the sleep.
fn arbiter_conflict_phase() {
    use kernel::ktimer::{self, TimerClient};

    ktimer::reset();

    // ---- (A) the pre-N2h defect: last-armer-wins, and the loser is told it expired.
    // Only a demonstration where bring-up verified a real one-shot; with an inert
    // timer every deadline already reads as expired, which would prove nothing.
    if arch::timer_irq_enabled() {
        let outer_ns: u64 = 20_000_000; // 20 ms - an outer subsystem's deadline
        let inner_ns: u64 = 1_000_000; // 1 ms - another subsystem's, armed on top
        let t0 = ktimer::now_ns();
        arch::timer_arm(outer_ns);
        arch::timer_arm(inner_ns); // the second requester silently takes the timer
        while !arch::timer_expired() {
            arch::timer_park();
        }
        arch::timer_disarm(); // ... and disarms it on its way out
        let elapsed = ktimer::now_ns().wrapping_sub(t0);
        assert!(
            elapsed < outer_ns,
            "the pre-N2h demonstration ran past the outer deadline ({elapsed} ns), so it proves \
             nothing"
        );
        // The outer requester's deadline is gone twice over: nothing is armed to wake
        // a halt, and the shared "did my deadline pass?" predicate now says yes - a
        // false expiry, a fraction of the way into the wait.
        assert!(
            arch::timer_expired(),
            "expected the pre-N2h pattern to report a false expiry for the outer deadline"
        );
        arch::timer_disarm();
        println!(
            "netwait: pre-N2h pattern reproduced - an inner requester's arm+disarm destroyed a \
             {} ms deadline and reported it elapsed after {} us (a lost deadline AND a false \
             expiry)",
            outer_ns / 1_000_000,
            elapsed / 1_000
        );
    } else {
        println!(
            "netwait: no verified hardware one-shot on this ISA - the pre-N2h demonstration is \
             skipped (with an inert timer every deadline reads as already expired, so it would \
             prove nothing); the arbiter's no-loss property below is still asserted, in software"
        );
    }

    // ---- (B) the arbiter: three clients, every deadline honoured, in order.
    ktimer::reset();
    let d_rx: u64 = 1_000_000; // 1 ms  (RxPoll: a poll slice)
    let d_net: u64 = 5_000_000; // 5 ms  (NetTimer: an RTO)
    let d_cell: u64 = 15_000_000; // 15 ms (CellSleep: a cell's sleep)
    let t0 = ktimer::now_ns();
    ktimer::register(TimerClient::RxPoll, d_rx);
    ktimer::register(TimerClient::NetTimer, d_net);
    ktimer::register(TimerClient::CellSleep, d_cell);

    // The nearest fires first, and only it.
    wait_deadline(TimerClient::RxPoll);
    let at_rx = ktimer::now_ns().wrapping_sub(t0);
    assert!(
        at_rx >= d_rx,
        "RxPoll fired early, {at_rx} ns into a {d_rx} ns deadline"
    );
    still_pending(
        TimerClient::NetTimer,
        at_rx,
        d_net,
        "the 5 ms deadline fired with the 1 ms one",
    );
    still_pending(
        TimerClient::CellSleep,
        at_rx,
        d_cell,
        "the 15 ms deadline fired with the 1 ms one",
    );

    // The completion that used to wreck everything: releasing the client that fired.
    ktimer::cancel(TimerClient::RxPoll);
    still_pending(
        TimerClient::NetTimer,
        at_rx,
        d_net,
        "releasing a fired client cancelled the 5 ms deadline (the N2h defect)",
    );
    still_pending(
        TimerClient::CellSleep,
        at_rx,
        d_cell,
        "releasing a fired client cancelled the 15 ms deadline (the N2h defect)",
    );
    assert!(
        ktimer::nearest_ns().is_some(),
        "no deadline armed after a client was released, though two are outstanding"
    );

    wait_deadline(TimerClient::NetTimer);
    let at_net = ktimer::now_ns().wrapping_sub(t0);
    assert!(
        at_net >= d_net,
        "NetTimer fired early, {at_net} ns into a {d_net} ns deadline"
    );
    assert!(at_net > at_rx, "deadlines fired out of order");
    still_pending(
        TimerClient::CellSleep,
        at_net,
        d_cell,
        "the 15 ms deadline was lost by the 5 ms one's completion",
    );
    ktimer::cancel(TimerClient::NetTimer);

    wait_deadline(TimerClient::CellSleep);
    let at_cell = ktimer::now_ns().wrapping_sub(t0);
    assert!(
        at_cell >= d_cell,
        "CellSleep fired early, {at_cell} ns into a {d_cell} ns deadline"
    );
    assert!(at_cell > at_net, "deadlines fired out of order");
    ktimer::cancel(TimerClient::CellSleep);
    assert!(
        ktimer::nearest_ns().is_none(),
        "a deadline is still armed after every client was released"
    );
    // Each of the two completions re-armed a still-pending deadline instead of
    // disarming it: exactly the deadlines part (A) threw away.
    assert!(
        ktimer::preserved() >= 2,
        "the arbiter never had to preserve another client's deadline, so nothing was proven"
    );
    println!(
        "netwait: arbiter honoured 3 concurrent deadlines in order - 1 ms at {} us, 5 ms at {} us, \
         15 ms at {} us; none lost or cancelled by another's completion ({} preserved, {} arms, \
         {} halts)",
        at_rx / 1_000,
        at_net / 1_000,
        at_cell / 1_000,
        ktimer::preserved(),
        ktimer::arms(),
        ktimer::parks()
    );

    // ---- (C) the production shape: a cell sleep outstanding across a receive wait.
    ktimer::reset();
    net_rx::reset();
    let d_sleep: u64 = 30_000_000; // 30 ms
    let d_recv: u64 = 5_000_000; // 5 ms
    let t0 = ktimer::now_ns();
    ktimer::register(TimerClient::CellSleep, d_sleep);
    let mut buf = [0u8; 64];
    // A real receive wait: it registers its own deadline, halts (and on an ISA with
    // no NIC interrupt registers poll slices too), then releases both on the way out.
    let n = net_rx::wait_frame_slice(&mut buf, d_recv);
    assert!(
        n == 0,
        "a frame arrived before anything was transmitted ({n} bytes)"
    );
    let at_recv = ktimer::now_ns().wrapping_sub(t0);
    assert!(
        at_recv >= d_recv,
        "the receive wait returned early, {at_recv} ns into {d_recv} ns"
    );
    assert!(
        ktimer::pending(TimerClient::CellSleep) && !ktimer::expired(TimerClient::CellSleep),
        "the receive wait cancelled the cell's sleep deadline (the N2h defect, production shape)"
    );
    wait_deadline(TimerClient::CellSleep);
    let at_sleep = ktimer::now_ns().wrapping_sub(t0);
    assert!(
        at_sleep >= d_sleep,
        "the cell sleep fired early, {at_sleep} ns into a {d_sleep} ns deadline"
    );
    ktimer::cancel(TimerClient::CellSleep);
    println!(
        "netwait: a 30 ms cell sleep survived a full 5 ms receive wait (returned at {} us) and \
         fired at {} us - the receive wait's exit no longer disarms it",
        at_recv / 1_000,
        at_sleep / 1_000
    );
    ktimer::reset();
}

/// **rheo-net N2h, the adaptive receive-poll policy.** Two halves: the escalation law
/// asserted as a pure function (deterministic, no timing at all), and the observable
/// counters from real waits showing the escalation actually happened - plus the
/// profile contrast (latency-first busy-polls, power-first never does).
fn adaptive_poll_phase() {
    use kernel::net_rx::{RxPolicy, RxProfile, RxTier};

    // ---- the escalation law: hot -> warm -> cold, per profile constants.
    let p = RxPolicy::of(RxProfile::Hft);
    let b = p.spin_polls;
    assert!(
        b > 0 && p.warm_slices > 0,
        "the latency-first profile must busy-poll"
    );
    assert!(p.tier(b, 0, 0) == RxTier::Hot);
    assert!(p.tier(b, b - 1, 0) == RxTier::Hot);
    assert!(p.tier(b, b, 0) == RxTier::Warm);
    assert!(p.tier(b, b, p.warm_slices - 1) == RxTier::Warm);
    assert!(p.tier(b, b, p.warm_slices) == RxTier::Cold);
    assert!(p.slice_ns(0) == p.warm_slice_ns && p.slice_ns(p.warm_slices) == p.cold_slice_ns);
    assert!(
        p.warm_slice_ns < p.cold_slice_ns,
        "cold slices must be the long ones"
    );
    // A wait with no busy-poll budget starts at warm - the escalation degenerates
    // cleanly rather than special-casing.
    assert!(p.tier(0, 0, 0) == RxTier::Warm);
    let e = RxPolicy::of(RxProfile::Embedded);
    assert!(
        e.hot_window_ns == 0 && e.spin_polls == 0 && !e.busy_poll_with_irq,
        "the power-first profile must never busy-poll"
    );
    assert!(e.cold_slice_ns > RxPolicy::of(RxProfile::Edge).cold_slice_ns);
    assert!(RxPolicy::of(RxProfile::Hft).warm_slice_ns < e.warm_slice_ns);

    // ---- observed escalation. The receive queue is provably empty (nothing has been
    // transmitted yet), so this wait must run to its deadline through every tier.
    net_rx::reset();
    net_rx::set_profile(RxProfile::Hft);
    net_rx::note_activity(); // "a frame just went out" - the link is hot
    assert!(net_rx::is_hot(), "note_activity did not make the link hot");
    let mut buf = [0u8; 64];
    // Long enough that the bounded busy-poll budget is provably exhausted inside the
    // deadline, so the escalation out of the hot tier is what is being measured.
    let n = net_rx::wait_frame_slice(&mut buf, 60_000_000); // 60 ms
    assert!(
        n == 0,
        "a frame arrived before anything was transmitted ({n} bytes)"
    );
    let hot_spins = net_rx::spin_polls();
    let hot_slices = net_rx::timer_slices();
    let hot_halts = net_rx::halts();
    assert!(
        hot_spins > 0,
        "a hot wait under the latency-first profile never busy-polled the receive queue"
    );
    assert!(
        hot_spins >= 100,
        "the busy-poll tier checked the queue only {hot_spins} times - a hot arrival would be \
         served at slice granularity, not spin granularity"
    );
    assert!(
        net_rx::escalations() >= 1,
        "the wait never escalated out of the busy-poll tier"
    );
    assert!(
        net_rx::tier() == RxTier::Warm || net_rx::tier() == RxTier::Cold,
        "the wait ended in {:?}, so it never escalated",
        net_rx::tier()
    );
    if arch::timer_irq_enabled() {
        assert!(
            hot_halts > 0,
            "the wait never halted the CPU after the spin tier"
        );
        // With a verified one-shot and no NIC RX interrupt yet, the wait is in
        // `IdleMode::TimerIdle`: the warm and cold tiers are real timer slices, so the
        // full hot -> warm -> cold escalation is observable.
        if !net_rx::interrupt_driven() {
            assert!(
                hot_slices > 0,
                "the timer-backed mode never halted on a poll slice"
            );
            assert!(
                net_rx::escalations() >= 2,
                "the wait escalated {} time(s); expected hot -> warm -> cold",
                net_rx::escalations()
            );
            assert!(
                net_rx::tier() == RxTier::Cold,
                "the wait ended in {:?}, not the cold tier",
                net_rx::tier()
            );
        }
    }
    println!(
        "netwait: adaptive escalation observed ({:?} profile, 60 ms wait on an empty queue): \
         {} spin poll(s) -> {} timer slice(s), {} halt(s), {} escalation(s), ended in {:?}",
        net_rx::profile(),
        hot_spins,
        hot_slices,
        hot_halts,
        net_rx::escalations(),
        net_rx::tier()
    );

    // ---- the contrast: the power-first profile never busy-polls, it just halts.
    net_rx::reset();
    net_rx::set_profile(RxProfile::Embedded);
    net_rx::note_activity(); // even "hot", this profile refuses to spin
    assert!(
        !net_rx::is_hot(),
        "the power-first profile must never report a hot link"
    );
    let n = net_rx::wait_frame_slice(&mut buf, 10_000_000); // 10 ms
    assert!(
        n == 0,
        "a frame arrived before anything was transmitted ({n} bytes)"
    );
    assert!(
        net_rx::spin_polls() == 0,
        "the power-first profile busy-polled {} time(s)",
        net_rx::spin_polls()
    );
    if arch::timer_irq_enabled() {
        assert!(
            net_rx::halts() > 0,
            "the power-first wait neither spun nor halted"
        );
    }
    println!(
        "netwait: {:?} profile did a 10 ms wait with {} spin poll(s) and {} halt(s) - the \
         latency/power trade-off is the profile's, not a magic constant",
        net_rx::profile(),
        net_rx::spin_polls(),
        net_rx::halts()
    );

    // Back to the general-purpose default for the cell's own receives, and clear the
    // counters so the cell-phase assertions below see only the cell's waits.
    net_rx::set_profile(RxProfile::Edge);
    net_rx::reset();
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("netwait: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    // Discover and install the virtio-net NIC.
    let dev = match virtio_net::probe() {
        Some(d) => d,
        None => {
            println!("netwait: no virtio-net device attached - skipping");
            println!("netwait: PASS");
            arch::exit(arch::ExitCode::Success)
        }
    };
    let m = dev.mac();
    let slot = dev.mmio_slot();
    virtio_net::install(dev);

    // Bring up the NIC's RX interrupt (opt-in: only this kernel calls it, so every
    // other kernel boots exactly as before). Where the ISA cannot deliver it, the
    // kernel's wait falls back to a bounded poll - reported, not claimed.
    net_rx::reset();
    // The timer interrupt first: a *bounded* receive arms a deadline so the wait can
    // halt the CPU and still wake (docs/NETSTACK.md). Opt-in, like the NIC IRQ.
    arch::enable_timer_irq();

    // rheo-net N2h, kernel-side and deterministic on every ISA: the timer arbiter's
    // no-lost-deadline property, and the adaptive receive-poll escalation. Both run
    // before the cell so the receive queue is provably empty (nothing has been
    // transmitted yet, so nothing can arrive) - and deliberately **before the NIC RX
    // interrupt is wired**, which is the only way to exercise the timer-slice tiers
    // (`IdleMode::TimerIdle`) on an ISA that has both interrupts: once the NIC line is
    // up, a receive wait rightly prefers the indefinite 0%-CPU park.
    arbiter_conflict_phase();
    adaptive_poll_phase();

    let irq = net_rx::enable_irq();
    println!(
        "netwait: virtio-net MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, mmio slot {:?}, receive wait: {}",
        m[0],
        m[1],
        m[2],
        m[3],
        m[4],
        m[5],
        slot,
        if irq {
            "interrupt-driven (WFI idle)"
        } else {
            "kernel poll (no NIC RX interrupt on this ISA)"
        }
    );

    svc::init();
    svc::set_file_ops(console_personality::console_and_empty_fs());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(DEMO, "librheo-netwait") };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "librheo-netwait exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!("netwait: parked receive woke on a real frame, exit {code:#x} OK");
        }
        Outcome::Faulted(addr) => panic!("librheo-netwait faulted at {addr:#x}"),
    }

    // Kernel-side evidence. On an interrupt-driven ISA the NIC must have raised at
    // least one interrupt that the kernel took (the count is only incremented from
    // the interrupt vector, so it cannot be faked); the WFI idle-park is asserted
    // whenever the wait actually had to halt (the frame had not yet arrived).
    if net_rx::interrupt_driven() {
        assert!(
            net_rx::irq_count() > 0,
            "interrupt-driven ISA but the kernel never took a NIC interrupt"
        );
        println!(
            "netwait: NIC interrupts taken: {} (genuine device interrupt)",
            net_rx::irq_count()
        );
        assert!(
            net_rx::did_idle(),
            "interrupt-driven ISA but the receive wait never halted the CPU"
        );
        println!(
            "netwait: idle-park proven (the kernel halted the CPU inside the receive wait - \
             0% CPU, woken by an interrupt)"
        );
    } else {
        // No NIC RX interrupt here. Whether the wait could still halt depends on the
        // timer: with a verified one-shot it halts on slices between polls
        // (`IdleMode::TimerIdle`, a real halt woken by the timer, never claimed as a
        // NIC interrupt); with neither interrupt it is the honest bounded poll
        // (`IdleMode::Poll`) - the CPU spins, and the deadline is still a deadline.
        println!(
            "netwait: no NIC RX interrupt on this ISA - the receive wait used {:?}{} \
             ({} spin poll(s), {} timer slice(s), {} halt(s))",
            net_rx::idle_mode(),
            if net_rx::did_idle() {
                " (the kernel halted the CPU between polls - a real halt, not a spin)"
            } else {
                " (no halt: either every frame was already queued, or this machine has no \
                 verified timer one-shot to halt on - the CPU spins, reported, never claimed)"
            },
            net_rx::spin_polls(),
            net_rx::timer_slices(),
            net_rx::halts()
        );
    }

    println!("netwait: PASS");
    arch::exit(arch::ExitCode::Success)
}
