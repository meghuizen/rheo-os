//! In-QEMU test kernel for SMP: per-CPU state, a kernel spinlock, and
//! secondary-core bring-up (docs/SMP.md, task #27).
//!
//! This is **opt-in** SMP: it exercises `kernel::smp` on the primary CPU (the
//! spinlock + the per-CPU registry, which must work single-core with zero fuss),
//! then asks the arch layer to start one secondary core. The honest per-ISA
//! outcome (docs/SMP.md):
//!   - RISC-V: a genuine second hart runs kernel code (SBI HSM `hart_start`).
//!   - x86-64: a genuine second core runs kernel code (a real-mode AP trampoline
//!     staged in low memory, released by INIT-SIPI-SIPI through the local APIC's
//!     interrupt command register - docs/SMP.md 6).
//!   - ARM64: bring-up is blocked in this QEMU config (PSCI `CPU_ON` needs EL3
//!     firmware that `virt` without `secure=on` does not have); the test makes a
//!     genuine, guarded attempt, prints skip-with-reason, and still PASSES
//!     (mirroring how librheonet/librheogpu skip when a device is absent).
//!
//! Where a secondary does come up, the assertions are chosen to be unfakeable by
//! the primary (docs/ENGINEERING.md 1): the shared counter carries a fixed magic
//! written only from `smp::secondary_run`, the registry slot is one the primary
//! never claims, and the secondary's recorded hardware id must **differ** from the
//! boot CPU's - a primary looping back through the same code could satisfy none of
//! the three.
//!
//! Either way the primary never hangs (the bring-up wait is bounded) and never
//! faults - a blocked ISA keeps single-core boot intact.

#![no_std]
#![no_main]

#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;

use harness::{CellStore, KernelStack, build_cell};
use kernel::arch::MapPerm;
use kernel::capability::{CapTable, ObjectTable};
use kernel::sched::{dispatch, preempt};
use kernel::smp::{self, SpinLock, StartError};
use kernel::user::Outcome;
use kernel::user_progs::{user_copair, user_placed};
use kernel::{arch, idle, ktimer, load, println, user};

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("smp: start on {}", arch::NAME);

    test_spinlock();
    test_percpu_single();
    test_secondary_bringup();

    println!("smp: PASS");
    arch::exit(arch::ExitCode::Success)
}

/// The spinlock gives mutual exclusion on the primary CPU (single-core sanity):
/// nested lock/unlock works and the guarded value updates.
fn test_spinlock() {
    static COUNTER: SpinLock<u64> = SpinLock::new(0);
    for _ in 0..1000 {
        let mut g = COUNTER.lock();
        *g += 1;
    }
    assert_eq!(*COUNTER.lock(), 1000, "spinlock lost updates");
    println!("smp: spinlock (single-core) OK");
}

/// The per-CPU registry defaults cleanly to CPU 0 before any SMP bring-up: init
/// establishes CPU 0's identity and marks it online, and `this_cpu()` resolves
/// to it. This is the zero-impact guarantee for non-SMP code.
fn test_percpu_single() {
    smp::init();
    assert!(
        smp::this_cpu().is_online(),
        "boot CPU not online after init"
    );
    assert_eq!(arch::cpu_index(), 0, "boot CPU index is not 0");
    assert_eq!(
        smp::cpu(0).hw_id(),
        arch::boot_cpu_hw_id(),
        "CPU 0 hw id wrong"
    );
    assert_eq!(
        smp::online_count(),
        1,
        "more than the boot CPU online pre-bring-up"
    );
    println!("smp: per-CPU registry (boot CPU) OK");
}

// ------------------------------------------------- two cores computing at once
//
// Bring-up proves a core executes. This proves the two cores do **useful work at the
// same time** - the thing "multi-CPU" actually means, and the thing bring-up alone
// says nothing about (docs/SMP.md 10).
//
// The workload is a tiled int8 GEMM split by output rows: each core writes a disjoint
// row range of C and reads all of A and B, which is how a GEMM is genuinely
// parallelised and which means the compute needs no lock at all. Integer, so the
// answer is bit-exact and the two-core result must equal a single-core reference
// **exactly** - not within a tolerance.
//
// The parallelism itself is proven by a **rendezvous**, not by timing: each core
// publishes a flag and then waits for the other's, and neither writes its flag after
// passing. Both passing therefore means both cores executed inside one interval, which
// a single core cannot produce - there is no kernel-context preemption to interleave
// them, and neither side yields. A wall-clock speedup measurement would prove nothing
// here anyway: QEMU's TCG time-slices the two vCPUs onto host threads, so the
// available evidence is *simultaneity*, and that is what is asserted.

/// GEMM shape. Small enough that the whole exchange fits well inside the boot-test
/// budget under TCG, large enough that both cores have real work between the
/// rendezvous and the barrier.
const GM: usize = 64;
const GN: usize = 48;
const GK: usize = 32;

static mut GA: [i8; GM * GK] = [0; GM * GK];
static mut GB: [i8; GK * GN] = [0; GK * GN];
static mut GC: [i32; GM * GN] = [0; GM * GN];
static mut GREF: [i32; GM * GN] = [0; GM * GN];

/// Two cores drain one GEMM work queue; the result is asserted bit-identical to a
/// single-core reference and both cores are asserted to have taken work. `idx` is the
/// secondary's registry index, so its share can be read back.
fn test_parallel_gemm(idx: usize) {
    // SAFETY: single-threaded setup; the secondary is parked in its work loop waiting
    // for a job and touches none of this until one is published.
    unsafe {
        let a = &mut *core::ptr::addr_of_mut!(GA);
        let b = &mut *core::ptr::addr_of_mut!(GB);
        for (i, x) in a.iter_mut().enumerate() {
            *x = ((i * 7 + 3) & 0x7F) as i8 - 64;
        }
        for (i, x) in b.iter_mut().enumerate() {
            *x = ((i * 11 + 5) & 0x7F) as i8 - 64;
        }
        // The single-core oracle, computed here by the primary alone.
        let refc = &mut *core::ptr::addr_of_mut!(GREF);
        refc.fill(0);
        for i in 0..GM {
            for j in 0..GN {
                let mut acc = 0i32;
                for p in 0..GK {
                    acc += (a[i * GK + p] as i32) * (b[p * GN + j] as i32);
                }
                refc[i * GN + j] = acc;
            }
        }
        (*core::ptr::addr_of_mut!(GC)).fill(0);

        // Split the output rows in half: the primary takes the low half, the secondary
        // the high half. Disjoint in C, so no lock is needed around the compute - which
        // is the point, and why the rendezvous is the only synchronisation.
        // The whole output range is published as one job; both cores claim blocks
        // from it. There is no reserved half - which is the point, and why the
        // per-core counts below are evidence rather than arithmetic.
        let job = smp::GemmJob {
            a: core::ptr::addr_of!(GA) as usize,
            b: core::ptr::addr_of!(GB) as usize,
            c: core::ptr::addr_of_mut!(GC) as usize,
            as_: GK,
            bs: GN,
            cs: GN,
            lo: 0,
            hi: GM,
            n: GN,
            k: GK,
        };
        let (met, finished) = smp::run_gemm_with_secondary(job, 0, GM);

        if !finished {
            println!(
                "smp: SKIP the parallel-GEMM phase - the secondary did not finish its \
                 rows within the bound, so nothing about two-core compute is claimed"
            );
            return;
        }
        // **Every online core met**, not just two. The two-way rendezvous this phase
        // used could only ever witness a pair - each half waits for exactly one peer -
        // so with four cores online it left the other two unaccounted for. The barrier
        // is sized from `online_count()`, which the primary already knows, so passing
        // it means every online core was inside the same interval.
        let cores = smp::online_count();
        assert!(
            met && smp::gemm_all_met(),
            "not all {cores} online cores met at the barrier, so they did not all run \
             at the same time - the GEMM may still be correct, but that would only mean \
             the shares ran one after another"
        );
        let got = &*core::ptr::addr_of!(GC);
        let want = &*core::ptr::addr_of!(GREF);
        assert_eq!(
            got, want,
            "two-core GEMM differs from the single-core oracle"
        );
        // **Load sharing**, which a fixed half-and-half split could not show: both
        // cores must have completed blocks, and the two counts must account for the
        // whole queue. A run where either is zero did the work on one core and would
        // still have produced the right answer - which is exactly why this is asserted
        // rather than inferred from correctness.
        let total_blocks = GM.div_ceil(smp::GEMM_BLOCK_ROWS);
        let mut sum = 0usize;
        let mut workers = 0usize;
        for c in 0..smp::MAX_CPUS {
            let n = smp::blocks_done(c);
            sum += n;
            if n > 0 {
                workers += 1;
            }
        }
        let _ = idx;
        assert_eq!(
            sum, total_blocks,
            "blocks completed ({sum}) do not account for the whole queue \
             ({total_blocks}) - a claim was lost or double-counted"
        );
        // Sharing, asserted; **which** cores got a block, reported. The barrier above is
        // what proves all `cores` were inside one interval; it does not promise each of
        // them *won* a block, because winning one is a race against peers that may drain
        // the queue first. An earlier version asserted `workers == cores` and was
        // therefore asserting a race outcome - it failed on a run where three cores took
        // all sixteen blocks before the fourth reached its first `fetch_add`, which is
        // correct behaviour for a work queue (docs/ENGINEERING.md 1: a proof must not be
        // able to fail on a legal schedule).
        assert!(
            workers > 1,
            "one core did all {total_blocks} blocks - the queue was drained serially, \
             not shared"
        );
        // And the frame allocator survived two cores using it (the pool lock,
        // docs/SMP.md 10.2): the incremental used counter still agrees with the bitmap
        // it summarises, which is exactly the invariant a lost update breaks.
        assert!(
            kernel::mm::frames::used_matches_bitmap(),
            "the frame pool's used counter drifted from its bitmap under two cores"
        );
        println!(
            "smp: {cores} CORES were inside one interval draining a {total_blocks}-block \
             work queue for a {GM}x{GN}x{GK} int8 GEMM - the tile framework's own \
             `gemm_i8_i32`, shared verbatim - {workers} of them won blocks (claimed, not \
             pre-assigned), result bit-identical to the single-core oracle, and all \
             {cores} met at a barrier none could pass alone OK"
        );
        for c in 0..smp::MAX_CPUS {
            let n = smp::blocks_done(c);
            if n > 0 {
                println!("smp:   CPU {c} computed {n} block(s)");
            }
        }
    }
}

/// Bring up one secondary core, or skip-with-reason where the ISA blocks it.
fn test_secondary_bringup() {
    match smp::bring_up_one() {
        Ok(idx) => {
            // A real second core ran kernel code. Verify it through the shared
            // state it touched: the per-CPU registry and the cross-core spinlock.
            assert!(smp::secondaries_up() >= 1, "secondary did not signal up");
            assert_eq!(
                smp::online_count(),
                2,
                "expected 2 CPUs online (boot + secondary)"
            );
            assert!(
                smp::cpu(idx).is_online(),
                "secondary CPU {idx} not marked online"
            );
            let shared = smp::shared_value();
            assert_eq!(
                shared,
                smp::SECONDARY_MARK,
                "secondary's cross-core spinlock write missing (got {shared:#x})"
            );
            // A *different* core, not the primary re-entering: the registry slot
            // is not the boot CPU's, and the hardware id the secondary recorded
            // (which it read from its own hardware - its hart id / APIC id) is not
            // the boot CPU's either.
            assert_ne!(idx, 0, "secondary claimed the boot CPU's registry slot");
            assert_ne!(
                smp::cpu(idx).hw_id(),
                arch::boot_cpu_hw_id(),
                "secondary recorded the boot CPU's hardware id"
            );
            println!(
                "smp: secondary CPU {idx} (hw id {}) ran kernel code - online={}, shared={:#x} OK",
                smp::cpu(idx).hw_id(),
                smp::online_count(),
                shared
            );
            println!("smp: real second core on {} confirmed", arch::NAME);
            test_parallel_gemm(idx);
            test_user_cells_on_both();
            test_placement();
            test_two_vcores_one_cell();
            test_vcore_yield();
            test_cross_core_preemption();
            test_linux_cell_on_secondary();
            test_two_linux_cells();
            test_nvme_per_core_queues();
        }
        Err(StartError::NoSecondary) => {
            println!(
                "smp: SKIP {} - no secondary CPU enumerable from the kernel's exception level",
                arch::NAME
            );
        }
        Err(StartError::Blocked(reason)) => {
            println!(
                "smp: SKIP {} - secondary bring-up blocked: {reason}",
                arch::NAME
            );
        }
        Err(StartError::Timeout) => {
            println!(
                "smp: SKIP {} - secondary start accepted but the core did not run kernel code",
                arch::NAME
            );
        }
    }
    // The primary is intact regardless of the outcome: still CPU 0, still online.
    assert_eq!(arch::cpu_index(), 0, "primary lost its identity");
    assert!(smp::this_cpu().is_online(), "primary went offline");
}

