//! An NVMe driver implementing [`BlockDevice`] (docs/SUBSTRATE.md S5,
//! docs/FILESYSTEMS.md 1 - which named NVMe as the second transport behind this
//! seam from the start).
//!
//! NVMe is what the storage claims in this tree actually rest on: virtio-blk is a
//! paravirtual transport with one queue and a hypervisor on the other side, while
//! NVMe is the real device interface, and its shape - **paired submission and
//! completion queues in host memory, rung by a doorbell, completed out of order,
//! one queue pair per core** - is the same shape as this OS's own queue ABI. That
//! is why it belongs here rather than being deferred to a driver cell: the
//! substrate's data-plane model and the device's are the same model, so the
//! adaptation is a mapping rather than a translation layer.
//!
//! **Bring-up** follows NVMe 1.4 section 7.6.1: disable the controller and wait
//! for `CSTS.RDY == 0`, publish an admin queue pair (`AQA`/`ASQ`/`ACQ`), enable
//! and wait for `RDY == 1`, then `IDENTIFY` the controller and namespace 1, ask
//! for I/O queues with `SET FEATURES`, and create one I/O completion queue and one
//! submission queue. Read and write are `NVM READ`/`NVM WRITE` on namespace 1.
//!
//! **Transport**: a PCIe endpoint (class 01h, subclass 08h, prog-if 02h), already
//! classified as [`EngineKind::Nvme`] by the enumerator. Unlike virtio-pci - which
//! this tree drives through the `VIRTIO_PCI_CAP_PCI_CFG` config tunnel to avoid
//! needing a BAR - NVMe has no such tunnel: its register file *is* BAR0 and must be
//! mapped. So this driver requires `hw::assign_pci_bars` to have run (there is no
//! firmware to program BARs on the bare arm/riscv boots, and PVH skips it on x86)
//! and maps BAR0 uncacheable through `arch::mmio_map_window`.
//!
//! **Scope, stated rather than implied.** One I/O queue pair, polled, with the
//! completion phase tag as the only ordering signal - no interrupt, no MSI-X, and
//! no per-core queue yet (that is the rest of S5, and the reason the queue count
//! is *requested* from the controller rather than assumed to be 1). Transfers are
//! bounced through a single page-aligned frame and issued one page at a time, so
//! `PRP1` alone addresses every command and no PRP list is built - correct, and
//! deliberately the simple form, since a PRP list buys throughput that QEMU's TCG
//! cannot show. DMA is by physical address (`arch::virt_to_phys`); an IOMMU domain
//! for a storage *cell* is the S5 gate this driver is the prerequisite for.

use super::block::{BlkError, BlockDevice, SECTOR};
use super::{EngineKind, PciDevice};
use crate::arch;
use crate::mm::frames;
use core::cell::RefCell;
use core::sync::atomic::{Ordering, fence};

// --- controller registers (NVMe 1.4 section 3.1), byte offsets into BAR0 ---
const REG_CAP: usize = 0x00;
const REG_CC: usize = 0x14;
const REG_CSTS: usize = 0x1C;
const REG_AQA: usize = 0x24;
const REG_ASQ: usize = 0x28;
const REG_ACQ: usize = 0x30;
/// First doorbell. Queue `q`'s submission tail is at `DOORBELL + 2*q*stride`,
/// its completion head at `DOORBELL + (2*q + 1)*stride`.
const REG_DOORBELL: usize = 0x1000;

const CC_EN: u32 = 1 << 0;
const CSTS_RDY: u32 = 1 << 0;
const CSTS_CFS: u32 = 1 << 1;

// Admin opcodes (NVMe 1.4 section 5).
const ADM_CREATE_SQ: u8 = 0x01;
const ADM_CREATE_CQ: u8 = 0x05;
const ADM_IDENTIFY: u8 = 0x06;
const ADM_SET_FEATURES: u8 = 0x09;

// NVM command-set opcodes (NVMe 1.4 section 6).
const NVM_WRITE: u8 = 0x01;
const NVM_READ: u8 = 0x02;

