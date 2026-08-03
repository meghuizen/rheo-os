//! In-QEMU test kernel for the **observability plane** (docs/OBSERVABILITY.md).
//!
//! The framework's whole promise is that something outside this kernel - a host
//! tool, a hypervisor, a collector cell - can find out what the machine is doing by
//! reading memory. Everything downstream of that rests on one step working: a
//! reader resolves `RHEO_OBS_ROOT` from the kernel ELF, converts its virtual
//! address to a physical one, reads a page, and finds a structure it recognises
//! describing where everything else is.
//!
//! This kernel proves that step from the inside, which is the only side that can
//! check it deterministically today. It asserts the things a reader will assert,
//! against values it computes independently of the publisher:
//!
//!   - the compile-time identity (magic, version, layout hash, header offset) that
//!     a reader checks *before* trusting anything else, so a wrong address produces
//!     "no root" rather than decoded garbage;
//!   - `self_pa`, against this kernel's own `virt_to_phys` of the symbol - the
//!     check that catches a relocated image or a stale section table;
//!   - that every published section carries a **physical** address, tested by
//!     asking whether it lands somewhere physical memory actually is rather than by
//!     recomputing the publisher's arithmetic;
//!   - that the tick is a real, advancing counter with a real frequency, since a
//!     timestamp domain published as zero would make every recorded time a lie.
//!
//! It then drives the **event plane** end to end: the ring funds from the real frame
//! pool, records come back field-for-field with advancing real ticks, the ring wraps
//! and reports which events were lost rather than how many, and reset returns every
//! frame.
//!
//! ## What it deliberately does not claim
//!
//! The symbol's resolvability *from outside* is checked by the host tool that
//! resolves it, which is where that claim belongs - here it would only be this kernel
//! reading its own address, which proves the linker kept the page but not that anyone
//! else can name it.
//!
//! The wrap **arithmetic** is checked to destruction on the host
//! (`verify/obs/fuzz.rs`), where the counter can be started next to a boundary a boot
//! would take four billion events to reach. And the **multi-core** property - that
//! each core writes its own ring behind its own sequence counter, which is the whole
//! reason this plane replaced `kernel::trace`'s single shared buffer - needs two
//! cores and lives in the `smp` kernel. This kernel is single-CPU, so asserting it
//! here would be one core agreeing with itself.

#![no_std]
#![no_main]

use kernel::abi::obs;
use kernel::mm::frames;
use kernel::{arch, println};

#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;

/// The M1 userland hello - the smallest real cell in the tree, reused here so the
/// snapshot plane's context-switch writer is driven by a genuine U-mode entry
/// rather than by calling the writer directly (which would prove the writer can
/// write, not that the kernel writes it).
static HELLO: &[u8] = fixture::cell!("hello");

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init(); // ends in obs::publish()
    println!("observe: start on {}", arch::NAME);

    identity();
    self_address();
    sections();
    tick();
    events();
    window_mask();
    snapshot();

    println!("observe: PASS");
    arch::exit(arch::ExitCode::Success)
}

