//! In-QEMU test kernel for hardware discovery. Boots, runs the discovery
//! pass, prints the machine inventory (firmware source, CPU features,
//! memory map, NUMA nodes, PCIe devices), and asserts the basics the
//! platform must report.

#![no_std]
#![no_main]

use kernel::hw;
use kernel::{arch, println};

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("hwinfo: start on {}", arch::NAME);

    hw::print_summary();

    let inv = hw::inventory();
    assert!(inv.firmware != hw::Firmware::None, "no firmware discovered");
    assert!(inv.ncpus >= 1, "no CPUs discovered");
    assert!(
        inv.ram_bytes() >= 64 * 1024 * 1024,
        "too little RAM discovered"
    );
    assert!(inv.nnodes >= 1, "no NUMA nodes");
    assert!(inv.cpu.features != 0, "no CPU features detected");

    cpu_topology(inv);
    core_classes(inv);
    cpu_features(inv);

    println!("hwinfo: PASS");
    arch::exit(arch::ExitCode::Success)
}

// ------------------------------------------------- the CPU topology, against the launch
//
// Which CPUs are threads of one core, and which share a last-level cache
// (docs/RESOURCE-GRAPH.md 2.4a). Two questions a scheduler asks for two different reasons -
// "whose run queue is cheap to steal from" (a shared cache) and "who am I contending with"
// (a shared core) - and until now the answer was nothing at all: `CpuInfo` carried a hardware
// id and a NUMA node, so every CPU looked equidistant from every other.
//
// **The oracle is the launch.** xtask starts this kernel with
// `-smp 4,sockets=1,cores=2,threads=2`: four CPUs, two SMT pairs, one cache domain. The base
// launch is a flat `-smp 4`, where a correct discovery and a broken one both say "four cores",
// so it could prove nothing. Every number below comes from that line and not from the kernel's
// own count.
//
// **What each ISA can see differs, and the difference is QEMU's, not the decode's.** Verified
// in QEMU's own source rather than inferred from this kernel's output:
//
// - x86-64 reads CPUID leaf `0x0B`, which reports the SMT and Core levels properly, so both
//   halves of the launch are visible. (Leaf 4 - the cache descriptors - returns all zeros
//   under TCG `-cpu max`, measured; the package boundary is the fallback.)
// - ARM64 builds MPIDR as `arm_build_mp_affinity(idx, clustersz)` =
//   `(idx / clustersz) << 8 | (idx % clustersz)` (`target/arm/cpu.c`), purely index-based with
//   no MT bit and no thread field. `-smp threads=2` cannot reach it, so the machine genuinely
//   describes four independent cores.
// - riscv64 emits `/cpus/cpu-map/cluster<socket>/core<hart>` with a `cpu` phandle per hart and
//   **no `thread` nodes at all** (`hw/riscv/virt.c`), so the same is true there.
//
// So the cache-domain claim is asserted on all three ISAs and the SMT claim only where the
// platform can express it. The alternative - asserting two cores everywhere - would fail on
// two ISAs for a reason that has nothing to do with the code being tested.
fn cpu_topology(inv: &hw::Inventory) {
    use kernel::hw::graph::{self, NodeKind};

    // Declared by the launch.
    const CPUS: usize = 4;
    const CACHE_DOMAINS: usize = 1; // sockets=1
    const THREADS_PER_CORE: usize = 2; // threads=2

    assert_eq!(
        inv.ncpus, CPUS,
        "the launch declares -smp 4 and discovery found {}",
        inv.ncpus
    );
    assert!(
        inv.topo != hw::TopoSource::None,
        "no CPU topology was discovered on {}. Every ISA here has a source: CPUID leaf 0x0B, \
         MPIDR's MT bit, or the device tree's cpu-map",
        arch::NAME
    );
    for c in &inv.cpus[..inv.ncpus] {
        assert!(
            c.core_id != hw::TOPO_UNKNOWN && c.llc_id != hw::TOPO_UNKNOWN,
            "CPU id{} has no core or cache id while the source is {:?} - a partly-filled \
             topology is worse than none, because a caller reads a real grouping for some \
             CPUs and the sentinel for others",
            c.hw_id,
            inv.topo
        );
    }

    // SAFETY: built at boot; read-only here, single-threaded.
    let g = unsafe { graph::graph() };

    // Every CPU is a graph node, in a Core, in a Cache. Asked through `siblings`, which is
    // the query a scheduler uses - so this tests the path a consumer takes, not the fields.
    let mut cache_domains = 0usize;
    for i in 0..CPUS {
        if g.find(NodeKind::Cache, i as u64).is_some() {
            cache_domains += 1;
        }
    }
    assert_eq!(
        cache_domains, CACHE_DOMAINS,
        "the graph holds {cache_domains} cache domains; the launch declares sockets=1, so \
         all {CPUS} CPUs share one"
    );

    for c in &inv.cpus[..inv.ncpus] {
        let cpu = g
            .find(NodeKind::Cpu, c.hw_id as u64)
            .expect("a discovered CPU is not a graph node");
        let sharing_cache = g.siblings(cpu, NodeKind::Cache).count();
        assert_eq!(
            sharing_cache, CPUS,
            "CPU id{} shares its cache with {sharing_cache} CPUs (itself included); the \
             launch declares one socket, so it must be all {CPUS}",
            c.hw_id
        );
        // A CPU is always a sibling of itself at every level: a caller that iterates
        // siblings to spread work must find itself in the set, or it will move work it is
        // already running.
        assert!(
            g.siblings(cpu, NodeKind::Core).any(|s| s == cpu),
            "CPU id{} is not among its own core's siblings",
            c.hw_id
        );
        // The level argument is not a free-for-all: only Core and Cache are groupings a CPU
        // has, and asking for anything else must answer nothing rather than something
        // plausible.
        assert!(
            g.group_of(cpu, NodeKind::MemoryNode).is_none(),
            "group_of answered for a level a CPU has no membership in"
        );
    }

    // The SMT half, where the platform can express it.
    #[cfg(target_arch = "x86_64")]
    {
        let mut cores = 0usize;
        for i in 0..CPUS {
            if g.find(NodeKind::Core, i as u64).is_some() {
                cores += 1;
            }
        }
        assert_eq!(
            cores,
            CPUS / THREADS_PER_CORE,
            "the graph holds {cores} cores; the launch declares cores=2"
        );
        for c in &inv.cpus[..inv.ncpus] {
            let cpu = g.find(NodeKind::Cpu, c.hw_id as u64).unwrap();
            let n = g.siblings(cpu, NodeKind::Core).count();
            assert_eq!(
                n, THREADS_PER_CORE,
                "CPU id{} has {n} threads on its core; the launch declares threads=2",
                c.hw_id
            );
        }
        println!(
            "hwinfo: CPU TOPOLOGY DISCOVERED - {CPUS} CPUs, {cores} cores of \
             {THREADS_PER_CORE} threads, {CACHE_DOMAINS} cache domain, source {:?}. Each \
             number is what `-smp 4,sockets=1,cores=2,threads=2` declares, read back through \
             the graph query a scheduler uses (`siblings`) rather than off the fields OK",
            inv.topo
        );
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        // Each CPU is its own core here, and that is the platform's own description rather
        // than a gap in the decode - see the note above, cited to QEMU's source.
        let mut cores = 0usize;
        for i in 0..CPUS {
            if g.find(NodeKind::Core, i as u64).is_some() {
                cores += 1;
            }
        }
        assert_eq!(
            cores, CPUS,
            "the graph holds {cores} cores; on this ISA QEMU describes {CPUS} independent \
             cores because MPIDR is index-based (ARM64) and cpu-map emits no thread nodes \
             (riscv64), so -smp threads=2 cannot reach the guest"
        );
        for c in &inv.cpus[..inv.ncpus] {
            let cpu = g.find(NodeKind::Cpu, c.hw_id as u64).unwrap();
            assert_eq!(
                g.siblings(cpu, NodeKind::Core).count(),
                1,
                "CPU id{} has an SMT sibling on a platform that cannot express one",
                c.hw_id
            );
        }
        println!(
            "hwinfo: CPU TOPOLOGY DISCOVERED - {CPUS} CPUs, {CACHE_DOMAINS} cache domain, \
             source {:?}. The cache claim is the launch's `sockets=1`. The SMT claim is \
             SKIPPED WITH A REASON: QEMU cannot express threads to a guest on this ISA \
             (ARM64 MPIDR is index-based; riscv64 cpu-map has no thread nodes - both read \
             out of QEMU's source, not guessed), so it reports {CPUS} independent cores and \
             that is asserted instead OK",
            inv.topo
        );
        let _ = THREADS_PER_CORE;
    }
}

