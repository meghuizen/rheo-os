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

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init(); // ends in obs::publish()
    println!("observe: start on {}", arch::NAME);

    identity();
    self_address();
    sections();
    tick();
    events();

    println!("observe: PASS");
    arch::exit(arch::ExitCode::Success)
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