/// `SET FEATURES` feature id for "number of queues".
const FEAT_NUM_QUEUES: u32 = 0x07;

/// Entries per queue. One 4 KiB frame holds 64 submission entries (64 B each) or
/// 256 completion entries (16 B each), so 64 is the largest depth that keeps both
/// queues to one frame apiece and needs no contiguous multi-frame allocation.
const QDEPTH: u16 = 64;
const SQE_BYTES: usize = 64;
const CQE_BYTES: usize = 16;

/// Queue id of the single I/O queue pair. Admin is always 0.
const IOQ: u16 = 1;

/// Bytes moved per command - one page, so `PRP1` addresses the whole transfer and
/// no PRP list is needed (a transfer that stays inside one page never needs
/// `PRP2`, NVMe 1.4 section 4.3).
const XFER: usize = 4096;

/// A submission-queue entry (NVMe 1.4 figure 105). Written field by field rather
/// than as a struct literal in the hot path, but declared here so the layout is
/// checked once.
#[repr(C)]
#[derive(Copy, Clone, Default)]
struct Sqe {
    /// opcode | fused | psdt | cid<<16
    cdw0: u32,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    mptr_lo: u32,
    mptr_hi: u32,
    prp1_lo: u32,
    prp1_hi: u32,
    prp2_lo: u32,
    prp2_hi: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
}
const _: () = assert!(core::mem::size_of::<Sqe>() == SQE_BYTES);

/// A completion-queue entry (NVMe 1.4 figure 90).
#[repr(C)]
#[derive(Copy, Clone, Default)]
struct Cqe {
    result: u32,
    _rsvd: u32,
    /// sq head (low 16) | sq id (high 16)
    sq: u32,
    /// cid (low 16) | phase (bit 16) | status (bits 17..)
    status: u32,
}
const _: () = assert!(core::mem::size_of::<Cqe>() == CQE_BYTES);

/// One queue pair: a submission ring and a completion ring, each one frame.
struct Queue {
    sq_va: usize,
    cq_va: usize,
    sq_pa: u64,
    cq_pa: u64,
    /// Next submission slot to fill.
    sq_tail: u16,
    /// Next completion slot to read.
    cq_head: u16,
    /// The phase bit a *new* completion carries. Flips every wrap - the only
    /// signal that a slot has been written, since the controller does not move a
    /// pointer the host can read.
    phase: bool,
    /// Command identifier, incremented per submission.
    cid: u16,
}

impl Inner {
    /// Queue `qid`'s state (0 = admin, anything else = the one I/O queue).
    fn queue(&mut self, qid: u16) -> &mut Queue {
        if qid == 0 {
            &mut self.admin
        } else {
            &mut self.io
        }
    }
}

impl Queue {
    /// Allocate and zero a queue pair's two frames.
    fn alloc() -> Option<Queue> {
        let sq_pa = frames::alloc()?;
        let Some(cq_pa) = frames::alloc() else {
            frames::free(sq_pa);
            return None;
        };
        let sq_va = arch::phys_to_virt(sq_pa);
        let cq_va = arch::phys_to_virt(cq_pa);
        // SAFETY: two freshly allocated frames, reached through the kernel linear
        // map; nothing else holds them.
        unsafe {
            core::ptr::write_bytes(sq_va as *mut u8, 0, frames::FRAME_SIZE);
            core::ptr::write_bytes(cq_va as *mut u8, 0, frames::FRAME_SIZE);
        }
        Some(Queue {
            sq_va,
            cq_va,
            sq_pa: sq_pa as u64,
            cq_pa: cq_pa as u64,
            sq_tail: 0,
            cq_head: 0,
            // A zeroed completion ring has phase 0, so the first new entry has 1.
            phase: true,
            cid: 0,
        })
    }
}

/// The parts a command mutates: the two rings and the bounce frame.
///
/// Behind a `RefCell` because [`BlockDevice`] is a `&self` trait - a filesystem
/// holds the device shared - while issuing a command advances a ring. That is the
/// same shape `block::BlockCache` has and the same answer it gives. Casting the
/// `&self` to `&mut` instead would be undefined behaviour, not a shortcut with a
/// cost: the compiler is entitled to assume a `&T` is not written through.
struct Inner {
    admin: Queue,
    io: Queue,
    /// The bounce frame every transfer goes through.
    bounce_va: usize,
    bounce_pa: u64,
}

