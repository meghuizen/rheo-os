// The block-source seam a filesystem reads through (docs/FILESYSTEMS.md 1). A
// `BlockSource` is byte-addressed random access - the layer between a filesystem
// driver and wherever the bytes physically live. The ext4 driver (the `ext4fs`
// crate over `ext4plus`) reads every field through this, so an image far larger
// than RAM streams through a bounded block cache rather than residing whole in
// memory (docs/ARCHITECTURE-DEBT.md 4.0 blocker 2 - "a binary need not reside
// whole in RAM"). Two implementors exist: an in-RAM `&[u8]` (here), and the
// kernel's `hw::block::BlockCache` over a live device (bridged in the test).
//
// This trait lives in `posix` (zero-dep) rather than in `ext4fs` because it is
// the *seam*: `posix` defines it, `ext4fs` consumes it, and the block cache is
// adapted to it - so no layer depends on a layer above it.
//
// Regular // comments keep this file host-includable alongside the rest of posix.

use crate::vfs::Errno;

/// Byte-addressed random-access source under a filesystem: fill `buf` with the
/// bytes starting at byte offset `off`. A short source (a read past the end) is
/// an error, not a silent zero-fill - the filesystem decides what a hole means,
/// the source only reports what is really there.
pub trait BlockSource {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), Errno>;
}

/// The simplest source: an in-RAM image (`include_bytes!` in the OS, a leaked
/// buffer in a host test). No streaming - the whole image is already resident.
impl BlockSource for &[u8] {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), Errno> {
        let off = off as usize;
        let end = off.checked_add(buf.len()).ok_or(Errno::Io)?;
        let src = self.get(off..end).ok_or(Errno::Io)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}
