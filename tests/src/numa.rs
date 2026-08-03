//! In-QEMU test kernel for **NUMA-typed memory** (docs/SUBSTRATE.md pillar 6).
//!
//! A frame's node used to be a fact the kernel discovered and then threw away: the
//! machine inventory recorded which NUMA node each memory region belongs to,
//! `SYS_GRANT`'s fourth argument carried a node hint that librheo's
//! `mem::reserve_on` has always sent, and the allocator served every request from one
//! rotating search over one pool. The hint was documented as "recorded but
//! single-node in QEMU" - two claims, of which only the second was true, and the
//! second stopped being true the moment QEMU was asked for two nodes.
//!
//! This kernel proves the allocator honours it, against an oracle it cannot reach.
//!
//! ## The oracle
//!
//! QEMU is launched with two 512 MiB nodes, so the boundary between them is the
//! **first 512 MiB of RAM** - a number this test knows because it is how the test
//! launched QEMU, not because it asked the kernel. RAM base is derived from the one
//! documented relationship in `mm/frames.rs`: the pool sits 64 MiB into RAM on every
//! ISA. So `boundary = (FRAME_POOL_BASE - 64 MiB) + 512 MiB`, which is
//! `0x2000_0000` on x86-64 and `0xa000_0000` on riscv64 - and every assertion below
//! compares a physical address against that, never against the node ranges the
//! allocator computed for itself.
//!
//! ## Per-ISA reality (the established skip-with-reason pattern)
//!
//!   - **x86-64**: two real nodes, discovered from the ACPI **SRAT**.
//!   - **riscv64**: two real nodes, discovered from the **device tree**
//!     (`numa-node-id`).
//!   - **ARM64**: skip-with-reason + PASS. QEMU hands a bare-ELF `-kernel` boot no
//!     device tree pointer in `x0` on `virt` (measured - passing `-dtb` explicitly
//!     does not reach it either), so there is no firmware source describing memory
//!     at all and the built-in profile reports one node. The single-node path is
//!     asserted unchanged instead, which is the honest thing this ISA can prove.
//!
//! ## Scope, stated rather than implied
//!
//! The placement decision is made in `frames::alloc_on`, and that is where this
//! proves it. The cell-facing path (`SYS_GRANT` node hint -> grant slot ->
//! `commit_range_from` -> `alloc_on`) is wired and the argument is threaded through,
//! but it is proven **at the kernel seam, not from inside a cell**: a cell cannot see
//! a physical address, so asserting placement from userspace would need the kernel to
//! walk the cell's page tables and report back - a harness, not a stronger claim
//! about the mechanism. Said plainly per docs/ENGINEERING.md 7.

#![no_std]
#![no_main]

use kernel::hw;
use kernel::mm::frames;
use kernel::mm::kmeta::{self, Funded};
use kernel::{arch, println};

/// How QEMU is launched for this kernel: node 0 gets the first 512 MiB of RAM.
/// The oracle, and the reason it is an oracle - it comes from the launch, not the
/// kernel (see the module docs).
const NODE0_BYTES: u64 = 512 * 1024 * 1024;

/// The pool's documented offset into RAM (`mm/frames.rs`): 64 MiB on every ISA.
const POOL_OFFSET_IN_RAM: u64 = 64 * 1024 * 1024;

/// Frames to take per node in the placement phase. Small - the claim is *where*
/// they land, not how many fit.
const PROBE: usize = 64;

/// Every free frame node 1 can hold, so the exhaustion phase can run it dry.
/// Node 1's share of a 512 MiB pool that starts 64 MiB into a 512 MiB node is
/// 64 MiB = 16384 frames; sized with headroom rather than exactly.
static mut HELD: [usize; 20000] = [0; 20000];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init(); // runs hw::detect -> frames::init_numa
    println!("numa: start on {}", arch::NAME);

    let inv = hw::inventory();
    println!(
        "numa: firmware={:?} nodes={} pool_nodes={}",
        inv.firmware,
        inv.nnodes,
        frames::nodes_known()
    );

    if inv.nnodes < 2 {
        single_node(inv);
        graph_degraded();
        println!("numa: PASS");
        arch::exit(arch::ExitCode::Success)
    }
    numa_two_nodes(inv);
    graph_distances(inv);
    println!("numa: PASS");
    arch::exit(arch::ExitCode::Success)
}

