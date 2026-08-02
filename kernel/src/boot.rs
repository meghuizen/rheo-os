//! The boot sequencer: bring the machine up in dependency order.
//!
//! This is five lines of ordering, but where they live matters. It used to be
//! `arch::init()`, inside the per-ISA namespace - and because a full bring-up
//! must also start the portable subsystems, that function reached *up* into
//! `crate::{time, hw, rng, svc}`, each of which depends on `arch`. Three module
//! cycles, from three references, none of them per-ISA
//! (docs/ARCHITECTURE-DEBT.md 3.6).
//!
//! Moving the sequencer here makes the layering acyclic without changing what
//! runs or in what order: `arch::init()` now does only the arch's own work
//! (console, exception vectors, kernel page tables), and the portable half is
//! sequenced from a portable module. `arch` no longer references anything above
//! it, so the dependency direction is one-way at every edge.
//!
//! Every kernel binary calls [`init`] first, before it touches any subsystem.

/// Full bring-up for a kernel binary, in dependency order.
///
/// 1. The arch itself: serial console, exception vectors, the kernel address
///    space with the MMU on ([`crate::arch::init`]).
/// 2. The portable subsystems that need the arch already up: the monotonic
///    clock, hardware discovery (which reads firmware tables through arch
///    accessors), the DRBG (which seeds from the arch's hwrng), and the
///    system-service layer.
///
/// Idempotence is *not* claimed: call it once, at the top of `kernel_main`.
pub fn init() {
    crate::arch::init();
    crate::time::init();
    crate::hw::detect();
    // Learn which pool frames sit on which NUMA node. Here and not in
    // `frames::init()`, because the pool is brought up inside `arch::init()` above -
    // before any firmware table has been read, so the question is unanswerable there
    // (docs/SUBSTRATE.md pillar 6).
    crate::mm::frames::init_numa(crate::hw::inventory());
    // The resource graph, from the same inventory and for the same reason it is here rather
    // than in `arch::init` (docs/RESOURCE-GRAPH.md). Additive: nothing reads it yet, so a
    // boot that ignores it behaves exactly as it did.
    crate::hw::graph_build::build(crate::hw::inventory());
    // A randomness *device*, before the root DRBG is keyed, so its bytes are in
    // the entropy pool when the key is drawn. Additive: a machine without one
    // probes, finds nothing and continues (docs/TIME-IDENTITY.md 4a). It has to
    // be after `hw::detect` because the PCI path needs the discovered ECAM base.
    crate::hw::tpm::init();
    crate::hw::virtio_rng::init();
    crate::hw::virtio_input::init();
    crate::rng::init();
    // Power-on self test for the generator itself (docs/TIME-IDENTITY.md 4a):
    // the ChaCha20 known-answer vector, the FIPS 140-2 continuous test, and an
    // SP 800-90B window over live output. Silent when healthy; a failure panics
    // naming the test, because a broken generator must not reach a cell.
    crate::rng::health::check();
    crate::svc::init();
    // Pre-fund every cell's recorded address-space layout, so the frames its table
    // needs are a boot cost rather than being charged to whichever operation happens to
    // establish a cell's first region (docs/SUBSTRATE.md pillar 2).
    crate::user::init_layouts();
}
