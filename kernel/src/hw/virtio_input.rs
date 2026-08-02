//! A **virtio-input driver** - HID devices (keyboard, mouse, tablet) as an
//! entropy source (docs/TIME-IDENTITY.md 4a).
//!
//! # Why HID events are entropy
//!
//! When a human presses a key, and which key, are unpredictable in a way no
//! deterministic machine can reproduce. Linux has collected this since its first
//! `/dev/random` (`add_input_randomness`), and it is one of the few sources that
//! exists on a machine with no randomness hardware at all.
//!
//! It is **mixed and never credited**, like every other device-timing source
//! here: this kernel has no entropy estimator, and a keyboard sending an
//! auto-repeat is not the same as a person typing. The pool's absorb step cannot
//! lose entropy, so mixing an unmeasured source can only help.
//!
//! # This is not a keylogger
//!
//! Only the **arrival** of an event is mixed, never what the event said.
//! [`crate::rng::feed_hid`] takes a sequence number and reads the cycle counter
//! itself; its signature carries no key code, so no caller can pass one. The
//! event buffer is wiped as soon as it is drained, so a keystroke does not sit in
//! kernel memory waiting for the device to overwrite it.
//!
//! That is not a restriction, it is the better design: the unpredictability is in
//! *when* a key was pressed, not which one - a key code is a few bits of highly
//! skewed, guessable text. Mixing it would put what a person typed into kernel
//! state for a source credited **zero**. All cost, no benefit.
//!
//! # The device
//!
//! virtio-input (virtio spec 5.8) is deliberately small. Two virtqueues - an
//! **eventq** the device fills and a statusq this driver does not use - and a
//! config space that answers questions selected by writing a `select`/`subsel`
//! pair. An event is eight bytes: `type`, `code`, `value`.
//!
//! The driver posts writable buffers on the eventq and drains whatever the
//! device wrote. That is all: it does not interpret key codes, because it is not
//! a keyboard driver - it is an entropy tap on the arrival of HID events.
//!
//! Two transports, the same pair every other virtio driver here uses.
//!
//! # No interrupt, deliberately
//!
//! The eventq is drained by [`drain`], called from the same places the pool is
//! pumped. An interrupt would give a tighter timestamp, but it would also mean a
//! HID device could raise a line on every keystroke of a busy typist for a
//! source that is credited **zero**. The arrival ordering and the cycle counter
//! at drain time are what get mixed either way.

use core::sync::atomic::{Ordering, fence};

use crate::arch;

// virtio-mmio register offsets (virtio spec 4.2.2).
const MAGIC: usize = 0x000;
const MAGIC_VALUE: u32 = 0x7472_6976;
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

const DEV_INPUT: u32 = 18;

const S_ACK: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;

const VRING_DESC_F_WRITE: u16 = 2;

/// Ring size, and therefore how many events can be outstanding before the
/// device runs out of buffers. 16 is a typist's burst.
const QSIZE: usize = 16;

/// `virtio_input_config.select` value asking for the device's name.
const CFG_ID_NAME: u8 = 0x01;

// PCI.
const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
/// Modern virtio PCI device id for an input device (0x1040 + device type 18).
const PCI_DEVICE_VIRTIO_INPUT: u16 = 0x1052;
const PCI_COMMAND: u16 = 0x04;
const PCI_STATUS_CAP_LIST: u32 = 0x10;
const PCI_CAP_PTR: u16 = 0x34;
const PCI_CMD_MEMORY: u32 = 0x02;
const PCI_CMD_MASTER: u32 = 0x04;
const CAP_ID_VENDOR: u32 = 0x09;
const VIRTIO_CAP_COMMON: u32 = 1;
const VIRTIO_CAP_NOTIFY: u32 = 2;
const VIRTIO_CAP_DEVICE: u32 = 4;
const VIRTIO_CAP_PCI: u32 = 5;

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

/// One HID event, exactly as the device writes it (virtio spec 5.8.6).
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct Event {
    pub etype: u16,
    pub code: u16,
    pub value: u32,
}

/// The event virtqueue plus the buffers the device DMAs into. Page-aligned and
/// static, so the physical addresses are stable for the life of the boot.
#[repr(C, align(4096))]
struct Ring {
    desc: [Desc; QSIZE],
    avail: Avail,
    used: Used,
    events: [Event; QSIZE],
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
    events: [Event {
        etype: 0,
        code: 0,
        value: 0,
    }; QSIZE],
};
static mut LAST_USED: u16 = 0;
/// Events drained since boot, for the report and the proof.
static mut EVENTS: u64 = 0;

unsafe fn r32(base: usize, off: usize) -> u32 {
    unsafe { ((base + off) as *const u32).read_volatile() }
}
unsafe fn w32(base: usize, off: usize, v: u32) {
    unsafe { ((base + off) as *mut u32).write_volatile(v) }
}

