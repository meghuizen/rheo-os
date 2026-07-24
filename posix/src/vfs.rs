// The VFS core - the translation layer every filesystem plugs into and the
// POSIX/std-fs layers sit on top of (docs/FILESYSTEMS.md, POSIX-PERSONALITY.md
// 1,3). A filesystem is a `FileSystem` trait object addressed by opaque
// `NodeId`s (an inode number for ext4, a slab index for ramfs). Read-only
// filesystems implement just the read half; the write methods default to
// EROFS. All methods take `&self` (single-vcore cooperative; writable
// backends use interior mutability), so a mount table can hold trait objects.
//
// Regular // comments keep this file includable by the host validation harness.

use alloc::string::String;
use alloc::vec::Vec;

/// Opaque per-filesystem node handle.
pub type NodeId = u64;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FileType {
    Regular,
    Dir,
    Symlink,
    Other,
}

#[derive(Copy, Clone, Debug)]
pub struct Metadata {
    pub kind: FileType,
    pub len: u64,
    pub mode: u16,
}

pub struct DirEntry {
    pub name: String,
    pub node: NodeId,
    pub kind: FileType,
}

/// POSIX-shaped errors (subset). Mapped to negative errno by the fd layer.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Errno {
    NoEnt,       // ENOENT
    NotDir,      // ENOTDIR
    IsDir,       // EISDIR
    Exists,      // EEXIST
    Inval,       // EINVAL
    Rofs,        // EROFS
    Badf,        // EBADF
    NoSpc,       // ENOSPC
    NoSys,       // ENOSYS
    NameTooLong, // ENAMETOOLONG
    NotEmpty,    // ENOTEMPTY
    Io,          // EIO
}

/// A mounted filesystem. Reads are mandatory; writes default to read-only.
pub trait FileSystem {
    fn root(&self) -> NodeId;
    fn lookup(&self, dir: NodeId, name: &str) -> Result<NodeId, Errno>;
    fn metadata(&self, node: NodeId) -> Result<Metadata, Errno>;
    fn read_at(&self, node: NodeId, off: u64, buf: &mut [u8]) -> Result<usize, Errno>;
    fn readdir(&self, node: NodeId) -> Result<Vec<DirEntry>, Errno>;

    // Writable filesystems override these; the default is a read-only mount.
    fn create(&self, _dir: NodeId, _name: &str, _kind: FileType) -> Result<NodeId, Errno> {
        Err(Errno::Rofs)
    }
    fn write_at(&self, _node: NodeId, _off: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(Errno::Rofs)
    }
    fn truncate(&self, _node: NodeId, _len: u64) -> Result<(), Errno> {
        Err(Errno::Rofs)
    }
    fn unlink(&self, _dir: NodeId, _name: &str) -> Result<(), Errno> {
        Err(Errno::Rofs)
    }
}
