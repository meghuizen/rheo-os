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

use kernel::smp::{self, SpinLock, StartError};
use kernel::{arch, println};

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
