//! A virtio-blk driver (virtio 1.0 "modern") implementing `BlockDevice`, over
//! either of two transports:
//!
//! - **virtio-mmio** - QEMU's `virt` machines (arm/riscv) expose it at fixed
//!   addresses (per-ISA constants in `arch`). Register access is plain MMIO.
//! - **virtio-pci** - QEMU q35 (x86-64) has no virtio-mmio; virtio is a PCIe
//!   device there. We drive it *entirely through PCI configuration space*
//!   using the `VIRTIO_PCI_CAP_PCI_CFG` capability (virtio spec 4.1.4.8): the
//!   device services BAR-relative reads/writes for us via a config-space
//!   window, so no BAR needs to be assigned or mapped. This matters because
//!   there is no firmware under PVH boot to program the BARs, and the kernel
//!   only identity-maps the low 1 GiB (the q35 PCI window sits above it).
//!
//! Both transports share one virtqueue and one block-request path; they differ
//! only in how registers are read/written and how the device is notified.
//! Single-vcore, polled (no interrupt path yet). DMA has no IOMMU in QEMU, so
//! the device uses physical addresses; the driver hands it `arch::virt_to_phys`
//! of each ring/buffer VA (the identity on x86/riscv, the high linear-map
//! offset on the aarch64 higher-half kernel - docs/MEMORY.md).

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

// Status bits (shared by both transports).
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

// -------------------------------------------------- virtio-pci constants

const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
// Modern virtio-blk (device type 2): 0x1040 + 2. QEMU presents this ID when
// the device is created with `disable-legacy=on`.
const PCI_DEVICE_VIRTIO_BLK: u16 = 0x1042;

const PCI_COMMAND: u16 = 0x04;
const PCI_CMD_MEMORY: u32 = 1 << 1;
const PCI_CMD_MASTER: u32 = 1 << 2;
const PCI_CAP_PTR: u16 = 0x34;
const PCI_STATUS_CAP_LIST: u32 = 1 << 4;

const CAP_ID_VENDOR: u32 = 0x09;
// virtio_pci_cap.cfg_type values.
const VIRTIO_CAP_COMMON: u32 = 1;
const VIRTIO_CAP_NOTIFY: u32 = 2;
const VIRTIO_CAP_DEVICE: u32 = 4;
const VIRTIO_CAP_PCI: u32 = 5;

// virtio_pci_common_cfg field offsets (bytes).
const CC_DEVICE_FEATURE_SELECT: u32 = 0;
const CC_DEVICE_FEATURE: u32 = 4;
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

/// The virtio-pci transport: a device on the PCI bus driven through the
/// `VIRTIO_PCI_CAP_PCI_CFG` config-space window. Holds the located virtio
/// capability regions (BAR + offset) and the queue-0 notify address.
struct PciXport {
    bus: u8,
    dev: u8,
    func: u8,
    /// Config-space offset of the VIRTIO_PCI_CAP_PCI_CFG capability - the
    /// window we route all BAR accesses through.
    pci_cfg: u16,
    common_bar: u8,
    common_off: u32,
    notify_bar: u8,
    /// Absolute offset within `notify_bar` to poke for queue 0.
    notify_off: u32,
}

impl PciXport {
    fn cfg_w(&self, off: u16, v: u32) {
        arch::pci_cfg_write32(0, self.bus, self.dev, self.func, off, v);
    }
    fn cfg_r(&self, off: u16) -> u32 {
        arch::pci_cfg_read32(0, self.bus, self.dev, self.func, off)
    }

    /// Point the PCI_CFG window at `(bar, off)` for a `len`-byte access.
    fn win(&self, bar: u8, off: u32, len: u32) {
        // virtio_pci_cap layout: bar @ cap+4 (low byte), offset @ cap+8,
        // length @ cap+12, pci_cfg_data @ cap+16 (all DWORD-aligned).
        self.cfg_w(self.pci_cfg + 4, bar as u32);
        self.cfg_w(self.pci_cfg + 12, len);
        self.cfg_w(self.pci_cfg + 8, off);
    }

    /// Read `len` (1/2/4) bytes from `(bar, off)`. QEMU returns the value in
    /// the low `len` bytes of pci_cfg_data (no offset-in-dword shifting).
    fn read(&self, bar: u8, off: u32, len: u32) -> u32 {
        self.win(bar, off, len);
        let d = self.cfg_r(self.pci_cfg + 16);
        if len == 4 {
            d
        } else {
            d & ((1u32 << (len * 8)) - 1)
        }
    }

    /// Write `len` (1/2/4) bytes to `(bar, off)` (value in the low bytes).
    fn write(&self, bar: u8, off: u32, len: u32, val: u32) {
        self.win(bar, off, len);
        self.cfg_w(self.pci_cfg + 16, val);
    }

