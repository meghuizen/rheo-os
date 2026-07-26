//! A virtio-rng (entropy) driver feeding the kernel's entropy pool
//! (docs/TIME-IDENTITY.md 4). virtio-rng is the simplest virtio device:
//! one virtqueue, no configuration space, no feature bits beyond
//! VERSION_1; the driver posts a device-writable buffer and the host
//! fills it with random bytes.
//!
//! Transports mirror hw::virtio_blk: **virtio-mmio** on the arm/riscv
//! `virt` machines and **virtio-pci via the VIRTIO_PCI_CAP_PCI_CFG
//! config-space window** on x86-64 q35 (no BAR mapping needed - see the
//! virtio_blk module doc for why that matters under PVH boot). The two
//! drivers deliberately keep the same file shape; when a third virtio
//! device lands, the shared transport should be factored out rather than
//! copied a third time.
//!
//! Single-vcore, polled. The buffer and virtqueue live in identity-mapped
//! kernel RAM, so their addresses are the physical addresses the device
//! DMAs to.

use crate::arch;
use core::sync::atomic::{Ordering, fence};

// virtio-mmio register offsets (modern / version 2).
const MAGIC: usize = 0x000;
const VERSION: usize = 0x004;
const DEVICE_ID: usize = 0x008;
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

const MAGIC_VALUE: u32 = 0x7472_6976; // "virt"
const DEV_ENTROPY: u32 = 4;

// Status bits (shared by both transports).
const S_ACK: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;

const VRING_DESC_F_WRITE: u16 = 2;

const QSIZE: usize = 2;

// -------------------------------------------------- virtio-pci constants

const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
// Modern virtio-rng (device type 4): 0x1040 + 4.
const PCI_DEVICE_VIRTIO_RNG: u16 = 0x1044;

const PCI_COMMAND: u16 = 0x04;
const PCI_CMD_MEMORY: u32 = 1 << 1;
const PCI_CMD_MASTER: u32 = 1 << 2;
const PCI_CAP_PTR: u16 = 0x34;
const PCI_STATUS_CAP_LIST: u32 = 1 << 4;

const CAP_ID_VENDOR: u32 = 0x09;
const VIRTIO_CAP_COMMON: u32 = 1;
const VIRTIO_CAP_NOTIFY: u32 = 2;
const VIRTIO_CAP_PCI: u32 = 5;

const CC_DRIVER_FEATURE_SELECT: u32 = 8;
const CC_DRIVER_FEATURE: u32 = 12;
const CC_DEVICE_STATUS: u32 = 20;
const CC_QUEUE_SELECT: u32 = 22;
const CC_QUEUE_SIZE: u32 = 24;
const CC_QUEUE_ENABLE: u32 = 28;
const CC_QUEUE_NOTIFY_OFF: u32 = 30;
const CC_QUEUE_DESC: u32 = 32;
const CC_QUEUE_DRIVER: u32 = 40;
const CC_QUEUE_DEVICE: u32 = 48;

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

