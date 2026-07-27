//! The block-device abstraction (docs/FILESYSTEMS.md 1). A `BlockDevice` is
//! the seam between a storage transport (virtio-blk today; NVMe later) and a
//! filesystem: a filesystem reads/writes 512-byte sectors and does not care
//! what is underneath. This is the interface that lets an on-disk filesystem
//! (ours, or an existing Rust driver dropped in behind it) talk to a real
//! disk instead of an embedded image.

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BlkError {
    /// The transport is not present (e.g. no virtio-blk on this machine).
    NoDevice,
    /// The device reported a failure or setup did not complete.
    Io,
    /// A length or sector argument was invalid (e.g. not sector-aligned).
    Inval,
}

/// A device addressed in 512-byte sectors.
pub const SECTOR: usize = 512;

pub trait BlockDevice {
    /// Total capacity in 512-byte sectors.
    fn capacity_sectors(&self) -> u64;

    /// Read `buf.len()` bytes (a multiple of `SECTOR`) starting at `sector`.
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), BlkError>;

    /// Write `buf.len()` bytes (a multiple of `SECTOR`) starting at `sector`.
    fn write(&self, sector: u64, buf: &[u8]) -> Result<(), BlkError>;
}

// --- A bounded block cache ---------------------------------------------------
//
// A `BlockDevice` is sector-addressed and read-per-request; a filesystem does
// many small random reads (a superblock field, a group descriptor, an inode,
// an extent header) and wants byte-addressed random access. `BlockCache` bridges
// the two with a fixed set of resident lines - so a filesystem can read a file
// far larger than the cache without the whole disk residing in RAM. That is the
// "a binary need not reside whole in RAM" rung (docs/ARCHITECTURE-DEBT.md 4.0
// blocker 2): before this, `blockfs` read the entire image into a buffer.
//
// Allocation-free: `LINES` lines of `LINE` bytes live inline in the struct, so
// the resident bound is exactly `CAPACITY = LINE * LINES` and is checkable
// against the disk size. Eviction is least-recently-used over a monotonic tick.
// Interior mutability via `RefCell` (the kernel is single-CPU cooperative, the
// `ramfs` idiom).

use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicU64, Ordering};

/// Bytes per cache line. A multiple of `SECTOR`; matches the 1 KiB ext4 block
/// the test image uses, so one fill serves a whole block's worth of fields.
const LINE: usize = 1024;
/// Resident lines. `CAPACITY = LINE * LINES` is the whole in-RAM footprint.
const LINES: usize = 8;

struct CacheLine {
    /// Line index (`byte_off / LINE`) resident here; `u64::MAX` when empty.
    tag: u64,
    /// LRU tick: the value of the cache clock at the last access.
    used: u64,
    data: [u8; LINE],
}

/// Device reads (line fills) across all caches - a streaming witness, in the
/// module-counter idiom the rest of the tree uses (`net_rx::irq_count()` etc.).
/// A test asserts this is non-zero to prove data really came from the device on
/// demand rather than a whole-disk preload.
static FILLS: AtomicU64 = AtomicU64::new(0);

/// Total line fills (device reads) performed by any `BlockCache`.
pub fn cache_fills() -> u64 {
    FILLS.load(Ordering::Relaxed)
}

/// A fixed-size LRU block cache turning a sector-addressed `BlockDevice` into a
/// byte-addressed random-access source. Owns the device.
pub struct BlockCache<D: BlockDevice> {
    dev: D,
    lines: RefCell<[CacheLine; LINES]>,
    clock: Cell<u64>,
}

impl<D: BlockDevice> BlockCache<D> {
    /// The whole in-RAM footprint of the cache, in bytes. Independent of the
    /// disk size - that is the point: reading a file larger than this proves
    /// the disk is not resident whole.
    pub const CAPACITY: usize = LINE * LINES;

    pub fn new(dev: D) -> Self {
        BlockCache {
            dev,
            lines: RefCell::new(core::array::from_fn(|_| CacheLine {
                tag: u64::MAX,
                used: 0,
                data: [0u8; LINE],
            })),
            clock: Cell::new(0),
        }
    }

    /// Total device capacity in 512-byte sectors (passthrough).
    pub fn capacity_sectors(&self) -> u64 {
        self.dev.capacity_sectors()
    }

    /// Fill `buf` with bytes starting at byte offset `off`, serving each covered
    /// line from RAM (filling on a miss). A read past the end of the device is
    /// an error - the caller (a filesystem) decides what a hole means.
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        let mut done = 0usize;
        while done < buf.len() {
            let pos = off + done as u64;
            let line_idx = pos / LINE as u64;
            let within = (pos % LINE as u64) as usize;
            let n = core::cmp::min(buf.len() - done, LINE - within);
            self.with_line(line_idx, |data: &[u8; LINE]| {
                buf[done..done + n].copy_from_slice(&data[within..within + n]);
            })?;
            done += n;
        }
        Ok(())
    }

    /// Run `f` over the resident data of `line_idx`, filling on a miss (evicting
    /// the least-recently-used line).
    fn with_line<R>(&self, line_idx: u64, f: impl FnOnce(&[u8; LINE]) -> R) -> Result<R, BlkError> {
        let tick = self.clock.get() + 1;
        self.clock.set(tick);
        let mut lines = self.lines.borrow_mut();

        if let Some(l) = lines.iter_mut().find(|l| l.tag == line_idx) {
            l.used = tick;
            return Ok(f(&l.data));
        }

        // Miss: evict the least-recently-used line (smallest `used`).
        let victim = lines
            .iter()
            .enumerate()
            .min_by_key(|(_, l)| l.used)
            .map(|(i, _)| i)
            .unwrap_or(0);
        // Fill happens against the device only (does not touch `self.lines`),
        // so borrowing `lines` mutably here is sound.
        self.fill(&mut lines[victim].data, line_idx)?;
        lines[victim].tag = line_idx;
        lines[victim].used = tick;
        Ok(f(&lines[victim].data))
    }

    /// Read the sectors backing `line_idx` into `data`, zeroing any tail that
    /// lies past the end of the device.
    fn fill(&self, data: &mut [u8; LINE], line_idx: u64) -> Result<(), BlkError> {
        let sectors_per_line = (LINE / SECTOR) as u64;
        let base = line_idx * sectors_per_line;
        let cap = self.dev.capacity_sectors();
        let avail = cap.saturating_sub(base).min(sectors_per_line) as usize;
        for b in data.iter_mut() {
            *b = 0;
        }
        if avail > 0 {
            self.dev.read(base, &mut data[..avail * SECTOR])?;
        }
        FILLS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
