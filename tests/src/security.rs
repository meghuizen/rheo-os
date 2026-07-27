//! In-QEMU test kernel: the three critical defects an architecture audit found
//! in the syscall surface, each attempted by a **real unprivileged U-mode cell**
//! and each asserted to be cleanly refused (docs/ENGINEERING.md 12).
//!
//! Every phase's evidence is something the cell under test cannot manufacture
//! (docs/ENGINEERING.md 1):
//!
//! **F1 - arbitrary kernel write.** The kernel plants a magic word in a
//! supervisor static no cell has a mapping for, hands the cell that kernel VA,
//! and the cell calls `SYS_QUEUE_INFO(out_va = <that VA>)`. Asserted: the
//! syscall is refused *and* the canary still holds its magic. Null, unaligned,
//! out-of-range and wrapping addresses follow. The control phase repeats the
//! call with the cell's own page and asserts it still reports the right queue
//! VA, so the check is a bound and not a break.
//!
//! **F2 - unbounded allocation.** The cell asks for `SYS_MMAP(1 << 40)` -
//! 268 million pages out of a 32768-frame pool. Asserted: refused, the cell
//! survives, and the pool's free count is **unchanged** (the hand-computed
//! oracle is a delta of exactly 0). One page over the per-cell budget is
//! refused too, and a legitimate two-page round trip still works.
//!
//! **F3 - `munmap` of frames the cell does not own.** The cell munmaps its own
//! queue-pair region, then submits an `OP_NOP` and reaps it: asserted refused,
//! ring still serving. It munmaps a kernel VA, its own `.user` stack, and the
//! channel / loaded-queue / unreserved-grant region bases: all refused, the
//! pool's free count unchanged, the cell still alive to report. Before the fix
//! each of those handed `frames::free` a frame the cell did not own - for the
//! `.user` window, a frame out of the kernel image, which trips that function's
//! range assertion: a kernel panic from unprivileged code.

#![no_std]
#![no_main]

#[path = "harness.rs"]
mod harness;

use harness::{CellStore, KernelStack, build_cell};
use kernel::capability::{CapTable, ObjectTable};
use kernel::mm::frames;
use kernel::queue::STATUS_OK;
use kernel::user::{self, Outcome};
use kernel::user_progs::{
    user_attack_mmap, user_attack_mmap_roundtrip, user_attack_munmap, user_attack_munmap_queue,
    user_attack_out,
};
use kernel::{arch, println};

#[unsafe(link_section = ".user.bss")]
static mut STORE: CellStore = CellStore::new();
static mut KSTACK: KernelStack = KernelStack::new();
static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();

/// The F1 target: an ordinary supervisor static in kernel `.bss` that no cell
/// has any mapping for. Two words, so the 16-byte `QueueInfo` an unchecked
/// `SYS_QUEUE_INFO` writes lands wholly inside it (before the fix it did, and
/// clobbered both).
static mut CANARY: [u64; 2] = [CANARY_MAGIC, CANARY_MAGIC];
const CANARY_MAGIC: u64 = 0x5EC0_0DED_5EC0_0DED;

fn canary() -> [u64; 2] {
    // SAFETY: single-threaded kernel, read between cell runs.
    unsafe { *core::ptr::addr_of!(CANARY) }
}

/// What one attack run reported back through its `Params`, plus two facts the
/// cell cannot influence: how many frames were still charged to it when it
/// exited, and how many frames the whole run cost the pool.
struct Report {
    outcome: Outcome,
    ticks: u64,
    ops: u64,
    status: u64,
    /// `user::cell_frames_charged(0)` read immediately after the run.
    charged: usize,
    /// Frames the pool lost across build + run. A fresh cell always costs a few
    /// (its address space's page tables, which teardown does not reclaim), so
    /// this is compared against the cost of a run that allocates nothing -
    /// `BASELINE` below - rather than against zero.
    pool_delta: usize,
}

/// Build a fresh attacker cell around `entry`, run it, and return what it wrote
/// into its `Params`. `target` reaches the cell as `Params.iters`.
fn attack(entry: extern "C" fn(usize) -> !, target: u64) -> Report {
    // SAFETY: single-threaded kernel; each run completes before the next, and
    // STORE is reused with a fresh address space each time.
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS);
        let store_ptr = core::ptr::addr_of_mut!(STORE);
        let kernel_sp = (*core::ptr::addr_of!(KSTACK)).top();

        let (free_before, _) = frames::stats();
        let (aspace, _obj, mut frame) = build_cell(
            &mut *store_ptr,
            objects,
            caps,
            kernel_sp,
            1,
            entry,
            0,
            target,
        );
        let qp = (*store_ptr).qp.qp.as_ptr();

        user::reset();
        user::install(
            0,
            &aspace,
            caps,
            objects,
            qp,
            core::ptr::addr_of_mut!(frame),
        );
        // Give the cell a queue-info record so `SYS_QUEUE_INFO` has something
        // real to report - otherwise it would refuse for the wrong reason and
        // the F1 phase would pass vacuously.
        let params = (*store_ptr).params;
        user::set_queue_info(0, params.qp_addr, params.cap_id as u32);

        let (_idx, outcome) = user::run(0);
        let charged = user::cell_frames_charged(0);
        // The O(1) free count is only trustworthy if it still agrees with the
        // bitmap it summarises; every phase below reads it, so check the
        // invariant here rather than trusting it (docs/ENGINEERING.md 1).
        assert!(
            frames::used_matches_bitmap(),
            "the frame allocator's used counter diverged from its bitmap"
        );
        let (free_after, _) = frames::stats();
        let p = (*store_ptr).params;
        Report {
            outcome,
            ticks: p.ticks,
            ops: p.ops,
            status: p.status,
            charged,
            pool_delta: free_before.saturating_sub(free_after),
        }
    }
}