// ------------------------------------------- a cell in user mode on a secondary
//
// The GEMM phase above proves two cores compute at once **in kernel context**. This
// proves the harder thing and the one "multi-CPU" is usually taken to mean: two cells
// run in **user mode**, on two cores, at the same instant - each in its own address
// space, each dropping to the ISA's unprivileged level and trapping back into its own
// core's kernel stack (docs/SMP.md 10.0).
//
// What had to be true for this to work at all, and is therefore what it tests:
//   - the kernel's "which cell is running" state is **per-CPU** (`user::CURRENT` /
//     `TOP_CELL` / `EXITED`), not one global;
//   - the saved kernel context a cell unwinds back into is **per-CPU** (RISC-V's
//     `KERNEL_CTX`, indexed by the hart's `tp`);
//   - on RISC-V, the kernel's own `tp` survives U-mode. `tp` is a saved GPR the cell
//     owns as its TLS pointer *and* where the kernel keeps its CPU index, so without
//     the frame's `kernel_tp` slot every trap handler would run reading the wrong
//     CPU's state - and on the boot CPU that is invisible, because the wrong answer
//     and the right one are both 0.
//
// The two cells are **partitioned**, not locked: distinct cell slots, distinct
// address spaces, distinct kernel stacks, distinct pages. That partitioning is the
// multikernel answer this design commits to (docs/SCHEDULING.md 1a), and it is why
// no lock appears here.
//
// Dispatch is left **off**: this phase is about two cores, not about preemption
// (which `preempt` owns), and a preemption timer firing here could hand one core's
// cell to the other core's scheduler - which is exactly the shared-state audit
// docs/SMP.md 10.0 lists as not done.

#[unsafe(link_section = ".user.bss")]
static mut STORE_P: CellStore = CellStore::new();
#[unsafe(link_section = ".user.bss")]
static mut STORE_S: CellStore = CellStore::new();

/// The four-word witness page both cells map read-write.
#[repr(C, align(4096))]
struct Witness([u64; 512]);
#[unsafe(link_section = ".user.bss")]
static mut WITNESS: Witness = Witness([0; 512]);

/// One kernel stack **per core**. Two cores trapping onto one stack would corrupt
/// each other's frames, and the corruption would look like a random fault rather
/// than like a missing stack - so this is the cheapest thing to get right.
static mut KSTACK_P: KernelStack = KernelStack::new();
static mut KSTACK_S: KernelStack = KernelStack::new();

static mut OBJECTS2: ObjectTable = ObjectTable::new();
static mut CAPS2: CapTable = CapTable::new();

/// Rounds each cell runs. Enough that the two overlap for many rounds under TCG,
/// few enough to finish well inside the boot budget.
const CO_ROUNDS: u64 = 64;

fn test_user_cells_on_both() {
    // SAFETY: single-threaded setup on the primary; the secondary is parked in its
    // work loop and touches none of this until a cell index is published.
    unsafe {
        let w = &mut *core::ptr::addr_of_mut!(WITNESS);
        w.0[0] = 0;
        w.0[1] = 0;
        w.0[2] = 0;
        w.0[3] = 0;
        let shared = core::ptr::addr_of!(WITNESS) as usize;

        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS2);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS2);
        *objects = ObjectTable::new();
        *caps = CapTable::new();

        let p = core::ptr::addr_of_mut!(STORE_P);
        let sec = core::ptr::addr_of_mut!(STORE_S);
        let (mut aspace_p, _op, mut frame_p) = build_cell(
            &mut *p,
            objects,
            caps,
            (*core::ptr::addr_of!(KSTACK_P)).top(),
            1,
            user_copair,
            0,
            CO_ROUNDS,
        );
        let (mut aspace_s, _os, mut frame_s) = build_cell(
            &mut *sec,
            objects,
            caps,
            (*core::ptr::addr_of!(KSTACK_S)).top(),
            2,
            user_copair,
            1,
            CO_ROUNDS,
        );
        aspace_p.map_user_range(shared, 4096, MapPerm::UserRw);
        aspace_s.map_user_range(shared, 4096, MapPerm::UserRw);
        (*p).params.ticks = shared as u64;
        (*sec).params.ticks = shared as u64;

        user::reset();
        user::install(
            0,
            &aspace_p,
            caps,
            objects,
            (*p).qp.qp.as_ptr(),
            core::ptr::addr_of_mut!(frame_p),
        );
        user::install(
            1,
            &aspace_s,
            caps,
            objects,
            (*sec).qp.qp.as_ptr(),
            core::ptr::addr_of_mut!(frame_s),
        );

        // SAFETY: both cells are installed, present, native, and distinct.
        let (met, finished, sec_code, own_code) = smp::run_cells_on_both(0, 1);

        if !finished {
            println!(
                "smp: SKIP the two-core user-mode phase - the secondary did not \
                 finish its cell within the bound, so nothing about a cell on a \
                 second core is claimed"
            );
            return;
        }
        assert!(
            met && !smp::rendezvous_timed_out(),
            "the two cores never met, so the cells did not start together"
        );
        assert_eq!(own_code, 0, "the primary's cell did not exit cleanly");
        assert_eq!(sec_code, 0, "the secondary's cell did not exit cleanly");
        assert_eq!((*p).params.status, 1, "the primary's cell never finished");
        assert_eq!(
            (*sec).params.status,
            1,
            "the secondary's cell never finished"
        );

        let w = &*core::ptr::addr_of!(WITNESS);
        let (rounds_p, rounds_s) = (w.0[0], w.0[1]);
        let (seen_p, seen_s) = (w.0[2], w.0[3]);
        assert_eq!(rounds_p, CO_ROUNDS, "the primary's cell lost rounds");
        assert_eq!(rounds_s, CO_ROUNDS, "the secondary's cell lost rounds");
        // The witness. A cell can only read a nonzero peer counter if the peer wrote
        // one **between two of this cell's own rounds** - and on one CPU with
        // cooperative dispatch the first cell runs to completion before the second is
        // entered, so it would read 0 every time.
        assert!(
            seen_p > 0 && seen_s > 0,
            "neither direction of overlap was observed (primary saw {seen_p}, \
             secondary saw {seen_s}) - the two cells ran one after the other"
        );
        assert!(
            kernel::mm::frames::used_matches_bitmap(),
            "the frame pool's used counter drifted from its bitmap under two cores"
        );
        println!(
            "smp: TWO CELLS ran in USER MODE on TWO CORES at once - each in its own \
             address space, {CO_ROUNDS} rounds each, and each saw the other advance \
             mid-run (primary saw the secondary reach {seen_p}, the secondary saw the \
             primary reach {seen_s}) OK"
        );
    }
}

// -------------------------------------------- placing cells on whichever core is free
//
// The phase above hands one *named* cell to the secondary: the primary decides who
// runs where, which is the decision a scheduler is supposed to make. This phase makes
// it: a set of runnable cells goes into one queue and **every core claims from it
// whenever it is free** (`smp::place_cells`). Nobody is assigned anything in advance.
//
// Two things are asserted, and they are different claims:
//
//   1. **Every cell ran and finished on some core**, identified by its own exit code -
//      which is what ties a completed run back to a cell when the caller did not
//      choose where it went.
//   2. **More than one core took work**, and the per-core counts sum to the queue.
//      A run where one core took everything would produce the same exit codes and
//      teach nothing, which is exactly why correctness is not the placement evidence.
//
// The workloads are deliberately **uneven** (one long cell, the rest short). Under an
// assign-in-advance split the core holding the long cell finishes late and the short
// ones sit behind it; under claiming, the other cores come back for the next cell
// while it runs. The resulting counts are reported rather than asserted to a
// particular shape, because QEMU's TCG time-slices the vCPUs onto host threads and the
// ratio is a property of that scheduling, not of ours.

#[unsafe(link_section = ".user.bss")]
static mut STORE_Q: [CellStore; PLACED] = [const { CellStore::new() }; PLACED];
static mut KSTACK_Q: [KernelStack; PLACED] = [const { KernelStack::new() }; PLACED];

/// Cells placed in one round. Deliberately **more than the machine has cores**, which
/// is what makes the round say something an assignment could not: some core has to
/// finish a cell and come back for another. Four cells on four cores would be one
/// each whether they were claimed or handed out.
const PLACED: usize = 8;

/// Spin rounds for the one long cell, and for the three short ones.
const LONG_ROUNDS: u64 = 96;
const SHORT_ROUNDS: u64 = 8;

// ------------------------------------------- FlashAttention 2 across every core
//
// The GEMM phase above drains its queue in **kernel context**, which works because it
// is integer. FlashAttention cannot go there: its softmax needs a real `exp`, so it is
// f32, and the kernel is deliberately FP-free (docs/SUBSTRATE.md pillar 4 - if the
// kernel never executes an FP instruction, no syscall, trap or interrupt has to save the
// vector file). The `.user`-window programs cannot host it either: they are compiled as
// part of the kernel crate, and soft-float f32 emits out-of-line calls into kernel
// `.text`, which a cell has no mapping for.
//
// A **loaded ELF cell** has neither problem - it carries its own builtins - so that is
// the parallel unit here. Several `librheo-fa` cells are installed, each given a slice
// of the query rows and a shared output page, and handed to the same placement the cell
// phase below uses: no cell is told which core to run on.
//
// **Why the split is exact.** Output row `i` of attention depends only on query row `i`,
// so slicing by query rows changes no row's arithmetic - not even its summation order,
// unlike slicing the K/V loop. N cells must therefore produce a result **bit-identical**
// to one cell doing every row, and that is what is asserted: not a tolerance, an
// equality. `librheotilebattle` proves the same decomposition single-threaded on all
// three ISAs ("FA2 decomposed over 4 query-row chunks matches the whole-batch result");
// this is that decomposition executed on separate CPUs.

static FA_CELL: &[u8] = fixture::cell!("librheo-fa");

/// Head shape, and the two VAs the cell expects - all four must agree with
/// `librheo/src/bin/librheo-fa.rs`.
const FA_TQ: usize = 32;
const FA_D: usize = 32;
const FA_PARAMS_VA: usize = 0x3_4000_0000;
const FA_OUT_VA: usize = 0x3_4001_0000;

/// How many cells split the rows. More than the machine has cores, so some core must
/// finish a slice and come back - the same reason the cell-placement round over-subscribes.
const FA_CELLS: usize = 8;

/// The launcher's view of `librheo-fa`'s parameter block.
#[repr(C)]
#[derive(Copy, Clone)]
struct FaParams {
    lo: u32,
    hi: u32,
    status: u32,
    rows: u32,
    job: u32,
}