// --------------------------------------------- core classes, and an honest absence
//
// P-cores and E-cores execute the **same** instruction set and differ in how fast and how
// efficiently they run it (docs/RESOURCE-GRAPH.md 2.4b), so the model carries a class and a
// capacity per CPU and says nothing about features. What this phase asserts is that the
// discovery **ran and honestly found nothing**, which is a real claim rather than a placeholder:
//
// - x86-64's probe reads CPUID leaf 7's hybrid flag and then leaf `0x1A`. QEMU 11 implements
//   neither - it has no hybrid support at all, checked in its source - so the answer is "not a
//   hybrid part".
// - ARM64 has no architectural register naming a core's class (`MIDR_EL1` names the *part*, and
//   mapping parts to classes needs a table of every part ever shipped), and a bare-ELF `virt`
//   boot gets neither PPTT nor a device tree.
// - riscv64's source is the device tree's `capacity-dmips-mhz`, which QEMU never emits (the
//   string does not appear in its source).
//
// So every core reads `Unknown` at full capacity, the graph reports not-hybrid, and Thread
// Director reports absent. The value of asserting an absence is that it fails the moment a
// discovery path starts inventing an answer - which is exactly what the first version of the
// cache-domain decode did.
fn core_classes(inv: &hw::Inventory) {
    use kernel::hw::graph::{self, CAPACITY_FULL, CoreClass};
    use kernel::sched::hetero;

    assert_eq!(
        inv.class_src,
        hw::ClassSource::None,
        "a core class was discovered on {}; no emulator here models a hybrid part, so this is \
         either new hardware or a discovery path inventing an answer",
        arch::NAME
    );
    for c in &inv.cpus[..inv.ncpus] {
        assert_eq!(
            c.class,
            CoreClass::Unknown,
            "CPU id{} claims a class on a machine that describes none",
            c.hw_id
        );
        assert_eq!(
            c.capacity, CAPACITY_FULL,
            "CPU id{} is not at full capacity on a machine whose cores are all the same - every \
             core of such a machine *is* the fastest core there is",
            c.hw_id
        );
    }
    // Every core reported its model id, and none diverged - the asymmetry this kernel could
    // notice without a table of part numbers.
    assert!(
        !inv.model_divergence,
        "two cores report different models on a machine QEMU builds from one CPU type"
    );
    assert!(
        !hetero::thread_director(),
        "Intel Thread Director reported present. QEMU 11 sets neither CPUID.06H:EAX[19] nor \
         EAX[23] - it has no hybrid support at all"
    );

    // SAFETY: built at boot; read-only here, single-threaded.
    let g = unsafe { graph::graph() };
    assert!(
        !g.is_hybrid(),
        "the graph reports a hybrid machine where the inventory reports one kind of core - the \
         two must not be able to disagree, since the graph is built from the inventory"
    );
    // The placement query still answers on a uniform machine, and answers the *first* CPU:
    // a query that refused unless the machine was hybrid would make every caller special-case it.
    let first = g
        .find(graph::NodeKind::Cpu, inv.cpus[0].hw_id as u64)
        .expect("CPU 0 is not a graph node");
    assert_eq!(
        g.fastest_cpu(None),
        Some(first),
        "fastest_cpu on a uniform machine must answer its first CPU, not None"
    );
    assert_eq!(
        g.fastest_cpu(Some(CoreClass::Performance)),
        None,
        "fastest_cpu answered for a class no CPU here claims"
    );
    println!(
        "hwinfo: CORE CLASSES DISCOVERED AND HONESTLY ABSENT - {} CPUs, all Unknown at capacity \
         {CAPACITY_FULL}, source {:?}, no model divergence, Thread Director absent. P-cores and \
         E-cores run the SAME instruction set and differ in capacity, so the model carries a \
         class and a capacity per CPU; QEMU models no hybrid part (no CPUID leaf 0x1A, no \
         hybrid flag, no capacity-dmips-mhz - read out of its source), so the discovery ran and \
         found nothing, and the graph agrees with the inventory that this machine is uniform OK",
        inv.ncpus, inv.class_src
    );
}

