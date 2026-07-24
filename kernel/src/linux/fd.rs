//! The Linux personality's per-cell file-descriptor table (docs/LINUX-COMPAT.md
//! L2). A Linux process's fds are personality state, not a kernel object: this
//! is a fixed 64-entry table per cell. fds 0/1/2 are the console (serial);
//! opened files forward to the registered `svc::FileOps` VFS handler; the
//! `/dev/{null,zero,urandom}` character devices are synthesized here.
//!
//! Console I/O is **non-blocking** (a hard requirement - the cell must never
//! park): stdin drains whatever the serial RX FIFO holds right now (0 bytes if
//! none), stdout/stderr write straight to the UART.

use crate::arch::{self, linux_abi::Stat};
use crate::linux::dirent;
use crate::linux::errno::*;
use crate::svc;

pub const NFD: usize = 64;
/// Longest path stored per open fd (for `getdents64`/`newfstatat` by fd).
const PATH_MAX: usize = 256;

/// AT_FDCWD: "relative to the current directory". For L2 only absolute paths
/// (and AT_FDCWD with an absolute path) are supported.
pub const AT_FDCWD: i64 = -100;

/// One descriptor slot. The `Vfs` variant carries the file's path so a
/// directory fd can be re-resolved for by-fd `getdents64`/`fstatat`; that
/// makes the enum large, but the kernel is allocation-free (no boxing) and the
/// fd table is a fixed per-cell static, so the size is deliberate.
#[allow(clippy::large_enum_variant)]
#[derive(Copy, Clone)]
enum FdKind {
    Closed,
    /// Serial console: 0 = stdin, 1 = stdout, 2 = stderr.
    Console(u8),
    /// A VFS-backed file. `vfs_fd` is the descriptor `FileOps::open` returned
    /// (passed back verbatim to read/write/lseek/close); `path` is stored for
    /// by-fd `getdents64`/`fstatat`.
    Vfs {
        vfs_fd: i64,
        path: [u8; PATH_MAX],
        path_len: u16,
        /// Bytes of the packed `linux_dirent64` stream already returned by
        /// `getdents64` for this fd, so repeated calls advance and finally
        /// report end-of-directory (0) instead of looping forever.
        dir_off: u16,
    },
    /// `/dev/null` - read EOF, writes discarded.
    Null,
    /// `/dev/zero` - reads zero-fill, writes discarded.
    Zero,
    /// `/dev/urandom` - reads from the cell's DRBG.
    Urandom,
    /// `/proc/self/auxv` - reads the cell's serialized auxv (docs/LINUX-COMPAT.md
    /// L3); `pos` is the read cursor. glibc/rustix read AT_EXECFN etc. from here
    /// when the kernel provides no PR_GET_AUXV, which is how the upstream
    /// coreutils multicall binary learns its own name to dispatch a utility.
    ProcAuxv {
        pos: usize,
    },
    /// One end of an in-process pipe (docs/LINUX-COMPAT.md L3): `idx` selects
    /// the per-cell ring buffer, `writer` picks the end. A single-process,
    /// non-blocking, bounded pipe - enough for tools that create a pipe and
    /// fall back when the `splice` fast path is unavailable (e.g. uu_cat);
    /// cross-context/blocking pipe semantics are L6.
    Pipe {
        idx: u8,
        writer: bool,
    },
}

/// Room for the serialized auxv served through `/proc/self/auxv` (matches
/// `linux::stack::AUXV_BYTES_MAX`).
const AUXV_MAX: usize = 20 * 16;

/// Per-cell in-process pipes: count and per-pipe ring capacity.
const PIPE_COUNT: usize = 4;
const PIPE_CAP: usize = 8192;

/// One bounded ring-buffer pipe. `ends` counts open read+write descriptors; the
/// slot is reclaimed when both ends close.
#[derive(Copy, Clone)]
struct Pipe {
    buf: [u8; PIPE_CAP],
    head: usize,
    len: usize,
    ends: u8,
}

impl Pipe {
    const fn new() -> Pipe {
        Pipe {
            buf: [0; PIPE_CAP],
            head: 0,
            len: 0,
            ends: 0,
        }
    }
}

pub struct FdTable {
    fds: [FdKind; NFD],
    /// The cell's `/proc/self/auxv` bytes, copied in by `install_cell` after
    /// the stack (with its auxv) is built.
    auxv: [u8; AUXV_MAX],
    auxv_len: usize,
    pipes: [Pipe; PIPE_COUNT],
}

impl Default for FdTable {
    fn default() -> FdTable {
        FdTable::new()
    }
}