/// The virtio-pci transport, driven through the `VIRTIO_PCI_CAP_PCI_CFG` window
/// so no BAR has to be assigned or mapped.
struct PciXport {
    bus: u8,
    dev: u8,
    func: u8,
    pci_cfg: u16,
    common_bar: u8,
    common_off: u32,
    notify_bar: u8,
    notify_off: u32,
    device_bar: u8,
    device_off: u32,
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
    /// Read one byte of the device configuration space.
    fn cfg_r8(&self, off: u32) -> u8 {
        match self {
            // SAFETY: a fixed MMIO address the kernel identity-maps.
            Transport::Mmio { base } => unsafe {
                ((*base + CONFIG + off as usize) as *const u8).read_volatile()
            },
            Transport::Pci(p) => p.read(p.device_bar, p.device_off + off, 1) as u8,
        }
    }
    /// Write one byte of the device configuration space.
    fn cfg_w8(&self, off: u32, v: u8) {
        match self {
            // SAFETY: as above.
            Transport::Mmio { base } => unsafe {
                ((*base + CONFIG + off as usize) as *mut u8).write_volatile(v)
            },
            Transport::Pci(p) => p.write(p.device_bar, p.device_off + off, 1, v as u32),
        }
    }
}

/// A discovered virtio-input device.
pub struct VirtioInput {
    transport: Transport,
}

fn ecam() -> u64 {
    crate::hw::inventory().ecam_base
}

/// Find and initialise the first virtio-input device, trying virtio-mmio
/// (arm/riscv `virt`) then virtio-pci (x86 q35).
pub fn probe() -> Option<VirtioInput> {
    probe_mmio().or_else(probe_pci)
}