/// The snapshot plane (docs/OBSERVABILITY.md 11, S3): busy/idle accounting moved
/// by a real halt, the coupled group written by a real cell entry, and everything
/// zero while the writers were off.
///
/// The oracle for idle time is the deadline itself: a 20 ms park must record
/// roughly 20 ms of idle ticks, bounded loosely (half to five times) because the
/// park wakes on the one-shot plus interrupt latency - the bound proves the
/// *attribution* (idle, not busy), not the timer's precision, which is
/// `nettcpcc`'s job. Everything is converted through the root's own published
/// `tick_hz`, so this phase also exercises the conversion contract a reader uses.
fn snapshot() {
    use kernel::ktimer::{self, TimerClient};
    use kernel::obs::cpu::{CTR_BUSY_TICKS, CTR_DISPATCHES, CTR_HALTS, CTR_IDLE_TICKS, CTR_SPINS};
    use kernel::{idle, obs as kobs};

    let me = kernel::smp::cpu_index();

    // ---- while the writers are off, nothing may have been stamped ----
    // The suite so far has parked (the events phase waits on nothing, but boot
    // paths may idle), entered no cell, and the writers were never enabled - so a
    // nonzero here means a writer runs without its gate, which is exactly the
    // cost regression the mask exists to prevent.
    for (slot, name) in [
        (CTR_BUSY_TICKS, "busy"),
        (CTR_IDLE_TICKS, "idle"),
        (CTR_DISPATCHES, "dispatches"),
        (CTR_HALTS, "halts"),
    ] {
        assert_eq!(
            kobs::cpu_counter(me, slot),
            0,
            "{name} accumulated while snapshots were off - a writer is not behind the gate"
        );
    }
    let s0 = kobs::cpu_snap(me).expect("the group must be readable when idle");
    assert_eq!(
        s0.state,
        obs::OBS_CPU_OFFLINE,
        "the group was written while snapshots were off"
    );

    kobs::enable_snapshots();
    assert!(kobs::snapshots_on(), "the snapshot bit did not latch");
    assert!(
        !kobs::enabled(),
        "the snapshot modifier must not turn event recording on"
    );

    // ---- a real park charges idle, not busy ----
    arch::enable_timer_irq();
    // First transition establishes the stamp; its interval is dropped by design.
    ktimer::register(TimerClient::CellSleep, 1_000_000);
    while !ktimer::expired(TimerClient::CellSleep) {
        idle::wait(idle::TIMER);
    }
    let idle_before = kobs::cpu_counter(me, CTR_IDLE_TICKS);
    let busy_before = kobs::cpu_counter(me, CTR_BUSY_TICKS);

    const PARK_NS: u64 = 20_000_000;
    ktimer::register(TimerClient::CellSleep, PARK_NS);
    while !ktimer::expired(TimerClient::CellSleep) {
        idle::wait(idle::TIMER);
    }
    let idle_ticks = kobs::cpu_counter(me, CTR_IDLE_TICKS) - idle_before;
    let halts = kobs::cpu_counter(me, CTR_HALTS);
    let spins = kobs::cpu_counter(me, CTR_SPINS);
    let hz = kernel::obs::root::root().tick_hz;
    assert!(hz > 0, "tick_hz unpublished - idle ticks cannot be judged");
    let expect_ticks = PARK_NS as u128 * hz as u128 / 1_000_000_000;
    println!(
        "observe: 20 ms park -> {idle_ticks} idle tick(s) (expect ~{expect_ticks}), \
         {halts} halt(s), {spins} spin(s)"
    );
    if halts > 0 {
        // A genuine halt: the park's time must be attributed to idle, roughly the
        // deadline's worth. The loose bound is the attribution test, not a timer
        // precision test.
        assert!(
            (idle_ticks as u128) >= expect_ticks / 2,
            "a 20 ms park recorded only {idle_ticks} idle ticks (expect ~{expect_ticks}) - \
             the park's time went to the wrong counter"
        );
        assert!(
            (idle_ticks as u128) <= expect_ticks * 5,
            "a 20 ms park recorded {idle_ticks} idle ticks (expect ~{expect_ticks}) - \
             busy time is being laundered as idle"
        );
    } else {
        // No ISA in this suite lacks a verified timer interrupt since SMP phase 1;
        // a future configuration that does must land here, with the spins counted.
        assert!(
            spins > 0,
            "no halt and no spin - the park did nothing at all"
        );
        assert_eq!(
            idle_ticks, 0,
            "a park that never halted must charge nothing to idle"
        );
        println!("observe: no timer interrupt here - park spun and charged busy (honest)");
    }
    let s1 = kobs::cpu_snap(me).expect("group readable after the park");
    assert_eq!(
        s1.state,
        obs::OBS_CPU_KERNEL,
        "after the park the CPU is in kernel context, and the group must say so"
    );

    // ---- a real cell entry writes the coupled group and counts a dispatch ----
    let dispatches_before = kobs::cpu_counter(me, CTR_DISPATCHES);
    let busy_mark = kobs::cpu_counter(me, CTR_BUSY_TICKS);
    // SAFETY: single-threaded init; the harness installs into slot 0 after reset.
    let outcome = unsafe { harness::run_elf_cell(HELLO, "hello") };
    assert_eq!(
        outcome,
        kernel::user::Outcome::Exited(42),
        "the hello cell must run to its normal exit under the snapshot writers"
    );
    assert!(
        kobs::cpu_counter(me, CTR_DISPATCHES) > dispatches_before,
        "a cell ran but no dispatch was counted - the enter writer did not fire"
    );
    let s2 = kobs::cpu_snap(me).expect("group readable after the run");
    assert_eq!(
        s2.state,
        obs::OBS_CPU_KERNEL,
        "the run is over; a group still claiming a cell would be a live-state lie"
    );
    assert!(
        kobs::cpu_counter(me, CTR_BUSY_TICKS) > busy_mark,
        "running a cell moved no busy time"
    );
    println!(
        "observe: SNAPSHOT PLANE - zero while off; a 20 ms park charged idle not busy; \
         a real cell entry wrote the group and counted a dispatch; the group reads \
         coherent and honest afterwards OK"
    );

    kernel::obs::reset();
    assert!(!kobs::snapshots_on(), "reset must clear the snapshot bit");
    assert_eq!(
        kobs::cpu_counter(me, CTR_BUSY_TICKS),
        0,
        "reset must clear the snapshot counters"
    );
}

