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
