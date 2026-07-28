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
use crate::smp::SpinLock;
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

/// One core's private I/O path: its queue pair and its bounce frame.
///
/// **One of these per CPU, and that is the point** (docs/SUBSTRATE.md S5). NVMe's
/// defining property is that a queue pair is a *private* channel to the
/// controller, so N cores submit through N rings and never touch each other's
/// state - no shared cursor, no cache line moving between cores. A single shared
/// queue pair would make the device the serialization point the whole design
/// exists to remove. The bounce frame is per-core for the same reason: two cores
/// staging through one buffer would corrupt each other's transfer with no
/// diagnostic at all.
///
/// The queue needs interior mutability because [`BlockDevice`] is a `&self`
/// trait (a filesystem holds the device shared) while issuing a command advances
/// a ring. Casting the `&self` to `&mut` instead would be undefined behaviour, not a
/// shortcut with a cost: the compiler is entitled to assume a `&T` is not written
/// through.
///
/// It is a [`SpinLock`], **not** a `RefCell`, and the difference is not stylistic.
/// A `RefCell`'s borrow flag is a plain `Cell` - non-atomic - so a `RefCell` is
/// `!Sync` and a type containing one cannot soundly be shared between cores at
/// all, whatever the access pattern underneath. This device *is* reached from two
/// cores. The partitioning above means the lock is never contended, so an acquire
/// is one uncontended atomic exchange, unmeasurable next to a PCIe round trip -
/// and in exchange the type says what is true rather than relying on a comment.
/// That is the same call `mm::frames` already made, for the same stated reason:
/// whether a structure needs a lock is a property of the structure, not of which
/// cargo features are enabled (docs/SMP.md).
struct Chan {
    q: SpinLock<Queue>,
    /// The bounce frame this core's transfers go through.
    bounce_va: usize,
    bounce_pa: u64,
}

/// The most I/O queue pairs to create - one per CPU, capped where the controller
/// or this constant runs out. Matches `smp::MAX_SMP_CPUS`.
pub const MAX_IOQ: usize = 8;

/// A brought-up NVMe controller with one namespace and one I/O queue pair **per
/// CPU**.
pub struct Nvme {
    /// BAR0's mapped VA.
    regs: usize,
    /// Doorbell stride in bytes (`4 << CAP.DSTRD`).
    stride: usize,
    /// Namespace 1's size in logical blocks, and the block size in bytes.
    nlba: u64,
    lba_bytes: u32,
    /// The admin queue. Used only during bring-up, on the boot CPU - but locked
    /// like the others, because "only the boot CPU touches it" is a fact about
    /// today's callers and not something the type would enforce tomorrow.
    admin: SpinLock<Queue>,
    /// Per-CPU I/O channels. Index `c` belongs to CPU `c` and is touched by no
    /// other core, which is what makes the inner `RefCell` sound without a lock.
    /// Filled during `probe` and never written again.
    io: [Option<Chan>; MAX_IOQ],
    /// How many I/O queue pairs the controller actually granted.
    nio: usize,
}

/// This device is reached from more than one core (the per-core channels above are
/// the whole point), so the type has to be `Sync` - and has to *stay* `Sync` when
/// someone adds a field. A `RefCell` here would fail this line rather than being
/// found later as two cores reading different bytes from one sector.
const _: () = {
    const fn assert_sync<T: Sync>() {}
    assert_sync::<Nvme>();
};