/// The event plane: recording, reading back, wrapping, and the counters that say
/// which of those happened.
///
/// The wrap *arithmetic* is checked to destruction on the host
/// (`verify/obs/fuzz.rs`), where the counter can be started next to a boundary. What
/// only a real boot can show is the parts that fuzzer explicitly excludes: that the
/// ring funds from the real frame pool and gives the frames back, that a real
/// `arch::obs_tick` lands in the records in order, and that the whole path from
/// `obs::emit` to a readable record works with the real `PerCpu` indexing rather than
/// a shim.
fn events() {
    use kernel::obs::{self, Kind, Window};

    let before = pool_used();
    assert!(
        !obs::enabled(),
        "something enabled recording before this phase"
    );
    assert_eq!(
        obs::counters(),
        (0, 0),
        "the rings are not empty at the start"
    );

    if !obs::enable() {
        // A pool that refuses the ring is a true statement about this machine, not a
        // gap in the plane - so it is reported and the phase ends, rather than being
        // dressed up as a pass of something that did not run.
        println!("observe: SKIP the event plane - the pool refused the ring");
        return;
    }
    assert!(
        obs::enabled(),
        "enable() reported success but recording is off"
    );
    let funded = pool_used() - before;
    println!("observe: ring funded, {funded} frame(s) charged to the kernel");
    assert!(
        funded > 0,
        "the ring claims to be funded but took no frames"
    );

    // The published mask must say what is actually being recorded. A reader consults
    // it to know whether an empty stream means "nothing happened" or "nothing was
    // asked for", so a zero here while recording would make that unanswerable.
    assert_eq!(
        kernel::obs::root::windows(),
        obs::WINDOW_MASK_ALL,
        "recording is on but the root advertises no windows"
    );

    // --- records go in and come back out, in order, with their own contents -----
    const N: u64 = 300;
    for i in 0..N {
        obs::emit(Window::Kmeta, Kind::Acquire, (i % 7) as u16, i, i ^ 0xa5a5);
    }
    let (written, unfunded) = obs::counters();
    assert_eq!(written, N, "recorded {written} of {N} events");
    assert_eq!(
        unfunded, 0,
        "{unfunded} emit(s) found no ring on a CPU that funded one"
    );
    assert_eq!(
        obs::overwritten(),
        0,
        "{N} events overwrote something in a ring of {}",
        obs::ring::RING_EVENTS
    );

    let cpu = kernel::smp::cpu_index();
    // SAFETY: this CPU's own ring, read after writing it, single-threaded here.
    let r = unsafe { obs::ring_of(cpu) };
    assert_eq!(r.written(), N, "this CPU's ring did not take every event");
    assert_eq!(r.oldest(), 0, "nothing was written yet the oldest is not 0");
    let mut last_tick = 0u64;
    for i in 0..N {
        let e = r.get(i).unwrap_or_else(|| {
            panic!("event {i} of {N} is missing from a ring that never wrapped")
        });
        // Every field, not just a count: a ring that kept the right *number* of the
        // wrong records is exactly what an off-by-one in the slot mask produces, and
        // a count alone cannot see it.
        assert_eq!(e.a, i, "event {i} carries a={}", e.a);
        assert_eq!(e.b, i ^ 0xa5a5, "event {i} carries the wrong b");
        assert_eq!(e.owner, (i % 7) as u16, "event {i} carries the wrong owner");
        assert_eq!(e.window, Window::Kmeta as u8, "event {i} moved window");
        assert_eq!(e.kind, Kind::Acquire as u8, "event {i} changed kind");
        assert!(
            e.tick >= last_tick,
            "event {i} has tick {} after {last_tick} - the counter went backwards",
            e.tick
        );
        last_tick = e.tick;
    }
    // The tick must actually *advance* across 300 records, not merely not-decrease: a
    // constant timestamp would satisfy the ordering above and carry no information.
    let first = r.get(0).expect("event 0").tick;
    assert!(
        last_tick > first,
        "300 events all carry tick {first} - the timestamp is a constant"
    );
    println!(
        "observe: {N} events recorded and read back field-for-field, ticks advancing \
         ({} over the run) OK",
        last_tick - first
    );

    // --- the ring wraps, and loss is located rather than counted ---------------
    let cap = obs::ring::RING_EVENTS as u64;
    for i in N..(cap + N + 17) {
        obs::emit(Window::Frames, Kind::Note, 0, i, 0);
    }
    let total = cap + N + 17;
    assert_eq!(obs::counters().0, total, "the ring stopped counting");
    assert_eq!(
        obs::overwritten(),
        total - cap,
        "overwritten should be everything past the ring's capacity"
    );
    // The oldest surviving event is exactly `total - cap`, and the one before it is
    // gone. That is what "located" means: a reader is told which events it missed
    // rather than how many.
    assert_eq!(
        r.oldest(),
        total - cap,
        "the surviving window moved wrongly"
    );
    assert!(
        r.get(total - cap).is_some(),
        "the oldest surviving event is not readable"
    );
    assert!(
        r.get(total - cap - 1).is_none(),
        "an overwritten event still reads back"
    );
    assert!(
        r.get(total).is_none(),
        "an event that has not been written reads back"
    );
    println!(
        "observe: ring wrapped - {} of {total} events overwritten, survivors are \
         [{}..{total}) and the record before them is gone OK",
        total - cap,
        total - cap
    );

    // --- and the frames come back ---------------------------------------------
    obs::reset();
    assert!(!obs::enabled(), "reset left recording on");
    assert_eq!(obs::counters(), (0, 0), "reset left counters behind");
    assert_eq!(
        kernel::obs::root::windows(),
        0,
        "reset left the root advertising windows that are off"
    );
    assert_eq!(
        pool_used(),
        before,
        "the ring did not give every frame back - a slot-handback path that is not \
         also a release path is the S1' leak"
    );
    println!("observe: reset returned all {funded} frame(s) OK");
}