impl FdTable {
    pub const fn new() -> FdTable {
        FdTable {
            fds: [FdKind::Closed; NFD],
            auxv: [0; AUXV_MAX],
            auxv_len: 0,
            pipes: [Pipe::new(); PIPE_COUNT],
        }
    }

    /// Reset to the initial state: fds 0/1/2 = console, the rest closed.
    pub fn init_console(&mut self) {
        self.fds = [FdKind::Closed; NFD];
        self.fds[0] = FdKind::Console(0);
        self.fds[1] = FdKind::Console(1);
        self.fds[2] = FdKind::Console(2);
    }

    /// Store the cell's serialized auxv for `/proc/self/auxv` reads.
    pub fn set_auxv(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(AUXV_MAX);
        self.auxv[..n].copy_from_slice(&bytes[..n]);
        self.auxv_len = n;
    }

    /// pipe2(pipefd[2], flags): allocate an in-process pipe and write the read
    /// and write fds (two `int`s) into `pipefd`. Non-blocking, bounded
    /// (docs/LINUX-COMPAT.md L3); flags (O_CLOEXEC/O_NONBLOCK) are accepted and
    /// ignored (the ends are always non-blocking, always close-on-run-end).
    pub fn pipe2(&mut self, pipefd_va: u64) -> i64 {
        let Some(idx) = (0..PIPE_COUNT).find(|&i| self.pipes[i].ends == 0) else {
            return -ENFILE;
        };
        let Some(rd) = self.free_slot(3) else {
            return -EMFILE;
        };
        self.fds[rd] = FdKind::Pipe {
            idx: idx as u8,
            writer: false,
        };
        let Some(wr) = self.free_slot(3) else {
            self.fds[rd] = FdKind::Closed;
            return -EMFILE;
        };
        self.fds[wr] = FdKind::Pipe {
            idx: idx as u8,
            writer: true,
        };
        self.pipes[idx] = Pipe::new();
        self.pipes[idx].ends = 2;
        // SAFETY: `pipefd_va` is a writable [i32; 2] in the calling cell.
        unsafe {
            let p = pipefd_va as *mut i32;
            p.write(rd as i32);
            p.add(1).write(wr as i32);
        }
        0
    }

    fn free_slot(&self, from: usize) -> Option<usize> {
        (from..NFD).find(|&i| matches!(self.fds[i], FdKind::Closed))
    }

    /// True if `fd` is one of the console descriptors (used by `ioctl` to
    /// answer TIOCGWINSZ only for the terminal).
    pub fn is_console(&self, fd: i64) -> bool {
        usize_fd(fd).is_some_and(|s| matches!(self.fds[s], FdKind::Console(_)))
    }

    /// True if `fd` refers to an open descriptor (used by `poll` to distinguish
    /// a valid fd from a closed one).
    pub fn is_open(&self, fd: i64) -> bool {
        usize_fd(fd).is_some_and(|s| !matches!(self.fds[s], FdKind::Closed))
    }

    /// read(fd, buf, count).
    pub fn read(&mut self, fd: i64, buf_va: u64, count: u64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        match self.fds[slot] {
            FdKind::Console(0) => {
                // Non-blocking stdin: drain the serial RX FIFO (0 if empty).
                let buf =
                    unsafe { core::slice::from_raw_parts_mut(buf_va as *mut u8, count as usize) };
                let mut n = 0;
                while n < buf.len() {
                    match arch::serial_read_byte() {
                        Some(b) => {
                            buf[n] = b;
                            n += 1;
                        }
                        None => break,
                    }
                }
                n as i64
            }
            FdKind::Console(_) => -EBADF,
            FdKind::Null => 0,
            FdKind::Zero => {
                let buf =
                    unsafe { core::slice::from_raw_parts_mut(buf_va as *mut u8, count as usize) };
                buf.fill(0);
                count as i64
            }
            FdKind::Urandom => {
                let buf =
                    unsafe { core::slice::from_raw_parts_mut(buf_va as *mut u8, count as usize) };
                crate::rng::derive_cell_drbg().fill_bytes(buf);
                count as i64
            }
            FdKind::ProcAuxv { pos } => {
                let end = self.auxv_len;
                let n = (end - pos.min(end)).min(count as usize);
                let buf = unsafe { core::slice::from_raw_parts_mut(buf_va as *mut u8, n) };
                buf.copy_from_slice(&self.auxv[pos..pos + n]);
                self.fds[slot] = FdKind::ProcAuxv { pos: pos + n };
                n as i64
            }
            FdKind::Pipe { idx, writer: false } => {
                let p = &mut self.pipes[idx as usize];
                let n = p.len.min(count as usize);
                let buf = unsafe { core::slice::from_raw_parts_mut(buf_va as *mut u8, n) };
                for b in buf.iter_mut() {
                    *b = p.buf[p.head];
                    p.head = (p.head + 1) % PIPE_CAP;
                }
                p.len -= n;
                n as i64
            }
            FdKind::Pipe { writer: true, .. } => -EBADF, // write end not readable
            FdKind::Vfs { vfs_fd, .. } => match svc::file_ops() {
                Some(o) => (o.read)(vfs_fd as u64, buf_va, count),
                None => -EBADF,
            },
            FdKind::Closed => -EBADF,
        }
    }

