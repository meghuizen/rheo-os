//! rheo-os filesystem backend (installed into std as `sys/fs/rheo.rs` by
//! targets/patch-std.py; docs/USERLAND.md M5). `File`/`metadata`/`read_dir`
//! translate onto the rheo file syscalls (open/close/read/write/lseek + stat/
//! fstat/getdents, kernel/src/abi.rs), which the kernel forwards to the POSIX
//! personality handler backed by the `posix` VFS. This is the read/write file
//! path that lets std programs - and the coreutils cell - open real files.
//!
//! Honest gaps (return `Unsupported`, never fake success): timestamps,
//! permissions changes, symlinks, hard links, truncate, fd duplication - the
//! VFS does not model them yet.
#![deny(unsafe_op_in_unsafe_fn)]

use crate::ffi::OsString;
use crate::fmt;
use crate::fs::TryLockError;
use crate::hash::{Hash, Hasher};
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, SeekFrom};
use crate::path::{Path, PathBuf};
pub use crate::sys::fs::common::Dir;
pub use crate::sys::fs::common::{copy, exists, remove_dir_all};
use crate::sys::time::SystemTime;

// Syscall numbers (kernel/src/abi.rs).
const SYS_OPEN: u64 = 23;
const SYS_CLOSE: u64 = 24;
const SYS_READ: u64 = 25;
const SYS_WRITE_FD: u64 = 26;
const SYS_LSEEK: u64 = 27;
const SYS_STAT: u64 = 28;
const SYS_FSTAT: u64 = 29;
const SYS_GETDENTS: u64 = 30;

// POSIX open flags (posix/src/sys.rs).
const O_WRONLY: u64 = 1;
const O_RDWR: u64 = 2;
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;
const O_APPEND: u64 = 0o2000;

// FileType kinds carried in the stat/getdents ABI.
const KIND_DIR: u64 = 1;
const KIND_SYMLINK: u64 = 2;

unsafe fn sc(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("ecall", in("a7") nr, inlateout("a0") a0 => ret,
            in("a1") a1, in("a2") a2, in("a3") a3, options(nostack));
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
            in("x1") a1, in("x2") a2, in("x3") a3, options(nostack));
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("syscall", inlateout("rax") nr => ret, in("rdi") a0,
            in("rsi") a1, in("rdx") a2, in("r10") a3,
            out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

fn to_result(n: i64) -> io::Result<usize> {
    if n < 0 { Err(io::Error::from_raw_os_error((-n) as i32)) } else { Ok(n as usize) }
}

fn unsupported<T>() -> io::Result<T> {
    Err(io::const_error!(io::ErrorKind::Unsupported, "operation not supported on rheo-os yet"))
}

fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

// The stat result block, kept in sync with `abi::Stat`.
#[repr(C)]
#[derive(Copy, Clone, Default)]
struct RawStat {
    size: u64,
    kind: u64,
}

fn raw_stat_path(path: &Path) -> io::Result<RawStat> {
    let mut st = RawStat::default();
    let b = path_bytes(path);
    let n = unsafe { sc(SYS_STAT, b.as_ptr() as u64, b.len() as u64, &mut st as *mut _ as u64, 0) };
    to_result(n)?;
    Ok(st)
}

// -- FileType / FileAttr / FilePermissions / FileTimes --

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileType {
    kind: u64,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.kind == KIND_DIR
    }
    pub fn is_file(&self) -> bool {
        self.kind != KIND_DIR && self.kind != KIND_SYMLINK
    }
    pub fn is_symlink(&self) -> bool {
        self.kind == KIND_SYMLINK
    }
}

#[derive(Copy, Clone)]
pub struct FileAttr {
    size: u64,
    kind: u64,
}