/// The window mask: a window that is off records **nothing**, and one that is on is
/// unaffected by its neighbours.
///
/// This is the property that makes the framework usable rather than merely present.
/// "Narrate everything" is almost never the useful request - a boot chasing a frame
/// leak wants two windows and would have the six lines that matter buried under
/// thousands of syscall records - so selection has to happen at the *source*, where the
/// events cost nothing to not produce.
///
/// Asserted as an exact count rather than as "fewer records": a mask that leaked one
/// window into another would still produce fewer records than everything.
fn window_mask() {
    use kernel::obs::{self, Kind, Window};

    assert!(!obs::enabled(), "the previous phase left recording on");
    if !obs::enable_windows(Window::Net.bit()) {
        println!("observe: SKIP the window mask - the pool refused the ring");
        return;
    }
    assert_eq!(
        obs::windows(),
        Window::Net.bit(),
        "asked for one window and got {:#x}",
        obs::windows()
    );
    assert!(obs::on(Window::Net), "the window asked for is off");
    assert!(
        !obs::on(Window::Lock),
        "a window that was not asked for is on"
    );
    // `enabled()` must mean "any window", not "all of them": a caller using it as a
    // cheap pre-test has to see true here or it would skip a window that is recording.
    assert!(obs::enabled(), "one window on reads as nothing recording");

    // Offer both; exactly one must land.
    const EACH: u64 = 20;
    for i in 0..EACH {
        obs::emit(Window::Net, Kind::Note, 0, i, 0);
        obs::emit(Window::Lock, Kind::Note, 0, i, 1);
        obs::emit(Window::Gpu, Kind::Note, 0, i, 2);
    }
    let (written, _) = obs::counters();
    assert_eq!(
        written,
        EACH,
        "asked for one of three windows and recorded {written} of {} offered events",
        EACH * 3
    );
    // And the records that landed are the right ones - a count could be reached by
    // recording the wrong window exclusively.
    let cpu = kernel::smp::cpu_index();
    // SAFETY: this CPU's own ring.
    let r = unsafe { obs::ring_of(cpu) };
    for i in 0..EACH {
        let e = r.get(i).expect("a recorded event is missing");
        assert_eq!(
            e.window,
            Window::Net as u8,
            "a record from window {} landed while only Net was enabled",
            e.window
        );
        assert_eq!(e.b, 0, "the record is not the one Net emitted");
    }

    // Turning a second window on takes effect without re-funding, and the first keeps
    // recording - so the mask is a filter rather than a mode.
    obs::set_windows(Window::Net.bit() | Window::Gpu.bit());
    obs::emit(Window::Gpu, Kind::Note, 0, 999, 2);
    obs::emit(Window::Lock, Kind::Note, 0, 999, 1);
    assert_eq!(
        obs::counters().0,
        EACH + 1,
        "enabling a second window recorded {} events instead of one more",
        obs::counters().0 - EACH
    );

    // Off means off: not "buffered for later".
    obs::set_windows(0);
    assert!(!obs::enabled(), "clearing the mask left recording on");
    for i in 0..EACH {
        obs::emit(Window::Net, Kind::Note, 0, i, 0);
    }
    assert_eq!(
        obs::counters().0,
        EACH + 1,
        "{} event(s) were recorded with every window off",
        obs::counters().0 - EACH - 1
    );
    println!(
        "observe: window mask - 1 of 3 windows recorded {EACH} of {} offered, a second \
         window took effect without re-funding, and every window off records nothing OK",
        EACH * 3
    );
    obs::reset();
}