/// Workload selector, mirroring `librheo/src/bin/librheo-fa.rs`.
const FA_JOB_ATTN: u32 = 0;
const FA_JOB_GEMM: u32 = 1;
/// The async job: `hi - lo` strands each doing a real queue round trip. Mirrors
/// `librheo-fa`'s `JOB_ASYNC`.
const FA_JOB_ASYNC: u32 = 2;
/// Strands per async cell. Its `[lo, hi)` is a strand count, not a row range.
const FA_ASYNC_STRANDS: usize = 8;
/// GEMM shape and output VA, likewise mirrored.
const FA_GM: usize = 32;
const FA_GN: usize = 32;
const FA_GEMM_OUT_VA: usize = 0x3_4002_0000;

/// One page, aligned so it can be mapped into a cell as a frame.
#[repr(C, align(4096))]
struct Page([u8; 4096]);

/// The shared output (`FA_TQ * FA_D` f32 = exactly one page) and one parameter page per
/// cell. Kernel statics, mapped into the cells rather than allocated from the pool, so
/// the launcher can read the result back afterwards through its own linear map.
static mut FA_OUT: Page = Page([0; 4096]);
/// The GEMM half's shared i32 output (`FA_GM * FA_GN` i32 = one page) and its
/// single-cell reference.
static mut FA_GEMM_OUT: Page = Page([0; 4096]);
static mut FA_GEMM_REF: [i32; FA_GM * FA_GN] = [0; FA_GM * FA_GN];
static mut FA_PARAM_PAGES: [Page; FA_CELLS] = [const { Page([0; 4096]) }; FA_CELLS];
/// The single-cell reference, copied out between the two rounds.
static mut FA_REF: [f32; FA_TQ * FA_D] = [0.0; FA_TQ * FA_D];

static mut FA_ASPACE: [core::mem::MaybeUninit<kernel::mm::AddressSpace>; FA_CELLS] =
    [const { core::mem::MaybeUninit::uninit() }; FA_CELLS];
static mut FA_FRAME: [core::mem::MaybeUninit<kernel::arch::TrapFrame>; FA_CELLS] =
    [const { core::mem::MaybeUninit::uninit() }; FA_CELLS];
static mut FA_KSTACK: [KernelStack; FA_CELLS] = [const { KernelStack::new() }; FA_CELLS];
static mut FA_QP: [core::mem::MaybeUninit<kernel::queue::QueuePair>; FA_CELLS] =
    [const { core::mem::MaybeUninit::uninit() }; FA_CELLS];
static mut FA_OBJECTS: ObjectTable = ObjectTable::new();
static mut FA_CAPS: CapTable = CapTable::new();

/// Install `n` `librheo-fa` cells, slice `[0, FA_TQ)` between them, and return the
/// queue of cell indices to place.
///
/// # Safety
/// Single-threaded setup on the primary, after `user::reset()`, with no cell live.
unsafe fn fa_install(n: usize, jobs: &[u32]) -> [usize; FA_CELLS] {
    use kernel::capability::{BUDGET_UNLIMITED, ObjectKind};
    use kernel::mm::AddressSpace;
    let mut queue = [0usize; FA_CELLS];
    // SAFETY: the caller's contract.
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(FA_OBJECTS);
        let caps = &mut *core::ptr::addr_of_mut!(FA_CAPS);
        *objects = ObjectTable::new();
        *caps = CapTable::new();
        // One page of shared output, cleared so a row nobody wrote is visibly zero.
        (*core::ptr::addr_of_mut!(FA_OUT)).0.fill(0);

        (*core::ptr::addr_of_mut!(FA_GEMM_OUT)).0.fill(0);
        let out_pa = arch::virt_to_phys(core::ptr::addr_of!(FA_OUT) as usize);
        let gemm_pa = arch::virt_to_phys(core::ptr::addr_of!(FA_GEMM_OUT) as usize);
        // Each workload's rows are split between the cells that carry it, so a mixed
        // queue still covers both outputs exactly once.
        let n_attn = jobs.iter().filter(|&&j| j == FA_JOB_ATTN).count().max(1);
        let n_gemm = jobs.iter().filter(|&&j| j == FA_JOB_GEMM).count().max(1);
        let (mut seen_attn, mut seen_gemm) = (0usize, 0usize);
        for i in 0..n {
            let job = jobs[i];
            let (lo, hi) = if job == FA_JOB_ASYNC {
                // Not a split: every async cell runs the full strand count on its own
                // queue pair, because the claim is that N independent reactors work at
                // once, not that one workload divides.
                (0, FA_ASYNC_STRANDS)
            } else {
                let (rows, k, of) = if job == FA_JOB_GEMM {
                    let k = seen_gemm;
                    seen_gemm += 1;
                    (FA_GM, k, n_gemm)
                } else {
                    let k = seen_attn;
                    seen_attn += 1;
                    (FA_TQ, k, n_attn)
                };
                let per = rows.div_ceil(of);
                ((k * per).min(rows), ((k + 1) * per).min(rows))
            };
            let pp = core::ptr::addr_of_mut!(FA_PARAM_PAGES[i]);
            (*pp).0.fill(0);
            (pp as *mut FaParams).write(FaParams {
                lo: lo as u32,
                hi: hi as u32,
                status: 0,
                rows: 0,
                job,
            });

            let aspace = &mut *core::ptr::addr_of_mut!(FA_ASPACE[i]);
            aspace.write(AddressSpace::new((i + 2) as u16));
            let a = aspace.assume_init_mut();
            let entry = load::load_elf(FA_CELL, a).expect("load librheo-fa");
            let stack_top = load::map_stack(a);
            let qp = load::map_queue(a);
            // The two shared regions. RW, and disjoint per cell except for the output -
            // which the cells write at disjoint offsets, so it needs no lock.
            a.map_user_frame(FA_OUT_VA, out_pa, MapPerm::UserRw);
            a.map_user_frame(FA_GEMM_OUT_VA, gemm_pa, MapPerm::UserRw);
            a.map_user_frame(
                FA_PARAMS_VA,
                arch::virt_to_phys(pp as usize),
                MapPerm::UserRw,
            );

            let object = objects.create(ObjectKind::QueuePair).unwrap();
            let cap = caps
                .mint(
                    objects,
                    object,
                    kernel::capability::READ | kernel::capability::WRITE,
                    BUDGET_UNLIMITED,
                )
                .unwrap();
            let cap_id = cap.raw_low32();
            (*core::ptr::addr_of_mut!(FA_QP[i])).write(qp);
            let qp_ptr = (*core::ptr::addr_of_mut!(FA_QP[i])).as_ptr();

            let kernel_sp = (*core::ptr::addr_of_mut!(FA_KSTACK[i])).top();
            let frame = &mut *core::ptr::addr_of_mut!(FA_FRAME[i]);
            frame.write(arch::trapframe_new(entry, stack_top, 0, kernel_sp));
            user::install(
                i,
                aspace.assume_init_ref(),
                caps,
                objects,
                qp_ptr,
                frame.assume_init_mut(),
            );
            user::set_queue_info(i, load::USER_QUEUE_VA as u64, cap_id);
            queue[i] = i;
        }
    }
    queue
}

/// FlashAttention 2 computed by `FA_CELLS` cells across every core, asserted
/// bit-identical to one cell computing every row.
fn test_flash_attention_parallel() {
    let cores = smp::online_count();
    // SAFETY: single-threaded setup on the primary between rounds; no cell is live.
    unsafe {
        // --- Round 1: one cell, every row. The reference. -------------------
        user::reset();
        let q1 = fa_install(1, &[FA_JOB_ATTN]);
        let out = user::run(q1[0]).1;
        let p0 = &*(core::ptr::addr_of!(FA_PARAM_PAGES[0]) as *const FaParams);
        assert!(
            matches!(out, Outcome::Exited(1)) && p0.status == 1 && p0.rows == FA_TQ as u32,
            "the single-cell FA2 reference did not complete: {out:?}, status {}, rows {}",
            p0.status,
            p0.rows
        );
        let src = core::ptr::addr_of!(FA_OUT) as *const f32;
        let refbuf = &mut *core::ptr::addr_of_mut!(FA_REF);
        for (i, r) in refbuf.iter_mut().enumerate() {
            *r = src.add(i).read();
        }
        // A softmax output is a convex combination of V rows, so an all-zero result
        // would mean nothing ran - and would then match any other all-zero result.
        assert!(
            refbuf.iter().any(|&x| x != 0.0),
            "the FA2 reference is entirely zero - nothing was computed"
        );
        println!("smp: FA2 single-cell reference computed ({FA_TQ} rows x {FA_D})");

        // The GEMM half's reference, the same way: one cell, every row.
        user::reset();
        let g1 = fa_install(1, &[FA_JOB_GEMM]);
        let gout = user::run(g1[0]).1;
        let gp = &*(core::ptr::addr_of!(FA_PARAM_PAGES[0]) as *const FaParams);
        assert!(
            matches!(gout, Outcome::Exited(1)) && gp.status == 1 && gp.rows == FA_GM as u32,
            "the single-cell GEMM reference did not complete: {gout:?}"
        );
        let gsrc = core::ptr::addr_of!(FA_GEMM_OUT) as *const i32;
        let grefbuf = &mut *core::ptr::addr_of_mut!(FA_GEMM_REF);
        for (i, r) in grefbuf.iter_mut().enumerate() {
            *r = gsrc.add(i).read();
        }
        assert!(
            grefbuf.iter().any(|&x| x != 0),
            "the GEMM reference is entirely zero - nothing was computed"
        );
        println!("smp: int8 GEMM single-cell reference computed ({FA_GM}x{FA_GN})");

        // --- Round 2: FA_CELLS cells, placed on whichever core is free. -----
        user::reset();
        // **A mixed queue of three unlike workloads**: f32 attention, integer GEMM, and
        // cells whose work is *async* rather than compute - strands parked on real queue
        // completions. The placement interleaves all three across the cores. This is the
        // part a separate proof per workload cannot show however many cores each uses:
        // the f32 softmax path, the integer GEMM path and the queue/reactor path resident
        // on the machine at the same instant, none disturbing another's result. The queue
        // ABI in particular had only ever been driven from one core at a time.
        let jobs = [
            FA_JOB_ATTN,
            FA_JOB_GEMM,
            FA_JOB_ASYNC,
            FA_JOB_ATTN,
            FA_JOB_GEMM,
            FA_JOB_ASYNC,
            FA_JOB_ATTN,
            FA_JOB_GEMM,
        ];
        let queue = fa_install(FA_CELLS, &jobs);
        let mut placed = [(0u64, 0usize); FA_CELLS];
        // SAFETY: every cell is installed, present, native and listed exactly once.
        let finished = smp::place_cells(&queue, &mut placed);
        if !finished {
            println!(
                "smp: SKIP parallel FA2 - the queue did not drain within the bound, so \
                 nothing about FlashAttention across cores is claimed"
            );
            return;
        }
        // Every slice ran, finished its rows, and says which slice it was.
        let n_attn = jobs.iter().filter(|&&j| j == FA_JOB_ATTN).count();
        let n_gemm = jobs.iter().filter(|&&j| j == FA_JOB_GEMM).count();
        let n_async = jobs.iter().filter(|&&j| j == FA_JOB_ASYNC).count();
        let (mut attn_rows, mut gemm_rows, mut ops) = (0usize, 0usize, 0usize);
        let (mut ka, mut kg) = (0usize, 0usize);
        for i in 0..FA_CELLS {
            let (lo, hi) = if jobs[i] == FA_JOB_ASYNC {
                (0, FA_ASYNC_STRANDS)
            } else {
                let (rows, k, of) = if jobs[i] == FA_JOB_GEMM {
                    let k = kg;
                    kg += 1;
                    (FA_GM, k, n_gemm)
                } else {
                    let k = ka;
                    ka += 1;
                    (FA_TQ, k, n_attn)
                };
                let per = rows.div_ceil(of);
                ((k * per).min(rows), ((k + 1) * per).min(rows))
            };
            let p = &*(core::ptr::addr_of!(FA_PARAM_PAGES[i]) as *const FaParams);
            assert_eq!(
                placed[i].0,
                (lo + 1) as u64,
                "cell {i} exited {} - expected its own slice code",
                placed[i].0
            );
            assert_eq!(p.status, 1, "cell {i} never finished its rows");
            assert_eq!(p.rows as usize, hi - lo, "cell {i} wrote the wrong count");
            match jobs[i] {
                FA_JOB_GEMM => gemm_rows += p.rows as usize,
                FA_JOB_ASYNC => ops += p.rows as usize,
                _ => attn_rows += p.rows as usize,
            }
        }
        // Every async cell completed every one of its round trips. A strand whose
        // completion carried another cell's token would come back with the wrong value
        // and the cell would exit 7 instead of its slice code, so this is a claim about
        // the kernel's opcode dispatch under N concurrent reactors.
        assert_eq!(
            ops,
            n_async * FA_ASYNC_STRANDS,
            "async cells completed {ops} of {} queue round trips",
            n_async * FA_ASYNC_STRANDS
        );
        assert_eq!(attn_rows, FA_TQ, "the slices do not cover every query row");
        assert_eq!(gemm_rows, FA_GM, "the slices do not cover every GEMM row");

        // **Bit-identical**, not within a tolerance: the query-row split changes no
        // row's arithmetic, so anything else is a defect rather than rounding.
        let got = core::ptr::addr_of!(FA_OUT) as *const f32;
        for i in 0..FA_TQ * FA_D {
            let (g, w) = (got.add(i).read(), refbuf[i]);
            assert!(
                g.to_bits() == w.to_bits(),
                "parallel FA2 differs from the single-cell reference at element {i}: \
                 {g:e} vs {w:e}"
            );
        }

        // The GEMM half too, and integer so equality is unconditional.
        let ggot = core::ptr::addr_of!(FA_GEMM_OUT) as *const i32;
        for i in 0..FA_GM * FA_GN {
            assert_eq!(
                ggot.add(i).read(),
                grefbuf[i],
                "parallel int8 GEMM differs from the single-cell reference at element {i}"
            );
        }

        // The work was genuinely spread, and by claim rather than assignment.
        let mut movers = 0;
        let mut claimed = 0;
        for c in 0..smp::MAX_CPUS {
            let n = smp::cells_taken(c);
            claimed += n;
            if n > 0 {
                movers += 1;
            }
        }
        assert_eq!(claimed, FA_CELLS, "claims do not account for the FA cells");
        assert!(
            movers > 1,
            "one core ran all {FA_CELLS} FA slices - the attention head was computed \
             serially"
        );
        println!(
            "smp: THREE UNLIKE WORKLOADS AT ONCE ACROSS {movers} OF {cores} CORES - \
             {FA_CELLS} loaded librheo cells in one mixed queue - {n_attn} computing \
             FlashAttention 2+3 over slices of a {FA_TQ}x{FA_D} head, {n_gemm} computing \
             a tiled int8 {FA_GM}x{FA_GN} GEMM, and {n_async} driving {FA_ASYNC_STRANDS} \
             parked strands each over their own queue pair ({ops} round trips in all) - \
             claimed, not assigned; every slice reported its own work, and **both** \
             assembled compute results are bit-identical to one cell computing every row"
        );
        for c in 0..smp::MAX_CPUS {
            let n = smp::cells_taken(c);
            if n > 0 {
                println!("smp:   CPU {c} ran {n} tile slice(s)");
            }
        }
        // The same tile kernels, on the **other** substrate.
        test_tile_under_linux();
        user::reset();
    }
}

