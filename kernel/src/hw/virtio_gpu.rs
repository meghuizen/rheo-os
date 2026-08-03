//! A virtio-gpu 2D driver (virtio 1.0 "modern", the plain 2D / VIRGL-off
//! subset), mirroring `virtio_net`/`virtio_blk` over the same two transports:
//!
//! - **virtio-mmio** - QEMU's `virt` machines (arm/riscv) expose it at fixed
//!   addresses (per-ISA constants in `arch`). Register access is plain MMIO.
//! - **virtio-pci** - QEMU q35 (x86-64) has no virtio-mmio; virtio-gpu is a
//!   PCIe device driven *entirely through PCI configuration space* using the
//!   `VIRTIO_PCI_CAP_PCI_CFG` capability (virtio spec 4.1.4.8), so no BAR needs
//!   to be assigned or mapped (the same `PciXport` pattern the other drivers use).
//!
//! One virtqueue is used: **controlq (queue 0)** - every 2D command goes there.
//! The cursorq (queue 1) is left unconfigured. A **minimal** feature set is
//! negotiated (just `VIRTIO_F_VERSION_1`); VIRGL/3D and EDID are deliberately
//! not negotiated. Each command is a `virtio_gpu_ctrl_hdr` + a command-specific
//! body written into a frame-pool command buffer, submitted on the controlq as
//! a **2-descriptor chain** ([readable command][writable response], linked with
//! `VRING_DESC_F_NEXT` - like virtio_blk's request/status), then the used ring
//! is polled for the device's response code (spec 5.7).
//!
//! The 2D bring-up (spec 5.7.6.2): `GET_DISPLAY_INFO` -> `RESOURCE_CREATE_2D`
//! (resource 1, `B8G8R8A8_UNORM`, 128x128) -> `RESOURCE_ATTACH_BACKING` (a
//! kernel-side framebuffer of frame-pool frames, one `virtio_gpu_mem_entry` per
//! frame - so the backing need not be physically contiguous) -> `SET_SCANOUT`
//! (scanout 0). A present is then `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`.
//!
//! **Honesty (docs/DISPLAY.md).** The test runs QEMU headless (`-display none`),
//! so there is no visible monitor to assert. The proof is the **genuine driver
//! round-trip**: every command returns its expected `RESP_OK_*` code from the
//! real QEMU device model - exactly as virtio-net's proof is SLIRP's real ARP
//! reply, not a rendered packet. This driver does **not** claim visible output;
//! it claims the device accepted the full create-2d -> attach -> set-scanout ->
//! transfer -> flush sequence. Which commands return OK headless is reported by
//! [`VirtioGpu::report`] and asserted by the test to exactly what genuinely
//! succeeds (`SET_SCANOUT` may be a no-op with no display backend; see the doc).
//!
//! The rings, command/response buffers, and framebuffer are **allocated from the
//! frame pool** at probe time (never large kernel statics, which would bloat
//! every kernel's `.bss`): the CPU reaches them through the kernel's high-half
//! linear map (`phys_to_virt`), and the device DMAs to their **physical**
//! address (`virt_to_phys`), since after the higher-half move PA no longer
//! equals VA (docs/MEMORY.md).
//!
//! The device is discovered + installed by a test kernel ([`probe`] then
//! [`install`]); the queue opcode `OP_GPU_PRESENT` (kernel/src/queue) bridges a
//! librheo cell's async `display::Scanout::present` to [`present`] during the
//! cell's `SYS_DOORBELL` trap (docs/LIBRHEO.md Phase H).

use crate::arch;
use crate::mm::frames::FRAME_SIZE;
use core::ptr::addr_of;
use core::sync::atomic::{Ordering, fence};

// virtio-mmio register offsets (modern / version 2). Identical to virtio_net.
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
const DEV_GPU: u32 = 16; // virtio device type 16 = GPU

// Status bits.
const S_ACK: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;

// Descriptor flags. Commands use a 2-descriptor chain: [readable cmd] -> NEXT ->
// [writable response].
const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