/// Frames taken out of the pool. `stats()` reports `(free, total)` and `total` is a
/// constant, so the difference is the only thing that measures anything.
fn pool_used() -> usize {
    let (free, total) = frames::stats();
    total - free
}

/// The four fields a reader checks before it trusts a single byte of the rest.
///
/// These are compile-time, so they are in the kernel image and a reader can
/// validate the *file* before the guest has executed an instruction. Asserting
/// them here is not redundant with that: it proves the const initialiser survived
/// into the running image, which is exactly what a linker that decided the page
/// was dead would break.
fn identity() {
    let r = kernel::obs::root::root();
    assert_eq!(r.magic, obs::OBS_MAGIC, "the root's magic is not the magic");
    assert_eq!(r.version, obs::OBS_VERSION, "root version mismatch");
    assert_eq!(
        r.abi_hash,
        obs::OBS_ABI_HASH,
        "the published layout hash is not the one this build computes"
    );
    assert_eq!(
        r.header_len as usize,
        core::mem::offset_of!(obs::ObsRoot, sections),
        "header_len does not point at the section table"
    );
    assert!(
        r.looks_valid(),
        "the root fails its own validity check, which is what a reader calls first"
    );
    // The identity check must also be able to say *no*. A reader's protection
    // against a wrong address is entirely `looks_valid`, so a version it does not
    // recognise has to fail - checked directly rather than assumed, because a
    // validator that accepts everything is indistinguishable from no validator.
    assert!(
        obs::OBS_VERSION.wrapping_add(1) != obs::OBS_VERSION,
        "version arithmetic"
    );
    assert_eq!(
        r.arch,
        arch::OBS_ARCH,
        "the root reports a different ISA than the one it was built for"
    );
    println!(
        "observe: root identity magic OK version={} abi_hash={:#018x} header_len={} arch={} OK",
        r.version, r.abi_hash, r.header_len, r.arch
    );
}