    /// write(fd, buf, count).
    pub fn write(&mut self, fd: i64, buf_va: u64, count: u64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        match self.fds[slot] {
            FdKind::Console(0) => -EBADF,
            FdKind::Console(_) => {
                let buf =
                    unsafe { core::slice::from_raw_parts(buf_va as *const u8, count as usize) };
                for &b in buf {
                    arch::serial_write_byte(b);
                }
                super::tap_stdout(buf);
                count as i64
            }
            FdKind::Null | FdKind::Zero | FdKind::Urandom => count as i64,
            FdKind::ProcAuxv { .. } => -EBADF, // read-only
            FdKind::Pipe { writer: false, .. } => -EBADF, // read end not writable
            FdKind::Pipe { idx, writer: true } => {
                let p = &mut self.pipes[idx as usize];
                let free = PIPE_CAP - p.len;
                if free == 0 {
                    return -EAGAIN; // non-blocking, buffer full
                }
                let n = free.min(count as usize);
                let buf = unsafe { core::slice::from_raw_parts(buf_va as *const u8, n) };
                for &b in buf {
                    let tail = (p.head + p.len) % PIPE_CAP;
                    p.buf[tail] = b;
                    p.len += 1;
                }
                n as i64
            }
            FdKind::Vfs { vfs_fd, .. } => match svc::file_ops() {
                Some(o) => (o.write)(vfs_fd as u64, buf_va, count),
                None => -EBADF,
            },
            FdKind::Closed => -EBADF,
        }
    }

