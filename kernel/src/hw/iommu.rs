//! Intel VT-d DMA remapping (docs/GPU-HARDWARE.md 4, BUILD-ORDER.md step
//! 12): the IOMMU containment mechanism. An IOMMU domain translates a
//! device's DMA through second-level page tables, so a device can reach
//! only the physical memory a mapping grants it - an out-of-grant DMA
//! faults instead of touching arbitrary RAM (doctrine 1).
//!
//! This is the x86-64 VT-d backend. The register access is plain MMIO
//! (portable Rust, no per-ISA cfg), runtime-gated on a discovered DMAR
//! register base (`Inventory::iommu_base`, from the ACPI DMAR table), so
//! the code is inert on a machine without an IOMMU or on an ISA whose
//! IOMMU is a different design (ARM SMMUv3, RISC-V IOMMU - separate
//! backends, the SMMUv3 one a documented next increment). VT-d registers
//! sit near 4 GiB, above the kernel's top-2 GiB linear map, so they are
//! reached through `arch::mmio_map_window` (the device-MMIO window).
//!
//! Scope: register-based invalidation (no queued invalidation), a single
//! shared identity domain over low RAM for every requester, and the
//! fault-status read that proves an out-of-grant DMA was blocked. Enough
//! to prove the grant-to-mapping path and the fault; per-device domains,
//! queued invalidation, and interrupt remapping are future work.

use crate::arch;
use crate::mm::frames;

// VT-d register offsets from the remapping-hardware base.
const REG_CAP: usize = 0x08; // Capability (u64)
const REG_ECAP: usize = 0x10; // Extended Capability (u64)
const REG_GCMD: usize = 0x18; // Global Command (u32)
const REG_GSTS: usize = 0x1C; // Global Status (u32)
const REG_RTADDR: usize = 0x20; // Root Table Address (u64)
const REG_FSTS: usize = 0x34; // Fault Status (u32)
const REG_IQH: usize = 0x80; // Invalidation Queue Head (u64)
const REG_IQT: usize = 0x88; // Invalidation Queue Tail (u64)
const REG_IQA: usize = 0x90; // Invalidation Queue Address (u64)

// Global Command / Status bits.
const GCMD_TE: u32 = 1 << 31; // translation enable
const GCMD_SRTP: u32 = 1 << 30; // set root table pointer
const GCMD_QIE: u32 = 1 << 26; // queued invalidation enable
const GSTS_TES: u32 = 1 << 31; // translation enable status
const GSTS_RTPS: u32 = 1 << 30; // root table pointer status
const GSTS_QIES: u32 = 1 << 26; // queued invalidation enable status

// Queued-invalidation descriptor DW0 encodings (VT-d 6.5.2). QEMU's
// caching-mode IOMMU only tears down its device shadow mappings in response
// to QUEUED invalidation, so the register-based CCMD/IOTLB path is not used.
const QI_CC_GLOBAL: u64 = 0x1 | (0x1 << 4); // context-cache, global
const QI_IOTLB_GLOBAL: u64 = 0x2 | (0x1 << 4) | (1 << 6) | (1 << 7); // iotlb, global, DR|DW

// Fault Status: PPF (primary pending fault) / PFO (fault overflow).
const FSTS_PFO: u32 = 1 << 0;
const FSTS_PPF: u32 = 1 << 1;

// Second-level PTE bits.
const SL_R: u64 = 1 << 0;
const SL_W: u64 = 1 << 1;
const SL_PS: u64 = 1 << 7; // superpage (at the 2 MiB level)

const MIB2: u64 = 2 << 20;

/// A live VT-d unit with one shared identity domain.
pub struct Vtd {
    reg_va: usize,
    /// Invalidation queue (one 4 KiB page = 256 128-bit descriptors).
    iq_pa: usize,
    iq_tail: usize,
    root_pa: usize,
    ctx_pa: usize,
    /// The identity second-level root (maps low RAM); a device using it can
    /// DMA to granted RAM.
    id_sl_pa: usize,
    /// An empty second-level root (maps nothing); a device using it faults
    /// on any DMA - the out-of-grant proof.
    empty_sl_pa: usize,
    /// Byte offset of the first fault-recording register (CAP.FRO * 16).
    fro_off: usize,
}