fn probe_mmio() -> Option<VirtioInput> {
    let count = arch::VIRTIO_MMIO_COUNT;
    for slot in 0..count {
        let base = arch::VIRTIO_MMIO_BASE + slot * arch::VIRTIO_MMIO_STRIDE;
        // SAFETY: a fixed MMIO address the kernel identity-maps.
        unsafe {
            if r32(base, MAGIC) != MAGIC_VALUE || r32(base, VERSION) != 2 {
                continue;
            }
            if r32(base, DEVICE_ID) != DEV_INPUT {
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
/// `base` must be a virtio-mmio input device whose magic/version/id matched.
unsafe fn init_mmio(base: usize) -> Option<VirtioInput> {
    unsafe {
        w32(base, STATUS, 0);
        w32(base, STATUS, S_ACK);
        w32(base, STATUS, S_ACK | S_DRIVER);

        // Only VIRTIO_F_VERSION_1 (feature bit 32 -> select 1, bit 0).
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

        // Queue 0 is the eventq; the statusq is left unconfigured because this
        // driver never sends the device anything.
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
        let d = VirtioInput {
            transport: Transport::Mmio { base },
        };
        post_all(&d);
        Some(d)
    }
}

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

fn probe_pci() -> Option<VirtioInput> {
    for dev in 0u8..32 {
        for func in 0u8..8 {
            let id = arch::pci_cfg_read32(ecam(), 0, dev, func, 0x00);
            if (id & 0xFFFF) as u16 != PCI_VENDOR_VIRTIO {
                continue;
            }
            if (id >> 16) as u16 != PCI_DEVICE_VIRTIO_INPUT {
                continue;
            }
            if let Some(d) = init_pci(0, dev, func) {
                return Some(d);
            }
        }
    }
    None
}

fn init_pci(bus: u8, dev: u8, func: u8) -> Option<VirtioInput> {
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
        notify_off: 0,
        device_bar,
        device_off,
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

    // SAFETY: single-threaded init.
    unsafe {
        LAST_USED = (*core::ptr::addr_of!(RING)).used.idx;
    }
    let d = VirtioInput {
        transport: Transport::Pci(x),
    };
    post_all(&d);
    Some(d)
}

/// Offer every event buffer to the device. Called once at bring-up; each buffer
/// is re-offered as its event is drained.
fn post_all(d: &VirtioInput) {
    // SAFETY: the ring is this driver's own static, and no event can arrive
    // before the buffers are published.
    unsafe {
        let r = core::ptr::addr_of_mut!(RING);
        let evbase = arch::virt_to_phys(core::ptr::addr_of!((*r).events) as usize) as u64;
        for i in 0..QSIZE {
            (*r).desc[i] = Desc {
                addr: evbase + (i * core::mem::size_of::<Event>()) as u64,
                len: core::mem::size_of::<Event>() as u32,
                flags: VRING_DESC_F_WRITE,
                next: 0,
            };
            let idx = (*r).avail.idx;
            (*r).avail.ring[(idx as usize) % QSIZE] = i as u16;
            fence(Ordering::SeqCst);
            (*r).avail.idx = idx.wrapping_add(1);
        }
        fence(Ordering::SeqCst);
    }
    d.transport.notify_q0();
}

impl VirtioInput {
    /// The device's own name from its configuration space, into `out`. Returns
    /// how many bytes it reported. A real device round trip, and the cheapest
    /// one there is: it proves the config handshake works even on a machine
    /// where nobody is typing.
    pub fn name(&self, out: &mut [u8]) -> usize {
        self.transport.cfg_w8(0, CFG_ID_NAME); // select
        self.transport.cfg_w8(1, 0); // subsel
        let size = self.transport.cfg_r8(2) as usize;
        let n = size.min(out.len());
        for (i, o) in out.iter_mut().enumerate().take(n) {
            // The union payload starts at offset 8, after select/subsel/size and
            // five reserved bytes.
            *o = self.transport.cfg_r8(8 + i as u32);
        }
        n
    }

    /// Drain every event the device has written, mixing each into the entropy
    /// pool, and hand its buffer back. Returns how many were drained.
    pub fn drain(&self) -> usize {
        let mut n = 0;
        // SAFETY: the ring is this driver's own static, touched from thread
        // context only.
        unsafe {
            let r = core::ptr::addr_of_mut!(RING);
            loop {
                fence(Ordering::SeqCst);
                let last = *core::ptr::addr_of!(LAST_USED);
                if (*r).used.idx == last {
                    break;
                }
                let elem = (*r).used.ring[(last as usize) % QSIZE];
                let id = elem.id as usize % QSIZE;

                // **The event's arrival, never its content.** `feed_hid` takes a
                // sequence number and reads the cycle counter itself; the key
                // code is not passed, cannot be passed, and is never read out of
                // the buffer here. See the module docs.
                EVENTS = (*core::ptr::addr_of!(EVENTS)).wrapping_add(1);
                crate::rng::feed_hid(*core::ptr::addr_of!(EVENTS));

                // Wipe the buffer before handing it back, so a keystroke does not
                // sit in kernel memory until the device happens to overwrite it.
                core::ptr::write_volatile(
                    core::ptr::addr_of_mut!((*r).events[id]),
                    Event::default(),
                );
                n += 1;

                // Hand the buffer straight back, so a burst of typing does not
                // run the device out of them.
                let aidx = (*r).avail.idx;
                (*r).avail.ring[(aidx as usize) % QSIZE] = id as u16;
                fence(Ordering::SeqCst);
                (*r).avail.idx = aidx.wrapping_add(1);
                *core::ptr::addr_of_mut!(LAST_USED) = last.wrapping_add(1);
            }
        }
        if n > 0 {
            fence(Ordering::SeqCst);
            self.transport.notify_q0();
        }
        n
    }
}

// ---------------------------------------------------------- the boot wiring

static mut DEVICE: Option<VirtioInput> = None;

/// Probe for a HID device and keep it. Called from the boot sequencer.
/// Returns the device's own name length, or `None` when there is no device.
pub fn init() -> Option<usize> {
    let d = probe()?;
    let mut name = [0u8; 64];
    let n = d.name(&mut name);
    // SAFETY: boot, single-threaded, before any cell runs.
    unsafe {
        DEVICE = Some(d);
    }
    Some(n)
}

/// Whether a HID device was found at boot.
pub fn present() -> bool {
    // SAFETY: written once at boot, read after.
    unsafe { (*core::ptr::addr_of!(DEVICE)).is_some() }
}

/// The device's own name, into `out`; returns its length, 0 when absent.
pub fn device_name(out: &mut [u8]) -> usize {
    // SAFETY: written once at boot, read after.
    let d = unsafe { (*core::ptr::addr_of!(DEVICE)).as_ref() };
    match d {
        Some(d) => d.name(out),
        None => 0,
    }
}

/// Drain pending HID events into the entropy pool. Returns how many.
pub fn pump() -> usize {
    // SAFETY: the device is owned by this module and reached from thread
    // context only.
    let d = unsafe { (*core::ptr::addr_of!(DEVICE)).as_ref() };
    match d {
        Some(d) => d.drain(),
        None => 0,
    }
}

/// Whether every event buffer is zero - nothing a person typed is left in
/// kernel memory. A property a test can check, unlike "the code does not read
/// the key code", which is a property of the signature.
pub fn buffers_clear() -> bool {
    // SAFETY: reading this driver's own static from thread context.
    unsafe {
        let r = core::ptr::addr_of!(RING);
        (0..QSIZE).all(|i| {
            let e = (*r).events[i];
            e.etype == 0 && e.code == 0 && e.value == 0
        })
    }
}

/// HID events drained since boot.
pub fn events() -> u64 {
    // SAFETY: a counter written by this module from thread context.
    unsafe { *core::ptr::addr_of!(EVENTS) }
}
