#![no_std]
//! The disk **ext4** filesystem for rheo-os: an adapter from the `ext4plus`
//! crate (Google's read-only `ext4-view`; `no_std`; driven here in `sync` mode)
//! to the `posix::FileSystem` trait, fed by a `posix::BlockSource` so it streams
//! through the kernel's bounded block cache - the whole image need not reside in
//! RAM (docs/ARCHITECTURE-DEBT.md 4.0 blocker 2).
//!
//! This replaces the hand-rolled `posix::Ext4` parser at the `BlockDevice` seam
//! CLAUDE.md anticipates ("existing Rust FS drivers can be dropped in behind it
//! rather than hand-written - gated by the no-deps rule, a doc must name the
//! crate"; docs/FILESYSTEMS.md names `ext4plus` and its transitive deps).
//!
//! **Posture.** The driver is kernel-resident behind `svc::FileOps`, whose
//! call site is a *synchronous* syscall trap over a *blocking* virtio-blk read,
//! so `sync` mode (`maybe-async` strips the futures) is the honest fit - async
//! here would be poll-to-completion over blocking I/O with no overlap. The
//! async-first payoff comes when the filesystem moves into a **service cell**
//! over the queue ABI (the FUSE-over-queues end state), where a read parks a
//! strand on an `OP_READ` completion - and where **NVMe's** submission/
//! completion queues realize real queue-depth parallelism. That flips this same
//! crate to its default async mode; the `BlockSource`/`BlockDevice` seam and the
//! `maybe-async` design are exactly what keep this choice reversible.

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::num::NonZeroU32;

use ext4plus::prelude::{Dir, Ext4, Ext4Read, FileType as ExtType, Inode};
use posix::{BlockSource, DirEntry, Errno, FileSystem, FileType, Metadata, NodeId};

/// Present a `posix::BlockSource` as the block seam `ext4plus` reads through.
struct SourceReader(Box<dyn BlockSource>);

/// The error `Ext4Read` wants on a failed device read. `ext4plus`'s block-read
/// contract is a boxed `core::error::Error` (Send + Sync); a `BlockSource`
/// reports only success/failure, so one unit error is enough.
#[derive(Debug)]
struct ReadFailed;

impl core::fmt::Display for ReadFailed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "block source read failed")
    }
}
impl core::error::Error for ReadFailed {}

impl Ext4Read for SourceReader {
    fn read(
        &self,
        start_byte: u64,
        dst: &mut [u8],
    ) -> Result<(), Box<dyn core::error::Error + Send + Sync + 'static>> {
        self.0.read_at(start_byte, dst).map_err(|_| {
            Box::new(ReadFailed) as Box<dyn core::error::Error + Send + Sync + 'static>
        })
    }
}

/// A mounted ext4 (or ext2) image, backed by `ext4plus`.
pub struct Ext4Fs {
    fs: Ext4,
}

impl Ext4Fs {
    /// Load a filesystem from a block source - the committed image via `&[u8]`,
    /// or a live disk via the kernel's `BlockCache`.
    pub fn new(src: Box<dyn BlockSource>) -> Result<Ext4Fs, Errno> {
        let fs = Ext4::load(Box::new(SourceReader(src))).map_err(|_| Errno::Inval)?;
        Ok(Ext4Fs { fs })
    }

    /// Read the inode numbered `node` (the VFS `NodeId` is the ext4 inode index).
    fn inode(&self, node: NodeId) -> Result<Inode, Errno> {
        let idx = NonZeroU32::new(node as u32).ok_or(Errno::NoEnt)?;
        Inode::read(&self.fs, idx).map_err(|_| Errno::Io)
    }
}

fn map_type(t: ExtType) -> FileType {
    match t {
        ExtType::Regular => FileType::Regular,
        ExtType::Directory => FileType::Dir,
        ExtType::Symlink => FileType::Symlink,
        _ => FileType::Other,
    }
}

impl FileSystem for Ext4Fs {
    fn root(&self) -> NodeId {
        2 // ext4 root is always inode 2
    }

    fn lookup(&self, dir: NodeId, name: &str) -> Result<NodeId, Errno> {
        for e in self.readdir(dir)? {
            if e.name == name {
                return Ok(e.node);
            }
        }
        Err(Errno::NoEnt)
    }

    fn metadata(&self, node: NodeId) -> Result<Metadata, Errno> {
        let m = self.inode(node)?.metadata();
        Ok(Metadata {
            kind: map_type(m.file_type()),
            len: m.len(),
            mode: m.mode() & 0x0FFF,
            node,
        })
    }

    fn read_at(&self, node: NodeId, off: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let ino = self.inode(node)?;
        if ino.metadata().is_dir() {
            return Err(Errno::IsDir);
        }
        // Fill `buf` completely (short of EOF): the `FileSystem::read_at` contract
        // is fill-the-buffer (ramfs upholds it, and the ELF loader's `stream_*`
        // paths read once and check `got == len`), but `ext4plus::read_at` returns
        // block-granular short reads, so loop over it. The streaming property
        // holds - only the requested bytes are read, not the whole file.
        let mut done = 0usize;
        while done < buf.len() {
            let n = ext4plus::prelude::read_at(&self.fs, &ino, &mut buf[done..], off + done as u64)
                .map_err(|_| Errno::Io)?;
            if n == 0 {
                break; // EOF
            }
            done += n;
        }
        Ok(done)
    }

    fn readdir(&self, node: NodeId) -> Result<Vec<DirEntry>, Errno> {
        let ino = self.inode(node)?;
        if !ino.metadata().is_dir() {
            return Err(Errno::NotDir);
        }
        let dir = Dir::open_inode(&self.fs, ino).map_err(|_| Errno::NotDir)?;
        let mut out = Vec::new();
        for entry in dir.read_dir().map_err(|_| Errno::Io)? {
            let entry = entry.map_err(|_| Errno::Io)?;
            // Skip a name that is not valid UTF-8 rather than fail the whole
            // listing (the VFS speaks `&str`).
            let Ok(name) = entry.file_name().as_str() else {
                continue;
            };
            let kind = entry.file_type().map(map_type).unwrap_or(FileType::Other);
            out.push(DirEntry {
                name: name.to_string(),
                node: u32::from(entry.inode) as NodeId,
                kind,
            });
        }
        Ok(out)
    }
}
