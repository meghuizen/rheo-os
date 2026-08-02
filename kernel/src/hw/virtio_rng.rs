//! A **virtio-rng driver** - a hardware randomness *device*
//! (docs/TIME-IDENTITY.md 4a).
//!
//! # Why this exists
//!
//! Two of the three ISAs can ask the CPU for random bits: x86-64 has RDSEED,
//! ARM64 has RNDR. RISC-V has the `seed` CSR from the Zkr extension, but
//! reading it from S-mode needs an M-mode `mseccfg` grant that the firmware
//! here does not give, so a RISC-V boot had **no** real entropy source at all
//! and fell back to a cycle-counter loop that is deterministic under QEMU.
//!
//! A randomness device fixes that without inventing anything: virtio-rng is
//! the standard paravirtual one, QEMU models it on every machine we boot, and
//! real hardware presents the same shape (a TRNG chip, a TPM). So the fix is a
//! driver, not a per-ISA workaround - which is also what
//! docs/TARGET-ARCHITECTURES.md 4 requires, since a per-ISA hole outside
//! `arch/` is an architecture bug.
//!
//! # The device
//!
//! virtio-rng is the simplest virtio device there is (virtio spec 5.4). One
//! virtqueue. A request is a single **writable** descriptor: the driver offers
//! a buffer, the device fills it with random bytes and reports how many it
//! wrote. There is no device configuration space and no feature to negotiate
//! beyond `VIRTIO_F_VERSION_1`.
//!
//! Two transports, the same pair every other virtio driver here uses:
//! virtio-mmio on the arm/riscv `virt` machines, and virtio-pci on x86-64 q35
//! through the `VIRTIO_PCI_CAP_PCI_CFG` config tunnel - which is what lets it
//! work with no BAR assigned, since the PVH boot has no firmware to program
//! one.
//!
//! # What the driver does *not* decide
//!
//! It hands bytes to [`crate::rng::feed_device`] and nothing else. Whether
//! those bytes are credited as entropy, how they are mixed, and when a root
//! DRBG is re-keyed all belong to `rng::entropy` - one owner per decision.

use core::sync::atomic::{Ordering, fence};

use crate::arch;

// virtio-mmio register offsets (virtio spec 4.2.2).
const MAGIC: usize = 0x000;
const MAGIC_VALUE: u32 = 0x7472_6976; // "virt"
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

const DEV_ENTROPY: u32 = 4;

const S_ACK: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;

const VRING_DESC_F_WRITE: u16 = 2;

/// Ring size. One outstanding request is all this driver ever makes, so the
/// smallest legal power of two is enough; the device may demand a larger ring,
/// which the probe checks for.
const QSIZE: usize = 4;

/// Bytes asked for per device request. 64 = two 256-bit seeds' worth, which is
/// one request per reseed with headroom, and small enough that the buffer is a
/// static rather than a frame allocation.
pub const CHUNK: usize = 64;

// PCI.
const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
/// Modern virtio PCI device id for an entropy source (0x1040 + device type 4).
const PCI_DEVICE_VIRTIO_RNG: u16 = 0x1044;
/// The transitional id QEMU also uses for virtio-rng-pci.
const PCI_DEVICE_VIRTIO_RNG_LEGACY: u16 = 0x1005;
const PCI_COMMAND: u16 = 0x04;
const PCI_STATUS_CAP_LIST: u32 = 0x10;
const PCI_CAP_PTR: u16 = 0x34;
const PCI_CMD_MEMORY: u32 = 0x02;
const PCI_CMD_MASTER: u32 = 0x04;
const CAP_ID_VENDOR: u32 = 0x09;
const VIRTIO_CAP_COMMON: u32 = 1;
const VIRTIO_CAP_NOTIFY: u32 = 2;
const VIRTIO_CAP_PCI: u32 = 5;

// Fields of the virtio-pci common configuration block.
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

/// The split virtqueue plus the buffer the device writes into. Page-aligned and
/// static, so its physical address is stable for the life of the boot.
#[repr(C, align(4096))]
struct Ring {
    desc: [Desc; QSIZE],
    avail: Avail,
    used: Used,
    buf: [u8; CHUNK],
}

static mut RING: Ring = Ring {
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
    buf: [0; CHUNK],
};
static mut LAST_USED: u16 = 0;

unsafe fn r32(base: usize, off: usize) -> u32 {
    unsafe { ((base + off) as *const u32).read_volatile() }
}
unsafe fn w32(base: usize, off: usize, v: u32) {
    unsafe { ((base + off) as *mut u32).write_volatile(v) }
}

/// The virtio-pci transport, driven entirely through the
/// `VIRTIO_PCI_CAP_PCI_CFG` window so no BAR has to be assigned or mapped.
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
    fn name(&self) -> &'static str {
        match self {
            Transport::Mmio { .. } => "virtio-mmio",
            Transport::Pci(_) => "virtio-pci",
        }
    }
}