/// Commands submitted on each I/O queue, and how many were submitted from a CPU
/// the queue does not belong to.
///
/// The second number is the S5 property stated as a measurement: a per-core data
/// path is only per-core if submissions never cross. `0` is the assertion, and a
/// nonzero value would mean two cores shared a ring - exactly the contention the
/// design exists to remove, and exactly the thing a throughput number hides.
static SUBMITS: [core::sync::atomic::AtomicU64; MAX_IOQ] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAX_IOQ];
static CROSS_CORE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Commands submitted on I/O queue `c` - the queue belonging to CPU `c`.
pub fn submits(c: usize) -> u64 {
    SUBMITS
        .get(c)
        .map(|a| a.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// Submissions made on a queue that does not belong to the submitting CPU.
/// **Must be zero** - see [`submits`].
pub fn cross_core_submits() -> u64 {
    CROSS_CORE.load(Ordering::Relaxed)
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
    fn submit_on(&self, qid: u16, qcell: &SpinLock<Queue>, sqe: Sqe) -> Option<u16> {
        self.submit_full(qid, qcell, sqe).map(|(s, _)| s)
    }

    /// [`Nvme::submit_on`] on the admin queue, keeping the completion's `result`
    /// dword - which is where `SET FEATURES` reports what it actually granted.
    fn submit_result(&self, qid: u16, sqe: Sqe) -> Option<(u16, u32)> {
        self.submit_full(qid, &self.admin, sqe)
    }

    fn submit_full(&self, qid: u16, qcell: &SpinLock<Queue>, mut sqe: Sqe) -> Option<(u16, u32)> {
        let tail = {
            let mut q = qcell.lock();
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
            let q = qcell.lock();
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
                    let mut q = qcell.lock();
                    q.cq_head = (head + 1) % QDEPTH;
                    if q.cq_head == 0 {
                        q.phase = !q.phase;
                    }
                    q.cq_head
                };
                self.ring_cq(qid, new_head);
                return Some((status, cqe.result));
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

    /// This CPU's I/O channel.
    ///
    /// The index is the CPU index - not a round-robin, not a hash. A core's queue
    /// is *its* queue, which is what makes the rings lock-free and what
    /// [`cross_core_submits`] measures.
    ///
    /// A core with no channel of its own gets **`None`**, and the caller reports
    /// `BlkError::NoDevice`. It used to fall back to channel 0 with the fallback
    /// counted, which reads as a sensible degraded mode and is not one: two cores
    /// on one ring is a data race, and it does not present as an error but as
    /// *wrong bytes*. It was found exactly that way - ARM64 enumerated one CPU
    /// while four ran, so both cores took channel 0 and the same sector read back
    /// differently on round 3 with no fault and no log. So the counter records
    /// something that must be impossible rather than merely undesirable.
    fn chan(&self) -> Option<(usize, &Chan)> {
        let cpu = crate::smp::cpu_index();
        if cpu >= self.nio {
            CROSS_CORE.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        self.io.get(cpu).and_then(|c| c.as_ref()).map(|c| (cpu, c))
    }

    /// One `NVM READ`/`NVM WRITE` of `XFER` bytes at logical block `lba`, through
    /// this CPU's own queue pair and bounce frame.
    fn rw_page(&self, write: bool, lba: u64, blocks: u16) -> Result<(), BlkError> {
        let Some((idx, ch)) = self.chan() else {
            return Err(BlkError::NoDevice);
        };
        let bounce_pa = ch.bounce_pa;
        SUBMITS[idx].fetch_add(1, Ordering::Relaxed);
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
        // Queue ids are 1-based (0 is admin), so CPU `idx` owns queue `idx + 1`.
        match self.submit_on(idx as u16 + 1, &ch.q, sqe) {
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
        match self.chan() {
            Some((_, ch)) => Ok(ch.bounce_va),
            None => Err(BlkError::NoDevice),
        }
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
    let admin_sq_pa = admin.sq_pa;
    let admin_cq_pa = admin.cq_pa;
    // A scratch frame for the admin `IDENTIFY` reply, before any I/O channel
    // exists. Freed once its answer has been read out.
    let ident_pa = frames::alloc()? as u64;
    let mut c = Nvme {
        regs,
        stride: 4,
        nlba: 0,
        lba_bytes: 0,
        admin: SpinLock::new(admin),
        io: [const { None }; MAX_IOQ],
        nio: 0,
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

    // **Ask for one I/O queue pair per possible CPU** (docs/SUBSTRATE.md S5).
    //
    // `MAX_IOQ`, which is the size of the index space `smp::cpu_index()` draws
    // from - not `inventory().ncpus`, which is a count of a different thing. The
    // channel is selected *by CPU index*, so what has to be covered is every index
    // that can be returned, and sizing by the CPU count only coincides with that
    // when the two agree. They did not: ARM64 reported one CPU while four ran, so
    // cores 1..3 had no channel (the enumeration is fixed now - PSCI
    // `AFFINITY_INFO`, `arch::discover` - but the driver should not have been
    // relying on it being right in the first place). Eight queue pairs cost 24
    // frames, which is not a reason to be clever.
    //
    // Both `SET FEATURES` fields are zero-based, so `want - 1`. The controller
    // answers with what it granted, which may be fewer, so the count is *read back*
    // rather than assumed.
    let cpus = MAX_IOQ;
    let sqe = Sqe {
        cdw0: ADM_SET_FEATURES as u32,
        cdw10: FEAT_NUM_QUEUES,
        cdw11: ((cpus as u32 - 1) << 16) | (cpus as u32 - 1),
        ..Default::default()
    };
    let granted = match c.submit_result(0, sqe) {
        Some((0, result)) => {
            // Both halves are zero-based; take the smaller, since a pair needs both.
            let sq = (result & 0xFFFF) as usize + 1;
            let cq = ((result >> 16) & 0xFFFF) as usize + 1;
            sq.min(cq).min(cpus)
        }
        _ => {
            crate::println!("nvme: SET FEATURES (number of queues) failed");
            return None;
        }
    };

    // IDENTIFY namespace 1 into the bounce frame: NSZE (size in logical blocks) at
    // byte 0, FLBAS at 26, the LBA format table at 128.
    let sqe = Sqe {
        cdw0: ADM_IDENTIFY as u32,
        nsid: 1,
        prp1_lo: ident_pa as u32,
        prp1_hi: (ident_pa >> 32) as u32,
        cdw10: 0, // CNS 0 = identify namespace
        ..Default::default()
    };
    if c.submit_on(0, &c.admin, sqe) != Some(0) {
        crate::println!("nvme: IDENTIFY namespace failed");
        return None;
    }
    // SAFETY: the controller just filled the bounce frame, which is mapped and
    // 4096 bytes long; every offset read here is inside it.
    unsafe {
        let p = arch::phys_to_virt(ident_pa as usize) as *const u8;
        c.nlba = (p as *const u64).read_unaligned();
        let flbas = (p.add(26).read()) & 0xF;
        // Each LBA format is 4 bytes; LBADS (log2 of the block size) is byte 2.
        let lbads = p.add(128 + 4 * flbas as usize + 2).read();
        c.lba_bytes = 1u32 << lbads;
    }

    frames::free(ident_pa as usize);

    // One queue pair per granted slot, CPU `i` owning queue id `i + 1`.
    for i in 0..granted {
        let q = Queue::alloc()?;
        let bounce_pa = frames::alloc()? as u64;
        let qid = i as u32 + 1;
        // The completion queue must exist first: a submission queue names the
        // completion queue it reports into, so the reverse order is rejected.
        let sqe = Sqe {
            cdw0: ADM_CREATE_CQ as u32,
            prp1_lo: q.cq_pa as u32,
            prp1_hi: (q.cq_pa >> 32) as u32,
            cdw10: ((QDEPTH as u32 - 1) << 16) | qid,
            cdw11: 1, // physically contiguous, interrupts disabled (polled)
            ..Default::default()
        };
        if c.submit_on(0, &c.admin, sqe) != Some(0) {
            crate::println!("nvme: CREATE IO COMPLETION QUEUE {qid} failed");
            return None;
        }
        let sqe = Sqe {
            cdw0: ADM_CREATE_SQ as u32,
            prp1_lo: q.sq_pa as u32,
            prp1_hi: (q.sq_pa >> 32) as u32,
            cdw10: ((QDEPTH as u32 - 1) << 16) | qid,
            cdw11: (qid << 16) | 1, // reports into CQ `qid`, contiguous
            ..Default::default()
        };
        if c.submit_on(0, &c.admin, sqe) != Some(0) {
            crate::println!("nvme: CREATE IO SUBMISSION QUEUE {qid} failed");
            return None;
        }
        c.io[i] = Some(Chan {
            q: SpinLock::new(q),
            bounce_va: arch::phys_to_virt(bounce_pa as usize),
            bounce_pa,
        });
    }
    c.nio = granted;

    crate::println!(
        "nvme: {:04x}:{:04x} up - {} blocks of {} bytes, doorbell stride {}, \
         {granted} I/O queue pair(s) (one per possible CPU, {} enumerated)",
        dev.vendor,
        dev.device,
        c.nlba,
        c.lba_bytes,
        c.stride,
        super::inventory().ncpus
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