/// A brought-up NVMe controller with one namespace and one I/O queue pair.
pub struct Nvme {
    /// BAR0's mapped VA.
    regs: usize,
    /// Doorbell stride in bytes (`4 << CAP.DSTRD`).
    stride: usize,
    /// Namespace 1's size in logical blocks, and the block size in bytes.
    nlba: u64,
    lba_bytes: u32,
    inner: RefCell<Inner>,
}

impl Nvme {
    fn rd32(&self, off: usize) -> u32 {
        // SAFETY: `off` is a register offset inside the mapped BAR0 window.
        unsafe { core::ptr::read_volatile((self.regs + off) as *const u32) }
    }
    fn wr32(&self, off: usize, v: u32) {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile((self.regs + off) as *mut u32, v) }
    }
    fn rd64(&self, off: usize) -> u64 {
        // SAFETY: as above. Split into two 32-bit accesses because a 64-bit MMIO
        // read is not guaranteed on every one of the three ISAs.
        (self.rd32(off) as u64) | ((self.rd32(off + 4) as u64) << 32)
    }
    fn wr64(&self, off: usize, v: u64) {
        self.wr32(off, v as u32);
        self.wr32(off + 4, (v >> 32) as u32);
    }

    /// Ring queue `qid`'s submission doorbell with the current tail.
    fn ring_sq(&self, qid: u16, tail: u16) {
        self.wr32(REG_DOORBELL + (2 * qid as usize) * self.stride, tail as u32);
    }
    /// Ring queue `qid`'s completion doorbell with the current head.
    fn ring_cq(&self, qid: u16, head: u16) {
        self.wr32(
            REG_DOORBELL + (2 * qid as usize + 1) * self.stride,
            head as u32,
        );
    }

    /// Submit `sqe` on queue `qid` and poll its completion. Returns the status
    /// field (0 = success) or `None` if the controller wedged.
    ///
    /// Synchronous by construction: this driver has one outstanding command at a
    /// time, so the completion polled for is necessarily this one. Depth is what
    /// a per-core queue and an interrupt buy, and both are the rest of S5.
    fn submit(&self, qid: u16, mut sqe: Sqe) -> Option<u16> {
        let tail = {
            let mut inner = self.inner.borrow_mut();
            let q = inner.queue(qid);
            let cid = q.cid;
            q.cid = q.cid.wrapping_add(1);
            sqe.cdw0 |= (cid as u32) << 16;

            // SAFETY: `sq_va` is a mapped frame holding `QDEPTH` entries; `sq_tail`
            // is reduced mod QDEPTH below, so the write stays inside it.
            unsafe {
                let slot = (q.sq_va as *mut Sqe).add(q.sq_tail as usize);
                slot.write_volatile(sqe);
            }
            q.sq_tail = (q.sq_tail + 1) % QDEPTH;
            q.sq_tail
        };
        // The entry must be visible to the device before the doorbell that tells
        // it to look.
        fence(Ordering::SeqCst);
        self.ring_sq(qid, tail);

        // Poll the completion ring for an entry carrying the expected phase.
        // Bounded by the controller-failure bit and a deadline rather than an
        // iteration count (docs/ENGINEERING.md 2).
        //
        // The ring's fields are read out before the loop rather than held as a
        // borrow across it, because the loop also reads a controller register
        // through `self`.
        let (cq_va, head, want) = {
            let mut inner = self.inner.borrow_mut();
            let q = inner.queue(qid);
            (q.cq_va, q.cq_head, q.phase)
        };
        let deadline = arch::timer_now_ns() + 5_000_000_000; // 5 s
        loop {
            // SAFETY: `cq_va` is a mapped frame holding `QDEPTH` completion
            // entries; `head` is below QDEPTH.
            let cqe: Cqe = unsafe { (cq_va as *const Cqe).add(head as usize).read_volatile() };
            let got_phase = cqe.status & (1 << 16) != 0;
            if got_phase == want {
                fence(Ordering::SeqCst);
                let status = (cqe.status >> 17) as u16;
                let new_head = {
                    let mut inner = self.inner.borrow_mut();
                    let q = inner.queue(qid);
                    q.cq_head = (head + 1) % QDEPTH;
                    if q.cq_head == 0 {
                        q.phase = !q.phase;
                    }
                    q.cq_head
                };
                self.ring_cq(qid, new_head);
                return Some(status);
            }
            if self.rd32(REG_CSTS) & CSTS_CFS != 0 {
                crate::println!("nvme: controller fatal status while polling queue {qid}");
                return None;
            }
            if arch::timer_now_ns() > deadline {
                crate::println!("nvme: queue {qid} completion timed out");
                return None;
            }
            core::hint::spin_loop();
        }
    }

    /// One `NVM READ`/`NVM WRITE` of `XFER` bytes at logical block `lba`, through
    /// the bounce frame.
    fn rw_page(&self, write: bool, lba: u64, blocks: u16) -> Result<(), BlkError> {
        let bounce_pa = self.inner.borrow().bounce_pa;
        let sqe = Sqe {
            cdw0: if write { NVM_WRITE } else { NVM_READ } as u32,
            nsid: 1,
            prp1_lo: bounce_pa as u32,
            prp1_hi: (bounce_pa >> 32) as u32,
            cdw10: lba as u32,
            cdw11: (lba >> 32) as u32,
            // NLB is zero-based: 0 means one block.
            cdw12: (blocks - 1) as u32,
            ..Default::default()
        };
        match self.submit(IOQ, sqe) {
            Some(0) => Ok(()),
            Some(s) => {
                crate::println!("nvme: {} lba {lba} failed, status {s:#x}", {
                    if write { "write" } else { "read" }
                });
                Err(BlkError::Io)
            }
            None => Err(BlkError::Io),
        }
    }

    /// Transfer `buf` to or from `sector`, one page per command.
    /// Check the arguments a transfer in either direction must satisfy, and return
    /// the bounce frame's VA.
    fn xfer_setup(&self, len: usize) -> Result<usize, BlkError> {
        if !len.is_multiple_of(SECTOR) {
            return Err(BlkError::Inval);
        }
        // Capacity is reported in 512-byte sectors, so a controller with a larger
        // logical block would need the caller's sector translated. QEMU's nvme
        // defaults to 512; anything else is refused rather than mistranslated.
        if self.lba_bytes as usize != SECTOR {
            return Err(BlkError::Inval);
        }
        Ok(self.inner.borrow().bounce_va)
    }

    /// Read `buf.len()` bytes from `sector`, one page per command.
    fn transfer_in(&self, sector: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        let bounce = self.xfer_setup(buf.len())?;
        let mut done = 0usize;
        while done < buf.len() {
            let bytes = (buf.len() - done).min(XFER);
            self.rw_page(
                false,
                sector + (done / SECTOR) as u64,
                (bytes / SECTOR) as u16,
            )?;
            // SAFETY: `bytes <= XFER` bytes out of the mapped bounce frame into the
            // caller's buffer at `done`, both in range; the frame is this driver's
            // own, so the two cannot alias.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bounce as *const u8,
                    buf.as_mut_ptr().add(done),
                    bytes,
                )
            };
            done += bytes;
        }
        Ok(())
    }

    /// Write `buf.len()` bytes to `sector`, one page per command.
    ///
    /// A separate function from [`Nvme::transfer_in`] rather than one with a
    /// direction flag, so the write path can take the caller's buffer as `&[u8]`.
    /// Sharing one body would have meant `&mut [u8]` for both and casting away a
    /// shared borrow at the call site - which is exactly the undefined behaviour
    /// the `RefCell` above exists to avoid, reintroduced one layer out.
    fn transfer_out(&self, sector: u64, buf: &[u8]) -> Result<(), BlkError> {
        let bounce = self.xfer_setup(buf.len())?;
        let mut done = 0usize;
        while done < buf.len() {
            let bytes = (buf.len() - done).min(XFER);
            // SAFETY: as in `transfer_in`, in the other direction.
            unsafe {
                core::ptr::copy_nonoverlapping(buf.as_ptr().add(done), bounce as *mut u8, bytes)
            };
            self.rw_page(
                true,
                sector + (done / SECTOR) as u64,
                (bytes / SECTOR) as u16,
            )?;
            done += bytes;
        }
        Ok(())
    }
}