#[repr(C, align(4096))]
struct VirtQueue {
    desc: [Desc; QSIZE],
    avail: Avail,
    used: Used,
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
/// DMA buffer the device writes entropy into.
static mut ENTROPY_BUF: [u8; 64] = [0; 64];
static mut LAST_USED: u16 = 0;

unsafe fn r32(base: usize, off: usize) -> u32 {
    unsafe { ((base + off) as *const u32).read_volatile() }
}
unsafe fn w32(base: usize, off: usize, v: u32) {
    unsafe { ((base + off) as *mut u32).write_volatile(v) }
}

/// The virtio-pci transport, driven through the PCI_CFG window (see
/// virtio_blk::PciXport - same mechanism, rng flavour).
struct PciXport {
    bus: u8,
    dev: u8,
    func: u8,
    pci_cfg: u16,
    common_bar: u8,
    common_off: u32,
    notify_bar: u8,
    notify_off: u32,
}

impl PciXport {
    fn cfg_w(&self, off: u16, v: u32) {
        arch::pci_cfg_write32(0, self.bus, self.dev, self.func, off, v);
    }
    fn cfg_r(&self, off: u16) -> u32 {
        arch::pci_cfg_read32(0, self.bus, self.dev, self.func, off)
    }
    fn win(&self, bar: u8, off: u32, len: u32) {
        self.cfg_w(self.pci_cfg + 4, bar as u32);
        self.cfg_w(self.pci_cfg + 12, len);
        self.cfg_w(self.pci_cfg + 8, off);
    }
    fn read(&self, bar: u8, off: u32, len: u32) -> u32 {
        self.win(bar, off, len);
        let d = self.cfg_r(self.pci_cfg + 16);
        if len == 4 {
            d
        } else {
            d & ((1u32 << (len * 8)) - 1)
        }
    }
    fn write(&self, bar: u8, off: u32, len: u32, val: u32) {
        self.win(bar, off, len);
        self.cfg_w(self.pci_cfg + 16, val);
    }
    fn cc_w32(&self, field: u32, v: u32) {
        self.write(self.common_bar, self.common_off + field, 4, v);
    }
    fn cc_r16(&self, field: u32) -> u16 {
        self.read(self.common_bar, self.common_off + field, 2) as u16
    }
    fn cc_w16(&self, field: u32, v: u16) {
        self.write(self.common_bar, self.common_off + field, 2, v as u32);
    }
    fn cc_r8(&self, field: u32) -> u8 {
        self.read(self.common_bar, self.common_off + field, 1) as u8
    }
    fn cc_w8(&self, field: u32, v: u8) {
        self.write(self.common_bar, self.common_off + field, 1, v as u32);
    }
    fn notify_q0(&self) {
        self.write(self.notify_bar, self.notify_off, 2, 0);
    }
}

enum Transport {
    Mmio { base: usize },
    Pci(PciXport),
}

impl Transport {
    fn notify_q0(&self) {
        match self {
            // SAFETY: `base` matched the virtio-mmio magic during probe.
            Transport::Mmio { base } => unsafe { w32(*base, QUEUE_NOTIFY, 0) },
            Transport::Pci(p) => p.notify_q0(),
        }
    }
}

/// A discovered virtio-rng device.
pub struct VirtioRng {
    transport: Transport,
}

/// Find and initialise the first virtio-rng device, virtio-mmio first
/// (arm/riscv `virt`), then virtio-pci (x86 q35).
pub fn probe() -> Option<VirtioRng> {
    probe_mmio().or_else(probe_pci)
}

fn probe_mmio() -> Option<VirtioRng> {
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
            if r32(base, DEVICE_ID) != DEV_ENTROPY {
                continue;
            }
            if let Some(dev) = init_mmio(base) {
                return Some(dev);
            }
        }
    }
    None
}

/// # Safety
/// `base` must be a virtio-mmio entropy device that magic/version/id matched.
unsafe fn init_mmio(base: usize) -> Option<VirtioRng> {
    unsafe {
        w32(base, STATUS, 0); // reset
        let mut status = S_ACK;
        w32(base, STATUS, status);
        status |= S_DRIVER;
        w32(base, STATUS, status);

        // Negotiate: require VIRTIO_F_VERSION_1 (bit 32 -> sel 1, bit 0).
        w32(base, DRIVER_FEATURES_SEL, 1);
        w32(base, DRIVER_FEATURES, 1);
        w32(base, DRIVER_FEATURES_SEL, 0);
        w32(base, DRIVER_FEATURES, 0);

        status |= S_FEATURES_OK;
        w32(base, STATUS, status);
        if r32(base, STATUS) & S_FEATURES_OK == 0 {
            return None;
        }

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

        LAST_USED = (*core::ptr::addr_of!(VQ)).used.idx;
        Some(VirtioRng {
            transport: Transport::Mmio { base },
        })
    }
}

