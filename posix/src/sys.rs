//! The POSIX file syscall surface (docs/POSIX-PERSONALITY.md 1): a file-
//! descriptor table over the VFS. `open`/`read`/`write`/`close`/`lseek`/
//! `stat`/`getdents`/`mkdir`/`unlink`, each translated to native VFS ops
//! against the mounted filesystems. Errors are `Errno` (see `errno` for the
//! numeric code). Single-vcore, so the fd table is a plain static slab.

use crate::mount;
use crate::vfs::{DirEntry, Errno, FileSystem, FileType, Metadata, NodeId};
use alloc::rc::Rc;
use alloc::vec::Vec;

pub const O_RDONLY: u32 = 0;
pub const O_WRONLY: u32 = 1;
pub const O_RDWR: u32 = 2;
pub const O_CREAT: u32 = 0o100;
pub const O_TRUNC: u32 = 0o1000;
pub const O_APPEND: u32 = 0o2000;
pub const O_DIRECTORY: u32 = 0o200000;

#[derive(Copy, Clone)]
pub enum Whence {
    Set,
    Cur,
    End,
}

struct OpenFile {
    fs: Rc<dyn FileSystem>,
    node: NodeId,
    offset: u64,
    kind: FileType,
    readable: bool,
    writable: bool,
}

static mut FDS: Option<Vec<Option<OpenFile>>> = None;

fn fds() -> &'static mut Vec<Option<OpenFile>> {
    // SAFETY: single-vcore cooperative; no concurrent fd-table access.
    unsafe { (*core::ptr::addr_of_mut!(FDS)).get_or_insert_with(Vec::new) }
}

/// Close every descriptor (tests start clean).
pub fn reset() {
    *fds() = Vec::new();
}

fn alloc_fd(f: OpenFile) -> usize {
    let table = fds();
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(f);
            return i;
        }
    }
    table.push(Some(f));
    table.len() - 1
}

fn with_fd<R>(fd: usize, f: impl FnOnce(&mut OpenFile) -> Result<R, Errno>) -> Result<R, Errno> {
    let slot = fds()
        .get_mut(fd)
        .and_then(|s| s.as_mut())
        .ok_or(Errno::Badf)?;
    f(slot)
}

/// Open (optionally creating) `path`. Returns a file descriptor.
pub fn open(path: &str, flags: u32) -> Result<usize, Errno> {
    let acc = flags & 3;
    let (fs, node) = match mount::resolve(path) {
        Ok(x) => x,
        Err(Errno::NoEnt) if flags & O_CREAT != 0 => {
            let (fs, parent, name) = mount::resolve_parent(path)?;
            let node = fs.create(parent, &name, FileType::Regular)?;
            (fs, node)
        }
        Err(e) => return Err(e),
    };
    let md = fs.metadata(node)?;
    if flags & O_DIRECTORY != 0 && md.kind != FileType::Dir {
        return Err(Errno::NotDir);
    }
    let mut offset = 0;
    if md.kind == FileType::Regular {
        if flags & O_TRUNC != 0 && acc != O_RDONLY {
            fs.truncate(node, 0)?;
        }
        if flags & O_APPEND != 0 {
            offset = md.len;
        }
    }
    Ok(alloc_fd(OpenFile {
        fs,
        node,
        offset,
        kind: md.kind,
        readable: acc == O_RDONLY || acc == O_RDWR,
        writable: acc == O_WRONLY || acc == O_RDWR,
    }))
}

pub fn read(fd: usize, buf: &mut [u8]) -> Result<usize, Errno> {
    with_fd(fd, |of| {
        if !of.readable {
            return Err(Errno::Badf);
        }
        if of.kind == FileType::Dir {
            return Err(Errno::IsDir);
        }
        let n = of.fs.read_at(of.node, of.offset, buf)?;
        of.offset += n as u64;
        Ok(n)
    })
}

pub fn write(fd: usize, buf: &[u8]) -> Result<usize, Errno> {
    with_fd(fd, |of| {
        if !of.writable {
            return Err(Errno::Badf);
        }
        let n = of.fs.write_at(of.node, of.offset, buf)?;
        of.offset += n as u64;
        Ok(n)
    })
}

pub fn lseek(fd: usize, off: i64, whence: Whence) -> Result<u64, Errno> {
    with_fd(fd, |of| {
        let base = match whence {
            Whence::Set => 0i64,
            Whence::Cur => of.offset as i64,
            Whence::End => of.fs.metadata(of.node)?.len as i64,
        };
        let pos = base.checked_add(off).ok_or(Errno::Inval)?;
        if pos < 0 {
            return Err(Errno::Inval);
        }
        of.offset = pos as u64;
        Ok(of.offset)
    })
}

pub fn close(fd: usize) -> Result<(), Errno> {
    let slot = fds().get_mut(fd).ok_or(Errno::Badf)?;
    if slot.take().is_none() {
        return Err(Errno::Badf);
    }
    Ok(())
}

pub fn fstat(fd: usize) -> Result<Metadata, Errno> {
    with_fd(fd, |of| of.fs.metadata(of.node))
}

pub fn stat(path: &str) -> Result<Metadata, Errno> {
    let (fs, node) = mount::resolve(path)?;
    fs.metadata(node)
}

/// Directory entries, with `.`/`..` filtered out (std-style).
pub fn getdents(path: &str) -> Result<Vec<DirEntry>, Errno> {
    let (fs, node) = mount::resolve(path)?;
    let mut entries = fs.readdir(node)?;
    entries.retain(|e| e.name != "." && e.name != "..");
    Ok(entries)
}

pub fn mkdir(path: &str) -> Result<(), Errno> {
    let (fs, parent, name) = mount::resolve_parent(path)?;
    fs.create(parent, &name, FileType::Dir)?;
    Ok(())
}

pub fn unlink(path: &str) -> Result<(), Errno> {
    let (fs, parent, name) = mount::resolve_parent(path)?;
    fs.unlink(parent, &name)
}

/// The negative errno a syscall ABI would return (POSIX numbers).
pub fn errno(e: Errno) -> i32 {
    match e {
        Errno::NoEnt => 2,
        Errno::Io => 5,
        Errno::Badf => 9,
        Errno::NoSpc => 28,
        Errno::NotDir => 20,
        Errno::IsDir => 21,
        Errno::Inval => 22,
        Errno::Exists => 17,
        Errno::Rofs => 30,
        Errno::NoSys => 38,
        Errno::NameTooLong => 36,
        Errno::NotEmpty => 39,
    }
}