/// `self_pa`, against an address this kernel computes for itself.
///
/// The point of the field is that a reader which derived a physical address from
/// the ELF can confirm it landed where it meant to. Here the same question is asked
/// from the other side: the publisher's answer must equal what `virt_to_phys` says
/// about the symbol's address, and it must be a physical address rather than the
/// virtual one that a forgotten mask would leave behind.
fn self_address() {
    let r = kernel::obs::root::root();
    let va = kernel::obs::root_va();
    let pa = arch::virt_to_phys(va);
    assert_eq!(
        r.self_pa, pa as u64,
        "the root's published physical address is not its own"
    );
    assert!(
        r.self_pa != va as u64,
        "self_pa still holds a virtual address - the linear-map mask was not applied"
    );
    assert!(
        physical(r.self_pa),
        "self_pa {:#x} is not anywhere physical memory is",
        r.self_pa
    );
    assert_eq!(va % 4096, 0, "the root is not page-aligned");
    assert_eq!(
        r.va_base,
        arch::KERNEL_VA_BASE as u64,
        "the published linear-map base is not this kernel's"
    );
    assert_eq!(
        (va as u64) & !r.va_base,
        r.self_pa,
        "a reader applying the published va_base to the root's own address gets a \
         different answer than the root reports"
    );
    println!(
        "observe: root at va={va:#x} pa={:#x} page-aligned, va_base={:#x} converts it OK",
        r.self_pa, r.va_base
    );
}

/// Every published section must be findable and must carry a real physical address.
///
/// The oracle is deliberately **not** a recomputation of `virt_to_phys`, which
/// would only show the publisher agrees with itself. It is a question about the
/// machine: does this address land where physical memory is? A section published
/// with its virtual address by mistake - the single most likely error, and the one
/// that makes a host reader silently decode nothing - lands in the high half and
/// fails.
fn sections() {
    let r = kernel::obs::root::root();
    let published = r.published();
    assert!(
        !published.is_empty(),
        "the root published no sections, so nothing is reachable through it"
    );
    assert!(
        published.len() <= obs::OBS_MAX_SECTIONS,
        "more sections than the table holds"
    );

    for s in published {
        if s.va == 0 {
            // A witness section carrying no region. Its whole content is `stride`,
            // so it must not claim a length or a count it does not have.
            assert_eq!(s.pa, 0, "section kind {} has a pa but no va", s.kind);
            assert_eq!(s.len, 0, "section kind {} has a length but no va", s.kind);
            println!(
                "observe: section kind={} witness stride={} (no region) OK",
                s.kind, s.stride
            );
            continue;
        }
        assert!(
            s.va >= arch::KERNEL_VA_BASE as u64,
            "section kind {} publishes va {:#x}, which is not a kernel address",
            s.kind,
            s.va
        );
        assert!(
            physical(s.pa),
            "section kind {} publishes pa {:#x}, which is not anywhere physical memory is \
             (a virtual address left unmasked looks exactly like this)",
            s.kind,
            s.pa
        );
        assert_eq!(
            s.pa,
            arch::virt_to_phys(s.va as usize) as u64,
            "section kind {} disagrees with the linear map",
            s.kind
        );
        assert!(s.len > 0, "section kind {} has a zero length", s.kind);
        assert!(s.stride > 0, "section kind {} has a zero stride", s.kind);
        assert!(
            s.stride as u64 * s.count as u64 <= s.len,
            "section kind {} strides {} x {} past its own {} bytes",
            s.kind,
            s.stride,
            s.count,
            s.len
        );
        println!(
            "observe: section kind={} id={} pa={:#x} len={} stride={} count={} OK",
            s.kind, s.id, s.pa, s.len, s.stride, s.count
        );
    }

    // The layout witness: a reader built against a different `rheo-abi` strides the
    // event frames by the wrong number and decodes plausible nonsense, so the size
    // is published rather than assumed on both sides.
    let w = r
        .section(obs::OBS_SEC_EVENT_LAYOUT, 0)
        .expect("no event-layout witness, so a reader cannot detect an ABI mismatch");
    assert_eq!(
        w.stride as usize,
        size_of::<obs::ObsEvent>(),
        "the published event size is not the event size"
    );
    assert_eq!(w.stride, 32, "the event record is no longer 32 bytes");

    // The two planes that already exist are indexed. Their *contents* are their own
    // modules' business; what is claimed here is only that the root points at them.
    let t = r
        .section(obs::OBS_SEC_TEXT_RINGS, 0)
        .expect("the text log rings are not published");
    assert_eq!(
        t.count as usize,
        kernel::telemetry::MAX_RING_CPUS,
        "the text-ring section advertises a different CPU count than the ring array has"
    );
    let rings = r
        .section(obs::OBS_SEC_RINGS, 0)
        .expect("the event rings are not published");
    assert_eq!(
        rings.count as usize,
        kernel::smp::MAX_CPUS,
        "the ring section advertises a different CPU count than the array has"
    );
    assert_eq!(
        rings.stride as usize,
        size_of::<kernel::obs::ring::ObsRing>(),
        "the ring section's stride is not the per-CPU ring size, so a reader striding \
         it lands between headers"
    );
    // A reader finds each CPU's header by striding from the section base, so the
    // header must be at offset 0 of each element - the thing that makes the frame
    // directory reachable without publishing a section per CPU.
    assert_eq!(
        core::mem::offset_of!(kernel::obs::ring::ObsRing, hdr),
        0,
        "the ring header is not at the start of the ring"
    );
    let h = r
        .section(obs::OBS_SEC_HISTOGRAMS, 0)
        .expect("the latency histograms are not published");
    assert_eq!(
        h.count as usize,
        kernel::smp::MAX_CPUS,
        "the histogram section advertises a different CPU count than the array has"
    );

    assert_eq!(
        r.max_cpus as usize,
        kernel::smp::MAX_CPUS,
        "the root reports a different per-CPU array size than the arrays have"
    );
    assert!(
        r.online_cpus >= 1,
        "the root reports zero online CPUs while running on one"
    );
    println!(
        "observe: {} sections, max_cpus={} online_cpus={} OK",
        published.len(),
        r.max_cpus,
        r.online_cpus
    );
}