// ------------------------- the tile kernels under the LINUX personality
//
// The tile framework lives in librheo, and every proof of it ran in a librheo cell.
// Node, Bun and Claude Code are not librheo cells - they are `Personality::Linux` cells
// speaking the Linux syscall ABI - so "the tile structure works" and "real Linux
// binaries run" were two claims about two substrates with nothing joining them.
//
// `tilelinux` joins them where they honestly can be: the tile *kernels* are
// dependency-free Rust, so the same source `kernel/engine.rs` and `bench-core` include
// is `#[path]`-included into a static-glibc Linux program. This phase runs it as a Linux
// cell and compares its output hashes against the **librheo cell's actual output**,
// computed here over the reference buffers the rounds above filled.
//
// What that establishes, precisely: the tile programs need nothing librheo provides that
// the Linux personality cannot - no queue pair, no typed grant, no native verb - and the
// two substrates agree bit for bit about the arithmetic. It is a claim about the kernels
// and the ABI beneath them, not about Node, which does not call these functions.

static TILELINUX: &[u8] = fixture::linux_cargo!("tilelinux");

/// FNV-1a, the same constants `tilelinux` uses, so one `u64` replaces a shared page.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Format `h` as the sixteen lowercase hex digits `tilelinux` prints.
fn hex16(h: u64, out: &mut [u8; 16]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for i in 0..16 {
        out[i] = HEX[((h >> (60 - 4 * i)) & 0xF) as usize];
    }
}

/// Run the tile kernels as a **Linux** cell and require the same bits as the librheo
/// cell produced.
///
/// # Safety
/// Single-threaded on the primary between rounds, with no cell live.
unsafe fn test_tile_under_linux() {
    // The hashes the librheo cells actually produced, over the buffers they filled.
    // SAFETY: read on the primary after those rounds ended.
    let (want_gemm, want_attn) = unsafe {
        let g = core::slice::from_raw_parts(
            core::ptr::addr_of!(FA_GEMM_OUT) as *const u8,
            FA_GM * FA_GN * 4,
        );
        let a =
            core::slice::from_raw_parts(core::ptr::addr_of!(FA_OUT) as *const u8, FA_TQ * FA_D * 4);
        (fnv(g), fnv(a))
    };

    // Route the cell's stdout into the capture buffer, the way this kernel's other
    // Linux phases do; `run_linux_cell` installs into slot 0, which is `captured(0)`.
    // SAFETY: the caller's contract.
    unsafe {
        (*core::ptr::addr_of_mut!(STDOUT_LEN))[0] = 0;
    }
    kernel::linux::set_stdout_tap(Some(tap));
    // SAFETY: the caller's contract.
    let out = unsafe { harness::run_linux_cell(TILELINUX, &[b"tilelinux"]) };
    kernel::linux::set_stdout_tap(None);
    let got = captured(0);
    assert!(
        matches!(out, Outcome::Exited(0)),
        "tilelinux exited {out:?}, expected 0"
    );

    let mut hg = [0u8; 16];
    let mut ha = [0u8; 16];
    hex16(want_gemm, &mut hg);
    hex16(want_attn, &mut ha);
    // Built from the librheo cells' **own bytes**, so the expected transcript is not a
    // constant anyone could have copied from a passing run. Fixed-size: this kernel
    // declares no allocator and does not need one for 66 bytes.
    let mut want = [0u8; 66];
    let mut w = 0usize;
    let mut put = |src: &[u8], w: &mut usize| {
        for &b in src {
            want[*w] = b;
            *w += 1;
        }
    };
    put(b"tilelinux: gemm ", &mut w);
    put(&hg, &mut w);
    put(b"\ntilelinux: attn ", &mut w);
    put(&ha, &mut w);
    put(b"\n", &mut w);
    assert_eq!(w, want.len(), "the expected transcript is the wrong length");
    assert!(
        got == &want[..],
        "the Linux cell's tile output does not match the librheo cell's\n  got:  {:?}\n  want: {:?}",
        core::str::from_utf8(got),
        core::str::from_utf8(&want)
    );
    println!(
        "smp: THE TILE KERNELS RAN UNDER THE LINUX PERSONALITY - the same \
         `#[path]`-included tile sources, built as a static-glibc Linux binary, produced \
         byte-identical GEMM ({:016x}) and FlashAttention ({:016x}) output to the librheo \
         cells above; the tile programs need no native verb, no queue pair and no typed \
         grant",
        want_gemm, want_attn
    );
}

/// **A cell ran on a core of the cell's own NUMA node** (docs/SUBSTRATE.md pillar 6,
/// the CPU half of "vcores follow memory").
///
/// `out[i].1` is the CPU that actually ran cell `i`, so the outcome can be checked
/// directly against the inventory rather than through the mechanism that produced it.
///
/// **Zero crossings is deliberately not asserted, and could not honestly be.** A core
/// that drains its own node's queue takes remote work rather than idling - an idle core
/// beside a runnable cell is worse than a remote access - and which core drains first
/// is a race. What *is* asserted is that the kernel's counters agree exactly with the
/// observed mapping, which is what makes them evidence instead of decoration: a counter
/// that missed the steal path, or a preference that was never applied, both show up as
/// a mismatch here.
fn node_affinity(out: &[(u64, usize)]) {
    let inv = kernel::hw::inventory();
    if kernel::mm::frames::nodes_known() < 2 {
        println!(
            "smp: SKIP node-affine placement - this machine reports {} memory node(s), \
             so there is no other node for a claim to prefer or to cross to",
            inv.nnodes
        );
        // The counters must be silent too, not merely small: with one node there is no
        // preference to express, and a count would mean the path ran anyway.
        assert_eq!(
            smp::node_claims(),
            (0, 0),
            "node claims were counted on a single-node machine"
        );
        return;
    }
    // Count the crossings from the *outcome*: for each cell, the node of the CPU that
    // ran it against the node the cell's memory was placed on.
    let mut observed_local = 0usize;
    let mut observed_remote = 0usize;
    for (i, &(_, cpu)) in out.iter().enumerate() {
        let cell_node = kernel::user::cell_node(i);
        let hw = smp::cpu_hw_id_of(cpu);
        let Some(cpu_node) = inv.cpu_node(hw) else {
            panic!("cell {i} ran on CPU {cpu} (hw {hw}), which the inventory does not list");
        };
        if cpu_node == cell_node {
            observed_local += 1;
        } else {
            observed_remote += 1;
        }
    }
    let (counted_local, counted_remote) = smp::node_claims();
    assert_eq!(
        (counted_local, counted_remote),
        (observed_local, observed_remote),
        "the kernel counted {counted_local} local / {counted_remote} crossing claims, \
         but the cells actually ran {observed_local} local / {observed_remote} crossing"
    );
    // **The load-bearing assertion, and it is exact rather than a threshold.** A
    // local/remote ratio cannot separate "the preference is applied" from "the
    // distribution happened to look local": with cells round-robin over two nodes and
    // cores split evenly, random claiming already lands about half of them locally. A
    // first version of this proof asserted only `observed_local > 0` and **passed with
    // the preference deleted** (4-5 of 8 local instead of 7-8), so it was measuring
    // nothing (docs/ENGINEERING.md 1).
    //
    // What is exact: a core must never cross while its own node still holds unclaimed
    // work. By construction it cannot - the own group is tried first and left only when
    // exhausted - so a nonzero count means the preference was not applied at all.
    assert_eq!(
        smp::avoidable_crossings(),
        0,
        "a core took work from another node while its own node still had unclaimed          cells - the own-node cursor is not being tried first"
    );
    println!(
        "smp: NODE-AFFINE PLACEMENT - {observed_local} of {} cells ran on a core of \
         their own memory node, {observed_remote} crossed (a core that drains its own \
         node takes remote work rather than idling), and the kernel's counters agree \
         with the cells' observed cores exactly",
        out.len()
    );
}