impl BlockDevice for Nvme {
    fn capacity_sectors(&self) -> u64 {
        self.nlba
    }

    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        self.transfer_in(sector, buf)
    }

    fn write(&self, sector: u64, buf: &[u8]) -> Result<(), BlkError> {
        // `transfer` takes `&mut [u8]` because the read direction fills it; on the
        // write path it only ever copies *out* of the slice, so a caller's shared
        // buffer is never written. Split rather than cast: the write direction gets
        // its own borrow of the caller's bytes.
        self.transfer_out(sector, buf)
    }
}

/// Find, bring up and return the machine's first NVMe controller, or `None` with
/// a printed reason.
///
/// `hw::assign_pci_bars()` must have run: NVMe's register file *is* BAR0 and
/// there is no config-space tunnel to reach it through, unlike virtio-pci.
pub fn probe() -> Option<Nvme> {
    let inv = super::inventory();
    let dev: &PciDevice = inv
        .pci
        .iter()
        .take(inv.npci)
        .find(|d| d.engine == EngineKind::Nvme)?;

    let bar0 = dev.bars[0];
    if bar0.base == 0 || bar0.size == 0 {
        crate::println!(
            "nvme: {:02x}:{:02x}.{} has no assigned BAR0 - call hw::assign_pci_bars() first",
            dev.bus,
            dev.dev,
            dev.func
        );
        return None;
    }

    // Bus-master + memory-space enable, or the controller's DMA never leaves it.
    let cmd = arch::pci_cfg_read32(inv.ecam_base, dev.bus, dev.dev, dev.func, 0x04);
    arch::pci_cfg_write32(
        inv.ecam_base,
        dev.bus,
        dev.dev,
        dev.func,
        0x04,
        cmd | 0b110, // memory space | bus master
    );

    let regs = arch::mmio_map_window(bar0.base as usize, bar0.size as usize);
    let admin = Queue::alloc()?;
    let io = Queue::alloc()?;
    let bounce_pa = frames::alloc()? as u64;
    let admin_sq_pa = admin.sq_pa;
    let admin_cq_pa = admin.cq_pa;
    let io_sq_pa = io.sq_pa;
    let io_cq_pa = io.cq_pa;
    let mut c = Nvme {
        regs,
        stride: 4,
        nlba: 0,
        lba_bytes: 0,
        inner: RefCell::new(Inner {
            admin,
            io,
            bounce_va: arch::phys_to_virt(bounce_pa as usize),
            bounce_pa,
        }),
    };

    let cap = c.rd64(REG_CAP);
    c.stride = 4 << ((cap >> 32) & 0xF);
    let mqes = (cap & 0xFFFF) as u16 + 1; // zero-based
    if mqes < QDEPTH {
        crate::println!("nvme: controller max queue entries {mqes} < {QDEPTH}");
        return None;
    }

    // Disable, wait for not-ready, publish the admin queues, enable, wait ready.
    c.wr32(REG_CC, 0);
    if !c.wait_ready(false) {
        return None;
    }
    c.wr32(
        REG_AQA,
        ((QDEPTH as u32 - 1) << 16) | (QDEPTH as u32 - 1), // zero-based, both rings
    );
    c.wr64(REG_ASQ, admin_sq_pa);
    c.wr64(REG_ACQ, admin_cq_pa);
    // CC: IOCQES = 4 (16 B), IOSQES = 6 (64 B), round-robin arbitration, NVM
    // command set, 4 KiB pages (MPS 0), enable.
    c.wr32(REG_CC, (4 << 20) | (6 << 16) | CC_EN);
    if !c.wait_ready(true) {
        return None;
    }

    // Ask for one I/O queue pair each way (both fields zero-based). The reply is
    // what the controller granted, which is where a per-core queue count will be
    // read from when S5 gets there.
    let sqe = Sqe {
        cdw0: ADM_SET_FEATURES as u32,
        cdw10: FEAT_NUM_QUEUES,
        cdw11: 0, // 0 = one of each
        ..Default::default()
    };
    if c.submit(0, sqe) != Some(0) {
        crate::println!("nvme: SET FEATURES (number of queues) failed");
        return None;
    }

    // IDENTIFY namespace 1 into the bounce frame: NSZE (size in logical blocks) at
    // byte 0, FLBAS at 26, the LBA format table at 128.
    let sqe = Sqe {
        cdw0: ADM_IDENTIFY as u32,
        nsid: 1,
        prp1_lo: bounce_pa as u32,
        prp1_hi: (bounce_pa >> 32) as u32,
        cdw10: 0, // CNS 0 = identify namespace
        ..Default::default()
    };
    if c.submit(0, sqe) != Some(0) {
        crate::println!("nvme: IDENTIFY namespace failed");
        return None;
    }
    // SAFETY: the controller just filled the bounce frame, which is mapped and
    // 4096 bytes long; every offset read here is inside it.
    unsafe {
        let p = arch::phys_to_virt(bounce_pa as usize) as *const u8;
        c.nlba = (p as *const u64).read_unaligned();
        let flbas = (p.add(26).read()) & 0xF;
        // Each LBA format is 4 bytes; LBADS (log2 of the block size) is byte 2.
        let lbads = p.add(128 + 4 * flbas as usize + 2).read();
        c.lba_bytes = 1u32 << lbads;
    }

    // Create the I/O completion queue first: a submission queue names the
    // completion queue it reports into, so the reverse order is rejected.
    let sqe = Sqe {
        cdw0: ADM_CREATE_CQ as u32,
        prp1_lo: io_cq_pa as u32,
        prp1_hi: (io_cq_pa >> 32) as u32,
        cdw10: ((QDEPTH as u32 - 1) << 16) | IOQ as u32,
        cdw11: 1, // physically contiguous, interrupts disabled (polled)
        ..Default::default()
    };
    if c.submit(0, sqe) != Some(0) {
        crate::println!("nvme: CREATE IO COMPLETION QUEUE failed");
        return None;
    }
    let sqe = Sqe {
        cdw0: ADM_CREATE_SQ as u32,
        prp1_lo: io_sq_pa as u32,
        prp1_hi: (io_sq_pa >> 32) as u32,
        cdw10: ((QDEPTH as u32 - 1) << 16) | IOQ as u32,
        cdw11: ((IOQ as u32) << 16) | 1, // reports into CQ `IOQ`, contiguous
        ..Default::default()
    };
    if c.submit(0, sqe) != Some(0) {
        crate::println!("nvme: CREATE IO SUBMISSION QUEUE failed");
        return None;
    }

    crate::println!(
        "nvme: {:04x}:{:04x} up - {} blocks of {} bytes, doorbell stride {}",
        dev.vendor,
        dev.device,
        c.nlba,
        c.lba_bytes,
        c.stride
    );
    Some(c)
}

impl Nvme {
    /// Wait for `CSTS.RDY` to reach `want`, on a deadline rather than a spin
    /// count. `CAP.TO` is the controller's own timeout in 500 ms units.
    fn wait_ready(&self, want: bool) -> bool {
        let to_ms = ((self.rd64(REG_CAP) >> 24) & 0xFF).max(1) * 500;
        let deadline = arch::timer_now_ns() + to_ms * 1_000_000;
        loop {
            let csts = self.rd32(REG_CSTS);
            if (csts & CSTS_RDY != 0) == want {
                return true;
            }
            if csts & CSTS_CFS != 0 {
                crate::println!("nvme: controller reported fatal status during bring-up");
                return false;
            }
            if arch::timer_now_ns() > deadline {
                crate::println!("nvme: CSTS.RDY did not reach {want} within {to_ms} ms");
                return false;
            }
            core::hint::spin_loop();
        }
    }
}