fn assert_exited(r: &Report, what: &str) {
    match r.outcome {
        Outcome::Exited(0) => {}
        other => panic!("{what}: cell did not exit cleanly: {other:?}"),
    }
}

/// The user VA of the cell's `Params.ticks` field. A `.user`-window cell is
/// identity-mapped (kernel VA == user VA), so the kernel can name it.
fn own_out_va() -> u64 {
    // SAFETY: address-of only, between runs.
    unsafe { core::ptr::addr_of!(STORE.params.ticks) as u64 }
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("security: start on {}", arch::NAME);

    let canary_va = core::ptr::addr_of!(CANARY) as u64;
    println!("security: canary at {canary_va:#x} (kernel .bss, unmapped in every cell)");

    // ---------------------------------------------------------------- F1
    // (a) An out-parameter pointing into kernel memory must be refused, and the
    //     canary must still hold its magic.
    let r = attack(user_attack_out, canary_va);
    assert_exited(&r, "F1 kernel-VA out-parameter");
    assert_eq!(
        r.status,
        u64::MAX,
        "F1: SYS_QUEUE_INFO with a kernel out_va returned {} - it must be refused",
        r.status
    );
    assert_eq!(
        canary(),
        [CANARY_MAGIC, CANARY_MAGIC],
        "F1: the kernel canary was overwritten through a cell-supplied address"
    );
    println!("security: F1 kernel-VA out-parameter refused, canary intact OK");

    // (b) Null, unaligned, just past the user range, and wrapping.
    for (name, va) in [
        ("null", 0u64),
        ("unaligned", canary_va + 1),
        ("at USER_VA_MAX", user::USER_VA_MAX),
        ("wrapping", u64::MAX - 3),
    ] {
        let r = attack(user_attack_out, va);
        assert_exited(&r, name);
        assert_eq!(
            r.status,
            u64::MAX,
            "F1: a {name} out_va ({va:#x}) was accepted"
        );
    }
    assert_eq!(canary(), [CANARY_MAGIC, CANARY_MAGIC], "F1: canary damaged");
    println!("security: F1 null / unaligned / out-of-range / wrapping out_va refused OK");

    // (c) Control: the same syscall with the cell's own page must still work,
    //     and report the real queue VA. A bound that breaks the legitimate path
    //     is not a fix.
    let out_va = own_out_va();
    let r = attack(user_attack_out, out_va);
    assert_exited(&r, "F1 control");
    assert_eq!(r.status, 0, "F1 control: a legitimate out_va was refused");
    // SAFETY: read between runs; the cell's own params record its queue VA.
    let want_qp = unsafe { (*core::ptr::addr_of!(STORE)).params.qp_addr };
    assert_eq!(
        r.ticks, want_qp,
        "F1 control: SYS_QUEUE_INFO reported qp_va {:#x}, expected {want_qp:#x}",
        r.ticks
    );
    println!("security: F1 control - a legitimate out_va still reports qp_va OK");

    // The cost of a run that allocates nothing for the cell: one fresh address
    // space's page tables (teardown does not reclaim intermediate tables, a
    // documented bounded leak - docs/LINUX-COMPAT.md L6). Every refused
    // allocation below must cost exactly this and no more, which is a tighter
    // oracle than "the pool did not shrink".
    let baseline = r.pool_delta;
    assert_eq!(
        r.charged, 0,
        "a cell that allocated nothing was charged frames"
    );
    println!("security: baseline cost of one cell = {baseline} page-table frames");

    // ---------------------------------------------------------------- F2
    let r = attack(user_attack_mmap, 1 << 40);
    assert_exited(&r, "F2 huge mmap");
    assert_eq!(
        r.ticks, 0,
        "F2: SYS_MMAP(1 << 40) returned {:#x} - it must be refused",
        r.ticks
    );
    assert_eq!(
        r.pool_delta, baseline,
        "F2: a refused mmap of 2^40 bytes cost {} frames, expected the {baseline}-frame baseline",
        r.pool_delta
    );
    assert_eq!(r.charged, 0, "F2: a refused mmap left frames charged");
    let (free_now, total) = frames::stats();
    println!("security: F2 SYS_MMAP(1<<40) refused at zero frame cost, pool {free_now}/{total} OK");

    // One page over the per-cell budget must also be refused - and the budget
    // must be smaller than the pool, so this exercises the budget, not the pool
    // (checked at compile time: both are constants).
    const _: () = assert!(user::MAX_FRAMES_PER_CELL < frames::POOL_FRAMES);
    let over = ((user::MAX_FRAMES_PER_CELL + 1) * frames::FRAME_SIZE) as u64;
    let r = attack(user_attack_mmap, over);
    assert_exited(&r, "F2 over-budget mmap");
    assert_eq!(
        r.ticks, 0,
        "F2: a request one page over the per-cell budget was granted"
    );
    assert_eq!(
        r.pool_delta, baseline,
        "F2: an over-budget mmap cost {} frames, expected the {baseline}-frame baseline",
        r.pool_delta
    );
    assert_eq!(r.charged, 0, "F2: an over-budget mmap left frames charged");
    println!(
        "security: F2 over-budget request ({} pages > {} budget) refused OK",
        user::MAX_FRAMES_PER_CELL + 1,
        user::MAX_FRAMES_PER_CELL
    );

    // ---------------------------------------------------------------- F3
    // (a) The legitimate anon round trip must still work: this is the path
    //     librheo's `mem::Grant`/`Mapping` drop uses, and the frame-leak fix it
    //     brought in.
    let r = attack(user_attack_mmap_roundtrip, 0);
    assert_exited(&r, "F3 anon round trip");
    assert!(r.ticks != 0, "F3: a legitimate 8 KiB SYS_MMAP was refused");
    assert_eq!(r.status, 1, "F3: the mapped page did not read back");
    assert_eq!(
        r.ops, 0,
        "F3: SYS_MUNMAP of the cell's own anon mmap was refused ({})",
        r.ops
    );
    // The two data frames went back: nothing is still charged to the cell. The
    // pool cost is the baseline plus the leaf page tables the 12 GiB mmap region
    // needed, which teardown does not reclaim - so this asserts the *data*
    // frames were returned, not that the run was free.
    assert_eq!(
        r.charged, 0,
        "F3: {} frames stayed charged after the round trip - munmap did not return them",
        r.charged
    );
    println!(
        "security: F3 legitimate anon mmap / write / munmap round trip OK (cost {} frames, 0 charged)",
        r.pool_delta
    );

    // (b) The cell's own queue-pair region is not its to free - the kernel holds
    //     a raw `QueuePair` overlay onto it. Refused, and the ring still serves.
    // SAFETY: address-of only, between runs.
    let qp_region = unsafe { core::ptr::addr_of!(STORE.region) as u64 };
    let r = attack(user_attack_munmap_queue, qp_region);
    assert_exited(&r, "F3 queue-region munmap");
    assert_eq!(
        r.ticks,
        u64::MAX,
        "F3: SYS_MUNMAP of the cell's own queue region returned {} - it must be refused",
        r.ticks
    );
    assert_eq!(
        r.status, 1,
        "F3: the queue ring stopped completing after the attempt"
    );
    assert_eq!(
        r.ops, STATUS_OK as u64,
        "F3: the OP_NOP after the attempt completed with status {}",
        r.ops
    );
    assert_eq!(
        r.pool_delta, baseline,
        "F3: the refused queue-region munmap changed the pool cost to {} (baseline {baseline})",
        r.pool_delta
    );
    println!("security: F3 queue-region munmap refused, ring still serving OK");

    // (c) A kernel VA, and the cell's own `.user` window stack - neither is a
    //     frame the allocator owns, so before the fix both reached
    //     `frames::free` with a kernel-image address and panicked the kernel.
    // SAFETY: address-of only, between runs.
    let own_stack = unsafe { core::ptr::addr_of!(STORE.stack) as u64 };
    for (name, va) in [("kernel VA", canary_va), ("own .user stack", own_stack)] {
        let r = attack(user_attack_munmap, va);
        assert_exited(&r, name);
        assert_eq!(
            r.ticks,
            u64::MAX,
            "F3: SYS_MUNMAP of a {name} ({va:#x}) returned {} - it must be refused",
            r.ticks
        );
        assert_eq!(
            r.pool_delta, baseline,
            "F3: a refused munmap of a {name} moved the pool cost to {} (baseline {baseline})",
            r.pool_delta
        );
    }
    assert_eq!(canary(), [CANARY_MAGIC, CANARY_MAGIC], "F3: canary damaged");
    println!("security: F3 kernel-VA and own-.user-window munmap refused OK");

    // (d) The cross-cell channel, a loaded cell's queue region and an
    //     unreserved grant VA: refused by rule, not merely by absence.
    for (name, va) in [
        ("channel slot 0", kernel::load::USER_CHANNEL_VA as u64),
        ("loaded-cell queue", kernel::load::USER_QUEUE_VA as u64),
        ("unreserved grant", 0x8_0000_0000u64),
    ] {
        let r = attack(user_attack_munmap, va);
        assert_exited(&r, name);
        assert_eq!(
            r.ticks,
            u64::MAX,
            "F3: SYS_MUNMAP of the {name} region ({va:#x}) was accepted"
        );
    }
    println!("security: F3 channel / loaded-queue / unreserved-grant munmap refused OK");

    println!("security: PASS");
    arch::exit(arch::ExitCode::Success)
}