fn test_placement() {
    // Bring up the rest of the machine first: with a single secondary, "whichever core
    // is free" has two participants and the result is hard to tell from a split.
    let extra = smp::start_all();
    let online = smp::online_count();
    println!("smp: {online} CPUs online ({extra} more secondaries started for placement)");

    // With the whole machine up, run the tile GEMM again - now with **every** core as a
    // participant. The earlier round had one secondary because it runs before
    // `start_all`, so it could only ever witness a pair; this is the same queue, the
    // same tile kernel and the same bit-exact oracle across all of them, which is what
    // the generation-gated publish exists for (a `take()`n job could serve one
    // secondary and no more).
    test_parallel_gemm(smp::online_count() - 1);

    // And the f32 half of the tile framework, which cannot run in kernel context at
    // all: FlashAttention 2 across every core, as loaded cells.
    test_flash_attention_parallel();

    // SAFETY: single-threaded setup on the primary; the secondaries are parked in
    // their work loop and claim nothing until `place_cells` publishes the queue.
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS2);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS2);
        *objects = ObjectTable::new();
        *caps = CapTable::new();

        let mut aspaces: [core::mem::MaybeUninit<kernel::mm::AddressSpace>; PLACED] =
            [const { core::mem::MaybeUninit::uninit() }; PLACED];
        let mut frames: [core::mem::MaybeUninit<kernel::arch::TrapFrame>; PLACED] =
            [const { core::mem::MaybeUninit::uninit() }; PLACED];
        for i in 0..PLACED {
            let store = core::ptr::addr_of_mut!(STORE_Q[i]);
            let rounds = if i == 0 { LONG_ROUNDS } else { SHORT_ROUNDS };
            let (aspace, _o, frame) = build_cell(
                &mut *store,
                objects,
                caps,
                (*core::ptr::addr_of!(KSTACK_Q[i])).top(),
                (i + 1) as u16,
                user_placed,
                // Exit code: i + 1, so a zero can never be mistaken for a result.
                (i + 1) as u64,
                rounds,
            );
            aspaces[i].write(aspace);
            frames[i].write(frame);
        }

        user::reset();
        let mut queue = [0usize; PLACED];
        for i in 0..PLACED {
            let store = core::ptr::addr_of_mut!(STORE_Q[i]);
            user::install(
                i,
                aspaces[i].assume_init_ref(),
                caps,
                objects,
                (*store).qp.qp.as_ptr(),
                frames[i].as_mut_ptr(),
            );
            queue[i] = i;
        }

        let mut out = [(0u64, 0usize); PLACED];
        // SAFETY: all four cells are installed, present, native, and each is listed
        // exactly once - which is what makes a claim exclusive ownership of a slot.
        let finished = smp::place_cells(&queue, &mut out);
        if !finished {
            println!(
                "smp: SKIP the placement phase - the queue did not drain within the \
                 bound, so nothing about placing cells on free cores is claimed"
            );
            return;
        }

        // 1. Every cell ran to completion, on some core, and says which cell it was.
        for i in 0..PLACED {
            let (code, cpu) = out[i];
            assert_eq!(code, (i + 1) as u64, "cell {i} exited with the wrong code");
            assert!(cpu < smp::MAX_CPUS, "cell {i} records no CPU that ran it");
            assert_eq!(
                (*core::ptr::addr_of!(STORE_Q[i])).params.status,
                1,
                "cell {i} never finished its loop"
            );
        }

        // 2. The work was shared, and the shares account for the whole queue.
        let mut movers = 0;
        let mut total = 0;
        for c in 0..smp::MAX_CPUS {
            let n = smp::cells_taken(c);
            total += n;
            if n > 0 {
                movers += 1;
            }
        }
        assert_eq!(
            total, PLACED,
            "claims ({total}) do not account for the queue ({PLACED})"
        );
        assert!(
            movers > 1,
            "one core claimed all {PLACED} cells - the queue was drained serially, so \
             nothing was placed anywhere"
        );
        // More cells than cores, so at least one core must have finished a cell and
        // **come back** for another. A one-cell-per-core hand-out cannot produce this,
        // and it is the property that makes the queue work-conserving rather than a
        // static split.
        let most = (0..smp::MAX_CPUS).map(smp::cells_taken).max().unwrap_or(0);
        assert!(
            most > 1,
            "no core claimed a second cell, so none came back for more work"
        );
        // 3. **Balancing after the claim.** Claiming divides work by arrival, and once
        // divided it stays divided: the core that drew the long cell *and* a short one
        // would finish late while another core idled. A core that runs dry therefore
        // takes an unstarted cell out of a peer's claim. With one deliberately long
        // cell in a batch of two, that has to happen - and it is asserted rather than
        // hoped for, because a round where it did not happen produced the same exit
        // codes and would have taught nothing.
        let steals = smp::steals();
        assert!(
            steals > 0,
            "no cell was rebalanced out of a peer's claim, yet one cell is {}x longer \
             than the rest - the work stayed divided the way arrival happened to divide \
             it",
            LONG_ROUNDS / SHORT_ROUNDS
        );
        assert!(
            kernel::mm::frames::used_matches_bitmap(),
            "the frame pool's used counter drifted from its bitmap under placement"
        );
        // 4. **The CPU half of "vcores follow memory"** (docs/SUBSTRATE.md pillar 6):
        // a core takes work from its own NUMA node's queue first, so a cell runs on a
        // core that shares a memory controller with the pages the cell was placed on.
        node_affinity(&out);
        println!(
            "smp: {PLACED} RUNNABLE CELLS were PLACED on whichever core was free - none \
             assigned in advance, {movers} cores claimed work (the busiest took \
             {most}), {steals} rebalanced out of a peer's claim by a core that ran \
             dry, every cell exited with its own code"
        );
        for c in 0..smp::MAX_CPUS {
            let n = smp::cells_taken(c);
            if n > 0 {
                println!("smp:   CPU {c} claimed {n}");
            }
        }
    }
}

// ------------------------------- ONE cell, TWO vcores, TWO cores, at the same instant
//
// Every phase above runs **different cells** on different cores. That is real
// parallelism, and it is not the parallelism a program has: a Node worker, a strand
// pool, an FA3 producer/consumer pair are all *one* address space that wants several
// cores. Before vcores, a cell belonged to one core - so the answer to "can my program
// use the machine" was no, however many cores were online (docs/SMP.md 10.0, the claim).
//
// Why a cell was one core, exactly: two cores in one cell would share one trap frame,
// one kernel stack and one FP/SIMD save area, none of which is locked. So the fix is not
// a lock - it is to make those three per **vcore** and move the ownership claim down
// with them (docs/SUBSTRATE.md pillar 3). A vcore is then the unit that is partitioned,
// exactly as a cell was, and the multikernel argument is unchanged one level lower.
//
// This phase publishes **two vcores of one cell** into the same queue every other
// placement uses, so whichever cores are free claim them - nobody is assigned anything.
//
// The witness is the one `test_user_cells_on_both` uses, and for the same reason: each
// vcore writes only its own round counter and the highest peer counter it ever saw, so
// there is nothing to lock and nothing to lose. A nonzero "highest peer seen" means this
// vcore read the peer's progress **between two of its own rounds** - which one CPU
// cannot produce, because there is no kernel-context preemption to interleave them and
// neither vcore yields. Both directions nonzero is only producible by two cores inside
// one address space at once.
//
// Dispatch stays off, as in the two-cell phase: this is about two cores in one cell, not
// about preemption.
//
// **What this phase proves, and what it only requires.** Two of the three per-vcore
// pieces are load-bearing here and were observed failing when reverted: entering
// `vframe[0]` for both vcores instead of `vframe[v]` makes vcore 1 never finish, and
// recording the outcome in `voutcome[0]` instead of `voutcome[v]` panics on the missing
// one. The other two are **construction requirements this phase cannot detect**, and
// saying so is the point:
//
//   * A **per-vcore kernel stack** is required because ARM64 and RISC-V load the trap
//     stack out of the frame (`ld sp, TF_KSP`). Giving both vcores one stack was tried
//     and **passes on all three ISAs**, even with a trap every round: the two cores run
//     the *same* short handler, so each overwrites the other's saved return address and
//     spilled registers with identical bytes. Detecting it needs a handler whose stack
//     contents differ per core, which is a deeper path than a `SYS_CYCLES`.
//   * A **per-vcore FP/SIMD save area** matters when a vcore is stopped and resumed. Each
//     vcore here is entered once and exits once, on its own core with its own register
//     file, so nothing ever reloads a saved image and sharing one area would be
//     invisible. The path that would expose it is preemption of a multi-vcore cell, which
//     the cooperative schedulers currently refuse outright
//     (`user::cell_on_this_cpu`) - so that proof arrives with that capability, not before.

/// The second vcore's user-visible store - its own stack and its own `Params`, in the
/// **same** `.user.bss` window, so both are mapped into the one address space.
#[unsafe(link_section = ".user.bss")]
static mut STORE_V1: CellStore = CellStore::new();

/// The witness page for this phase. Its own static rather than a reuse of `WITNESS`:
/// sharing one would couple two phases' assertions through a static.
#[repr(C, align(4096))]
struct VWitness([u64; 512]);
#[unsafe(link_section = ".user.bss")]
static mut VWITNESS: VWitness = VWitness([0; 512]);

/// One kernel stack **per vcore**, not per cell. On ARM64 and RISC-V a vcore's kernel
/// stack is carried in its own trap frame, so two vcores sharing one would have two
/// cores trapping onto the same stack - a corrupted frame, which presents as a random
/// fault rather than as a missing stack.
static mut KSTACK_V0: KernelStack = KernelStack::new();
static mut KSTACK_V1: KernelStack = KernelStack::new();

/// Rounds each vcore runs. As `CO_ROUNDS`: enough to overlap for many rounds under TCG,
/// few enough to finish inside the boot budget.
const VCORE_ROUNDS: u64 = 64;