fn cfg_read8(bus: u8, dev: u8, func: u8, off: u16) -> u8 {
    let d = arch::pci_cfg_read32(0, bus, dev, func, off & !3);
    ((d >> ((off & 3) * 8)) & 0xFF) as u8
}

fn probe_pci() -> Option<VirtioRng> {
    for dev in 0u8..32 {
        for func in 0u8..8 {
            let id = arch::pci_cfg_read32(0, 0, dev, func, 0x00);
            if (id & 0xFFFF) as u16 != PCI_VENDOR_VIRTIO {
                continue;
            }
            if (id >> 16) as u16 != PCI_DEVICE_VIRTIO_RNG {
                continue;
            }
            if let Some(d) = init_pci(0, dev, func) {
                return Some(d);
            }
        }
    }
    None
}

fn init_pci(bus: u8, dev: u8, func: u8) -> Option<VirtioRng> {
    let status_cmd = arch::pci_cfg_read32(0, bus, dev, func, PCI_COMMAND);
    if status_cmd & (PCI_STATUS_CAP_LIST << 16) == 0 {
        return None;
    }

    let mut common: Option<(u8, u32)> = None;
    let mut notify: Option<(u8, u32)> = None;
    let mut notify_mult: u32 = 0;
    let mut pci_cfg: Option<u16> = None;

    let mut cap = cfg_read8(bus, dev, func, PCI_CAP_PTR) as u16;
    let mut guard = 0;
    while cap != 0 && cap != 0xFF && guard < 48 {
        guard += 1;
        let hdr = arch::pci_cfg_read32(0, bus, dev, func, cap);
        let id = hdr & 0xFF;
        let next = (hdr >> 8) & 0xFF;
        let cfg_type = (hdr >> 24) & 0xFF;
        if id == CAP_ID_VENDOR {
            let bar = (arch::pci_cfg_read32(0, bus, dev, func, cap + 4) & 0xFF) as u8;
            let offset = arch::pci_cfg_read32(0, bus, dev, func, cap + 8);
            match cfg_type {
                VIRTIO_CAP_COMMON => common = Some((bar, offset)),
                VIRTIO_CAP_NOTIFY => {
                    notify = Some((bar, offset));
                    notify_mult = arch::pci_cfg_read32(0, bus, dev, func, cap + 16);
                }
                VIRTIO_CAP_PCI => pci_cfg = Some(cap),
                _ => {}
            }
        }
        cap = next as u16;
    }

    let (common_bar, common_off) = common?;
    let (notify_bar, notify_base) = notify?;
    let pci_cfg = pci_cfg?;

    let mut x = PciXport {
        bus,
        dev,
        func,
        pci_cfg,
        common_bar,
        common_off,
        notify_bar,
        notify_off: 0,
    };

    let cmd = arch::pci_cfg_read32(0, bus, dev, func, PCI_COMMAND);
    arch::pci_cfg_write32(
        0,
        bus,
        dev,
        func,
        PCI_COMMAND,
        cmd | PCI_CMD_MEMORY | PCI_CMD_MASTER,
    );

    x.cc_w8(CC_DEVICE_STATUS, 0);
    x.cc_w8(CC_DEVICE_STATUS, S_ACK as u8);
    x.cc_w8(CC_DEVICE_STATUS, (S_ACK | S_DRIVER) as u8);

    x.cc_w32(CC_DRIVER_FEATURE_SELECT, 1);
    x.cc_w32(CC_DRIVER_FEATURE, 1);
    x.cc_w32(CC_DRIVER_FEATURE_SELECT, 0);
    x.cc_w32(CC_DRIVER_FEATURE, 0);

    x.cc_w8(CC_DEVICE_STATUS, (S_ACK | S_DRIVER | S_FEATURES_OK) as u8);
    if x.cc_r8(CC_DEVICE_STATUS) & S_FEATURES_OK as u8 == 0 {
        return None;
    }

    x.cc_w16(CC_QUEUE_SELECT, 0);
    if (x.cc_r16(CC_QUEUE_SIZE) as usize) < QSIZE {
        return None;
    }
    x.cc_w16(CC_QUEUE_SIZE, QSIZE as u16);

    // SAFETY: single-vcore init; the static virtqueue is set up once here.
    let (desc_pa, avail_pa, used_pa) = unsafe {
        let vq = core::ptr::addr_of!(VQ);
        (
            core::ptr::addr_of!((*vq).desc) as u64,
            core::ptr::addr_of!((*vq).avail) as u64,
            core::ptr::addr_of!((*vq).used) as u64,
        )
    };
    x.cc_w32(CC_QUEUE_DESC, desc_pa as u32);
    x.cc_w32(CC_QUEUE_DESC + 4, (desc_pa >> 32) as u32);
    x.cc_w32(CC_QUEUE_DRIVER, avail_pa as u32);
    x.cc_w32(CC_QUEUE_DRIVER + 4, (avail_pa >> 32) as u32);
    x.cc_w32(CC_QUEUE_DEVICE, used_pa as u32);
    x.cc_w32(CC_QUEUE_DEVICE + 4, (used_pa >> 32) as u32);

    let qnotify = x.cc_r16(CC_QUEUE_NOTIFY_OFF) as u32;
    x.notify_off = notify_base + qnotify * notify_mult;

    x.cc_w16(CC_QUEUE_ENABLE, 1);
    x.cc_w8(
        CC_DEVICE_STATUS,
        (S_ACK | S_DRIVER | S_FEATURES_OK | S_DRIVER_OK) as u8,
    );

    // SAFETY: single-vcore; initialise the used-ring watermark.
    unsafe {
        LAST_USED = (*core::ptr::addr_of!(VQ)).used.idx;
    }
    Some(VirtioRng {
        transport: Transport::Pci(x),
    })
}