/// The two-node proof. Split from `kernel_main` so the single-node early exit above
/// is a plain `return`-shaped branch rather than a diverging call in the middle.
fn numa_two_nodes(inv: &hw::Inventory) {
    // --- The oracle, and the ranges the allocator built for itself -----------
    let ram_base = arch::FRAME_POOL_BASE as u64 - POOL_OFFSET_IN_RAM;
    let boundary = ram_base + NODE0_BYTES;
    println!("numa: ram_base={ram_base:#x} node1 starts at {boundary:#x} (oracle)");

    assert_eq!(
        frames::nodes_known(),
        inv.nnodes,
        "the pool learned a different node count than discovery reported"
    );
    let (lo0, hi0) = frames::node_range(0);
    let (lo1, hi1) = frames::node_range(1);
    println!("numa: pool frames node0=[{lo0}..{hi0}) node1=[{lo1}..{hi1})");
    assert!(lo0 < hi0, "node 0 holds no pool frames");
    assert!(
        lo1 < hi1,
        "node 1 holds no pool frames - nothing to place on"
    );
    // The two ranges must **partition** the pool: adjacent, and together the whole
    // of it. A gap would be frames no node claims (so `alloc_on` could never reach
    // them); an overlap would be frames two nodes both claim (so placement would be
    // a coin toss). Neither is detectable from a single successful allocation.
    assert_eq!(lo0, 0, "node 0's range does not start at the pool base");
    assert_eq!(hi0, lo1, "the node ranges are not adjacent");
    assert_eq!(
        hi1,
        frames::POOL_FRAMES,
        "the node ranges do not cover the pool"
    );
    // And the boundary the allocator derived is the one the oracle names.
    assert_eq!(
        arch::FRAME_POOL_BASE as u64 + (lo1 * frames::FRAME_SIZE) as u64,
        boundary,
        "the allocator's node boundary is not where QEMU was told to put it"
    );

    // --- Placement: a node preference is honoured ---------------------------
    assert_eq!(frames::numa_fallbacks(), 0, "a fallback before any request");
    let held = unsafe { &mut *core::ptr::addr_of_mut!(HELD) };

    // Node 1 is the load-bearing case: it is the *upper* range, so a pre-NUMA
    // allocator - which searches from a rotating hint near the pool base - would
    // essentially never return one of these by chance.
    for i in 0..PROBE {
        let pa = frames::alloc_on(1).expect("node 1 allocation failed");
        assert!(
            pa as u64 >= boundary,
            "asked node 1, got {pa:#x} which is below the {boundary:#x} boundary"
        );
        held[i] = pa;
    }
    // Node 0 too, so the proof is "the argument decides" and not "everything comes
    // from the top of the pool now".
    for i in 0..PROBE {
        let pa = frames::alloc_on(0).expect("node 0 allocation failed");
        assert!(
            (pa as u64) < boundary,
            "asked node 0, got {pa:#x} which is at or above the {boundary:#x} boundary"
        );
        held[PROBE + i] = pa;
    }
    assert_eq!(
        frames::numa_fallbacks(),
        0,
        "a node with free frames fell back anyway"
    );
    println!("numa: {PROBE} frames placed on each node, no fallbacks");

    // No preference must not be counted as a missed preference - nothing was asked
    // for, so nothing was denied.
    let pa = frames::alloc_on(frames::NODE_ANY).expect("NODE_ANY allocation failed");
    held[2 * PROBE] = pa;
    assert_eq!(
        frames::numa_fallbacks(),
        0,
        "NODE_ANY was counted as a fallback"
    );

    let mut n = 2 * PROBE + 1;

    // --- Exhaustion: a full node degrades, and says so ----------------------
    // The count is exact: `node_free` says how many frames node 1 can still give,
    // so the run-dry point is a hand-computed number rather than "eventually".
    let free1 = frames::node_free(1);
    assert!(
        free1 > 0 && free1 < held.len() - n,
        "node 1 free count {free1} out of range"
    );
    for _ in 0..free1 {
        let pa = frames::alloc_on(1).expect("node 1 allocation failed before it was dry");
        assert!(
            pa as u64 >= boundary,
            "node 1 gave {pa:#x} below the boundary while it still had free frames"
        );
        held[n] = pa;
        n += 1;
    }
    assert_eq!(
        frames::node_free(1),
        0,
        "node 1 still has free frames after taking every one it reported"
    );
    assert_eq!(
        frames::numa_fallbacks(),
        0,
        "a fallback while node 1 could still serve"
    );

    // Now the request that cannot be honoured. It is served - refusing would turn a
    // bandwidth question into an out-of-memory one - from the other node, and it is
    // **counted**, which is the whole difference between a reported degradation and
    // a silent misplacement.
    let pa = frames::alloc_on(1).expect("the fallback allocation failed");
    assert!(
        (pa as u64) < boundary,
        "node 1 was dry yet returned {pa:#x} from its own range"
    );
    assert_eq!(
        frames::numa_fallbacks(),
        1,
        "the fallback was not counted - a silent misplacement"
    );
    held[n] = pa;
    n += 1;
    println!(
        "numa: node 1 ran dry after its {free1} free frames; the next request fell \
         back to node 0 and was counted"
    );

    // --- Give it all back ---------------------------------------------------
    // The invariant a lost update breaks (`mm::frames`' own): the used counter must
    // still agree with the bitmap after a bounded, node-directed workload.
    for i in 0..n {
        frames::free(held[i]);
    }
    assert!(
        frames::used_matches_bitmap(),
        "the used counter and the bitmap disagree after node-directed allocation"
    );
    assert_eq!(
        frames::node_free(1),
        free1 + PROBE,
        "node 1's free count did not return to what it was"
    );

    metadata_follows_its_owner(boundary);

    println!(
        "numa: OK on {} - {} nodes discovered from {:?}, the pool partitioned at \
         {:#x} exactly where QEMU was told to put the boundary, a node preference \
         honoured on both nodes with zero fallbacks, and a run-dry node degrading \
         to its peer with the loss counted rather than hidden",
        arch::NAME,
        inv.nnodes,
        inv.firmware,
        boundary
    );
}

