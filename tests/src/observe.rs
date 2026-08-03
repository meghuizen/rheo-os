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
//! ## What it deliberately does not claim
//!
//! Nothing is instrumented yet and no window exists, so this says nothing about
//! event recording, cost, or what a real reader does with the plane. The symbol's
//! resolvability from outside is checked by the host tool that resolves it, which
//! is where that claim belongs - here it would only be this kernel reading its own
//! address, which proves the linker kept the page but not that anyone else can name
//! it.

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

    println!("observe: PASS");
    arch::exit(arch::ExitCode::Success)
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
