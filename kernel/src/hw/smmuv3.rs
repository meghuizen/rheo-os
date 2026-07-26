//! ARM SMMUv3 DMA remapping (docs/GPU-HARDWARE.md 4, BUILD-ORDER.md step
//! 12): the ARM64 backend of the IOMMU containment mechanism, the sibling
//! of the x86-64 VT-d driver (`iommu.rs`). Structurally unrelated to VT-d:
//! a linear **stream table** (one Stream Table Entry per PCI requester ID)
//! configures translation; each STE points at a **Context Descriptor**
//! that points at ARM LPAE (VMSAv8-64) stage-1 page tables; a **command
//! queue** carries configuration/TLB invalidations and an **event queue**
//! records faults.
//!
//! Scope, matching the VT-d backend: a single shared identity domain over
//! low RAM (2 MiB blocks), plus the revoke (invalidate the STEs) that makes
//! a device's DMA fault - the out-of-grant proof, read back from the event
//! queue. QEMU's SMMUv3 models **stage-1** translation only (a stage-2 STE
//! is rejected `C_BAD_STE`, "S2 used but not supported"), so the identity
//! domain is stage-1: each STE points at a Context Descriptor, which points
//! at ARM LPAE stage-1 page tables via TTB0. The register base is fixed in
//! the QEMU `virt` map (`Inventory::iommu_base` on ARM), present only when
//! the machine is booted with `iommu=smmuv3`.

use crate::arch;
use crate::mm::frames;

// SMMUv3 register offsets (page 0) from the register base.
const R_IDR0: usize = 0x00;
const R_CR0: usize = 0x20;
const R_CR0ACK: usize = 0x24;
const R_CR1: usize = 0x28;
const R_GBPA: usize = 0x44;
const R_STRTAB_BASE: usize = 0x80; // u64
const R_STRTAB_BASE_CFG: usize = 0x88; // u32
const R_CMDQ_BASE: usize = 0x90; // u64
const R_CMDQ_PROD: usize = 0x98; // u32
const R_CMDQ_CONS: usize = 0x9C; // u32
const R_EVENTQ_BASE: usize = 0xA0; // u64
// Event queue producer/consumer live in page 1.
const R_EVENTQ_PROD: usize = 0x100A8; // u32
const R_EVENTQ_CONS: usize = 0x100AC; // u32

// CR0 bits.
const CR0_SMMUEN: u32 = 1 << 0;
const CR0_EVENTQEN: u32 = 1 << 2;
const CR0_CMDQEN: u32 = 1 << 3;

// GBPA: ABORT (unmatched streams abort) + UPDATE (commit the write).
const GBPA_UPDATE: u32 = 1 << 31;
const GBPA_ABORT: u32 = 1 << 20;

// Queue geometry: one 4 KiB frame each.
const STE_DWORDS: usize = 8; // 64-byte Stream Table Entry
const N_STES: usize = 64; // LOG2SIZE = 6 (64 * 64 = 4096)
const STRTAB_LOG2: u32 = 6;
const CMDQ_LOG2: u32 = 8; // 256 * 16 = 4096
const EVENTQ_LOG2: u32 = 7; // 128 * 32 = 4096

// Command opcodes.
const CMD_CFGI_STE_RANGE: u64 = 0x04;
const CMD_TLBI_NSNH_ALL: u64 = 0x30;
const CMD_SYNC: u64 = 0x46;

const MIB2: u64 = 2 << 20;

// Stage-1 LPAE (VMSAv8) descriptor flags for a 2 MiB identity block:
// block(0b01) | AttrIndx=0(<<2) | AP=rw-all(0b01<<6) | SH=inner(0b11<<8)
// | AF(1<<10). AttrIndx 0 selects MAIR0 byte 0 (Normal WB).
const S1_BLOCK: u64 = 0x1 | (0b01 << 6) | (0b11 << 8) | (1 << 10);
const LPAE_TABLE: u64 = 0b11;

/// A live SMMUv3 with one shared identity stage-1 domain.
pub struct Smmu {
    reg_va: usize,
    strtab_pa: usize,
    cmdq_pa: usize,
    cmdq_prod: u32,
    /// The Context Descriptor every STE points at (stage-1 config).
    cd_pa: usize,
}

fn r32(va: usize, off: usize) -> u32 {
    unsafe { ((va + off) as *const u32).read_volatile() }
}
fn w32(va: usize, off: usize, v: u32) {
    unsafe { ((va + off) as *mut u32).write_volatile(v) };
}
fn w64(va: usize, off: usize, v: u64) {
    unsafe { ((va + off) as *mut u64).write_volatile(v) };
}