    /// openat(dirfd, path, flags, mode). L2: absolute paths (or AT_FDCWD with
    /// an absolute path) only. `/dev/{null,zero,urandom,random}` are
    /// synthesized; everything else forwards to `FileOps::open`.
    pub fn openat(&mut self, dirfd: i64, path_va: u64, path_len: usize, flags: u64) -> i64 {
        if dirfd != AT_FDCWD && dirfd >= 0 {
            // Relative-to-a-dirfd resolution is L3 (docs/LINUX-COMPAT.md).
            return -ENOSYS;
        }
        let bytes = unsafe { core::slice::from_raw_parts(path_va as *const u8, path_len) };
        let name = &bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())];

        let Some(slot) = self.free_slot(3) else {
            return -EMFILE;
        };
        let dev = match name {
            b"/dev/null" => Some(FdKind::Null),
            b"/dev/zero" => Some(FdKind::Zero),
            b"/dev/urandom" | b"/dev/random" => Some(FdKind::Urandom),
            b"/proc/self/auxv" => Some(FdKind::ProcAuxv { pos: 0 }),
            _ => None,
        };
        if let Some(kind) = dev {
            self.fds[slot] = kind;
            return slot as i64;
        }
        let Some(o) = svc::file_ops() else {
            return -ENOENT;
        };
        let vfs_fd = (o.open)(path_va, name.len() as u64, flags);
        if vfs_fd < 0 {
            return vfs_fd;
        }
        let mut path = [0u8; PATH_MAX];
        let plen = name.len().min(PATH_MAX);
        path[..plen].copy_from_slice(&name[..plen]);
        self.fds[slot] = FdKind::Vfs {
            vfs_fd,
            path,
            path_len: plen as u16,
            dir_off: 0,
        };
        slot as i64
    }

    /// close(fd).
    pub fn close(&mut self, fd: i64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        match self.fds[slot] {
            FdKind::Closed => return -EBADF,
            FdKind::Vfs { vfs_fd, .. } => {
                if let Some(o) = svc::file_ops() {
                    (o.close)(vfs_fd as u64);
                }
            }
            FdKind::Pipe { idx, .. } => {
                let p = &mut self.pipes[idx as usize];
                p.ends = p.ends.saturating_sub(1); // reclaim the slot when both ends close
            }
            _ => {}
        }
        self.fds[slot] = FdKind::Closed;
        0
    }

    /// dup(oldfd) - lowest free slot. Vfs entries share the underlying VFS fd
    /// (close-once semantics; acceptable for the L2 fixtures).
    pub fn dup(&mut self, oldfd: i64) -> i64 {
        let Some(old) = usize_fd(oldfd) else {
            return -EBADF;
        };
        if matches!(self.fds[old], FdKind::Closed) {
            return -EBADF;
        }
        let Some(slot) = self.free_slot(0) else {
            return -EMFILE;
        };
        self.fds[slot] = self.fds[old];
        slot as i64
    }

    /// dup3(oldfd, newfd, flags) / dup2 semantics: place `oldfd` at `newfd`.
    pub fn dup3(&mut self, oldfd: i64, newfd: i64) -> i64 {
        let (Some(old), Some(new)) = (usize_fd(oldfd), usize_fd(newfd)) else {
            return -EBADF;
        };
        if matches!(self.fds[old], FdKind::Closed) {
            return -EBADF;
        }
        if old != new {
            self.close(newfd);
            self.fds[new] = self.fds[old];
        }
        new as i64
    }

    /// fcntl(fd, cmd, arg) - the minimal subset glibc needs.
    pub fn fcntl(&mut self, fd: i64, cmd: u64, arg: u64) -> i64 {
        const F_DUPFD: u64 = 0;
        const F_GETFD: u64 = 1;
        const F_SETFD: u64 = 2;
        const F_GETFL: u64 = 3;
        const F_SETFL: u64 = 4;
        const F_DUPFD_CLOEXEC: u64 = 1030;
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        if matches!(self.fds[slot], FdKind::Closed) {
            return -EBADF;
        }
        match cmd {
            F_DUPFD | F_DUPFD_CLOEXEC => {
                let start = arg as usize;
                let Some(dst) = self.free_slot(start.min(NFD)) else {
                    return -EMFILE;
                };
                self.fds[dst] = self.fds[slot];
                dst as i64
            }
            F_GETFD | F_SETFD | F_SETFL => 0,
            F_GETFL => 2, // O_RDWR - the personality does not track open flags
            _ => 0,
        }
    }

    /// lseek(fd, off, whence).
    pub fn lseek(&mut self, fd: i64, off: i64, whence: u64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        match self.fds[slot] {
            FdKind::Vfs { vfs_fd, .. } => match svc::file_ops() {
                Some(o) => (o.lseek)(vfs_fd as u64, off, whence),
                None => -EBADF,
            },
            FdKind::Console(_) | FdKind::Null | FdKind::Zero | FdKind::Urandom => -ESPIPE,
            FdKind::ProcAuxv { .. } | FdKind::Pipe { .. } => -ESPIPE,
            FdKind::Closed => -EBADF,
        }
    }

    /// fstat(fd, statbuf) - synthesize a `struct stat` for the entry.
    pub fn fstat(&mut self, fd: i64, statbuf_va: u64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        let st = match self.fds[slot] {
            FdKind::Closed => return -EBADF,
            FdKind::Console(_) | FdKind::Null | FdKind::Zero | FdKind::Urandom => {
                // A character device: mode S_IFCHR|0620, zero size.
                Stat::new(dirent::S_IFCHR | 0o620, 0, 1, 1, 1000, 1000, 0, 4096, 0, 0)
            }
            FdKind::ProcAuxv { .. } => {
                // A read-only regular file sized to the auxv byte stream.
                let size = self.auxv_len as u64;
                Stat::new(
                    dirent::S_IFREG | 0o444,
                    size,
                    1,
                    1,
                    1000,
                    1000,
                    0,
                    4096,
                    size.div_ceil(512),
                    0,
                )
            }
            FdKind::Pipe { .. } => {
                Stat::new(dirent::S_IFIFO | 0o600, 0, 1, 1, 1000, 1000, 0, 4096, 0, 0)
            }
            FdKind::Vfs { vfs_fd, .. } => {
                let Some(o) = svc::file_ops() else {
                    return -EBADF;
                };
                // FileOps writes the native abi::Stat into a kernel temp
                // (identity-mapped, writable there); convert to the Linux ABI.
                let mut native = crate::abi::Stat { size: 0, kind: 0 };
                let r = (o.fstat)(vfs_fd as u64, &mut native as *mut _ as u64);
                if r < 0 {
                    return r;
                }
                let mode = dirent::mode_for_kind(native.kind);
                let blocks = native.size.div_ceil(512);
                Stat::new(mode, native.size, 1, 1, 1000, 1000, 0, 4096, blocks, 0)
            }
        };
        // SAFETY: `statbuf_va` is a writable VA in the calling cell.
        unsafe { (statbuf_va as *mut Stat).write(st) };
        0
    }

    /// The `(st_mode, size)` a `fstat`/`statx` would report for `fd`, without
    /// writing a `struct stat`. Used by `statx` (docs/LINUX-COMPAT.md L3), which
    /// has its own ABI-independent buffer layout.
    pub fn mode_size(&mut self, fd: i64) -> Result<(u32, u64), i64> {
        let Some(slot) = usize_fd(fd) else {
            return Err(-EBADF);
        };
        match self.fds[slot] {
            FdKind::Closed => Err(-EBADF),
            FdKind::Console(_) | FdKind::Null | FdKind::Zero | FdKind::Urandom => {
                Ok((dirent::S_IFCHR | 0o620, 0))
            }
            FdKind::ProcAuxv { .. } => Ok((dirent::S_IFREG | 0o444, self.auxv_len as u64)),
            FdKind::Pipe { .. } => Ok((dirent::S_IFIFO | 0o600, 0)),
            FdKind::Vfs { vfs_fd, .. } => {
                let Some(o) = svc::file_ops() else {
                    return Err(-EBADF);
                };
                let mut native = crate::abi::Stat { size: 0, kind: 0 };
                let r = (o.fstat)(vfs_fd as u64, &mut native as *mut _ as u64);
                if r < 0 {
                    return Err(r);
                }
                Ok((dirent::mode_for_kind(native.kind), native.size))
            }
        }
    }

    /// getdents64(fd, buf, len) for a VFS-backed directory fd. The full
    /// directory is packed as a `linux_dirent64` stream and paged out across
    /// calls via the fd's `dir_off`, so a reader looping until it gets 0 (real
    /// `ls`) terminates. The directory must fit `PACKED_MAX` bytes of records
    /// (ample for L3 test roots); records are always returned whole.
    pub fn getdents64(&mut self, fd: i64, buf_va: u64, len: u64) -> i64 {
        const PACKED_MAX: usize = 4096;
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        let (path, path_len, dir_off) = match &self.fds[slot] {
            FdKind::Vfs {
                path,
                path_len,
                dir_off,
                ..
            } => (*path, *path_len as usize, *dir_off as usize),
            FdKind::Closed => return -EBADF,
            _ => return -ENOTDIR,
        };
        let Some(o) = svc::file_ops() else {
            return -EBADF;
        };
        // FileOps.getdents packs [u32 kind][u32 name_len][name] records into a
        // kernel scratch buffer; repack the whole thing as a linux_dirent64
        // stream, then hand out the slice starting at dir_off.
        let mut scratch = [0u8; PACKED_MAX];
        let n = (o.getdents)(
            path.as_ptr() as u64,
            path_len as u64,
            scratch.as_mut_ptr() as u64,
            scratch.len() as u64,
        );
        if n < 0 {
            return n;
        }
        let src = &scratch[..n as usize];
        let mut packed = [0u8; PACKED_MAX];
        let mut si = 0usize;
        let mut pi = 0usize;
        let mut ino = 2u64;
        while si + 8 <= src.len() {
            let kind = u32::from_ne_bytes(src[si..si + 4].try_into().unwrap()) as u64;
            let nlen = u32::from_ne_bytes(src[si + 4..si + 8].try_into().unwrap()) as usize;
            si += 8;
            if si + nlen > src.len() {
                break;
            }
            let name = &src[si..si + nlen];
            si += nlen;
            match dirent::pack(&mut packed, pi, ino, kind, name) {
                Some(next) => pi = next,
                None => break,
            }
            ino += 1;
        }
        let total = pi;
        if dir_off >= total {
            return 0; // end of directory
        }
        // Copy whole records from dir_off while they fit the caller's buffer.
        let out = unsafe { core::slice::from_raw_parts_mut(buf_va as *mut u8, len as usize) };
        let mut cur = dir_off;
        let mut oi = 0usize;
        while cur < total {
            let reclen =
                u16::from_ne_bytes(packed[cur + 16..cur + 18].try_into().unwrap()) as usize;
            if oi + reclen > out.len() {
                break;
            }
            out[oi..oi + reclen].copy_from_slice(&packed[cur..cur + reclen]);
            oi += reclen;
            cur += reclen;
        }
        if oi == 0 {
            return -EINVAL; // caller's buffer too small for even one record
        }
        if let FdKind::Vfs { dir_off, .. } = &mut self.fds[slot] {
            *dir_off = cur as u16;
        }
        oi as i64
    }
}

/// Validate a raw fd and narrow it to a table slot.
fn usize_fd(fd: i64) -> Option<usize> {
    if (0..NFD as i64).contains(&fd) {
        Some(fd as usize)
    } else {
        None
    }
}