impl VirtioRng {
    /// Ask the device for up to `buf.len()` (max 64) random bytes. Returns
    /// how many bytes were delivered (0 on timeout or empty completion).
    pub fn fill(&self, buf: &mut [u8]) -> usize {
        let want = core::cmp::min(buf.len(), 64);
        if want == 0 {
            return 0;
        }
        // SAFETY: single-vcore; the static virtqueue/buffer are only
        // touched here, and the device only writes ENTROPY_BUF while the
        // request is in flight (we poll it to completion below).
        unsafe {
            let vq = core::ptr::addr_of_mut!(VQ);
            let buf_pa = core::ptr::addr_of!(ENTROPY_BUF) as u64;
            (*vq).desc[0] = Desc {
                addr: buf_pa,
                len: want as u32,
                flags: VRING_DESC_F_WRITE,
                next: 0,
            };
            let idx = (*vq).avail.idx;
            (*vq).avail.ring[(idx as usize) % QSIZE] = 0;
            fence(Ordering::SeqCst);
            (*vq).avail.idx = idx.wrapping_add(1);
            fence(Ordering::SeqCst);

            self.transport.notify_q0();

            let mut spins = 0u64;
            while (*vq).used.idx == *core::ptr::addr_of!(LAST_USED) {
                fence(Ordering::SeqCst);
                spins += 1;
                if spins > 100_000_000 {
                    return 0;
                }
                core::hint::spin_loop();
            }
            let used = (*vq).used.idx;
            let got = (*vq).used.ring[(used.wrapping_sub(1) as usize) % QSIZE].len as usize;
            *core::ptr::addr_of_mut!(LAST_USED) = used;

            let n = core::cmp::min(got, want);
            let src = core::ptr::addr_of!(ENTROPY_BUF) as *const u8;
            for (i, b) in buf[..n].iter_mut().enumerate() {
                *b = src.add(i).read_volatile();
            }
            n
        }
    }
}