fn r32(va: usize, off: usize) -> u32 {
    // SAFETY: `va+off` is a mapped VT-d register.
    unsafe { ((va + off) as *const u32).read_volatile() }
}
fn w32(va: usize, off: usize, v: u32) {
    // SAFETY: as above.
    unsafe { ((va + off) as *mut u32).write_volatile(v) };
}
fn r64(va: usize, off: usize) -> u64 {
    unsafe { ((va + off) as *const u64).read_volatile() }
}
fn w64(va: usize, off: usize, v: u64) {
    unsafe { ((va + off) as *mut u64).write_volatile(v) };
}

/// Allocate a zeroed 4 KiB table frame; returns its physical address.
fn table_frame() -> usize {
    let pa = frames::alloc().expect("VT-d table (boot, reserve held)");
    let va = arch::phys_to_virt(pa);
    // SAFETY: a fresh frame, reachable through the linear map.
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, frames::FRAME_SIZE) };
    pa
}

fn table_mut(pa: usize) -> *mut u64 {
    arch::phys_to_virt(pa) as *mut u64
}

impl Vtd {
    /// Bring up VT-d at `base` (a DMAR register base): build a root table,
    /// a shared context table, and an identity second-level domain over
    /// `[0, 1 GiB)` (2 MiB superpages), then enable translation. Returns
    /// `None` if `base` is 0 (no IOMMU) or the hardware rejects bring-up.
    pub fn init(base: u64) -> Option<Vtd> {
        if base == 0 {
            return None;
        }
        let reg_va = arch::mmio_map_window(base as usize, 0x1000);
        let _ = r64(reg_va, REG_ECAP);
        // Fault Recording Offset: CAP bits [33:24], in 16-byte units.
        let cap = r64(reg_va, REG_CAP);
        let fro_off = (((cap >> 24) & 0x3ff) as usize) * 16;

        // Identity second-level domain: SL3[0] -> SL2 table of 512 2 MiB
        // superpages covering [0, 1 GiB). Every DMA the drivers issue (queue
        // rings, buffers - all in low kernel RAM under -m 1G) resolves.
        let id_sl_pa = table_frame();
        let sl2_pa = table_frame();
        // SAFETY: freshly allocated tables reached through the linear map.
        unsafe {
            *table_mut(id_sl_pa) = (sl2_pa as u64) | SL_R | SL_W;
            let sl2 = table_mut(sl2_pa);
            for i in 0..512u64 {
                *sl2.add(i as usize) = (i * MIB2) | SL_R | SL_W | SL_PS;
            }
        }
        // An empty domain: SL root with no entries -> any DMA faults.
        let empty_sl_pa = table_frame();

        // Context table: all 256 devfn entries present, pointing at the
        // identity domain (AW=001 -> 39-bit / 3-level). DID 1.
        let ctx_pa = table_frame();
        // SAFETY: fresh context table.
        unsafe {
            let ctx = table_mut(ctx_pa);
            for i in 0..256usize {
                let lo = (id_sl_pa as u64) | 1; // present, T=00 (SL only)
                let hi = 0x01 | (1u64 << 8); // AW=001, DID=1
                *ctx.add(i * 2) = lo;
                *ctx.add(i * 2 + 1) = hi;
            }
        }

        // Root table: all 256 bus entries present, pointing at the one
        // shared context table (so any bus:devfn maps identically).
        let root_pa = table_frame();
        // SAFETY: fresh root table.
        unsafe {
            let root = table_mut(root_pa);
            for i in 0..256usize {
                *root.add(i * 2) = (ctx_pa as u64) | 1; // present
                *root.add(i * 2 + 1) = 0;
            }
        }

        let iq_pa = table_frame();
        let mut v = Vtd {
            reg_va,
            iq_pa,
            iq_tail: 0,
            root_pa,
            ctx_pa,
            id_sl_pa,
            empty_sl_pa,
            fro_off,
        };
        v.set_root_table();
        v.enable_qi();
        v.invalidate_all();
        v.enable_translation()?;
        Some(v)
    }

    /// Enable queued invalidation: point IQA at our queue (256 descriptors,
    /// 128-bit) and set QIE, waiting for QIES.
    fn enable_qi(&mut self) {
        w64(self.reg_va, REG_IQA, self.iq_pa as u64); // QS=0 (256 entries), DW=0
        w64(self.reg_va, REG_IQH, 0);
        w64(self.reg_va, REG_IQT, 0);
        self.iq_tail = 0;
        let gcmd = r32(self.reg_va, REG_GSTS) | GCMD_QIE;
        w32(self.reg_va, REG_GCMD, gcmd);
        while r32(self.reg_va, REG_GSTS) & GSTS_QIES == 0 {
            core::hint::spin_loop();
        }
    }