fn table_frame() -> usize {
    let pa = frames::alloc();
    let va = arch::phys_to_virt(pa);
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, frames::FRAME_SIZE) };
    pa
}
fn as_u64s(pa: usize) -> *mut u64 {
    arch::phys_to_virt(pa) as *mut u64
}

impl Smmu {
    /// Bring up the SMMUv3 with a stage-1 identity domain over `[0, 2 GiB)`
    /// and translation enabled. Returns `None` if `base` is 0.
    pub fn init(base: u64) -> Option<Smmu> {
        if base == 0 {
            return None;
        }
        let reg_va = arch::mmio_map_window(base as usize, 0x20000);
        let _ = r32(reg_va, R_IDR0);

        // Stage-1 identity tables (39-bit VA, 4 KiB granule, 3-level): map
        // [0, 2 GiB) with two L1 entries of 512 2 MiB blocks each. ARM
        // `virt` RAM starts at 1 GiB, so the device's DMA targets (queue
        // rings + buffers) live in L1[1]; L1[0] covers the low/device half.
        let s1_root_pa = table_frame();
        unsafe {
            let root = as_u64s(s1_root_pa);
            for gib in 0..2u64 {
                let l2_pa = table_frame();
                *root.add(gib as usize) = (l2_pa as u64) | LPAE_TABLE;
                let l2 = as_u64s(l2_pa);
                for i in 0..512u64 {
                    let pa = gib * (1 << 30) + i * MIB2;
                    *l2.add(i as usize) = pa | S1_BLOCK;
                }
            }
        }

        // Context Descriptor: TTB0 = the stage-1 root, T0SZ=25 (39-bit VA),
        // 4 KiB granule, TTB0 walks enabled / TTB1 disabled, MAIR0 byte 0 =
        // Normal WB, AA64.
        let cd_pa = table_frame();
        unsafe {
            let cd = as_u64s(cd_pa);
            // dw0: T0SZ[5:0]=25, TG0[7:6]=0, IR0[9:8]=1, OR0[11:10]=1,
            // SH0[13:12]=3, EPD0[14]=0, EPD1[30]=1, V[31]=1 (Context
            // Descriptor valid), IPS[34:32]=2 (40-bit), AA64[41]=1,
            // R[45]=1 (record faults), A[46]=1 (access-flag on), ASID=0.
            *cd.add(0) = 25u64
                | (1 << 8)
                | (1 << 10)
                | (0b11 << 12)
                | (1 << 30)
                | (1u64 << 31)
                | (2u64 << 32)
                | (1u64 << 41)
                | (1u64 << 45)
                | (1u64 << 46);
            *cd.add(1) = s1_root_pa as u64; // TTB0
            *cd.add(3) = 0xFF; // MAIR0: attr0 = Normal WB
        }

        // Stream table: every STE points at the CD (stage-1 translate).
        let strtab_pa = table_frame();
        for sid in 0..N_STES {
            write_ste(strtab_pa, sid, cd_pa, true);
        }

        // Command + event queues (one frame each, zeroed).
        let cmdq_pa = table_frame();
        let eventq_pa = table_frame();

        let mut s = Smmu {
            reg_va,
            strtab_pa,
            cmdq_pa,
            cmdq_prod: 0,
            cd_pa,
        };

        // Program the tables/queues and enable the queues, then SMMUEN.
        w32(reg_va, R_CR1, 0); // walk/queue attrs: device/nGnRE (QEMU: don't care)
        w64(reg_va, R_STRTAB_BASE, strtab_pa as u64);
        // STRTAB_BASE_CFG: FMT=0 (linear), LOG2SIZE in [5:0].
        w32(reg_va, R_STRTAB_BASE_CFG, STRTAB_LOG2);
        w64(reg_va, R_CMDQ_BASE, (cmdq_pa as u64) | CMDQ_LOG2 as u64);
        w32(reg_va, R_CMDQ_PROD, 0);
        w32(reg_va, R_CMDQ_CONS, 0);
        w64(
            reg_va,
            R_EVENTQ_BASE,
            (eventq_pa as u64) | EVENTQ_LOG2 as u64,
        );
        w32(reg_va, R_EVENTQ_PROD, 0);
        w32(reg_va, R_EVENTQ_CONS, 0);

        // Enable command + event queues.
        s.set_cr0(CR0_CMDQEN | CR0_EVENTQEN)?;
        // Invalidate stale config/TLB now that the command queue is live.
        s.invalidate_all();
        // Unmatched streams abort (a full stream table covers our device).
        w32(reg_va, R_GBPA, GBPA_UPDATE | GBPA_ABORT);
        // Enable translation.
        s.set_cr0(CR0_CMDQEN | CR0_EVENTQEN | CR0_SMMUEN)?;
        Some(s)
    }

