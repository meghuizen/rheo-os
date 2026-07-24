//! A virtio-blk driver over the virtio-mmio transport (virtio 1.0 "modern"),
//! implementing `BlockDevice`. This is the OS's first real storage driver: a
//! filesystem can now read a live disk instead of an embedded image.
//!
//! Transport: virtio-mmio, which QEMU's `virt` machines expose at fixed
//! addresses (per-ISA constants in `arch`). x86-64 q35 has no virtio-mmio -
//! it needs the virtio-*pci* transport (BAR + capability parsing), which is a
//! follow-on; `probe` simply finds no device there. Single-vcore, polled (no
//! interrupt path yet). DMA has no IOMMU in QEMU, so the device uses physical
//! addresses; kernel RAM is identity-mapped, so a buffer's address *is* its
//! physical address.

use super::block::{BlkError, BlockDevice, SECTOR};
use crate::arch;
use core::sync::atomic::{Ordering, fence};

// virtio-mmio register offsets (modern / version 2).
const MAGIC: usize = 0x000;
const VERSION: usize = 0x004;
const DEVICE_ID: usize = 0x008;
const DEVICE_FEATURES: usize = 0x010;
const DEVICE_FEATURES_SEL: usize = 0x014;
const DRIVER_FEATURES: usize = 0x020;
const DRIVER_FEATURES_SEL: usize = 0x024;
const QUEUE_SEL: usize = 0x030;
const QUEUE_NUM_MAX: usize = 0x034;
const QUEUE_NUM: usize = 0x038;
const QUEUE_READY: usize = 0x044;
const QUEUE_NOTIFY: usize = 0x050;
const STATUS: usize = 0x070;
const QUEUE_DESC_LOW: usize = 0x080;
const QUEUE_DESC_HIGH: usize = 0x084;
const QUEUE_DRIVER_LOW: usize = 0x090;
const QUEUE_DRIVER_HIGH: usize = 0x094;
const QUEUE_DEVICE_LOW: usize = 0x0a0;
const QUEUE_DEVICE_HIGH: usize = 0x0a4;
const CONFIG: usize = 0x100;

const MAGIC_VALUE: u32 = 0x7472_6976; // "virt"
const DEV_BLOCK: u32 = 2;

// Status bits.
const S_ACK: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;

// Descriptor flags.
const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

// virtio-blk request types.
const BLK_T_IN: u32 = 0; // read
const BLK_T_OUT: u32 = 1; // write

const QSIZE: usize = 8;

#[repr(C)]
#[derive(Copy, Clone)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct Avail {
    flags: u16,
    idx: u16,
    ring: [u16; QSIZE],
    used_event: u16,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct UsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
struct Used {
    flags: u16,
    idx: u16,
    ring: [UsedElem; QSIZE],
    avail_event: u16,
}

/// The split virtqueue, page-aligned. Lives in identity-mapped kernel RAM so
/// its address is the physical address the device DMAs to.
#[repr(C, align(4096))]
struct VirtQueue {
    desc: [Desc; QSIZE],
    avail: Avail,
    used: Used,
}

#[repr(C)]
struct BlkReqHeader {
    kind: u32,
    reserved: u32,
    sector: u64,
}

static mut VQ: VirtQueue = VirtQueue {
    desc: [Desc {
        addr: 0,
        len: 0,
        flags: 0,
        next: 0,
    }; QSIZE],
    avail: Avail {
        flags: 0,
        idx: 0,
        ring: [0; QSIZE],
        used_event: 0,
    },
    used: Used {
        flags: 0,
        idx: 0,
        ring: [UsedElem { id: 0, len: 0 }; QSIZE],
        avail_event: 0,
    },
};
static mut HDR: BlkReqHeader = BlkReqHeader {
    kind: 0,
    reserved: 0,
    sector: 0,
};
static mut STATUS_BYTE: u8 = 0;
static mut LAST_USED: u16 = 0;

unsafe fn r32(base: usize, off: usize) -> u32 {
    unsafe { ((base + off) as *const u32).read_volatile() }
}
unsafe fn w32(base: usize, off: usize, v: u32) {
    unsafe { ((base + off) as *mut u32).write_volatile(v) }
}

/// A discovered virtio-blk device.
pub struct VirtioBlk {
    base: usize,
    capacity: u64,
}

/// Scan the virtio-mmio slots for a block device and initialise the first one.
pub fn probe() -> Option<VirtioBlk> {
    let count = arch::VIRTIO_MMIO_COUNT;
    for slot in 0..count {
        let base = arch::VIRTIO_MMIO_BASE + slot * arch::VIRTIO_MMIO_STRIDE;
        // SAFETY: `base` is a fixed MMIO address the kernel identity-maps.
        unsafe {
            if r32(base, MAGIC) != MAGIC_VALUE {
                continue;
            }
            if r32(base, VERSION) != 2 {
                continue; // only the modern transport
            }
            if r32(base, DEVICE_ID) != DEV_BLOCK {
                continue;
            }
            if let Some(dev) = init(base) {
                return Some(dev);
            }
        }
    }
    None
}