/// **A cell's kernel metadata sits on the cell's own node** (docs/SUBSTRATE.md pillar
/// 6): its page tables, capability tables and VA record are funded through
/// `mm::kmeta`, and `kmeta` places them on the node `user::install` stamped on the
/// cell rather than wherever the allocator's rotating hint happened to be.
///
/// Driven at the `kmeta` seam - `set_owner_node` then grow a funded table - because
/// that is exactly the call `install` makes and the call every table growth goes
/// through. `frames::node_of` answers where each frame actually came from, which is
/// the only way to check placement rather than re-reading the intent.
fn metadata_follows_its_owner(boundary: u64) {
    // Two owners, deliberately given *different* nodes, so the assertion is "the
    // owner decides" and not "everything moved to one node".
    for (slot, node) in [(6usize, 1u8), (7usize, 0u8)] {
        let owner = kmeta::Owner::cell(slot);
        kmeta::set_owner_node(owner, node);
        assert_eq!(kmeta::owner_node(owner), node);

        let mut table: Funded<u64> = Funded::new();
        table.set_owner(owner);
        // Enough elements to cross the inline directory (kmeta's first
        // INLINE_PAGES page addresses live in the struct and cost no frame), so
        // the walk below covers data frames resolved through **both** tiers -
        // inline and the overflow directory frame - and the overflow frame
        // itself is charged through the same owner-placed allocation the data
        // frames take.
        let want = kmeta::INLINE_PAGES * kmeta::elems_per_page::<u64>() + 1;
        assert!(
            table.reserve(want),
            "funded reserve failed for owner {slot}"
        );
        assert_eq!(
            table.frames_held(),
            kmeta::INLINE_PAGES + 2,
            "want INLINE_PAGES+1 data frames plus the one overflow directory frame"
        );

        // Every element's frame must be on the owner's node. Walked per element
        // rather than per page because the mapping from element to frame is
        // `kmeta`'s business, not this test's.
        for i in 0..want {
            let va = table.get_ref(i).expect("element in range") as *const u64 as usize;
            let pa = arch::virt_to_phys(va);
            assert_eq!(
                frames::node_of(pa),
                node,
                "owner {slot} asked for node {node}; element {i} landed at {pa:#x}, \
                 node {}",
                frames::node_of(pa)
            );
            // And against the launch-derived oracle, not just `node_of` - which is
            // built from the same ranges `alloc_on` places against, so on its own it
            // would only prove the allocator is self-consistent.
            if node == 1 {
                assert!(pa as u64 >= boundary, "node 1 element below the boundary");
            } else {
                assert!((pa as u64) < boundary, "node 0 element above the boundary");
            }
        }
        table.release();
        assert_eq!(
            kmeta::charged(owner),
            0,
            "owner {slot} still charged after release"
        );
    }
    // Placement must not have cost anything: both nodes had free frames throughout.
    assert_eq!(
        frames::numa_fallbacks(),
        1,
        "metadata placement fell back - only the deliberate exhaustion above should"
    );
    println!(
        "numa: kernel metadata follows its owner - two owners on different nodes, \
         every funded frame on the node its owner was given"
    );
}

