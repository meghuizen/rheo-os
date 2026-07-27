//! A virtio-net driver (virtio 1.0 "modern"), mirroring `virtio_blk` over the
//! same two transports:
//!
//! - **virtio-mmio** - QEMU's `virt` machines (arm/riscv) expose it at fixed
//!   addresses (per-ISA constants in `arch`). Register access is plain MMIO.
//! - **virtio-pci** - QEMU q35 (x86-64) has no virtio-mmio; virtio-net is a
//!   PCIe device driven *entirely through PCI configuration space* using the
//!   `VIRTIO_PCI_CAP_PCI_CFG` capability (virtio spec 4.1.4.8), so no BAR needs
//!   to be assigned or mapped (there is no firmware under PVH boot to program
//!   the BARs). This is the exact `PciXport` pattern `virtio_blk` uses.
//!
//! Two virtqueues: RX (queue 0, receiveq) and TX (queue 1, transmitq). Each
//! buffer is prefixed by a 12-byte `virtio_net_hdr` (the v1 header - `num_buffers`
//! is always present once `VIRTIO_F_VERSION_1` is negotiated, virtio spec 5.1.6).
//! We negotiate a **minimal** feature set (`VIRTIO_F_VERSION_1` +
//! `VIRTIO_NET_F_MAC`) - no mergeable-rx-buffers, no checksum/GSO offload - so a
//! received packet fits in one pre-posted RX buffer and a sent frame is one TX
//! descriptor. Single-vcore, **polled** (no interrupt path yet - a device IRQ is
//! a later refinement, like `virtio_blk`).
//!
//! The rings and buffers are **allocated from the frame pool** at probe time
//! (not held as huge kernel statics, which would bloat every kernel's `.bss`):
//! the CPU reaches them through the kernel's high-half linear map
//! (`phys_to_virt`) and the device DMAs to their **physical** address
//! (`virt_to_phys`), since after the higher-half move PA no longer equals VA
//! (docs/MEMORY.md).
//!
//! The device is discovered + installed by a test kernel (`probe` then
//! [`install`]); the queue opcodes `OP_NET_TX`/`OP_NET_RX`/`OP_NET_MAC`
//! (kernel/src/queue) bridge a librheo cell's async submissions to [`tx`]/[`rx`]/
//! [`mac`] during the cell's `SYS_DOORBELL` trap (docs/NETWORKING.md, LIBRHEO.md
//! Phase G).

use crate::arch;
use core::ptr::addr_of;
use core::sync::atomic::{Ordering, fence};

// virtio-mmio register offsets (modern / version 2). Identical to virtio_blk.
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
const CONFIG: usize = 0x100;
// Interrupt status / acknowledge (virtio-mmio 4.2.2). Bit 0 = "used buffer
// notification" - the device raised its line because a queue completed. The
// handler must write the bits back to ACK, or the (level-triggered) line stays
// asserted (docs/NETSTACK.md, rheo-net N2d).
const INTERRUPT_STATUS: usize = 0x060;
const INTERRUPT_ACK: usize = 0x064;

const MAGIC_VALUE: u32 = 0x7472_6976; // "virt"
const DEV_NET: u32 = 1; // virtio device type 1 = network

// Status bits.
const S_ACK: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;

// Descriptor flags. Both RX and TX use single (unchained) descriptors, so only
// the device-writable flag is needed (no VRING_DESC_F_NEXT).
const VRING_DESC_F_WRITE: u16 = 2;

/// `avail.flags`: "do not interrupt me when you consume these" (virtio 2.7.7).
/// Set on the **TX** ring - transmit completions are polled in `send_frame`, so
/// their interrupts would only be spurious wakeups. The **RX** ring leaves it
/// clear, which is what asks the device to raise its line on a received frame
/// (docs/NETSTACK.md, rheo-net N2d).
const VRING_AVAIL_F_NO_INTERRUPT: u16 = 1;

// Feature bits we drive. VIRTIO_NET_F_MAC = bit 5 (feature select 0);
// VIRTIO_F_VERSION_1 = bit 32 (feature select 1, bit 0).
const NET_F_MAC_SEL0: u32 = 1 << 5;
const VERSION_1_SEL1: u32 = 1 << 0;

/// Virtqueue depth (entries) for both RX and TX. A power of two, <= what fits
/// (with all rings) in a single 4 KiB frame.
const QSIZE: usize = 16;
/// Per-buffer bytes: the 12-byte header + a full Ethernet frame. One buffer per
/// frame (the frame pool is 4 KiB-granular).
const BUF_SIZE: usize = 2048;
/// The v1 `virtio_net_hdr` length (num_buffers included once VERSION_1 is on).
const NET_HDR_LEN: usize = 12;