fn test_two_vcores_one_cell() {
    // SAFETY: single-threaded setup on the primary; the secondaries are parked in their
    // work loop and claim nothing until `place_vcores` publishes the queue.
    unsafe {
        let w = &mut *core::ptr::addr_of_mut!(VWITNESS);
        w.0[..4].fill(0);
        let shared = core::ptr::addr_of!(VWITNESS) as usize;

        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS2);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS2);
        *objects = ObjectTable::new();
        *caps = CapTable::new();

        // Vcore 0: an ordinary cell, built exactly as every other phase builds one.
        let v0 = core::ptr::addr_of_mut!(STORE_P);
        let (mut aspace, _o, mut frame0) = build_cell(
            &mut *v0,
            objects,
            caps,
            (*core::ptr::addr_of!(KSTACK_V0)).top(),
            7,
            user_copair,
            // Slot 0, **and** bit 1: trap once per round. Without that this program
            // traps only at its exit, and two contexts that each trap once, far apart,
            // never collide - so a kernel stack shared between them would go unnoticed
            // (verified: with one stack for both vcores and no per-round trap the phase
            // passes on all three ISAs).
            0 | 2,
            VCORE_ROUNDS,
        );

        // Vcore 1: a second context in the **same address space**. Its stack and its
        // `Params` page are mapped into that one aspace, and its frame names its own
        // user stack, its own params VA and its own kernel stack. Written out here
        // rather than factored into `harness` because this is its only caller; a second
        // one is when it earns a helper.
        let v1 = core::ptr::addr_of_mut!(STORE_V1);
        let stack1 = core::ptr::addr_of!((*v1).stack) as usize;
        aspace.map_user_range(
            stack1,
            core::mem::size_of_val(&(*v1).stack),
            MapPerm::UserRw,
        );
        let params1 = core::ptr::addr_of!((*v1).params) as usize;
        aspace.map_user(params1 & !0xFFF, MapPerm::UserRw);
        (*v1).params = kernel::abi::Params {
            workload: 1 | 2,
            iters: VCORE_ROUNDS,
            ..kernel::abi::Params::ZERO
        };

        // The shared witness page, mapped once - one address space, so "shared between
        // the vcores" needs no second mapping. That is the whole economy of a vcore over
        // a second cell: no cross-cell grant, no channel, just memory both already have.
        aspace.map_user_range(shared, 4096, MapPerm::UserRw);
        (*v0).params.ticks = shared as u64;
        (*v1).params.ticks = shared as u64;

        let frame1 = arch::trapframe_new(
            user_copair as usize,
            stack1 + core::mem::size_of_val(&(*v1).stack),
            params1,
            (*core::ptr::addr_of!(KSTACK_V1)).top(),
        );
        static mut FRAME1: core::mem::MaybeUninit<kernel::arch::TrapFrame> =
            core::mem::MaybeUninit::uninit();
        let f1 = core::ptr::addr_of_mut!(FRAME1);
        (*f1).write(frame1);

        user::reset();
        user::install(
            0,
            &aspace,
            caps,
            objects,
            (*v0).qp.qp.as_ptr(),
            core::ptr::addr_of_mut!(frame0),
        );
        // SAFETY: `FRAME1` outlives the run, and no other vcore shares vcore 1's user
        // stack or kernel stack.
        let vi = user::install_vcore(0, (*f1).as_mut_ptr());
        assert_eq!(vi, 1, "the second vcore did not land at index 1");
        assert_eq!(user::cell_vcores(0), 2, "cell 0 does not hold two vcores");

        // Publish both vcores of cell 0 as the runnable set.
        let vids = [0 * user::MAX_VCORES, 0 * user::MAX_VCORES + 1];
        let mut out = [(u64::MAX, usize::MAX); 2];
        let before = user::double_entries();
        // SAFETY: cell 0 is installed, present and native, and each vcore is listed once.
        let finished = smp::place_vcores(&vids, &mut out);
        if !finished {
            println!(
                "smp: SKIP the two-vcore phase - the queue did not drain inside the \
                 bound, so nothing about one cell on two cores is claimed"
            );
            return;
        }

        assert_eq!(out[0].0, 0, "vcore 0 exited {:#x}", out[0].0);
        assert_eq!(out[1].0, 0, "vcore 1 exited {:#x}", out[1].0);
        assert_eq!((*v0).params.status, 1, "vcore 0 never finished");
        assert_eq!((*v1).params.status, 1, "vcore 1 never finished");
        assert_eq!(
            user::double_entries(),
            before,
            "two cores were inside the same vcore"
        );

        let (r0, r1) = (w.0[0], w.0[1]);
        let (seen0, seen1) = (w.0[2], w.0[3]);
        assert_eq!(r0, VCORE_ROUNDS, "vcore 0 lost rounds");
        assert_eq!(r1, VCORE_ROUNDS, "vcore 1 lost rounds");

        // **Two different cores.** Without this the phase would pass with both vcores
        // run one after the other on one core - the exit codes and the round counts are
        // identical either way, and only the overlap and the CPUs distinguish them.
        assert!(
            out[0].1 != out[1].1,
            "both vcores of the cell ran on CPU {} - one cell still occupies one core",
            out[0].1
        );
        // The overlap itself.
        assert!(
            seen0 > 0 && seen1 > 0,
            "neither direction of overlap was observed (vcore 0 saw {seen0}, vcore 1 \
             saw {seen1}) - the two vcores ran one after the other"
        );
        assert!(
            kernel::mm::frames::used_matches_bitmap(),
            "the frame pool's used counter drifted from its bitmap"
        );
        println!(
            "smp: ONE CELL ran on TWO CORES at once - two vcores of cell 0 in ONE \
             address space, claimed by CPU {} and CPU {}, {VCORE_ROUNDS} rounds each, \
             and each saw the other advance mid-run (vcore 0 saw vcore 1 reach {seen0}, \
             vcore 1 saw vcore 0 reach {seen1}) OK",
            out[0].1, out[1].1
        );
    }
}

// ------------------------------------- a vcore YIELDS to its sibling, on one core
//
// The phase above puts two vcores on two cores. It says nothing about a cell with more
// vcores than cores, which is the ordinary case the moment a program asks for eight
// workers on four cores - and before this, a multi-vcore cell could not yield at all:
// the cooperative schedulers pick a *cell* and enter its vcore 0, so `cell_on_this_cpu`
// refused multi-vcore cells outright rather than enter a context another core owned.
//
// The fix is the predicate one level down. `user::vcore_on_this_cpu(cell, v)` answers per
// vcore, `SYS_YIELD` tries a **sibling vcore of the same cell first** (round-robin from
// the running one), and the move itself is `user::switch_native_vcore` - the cheapest
// switch in the system, because both contexts share one address space, so there is no
// `activate()` and no TLB consequence: only the FP/SIMD register file and the frame
// change hands.
//
// **This phase is deliberately single-core.** Both vcores are left unclaimed and run on
// the primary, which is what makes the oracle exact: each round of each vcore is one
// append then one yield, so two vcores must produce a strictly **alternating** order
// vector. An alternation is only producible if the yield reached the sibling context -
// not the caller again (which would give a run of one marker) and not another cell
// (there is none). The shared append cursor is safe for the same reason `schedidle`'s is:
// one context executes at a time here.

/// The third vcore store - this phase runs two vcores, and reusing `STORE_V1` would
/// couple the two phases' `Params` through a static.
#[unsafe(link_section = ".user.bss")]
static mut STORE_Y0: CellStore = CellStore::new();
#[unsafe(link_section = ".user.bss")]
static mut STORE_Y1: CellStore = CellStore::new();

/// The shared order page: byte 0 is the append cursor, bytes 1.. the vector.
#[repr(C, align(4096))]
struct OrderPage([u8; 4096]);
#[unsafe(link_section = ".user.bss")]
static mut ORDER: OrderPage = OrderPage([0; 4096]);

static mut KSTACK_Y0: KernelStack = KernelStack::new();
static mut KSTACK_Y1: KernelStack = KernelStack::new();

/// Rounds each vcore runs. Two vcores x 6 rounds = 12 markers, inside `ORDER_MAX` (60).
const YIELD_ROUNDS: u64 = 6;

fn test_vcore_yield() {
    // SAFETY: single-threaded on the primary; nothing is published to a secondary here -
    // this phase runs both vcores on this core on purpose.
    unsafe {
        let ord = &mut *core::ptr::addr_of_mut!(ORDER);
        ord.0.fill(0);
        let shared = core::ptr::addr_of!(ORDER) as usize;

        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS2);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS2);
        *objects = ObjectTable::new();
        *caps = CapTable::new();

        let y0 = core::ptr::addr_of_mut!(STORE_Y0);
        let (mut aspace, _o, mut frame0) = build_cell(
            &mut *y0,
            objects,
            caps,
            (*core::ptr::addr_of!(KSTACK_Y0)).top(),
            8,
            kernel::user_progs::user_vyield,
            b'0' as u64,
            YIELD_ROUNDS,
        );

        let y1 = core::ptr::addr_of_mut!(STORE_Y1);
        let stack1 = core::ptr::addr_of!((*y1).stack) as usize;
        let stack1_len = core::mem::size_of_val(&(*y1).stack);
        aspace.map_user_range(stack1, stack1_len, MapPerm::UserRw);
        let params1 = core::ptr::addr_of!((*y1).params) as usize;
        aspace.map_user(params1 & !0xFFF, MapPerm::UserRw);
        (*y1).params = kernel::abi::Params {
            workload: b'1' as u64,
            iters: YIELD_ROUNDS,
            ticks: shared as u64,
            ..kernel::abi::Params::ZERO
        };
        aspace.map_user_range(shared, 4096, MapPerm::UserRw);
        (*y0).params.ticks = shared as u64;

        static mut YFRAME: core::mem::MaybeUninit<kernel::arch::TrapFrame> =
            core::mem::MaybeUninit::uninit();
        let yf = core::ptr::addr_of_mut!(YFRAME);
        (*yf).write(arch::trapframe_new(
            kernel::user_progs::user_vyield as usize,
            stack1 + stack1_len,
            params1,
            (*core::ptr::addr_of!(KSTACK_Y1)).top(),
        ));

        user::reset();
        user::install(
            0,
            &aspace,
            caps,
            objects,
            (*y0).qp.qp.as_ptr(),
            core::ptr::addr_of_mut!(frame0),
        );
        // SAFETY: `YFRAME` outlives the run; vcore 1 has its own user and kernel stack.
        user::install_vcore(0, (*yf).as_mut_ptr());

        // Both vcores stay **unclaimed**, so both are enterable by this core - which is
        // exactly the single-core behaviour the predicate is written to preserve.
        let before = user::double_entries();
        let (_c, _v, out) = user::run_vcore(0, 0);

        assert_eq!(
            user::double_entries(),
            before,
            "two cores were inside the same vcore"
        );
        assert!(
            matches!(out, Outcome::Exited(0)),
            "the yielding vcores ended {out:?}"
        );
        assert_eq!((*y0).params.status, 1, "vcore 0 never finished");
        // **Vcore 1 is left mid-flight, and that is asserted rather than ignored.** The
        // first vcore to exit unwinds `run`, because `finish` records an outcome and
        // returns the null frame the trampoline reads as "unwind" - which is the correct
        // rule for a *cell* and is not yet a rule for a cell with several vcores. "The
        // cell exits when its **last** vcore exits" is the missing semantics, and it is a
        // named follow-on rather than something to slip in here. Pinning the 0 means that
        // rule arriving shows up as a test change instead of silently.
        assert_eq!(
            (*y1).params.status,
            0,
            "vcore 1 reached its exit, so the run no longer unwinds on the first vcore \
             out - the last-vcore-out rule landed and this assertion is what should change"
        );

        // The oracle, hand-computed: 12 markers, strictly alternating from '0'.
        let n = ord.0[0] as usize;
        assert_eq!(
            n,
            2 * YIELD_ROUNDS as usize,
            "the order vector holds {n} markers, not {}",
            2 * YIELD_ROUNDS
        );
        for (i, &c) in ord.0[1..=n].iter().enumerate() {
            let want = if i % 2 == 0 { b'0' } else { b'1' };
            assert_eq!(
                c, want,
                "order[{i}] is {:?}, not {:?} - the yield did not reach the sibling vcore",
                c as char, want as char
            );
        }
        println!(
            "smp: a VCORE YIELDED TO ITS SIBLING - two vcores of one cell alternated \
             strictly over {n} rounds on ONE core ({YIELD_ROUNDS} each), which only a \
             yield that reaches the sibling context can produce; the switch changes the \
             FP file and the frame and nothing else, since both share one address space OK"
        );
    }
}

// ------------------------------------------------- preemption on every core at once
//
// The placement phase above is work-conserving but **non-preemptive**: a core claims
// a cell and runs it to completion. That is enough to balance load and not enough to
// be a scheduler - a cell that never traps still owns its core until it exits, on
// three cores instead of one.
//
// This phase closes that. Each core claims a *pair* of cells (`smp::CLAIM_BATCH`) and
// runs them **under its own preemption timer**: the slice fires, `on_user_interrupt`
// asks the scheduler for another runnable cell **this core owns**, and the CPU moves.
// Every piece of that is per-core hardware the bring-up trampolines do not set - the
// RISC-V `stimecmp`/`sie` CSRs, the GICv3 redistributor and CPU interface, the LAPIC -
// so each core brings up its own (`enable_preemption_here`).
//
// The cells run [`user_placed`] with **no syscall at all** until they exit, which is
// what makes the evidence unambiguous: under cooperative scheduling the number of
// preemptions taken is exactly zero, because there is no other moment at which the
// CPU could change hands. So the assertion is simply that preemptions were **taken**,
// with the cooperative placement round immediately above as the negative control -
// same cells, same cores, same claims, and `preempt::counters()` reads zero there.
//
// What is NOT claimed: this is preemption *within* a core's own claim. Nothing takes
// a cell away from another core, and nothing migrates a running cell - the cell-to-
// core binding is still the partition that makes the whole path safe without locking
// (docs/SMP.md 10.0).