    /// Write CR0 and wait for CR0ACK to match.
    fn set_cr0(&mut self, val: u32) -> Option<()> {
        w32(self.reg_va, R_CR0, val);
        for _ in 0..1_000_000 {
            if r32(self.reg_va, R_CR0ACK) & val == val {
                return Some(());
            }
            core::hint::spin_loop();
        }
        None
    }

    fn cmd_push(&mut self, dw0: u64, dw1: u64) {
        let slot = (self.cmdq_prod as usize) % (1 << CMDQ_LOG2);
        unsafe {
            let q = as_u64s(self.cmdq_pa);
            *q.add(slot * 2) = dw0;
            *q.add(slot * 2 + 1) = dw1;
        }
        self.cmdq_prod = self.cmdq_prod.wrapping_add(1) & ((1 << (CMDQ_LOG2 + 1)) - 1);
    }

    fn cmd_submit(&mut self) {
        w32(self.reg_va, R_CMDQ_PROD, self.cmdq_prod);
        // Wait for the consumer to drain (CONS == PROD).
        for _ in 0..10_000_000 {
            if r32(self.reg_va, R_CMDQ_CONS) == self.cmdq_prod {
                return;
            }
            core::hint::spin_loop();
        }
    }

    /// Invalidate all STE config caches + the whole TLB, then sync.
    fn invalidate_all(&mut self) {
        self.cmd_push(CMD_CFGI_STE_RANGE, 31); // Range = 31 -> all STEs
        self.cmd_push(CMD_TLBI_NSNH_ALL, 0);
        self.cmd_push(CMD_SYNC, 0);
        self.cmd_submit();
    }

    /// Whether translation is enabled (SMMUEN acked).
    pub fn translation_enabled(&self) -> bool {
        r32(self.reg_va, R_CR0ACK) & CR0_SMMUEN != 0
    }

    /// Read + drain the event queue. Returns true if a fault event was
    /// recorded (EVENTQ producer advanced past the consumer), then advances
    /// the consumer to clear it.
    pub fn take_fault(&mut self) -> bool {
        let prod = r32(self.reg_va, R_EVENTQ_PROD) & ((1 << (EVENTQ_LOG2 + 1)) - 1);
        let cons = r32(self.reg_va, R_EVENTQ_CONS) & ((1 << (EVENTQ_LOG2 + 1)) - 1);
        let faulted = prod != cons;
        if faulted {
            w32(self.reg_va, R_EVENTQ_CONS, prod); // drain
        }
        faulted
    }

    /// Invalidate every STE (mark invalid) so any device DMA faults - the
    /// out-of-grant path.
    pub fn revoke_all(&mut self) {
        for sid in 0..N_STES {
            write_ste(self.strtab_pa, sid, 0, false); // V=0
        }
        self.invalidate_all();
    }

    /// Restore the identity domain (undo `revoke_all`).
    pub fn restore_all(&mut self) {
        for sid in 0..N_STES {
            write_ste(self.strtab_pa, sid, self.cd_pa, true);
        }
        self.invalidate_all();
    }
}

/// Write Stream Table Entry `sid`. When `valid`, configure stage-1
/// translation through the Context Descriptor at `cd_pa` (Config=0b101,
/// stage-1 translate + stage-2 bypass); otherwise mark it invalid (V=0).
fn write_ste(strtab_pa: usize, sid: usize, cd_pa: usize, valid: bool) {
    let base = as_u64s(strtab_pa);
    let ste = unsafe { base.add(sid * STE_DWORDS) };
    // Clear all 8 dwords first.
    for i in 0..STE_DWORDS {
        unsafe { *ste.add(i) = 0 };
    }
    if !valid {
        return;
    }
    // dword0: V=1, Config=0b101 (stage-1 translate, stage-2 bypass) at
    //   bits[3:1], S1Fmt=0 (linear single CD), S1ContextPtr[51:6] = cd_pa
    //   (64-byte aligned, so its low bits are 0 and it ORs in directly).
    // dword1: S1CIR/S1COR=WB, S1CSH=inner (CD-fetch attrs); S1CDMax=0.
    let dword0 = 1u64 | (0b101 << 1) | (cd_pa as u64);
    let dword1 = (0b01u64 << 2) | (0b01u64 << 4) | (0b11u64 << 6);
    unsafe {
        *ste.add(0) = dword0;
        *ste.add(1) = dword1;
    }
}