// -------------------------------------------------- virtio-pci constants
const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
// Modern virtio-net (device type 1): 0x1040 + 1. QEMU presents this when the
// device is created with `disable-legacy=on`.
const PCI_DEVICE_VIRTIO_NET: u16 = 0x1041;

const PCI_COMMAND: u16 = 0x04;
const PCI_CMD_MEMORY: u32 = 1 << 1;
const PCI_CMD_MASTER: u32 = 1 << 2;
const PCI_CAP_PTR: u16 = 0x34;
const PCI_STATUS_CAP_LIST: u32 = 1 << 4;

const CAP_ID_VENDOR: u32 = 0x09;
const VIRTIO_CAP_COMMON: u32 = 1;
const VIRTIO_CAP_NOTIFY: u32 = 2;
const VIRTIO_CAP_DEVICE: u32 = 4;
const VIRTIO_CAP_PCI: u32 = 5;

// virtio_pci_common_cfg field offsets (bytes).
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

/// A split virtqueue, laid over one frame-pool 4 KiB frame (desc + avail + used
/// fit well within a page for `QSIZE = 16`). The device DMAs to its physical
/// address; the CPU reaches it through the high-half linear map.
#[repr(C)]
struct VirtQueue {
    desc: [Desc; QSIZE],
    avail: Avail,
    used: Used,
}

const _: () = assert!(core::mem::size_of::<VirtQueue>() <= 4096);

/// Allocate one zeroed frame-pool frame and return its kernel VA (high-half
/// linear map). `frames::alloc` zeroes the frame, so an overlaid `VirtQueue`
/// starts all-zero (empty rings).
fn alloc_frame_va() -> usize {
    arch::phys_to_virt(crate::mm::frames::alloc().expect("virtio-net ring (boot, reserve held)"))
}

unsafe fn r32(base: usize, off: usize) -> u32 {
    unsafe { ((base + off) as *const u32).read_volatile() }
}
unsafe fn w32(base: usize, off: usize, v: u32) {
    unsafe { ((base + off) as *mut u32).write_volatile(v) }
}

/// The virtio-pci transport (the `VIRTIO_PCI_CAP_PCI_CFG` config-space window).
/// Like `virtio_blk::PciXport`, but records a notify offset per virtqueue (RX/TX).
struct PciXport {
    bus: u8,
    dev: u8,
    func: u8,
    pci_cfg: u16,
    common_bar: u8,
    common_off: u32,
    notify_bar: u8,
    /// Absolute offset in `notify_bar` to poke, per queue index (0=RX, 1=TX).
    notify_off: [u32; 2],
}

impl PciXport {
    fn cfg_w(&self, off: u16, v: u32) {
        arch::pci_cfg_write32(ecam(), self.bus, self.dev, self.func, off, v);
    }
    fn cfg_r(&self, off: u16) -> u32 {
        arch::pci_cfg_read32(ecam(), self.bus, self.dev, self.func, off)
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
}

/// How a discovered device is reached (transport-specific register access +
/// per-queue notify).
enum Transport {
    /// virtio-mmio, with the transport-bus **slot** the device was found in.
    /// The slot is a portable fact; turning it into an interrupt id is per-ISA
    /// (`arch::enable_virtio_net_irq`), which is why it is carried here.
    Mmio {
        base: usize,
        slot: usize,
    },
    Pci(PciXport),
}

impl Transport {
    /// Notify the device that queue `qidx` has new available entries.
    fn notify(&self, qidx: u16) {
        match self {
            // SAFETY: `base` matched the virtio-mmio magic during probe.
            Transport::Mmio { base, .. } => unsafe { w32(*base, QUEUE_NOTIFY, qidx as u32) },
            // Modern notify: write the virtqueue index at the queue's notify off.
            Transport::Pci(p) => p.write(p.notify_bar, p.notify_off[qidx as usize], 2, qidx as u32),
        }
    }