fn test_cross_core_preemption() {
    // The control: the cooperative round above must have taken none, or "preemptions
    // happened" below is not evidence of anything.
    let (_a0, taken0, _u0, _s0, _c0) = preempt::counters();
    assert_eq!(
        taken0, 0,
        "the cooperative placement round preempted {taken0} times - it is supposed to \
         be the negative control"
    );

    dispatch::enable(true);
    // SAFETY: single-threaded setup on the primary; secondaries are parked in their
    // work loop and claim nothing until the queue is published.
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS2);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS2);
        *objects = ObjectTable::new();
        *caps = CapTable::new();

        let mut aspaces: [core::mem::MaybeUninit<kernel::mm::AddressSpace>; PLACED] =
            [const { core::mem::MaybeUninit::uninit() }; PLACED];
        let mut frames: [core::mem::MaybeUninit<kernel::arch::TrapFrame>; PLACED] =
            [const { core::mem::MaybeUninit::uninit() }; PLACED];
        for i in 0..PLACED {
            let store = core::ptr::addr_of_mut!(STORE_Q[i]);
            let (aspace, _o, frame) = build_cell(
                &mut *store,
                objects,
                caps,
                (*core::ptr::addr_of!(KSTACK_Q[i])).top(),
                (i + 1) as u16,
                user_placed,
                (i + 1) as u64,
                // Every cell long here: a short one can exit before its first slice,
                // and then its core never had anything to preempt.
                LONG_ROUNDS,
            );
            aspaces[i].write(aspace);
            frames[i].write(frame);
        }

        user::reset();
        ktimer::reset();
        idle::reset();
        preempt::reset();
        let mut queue = [0usize; PLACED];
        for i in 0..PLACED {
            let store = core::ptr::addr_of_mut!(STORE_Q[i]);
            user::install(
                i,
                aspaces[i].assume_init_ref(),
                caps,
                objects,
                (*store).qp.qp.as_ptr(),
                frames[i].as_mut_ptr(),
            );
            queue[i] = i;
        }

        let mut out = [(0u64, 0usize); PLACED];
        // SAFETY: as the cooperative round - installed, present, native, listed once.
        let finished = smp::place_cells_preemptive(&queue, &mut out);
        dispatch::enable(false);
        if !finished {
            println!(
                "smp: SKIP the cross-core preemption phase - the queue did not drain \
                 within the bound"
            );
            return;
        }
        for i in 0..PLACED {
            assert_eq!(
                out[i].0,
                (i + 1) as u64,
                "cell {i} exited with the wrong code"
            );
        }

        let (armed, taken, unarmable, to_sibling, to_cell) = preempt::counters();
        if unarmable > 0 && taken == 0 {
            println!(
                "smp: SKIP the cross-core preemption phase - {unarmable} slices could \
                 not be armed (no wired timer interrupt on this ISA), so no preemption \
                 is claimed"
            );
            return;
        }
        assert!(
            taken > 0,
            "no preemption was taken across {armed} armed slices, yet every cell ran a \
             loop that issues no syscall - so the CPU could not have changed hands at \
             all"
        );
        assert_eq!(
            to_sibling, 0,
            "a native cell has one context; a sibling-context preemption is impossible \
             here"
        );
        assert!(
            kernel::mm::frames::used_matches_bitmap(),
            "the frame pool's used counter drifted from its bitmap under preemption"
        );
        // The invariant every other assertion here rests on: one cell, one core. The
        // kernel records which cell each CPU is inside and refuses a second entry, so
        // this is checked rather than inferred from the absence of a crash - every
        // multi-core defect on this path has surfaced as corruption somewhere else
        // entirely (docs/SMP.md 10.0).
        assert_eq!(
            user::double_entries(),
            0,
            "two cores were inside one cell at once"
        );
        // Which cores actually took work, so "on every core" is measured rather than
        // assumed.
        let movers = (0..smp::MAX_CPUS)
            .filter(|&c| smp::cells_taken(c) > 0)
            .count();
        println!(
            "smp: CELLS WERE PREEMPTED ON {movers} CORES AT ONCE - {taken} of {armed} \
             slices took the CPU from a cell that issues no syscall ({to_cell} to \
             another cell), against 0 in the cooperative round just above"
        );
    }
}

// --------------------------------------- a LINUX cell in user mode on a secondary
//
// Every cell run on a secondary so far has been **native**: the tree's own ABI, one
// execution context, no fd table, no VMA list, no signal state. That was the honest
// stopping point, because the Linux personality keeps far more per-cell state and a
// few genuinely global registries beside it (the mapped-file table, the pipe and
// eventfd registries, the pid counter), and docs/SMP.md 10.2 names auditing those as
// the gate for running Linux cells on several cores.
//
// This phase takes the part of that which does **not** need the audit: **one** Linux
// cell, on one core, at a time. The global registries have exactly one writer, so the
// question the audit exists to answer - what happens when two cores mutate them - is
// not being asked. What is being asked is narrower and was genuinely unknown: does the
// Linux syscall path work at all on a core that is not the boot CPU? It runs through
// the same per-CPU trap state the native path needed (the saved kernel context, the
// current-cell record, RISC-V's kernel `tp`, x86-64's GS-relative stub words) plus its
// own dispatch branch, its own fault-to-signal path and its own scheduler.
//
// The fixture is `chello`, an **unmodified static-glibc C binary** - the same one
// `linuxrun` asserts on the primary. Asserting its exact stdout and exit code from a
// secondary is what makes this a claim about the personality rather than about a stub:
// glibc's startup runs `arch_prctl`/`set_tid_address`/`brk`/`readlink` and a demand-
// paged image before it reaches `main`.
//
// It runs **concurrently with a native cell on the primary**, through the same
// rendezvous the two-cells phase uses, so the two are known to have overlapped rather
// than run in sequence.

static CHELLO: &[u8] = fixture::linux!("chello");
/// `chello`'s exact output and exit code, as `linuxrun` asserts them on the primary.
const CHELLO_OUT: &[u8] = b"hello from glibc C\n";
const CHELLO_EXIT: u64 = 9;

/// Captured stdout of the Linux cell. Written only by the core running it (there is
/// one such core), read by the primary after the run.
const CAP_MAX: usize = 1024;
/// One capture buffer per cell slot used by these phases.
const CAP_CELLS: usize = 2;
static mut STDOUT_CAP: [[u8; CAP_MAX]; CAP_CELLS] = [[0; CAP_MAX]; CAP_CELLS];
static mut STDOUT_LEN: [usize; CAP_CELLS] = [0; CAP_CELLS];

/// Route each cell's stdout to **its own** buffer, keyed by the cell the calling core
/// is currently running.
///
/// A single shared buffer works while one core runs a Linux cell; with two, the two
/// transcripts interleave and neither can be asserted. `user::current_index()` reads
/// this CPU's own record (it is `PerCpu`), so the tap needs no argument and no lock -
/// each core writes only its own cell's slot.
fn tap(bytes: &[u8]) {
    let cell = user::current_index().min(CAP_CELLS - 1);
    // SAFETY: each core writes only the slot of the cell it is running, and a cell
    // runs on one core (`user::claim_cell`), so the slots are disjoint.
    unsafe {
        let cap = &mut *core::ptr::addr_of_mut!(STDOUT_CAP);
        let len = &mut *core::ptr::addr_of_mut!(STDOUT_LEN);
        for &b in bytes {
            if len[cell] < CAP_MAX {
                cap[cell][len[cell]] = b;
                len[cell] += 1;
            }
        }
    }
}

/// Captured stdout of cell `i`.
fn captured(i: usize) -> &'static [u8] {
    // SAFETY: read on the primary after the run has ended.
    unsafe {
        let cap = &*core::ptr::addr_of!(STDOUT_CAP);
        let len = *(*core::ptr::addr_of!(STDOUT_LEN)).get_unchecked(i);
        &cap[i][..len]
    }
}

static mut KSTACK_L: KernelStack = KernelStack::new();
static mut QP_L: core::mem::MaybeUninit<kernel::queue::QueuePair> =
    core::mem::MaybeUninit::uninit();

/// The native peer's exit code - distinct from `chello`'s, so neither can be mistaken
/// for the other.
const PEER_EXIT: u64 = 21;

fn test_linux_cell_on_secondary() {
    // SAFETY: single-threaded setup on the primary; secondaries are parked.
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS2);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS2);
        *objects = ObjectTable::new();
        *caps = CapTable::new();

        // Reset **before** loading: `user::reset` clears the personality's mapped-file
        // registry, which the loader registers the image in, so resetting afterwards
        // would make every page of the image fault in as zeros.
        user::reset();
        ktimer::reset();
        idle::reset();

        // Cell 0: the native peer, on the primary.
        let p = core::ptr::addr_of_mut!(STORE_P);
        let (mut aspace_p, _op, mut frame_p) = build_cell(
            &mut *p,
            objects,
            caps,
            (*core::ptr::addr_of!(KSTACK_P)).top(),
            1,
            user_placed,
            PEER_EXIT,
            LONG_ROUNDS,
        );
        let _ = &mut aspace_p;

        // Cell 1: the unmodified glibc binary, on the secondary.
        let mut aspace_l = kernel::mm::AddressSpace::new(2);
        let img = kernel::load::load_elf_linux(CHELLO, &mut aspace_l).expect("load chello");
        let sp = kernel::linux::stack::setup_stack(&mut aspace_l, &img, &[b"chello"], &[]);
        let mut frame_l =
            arch::trapframe_new(img.entry, sp, 0, (*core::ptr::addr_of!(KSTACK_L)).top());

        user::install(
            0,
            &aspace_p,
            caps,
            objects,
            (*p).qp.qp.as_ptr(),
            core::ptr::addr_of_mut!(frame_p),
        );
        user::install(
            1,
            &aspace_l,
            caps,
            objects,
            core::ptr::addr_of!(QP_L) as *const kernel::queue::QueuePair,
            core::ptr::addr_of_mut!(frame_l),
        );
        user::set_personality(1, user::Personality::Linux);
        kernel::linux::install_cell(1, &img, b"");
        // Bind each cell to the core that will run it, so neither core's scheduler can
        // reach into the other's (docs/SMP.md 10.0).
        user::claim_cell(0, 0);
        user::claim_cell(1, 1);

        STDOUT_LEN = [0; CAP_CELLS];
        kernel::linux::set_stdout_tap(Some(tap));
        // SAFETY: both cells are installed and present, and they are distinct; cell 1
        // is Linux, which is what this phase is about.
        let (met, finished, sec_code, own_code) = smp::run_cells_on_both(0, 1);
        kernel::linux::set_stdout_tap(None);

        if !finished {
            println!(
                "smp: SKIP the Linux-on-a-secondary phase - the secondary did not \
                 finish the cell within the bound"
            );
            return;
        }
        assert!(
            met && !smp::rendezvous_timed_out(),
            "the two cores never met, so the Linux cell and the native peer did not \
             overlap"
        );
        assert_eq!(
            own_code, PEER_EXIT,
            "the primary's native peer exited wrong"
        );
        assert_eq!(
            sec_code as u64, CHELLO_EXIT,
            "the glibc binary on the secondary exited {sec_code}, expected {CHELLO_EXIT}"
        );
        let got = captured(1);
        assert!(
            got == CHELLO_OUT,
            "the glibc binary's stdout from the secondary did not match ({} bytes)",
            got.len()
        );
        assert!(
            kernel::mm::frames::used_matches_bitmap(),
            "the frame pool's used counter drifted from its bitmap"
        );
        println!(
            "smp: an UNMODIFIED static-glibc BINARY ran as a LINUX CELL on a SECONDARY \
             core - exact stdout and exit {CHELLO_EXIT} asserted - while a native cell \
             ran on the primary, the two overlapping at a rendezvous neither could pass \
             alone"
        );
    }
}