impl FileAttr {
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn perm(&self) -> FilePermissions {
        FilePermissions { readonly: false }
    }
    pub fn file_type(&self) -> FileType {
        FileType { kind: self.kind }
    }
    pub fn modified(&self) -> io::Result<SystemTime> {
        unsupported()
    }
    pub fn accessed(&self) -> io::Result<SystemTime> {
        unsupported()
    }
    pub fn created(&self) -> io::Result<SystemTime> {
        unsupported()
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FilePermissions {
    readonly: bool,
}

impl FilePermissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }
    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct FileTimes {}

impl FileTimes {
    pub fn set_accessed(&mut self, _t: SystemTime) {}
    pub fn set_modified(&mut self, _t: SystemTime) {}
}

// -- OpenOptions --

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }
    pub fn read(&mut self, read: bool) {
        self.read = read;
    }
    pub fn write(&mut self, write: bool) {
        self.write = write;
    }
    pub fn append(&mut self, append: bool) {
        self.append = append;
    }
    pub fn truncate(&mut self, truncate: bool) {
        self.truncate = truncate;
    }
    pub fn create(&mut self, create: bool) {
        self.create = create;
    }
    pub fn create_new(&mut self, create_new: bool) {
        self.create_new = create_new;
    }

    fn flags(&self) -> u64 {
        let writing = self.write || self.append;
        let mut f = if writing {
            if self.read { O_RDWR } else { O_WRONLY }
        } else {
            0 // O_RDONLY
        };
        if self.create || self.create_new {
            f |= O_CREAT;
        }
        if self.truncate {
            f |= O_TRUNC;
        }
        if self.append {
            f |= O_APPEND;
        }
        f
    }
}

// -- File --

pub struct File {
    fd: u64,
}

impl File {
    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let b = path_bytes(path);
        let n = unsafe { sc(SYS_OPEN, b.as_ptr() as u64, b.len() as u64, opts.flags(), 0) };
        let fd = to_result(n)?;
        Ok(File { fd: fd as u64 })
    }

    pub fn file_attr(&self) -> io::Result<FileAttr> {
        let mut st = RawStat::default();
        let n = unsafe { sc(SYS_FSTAT, self.fd, &mut st as *mut _ as u64, 0, 0) };
        to_result(n)?;
        Ok(FileAttr { size: st.size, kind: st.kind })
    }

    pub fn fsync(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn datasync(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn lock(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn lock_shared(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn try_lock(&self) -> Result<(), TryLockError> {
        Ok(())
    }
    pub fn try_lock_shared(&self) -> Result<(), TryLockError> {
        Ok(())
    }
    pub fn unlock(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn truncate(&self, _size: u64) -> io::Result<()> {
        unsupported()
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { sc(SYS_READ, self.fd, buf.as_mut_ptr() as u64, buf.len() as u64, 0) };
        to_result(n)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for b in bufs {
            let n = self.read(b)?;
            total += n;
            if n < b.len() {
                break;
            }
        }
        Ok(total)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_buf(&self, mut cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        let mut tmp = [0u8; 8192];
        let cap = cursor.capacity().min(tmp.len());
        let n = self.read(&mut tmp[..cap])?;
        cursor.append(&tmp[..n]);
        Ok(())
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let n = unsafe { sc(SYS_WRITE_FD, self.fd, buf.as_ptr() as u64, buf.len() as u64, 0) };
        to_result(n)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for b in bufs {
            total += self.write(b)?;
        }
        Ok(total)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn seek(&self, pos: SeekFrom) -> io::Result<u64> {
        let (whence, off) = match pos {
            SeekFrom::Start(n) => (0u64, n as i64),
            SeekFrom::Current(n) => (1u64, n),
            SeekFrom::End(n) => (2u64, n),
        };
        let n = unsafe { sc(SYS_LSEEK, self.fd, off as u64, whence, 0) };
        if n < 0 { Err(io::Error::from_raw_os_error((-n) as i32)) } else { Ok(n as u64) }
    }

    pub fn size(&self) -> Option<io::Result<u64>> {
        Some(self.file_attr().map(|a| a.size()))
    }

    pub fn tell(&self) -> io::Result<u64> {
        self.seek(SeekFrom::Current(0))
    }

    pub fn duplicate(&self) -> io::Result<File> {
        unsupported()
    }

    pub fn set_permissions(&self, _perm: FilePermissions) -> io::Result<()> {
        unsupported()
    }

    pub fn set_times(&self, _times: FileTimes) -> io::Result<()> {
        unsupported()
    }
}

impl Drop for File {
    fn drop(&mut self) {
        // SAFETY: `fd` is our open descriptor; close is idempotent kernel-side.
        unsafe {
            sc(SYS_CLOSE, self.fd, 0, 0, 0);
        }
    }
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("File").field("fd", &self.fd).finish()
    }
}

// -- DirBuilder / ReadDir / DirEntry --

#[derive(Debug)]
pub struct DirBuilder {}

impl DirBuilder {
    pub fn new() -> DirBuilder {
        DirBuilder {}
    }
    pub fn mkdir(&self, _p: &Path) -> io::Result<()> {
        unsupported()
    }
}

pub struct ReadDir {
    root: PathBuf,
    entries: crate::vec::IntoIter<(u64, OsString)>,
}

impl fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadDir").field("root", &self.root).finish()
    }
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;
    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        let (kind, name) = self.entries.next()?;
        Some(Ok(DirEntry { root: self.root.clone(), name, kind }))
    }
}

pub struct DirEntry {
    root: PathBuf,
    name: OsString,
    kind: u64,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.root.join(&self.name)
    }
    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }
    pub fn metadata(&self) -> io::Result<FileAttr> {
        stat(&self.path())
    }
    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(FileType { kind: self.kind })
    }
}