    /// Acknowledge a device interrupt: read the pending status and write it back
    /// (virtio-mmio 4.2.2), which drops the device's interrupt line. virtio-pci
    /// here is driven through the config tunnel with no interrupt wired
    /// (docs/NETSTACK.md per-ISA table), so there is nothing to acknowledge.
    fn ack_irq(&self) {
        match self {
            // SAFETY: `base` matched the virtio-mmio magic during probe.
            Transport::Mmio { base, .. } => unsafe {
                let status = r32(*base, INTERRUPT_STATUS);
                if status != 0 {
                    w32(*base, INTERRUPT_ACK, status);
                }
            },
            Transport::Pci(_) => {}
        }
    }
}

/// A discovered, initialised virtio-net device. The rings and buffers live in
/// frame-pool memory (kernel VAs held here); only this small struct is a static.
pub struct VirtioNet {
    transport: Transport,
    mac: [u8; 6],
    /// Kernel VA of the RX / TX `VirtQueue` (one frame each).
    rx_vq: usize,
    tx_vq: usize,
    /// Kernel VA of each RX buffer (one frame each) and the single TX buffer.
    rx_buf: [usize; QSIZE],
    tx_buf: usize,
    /// Used-ring watermarks (how many entries we have consumed).
    rx_last_used: u16,
    tx_last_used: u16,
}

/// Physical address of a `VirtQueue`'s three rings (desc, avail, used), given
/// its kernel VA.
fn vq_phys(vq_va: usize) -> (u64, u64, u64) {
    let vq = vq_va as *const VirtQueue;
    // SAFETY: `vq_va` is a live frame-pool VA holding a VirtQueue.
    unsafe {
        (
            arch::virt_to_phys(addr_of!((*vq).desc) as usize) as u64,
            arch::virt_to_phys(addr_of!((*vq).avail) as usize) as u64,
            arch::virt_to_phys(addr_of!((*vq).used) as usize) as u64,
        )
    }
}

/// Find and initialise the first virtio-net device, trying virtio-mmio
/// (arm/riscv `virt`) first, then virtio-pci (x86 q35). Allocates its rings and
/// buffers from the frame pool, so the frame allocator must be initialised.
pub fn probe() -> Option<VirtioNet> {
    probe_mmio().or_else(probe_pci)
}

/// Allocate the RX/TX rings + buffers from the frame pool. Returns
/// `(rx_vq, tx_vq, rx_buf[QSIZE], tx_buf)` kernel VAs.
fn alloc_rings() -> (usize, usize, [usize; QSIZE], usize) {
    let rx_vq = alloc_frame_va();
    let tx_vq = alloc_frame_va();
    let mut rx_buf = [0usize; QSIZE];
    for b in rx_buf.iter_mut() {
        *b = alloc_frame_va();
    }
    let tx_buf = alloc_frame_va();
    (rx_vq, tx_vq, rx_buf, tx_buf)
}

fn probe_mmio() -> Option<VirtioNet> {
    // Bound in a variable, not a literal range, so the const-0 x86-64 count does
    // not make clippy flag an "empty range" (mirrors `virtio_blk::probe_mmio`).
    let count = arch::VIRTIO_MMIO_COUNT;
    for slot in 0..count {
        let base = arch::VIRTIO_MMIO_BASE + slot * arch::VIRTIO_MMIO_STRIDE;
        // SAFETY: `base` is a fixed MMIO address the kernel maps.
        unsafe {
            if r32(base, MAGIC) != MAGIC_VALUE || r32(base, VERSION) != 2 {
                continue;
            }
            if r32(base, DEVICE_ID) != DEV_NET {
                continue;
            }
            if let Some(dev) = init_mmio(base, slot) {
                return Some(dev);
            }
        }
    }
    None
}

/// # Safety
/// `base` must be a virtio-mmio net device that magic/version/id matched.
unsafe fn init_mmio(base: usize, slot: usize) -> Option<VirtioNet> {
    unsafe {
        w32(base, STATUS, 0); // reset
        let mut status = S_ACK;
        w32(base, STATUS, status);
        status |= S_DRIVER;
        w32(base, STATUS, status);

        // Negotiate VIRTIO_F_VERSION_1 (bit 32) + VIRTIO_NET_F_MAC (bit 5).
        w32(base, DRIVER_FEATURES_SEL, 1);
        w32(base, DRIVER_FEATURES, VERSION_1_SEL1);
        w32(base, DRIVER_FEATURES_SEL, 0);
        w32(base, DRIVER_FEATURES, NET_F_MAC_SEL0);

        status |= S_FEATURES_OK;
        w32(base, STATUS, status);
        if r32(base, STATUS) & S_FEATURES_OK == 0 {
            return None;
        }

        let (rx_vq, tx_vq, rx_buf, tx_buf) = alloc_rings();
        if !setup_queue_mmio(base, 0, rx_vq) || !setup_queue_mmio(base, 1, tx_vq) {
            return None;
        }

        // MAC: virtio-net config `mac[6]` at device-config offset 0.
        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            *b = ((base + CONFIG) as *const u8).add(i).read_volatile();
        }

        status |= S_DRIVER_OK;
        w32(base, STATUS, status);

        let mut dev = VirtioNet {
            transport: Transport::Mmio { base, slot },
            mac,
            rx_vq,
            tx_vq,
            rx_buf,
            tx_buf,
            rx_last_used: 0,
            tx_last_used: 0,
        };
        dev.post_all_rx();
        Some(dev)
    }
}

