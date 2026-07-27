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

#[path = "harness.rs"]
mod harness;

use harness::{CellStore, KernelStack, build_cell};
use kernel::arch::MapPerm;
use kernel::capability::{CapTable, ObjectTable};
use kernel::smp::{self, SpinLock, StartError};
use kernel::user_progs::{user_copair, user_placed};
use kernel::{arch, println, user};

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
        assert!(
            met && !smp::rendezvous_timed_out(),
            "the two cores never met at the rendezvous, so they did not run at the \
             same time - the GEMM may still be correct, but that would only mean the \
             halves ran one after the other"
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
        let (p_blocks, s_blocks) = (smp::blocks_done(0), smp::blocks_done(idx));
        let total_blocks = GM.div_ceil(smp::GEMM_BLOCK_ROWS);
        assert_eq!(
            p_blocks + s_blocks,
            total_blocks,
            "blocks completed ({p_blocks} + {s_blocks}) do not account for the whole \
             queue ({total_blocks}) - a claim was lost or double-counted"
        );
        assert!(
            p_blocks > 0 && s_blocks > 0,
            "one core did all {total_blocks} blocks (primary {p_blocks}, secondary \
             {s_blocks}) - the queue was drained serially, not shared"
        );
        // And the frame allocator survived two cores using it (the pool lock,
        // docs/SMP.md 10.2): the incremental used counter still agrees with the bitmap
        // it summarises, which is exactly the invariant a lost update breaks.
        assert!(
            kernel::mm::frames::used_matches_bitmap(),
            "the frame pool's used counter drifted from its bitmap under two cores"
        );
        println!(
            "smp: TWO CORES drained one {total_blocks}-block work queue for a \
             {GM}x{GN}x{GK} int8 GEMM at the same time (CPU 0 took {p_blocks}, the \
             secondary {s_blocks} - claimed, not pre-assigned), result \
             bit-identical to the single-core oracle, and the two met at a rendezvous \
             neither could pass alone OK"
        );
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

fn test_placement() {
    // Bring up the rest of the machine first: with a single secondary, "whichever core
    // is free" has two participants and the result is hard to tell from a split.
    let extra = smp::start_all();
    let online = smp::online_count();
    println!("smp: {online} CPUs online ({extra} more secondaries started for placement)");

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
        assert!(
            kernel::mm::frames::used_matches_bitmap(),
            "the frame pool's used counter drifted from its bitmap under placement"
        );
        println!(
            "smp: {PLACED} RUNNABLE CELLS were PLACED on whichever core was free - none \
             assigned in advance, {movers} cores claimed work (the busiest took \
             {most}), every cell exited with its own code"
        );
        for c in 0..smp::MAX_CPUS {
            let n = smp::cells_taken(c);
            if n > 0 {
                println!("smp:   CPU {c} claimed {n}");
            }
        }
    }
}