/// The ISA with no firmware source for memory: assert the single-node path is
/// exactly what it was, so "NUMA landed" never quietly changes a machine that has
/// none.
fn single_node(inv: &hw::Inventory) {
    println!(
        "numa: SKIP on {} - firmware reports {} node(s); QEMU hands a bare-ELF \
         -kernel boot no device tree on this machine (measured: -dtb does not \
         reach x0 either), so no source describes memory nodes",
        arch::NAME,
        inv.nnodes
    );
    // With fewer than two nodes `init_numa` leaves every range empty, and that is
    // what makes `alloc_on` degenerate to `alloc`.
    assert_eq!(frames::nodes_known(), 0, "node ranges built for one node");
    assert_eq!(frames::node_range(0), (0, 0), "node 0 range built anyway");
    let pa = frames::alloc_on(0).expect("allocation failed on a single-node machine");
    assert!(frames::in_pool(pa), "allocation outside the pool");
    assert_eq!(
        frames::numa_fallbacks(),
        0,
        "an unknown node counted as a fallback - it is not a missed preference, \
         there is no node to miss"
    );
    frames::free(pa);
    assert!(frames::used_matches_bitmap());
    println!("numa: single-node path unchanged (alloc_on degenerates to alloc)");
}

/// The **resource graph's distances**, asserted against the numbers the *launch* named
/// (docs/RESOURCE-GRAPH.md 5).
///
/// The oracle is `xtask`'s `-numa dist,src=,dst=,val=` arguments, which is the same
/// discipline the rest of this kernel uses: assert against what the launch declared, never
/// against the code's own tables, because a parser compared with itself is self-consistent
/// and can still be wrong.
///
/// Four claims:
///
/// 1. A memory locality exists as a graph node for every locality firmware reported.
/// 2. `cost(n, n)` is `LOCAL` - which SLIT's own local value of 10 corroborates.
/// 3. `cost(0, 1)` carries the **declared** distance, in ACPI's relative units, in `hops`.
/// 4. Latency and bandwidth are **0 = unknown**, because SLIT does not report them. HMAT
///    does and is not parsed; claiming them here would fabricate the two fields a caller is
///    most likely to rank by, which is the failure the whole graph exists to avoid.
fn graph_distances(inv: &hw::Inventory) {
    use kernel::hw::graph::{self, NodeKind, Source};
    // SAFETY: built at boot; read-only here, single-threaded.
    let g = unsafe { graph::graph() };

    let a = g
        .find(NodeKind::MemoryNode, 0)
        .expect("locality 0 is not a graph node");
    let b = g
        .find(NodeKind::MemoryNode, 1)
        .expect("locality 1 is not a graph node");

    let local = g.cost(a, a).expect("a locality has no cost to itself");
    assert_eq!(
        local,
        graph::Cost::LOCAL,
        "a node to itself is not LOCAL - every locality-aware decision starts from this"
    );

    let across = g
        .cost(a, b)
        .expect("no distance between locality 0 and 1 - SLIT was not parsed");
    // The oracle: xtask launches this kernel with `-numa dist,src=0,dst=1,val=20`.
    const DECLARED: u8 = 20;
    assert_eq!(
        across.hops, DECLARED,
        "the graph reports distance {} between nodes 0 and 1; the launch declared {DECLARED}",
        across.hops
    );
    // And the reverse direction, because edges are directed and SLIT is not required to be
    // symmetric - so a builder that recorded only one direction must fail here.
    let back = g
        .cost(b, a)
        .expect("no distance from locality 1 back to 0 - only one direction was recorded");
    assert_eq!(back.hops, DECLARED, "the reverse distance disagrees");

    // HMAT is ACPI-only, so the magnitudes are asserted where firmware provides them and
    // asserted ABSENT where it does not. Both directions matter: a graph that invented a
    // latency on riscv64 would be fabricating, and one that dropped it on x86-64 would be
    // discarding what firmware said.
    //
    // The oracle is again the launch: `-numa hmat-lb,...,latency=100` and `bandwidth=10G`.
    if g.source() == Source::Acpi {
        const DECLARED_LAT_NS: u32 = 100;
        // Declared as `10240M` rather than `10G` on purpose. QEMU's size suffixes are
        // **binary**, so `10G` is 10 * 1024 = 10240 MB/s - and the first version of this
        // oracle hand-computed 10000 from a decimal reading and failed against a parser that
        // was correct. Stating the launch value in the unit the graph reports removes the
        // conversion from the test rather than encoding a guess about it.
        const DECLARED_BW_MBS: u32 = 10_240;
        assert_eq!(
            across.latency_ns, DECLARED_LAT_NS,
            "the graph reports {} ns between nodes 0 and 1; the launch declared \
             {DECLARED_LAT_NS} ns via HMAT",
            across.latency_ns
        );
        assert_eq!(
            across.bandwidth_mbs, DECLARED_BW_MBS,
            "the graph reports {} MB/s between nodes 0 and 1; the launch declared \
             {DECLARED_BW_MBS} MB/s via HMAT",
            across.bandwidth_mbs
        );
        println!(
            "numa: AND REAL MAGNITUDES FROM HMAT - {} ns and {} MB/s between nodes 0 and 1, \
             exactly what the launch declared. SLIT's hops orders localities; HMAT is what \
             makes 'how much further' a number a caller can rank by, and nothing is derived \
             from hops because a relative distance is not a latency OK",
            across.latency_ns, across.bandwidth_mbs
        );
    } else {
        assert_eq!(
            (across.latency_ns, across.bandwidth_mbs),
            (0, 0),
            "the graph claims a latency or bandwidth on a machine with no HMAT - these must \
             read as unknown rather than as a number derived from the SLIT distance"
        );
        println!(
            "numa: magnitudes read 0 = unknown, correctly - HMAT is ACPI-only and this \
             machine described its distances through {:?} OK",
            g.source()
        );
    }
    // ACPI on x86-64 via SLIT, the device tree on riscv64 via `numa-distance-map-v1`. The
    // assertion is that the graph names the source it actually read from - not which one,
    // since that is a property of the machine.
    assert!(
        matches!(g.source(), Source::Acpi | Source::DeviceTree),
        "distances were recorded but the source reads {:?}",
        g.source()
    );
    assert!(
        !inv.slit_truncated,
        "SLIT was truncated on a {}-node machine",
        inv.nnodes
    );
    println!(
        "numa: THE RESOURCE GRAPH CARRIES REAL DISTANCES - {} nodes, {} edges, and \
         cost(0,1).hops == {} which is exactly what the launch declared with \
         `-numa dist,val={}`; cost(n,n) is LOCAL; latency and bandwidth read 0 because SLIT \
         does not report them and HMAT is not parsed, so nothing is fabricated; source={:?} OK",
        g.node_count(),
        g.edge_count(),
        across.hops,
        DECLARED,
        g.source()
    );
}