/// # Safety
/// The device must be past FEATURES_OK; `vq_va` is a live frame-pool VirtQueue.
unsafe fn setup_queue_mmio(base: usize, qidx: u32, vq_va: usize) -> bool {
    unsafe {
        w32(base, QUEUE_SEL, qidx);
        if r32(base, QUEUE_NUM_MAX) < QSIZE as u32 {
            return false;
        }
        w32(base, QUEUE_NUM, QSIZE as u32);
        let (desc_pa, avail_pa, used_pa) = vq_phys(vq_va);
        w32(base, QUEUE_DESC_LOW, desc_pa as u32);
        w32(base, QUEUE_DESC_HIGH, (desc_pa >> 32) as u32);
        w32(base, QUEUE_DRIVER_LOW, avail_pa as u32);
        w32(base, QUEUE_DRIVER_HIGH, (avail_pa >> 32) as u32);
        w32(base, QUEUE_DEVICE_LOW, used_pa as u32);
        w32(base, QUEUE_DEVICE_HIGH, (used_pa >> 32) as u32);
        w32(base, QUEUE_READY, 1);
        true
    }
}

/// The PCIe ECAM base. On x86 the CF8/CFC config accessor ignores it; on
/// ARM/RISC-V it is the MMIO base the accessor reads through, so it must be
/// the discovered value, not 0 (docs/GPU-HARDWARE.md 3).
fn ecam() -> u64 {
    crate::hw::inventory().ecam_base
}

fn probe_pci() -> Option<VirtioNet> {
    for dev in 0u8..32 {
        for func in 0u8..8 {
            let id = arch::pci_cfg_read32(ecam(), 0, dev, func, 0x00);
            if (id & 0xFFFF) as u16 != PCI_VENDOR_VIRTIO {
                continue;
            }
            if (id >> 16) as u16 != PCI_DEVICE_VIRTIO_NET {
                continue;
            }
            if let Some(d) = init_pci(0, dev, func) {
                return Some(d);
            }
        }
    }
    None
}

fn cfg_read8(bus: u8, dev: u8, func: u8, off: u16) -> u8 {
    let d = arch::pci_cfg_read32(ecam(), bus, dev, func, off & !3);
    ((d >> ((off & 3) * 8)) & 0xFF) as u8
}

