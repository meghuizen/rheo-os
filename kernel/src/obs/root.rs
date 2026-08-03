//! The observability root: one exported symbol from which every telemetry region
//! in the machine is reachable (docs/OBSERVABILITY.md).
//!
//! # Why a symbol in ordinary RAM and nothing else
//!
//! The alternatives were a device, an MMIO window, or a firmware config entry, and
//! each of them makes the plane a property of the *platform* rather than of the
//! kernel. This OS is meant to run on bare metal and under whatever hypervisor is
//! in front of it, so the plane has to be findable with nothing but the kernel ELF
//! and the ability to read memory - which a debugger, a hypervisor, a crash dump
//! and a cell holding the right capability all have, and none of which agree on
//! what devices exist.
//!
//! So: one page-aligned `#[no_mangle]` static. A reader resolves its virtual
//! address from the symbol table, converts to physical through the `PT_LOAD` that
//! contains it (all three linker scripts carry a real load address via `AT()`),
//! and reads. Nothing about that step is ISA-specific and nothing about it is
//! QEMU-specific.
//!
//! # What is filled when
//!
//! Magic, version, layout hash, `va_base` and `arch` are **compile-time**, so they
//! are in the image and a reader can validate the file before the guest has run an
//! instruction. Everything else - the physical address, the CPU counts, the tick
//! rate, the section table - is filled by [`publish`] during boot, because none of
//! it is knowable earlier.

use crate::abi::obs::{
    OBS_MAX_SECTIONS, OBS_SEC_CPU, OBS_SEC_EVENT_LAYOUT, OBS_SEC_HISTOGRAMS, OBS_SEC_NAMES,
    OBS_SEC_RINGS, OBS_SEC_TEXT_RINGS, ObsCpu, ObsEvent, ObsName, ObsRoot, ObsSection,
};
use core::sync::atomic::Ordering;

/// The one exported symbol.
///
/// `#[used]` guards against a future in which nothing in the kernel references this
/// static: a link-time garbage collector would then be entitled to drop the whole
/// page, which presents as a host tool reporting "no such symbol" on exactly the
/// builds most worth inspecting.
///
/// **Honest non-result**: it is not load-bearing today, and that was checked rather
/// than assumed. Removing it and rebuilding leaves the symbol present on all three
/// ISAs, because [`publish`] takes its address and `boot::init` calls [`publish`] on
/// every kernel - so there is a real reference and nothing to collect. It is kept
/// because the reference is incidental to the plane's purpose (a boot that stopped
/// publishing would still want the page findable), not because a control fired.
#[unsafe(no_mangle)]
#[used]
pub static mut RHEO_OBS_ROOT: ObsRoot =
    ObsRoot::new(crate::arch::OBS_ARCH, crate::arch::KERNEL_VA_BASE as u64);

/// The root, shared.
///
/// # Safety of the shared reference
/// The non-atomic fields are written exactly once, by [`publish`], during
/// single-threaded boot; after that the only mutation is through the root's own
/// atomics. So a shared reference handed out afterwards aliases nothing that
/// changes underneath it. This is the same argument `crate::telemetry` and
/// `crate::metrics` make for their statics.
#[inline(always)]
pub fn root() -> &'static ObsRoot {
    // SAFETY: as documented above - published once at boot, atomics thereafter.
    unsafe { &*core::ptr::addr_of!(RHEO_OBS_ROOT) }
}

/// The root, exclusively. Boot-time publication only.
///
/// # Safety
/// The caller must be the only thing touching the root - true during boot setup
/// and between runs, and nowhere else.
#[allow(clippy::mut_from_ref)]
unsafe fn root_mut() -> &'static mut ObsRoot {
    // SAFETY: the caller's contract, above.
    unsafe { &mut *core::ptr::addr_of_mut!(RHEO_OBS_ROOT) }
}

/// Kernel VA of the root, which is what a reader's ELF-derived address must match.
pub fn root_va() -> usize {
    core::ptr::addr_of!(RHEO_OBS_ROOT) as usize
}