// VIRTIO_F_VERSION_1 = bit 32 (feature select 1, bit 0). No GPU-specific
// features (no VIRGL, no EDID) - the plain 2D path.
const VERSION_1_SEL1: u32 = 1 << 0;

/// Controlq depth. A power of two; only the head 2 descriptors are ever used
/// (one command in flight at a time - synchronous, polled).
const QSIZE: usize = 16;

// -------------------------------------------------- virtio-pci constants
const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
// Modern virtio-gpu (device type 16): 0x1040 + 16. QEMU presents this when the
// device is created with `disable-legacy=on`.
const PCI_DEVICE_VIRTIO_GPU: u16 = 0x1050;

const PCI_COMMAND: u16 = 0x04;
const PCI_CMD_MEMORY: u32 = 1 << 1;
const PCI_CMD_MASTER: u32 = 1 << 2;
const PCI_CAP_PTR: u16 = 0x34;
const PCI_STATUS_CAP_LIST: u32 = 1 << 4;

const CAP_ID_VENDOR: u32 = 0x09;
const VIRTIO_CAP_COMMON: u32 = 1;
const VIRTIO_CAP_NOTIFY: u32 = 2;
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

// -------------------------------------------------- virtio-gpu 2D protocol
// (virtio spec 5.7.6). All structs little-endian; `virtio_gpu_ctrl_hdr` is 24
// bytes and prefixes every command and response.
const CTRL_GET_DISPLAY_INFO: u32 = 0x0100;
const CTRL_RESOURCE_CREATE_2D: u32 = 0x0101;
const CTRL_SET_SCANOUT: u32 = 0x0103;
const CTRL_RESOURCE_FLUSH: u32 = 0x0104;
const CTRL_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CTRL_RESOURCE_ATTACH_BACKING: u32 = 0x0106;

const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;

/// `virtio_gpu_ctrl_hdr` size (type u32, flags u32, fence_id u64, ctx_id u32,
/// padding u32).
const HDR_LEN: usize = 24;

/// `VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM` (=1). The compositor treats each pixel as
/// an opaque 32-bit word, so the exact channel order is not load-bearing here;
/// B8G8R8A8 is the format QEMU's default display expects.
const FORMAT_B8G8R8A8_UNORM: u32 = 1;

/// The single 2D resource id and scanout id we drive.
const RESOURCE_ID: u32 = 1;
const SCANOUT_ID: u32 = 0;

/// Framebuffer geometry. Fixed 128x128 RGBA = 65536 bytes = 16 frame-pool
/// frames. Fixed (not sized from `GET_DISPLAY_INFO`) so the backing frame count
/// is a constant; `SET_SCANOUT` with a 128x128 rect is a partial scanout on a
/// larger display, which is fine. 128x128 also matches the display-info
/// fallback, so a headless device reporting a 0-size mode is handled uniformly.
pub const FB_W: u32 = 128;
pub const FB_H: u32 = 128;
const FB_BYTES: usize = (FB_W * FB_H * 4) as usize;
const FB_FRAMES: usize = FB_BYTES / FRAME_SIZE;
const _: () = assert!(FB_BYTES.is_multiple_of(FRAME_SIZE));

/// Command buffer capacity used/zeroed per command. The largest command is
/// `RESOURCE_ATTACH_BACKING` = 32 + `FB_FRAMES`*16 bytes; a full frame holds it.
const CMD_CLEAR: usize = 512;
const _: () = assert!(32 + FB_FRAMES * 16 <= CMD_CLEAR);

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

/// A split virtqueue laid over one frame-pool 4 KiB frame (desc + avail + used
/// fit within a page for `QSIZE = 16`).
#[repr(C)]
struct VirtQueue {
    desc: [Desc; QSIZE],
    avail: Avail,
    used: Used,
}

const _: () = assert!(core::mem::size_of::<VirtQueue>() <= 4096);

/// Per-command round-trip results (the honest proof surface; docs/DISPLAY.md).
/// Each field records whether that command returned its expected `RESP_OK_*`.
#[derive(Copy, Clone)]
pub struct GpuReport {
    pub display_info_ok: bool,
    pub display_w: u32,
    pub display_h: u32,
    pub create_2d_ok: bool,
    pub attach_ok: bool,
    pub set_scanout_ok: bool,
    pub transfer_ok: bool,
    pub flush_ok: bool,
}

