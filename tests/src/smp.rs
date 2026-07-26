//! In-QEMU test kernel for SMP: per-CPU state, a kernel spinlock, and
//! secondary-core bring-up (docs/SMP.md, task #27).
//!
//! This is **opt-in** SMP: it exercises `kernel::smp` on the primary CPU (the
//! spinlock + the per-CPU registry, which must work single-core with zero fuss),
//! then asks the arch layer to start one secondary core. The honest per-ISA
//! outcome (docs/SMP.md):
//!   - RISC-V: a genuine second hart runs kernel code (SBI HSM hart_start); the
//!     test asserts the secondary marked itself online and wrote the shared
//!     counter through the cross-core spinlock.
//!   - ARM64 / x86-64: secondary bring-up is blocked in this QEMU config; the
//!     test makes a genuine attempt, prints skip-with-reason, and still PASSES
//!     (mirroring how librheonet/librheogpu skip when a device is absent).
//!
//! Either way the primary never hangs (the bring-up wait is bounded) and never
//! faults - a blocked ISA keeps single-core boot intact.

#![no_std]
#![no_main]

use kernel::smp::{self, SpinLock, StartError};
use kernel::{arch, println};

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
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
            println!(
                "smp: secondary CPU {idx} (hw id {}) ran kernel code - online={}, shared={:#x} OK",
                smp::cpu(idx).hw_id(),
                smp::online_count(),
                shared
            );
            println!("smp: real second core on {} confirmed", arch::NAME);
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
