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
        println!("numa: PASS");
        arch::exit(arch::ExitCode::Success)
    }
    numa_two_nodes(inv);
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