fn init_pci(bus: u8, dev: u8, func: u8) -> Option<VirtioNet> {
    let status_cmd = arch::pci_cfg_read32(ecam(), bus, dev, func, PCI_COMMAND);
    if status_cmd & (PCI_STATUS_CAP_LIST << 16) == 0 {
        return None;
    }

    let mut common: Option<(u8, u32)> = None;
    let mut notify: Option<(u8, u32)> = None;
    let mut notify_mult: u32 = 0;
    let mut device: Option<(u8, u32)> = None;
    let mut pci_cfg: Option<u16> = None;

    let mut cap = cfg_read8(bus, dev, func, PCI_CAP_PTR) as u16;
    let mut guard = 0;
    while cap != 0 && cap != 0xFF && guard < 48 {
        guard += 1;
        let hdr = arch::pci_cfg_read32(ecam(), bus, dev, func, cap);
        let id = hdr & 0xFF;
        let next = (hdr >> 8) & 0xFF;
        let cfg_type = (hdr >> 24) & 0xFF;
        if id == CAP_ID_VENDOR {
            let bar = (arch::pci_cfg_read32(ecam(), bus, dev, func, cap + 4) & 0xFF) as u8;
            let offset = arch::pci_cfg_read32(ecam(), bus, dev, func, cap + 8);
            match cfg_type {
                VIRTIO_CAP_COMMON => common = Some((bar, offset)),
                VIRTIO_CAP_NOTIFY => {
                    notify = Some((bar, offset));
                    notify_mult = arch::pci_cfg_read32(ecam(), bus, dev, func, cap + 16);
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
        notify_off: [0; 2],
    };

    let cmd = arch::pci_cfg_read32(ecam(), bus, dev, func, PCI_COMMAND);
    arch::pci_cfg_write32(
        ecam(),
        bus,
        dev,
        func,
        PCI_COMMAND,
        cmd | PCI_CMD_MEMORY | PCI_CMD_MASTER,
    );

    x.cc_w8(CC_DEVICE_STATUS, 0);
    x.cc_w8(CC_DEVICE_STATUS, S_ACK as u8);
    x.cc_w8(CC_DEVICE_STATUS, (S_ACK | S_DRIVER) as u8);

    // Negotiate VIRTIO_F_VERSION_1 + VIRTIO_NET_F_MAC.
    x.cc_w32(CC_DRIVER_FEATURE_SELECT, 1);
    x.cc_w32(CC_DRIVER_FEATURE, VERSION_1_SEL1);
    x.cc_w32(CC_DRIVER_FEATURE_SELECT, 0);
    x.cc_w32(CC_DRIVER_FEATURE, NET_F_MAC_SEL0);

    x.cc_w8(CC_DEVICE_STATUS, (S_ACK | S_DRIVER | S_FEATURES_OK) as u8);
    if x.cc_r8(CC_DEVICE_STATUS) & S_FEATURES_OK as u8 == 0 {
        return None;
    }

    let (rx_vq, tx_vq, rx_buf, tx_buf) = alloc_rings();
    let rx_ok = setup_queue_pci(&mut x, 0, rx_vq, notify_base, notify_mult);
    let tx_ok = setup_queue_pci(&mut x, 1, tx_vq, notify_base, notify_mult);
    if !rx_ok || !tx_ok {
        return None;
    }

    // MAC: virtio-net device config `mac[6]` at offset 0.
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = x.read(device_bar, device_off + i as u32, 1) as u8;
    }

    x.cc_w8(
        CC_DEVICE_STATUS,
        (S_ACK | S_DRIVER | S_FEATURES_OK | S_DRIVER_OK) as u8,
    );

    let mut dev = VirtioNet {
        transport: Transport::Pci(x),
        mac,
        rx_vq,
        tx_vq,
        rx_buf,
        tx_buf,
        rx_last_used: 0,
        tx_last_used: 0,
    };
    dev.post_all_rx();
    Some(dev)
}

/// Configure one PCI virtqueue and record its notify offset. Returns false if
/// the device's max queue size is below [`QSIZE`].
fn setup_queue_pci(
    x: &mut PciXport,
    qidx: u16,
    vq_va: usize,
    notify_base: u32,
    notify_mult: u32,
) -> bool {
    x.cc_w16(CC_QUEUE_SELECT, qidx);
    if (x.cc_r16(CC_QUEUE_SIZE) as usize) < QSIZE {
        return false;
    }
    x.cc_w16(CC_QUEUE_SIZE, QSIZE as u16);
    let (desc_pa, avail_pa, used_pa) = vq_phys(vq_va);
    x.cc_w32(CC_QUEUE_DESC, desc_pa as u32);
    x.cc_w32(CC_QUEUE_DESC + 4, (desc_pa >> 32) as u32);
    x.cc_w32(CC_QUEUE_DRIVER, avail_pa as u32);
    x.cc_w32(CC_QUEUE_DRIVER + 4, (avail_pa >> 32) as u32);
    x.cc_w32(CC_QUEUE_DEVICE, used_pa as u32);
    x.cc_w32(CC_QUEUE_DEVICE + 4, (used_pa >> 32) as u32);
    let qnotify = x.cc_r16(CC_QUEUE_NOTIFY_OFF) as u32;
    x.notify_off[qidx as usize] = notify_base + qnotify * notify_mult;
    x.cc_w16(CC_QUEUE_ENABLE, 1);
    true
}

impl VirtioNet {
    /// The device MAC address.
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// The virtio-mmio transport-bus slot this device sits in, or `None` for a
    /// virtio-pci device. `net_rx::enable_irq` turns it into a per-ISA interrupt
    /// id (docs/NETSTACK.md, rheo-net N2d).
    pub fn mmio_slot(&self) -> Option<usize> {
        match self.transport {
            Transport::Mmio { slot, .. } => Some(slot),
            Transport::Pci(_) => None,
        }
    }

    #[inline]
    fn rx(&self) -> *mut VirtQueue {
        self.rx_vq as *mut VirtQueue
    }
    #[inline]
    fn tx(&self) -> *mut VirtQueue {
        self.tx_vq as *mut VirtQueue
    }

    /// Post all [`QSIZE`] RX buffers device-writable, then notify the RX queue.
    /// Each buffer is one descriptor covering `[12-byte hdr][frame]`; the device
    /// writes the header then the packet and reports the total in the used ring.
    fn post_all_rx(&mut self) {
        // SAFETY: the rings live in frame-pool memory reached through the linear
        // map; set up once here before any completion.
        unsafe {
            let vq = self.rx();
            for i in 0..QSIZE {
                let pa = arch::virt_to_phys(self.rx_buf[i]) as u64;
                (*vq).desc[i] = Desc {
                    addr: pa,
                    len: BUF_SIZE as u32,
                    flags: VRING_DESC_F_WRITE,
                    next: 0,
                };
                (*vq).avail.ring[i] = i as u16;
            }
            // RX leaves avail.flags clear: that is what asks the device to raise
            // its interrupt line when a frame lands (rheo-net N2d). TX sets
            // NO_INTERRUPT - `send_frame` polls its completion, so a transmit
            // interrupt would only be a spurious wakeup for the RX wait.
            (*vq).avail.flags = 0;
            (*self.tx()).avail.flags = VRING_AVAIL_F_NO_INTERRUPT;
            fence(Ordering::SeqCst);
            (*vq).avail.idx = QSIZE as u16;
            fence(Ordering::SeqCst);
            self.rx_last_used = (*vq).used.idx;
            self.tx_last_used = (*self.tx()).used.idx;
        }
        self.transport.notify(0);
    }

    /// Send one Ethernet `frame`. Prepends a zeroed 12-byte `virtio_net_hdr` (no
    /// offload), posts it on the TX queue, notifies, and polls the used ring.
    /// Returns `false` if the device never completed the descriptor (timeout).
    pub fn send_frame(&mut self, frame: &[u8]) -> bool {
        let n = frame.len().min(BUF_SIZE - NET_HDR_LEN);
        // SAFETY: the TX buffer/ring live in frame-pool memory reached through
        // the linear map; single-vcore, only touched here.
        unsafe {
            let buf = self.tx_buf as *mut u8;
            for i in 0..NET_HDR_LEN {
                buf.add(i).write(0);
            }
            for (i, &b) in frame[..n].iter().enumerate() {
                buf.add(NET_HDR_LEN + i).write(b);
            }
            let pa = arch::virt_to_phys(self.tx_buf) as u64;

            let vq = self.tx();
            (*vq).desc[0] = Desc {
                addr: pa,
                len: (NET_HDR_LEN + n) as u32,
                flags: 0, // device-readable
                next: 0,
            };
            let idx = (*vq).avail.idx;
            (*vq).avail.ring[(idx as usize) % QSIZE] = 0;
            fence(Ordering::SeqCst);
            (*vq).avail.idx = idx.wrapping_add(1);
            fence(Ordering::SeqCst);

            self.transport.notify(1);

            // Poll the used ring. The device advances `used.idx` by DMA, so read
            // it volatile (also tells clippy the condition is externally mutated).
            let used_idx = core::ptr::addr_of!((*vq).used.idx);
            let mut spins = 0u64;
            while used_idx.read_volatile() == self.tx_last_used {
                fence(Ordering::SeqCst);
                spins += 1;
                if spins > 100_000_000 {
                    return false;
                }
                core::hint::spin_loop();
            }
            self.tx_last_used = (*vq).used.idx;
        }
        // A transmit usually means a reply is imminent (an ARP request, a DNS
        // query, a TCP segment), so it counts as link activity for the receive
        // path's hot tier (docs/NETSTACK.md 16, the adaptive poll policy).
        crate::net_rx::note_activity();
        true
    }

    /// Poll for one received frame. Copies the frame (past the 12-byte header)
    /// into `out`, re-posts the buffer, and returns the frame length. `None` if
    /// no packet is available (non-blocking, like `virtio_blk`'s polled path).
    pub fn recv_frame(&mut self, out: &mut [u8]) -> Option<usize> {
        // SAFETY: the rings/buffers live in frame-pool memory reached through the
        // linear map; single-vcore, only touched here.
        unsafe {
            let vq = self.rx();
            fence(Ordering::SeqCst);
            let last = self.rx_last_used;
            if (*vq).used.idx == last {
                return None;
            }
            let slot = (last as usize) % QSIZE;
            let elem = (*vq).used.ring[slot];
            let id = elem.id as usize % QSIZE;
            let total = elem.len as usize;
            let frame_len = total.saturating_sub(NET_HDR_LEN);
            let n = frame_len.min(out.len());
            let src = self.rx_buf[id] as *const u8;
            for (i, o) in out.iter_mut().enumerate().take(n) {
                *o = src.add(NET_HDR_LEN + i).read();
            }

            // Re-post this buffer (its descriptor still points at rx_buf[id]).
            let aidx = (*vq).avail.idx;
            (*vq).avail.ring[(aidx as usize) % QSIZE] = id as u16;
            fence(Ordering::SeqCst);
            (*vq).avail.idx = aidx.wrapping_add(1);
            fence(Ordering::SeqCst);
            self.rx_last_used = last.wrapping_add(1);

            self.transport.notify(0);
            Some(frame_len)
        }
    }
}

// -------------------------------------------------- kernel queue-opcode bridge
//
// The device is a single-instance kernel resource (like the shell PTY): a test
// kernel installs it once, and the OP_NET_* queue opcodes reach it during a
// cell's `SYS_DOORBELL` trap. `buf_va` is the calling cell's own mapped memory
// (its address space is active during the drain), so TX reads and RX writes land
// there directly.

static mut NET: Option<VirtioNet> = None;

/// Install the discovered device as the kernel's NIC (called once at boot), and
/// register it as the `svc::NicOps` bridge the `OP_NET_*` opcodes reach.
///
/// The registration lives here rather than in a boot sequencer because *this* is
/// where the device is known to exist: a kernel binary that never discovers a NIC
/// never installs one, and its `OP_NET_*` opcodes then complete `STATUS_IO`
/// instead of reaching a driver that is not there. That is what lets the queue's
/// dispatch stop naming this module (docs/ARCHITECTURE-DEBT.md 3.2) without any
/// caller changing: a driver **cell** installs into the same slot later.
pub fn install(dev: VirtioNet) {
    // SAFETY: single-threaded boot; set once before any cell runs.
    unsafe {
        *core::ptr::addr_of_mut!(NET) = Some(dev);
    }
    crate::svc::set_nic_ops(crate::svc::NicOps { tx, rx, mac });
}

fn net_mut() -> Option<&'static mut VirtioNet> {
    // SAFETY: single-CPU; the NIC is a single-instance resource touched only
    // during a cell's (serialised) doorbell trap.
    unsafe { (*core::ptr::addr_of_mut!(NET)).as_mut() }
}

/// `OP_NET_TX`: send the `len` bytes at the cell VA `buf_va` as one frame.
/// Returns `(status, bytes_sent)`.
///
/// # Safety
/// `buf_va` is the calling cell's mapped buffer; called only during its trap.
pub fn tx(buf_va: u64, len: u64) -> (u32, u32) {
    use crate::queue::{STATUS_IO, STATUS_OK};
    let Some(dev) = net_mut() else {
        return (STATUS_IO, 0);
    };
    let n = (len as usize).min(BUF_SIZE);
    // SAFETY: the cell passes a VA of `len` readable bytes in its own memory.
    let frame = unsafe { core::slice::from_raw_parts(buf_va as *const u8, n) };
    if dev.send_frame(frame) {
        (STATUS_OK, n as u32)
    } else {
        (STATUS_IO, 0)
    }
}

/// `OP_NET_RX`: try to receive one frame into the cell buffer at `buf_va` (up to
/// `len`). Returns `(STATUS_OK, frame_len)`; `frame_len == 0` means no packet is
/// available (the caller re-submits to poll).
///
/// # Safety
/// `buf_va` is the calling cell's mapped buffer; called only during its trap.
pub fn rx(buf_va: u64, len: u64) -> (u32, u32) {
    use crate::queue::{STATUS_IO, STATUS_OK};
    let Some(dev) = net_mut() else {
        return (STATUS_IO, 0);
    };
    let n = len as usize;
    // SAFETY: the cell passes a VA of `len` writable bytes in its own memory.
    let out = unsafe { core::slice::from_raw_parts_mut(buf_va as *mut u8, n) };
    match dev.recv_frame(out) {
        Some(frame_len) => (STATUS_OK, frame_len as u32),
        None => (STATUS_OK, 0),
    }
}

/// The installed NIC's virtio-mmio transport slot, or `None` if there is no NIC
/// or it is a virtio-pci device. `net_rx::enable_irq` uses it to wire the per-ISA
/// RX interrupt (docs/NETSTACK.md, rheo-net N2d).
pub fn mmio_slot() -> Option<usize> {
    net_mut().and_then(|d| d.mmio_slot())
}

/// Acknowledge a pending device interrupt (drops its line). Called from
/// `net_rx::on_irq`, i.e. from the per-ISA interrupt vector - which the kernel
/// takes only inside its idle path, so this never re-enters a driver operation.
pub fn ack_irq() {
    if let Some(dev) = net_mut() {
        dev.transport.ack_irq();
    }
}

/// Try to receive one frame into the cell buffer at `buf_va` (up to `len`) for
/// the `SYS_WAIT_NET` wait loop: `Some(frame_len)` (0 = queue empty), or `None`
/// if no NIC is installed. Same one-copy path as [`rx`], without the queue
/// completion wrapper.
///
/// # Safety
/// `buf_va` is the calling cell's mapped buffer; called only during its trap.
/// Whether the receive virtqueue already holds a completed frame - a
/// **non-destructive** peek (it only compares the used-ring index against the
/// driver's cursor, exactly what [`VirtioNet::recv_frame`] tests before copying).
/// The scheduler needs this to decide that a cell parked on `SYS_WAIT_NET` is now
/// satisfiable without consuming the frame in the wrong address space
/// (docs/ARCHITECTURE-DEBT.md 2.4). False when no NIC is installed.
pub fn rx_pending() -> bool {
    let Some(dev) = net_mut() else {
        return false;
    };
    // SAFETY: the rings live in frame-pool memory reached through the linear map;
    // single-vcore, and this only reads two indices.
    unsafe {
        let vq = dev.rx();
        fence(Ordering::SeqCst);
        (*vq).used.idx != dev.rx_last_used
    }
}

pub fn drain_frame(buf_va: u64, len: usize) -> Option<usize> {
    let dev = net_mut()?;
    // SAFETY: the cell passes a VA of `len` writable bytes in its own memory.
    let out = unsafe { core::slice::from_raw_parts_mut(buf_va as *mut u8, len) };
    Some(dev.recv_frame(out).unwrap_or(0))
}

// ------------------------------------------- kernel-side (SocketOps) accessors
//
// The rheo-net **N4b** remote-INET bridge (docs/NETSTACK.md N4b,
// docs/LINUX-COMPAT.md L8-INET remote) runs its datapath in *kernel context* -
// it is a registered `svc::SocketOps` table, the `svc::FileOps` precedent - so it
// needs the driver over plain slices rather than cell VAs. These three are the
// same one-copy paths as [`tx`]/[`rx`]/[`mac`] with the queue-completion wrapper
// removed: **mechanism only, no new kernel object**, and the kernel still holds
// no network stack (that lives in the registered bridge over the `rheo-net`
// codec).

/// Send one Ethernet frame from a kernel-owned slice. `false` if no NIC is
/// installed or the device never completed the descriptor.
pub fn send_frame_slice(frame: &[u8]) -> bool {
    match net_mut() {
        Some(dev) => dev.send_frame(frame),
        None => false,
    }
}

/// Poll for one received frame into a kernel-owned slice. `Some(len)` (0 = the
/// receive queue is empty) or `None` if no NIC is installed.
pub fn recv_frame_slice(out: &mut [u8]) -> Option<usize> {
    let dev = net_mut()?;
    Some(dev.recv_frame(out).unwrap_or(0))
}

/// The installed NIC's MAC address, or `None` if there is no NIC.
pub fn mac_addr() -> Option<[u8; 6]> {
    net_mut().map(|d| d.mac())
}

/// `OP_NET_MAC`: write the 6-byte MAC to the cell buffer at `buf_va`. Returns
/// `(STATUS_OK, 6)`.
///
/// # Safety
/// `buf_va` must have been validated as 6 writable bytes in the calling cell
/// (`queue::run_opcode` does this) and this must run during that cell's trap.
pub fn mac(buf_va: u64) -> (u32, u32) {
    use crate::queue::{STATUS_IO, STATUS_OK};
    let Some(dev) = net_mut() else {
        return (STATUS_IO, 0);
    };
    let m = dev.mac();
    // SAFETY: `buf_va` was range-checked for 6 writable bytes in this cell by
    // the caller (`queue::run_opcode`), whose address space is active.
    unsafe {
        let dst = buf_va as *mut u8;
        for (i, &b) in m.iter().enumerate() {
            dst.add(i).write(b);
        }
    }
    (STATUS_OK, 6)
}