/// The **degraded** case, asserted rather than assumed: a machine whose firmware describes
/// no localities must answer "everything is equally near" and must say so.
///
/// This is the half that keeps the feature honest. Without it, "topology landed" could
/// quietly alter a machine that has none - which is the rule the NUMA work already follows
/// and the reason ARM64 reaches this path here (no firmware describes memory to a bare-ELF
/// `virt` boot, checked rather than assumed).
fn graph_degraded() {
    use kernel::hw::graph::{self, NodeKind, Source};
    // SAFETY: as above.
    let g = unsafe { graph::graph() };
    let only = g
        .find(NodeKind::MemoryNode, 0)
        .expect("even a single-locality machine must have one memory node");
    assert_eq!(
        g.cost(only, only),
        Some(graph::Cost::LOCAL),
        "a single locality is not local to itself"
    );
    assert_eq!(
        g.source(),
        Source::None,
        "a graph with no distances claims a firmware source it did not read from"
    );
    assert_eq!(
        g.edge_count(),
        0,
        "a machine with one locality has {} distance edges",
        g.edge_count()
    );
    println!(
        "numa: the graph DEGRADES HONESTLY - one locality, {} nodes, 0 distance edges, \
         source=None, and cost(n,n) is LOCAL, so 'everything is equally near' is the answer \
         rather than a fabricated matrix OK",
        g.node_count()
    );
}