/// A discovered virtio-rng device.
pub struct VirtioRng {
    transport: Transport,
}

/// The PCIe ECAM base (see `virtio_blk::ecam` for why it must be the
/// discovered value rather than 0).
fn ecam() -> u64 {
    crate::hw::inventory().ecam_base
}

/// Find and initialise the first virtio-rng device, trying virtio-mmio
/// (arm/riscv `virt`) then virtio-pci (x86 q35).
pub fn probe() -> Option<VirtioRng> {
    probe_mmio().or_else(probe_pci)
}

fn probe_mmio() -> Option<VirtioRng> {
    let count = arch::VIRTIO_MMIO_COUNT;
    for slot in 0..count {
        let base = arch::VIRTIO_MMIO_BASE + slot * arch::VIRTIO_MMIO_STRIDE;
        // SAFETY: a fixed MMIO address the kernel identity-maps.
        unsafe {
            if r32(base, MAGIC) != MAGIC_VALUE || r32(base, VERSION) != 2 {
                continue;
            }
            if r32(base, DEVICE_ID) != DEV_ENTROPY {
                continue;
            }
            if let Some(d) = init_mmio(base) {
                return Some(d);
            }
        }
    }
    None
}

/// # Safety
/// `base` must be a virtio-mmio entropy device whose magic/version/id matched.
unsafe fn init_mmio(base: usize) -> Option<VirtioRng> {
    unsafe {
        w32(base, STATUS, 0); // reset
        w32(base, STATUS, S_ACK);
        w32(base, STATUS, S_ACK | S_DRIVER);

        // Only VIRTIO_F_VERSION_1 (feature bit 32 -> select 1, bit 0). The
        // device offers nothing else this driver wants.
        w32(base, DEVICE_FEATURES_SEL, 1);
        let _ = r32(base, DEVICE_FEATURES);
        w32(base, DRIVER_FEATURES_SEL, 1);
        w32(base, DRIVER_FEATURES, 1);
        w32(base, DRIVER_FEATURES_SEL, 0);
        w32(base, DRIVER_FEATURES, 0);

        w32(base, STATUS, S_ACK | S_DRIVER | S_FEATURES_OK);
        if r32(base, STATUS) & S_FEATURES_OK == 0 {
            return None;
        }

        w32(base, QUEUE_SEL, 0);
        if r32(base, QUEUE_NUM_MAX) < QSIZE as u32 {
            return None;
        }
        w32(base, QUEUE_NUM, QSIZE as u32);

        let (desc_pa, avail_pa, used_pa) = ring_addrs();
        w32(base, QUEUE_DESC_LOW, desc_pa as u32);
        w32(base, QUEUE_DESC_HIGH, (desc_pa >> 32) as u32);
        w32(base, QUEUE_DRIVER_LOW, avail_pa as u32);
        w32(base, QUEUE_DRIVER_HIGH, (avail_pa >> 32) as u32);
        w32(base, QUEUE_DEVICE_LOW, used_pa as u32);
        w32(base, QUEUE_DEVICE_HIGH, (used_pa >> 32) as u32);
        w32(base, QUEUE_READY, 1);

        w32(base, STATUS, S_ACK | S_DRIVER | S_FEATURES_OK | S_DRIVER_OK);
        LAST_USED = (*core::ptr::addr_of!(RING)).used.idx;
        Some(VirtioRng {
            transport: Transport::Mmio { base },
        })
    }
}

/// Physical addresses of the three virtqueue parts.
fn ring_addrs() -> (u64, u64, u64) {
    // SAFETY: taking addresses of a static; no reference to its contents.
    unsafe {
        let r = core::ptr::addr_of!(RING);
        (
            arch::virt_to_phys(core::ptr::addr_of!((*r).desc) as usize) as u64,
            arch::virt_to_phys(core::ptr::addr_of!((*r).avail) as usize) as u64,
            arch::virt_to_phys(core::ptr::addr_of!((*r).used) as usize) as u64,
        )
    }
}

fn cfg_read8(bus: u8, dev: u8, func: u8, off: u16) -> u8 {
    let d = arch::pci_cfg_read32(ecam(), bus, dev, func, off & !3);
    ((d >> ((off & 3) * 8)) & 0xFF) as u8
}

