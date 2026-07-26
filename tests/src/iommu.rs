//! In-QEMU test kernel for IOMMU containment (docs/GPU-HARDWARE.md 4,
//! BUILD-ORDER.md step 12): a device's DMA is mediated by an IOMMU domain,
//! so it reaches only granted physical memory and an out-of-grant DMA
//! faults instead of touching arbitrary RAM.
//!
//! The proof is a real device (virtio-blk) doing real DMA, observed three
//! ways:
//!   1. translation enabled - the IOMMU is genuinely programmed;
//!   2. with an identity domain over low RAM, a block read SUCCEEDS - the
//!      device DMAs through the IOMMU to granted memory;
//!   3. after revoking the domain (map nothing), the same read FAULTS -
//!      the IOMMU records the fault and the read fails.
//!
//! Per-ISA: x86-64 q35 with `-device intel-iommu` drives VT-d; ARM64 virt
//! with `iommu=smmuv3` drives the SMMUv3 backend - both prove the same
//! containment. RISC-V has no QEMU IOMMU model in 8.2, so it surfaces no
//! register base and skips-with-reason (docs/GPU-HARDWARE.md 4).

#![no_std]
#![no_main]

use kernel::hw::{self};
use kernel::{arch, println};

#[cfg(target_arch = "x86_64")]
use kernel::hw::iommu::Vtd as Iommu;
#[cfg(target_arch = "aarch64")]
use kernel::hw::smmuv3::Smmu as Iommu;

/// DMA landing buffer for the mediated virtio-blk read. Only the ISAs with a
/// QEMU IOMMU model (x86-64 VT-d, ARM64 SMMUv3) reach the DMA phase; riscv
/// skips with a reason, so there the static is unreferenced.
#[allow(dead_code)]
static mut BUF: [u8; 512] = [0; 512];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("iommu: start on {}", arch::NAME);

    let iommu_base = hw::inventory().iommu_base;
    if iommu_base == 0 {
        println!(
            "iommu: no IOMMU register base on {} - skip-with-reason \
             (QEMU 8.2 riscv virt has no IOMMU model)",
            arch::NAME
        );
        println!("iommu: PASS");
        arch::exit(arch::ExitCode::Success)
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    run_iommu(iommu_base);

    #[cfg(target_arch = "riscv64")]
    {
        println!("iommu: PASS");
        arch::exit(arch::ExitCode::Success)
    }
}

/// Drive the ISA's IOMMU (VT-d or SMMUv3, aliased as `Iommu`) and prove
/// containment with a virtio-blk device.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn run_iommu(iommu_base: u64) -> ! {
    use kernel::hw::block::BlockDevice;
    use kernel::hw::virtio_blk;

    println!("iommu: IOMMU register base {:#x}", iommu_base);

    // On ARM `virt` there is no firmware to assign PCI BARs (x86 has
    // SeaBIOS), so program them before the virtio-blk-pci device is driven.
    let assigned = hw::assign_pci_bars();
    println!("iommu: assigned {} PCI BARs", assigned);

    let dev = match virtio_blk::probe() {
        Some(d) => d,
        None => {
            println!("iommu: no virtio-blk device to DMA with - skipping");
            println!("iommu: PASS");
            arch::exit(arch::ExitCode::Success)
        }
    };
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };

    // Baseline: a read before translation is enabled.
    dev.read(0, buf).expect("baseline read (no IOMMU)");
    println!("iommu: baseline virtio-blk read OK");

    // Bring up the IOMMU: identity domain over low RAM + translation enable.
    let mut iommu = Iommu::init(iommu_base).expect("IOMMU bring-up");
    assert!(iommu.translation_enabled(), "translation not enabled");
    iommu.take_fault(); // clear any stale status
    println!("iommu: translation enabled");

    // (2) A read while translation is enforced, through the identity domain.
    dev.read(1, buf)
        .expect("read through the identity IOMMU domain");
    assert!(!iommu.take_fault(), "unexpected fault on a granted DMA");
    println!("iommu: read through the identity domain OK (DMA mediated, not blocked)");

    // (3) Revoke the domain (map nothing) and read again: the DMA is now
    // out-of-grant and MUST fault - the BUILD-ORDER step 12 done-when.
    iommu.revoke_all();
    let revoked = dev.read(2, buf);
    let faulted = iommu.take_fault();
    assert!(revoked.is_err(), "out-of-grant DMA should have failed");
    assert!(faulted, "IOMMU did not record an out-of-grant DMA fault");
    println!("iommu: out-of-grant DMA FAULTED and was recorded (read failed)");

    // Restore the identity domain and confirm a clean translating state.
    // A fresh successful read is not asserted: the deliberate fault wedges
    // the virtio device's virtqueue (a device-state effect, not an IOMMU
    // one), which recovers only with a full device reset.
    iommu.restore_all();
    iommu.take_fault();
    assert!(
        iommu.translation_enabled(),
        "translation dropped after restore"
    );
    assert!(!iommu.take_fault(), "spurious fault after restore");
    println!("iommu: domain restored, IOMMU translating cleanly again");

    println!("iommu: PASS");
    arch::exit(arch::ExitCode::Success)
}