    // Common-configuration accessors (width matches the field).
    fn cc_w32(&self, field: u32, v: u32) {
        self.write(self.common_bar, self.common_off + field, 4, v);
    }
    fn cc_r32(&self, field: u32) -> u32 {
        self.read(self.common_bar, self.common_off + field, 4)
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
        // Modern notify: write the virtqueue index (0) at the notify offset.
        self.write(self.notify_bar, self.notify_off, 2, 0);
    }
}

/// How a discovered device is reached.
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

/// A discovered virtio-blk device.
pub struct VirtioBlk {
    transport: Transport,
    capacity: u64,
}

/// Find and initialise the first virtio-blk device on this machine, trying
/// virtio-mmio (arm/riscv `virt`) first, then virtio-pci (x86 q35).
pub fn probe() -> Option<VirtioBlk> {
    probe_mmio().or_else(probe_pci)
}

/// Scan the virtio-mmio slots for a block device and initialise the first one.
fn probe_mmio() -> Option<VirtioBlk> {
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
            if let Some(dev) = init_mmio(base) {
                return Some(dev);
            }
        }
    }
    None
}

/// # Safety
/// `base` must be a virtio-mmio block device that magic/version/id matched.
unsafe fn init_mmio(base: usize) -> Option<VirtioBlk> {
    unsafe {
        w32(base, STATUS, 0); // reset
        let mut status = S_ACK;
        w32(base, STATUS, status);
        status |= S_DRIVER;
        w32(base, STATUS, status);

        // Negotiate: require VIRTIO_F_VERSION_1 (feature bit 32 -> sel 1,
        // bit 0), and also ack VIRTIO_F_ACCESS_PLATFORM (bit 33 -> sel 1,
        // bit 1) when the device offers it - the bit that makes the device
        // route its DMA through a platform IOMMU (docs/GPU-HARDWARE.md 4).
        // When `iommu_platform=off` (every other test) the device does not
        // offer it, so `ap` is 0 and behaviour is unchanged.
        w32(base, DEVICE_FEATURES_SEL, 1);
        let ap = r32(base, DEVICE_FEATURES) & 0x2;
        w32(base, DRIVER_FEATURES_SEL, 1);
        w32(base, DRIVER_FEATURES, 1 | ap);
        w32(base, DRIVER_FEATURES_SEL, 0);
        w32(base, DRIVER_FEATURES, 0);

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
        let desc_pa = arch::virt_to_phys(core::ptr::addr_of!((*vq).desc) as usize) as u64;
        let avail_pa = arch::virt_to_phys(core::ptr::addr_of!((*vq).avail) as usize) as u64;
        let used_pa = arch::virt_to_phys(core::ptr::addr_of!((*vq).used) as usize) as u64;
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
            transport: Transport::Mmio { base },
            capacity: cap_lo | (cap_hi << 32),
        })
    }
}

/// Scan PCI bus 0 for a modern virtio-blk device and initialise it.
fn probe_pci() -> Option<VirtioBlk> {
    for dev in 0u8..32 {
        for func in 0u8..8 {
            let id = arch::pci_cfg_read32(0, 0, dev, func, 0x00);
            let vendor = (id & 0xFFFF) as u16;
            if vendor != PCI_VENDOR_VIRTIO {
                continue;
            }
            if (id >> 16) as u16 != PCI_DEVICE_VIRTIO_BLK {
                continue;
            }
            if let Some(d) = init_pci(0, dev, func) {
                return Some(d);
            }
        }
    }
    None
}

/// Read one config-space byte (config access is DWORD-granular).
fn cfg_read8(bus: u8, dev: u8, func: u8, off: u16) -> u8 {
    let d = arch::pci_cfg_read32(0, bus, dev, func, off & !3);
    ((d >> ((off & 3) * 8)) & 0xFF) as u8
}