// ------------------- per-CPU features, and the rule the AVX-512 story wrote
//
// A core class is not an instruction set (docs/RESOURCE-GRAPH.md 2.4b) - but a hybrid part *can*
// differ in features, and when it does the consequence is not a preference. Early Alder Lake had
// AVX-512 on the P-cores only, and Intel disabled it **chip-wide** rather than ship a machine where
// a thread using it could not be migrated. The rule that follows:
//
//   A feature present on some cores and not others must either restrict placement to those cores,
//   or not be advertised at all.
//
// So the inventory carries a feature set **per CPU** (read by that core, because CPUID answers
// about whoever executed it), and what the machine advertises - `inv.cpu.features` - is the
// **intersection**. The union is kept beside it, so the difference between the two is exactly the
// set of features that exist on part of the machine, and a caller can tell the two apart.
//
// Two halves, because QEMU builds every CPU of a machine from one model (checked in its source) and
// the interesting case therefore cannot be booted:
//
//  1. **Discovered**: every core reports, they all agree, and the intersection equals the union
//     equals what the machine advertises. That fails the moment a per-core read starts answering
//     about the wrong core, which is the defect this shape is prone to.
//  2. **Declared**: one core is declared to lack a feature the others have, and the rule is then
//     asserted - the machine stops advertising it, the union still has it, and the graph's provider
//     query returns exactly the cores that kept it. Restored afterwards.
fn cpu_features(inv: &hw::Inventory) {
    use kernel::hw::graph::{self, CapClass, IsaSet, NodeKind, Request};

    // 1. Discovered. **This boot starts no secondaries**, so exactly one core has reported - and
    // that is the first thing asserted, because the tempting shortcut is to fill every core with
    // the boot CPU's answer. A core that has never executed an instruction has not told anyone
    // what it can do, and saying so with 0 is what lets the intersection be computed over
    // *reporting* cores rather than over guesses (docs/ENGINEERING.md 11). The multi-core
    // agreement is asserted in `smp`, where the secondaries genuinely come up and report.
    assert!(
        inv.cpus[0].features != 0,
        "the boot CPU reported no features of its own"
    );
    for c in &inv.cpus[1..inv.ncpus] {
        assert_eq!(
            c.features, 0,
            "CPU id{} carries a feature set on a boot that never started it. Only a core can read \
             its own CPUID, so anything here was copied from another core",
            c.hw_id
        );
    }
    assert_eq!(
        inv.features_common, inv.features_any,
        "one reporting core, and its features are not both the intersection and the union"
    );
    assert_eq!(
        inv.cpu.features, inv.features_common,
        "the machine advertises a feature set that is not the intersection of its cores'"
    );

    // 2. Declared: take a feature every core has and remove it from CPU 1 alone.
    if inv.ncpus < 2 || inv.features_common == 0 {
        println!("hwinfo: SKIP the feature-divergence half - it needs two cores and a feature");
        return;
    }
    let bit = 1u64 << inv.features_common.trailing_zeros();
    let kept = inv.cpus[0].features;
    kernel::hw::declare_cpu_features(1, kept & !bit);

    let after = hw::inventory();
    let common_ok = after.features_common & bit == 0;
    let any_ok = after.features_any & bit != 0;
    let advertised_ok = after.cpu.features & bit == 0;

    // The graph's own answer: which CPUs can run code that needs the feature?
    // SAFETY: read-only, single-threaded, and `declare_cpu_features` has just refreshed it.
    let g = unsafe { graph::graph() };
    let req = Request {
        class: CapClass::FloatSimd,
        bytes: 0,
        isa: IsaSet {
            arch: graph::Arch::Unknown,
            baseline: 0,
            features: bit,
        },
        bit_exact: false,
        detail_mask: 0,
    };
    // `Arch::Unknown` would filter everything out, so ask with the arch the nodes carry.
    let arch_of = g
        .get(g.find(NodeKind::Cpu, after.cpus[0].hw_id as u64).unwrap())
        .unwrap()
        .isa
        .arch;
    let req = Request {
        isa: IsaSet {
            arch: arch_of,
            ..req.isa
        },
        ..req
    };
    let providers: usize = g.providers(&req).count();
    let excluded_listed = g
        .find(NodeKind::Cpu, after.cpus[1].hw_id as u64)
        .map(|id| g.providers(&req).any(|p| p == id))
        .unwrap_or(true);

    // Restore before asserting, so a failure cannot leave a declared divergence behind.
    kernel::hw::declare_cpu_features(1, kept);
    let restored = hw::inventory();

    assert!(
        common_ok,
        "a feature one core lacks is still in the intersection - the machine would advertise a \
         promise it cannot keep for a thread the scheduler migrates"
    );
    assert!(
        any_ok,
        "the union lost a feature that one core still has, so the difference between what exists \
         and what is safe to advertise is no longer visible"
    );
    assert!(
        advertised_ok,
        "the machine still advertises a feature only some of its cores have. That is the early \
         Alder Lake hazard, and Intel's answer was to disable it chip-wide"
    );
    // **One**, and the oracle is the boot rather than the inventory: this kernel starts no
    // secondaries, so the boot CPU is the only core that ever reported a feature set, and CPU 1
    // has just been declared to lack this one. Counting cores whose recorded features contain the
    // bit would ask the same data the graph was built from - self-consistency, not a check.
    assert_eq!(
        providers, 1,
        "the graph offers the feature on {providers} CPUs. Only the boot CPU reported one (no \
         secondary is started here) and CPU 1 was just declared to lack it, so exactly one core \
         can provide it"
    );
    assert!(
        !excluded_listed,
        "the core that lacks the feature is still offered as a provider of it - a placement \
         following that answer produces a SIGILL, which is the outcome the ISA filter exists to \
         make impossible"
    );
    assert_eq!(
        restored.cpu.features, kept,
        "the machine was not restored after the declared divergence"
    );
    println!(
        "hwinfo: PER-CPU FEATURES, AND THE MIGRATION RULE - the boot CPU reports its own set and \
         the {} cores this boot never starts report NOTHING rather than a copy of it. With one \
         core declared to lack a feature the boot CPU has, the machine STOPS ADVERTISING it (the \
         intersection, not the union), the union still shows it exists, and the graph offers it on \
         exactly the one core that kept it - the core that lacks it is not a provider, so a \
         placement cannot follow the graph into a SIGILL. That is the early Alder Lake rule - \
         AVX-512 on P-cores only, disabled chip-wide rather than shipped as a migration hazard - \
         made mechanical rather than remembered OK",
        restored.ncpus - 1
    );
}