/// # Safety
/// `base` must be a virtio-mmio block device that magic/version/id matched.
unsafe fn init(base: usize) -> Option<VirtioBlk> {
    unsafe {
        w32(base, STATUS, 0); // reset
        let mut status = S_ACK;
        w32(base, STATUS, status);
        status |= S_DRIVER;
        w32(base, STATUS, status);

        // Negotiate: require VIRTIO_F_VERSION_1 (feature bit 32 -> sel 1, bit 0).
        w32(base, DRIVER_FEATURES_SEL, 1);
        w32(base, DRIVER_FEATURES, 1);
        w32(base, DRIVER_FEATURES_SEL, 0);
        w32(base, DRIVER_FEATURES, 0);
        let _ = (DEVICE_FEATURES, DEVICE_FEATURES_SEL);

        status |= S_FEATURES_OK;
        w32(base, STATUS, status);
        if r32(base, STATUS) & S_FEATURES_OK == 0 {
            return None; // device rejected our feature set
        }

        // Queue 0.
        w32(base, QUEUE_SEL, 0);
        if r32(base, QUEUE_NUM_MAX) < QSIZE as u32 {
            return None;
        }
        w32(base, QUEUE_NUM, QSIZE as u32);

        let vq = core::ptr::addr_of!(VQ);
        let desc_pa = core::ptr::addr_of!((*vq).desc) as u64;
        let avail_pa = core::ptr::addr_of!((*vq).avail) as u64;
        let used_pa = core::ptr::addr_of!((*vq).used) as u64;
        w32(base, QUEUE_DESC_LOW, desc_pa as u32);
        w32(base, QUEUE_DESC_HIGH, (desc_pa >> 32) as u32);
        w32(base, QUEUE_DRIVER_LOW, avail_pa as u32);
        w32(base, QUEUE_DRIVER_HIGH, (avail_pa >> 32) as u32);
        w32(base, QUEUE_DEVICE_LOW, used_pa as u32);
        w32(base, QUEUE_DEVICE_HIGH, (used_pa >> 32) as u32);
        w32(base, QUEUE_READY, 1);

        status |= S_DRIVER_OK;
        w32(base, STATUS, status);

        // Capacity is the first config field: u64 sectors at offset 0.
        let cap_lo = r32(base, CONFIG) as u64;
        let cap_hi = r32(base, CONFIG + 4) as u64;
        LAST_USED = (*core::ptr::addr_of!(VQ)).used.idx;
        Some(VirtioBlk {
            base,
            capacity: cap_lo | (cap_hi << 32),
        })
    }
}

impl VirtioBlk {
    /// One block request (read or write) covering `buf.len()` bytes.
    fn request(&self, kind: u32, sector: u64, addr: u64, len: u32) -> Result<(), BlkError> {
        // SAFETY: single-vcore; the static virtqueue is only touched here.
        unsafe {
            let vq = core::ptr::addr_of_mut!(VQ);
            *core::ptr::addr_of_mut!(HDR) = BlkReqHeader {
                kind,
                reserved: 0,
                sector,
            };
            *core::ptr::addr_of_mut!(STATUS_BYTE) = 0xff;

            let hdr_pa = core::ptr::addr_of!(HDR) as u64;
            let status_pa = core::ptr::addr_of!(STATUS_BYTE) as u64;

            // desc0: header (device-readable) -> desc1: data -> desc2: status.
            let data_write = if kind == BLK_T_IN {
                VRING_DESC_F_WRITE
            } else {
                0
            };
            (*vq).desc[0] = Desc {
                addr: hdr_pa,
                len: 16,
                flags: VRING_DESC_F_NEXT,
                next: 1,
            };
            (*vq).desc[1] = Desc {
                addr,
                len,
                flags: VRING_DESC_F_NEXT | data_write,
                next: 2,
            };
            (*vq).desc[2] = Desc {
                addr: status_pa,
                len: 1,
                flags: VRING_DESC_F_WRITE,
                next: 0,
            };

            let idx = (*vq).avail.idx;
            (*vq).avail.ring[(idx as usize) % QSIZE] = 0; // head descriptor
            fence(Ordering::SeqCst);
            (*vq).avail.idx = idx.wrapping_add(1);
            fence(Ordering::SeqCst);

            w32(self.base, QUEUE_NOTIFY, 0);

            // Poll the used ring (no interrupts yet).
            let mut spins = 0u64;
            while (*vq).used.idx == *core::ptr::addr_of!(LAST_USED) {
                fence(Ordering::SeqCst);
                spins += 1;
                if spins > 100_000_000 {
                    return Err(BlkError::Io);
                }
                core::hint::spin_loop();
            }
            *core::ptr::addr_of_mut!(LAST_USED) = (*vq).used.idx;

            if *core::ptr::addr_of!(STATUS_BYTE) == 0 {
                Ok(())
            } else {
                Err(BlkError::Io)
            }
        }
    }
}

impl BlockDevice for VirtioBlk {
    fn capacity_sectors(&self) -> u64 {
        self.capacity
    }

    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        if buf.is_empty() || !buf.len().is_multiple_of(SECTOR) {
            return Err(BlkError::Inval);
        }
        self.request(BLK_T_IN, sector, buf.as_ptr() as u64, buf.len() as u32)
    }

    fn write(&self, sector: u64, buf: &[u8]) -> Result<(), BlkError> {
        if buf.is_empty() || !buf.len().is_multiple_of(SECTOR) {
            return Err(BlkError::Inval);
        }
        self.request(BLK_T_OUT, sector, buf.as_ptr() as u64, buf.len() as u32)
    }
}