/// The tick must be a real counter, published with a real frequency.
///
/// A timestamp is only worth recording if a reader can turn it into a time, and it
/// can only do that from `tick_domain` and `tick_hz`. Both being non-zero is the
/// minimum; that the counter actually advances is the part worth checking, because
/// a per-ISA read that returned a constant would be indistinguishable from a very
/// fast machine.
fn tick() {
    let r = kernel::obs::root::root();
    assert!(
        r.tick_domain != obs::OBS_TICK_NONE,
        "no tick domain published, so a reader cannot say which clock the timestamps are"
    );
    assert_eq!(
        r.tick_domain,
        arch::OBS_TICK_DOMAIN,
        "the published tick domain is not this ISA's"
    );
    assert!(
        r.tick_hz > 0,
        "tick_hz is zero, so every recorded timestamp is unconvertible"
    );

    let a = arch::obs_tick();
    // Bounded work rather than a fixed iteration count, so this measures the
    // counter and not the compiler's opinion of an empty loop.
    arch::spin_loop(10_000);
    let b = arch::obs_tick();
    assert!(
        b > a,
        "the observability counter did not advance across 10,000 iterations \
         (a={a}, b={b}) - a constant timestamp is worse than none"
    );
    assert!(
        r.boot_tick > 0 && r.boot_tick <= a,
        "boot_tick {} is not a reading taken before this one ({a})",
        r.boot_tick
    );

    // How much time one tick is worth, so the resolution is on the record rather
    // than something a reader has to work out. On QEMU's riscv64 `virt` the
    // timebase is 10 MHz, so a tick is 100 ns and intervals shorter than that are
    // simply not resolvable there - which is why `tick_hz` is published at all.
    let ns_per_tick = 1_000_000_000u64 / r.tick_hz;
    println!(
        "observe: tick domain={} hz={} ({} ns/tick) advanced {} over 10k iterations OK",
        r.tick_domain,
        r.tick_hz,
        ns_per_tick,
        b - a
    );
}

/// Whether `pa` is somewhere the machine's physical memory actually is: either in
/// the frame pool, or below it, which is where the kernel image and the firmware
/// regions live (`arch::FRAME_POOL_BASE` is documented as sitting above the image).
///
/// Coarse on purpose. It does not need to identify the region - it needs to reject
/// the one wrong answer that matters, a kernel *virtual* address published where a
/// physical one belongs, which is astronomically above the top of RAM on all three
/// ISAs.
fn physical(pa: u64) -> bool {
    let pool_top = arch::FRAME_POOL_BASE as u64 + (frames::POOL_FRAMES * frames::FRAME_SIZE) as u64;
    pa < pool_top
}