fn probe_pci() -> Option<VirtioRng> {
    for dev in 0u8..32 {
        for func in 0u8..8 {
            let id = arch::pci_cfg_read32(ecam(), 0, dev, func, 0x00);
            if (id & 0xFFFF) as u16 != PCI_VENDOR_VIRTIO {
                continue;
            }
            let did = (id >> 16) as u16;
            if did != PCI_DEVICE_VIRTIO_RNG && did != PCI_DEVICE_VIRTIO_RNG_LEGACY {
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
    let status_cmd = arch::pci_cfg_read32(ecam(), bus, dev, func, PCI_COMMAND);
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

    x.cc_w32(CC_DEVICE_FEATURE_SELECT, 1);
    let _ = x.cc_r32(CC_DEVICE_FEATURE);
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

    let (desc_pa, avail_pa, used_pa) = ring_addrs();
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

    // SAFETY: single-threaded init; set the used-ring watermark.
    unsafe {
        LAST_USED = (*core::ptr::addr_of!(RING)).used.idx;
    }
    Some(VirtioRng {
        transport: Transport::Pci(x),
    })
}

impl VirtioRng {
    /// Which transport this device was found on, for the boot report.
    pub fn transport_name(&self) -> &'static str {
        self.transport.name()
    }

    /// Ask the device for up to `dst.len()` random bytes. Returns how many it
    /// actually wrote, which the spec allows to be fewer than asked for.
    ///
    /// Polled: this runs at boot and on reseed, both of them rare, and adding
    /// an interrupt would buy nothing a spin of a few microseconds does not
    /// already give.
    pub fn read(&mut self, dst: &mut [u8]) -> usize {
        let want = core::cmp::min(dst.len(), CHUNK);
        if want == 0 {
            return 0;
        }
        // SAFETY: the ring and its buffer are a static this driver alone owns,
        // and only one request is ever outstanding.
        unsafe {
            let r = core::ptr::addr_of_mut!(RING);
            let buf_pa = arch::virt_to_phys(core::ptr::addr_of!((*r).buf) as usize) as u64;
            (*r).desc[0] = Desc {
                addr: buf_pa,
                len: want as u32,
                flags: VRING_DESC_F_WRITE, // the device writes, we read
                next: 0,
            };
            let idx = (*r).avail.idx;
            (*r).avail.ring[(idx as usize) % QSIZE] = 0;
            fence(Ordering::SeqCst);
            (*r).avail.idx = idx.wrapping_add(1);
            fence(Ordering::SeqCst);

            self.transport.notify_q0();

            // Bounded spin. A device that never answers must not wedge the
            // boot: the pool simply stays uncredited and the seed source
            // reports what actually happened.
            let mut spins = 0u64;
            while (*r).used.idx == *core::ptr::addr_of!(LAST_USED) {
                fence(Ordering::SeqCst);
                spins += 1;
                if spins > 10_000_000 {
                    return 0;
                }
                core::hint::spin_loop();
            }
            let elem = (*r).used.ring[(*core::ptr::addr_of!(LAST_USED) as usize) % QSIZE];
            *core::ptr::addr_of_mut!(LAST_USED) = (*r).used.idx;

            let got = core::cmp::min(elem.len as usize, want);
            let src = core::ptr::addr_of!((*r).buf) as *const u8;
            core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), got);
            // Do not leave the bytes lying in the DMA buffer.
            core::ptr::write_bytes(core::ptr::addr_of_mut!((*r).buf) as *mut u8, 0, CHUNK);
            got
        }
    }
}

// ---------------------------------------------------------- the boot wiring

/// The discovered device, if any. One per machine is all the entropy path
/// needs; a second would only be a second name for the same host source.
static mut DEVICE: Option<VirtioRng> = None;

/// Probe for a randomness device and, if one is there, feed the entropy pool
/// from it. Called from the boot sequencer after PCI enumeration.
///
/// Returns the transport it was found on, or `None`.
pub fn init() -> Option<&'static str> {
    let mut d = probe()?;
    let name = d.transport_name();
    let mut buf = [0u8; CHUNK];
    let got = d.read(&mut buf);
    // SAFETY: boot, single-threaded, before any cell runs.
    unsafe {
        DEVICE = Some(d);
    }
    if got == 0 {
        return Some(name);
    }
    crate::rng::feed_device(&buf[..got]);
    for b in buf.iter_mut() {
        // SAFETY: a plain write, volatile so it is not optimised away.
        unsafe { core::ptr::write_volatile(b, 0) };
    }
    Some(name)
}

/// Whether a randomness device was found at boot.
pub fn present() -> bool {
    // SAFETY: written once at boot, read after.
    unsafe { (*core::ptr::addr_of!(DEVICE)).is_some() }
}

/// Pull a fresh chunk from the device into the entropy pool. Returns how many
/// bytes were fed. Used by the periodic reseed path.
pub fn refill() -> usize {
    // SAFETY: the device is owned by this module and only reached from thread
    // context; the driver keeps one request outstanding at a time.
    let d = unsafe { (*core::ptr::addr_of_mut!(DEVICE)).as_mut() };
    let Some(d) = d else {
        return 0;
    };
    let mut buf = [0u8; CHUNK];
    let got = d.read(&mut buf);
    if got > 0 {
        crate::rng::feed_device(&buf[..got]);
    }
    for b in buf.iter_mut() {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile(b, 0) };
    }
    got
}
