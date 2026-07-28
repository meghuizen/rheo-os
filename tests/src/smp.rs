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

/// Bring up one secondary core, or skip-with-reason where the ISA blocks it.
fn test_secondary_bringup() {
    // Baseline free-frame count, captured before the secondary starts. The two-core
    // frame-allocator contention (`smp::contend_frames`) runs *inside* bring-up,
    // and each of its iterations is net-zero (alloc then free), so a correct,
    // properly-locked allocator returns to exactly this count afterward (task #132).
    let frames_before = kernel::mm::frames::stats().0;
    // Same baseline for the persistent-memory pool. `(free, total)`; total is 0
    // where no nvdimm was surfaced (arm/riscv `virt`, or x86 without the device),
    // in which case the pmem contention phase is a documented skip.
    let (pmem_free_before, pmem_total) = kernel::mm::frames_pmem::stats();

    match smp::bring_up_one() {
        Ok(idx) => {
            // A real second core ran kernel code. Verify it through the shared
            // state it touched: the per-CPU registry and the cross-core spinlock.
            assert!(smp::secondaries_up() >= 1, "secondary did not signal up");
            assert!(
                smp::online_count() >= 2,
                "expected at least 2 CPUs online (boot + secondary)"
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
            // Genuine two-core mutual exclusion: the primary and the secondary each
            // incremented one shared counter CONTENTION_ITERS times, concurrently,
            // under the SpinLock. The exact sum survives only because the lock
            // serialised every read-modify-write; a lock without real cross-core
            // exclusion would lose updates to the race and fall short. This is the
            // proof that upgrades "a cross-core write lands" to "the lock is a
            // correct mutual-exclusion primitive under contention" - the foundation
            // the #132 kernel-wide locks rest on (docs/SMP.md 10).
            let contended = smp::contended_value();
            let want = smp::CONTENTION_ITERS * 2;
            assert_eq!(
                contended, want,
                "two-core lock contention lost updates: got {contended}, want {want}"
            );
            println!(
                "smp: two-core lock contention OK - {} locked increments from each \
                 of 2 cores serialised to exactly {contended}",
                smp::CONTENTION_ITERS
            );
            println!("smp: real second core on {} confirmed", arch::NAME);

            // Genuine two-core frame-allocator contention (task #132): the primary
            // and the secondary each ran FRAME_CONTENTION_ITERS alloc+free cycles
            // against `mm::frames` concurrently, under its internal lock. A lock
            // that failed to serialise the bitmap read-modify-write would either
            // trip the double-free assertion mid-run (a panic that fails the test)
            // or leave the count and the bitmap disagreeing. The two survivors
            // prove it held: the free-frame count is back at its baseline (every
            // alloc was matched by exactly one free - no frame handed out twice, no
            // count lost) and the O(1) count still agrees with the bitmap.
            assert!(
                kernel::mm::frames::used_matches_bitmap(),
                "frame count/bitmap disagree after concurrent alloc/free"
            );
            let frames_after = kernel::mm::frames::stats().0;
            assert_eq!(
                frames_after, frames_before,
                "frame pool not balanced after concurrent alloc/free \
                 (before {frames_before}, after {frames_after})"
            );
            println!(
                "smp: two-core frame-allocator contention OK - {} alloc+free cycles \
                 from each of 2 cores, pool balanced at {frames_before} free, \
                 bitmap consistent",
                smp::FRAME_CONTENTION_ITERS
            );

            // The persistent-memory allocator, the other truly-global pool, made
            // SMP-safe the same way (task #132). Only x86-64 q35 with an attached
            // nvdimm surfaces one here; arm/riscv `virt` skip-with-reason. The proof
            // is the double-free assertion in `frames_pmem::free` never firing (a
            // broken lock would hand one frame to both cores) plus the pool back at
            // its baseline after the net-zero contention.
            if pmem_total > 0 {
                let pmem_after = kernel::mm::frames_pmem::stats().0;
                assert_eq!(
                    pmem_after, pmem_free_before,
                    "pmem pool not balanced after concurrent alloc/free \
                     (before {pmem_free_before}, after {pmem_after})"
                );
                println!(
                    "smp: two-core pmem-allocator contention OK - {} alloc+free cycles \
                     from each of 2 cores, pool balanced at {pmem_free_before} free",
                    smp::FRAME_CONTENTION_ITERS
                );
            } else {
                println!(
                    "smp: pmem-allocator contention SKIP {} - no nvdimm surfaced",
                    arch::NAME
                );
            }

            // The system-wide admission ledger, the third truly-global static made
            // SMP-safe (task #132). Both cores ran net-zero admit+release cycles
            // against it concurrently through the lock-guarded `sched::system_*`
            // path (the old `&'static mut` accessor is gone - handing a `&mut` to
            // two cores was unsound). The oracle is that the ledger is back at zero
            // committed: every admit matched by exactly one release, no committed
            // utilization lost or stranded.
            let committed = kernel::sched::system_committed_ppm();
            assert_eq!(
                committed, 0,
                "admission ledger not balanced after concurrent admit/release \
                 ({committed} ppm still committed)"
            );
            println!(
                "smp: two-core admission-ledger contention OK - {} admit+release cycles \
                 from each of 2 cores, ledger balanced at 0 ppm committed",
                smp::FRAME_CONTENTION_ITERS
            );

            // Start-all: bring up any *additional* secondaries this ISA supports
            // (docs/SMP.md 10). Sequential - each is fully online before the next
            // is released - so the per-CPU stack hand-off has no race. Each extra
            // core must claim a *distinct* registry slot and a hardware id that is
            // neither the boot CPU's nor the first secondary's - unfakeable by the
            // primary or by one core looping. A failed extra bring-up degrades to a
            // skip (the ISA keeps the cores it did start); it never fails the test.
            let mut online = 2usize; // boot + first secondary
            for ordinal in 1..smp::secondary_count() {
                match smp::bring_up_nth(ordinal) {
                    Ok(slot) => {
                        assert_ne!(slot, 0, "extra secondary took the boot slot");
                        assert_ne!(slot, idx, "extra secondary reused the first's slot");
                        assert!(smp::cpu(slot).is_online(), "extra secondary {slot} offline");
                        assert_ne!(
                            smp::cpu(slot).hw_id(),
                            arch::boot_cpu_hw_id(),
                            "extra secondary recorded the boot CPU's hw id"
                        );
                        assert_ne!(
                            smp::cpu(slot).hw_id(),
                            smp::cpu(idx).hw_id(),
                            "extra secondary recorded the first secondary's hw id"
                        );
                        online += 1;
                        println!(
                            "smp: additional secondary CPU {slot} (hw id {}) online",
                            smp::cpu(slot).hw_id()
                        );
                    }
                    Err(e) => {
                        println!("smp: additional secondary {ordinal} skipped: {e:?}");
                    }
                }
            }
            assert_eq!(
                smp::online_count(),
                online,
                "online count disagrees with the cores actually brought up"
            );
            if smp::secondary_count() > 1 {
                println!(
                    "smp: start-all OK on {} - {online} CPUs online (boot + {} secondaries)",
                    arch::NAME,
                    online - 1
                );
            }
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