impl GpuReport {
    const EMPTY: GpuReport = GpuReport {
        display_info_ok: false,
        display_w: 0,
        display_h: 0,
        create_2d_ok: false,
        attach_ok: false,
        set_scanout_ok: false,
        transfer_ok: false,
        flush_ok: false,
    };
}

/// Allocate one zeroed frame-pool frame and return its kernel VA (high-half
/// linear map).
fn alloc_frame_va() -> usize {
    arch::phys_to_virt(crate::mm::frames::alloc().expect("virtio-gpu ring (boot, reserve held)"))
}

unsafe fn r32(base: usize, off: usize) -> u32 {
    unsafe { ((base + off) as *const u32).read_volatile() }
}
unsafe fn w32(base: usize, off: usize, v: u32) {
    unsafe { ((base + off) as *mut u32).write_volatile(v) }
}

// Little-endian field writers/readers over a frame-pool buffer (normal kernel
// RAM; a fence before the doorbell publishes the writes to the device DMA).
fn cmd_wr32(base: usize, off: usize, v: u32) {
    // SAFETY: `base+off` is within the command frame (offsets bounded by CMD_CLEAR).
    unsafe { ((base + off) as *mut u32).write_unaligned(v) }
}
fn cmd_wr64(base: usize, off: usize, v: u64) {
    // SAFETY: as above.
    unsafe { ((base + off) as *mut u64).write_unaligned(v) }
}
fn resp_rd32(base: usize, off: usize) -> u32 {
    // SAFETY: `base+off` is within the response frame.
    unsafe { ((base + off) as *const u32).read_unaligned() }
}

/// The virtio-pci transport (the `VIRTIO_PCI_CAP_PCI_CFG` config-space window).
/// One notify offset (only the controlq is driven).
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

/// How a discovered device is reached.
enum Transport {
    Mmio { base: usize },
    Pci(PciXport),
}

impl Transport {
    /// Notify the device that the controlq (queue 0) has a new available entry.
    fn notify(&self) {
        match self {
            // SAFETY: `base` matched the virtio-mmio magic during probe.
            Transport::Mmio { base } => unsafe { w32(*base, QUEUE_NOTIFY, 0) },
            Transport::Pci(p) => p.write(p.notify_bar, p.notify_off, 2, 0),
        }
    }
}

/// A discovered, initialised virtio-gpu device. The controlq, command/response
/// buffers, and framebuffer live in frame-pool memory (kernel VAs held here);
/// only this small struct is a static.
pub struct VirtioGpu {
    transport: Transport,
    /// Kernel VA of the controlq `VirtQueue` (one frame).
    controlq: usize,
    /// Kernel VA of the command buffer and the response buffer (one frame each).
    cmd_buf: usize,
    resp_buf: usize,
    /// Kernel VAs of the framebuffer frames (`FB_FRAMES` of them, non-contiguous;
    /// attached to the resource as one mem entry each).
    fb_frames: [usize; FB_FRAMES],
    /// Used-ring watermark.
    last_used: u16,
    report: GpuReport,
}

/// Physical address of a `VirtQueue`'s three rings, given its kernel VA.
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

/// Find and initialise the first virtio-gpu device, trying virtio-mmio
/// (arm/riscv `virt`) first, then virtio-pci (x86 q35). Allocates its ring and
/// buffers from the frame pool, so the frame allocator must be initialised.
/// Returns `None` if no device is present, or if the essential 2D bring-up
/// (create + attach) is refused (skip-with-reason, keeping the tree bootable).
pub fn probe() -> Option<VirtioGpu> {
    probe_mmio().or_else(probe_pci)
}

/// Allocate the controlq, command/response buffers, and framebuffer frames.
fn alloc_all() -> (usize, usize, usize, [usize; FB_FRAMES]) {
    let controlq = alloc_frame_va();
    let cmd_buf = alloc_frame_va();
    let resp_buf = alloc_frame_va();
    let mut fb = [0usize; FB_FRAMES];
    for f in fb.iter_mut() {
        *f = alloc_frame_va();
    }
    (controlq, cmd_buf, resp_buf, fb)
}