/// Initialise a virtio-blk device found on the PCI bus. Walks the virtio PCI
/// capabilities, then runs the modern handshake through the PCI_CFG window.
fn init_pci(bus: u8, dev: u8, func: u8) -> Option<VirtioBlk> {
    // A capability list must be present (PCI_STATUS bit 4; PCI_STATUS is the
    // high half of the 0x04 dword, so the bit is at 16 + 4).
    let status_cmd = arch::pci_cfg_read32(0, bus, dev, func, PCI_COMMAND);
    if status_cmd & (PCI_STATUS_CAP_LIST << 16) == 0 {
        return None;
    }

    // Locate the virtio capabilities.
    let mut common: Option<(u8, u32)> = None;
    let mut notify: Option<(u8, u32)> = None;
    let mut notify_mult: u32 = 0;
    let mut device: Option<(u8, u32)> = None;
    let mut pci_cfg: Option<u16> = None;

    let mut cap = cfg_read8(bus, dev, func, PCI_CAP_PTR) as u16;
    let mut guard = 0;
    while cap != 0 && cap != 0xFF && guard < 48 {
        guard += 1;
        let hdr = arch::pci_cfg_read32(0, bus, dev, func, cap); // [id,next,len,cfg_type]
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
                VIRTIO_CAP_DEVICE => device = Some((bar, offset)),
                VIRTIO_CAP_PCI => pci_cfg = Some(cap),
                _ => {}
            }
        }
        cap = next as u16;
    }

    let (common_bar, common_off) = common?;
    let (notify_bar, notify_base) = notify?;
    let (device_bar, device_off) = device?;
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

    // Enable memory-space decoding and bus mastering (DMA needs MASTER).
    let cmd = arch::pci_cfg_read32(0, bus, dev, func, PCI_COMMAND);
    arch::pci_cfg_write32(
        0,
        bus,
        dev,
        func,
        PCI_COMMAND,
        cmd | PCI_CMD_MEMORY | PCI_CMD_MASTER,
    );

    // Reset, then ACK + DRIVER.
    x.cc_w8(CC_DEVICE_STATUS, 0);
    x.cc_w8(CC_DEVICE_STATUS, S_ACK as u8);
    x.cc_w8(CC_DEVICE_STATUS, (S_ACK | S_DRIVER) as u8);

    // Require VIRTIO_F_VERSION_1 (feature bit 32 -> select 1, bit 0), and
    // ack VIRTIO_F_ACCESS_PLATFORM (bit 33 -> select 1, bit 1) when offered
    // - the bit that routes device DMA through a platform IOMMU
    // (docs/GPU-HARDWARE.md 4). Not offered under `iommu_platform=off`, so
    // `ap` is 0 and every other test is unchanged.
    x.cc_w32(CC_DEVICE_FEATURE_SELECT, 1);
    let ap = x.cc_r32(CC_DEVICE_FEATURE) & 0x2;
    x.cc_w32(CC_DRIVER_FEATURE_SELECT, 1);
    x.cc_w32(CC_DRIVER_FEATURE, 1 | ap);
    x.cc_w32(CC_DRIVER_FEATURE_SELECT, 0);
    x.cc_w32(CC_DRIVER_FEATURE, 0);

    x.cc_w8(CC_DEVICE_STATUS, (S_ACK | S_DRIVER | S_FEATURES_OK) as u8);
    if x.cc_r8(CC_DEVICE_STATUS) & S_FEATURES_OK as u8 == 0 {
        return None; // device rejected our feature set
    }

    // Queue 0 setup.
    x.cc_w16(CC_QUEUE_SELECT, 0);
    if (x.cc_r16(CC_QUEUE_SIZE) as usize) < QSIZE {
        return None;
    }
    x.cc_w16(CC_QUEUE_SIZE, QSIZE as u16);

    // SAFETY: single-vcore init; the static virtqueue is set up once here.
    let (desc_pa, avail_pa, used_pa) = unsafe {
        let vq = core::ptr::addr_of!(VQ);
        (
            arch::virt_to_phys(core::ptr::addr_of!((*vq).desc) as usize) as u64,
            arch::virt_to_phys(core::ptr::addr_of!((*vq).avail) as usize) as u64,
            arch::virt_to_phys(core::ptr::addr_of!((*vq).used) as usize) as u64,
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

    // Capacity: virtio-blk device config, u64 sectors at offset 0.
    let cap_lo = x.read(device_bar, device_off, 4) as u64;
    let cap_hi = x.read(device_bar, device_off + 4, 4) as u64;

    // SAFETY: single-vcore; initialise the used-ring watermark.
    unsafe {
        LAST_USED = (*core::ptr::addr_of!(VQ)).used.idx;
    }
    Some(VirtioBlk {
        transport: Transport::Pci(x),
        capacity: cap_lo | (cap_hi << 32),
    })
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

            let hdr_pa = arch::virt_to_phys(core::ptr::addr_of!(HDR) as usize) as u64;
            let status_pa = arch::virt_to_phys(core::ptr::addr_of!(STATUS_BYTE) as usize) as u64;

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

            self.transport.notify_q0();

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
        let pa = arch::virt_to_phys(buf.as_ptr() as usize) as u64;
        self.request(BLK_T_IN, sector, pa, buf.len() as u32)
    }

    fn write(&self, sector: u64, buf: &[u8]) -> Result<(), BlkError> {
        if buf.is_empty() || !buf.len().is_multiple_of(SECTOR) {
            return Err(BlkError::Inval);
        }
        let pa = arch::virt_to_phys(buf.as_ptr() as usize) as u64;
        self.request(BLK_T_OUT, sector, pa, buf.len() as u32)
    }
}