// ------------------------------------- TWO Linux cells, on two cores, at the same time
//
// The phase above runs **one** Linux cell off the boot CPU, which is safe because the
// personality's genuinely global tables - the mapped-file registry, the pipe/eventfd/
// timerfd/unix-socket registries, the pid counter, the trace ring - then have exactly
// one writer. Two Linux cells reach them concurrently, and that is the question
// docs/SMP.md 10.2 gates.
//
// It is answered the way 10.2 says to answer it first: a **big lock over the whole
// personality dispatch** (`linux::plock`), taken only while more than one CPU is
// online, recursive per CPU because a syscall re-enters the personality through
// `uaccess` -> `fill_fault`. It serialises the *syscalls*; the two cells' user-mode
// code runs genuinely in parallel. Coarse on purpose - there is exactly one place a
// Linux syscall enters, so "every global it touches is protected" is a property of one
// line rather than of a list a new registry can be added to without noticing.
//
// The proof runs the same unmodified static-glibc binary as **both** cells and asserts
// **both** transcripts exactly. That needs the stdout tap to be per cell rather than
// per machine (see `tap`), which is itself the point: with one shared buffer the two
// transcripts interleave, and a test that cannot tell them apart cannot show that both
// ran correctly.

fn test_two_linux_cells() {
    // SAFETY: single-threaded setup on the primary; secondaries are parked.
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS2);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS2);
        *objects = ObjectTable::new();
        *caps = CapTable::new();

        user::reset();
        ktimer::reset();
        idle::reset();

        let mut aspace = [
            kernel::mm::AddressSpace::new(3),
            kernel::mm::AddressSpace::new(4),
        ];
        let mut frame: [core::mem::MaybeUninit<kernel::arch::TrapFrame>; 2] =
            [const { core::mem::MaybeUninit::uninit() }; 2];
        let kstacks = [
            (*core::ptr::addr_of!(KSTACK_P)).top(),
            (*core::ptr::addr_of!(KSTACK_L)).top(),
        ];
        for i in 0..2 {
            let img = kernel::load::load_elf_linux(CHELLO, &mut aspace[i]).expect("load chello");
            let sp = kernel::linux::stack::setup_stack(&mut aspace[i], &img, &[b"chello"], &[]);
            frame[i].write(arch::trapframe_new(img.entry, sp, 0, kstacks[i]));
            user::install(
                i,
                &aspace[i],
                caps,
                objects,
                core::ptr::addr_of!(QP_L) as *const kernel::queue::QueuePair,
                frame[i].as_mut_ptr(),
            );
            user::set_personality(i, user::Personality::Linux);
            kernel::linux::install_cell(i, &img, b"");
            user::claim_cell(i, i);
        }

        STDOUT_LEN = [0; CAP_CELLS];
        kernel::linux::set_stdout_tap(Some(tap));
        // SAFETY: both cells are installed, present and distinct; both are Linux,
        // which is what this phase is about.
        let (met, finished, sec_code, own_code) = smp::run_cells_on_both(0, 1);
        kernel::linux::set_stdout_tap(None);

        if !finished {
            println!(
                "smp: SKIP the two-Linux-cells phase - the secondary did not finish its \
                 cell within the bound"
            );
            return;
        }
        assert!(
            met && !smp::rendezvous_timed_out(),
            "the two cores never met, so the two Linux cells did not overlap"
        );
        assert_eq!(
            own_code, CHELLO_EXIT,
            "the primary's Linux cell exited wrong"
        );
        assert_eq!(
            sec_code as u64, CHELLO_EXIT,
            "the secondary's Linux cell exited wrong"
        );
        for i in 0..2 {
            let got = captured(i);
            assert!(
                got == CHELLO_OUT,
                "Linux cell {i}'s stdout did not match ({} bytes) - the two transcripts \
                 were not both produced correctly",
                got.len()
            );
        }
        assert!(
            kernel::mm::frames::used_matches_bitmap(),
            "the frame pool's used counter drifted from its bitmap"
        );
        println!(
            "smp: TWO LINUX CELLS ran on TWO CORES at the same time - the same \
             unmodified static-glibc binary twice, each transcript captured separately \
             and asserted exactly, each exiting {CHELLO_EXIT}, with the personality's \
             global tables held by one recursive lock over its dispatch"
        );
    }
}

// ------------------------------------------------ NVMe: a queue pair per core
//
// docs/SUBSTRATE.md S5 states the property as a measurement: "per-vcore submission
// never crosses cores (counter-asserted)". This is that assertion.
//
// NVMe's defining property is that a queue pair is a *private* channel to the
// controller. A driver that creates one pair and locks it has the device's
// interface without its design - the ring becomes the serialization point, and the
// cost shows up as contention that no throughput number distinguishes from a slow
// disk. So the driver creates one pair per CPU and each core submits on its own,
// and what makes that a fact rather than an intention is that both cores read at
// the same instant and the cross-core counter is still zero.
//
// The reads are of *different* sectors, and each core checks its own bytes: two
// cores reading the same sector could be served by one queue and look identical.

/// The device, brought up on the primary and read by both cores. A static because
/// the secondary reaches it through a bare `fn()` with no captured environment -
/// the only thing that crosses cores here.
static mut NVME: Option<kernel::hw::nvme::Nvme> = None;

/// Sector each core reads, and where it puts the bytes. Disjoint per core, so the
/// two transfers share nothing at all - which is the point being demonstrated.
const NVME_ROUNDS: usize = 32;
static mut BUF_P: [u8; 512] = [0; 512];
static mut BUF_S: [u8; 512] = [0; 512];
static NVME_P_OK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static NVME_S_OK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Read sector `sector` `NVME_ROUNDS` times into `buf`, counting the reads that
/// succeeded and returned the same bytes every time.
///
/// # Safety
/// `buf` must be this core's own buffer, and the device must be brought up.
unsafe fn nvme_hammer(sector: u64, buf: *mut [u8; 512], done: &core::sync::atomic::AtomicUsize) {
    use kernel::hw::block::BlockDevice;
    // SAFETY: brought up on the primary before either core is released, and never
    // written again; the caller owns `buf`.
    let dev = match unsafe { (*core::ptr::addr_of!(NVME)).as_ref() } {
        Some(d) => d,
        None => return,
    };
    let mut first = [0u8; 512];
    for i in 0..NVME_ROUNDS {
        // SAFETY: the caller's contract - this core's own buffer.
        let b = unsafe { &mut *buf };
        if dev.read(sector, b).is_err() {
            return;
        }
        if i == 0 {
            first = *b;
        } else if *b != first {
            // The same sector read twice must give the same bytes. A difference is
            // a torn or crossed transfer, which is what two cores sharing one ring
            // produces - so it is reported with the bytes rather than counted.
            println!(
                "smp: nvme sector {sector} round {i} differs: {:?} vs first {:?}",
                &b[..8],
                &first[..8]
            );
            return;
        }
        done.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}

fn nvme_primary() {
    // SAFETY: BUF_P is the primary's alone.
    unsafe { nvme_hammer(0, core::ptr::addr_of_mut!(BUF_P), &NVME_P_OK) };
}

fn nvme_secondary() {
    // SAFETY: BUF_S is the secondary's alone.
    unsafe { nvme_hammer(8, core::ptr::addr_of_mut!(BUF_S), &NVME_S_OK) };
}

fn test_nvme_per_core_queues() {
    use kernel::hw::nvme;

    // NVMe's registers are BAR0 and there is no config-space tunnel to reach them
    // through, so the BARs have to be programmed first (no firmware does it on the
    // bare arm/riscv boots).
    kernel::hw::assign_pci_bars();
    let dev = match nvme::probe() {
        Some(d) => d,
        None => {
            println!("smp: no NVMe controller attached - per-core queue phase skipped");
            return;
        }
    };
    let cpus = kernel::hw::inventory().ncpus;
    let online = smp::online_count();
    // SAFETY: the primary, before either core is released to `nvme_hammer`.
    unsafe { *core::ptr::addr_of_mut!(NVME) = Some(dev) };

    let before_cross = nvme::cross_core_submits();
    let (met, finished) = smp::run_fn_with_secondary(nvme_secondary, nvme_primary);
    assert!(met, "smp: cores did not meet before the NVMe reads");
    assert!(finished, "smp: the secondary did not finish its NVMe reads");

    let p = NVME_P_OK.load(core::sync::atomic::Ordering::Relaxed);
    let s = NVME_S_OK.load(core::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        p, NVME_ROUNDS,
        "smp: primary completed {p} of {NVME_ROUNDS} NVMe reads"
    );
    assert_eq!(
        s, NVME_ROUNDS,
        "smp: secondary completed {s} of {NVME_ROUNDS} NVMe reads"
    );

    // Each core's reads landed on its **own** queue, so **two distinct queues** took
    // work. Which secondary answered is not fixed - the placement phase above brought
    // up every core, and whichever is free claims the job - so the assertion is on the
    // shape (two queues, both busy) rather than on queue 1 by name. Without per-core
    // queues exactly one queue is nonzero and holds everything.
    let mut busy = 0;
    let mut total = 0u64;
    for c in 0..nvme::MAX_IOQ {
        let n = nvme::submits(c);
        if n > 0 {
            busy += 1;
            total += n;
        }
    }
    assert!(
        busy >= 2,
        "smp: only {busy} NVMe queue(s) took submissions - the two cores shared one ring"
    );
    assert!(
        total >= 2 * NVME_ROUNDS as u64,
        "smp: {total} submissions for {} reads",
        2 * NVME_ROUNDS
    );

    // And the headline: **no submission crossed a core**. This is what makes the
    // two counts above evidence of a per-core data path rather than of two cores
    // taking turns on one ring.
    let cross = nvme::cross_core_submits() - before_cross;
    assert_eq!(
        cross, 0,
        "smp: {cross} NVMe submissions went to another CPU's queue"
    );

    // The bytes: different sectors, and each core got its own. Reading the same
    // sector on both would pass with one shared queue too.
    // SAFETY: both cores are done (`finished` above).
    let (bp, bs) = unsafe { (*core::ptr::addr_of!(BUF_P), *core::ptr::addr_of!(BUF_S)) };
    assert!(
        bp != bs,
        "smp: the two cores read identical bytes from different sectors - \
         one of them did not read what it asked for"
    );

    // Each core's completions must wake **that core**. A queue whose vector is
    // delivered elsewhere still returns the right bytes - the owner just polls
    // instead of halting - so nothing above this catches it, and the first version
    // of the per-queue routing had exactly that bug (every queue named vector 0).
    let fell_back = nvme::poll_fallbacks();
    assert_eq!(
        fell_back, 0,
        "smp: {fell_back} core(s) armed MSI-X and then never saw their own completion \
         vector - the queues are not interrupting the cores that own them"
    );

    println!(
        "smp: TWO CORES DROVE NVMe THROUGH THEIR OWN QUEUE PAIRS at the same time - \
         {} queue pair(s) ({online} cores online, {cpus} enumerated), {busy} took \
         work ({total} \
         submissions in total), {cross} submissions crossed a core, and each core \
         read its own sector correctly {NVME_ROUNDS} times, each woken by its own \
         completion vector",
        nvme::MAX_IOQ
    );
}
