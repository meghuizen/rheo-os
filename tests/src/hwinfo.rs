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