fn probe_mmio() -> Option<VirtioGpu> {
    let count = arch::VIRTIO_MMIO_COUNT;
    for slot in 0..count {
        let base = arch::VIRTIO_MMIO_BASE + slot * arch::VIRTIO_MMIO_STRIDE;
        // SAFETY: `base` is a fixed MMIO address the kernel maps.
        unsafe {
            if r32(base, MAGIC) != MAGIC_VALUE || r32(base, VERSION) != 2 {
                continue;
            }
            if r32(base, DEVICE_ID) != DEV_GPU {
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
/// `base` must be a virtio-mmio gpu device that magic/version/id matched.
unsafe fn init_mmio(base: usize) -> Option<VirtioGpu> {
    unsafe {
        w32(base, STATUS, 0); // reset
        let mut status = S_ACK;
        w32(base, STATUS, status);
        status |= S_DRIVER;
        w32(base, STATUS, status);

        // Negotiate VIRTIO_F_VERSION_1 (bit 32) only.
        w32(base, DRIVER_FEATURES_SEL, 1);
        w32(base, DRIVER_FEATURES, VERSION_1_SEL1);
        w32(base, DRIVER_FEATURES_SEL, 0);
        w32(base, DRIVER_FEATURES, 0);

        status |= S_FEATURES_OK;
        w32(base, STATUS, status);
        if r32(base, STATUS) & S_FEATURES_OK == 0 {
            return None;
        }

        let (controlq, cmd_buf, resp_buf, fb_frames) = alloc_all();
        if !setup_queue_mmio(base, 0, controlq) {
            return None;
        }

        status |= S_DRIVER_OK;
        w32(base, STATUS, status);

        let mut dev = VirtioGpu {
            transport: Transport::Mmio { base },
            controlq,
            cmd_buf,
            resp_buf,
            fb_frames,
            last_used: 0,
            report: GpuReport::EMPTY,
        };
        dev.last_used = (*(controlq as *const VirtQueue)).used.idx;
        dev.bringup()
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

fn probe_pci() -> Option<VirtioGpu> {
    for dev in 0u8..32 {
        for func in 0u8..8 {
            let id = arch::pci_cfg_read32(ecam(), 0, dev, func, 0x00);
            if (id & 0xFFFF) as u16 != PCI_VENDOR_VIRTIO {
                continue;
            }
            if (id >> 16) as u16 != PCI_DEVICE_VIRTIO_GPU {
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

fn init_pci(bus: u8, dev: u8, func: u8) -> Option<VirtioGpu> {
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

    // Negotiate VIRTIO_F_VERSION_1 only.
    x.cc_w32(CC_DRIVER_FEATURE_SELECT, 1);
    x.cc_w32(CC_DRIVER_FEATURE, VERSION_1_SEL1);
    x.cc_w32(CC_DRIVER_FEATURE_SELECT, 0);
    x.cc_w32(CC_DRIVER_FEATURE, 0);

    x.cc_w8(CC_DEVICE_STATUS, (S_ACK | S_DRIVER | S_FEATURES_OK) as u8);
    if x.cc_r8(CC_DEVICE_STATUS) & S_FEATURES_OK as u8 == 0 {
        return None;
    }

    let (controlq, cmd_buf, resp_buf, fb_frames) = alloc_all();
    if !setup_queue_pci(&mut x, 0, controlq, notify_base, notify_mult) {
        return None;
    }

    x.cc_w8(
        CC_DEVICE_STATUS,
        (S_ACK | S_DRIVER | S_FEATURES_OK | S_DRIVER_OK) as u8,
    );

    let mut dev = VirtioGpu {
        transport: Transport::Pci(x),
        controlq,
        cmd_buf,
        resp_buf,
        fb_frames,
        last_used: 0,
        report: GpuReport::EMPTY,
    };
    // SAFETY: the controlq ring is live frame-pool memory.
    dev.last_used = unsafe { (*(controlq as *const VirtQueue)).used.idx };
    dev.bringup()
}

/// Configure the controlq and record its notify offset.
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
    x.notify_off = notify_base + qnotify * notify_mult;
    x.cc_w16(CC_QUEUE_ENABLE, 1);
    true
}

impl VirtioGpu {
    /// The per-command round-trip report (docs/DISPLAY.md - the honest proof
    /// surface; the test asserts exactly what genuinely returns OK headless).
    pub fn report(&self) -> GpuReport {
        self.report
    }

    #[inline]
    fn cq(&self) -> *mut VirtQueue {
        self.controlq as *mut VirtQueue
    }

    /// Zero the command buffer's working area (padding fields must be clean).
    fn clear_cmd(&self) {
        // SAFETY: cmd_buf is a live frame-pool frame; CMD_CLEAR < 4096.
        unsafe { core::ptr::write_bytes(self.cmd_buf as *mut u8, 0, CMD_CLEAR) }
    }

    /// Submit a `cmd_len`-byte command (already written into `cmd_buf`) plus a
    /// `resp_len`-byte writable response, over a 2-descriptor chain on the
    /// controlq, poll the used ring, and return the response's `ctrl_hdr.type`.
    /// Returns 0 on timeout.
    fn submit_cmd(&mut self, cmd_len: usize, resp_len: usize) -> u32 {
        // Clear the response header so a stale value can't masquerade as success.
        cmd_wr32(self.resp_buf, 0, 0);
        // SAFETY: the ring/buffers live in frame-pool memory reached through the
        // linear map; single-vcore, only touched here.
        unsafe {
            let cmd_pa = arch::virt_to_phys(self.cmd_buf) as u64;
            let resp_pa = arch::virt_to_phys(self.resp_buf) as u64;
            let vq = self.cq();
            // desc0: command (device-readable) -> desc1: response (writable).
            (*vq).desc[0] = Desc {
                addr: cmd_pa,
                len: cmd_len as u32,
                flags: VRING_DESC_F_NEXT,
                next: 1,
            };
            (*vq).desc[1] = Desc {
                addr: resp_pa,
                len: resp_len as u32,
                flags: VRING_DESC_F_WRITE,
                next: 0,
            };
            let idx = (*vq).avail.idx;
            (*vq).avail.ring[(idx as usize) % QSIZE] = 0; // head descriptor
            fence(Ordering::SeqCst);
            (*vq).avail.idx = idx.wrapping_add(1);
            fence(Ordering::SeqCst);

            self.transport.notify();

            let used_idx = addr_of!((*vq).used.idx);
            let mut spins = 0u64;
            while used_idx.read_volatile() == self.last_used {
                fence(Ordering::SeqCst);
                spins += 1;
                if spins > 100_000_000 {
                    return 0;
                }
                core::hint::spin_loop();
            }
            self.last_used = (*vq).used.idx;
        }
        resp_rd32(self.resp_buf, 0)
    }

    /// Write a `virtio_gpu_ctrl_hdr` (type + zeroed flags/fence/ctx/padding) at
    /// the start of the command buffer.
    fn write_hdr(&self, cmd_type: u32) {
        self.clear_cmd();
        cmd_wr32(self.cmd_buf, 0, cmd_type); // type
        // flags/fence_id/ctx_id/padding already zeroed by clear_cmd.
    }

    /// Write a `virtio_gpu_rect{x,y,width,height}` at `off`.
    fn write_rect(&self, off: usize, x: u32, y: u32, w: u32, h: u32) {
        cmd_wr32(self.cmd_buf, off, x);
        cmd_wr32(self.cmd_buf, off + 4, y);
        cmd_wr32(self.cmd_buf, off + 8, w);
        cmd_wr32(self.cmd_buf, off + 12, h);
    }

    /// Run the install-time 2D bring-up. Returns `Some(self)` only if the
    /// essential commands (create_2d + attach) succeeded; otherwise `None`
    /// (skip-with-reason). Records every command's result in [`GpuReport`].
    fn bringup(mut self) -> Option<VirtioGpu> {
        // 1. GET_DISPLAY_INFO - learn scanout 0's size (informational; the
        //    resource is a fixed 128x128 regardless). Tolerate failure/0-size.
        self.write_hdr(CTRL_GET_DISPLAY_INFO);
        // resp: ctrl_hdr(24) + pmodes[16] * (rect 16 + enabled 4 + flags 4 = 24).
        let resp = self.submit_cmd(HDR_LEN, HDR_LEN + 16 * 24);
        if resp == RESP_OK_DISPLAY_INFO {
            self.report.display_info_ok = true;
            // pmodes[0].r = rect at resp offset 24; width@+8, height@+12.
            self.report.display_w = resp_rd32(self.resp_buf, HDR_LEN + 8);
            self.report.display_h = resp_rd32(self.resp_buf, HDR_LEN + 12);
        }

        // 2. RESOURCE_CREATE_2D - resource 1, B8G8R8A8, 128x128.
        self.write_hdr(CTRL_RESOURCE_CREATE_2D);
        cmd_wr32(self.cmd_buf, 24, RESOURCE_ID);
        cmd_wr32(self.cmd_buf, 28, FORMAT_B8G8R8A8_UNORM);
        cmd_wr32(self.cmd_buf, 32, FB_W);
        cmd_wr32(self.cmd_buf, 36, FB_H);
        self.report.create_2d_ok = self.submit_cmd(40, HDR_LEN) == RESP_OK_NODATA;

        // 3. RESOURCE_ATTACH_BACKING - one mem entry per framebuffer frame
        //    (physical address + length), so the backing need not be contiguous.
        self.write_hdr(CTRL_RESOURCE_ATTACH_BACKING);
        cmd_wr32(self.cmd_buf, 24, RESOURCE_ID);
        cmd_wr32(self.cmd_buf, 28, FB_FRAMES as u32);
        for (i, &fva) in self.fb_frames.iter().enumerate() {
            let e = 32 + i * 16;
            cmd_wr64(self.cmd_buf, e, arch::virt_to_phys(fva) as u64);
            cmd_wr32(self.cmd_buf, e + 8, FRAME_SIZE as u32);
            cmd_wr32(self.cmd_buf, e + 12, 0); // padding
        }
        self.report.attach_ok = self.submit_cmd(32 + FB_FRAMES * 16, HDR_LEN) == RESP_OK_NODATA;

        // 4. SET_SCANOUT - bind resource 1 to scanout 0 over the full rect.
        //    May be a no-op with no display backend (headless); recorded, not
        //    required (docs/DISPLAY.md).
        self.write_hdr(CTRL_SET_SCANOUT);
        self.write_rect(24, 0, 0, FB_W, FB_H);
        cmd_wr32(self.cmd_buf, 40, SCANOUT_ID);
        cmd_wr32(self.cmd_buf, 44, RESOURCE_ID);
        self.report.set_scanout_ok = self.submit_cmd(48, HDR_LEN) == RESP_OK_NODATA;

        if self.report.create_2d_ok && self.report.attach_ok {
            Some(self)
        } else {
            None
        }
    }

    /// TRANSFER_TO_HOST_2D the full framebuffer rect into the host resource.
    fn transfer(&mut self) -> bool {
        self.write_hdr(CTRL_TRANSFER_TO_HOST_2D);
        self.write_rect(24, 0, 0, FB_W, FB_H);
        cmd_wr64(self.cmd_buf, 40, 0); // offset
        cmd_wr32(self.cmd_buf, 48, RESOURCE_ID);
        cmd_wr32(self.cmd_buf, 52, 0); // padding
        self.submit_cmd(56, HDR_LEN) == RESP_OK_NODATA
    }

    /// RESOURCE_FLUSH the full framebuffer rect to the scanout.
    fn flush(&mut self) -> bool {
        self.write_hdr(CTRL_RESOURCE_FLUSH);
        self.write_rect(24, 0, 0, FB_W, FB_H);
        cmd_wr32(self.cmd_buf, 40, RESOURCE_ID);
        cmd_wr32(self.cmd_buf, 44, 0); // padding
        self.submit_cmd(48, HDR_LEN) == RESP_OK_NODATA
    }

    /// Present a client frame: copy `len` bytes from the cell VA `buf_va` into
    /// the kernel-side framebuffer (the frames attached to the resource), then
    /// TRANSFER_TO_HOST_2D + RESOURCE_FLUSH. Returns whether both succeeded.
    ///
    /// # Safety
    /// `buf_va` is the calling cell's mapped buffer, readable during its trap.
    unsafe fn present_frame(&mut self, buf_va: u64, len: usize) -> bool {
        let total = len.min(FB_BYTES);
        // Copy the (contiguous) client buffer into the non-contiguous fb frames.
        let mut done = 0;
        for &fva in self.fb_frames.iter() {
            if done >= total {
                break;
            }
            let n = (total - done).min(FRAME_SIZE);
            // SAFETY: src is `total` readable cell bytes; dst is a live frame.
            unsafe {
                core::ptr::copy_nonoverlapping((buf_va as *const u8).add(done), fva as *mut u8, n);
            }
            done += n;
        }
        let t = self.transfer();
        let f = self.flush();
        self.report.transfer_ok = t;
        self.report.flush_ok = f;
        t && f
    }
}

// -------------------------------------------------- kernel queue-opcode bridge
//
// The device is a single-instance kernel resource (like the NIC): a test kernel
// installs it once, and the OP_GPU_PRESENT queue opcode reaches it during a
// cell's `SYS_DOORBELL` trap. `buf_va` is the calling cell's own mapped memory
// (its address space is active during the drain), so the present copies from
// there directly.

static mut GPU: Option<VirtioGpu> = None;

/// Install the discovered device as the kernel's GPU (called once at boot), and
/// register it as the `svc::DisplayOps` bridge `OP_GPU_PRESENT` reaches. Same
/// reasoning as `virtio_net::install`: registration where the device is known to
/// exist, so a kernel with no display answers `STATUS_IO` honestly and the
/// queue's dispatch need not name this module (docs/ARCHITECTURE-DEBT.md 3.2).
pub fn install(dev: VirtioGpu) {
    // SAFETY: single-threaded boot; set once before any cell runs.
    unsafe {
        *core::ptr::addr_of_mut!(GPU) = Some(dev);
    }
    crate::svc::set_display_ops(crate::svc::DisplayOps { present });
}

fn gpu_mut() -> Option<&'static mut VirtioGpu> {
    // SAFETY: single-CPU; the GPU is a single-instance resource touched only
    // during a cell's (serialised) doorbell trap.
    unsafe { (*core::ptr::addr_of_mut!(GPU)).as_mut() }
}

/// The installed GPU's per-command report (for the test kernel to print/assert).
pub fn report() -> Option<GpuReport> {
    // SAFETY: single-CPU; read-only view of the installed device.
    unsafe { (*core::ptr::addr_of!(GPU)).as_ref().map(|g| g.report()) }
}

/// `OP_GPU_PRESENT`: present the `w x h` RGBA frame at the cell VA `buf_va` (copy
/// into the framebuffer, transfer to host, flush to the scanout). Returns
/// `(status, bytes_presented)`.
///
/// # Safety
/// `buf_va` is the calling cell's mapped buffer; called only during its trap.
pub fn present(buf_va: u64, w: u32, h: u32) -> (u32, u32) {
    use crate::queue::{STATUS_IO, STATUS_OK};
    let Some(dev) = gpu_mut() else {
        return (STATUS_IO, 0);
    };
    let len = (w as usize).saturating_mul(h as usize).saturating_mul(4);
    let n = len.min(FB_BYTES);
    // SAFETY: `buf_va` was range-checked for `len` readable bytes in the calling
    // cell by `queue::run_opcode`, whose address space is active; `n <= len`.
    if unsafe { dev.present_frame(buf_va, n) } {
        // The GPU status pane's counts (docs/OBSERVABILITY.md 11, S5): a
        // completed present and the bytes it copied into the device resource.
        crate::obs::cpu_bump(crate::obs::cpu::CTR_GPU_PRESENTS, 1);
        crate::obs::cpu_bump(crate::obs::cpu::CTR_GPU_PRESENT_BYTES, n as u64);
        (STATUS_OK, n as u32)
    } else {
        (STATUS_IO, 0)
    }
}