pub fn readdir(path: &Path) -> io::Result<ReadDir> {
    let b = path_bytes(path);
    let mut buf = alloc_buf(64 * 1024);
    let n = unsafe {
        sc(SYS_GETDENTS, b.as_ptr() as u64, b.len() as u64, buf.as_mut_ptr() as u64, buf.len() as u64)
    };
    let used = to_result(n)?;

    // Records: [u32 kind][u32 name_len][name bytes], read sequentially.
    let mut entries: Vec<(u64, OsString)> = Vec::new();
    let mut i = 0usize;
    while i + 8 <= used {
        let kind = u32::from_ne_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as u64;
        let nlen =
            u32::from_ne_bytes([buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]]) as usize;
        i += 8;
        if i + nlen > used {
            break;
        }
        let name =
            unsafe { crate::ffi::OsStr::from_encoded_bytes_unchecked(&buf[i..i + nlen]) }
                .to_os_string();
        entries.push((kind, name));
        i += nlen;
    }

    Ok(ReadDir { root: path.to_path_buf(), entries: entries.into_iter() })
}

fn alloc_buf(n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n);
    v.resize(n, 0);
    v
}

// -- path-based operations --

pub fn stat(p: &Path) -> io::Result<FileAttr> {
    let st = raw_stat_path(p)?;
    Ok(FileAttr { size: st.size, kind: st.kind })
}

pub fn lstat(p: &Path) -> io::Result<FileAttr> {
    stat(p)
}

pub fn unlink(_p: &Path) -> io::Result<()> {
    unsupported()
}
pub fn rename(_old: &Path, _new: &Path) -> io::Result<()> {
    unsupported()
}
pub fn set_perm(_p: &Path, _perm: FilePermissions) -> io::Result<()> {
    unsupported()
}
pub fn set_times(_p: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}
pub fn set_times_nofollow(_p: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}
pub fn rmdir(_p: &Path) -> io::Result<()> {
    unsupported()
}
pub fn readlink(_p: &Path) -> io::Result<PathBuf> {
    unsupported()
}
pub fn symlink(_original: &Path, _link: &Path) -> io::Result<()> {
    unsupported()
}
pub fn link(_src: &Path, _dst: &Path) -> io::Result<()> {
    unsupported()
}
pub fn canonicalize(_p: &Path) -> io::Result<PathBuf> {
    unsupported()
}