/// Fill in everything that is not knowable at compile time, and publish the
/// sections that exist.
///
/// Called from `boot::init`. Idempotent: it rebuilds the section table from
/// scratch rather than appending, so calling it again after more of the plane has
/// come up republishes rather than duplicating.
///
/// **Only regions that really exist are published.** A section for a plane that is
/// not built would be a reader following an address to nothing; `section_count`
/// reflects what is there, and a reader that cannot find a kind reports it absent.
pub fn publish() {
    // SAFETY: boot setup, single-threaded, before any secondary is started.
    let r = unsafe { root_mut() };

    r.self_pa = crate::arch::virt_to_phys(root_va()) as u64;
    r.max_cpus = crate::smp::MAX_CPUS as u32;
    r.online_cpus = online_cpus();
    r.tick_domain = crate::arch::OBS_TICK_DOMAIN;
    r.tick_hz = crate::arch::obs_tick_hz();
    r.boot_tick = crate::arch::obs_tick();
    r.section_count = 0;

    // A layout witness carrying no address: a reader built against a different
    // `rheo-abi` would otherwise stride the event frames by the wrong amount and
    // decode plausible nonsense. Published first so it is cheap to find.
    add(
        r,
        ObsSection {
            kind: OBS_SEC_EVENT_LAYOUT,
            id: 0,
            va: 0,
            pa: 0,
            len: 0,
            stride: size_of::<ObsEvent>() as u32,
            count: 0,
        },
    );

    // The event plane. One section for the whole per-CPU array: each element begins
    // with an `ObsRingHdr`, and the event frames are reached from that header rather
    // than published separately - a ring is funded by its own CPU, so per-directory
    // sections would mean several cores appending to one section table.
    add(
        r,
        region(
            OBS_SEC_RINGS,
            0,
            crate::obs::rings_va(),
            size_of::<crate::smp::PerCpu<crate::obs::ring::ObsRing>>(),
            size_of::<crate::obs::ring::ObsRing>() as u32,
            crate::smp::MAX_CPUS as u32,
        ),
    );

    add(
        r,
        region(
            OBS_SEC_TEXT_RINGS,
            0,
            crate::telemetry::rings_va(),
            size_of::<crate::telemetry::Rings>(),
            size_of::<crate::telemetry::Ring>() as u32,
            crate::telemetry::MAX_RING_CPUS as u32,
        ),
    );

    add(
        r,
        region(
            OBS_SEC_HISTOGRAMS,
            0,
            crate::metrics::sets_va(),
            size_of::<crate::smp::PerCpu<[crate::metrics::Histogram; crate::metrics::METRICS]>>(),
            size_of::<[crate::metrics::Histogram; crate::metrics::METRICS]>() as u32,
            crate::smp::MAX_CPUS as u32,
        ),
    );

    // The snapshot plane: one `ObsCpu` per CPU (docs/OBSERVABILITY.md 11, S3).
    add(
        r,
        region(
            OBS_SEC_CPU,
            0,
            crate::obs::cpus_va(),
            size_of::<crate::smp::PerCpu<ObsCpu>>(),
            size_of::<ObsCpu>() as u32,
            crate::smp::MAX_CPUS as u32,
        ),
    );

    // The name table: which counter slot means what, as data a reader takes from
    // the kernel it is actually reading rather than from a header it was built
    // against (`.rodata`, so `pa` is in the image).
    let (names_va, names_n) = crate::obs::names_va();
    add(
        r,
        region(
            OBS_SEC_NAMES,
            0,
            names_va,
            names_n * size_of::<ObsName>(),
            size_of::<ObsName>() as u32,
            names_n as u32,
        ),
    );
}

/// Re-read how many CPUs are online.
///
/// Separate from [`publish`] because bring-up happens long after it: `boot::init`
/// runs before `smp::start_all`, so a count taken there would say 1 for the rest
/// of the run on a machine with four cores. Called by whoever brings the
/// secondaries up, which is the only code that knows the answer changed.
pub fn refresh_online() {
    // SAFETY: called after bring-up completes, from the CPU that performed it.
    let r = unsafe { root_mut() };
    r.online_cpus = online_cpus();
}

/// Publish which windows are being recorded.
///
/// The mask lives **in the root** rather than in a private kernel static so that
/// there is one copy: a reader sees exactly what is on, and cannot be told one thing
/// by a mirror while the kernel consults another. An atomic store, so this is the one
/// field that legitimately changes after boot.
pub fn republish_windows(mask: u32) {
    root().windows.store(mask, Ordering::Release);
}

/// Which windows are being recorded.
#[inline(always)]
pub fn windows() -> u32 {
    root().windows.load(Ordering::Relaxed)
}

/// How many CPUs are running.
///
/// **Not** `smp::online_count()` on its own, which turns out to answer a different
/// question than the name suggests: it counts CPUs the SMP bring-up *registered*,
/// and the only thing that registers the boot CPU is `smp::init`, which exists only
/// under the `smp` feature. So on every single-CPU boot it returns 0 - while a CPU
/// is demonstrably executing this function.
///
/// Publishing that 0 would be a plain falsehood in the field a reader uses to size
/// every per-CPU view, so the floor is stated instead: whoever is running this is
/// running, whatever the registry was told. The `max` is the honest answer, not a
/// papering-over - and it is here rather than in `smp` because `online_count`'s
/// answer is correct for the question `smp` asks it.
fn online_cpus() -> u32 {
    (crate::smp::online_count() as u32).max(1)
}

/// Build a section for a region reached through the kernel's linear map.
fn region(kind: u32, id: u32, va: usize, len: usize, stride: u32, count: u32) -> ObsSection {
    ObsSection {
        kind,
        id,
        va: va as u64,
        pa: crate::arch::virt_to_phys(va) as u64,
        len: len as u64,
        stride,
        count,
    }
}

/// Append a section, silently ignoring anything past the table.
///
/// Silent because the table is fixed at 32 and the kernel publishes far fewer;
/// overflowing it would be a programming error caught by the `observe` kernel's
/// count assertion, not a runtime condition worth a branch at every call site.
fn add(r: &mut ObsRoot, s: ObsSection) {
    let n = r.section_count as usize;
    if n >= OBS_MAX_SECTIONS {
        return;
    }
    r.sections[n] = s;
    r.section_count += 1;
}