    /// Push one 128-bit descriptor onto the invalidation queue.
    fn qi_push(&mut self, dw0: u64, dw1: u64) {
        let slot = self.iq_tail % 256;
        // SAFETY: the IQ is our own 4 KiB frame; slot < 256.
        unsafe {
            let q = table_mut(self.iq_pa);
            *q.add(slot * 2) = dw0;
            *q.add(slot * 2 + 1) = dw1;
        }
        self.iq_tail = (self.iq_tail + 1) % 256;
    }

    /// Submit the queued descriptors and wait for the hardware to drain them
    /// (IQH catches up to IQT).
    fn qi_submit(&mut self) {
        w64(self.reg_va, REG_IQT, (self.iq_tail as u64) << 4);
        while (r64(self.reg_va, REG_IQH) >> 4) as usize % 256 != self.iq_tail {
            core::hint::spin_loop();
        }
    }

    /// Program RTADDR and set the root-table pointer, waiting for RTPS.
    fn set_root_table(&mut self) {
        w64(self.reg_va, REG_RTADDR, self.root_pa as u64);
        w32(self.reg_va, REG_GCMD, GCMD_SRTP);
        while r32(self.reg_va, REG_GSTS) & GSTS_RTPS == 0 {
            core::hint::spin_loop();
        }
    }

    /// Global context-cache + IOTLB invalidation via queued invalidation
    /// (the path QEMU's caching-mode IOMMU honors for shadow-mapping
    /// teardown).
    fn invalidate_all(&mut self) {
        self.qi_push(QI_CC_GLOBAL, 0);
        self.qi_push(QI_IOTLB_GLOBAL, 0);
        self.qi_submit();
    }

    /// Set TE and wait for TES: translation is now enforced for all
    /// remapped devices.
    fn enable_translation(&mut self) -> Option<()> {
        w32(self.reg_va, REG_GCMD, GCMD_TE | GCMD_SRTP);
        for _ in 0..1_000_000 {
            if r32(self.reg_va, REG_GSTS) & GSTS_TES != 0 {
                return Some(());
            }
            core::hint::spin_loop();
        }
        None
    }

    /// Whether translation is enabled (TES).
    pub fn translation_enabled(&self) -> bool {
        r32(self.reg_va, REG_GSTS) & GSTS_TES != 0
    }

    /// Read + clear the fault status register. Returns true if a DMA-remap
    /// fault was pending (PPF or PFO) - the signal an out-of-grant DMA was
    /// blocked. Writing 1s back to FSTS clears the sticky bits.
    pub fn take_fault(&mut self) -> bool {
        let fsts = r32(self.reg_va, REG_FSTS);
        let faulted = fsts & (FSTS_PPF | FSTS_PFO) != 0;
        // Clear the fault-recording register's F bit (bit 127, RW1C) so PPF
        // does not re-latch; then clear the status bits themselves.
        let frcd_hi = self.fro_off + 8;
        let hi = r64(self.reg_va, frcd_hi);
        if hi & (1 << 63) != 0 {
            w64(self.reg_va, frcd_hi, 1 << 63);
        }
        if fsts != 0 {
            w32(self.reg_va, REG_FSTS, fsts); // write-1-to-clear
        }
        faulted
    }

    /// Point every requester at the empty domain (maps nothing) and flush,
    /// so the next DMA any device issues faults. The out-of-grant path: the
    /// device's buffers are no longer granted.
    pub fn revoke_all(&mut self) {
        // SAFETY: the context table is our own, reached through the linear map.
        unsafe {
            let ctx = table_mut(self.ctx_pa);
            for i in 0..256usize {
                *ctx.add(i * 2) = (self.empty_sl_pa as u64) | 1; // present, empty SL
            }
        }
        self.flush();
    }

    /// Restore the identity domain (undo `revoke_all`) and flush.
    pub fn restore_all(&mut self) {
        // SAFETY: as above.
        unsafe {
            let ctx = table_mut(self.ctx_pa);
            for i in 0..256usize {
                *ctx.add(i * 2) = (self.id_sl_pa as u64) | 1;
            }
        }
        self.flush();
    }

    /// Flush the IOMMU's translation caches after a context-table edit:
    /// global context-cache + IOTLB queued invalidation, which QEMU's
    /// caching-mode IOMMU honors to tear down the device's shadow mappings.
    fn flush(&mut self) {
        self.invalidate_all();
    }
}
