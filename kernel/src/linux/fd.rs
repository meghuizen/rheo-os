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
use crate::linux::epoll;
use crate::linux::errno::*;
use crate::linux::eventfd;
use crate::linux::inetsock::{self, AF_INET, AF_INET6};
use crate::linux::pipe;
use crate::linux::timerfd;
use crate::linux::unixsock::{self, AF_UNIX, NAME_MAX, SOCK_DGRAM, SOCK_STREAM, SOCK_TYPE_MASK};
use crate::svc;

pub const NFD: usize = 64;

/// `S_IFSOCK` (a socket file type), for a socket fd's `fstat` (docs/LINUX-COMPAT.md
/// L8). Not in `dirent` (no VFS socket nodes); local to the fd table.
const S_IFSOCK: u32 = 0o140000;
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
    /// `/proc/self/maps` - the cell's own mapping table, rendered on open from its
    /// VMA list (docs/LINUX-COMPAT.md).
    ///
    /// A **real** map, not a plausible one: every line comes from a record the
    /// personality actually holds, so the addresses, lengths and permissions are the
    /// ones the page tables were built from. That distinction is the whole reason this
    /// is synthesized in the kernel rather than seeded as a file on the test image -
    /// a static `maps` would be a fabricated memory layout, and a runtime that reads
    /// it to locate its own code would be misled rather than refused
    /// (docs/ENGINEERING.md 1).
    ///
    /// Rendered once at open, into the same per-cell buffer `/proc/self/auxv` uses a
    /// sibling of, because a reader expects a consistent snapshot: generating each
    /// `read` afresh from a list the program is concurrently changing would hand back
    /// a map that never existed at any instant.
    ProcMaps {
        pos: usize,
    },
    ProcAuxv {
        pos: usize,
    },
    /// One end of a **cross-cell** pipe (docs/LINUX-COMPAT.md L6): `idx` selects
    /// the global ring buffer (`linux::pipe`), `writer` picks the end. After
    /// `fork` the two ends live in different cells; blocking read/write with
    /// cross-cell wake is handled by the process scheduler.
    Pipe {
        idx: u8,
        writer: bool,
    },
    /// An AF_UNIX socket created by `socket()`, not yet bound or connected
    /// (docs/LINUX-COMPAT.md L8). SOCK_STREAM only (SOCK_DGRAM is a documented
    /// deferral - datagram boundary preservation is not implemented).
    SockFresh,
    /// A bound + listening AF_UNIX socket: `lst` indexes the global listener
    /// registry (`linux::unixsock`) holding its name + accept backlog.
    SockListen {
        lst: u8,
    },
    /// A connected AF_UNIX socket (from `socketpair`, `connect`, or `accept`):
    /// reads ring `rx`, writes ring `tx` (both `linux::pipe` indices). Backed by
    /// the L6 cross-cell ring machinery - one connection is two rings.
    SockConn {
        rx: u8,
        tx: u8,
    },
    /// An AF_INET/AF_INET6 **stream** socket created by `socket()`, not yet
    /// listening or connected (docs/LINUX-COMPAT.md L8-INET). `local_port` is 0
    /// until `bind`; loopback-only.
    InetStreamFresh {
        v6: bool,
        local_port: u16,
    },
    /// A bound + listening AF_INET/AF_INET6 stream socket: `lst` indexes the INET
    /// listener registry (`linux::inetsock`).
    InetListen {
        lst: u8,
        v6: bool,
        port: u16,
    },
    /// A connected AF_INET/AF_INET6 stream socket (from `connect`/`accept`): the
    /// transport is the same L6 ring pair as AF_UNIX (`rx`/`tx`); the ports are
    /// kept for `getsockname`/`getpeername`.
    InetConn {
        rx: u8,
        tx: u8,
        local_port: u16,
        peer_port: u16,
        v6: bool,
    },
    /// An AF_INET/AF_INET6 **datagram** (UDP) socket. `ep` indexes the INET
    /// datagram registry once `bound` (an explicit `bind`, or an implicit
    /// ephemeral bind on the first `sendto`); `peer_port` is a `connect`-set
    /// default destination.
    InetDgram {
        v6: bool,
        ep: u8,
        bound: bool,
        peer_port: u16,
    },
    /// A UDP socket on the **remote** (NIC-backed) datapath (rheo-net N4b,
    /// docs/LINUX-COMPAT.md L8-INET remote): `ep` is the opaque handle the
    /// registered `svc::SocketOps` bridge returned, `port` the local port the
    /// personality allocated, and `peer_ip`/`peer_port` a `connect`-set default
    /// destination. A UDP socket becomes remote the first time it names a
    /// **non-loopback** address; a loopback one keeps `InetDgram` unchanged.
    InetUdpRemote {
        ep: u8,
        port: u16,
        peer_ip: [u8; 4],
        peer_port: u16,
    },
    /// A connected TCP socket on the **remote** datapath (rheo-net N4b): `h` is the
    /// `svc::SocketOps` connection handle; `read`/`write` forward to it. Loopback
    /// TCP keeps `InetConn` (the L6 ring pair) unchanged.
    InetTcpRemote {
        h: u8,
        local_port: u16,
        peer_ip: [u8; 4],
        peer_port: u16,
    },
    /// An epoll instance (docs/LINUX-COMPAT.md L8-INET): `ep` indexes the
    /// per-personality epoll registry (`linux::epoll`).
    Epoll {
        ep: u8,
    },
    /// An `eventfd2` counter (docs/LINUX-COMPAT.md L8-EVENTFD): `ev` indexes the
    /// per-personality registry (`linux::eventfd`). The counter deliberately lives
    /// in the registry, not here - `dup`/`fork` make a second descriptor for the
    /// *same* object, and a counter copied per descriptor would give two counters
    /// that silently stop waking each other.
    EventFd {
        ev: u8,
    },
    /// A `timerfd` (docs/LINUX-COMPAT.md L8-TIMERFD): `tf` indexes the
    /// per-personality registry (`linux::timerfd`). The armed deadline lives in the
    /// registry for the same reason an eventfd's counter does - `dup`/`fork` alias
    /// one object, and a per-descriptor deadline would give two that disagree.
    TimerFd {
        tf: u8,
    },
}

/// How long a **remote** (NIC-backed) blocking receive waits for a frame before
/// reporting `EAGAIN` (rheo-net N4b). The personality tracks no `O_NONBLOCK` or
/// `SO_RCVTIMEO`, so one documented bound serves every remote receive: long enough
/// that a real reply from the network always lands, short enough that a lost packet
/// cannot wedge a cell. The wait itself is a genuine park (`net_rx::wait_frame`),
/// not a spin, on the ISAs where the NIC RX interrupt is wired.
const REMOTE_RECV_TIMEOUT_NS: u64 = 2_000_000_000;

/// The handshake budget for a **remote** TCP `connect` (rheo-net N4b): the SYN is
/// retransmitted inside this window by the transport's own RTO, and the call
/// reports `ETIMEDOUT` at the deadline.
const REMOTE_CONNECT_TIMEOUT_NS: u64 = 3_000_000_000;

/// Room for the rendered `/proc/self/maps` snapshot.
///
/// A line is ~60 bytes and a dynamically linked runtime has a few dozen mappings, so
/// 8 KiB holds well over a hundred. A cell with more is **truncated at a line
/// boundary** and the fact is printed, rather than the buffer being grown to a size
/// nothing has needed: a half-line would make the last entry a lie, whereas a short
/// map is honestly short.
const MAPS_MAX: usize = 8192;

/// Room for the serialized auxv served through `/proc/self/auxv` (matches
/// `linux::stack::AUXV_BYTES_MAX`).
const AUXV_MAX: usize = 20 * 16;

// ---------------------------------------------------------------- open flags
// The three ISAs agree on every bit used here (x86-64's `asm/fcntl.h` and the
// asm-generic one define the same values), so these are portable constants and
// not `arch::linux_abi` entries.

/// `O_ACCMODE`: the access-mode field of a descriptor's status flags.
const O_ACCMODE: u64 = 0o3;
/// `O_APPEND` - not honoured by this personality (see [`FdTable::fcntl`]).
const O_APPEND: u64 = 0o2000;
/// `O_NONBLOCK` (== `SOCK_NONBLOCK`): honoured when set through
/// `fcntl(F_SETFL)`.
const O_NONBLOCK: u64 = 0o4000;
/// `O_ASYNC`/`FASYNC` - not honoured (no SIGIO is ever delivered).
const O_ASYNC: u64 = 0o20000;
/// `O_CLOEXEC` (== `SOCK_CLOEXEC` == `EPOLL_CLOEXEC`).
pub const O_CLOEXEC: u64 = 0o2000000;
/// `FD_CLOEXEC`, the single `F_GETFD`/`F_SETFD` bit.
const FD_CLOEXEC: u64 = 1;

/// The per-descriptor flags the personality **tracks and honours**
/// (docs/LINUX-COMPAT.md, the `fcntl` row). Kept in a table parallel to `fds`
/// rather than inside each [`FdKind`] variant, so every kind gets them without
/// widening an already-large enum.
#[derive(Copy, Clone)]
struct FdFlags {
    /// `FD_CLOEXEC`: this descriptor is closed by `execve`.
    cloexec: bool,
    /// `O_NONBLOCK`: a read/write that would block reports `-EAGAIN` instead.
    nonblock: bool,
    /// The access mode (`O_RDONLY`/`O_WRONLY`/`O_RDWR`), so `F_GETFL` reports
    /// what the descriptor was actually opened with rather than a constant.
    accmode: u8,
}

impl FdFlags {
    const fn new(accmode: u8) -> FdFlags {
        FdFlags {
            cloexec: false,
            nonblock: false,
            accmode,
        }
    }
}

/// `O_RDWR`, the default access mode for a descriptor that is not opened from a
/// path (pipes get per-end modes; sockets are read-write).
const ACC_RDWR: u8 = 2;

#[derive(Copy, Clone)]
pub struct FdTable {
    fds: [FdKind; NFD],
    /// Per-descriptor flags, indexed exactly like `fds`.
    flags: [FdFlags; NFD],
    /// The cell's `/proc/self/auxv` bytes, copied in by `install_cell` after
    /// the stack (with its auxv) is built.
    auxv: [u8; AUXV_MAX],
    auxv_len: usize,
    /// Length of the rendered `/proc/self/maps` snapshot; the bytes are in
    /// [`CELL_MAPS`], funded per cell (docs/EXECUTION-MODEL.md 9.8).
    maps_len: usize,
}

/// One frame of a `/proc/self/maps` snapshot. A frame exactly, so `Funded` holds one
/// element per frame - the shape the pipe ring takes, for the same reason.
#[derive(Copy, Clone)]
#[repr(C, align(8))]
struct MapsPage([u8; MAPS_PAGE]);

const MAPS_PAGE: usize = crate::mm::frames::FRAME_SIZE;
/// Frames a full snapshot occupies.
const MAPS_PAGES: usize = MAPS_MAX.div_ceil(MAPS_PAGE);

/// The rendered `/proc/self/maps` bytes, **funded per cell**.
///
/// This was `[u8; 8192]` inline in every `FdTable` - 131 KiB across the table, resident
/// in every cell whether or not it ever reads its own memory map. Almost none do: it is
/// JavaScriptCore's probe, which is the only reason the file is synthesized at all
/// (docs/LINUX-COMPAT.md). A cell that never asks now pays nothing.
///
/// Its own static rather than a field of `FdTable`, and the compiler settled that:
/// `FdTable` is `Copy`, a `Funded` descriptor must never be raw-copied, and putting one
/// inside simply stops compiling. That is the S1' scar enforced by the type system rather
/// than by review - the same reason `user::CELL_VCORES` sits beside `RunCell`.
static mut CELL_MAPS: [crate::mm::kmeta::Funded<MapsPage>; crate::user::MAX_CELLS] =
    [const { crate::mm::kmeta::Funded::new() }; crate::user::MAX_CELLS];

/// The running cell's snapshot storage.
///
/// Keyed on the **running** cell rather than an argument, for the reason `pipe::alloc`
/// is: every path here services a syscall for that cell, so it is the owner by
/// construction, and threading an index through `FdTable`'s methods would be a chance to
/// pass the wrong one.
fn cell_maps() -> &'static mut crate::mm::kmeta::Funded<MapsPage> {
    let idx = crate::user::current_index().min(crate::user::MAX_CELLS - 1);
    // SAFETY: single CPU per cell; a cell belongs to one core.
    unsafe { &mut (*core::ptr::addr_of_mut!(CELL_MAPS))[idx] }
}

/// Frames every cell's `/proc/self/maps` snapshot holds. **0 once released** - the
/// property a slot-handback path that is not a release path breaks.
pub fn maps_frames() -> usize {
    (0..crate::user::MAX_CELLS)
        // SAFETY: a read.
        .map(|i| unsafe { (*core::ptr::addr_of!(CELL_MAPS))[i].frames_held() })
        .sum()
}

/// Release every cell's snapshot storage (called from `linux::reset`).
pub fn reset_maps() {
    for i in 0..crate::user::MAX_CELLS {
        // SAFETY: between runs.
        unsafe { (*core::ptr::addr_of_mut!(CELL_MAPS))[i].release() };
    }
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
            flags: [FdFlags::new(ACC_RDWR); NFD],
            auxv: [0; AUXV_MAX],
            auxv_len: 0,
            maps_len: 0,
        }
    }

    /// Reset to the initial state: fds 0/1/2 = console, the rest closed.
    pub fn init_console(&mut self) {
        self.fds = [FdKind::Closed; NFD];
        self.flags = [FdFlags::new(ACC_RDWR); NFD];
        self.fds[0] = FdKind::Console(0);
        self.flags[0] = FdFlags::new(0); // O_RDONLY
        self.fds[1] = FdKind::Console(1);
        self.flags[1] = FdFlags::new(1); // O_WRONLY
        self.fds[2] = FdKind::Console(2);
        self.flags[2] = FdFlags::new(1); // O_WRONLY
    }

    /// True if `fd` has `O_NONBLOCK` set (through `fcntl(F_SETFL)`). The
    /// cooperative blocking paths in `linux::mod`/`linux::proc` consult this
    /// before parking a cell, so a non-blocking descriptor reports `-EAGAIN`
    /// instead (docs/LINUX-COMPAT.md, the `fcntl` row).
    pub fn is_nonblock(&self, fd: i64) -> bool {
        usize_fd(fd).is_some_and(|s| self.flags[s].nonblock)
    }

    /// Close every descriptor marked `FD_CLOEXEC` - the `execve` step
    /// (docs/LINUX-COMPAT.md L6). Returns how many were closed, so a test can
    /// observe that it did something rather than infer it.
    pub fn close_cloexec(&mut self) -> usize {
        let mut n = 0;
        for i in 0..NFD {
            if self.flags[i].cloexec && !matches!(self.fds[i], FdKind::Closed) {
                self.close(i as i64);
                n += 1;
            }
        }
        n
    }

    /// Open a `/proc/self/maps` descriptor over `snapshot`, which the caller renders
    /// from the cell's VMA list.
    ///
    /// The rendering happens in the caller, not here, because the VMA list is a sibling
    /// field of this table inside `LinuxState` and only the dispatcher holds both. That
    /// is also why the snapshot is taken at **open**: a reader expects one consistent
    /// map, and regenerating it per `read` from a list the program is concurrently
    /// changing would hand back a layout that never existed at any instant.
    ///
    /// Returns the fd, or `-EMFILE` when the table is full. `truncated` is reported by
    /// the caller.
    pub fn open_maps(&mut self, snapshot: &[u8], flags: u64) -> i64 {
        let Some(slot) = self.free_slot(0) else {
            return -EMFILE;
        };
        let n = snapshot.len().min(MAPS_MAX);
        // Fund the frames on first use. A cell that cannot afford them cannot read its
        // own map, and that is a refusal rather than a short answer: a truncated
        // `/proc/self/maps` is a fabricated memory layout, which is the one thing this
        // file must never be (docs/LINUX-COMPAT.md).
        let store = cell_maps();
        store.set_owner(crate::mm::kmeta::Owner::cell(crate::user::current_index()));
        if !store.reserve(MAPS_PAGES) {
            return -EMFILE;
        }
        for (i, chunk) in snapshot[..n].chunks(MAPS_PAGE).enumerate() {
            if let Some(pg) = store.get_mut(i) {
                pg.0[..chunk.len()].copy_from_slice(chunk);
            }
        }
        self.maps_len = n;
        self.fds[slot] = FdKind::ProcMaps { pos: 0 };
        self.set_open_flags(slot, flags);
        slot as i64
    }

    /// Store the cell's serialized auxv for `/proc/self/auxv` reads.
    pub fn set_auxv(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(AUXV_MAX);
        self.auxv[..n].copy_from_slice(&bytes[..n]);
        self.auxv_len = n;
    }

    /// pipe2(pipefd[2], flags): allocate a global cross-cell pipe and write the
    /// read and write fds (two `int`s) into `pipefd` (docs/LINUX-COMPAT.md L6).
    /// `O_CLOEXEC` and `O_NONBLOCK` in `flags` are both **honoured** on both ends
    /// (docs/ARCHITECTURE-DEBT.md 2.4 - creation-time non-blocking used to be a named
    /// deferral, blocked on `poll` computing real readiness).
    pub fn pipe2(&mut self, pipefd_va: u64, flags: u64) -> i64 {
        let Some(idx) = pipe::alloc() else {
            return -ENFILE;
        };
        let Some(rd) = self.free_slot(3) else {
            pipe::close_end(idx, false);
            pipe::close_end(idx, true);
            return -EMFILE;
        };
        self.fds[rd] = FdKind::Pipe {
            idx: idx as u8,
            writer: false,
        };
        self.flags[rd] = FdFlags::new(0); // read end: O_RDONLY
        let Some(wr) = self.free_slot(3) else {
            self.fds[rd] = FdKind::Closed;
            pipe::close_end(idx, false);
            pipe::close_end(idx, true);
            return -EMFILE;
        };
        self.fds[wr] = FdKind::Pipe {
            idx: idx as u8,
            writer: true,
        };
        self.flags[wr] = FdFlags::new(1); // write end: O_WRONLY
        if flags & O_CLOEXEC != 0 {
            self.flags[rd].cloexec = true;
            self.flags[wr].cloexec = true;
        }
        // Creation-time O_NONBLOCK, honoured on both ends (see `set_open_flags`).
        if flags & O_NONBLOCK != 0 {
            self.flags[rd].nonblock = true;
            self.flags[wr].nonblock = true;
        }
        // Through `uaccess`: bounded, present, and copy-on-write resolved before the
        // store (a fresh pipe's fd pair often lands on a stack shared by a fork).
        if !crate::uaccess::write::<i32>(pipefd_va, rd as i32)
            || !crate::uaccess::write::<i32>(pipefd_va + 4, wr as i32)
        {
            return -EFAULT;
        }
        0
    }

    /// If `fd` is a pipe end, its `(global pipe idx, writer)`. The process
    /// scheduler uses this to route blocking read/write (docs/LINUX-COMPAT.md L6).
    pub fn pipe_end(&self, fd: i64) -> Option<(usize, bool)> {
        let slot = usize_fd(fd)?;
        match self.fds[slot] {
            FdKind::Pipe { idx, writer } => Some((idx as usize, writer)),
            _ => None,
        }
    }

    /// Bump the global pipe end-refcount for every pipe fd in this table - the
    /// `fork` inheritance step, after the child's table is copied from the
    /// parent (docs/LINUX-COMPAT.md L6): both processes now hold the end.
    pub fn inherit_pipe_ends(&self) {
        for f in self.fds.iter() {
            match *f {
                FdKind::Pipe { idx, writer } => pipe::add_end(idx as usize, writer),
                // A connected socket is two ring ends (rx = reader, tx = writer).
                FdKind::SockConn { rx, tx } | FdKind::InetConn { rx, tx, .. } => {
                    pipe::add_end(rx as usize, false);
                    pipe::add_end(tx as usize, true);
                }
                FdKind::SockListen { lst } => unixsock::addref(lst),
                FdKind::InetListen { lst, .. } => inetsock::addref_listener(lst),
                FdKind::InetDgram {
                    ep, bound: true, ..
                } => inetsock::addref_dgram(ep),
                FdKind::Epoll { ep } => epoll::addref(ep),
                FdKind::EventFd { ev } => eventfd::addref(ev),
                FdKind::TimerFd { tf } => timerfd::addref(tf),
                _ => {}
            }
        }
    }

    /// Close every open descriptor - the process-exit teardown
    /// (docs/LINUX-COMPAT.md L6): drops pipe ends (firing EOF/EPIPE for peers)
    /// and closes VFS files.
    pub fn close_all(&mut self) {
        for i in 0..NFD {
            if !matches!(self.fds[i], FdKind::Closed) {
                self.close(i as i64);
            }
        }
    }

    fn free_slot(&self, from: usize) -> Option<usize> {
        (from..NFD).find(|&i| matches!(self.fds[i], FdKind::Closed))
    }

    /// True if `fd` is one of the console descriptors (used by `ioctl` to
    /// answer TIOCGWINSZ only for the terminal).
    pub fn is_console(&self, fd: i64) -> bool {
        usize_fd(fd).is_some_and(|s| matches!(self.fds[s], FdKind::Console(_)))
    }

    /// True if `fd` is the **console input** descriptor - the one read that can
    /// block on console input (docs/ARCHITECTURE-DEBT.md 2.4).
    pub fn is_console_in(&self, fd: i64) -> bool {
        usize_fd(fd).is_some_and(|s| matches!(self.fds[s], FdKind::Console(0)))
    }

    /// True if `fd` refers to an open descriptor (used by `poll` to distinguish
    /// a valid fd from a closed one).
    pub fn is_open(&self, fd: i64) -> bool {
        usize_fd(fd).is_some_and(|s| !matches!(self.fds[s], FdKind::Closed))
    }

    /// read(fd, buf, count).
    ///
    /// `buf_va` is bound to the calling cell's user VA range here rather than
    /// only at the syscall entry, because `readv`/`writev` reach this with a
    /// **per-iovec** base the entry check never saw (docs/ENGINEERING.md 12).
    pub fn read(&mut self, fd: i64, buf_va: u64, count: u64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        if crate::user::user_buf_mut(buf_va, count as usize).is_none() {
            return -EFAULT;
        }
        match self.fds[slot] {
            // stdin. Bytes come from the kernel's console **RX ring**
            // (`crate::input`), which is the same buffer the UART RX interrupt fills
            // and the same one the scheduler drains when a parked reader wakes - so a
            // blocking and a non-blocking read cannot disagree about what has
            // arrived. `sys_read` decides *whether* to park before reaching here
            // (docs/ARCHITECTURE-DEBT.md 2.4); this is the non-blocking drain.
            //
            // Before that, this read went straight to the UART FIFO and answered 0 on
            // an empty console - "end of input", which was a lie to every reader.
            FdKind::Console(0) => {
                // SAFETY: `[buf_va, buf_va+count)` was range-checked above and the
                // calling cell's address space is active.
                let n = unsafe { crate::input::drain(buf_va, count as usize) };
                if n == 0 && count > 0 {
                    if self.flags[slot].nonblock {
                        return -EAGAIN;
                    }
                    // Reached only when the caller was allowed to see end of input
                    // (`sys_read` parks otherwise).
                    return 0;
                }
                n as i64
            }
            FdKind::Console(_) => -EBADF,
            FdKind::Null => 0,
            FdKind::Zero => {
                if !crate::uaccess::fill(buf_va, 0, count as usize) {
                    return -EFAULT;
                }
                count as i64
            }
            FdKind::Urandom => {
                // SAFETY: `uaccess::slice` bounds, faults in and un-shares the range;
                // we are servicing this cell's synchronous trap.
                let Some(buf) = (unsafe { crate::uaccess::slice(buf_va, count as usize) }) else {
                    return -EFAULT;
                };
                crate::rng::derive_cell_drbg().fill_bytes(buf);
                count as i64
            }
            FdKind::ProcMaps { pos } => {
                let end = self.maps_len;
                let n = (end - pos.min(end)).min(count as usize);
                // Copied page by page: the snapshot is frames now, so no single slice
                // spans it. Bounded by `n`, already clamped to what was stored.
                let store = cell_maps();
                let mut done = 0usize;
                while done < n {
                    let at = pos + done;
                    let Some(pg) = store.get_ref(at / MAPS_PAGE) else {
                        break;
                    };
                    let off = at % MAPS_PAGE;
                    let take = (MAPS_PAGE - off).min(n - done);
                    if !crate::uaccess::copy_out(buf_va + done as u64, &pg.0[off..off + take]) {
                        return -EFAULT;
                    }
                    done += take;
                }
                self.fds[slot] = FdKind::ProcMaps { pos: pos + done };
                done as i64
            }
            FdKind::ProcAuxv { pos } => {
                let end = self.auxv_len;
                let n = (end - pos.min(end)).min(count as usize);
                if !crate::uaccess::copy_out(buf_va, &self.auxv[pos..pos + n]) {
                    return -EFAULT;
                }
                self.fds[slot] = FdKind::ProcAuxv { pos: pos + n };
                n as i64
            }
            FdKind::Pipe { idx, writer: false } => match pipe::read(idx as usize, buf_va, count) {
                // Non-blocking here: -EAGAIN when empty with writers still open.
                // The blocking + cross-cell wake path is `proc::sys_read`, which
                // intercepts pipe fds before reaching here; readv/writev on a
                // pipe fall through to this non-blocking behavior (documented).
                pipe::ReadNb::Done(n) => n,
                pipe::ReadNb::WouldBlock => -EAGAIN,
            },
            FdKind::Pipe { writer: true, .. } => -EBADF, // write end not readable
            // Connected socket (AF_UNIX or AF_INET loopback): read this end's rx
            // ring (non-blocking; the cross-cell blocking path is
            // `proc`/`sys_read`, docs/LINUX-COMPAT.md L8). readv/recvmsg fall here.
            FdKind::SockConn { rx, .. } | FdKind::InetConn { rx, .. } => {
                match pipe::read(rx as usize, buf_va, count) {
                    pipe::ReadNb::Done(n) => n,
                    pipe::ReadNb::WouldBlock => -EAGAIN,
                }
            }
            // A connected **remote** TCP socket (rheo-net N4b): the byte stream lives
            // in the registered `svc::SocketOps` datapath, not in a local ring. A
            // non-blocking descriptor passes a zero deadline, so the bridge drains
            // what has already arrived and reports -EAGAIN rather than parking.
            FdKind::InetTcpRemote { h, .. } => match svc::socket_ops() {
                Some(o) => (o.tcp_recv)(
                    h as u64,
                    buf_va,
                    count,
                    if self.flags[slot].nonblock {
                        0
                    } else {
                        REMOTE_RECV_TIMEOUT_NS
                    },
                ),
                None => -ENETUNREACH,
            },
            FdKind::SockFresh
            | FdKind::SockListen { .. }
            | FdKind::InetStreamFresh { .. }
            | FdKind::InetListen { .. }
            | FdKind::InetDgram { .. }
            | FdKind::InetUdpRemote { .. } => -ENOTCONN,
            FdKind::Epoll { .. } => -EINVAL,
            // An eventfd read drains the counter. Non-blocking here, like a pipe:
            // the parking path is `mod::sys_read`, which intercepts eventfd fds
            // before reaching this table (readv falls through to this behaviour).
            FdKind::EventFd { ev } => match eventfd::read(ev, buf_va, count) {
                Ok(eventfd::ReadNb::Done) => 8,
                Ok(eventfd::ReadNb::WouldBlock) => -EAGAIN,
                Err(e) => e,
            },
            // A timerfd read returns the expiration count. Non-blocking here like an
            // eventfd: the parking path is `mod::sys_read`, which intercepts a
            // not-yet-expired timerfd before reaching this table.
            FdKind::TimerFd { tf } => match timerfd::read(tf, buf_va, count) {
                Ok(timerfd::ReadNb::Done) => 8,
                Ok(timerfd::ReadNb::WouldBlock) => -EAGAIN,
                Err(e) => e,
            },
            FdKind::Vfs { vfs_fd, .. } => match svc::file_ops() {
                Some(o) => (o.read)(vfs_fd as u64, buf_va, count),
                None => -EBADF,
            },
            FdKind::Closed => -EBADF,
        }
    }

    /// write(fd, buf, count). `buf_va` is bound to the calling cell's user VA
    /// range (see [`Fds::read`] for why the check is here, not only at entry).
    pub fn write(&mut self, fd: i64, buf_va: u64, count: u64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        if crate::user::user_buf(buf_va, count as usize).is_none() {
            return -EFAULT;
        }
        match self.fds[slot] {
            FdKind::Console(0) => -EBADF,
            FdKind::Console(_) => {
                if crate::uaccess::buf(buf_va, count as usize).is_none() {
                    return -EFAULT;
                }
                // SAFETY: `uaccess::buf` validated the range readable in the active cell.
                let buf =
                    unsafe { core::slice::from_raw_parts(buf_va as *const u8, count as usize) };
                for &b in buf {
                    arch::serial_write_byte(b);
                }
                super::tap_stdout(buf);
                count as i64
            }
            FdKind::Null | FdKind::Zero | FdKind::Urandom => count as i64,
            FdKind::ProcMaps { .. } | FdKind::ProcAuxv { .. } => -EBADF, // read-only
            FdKind::Pipe { writer: false, .. } => -EBADF,                // read end not writable
            FdKind::Pipe { idx, writer: true } => match pipe::write(idx as usize, buf_va, count) {
                pipe::WriteNb::Done(n) => n,
                pipe::WriteNb::WouldBlock => -EAGAIN,
                pipe::WriteNb::Epipe => -EPIPE,
            },
            // Connected socket (AF_UNIX or AF_INET loopback): write this end's tx
            // ring (non-blocking; the cross-cell blocking + SIGPIPE path is
            // `sys_write`). writev/sendmsg fall through to here.
            FdKind::SockConn { tx, .. } | FdKind::InetConn { tx, .. } => {
                match pipe::write(tx as usize, buf_va, count) {
                    pipe::WriteNb::Done(n) => n,
                    pipe::WriteNb::WouldBlock => -EAGAIN,
                    pipe::WriteNb::Epipe => -EPIPE,
                }
            }
            // A connected **remote** TCP socket (rheo-net N4b).
            FdKind::InetTcpRemote { h, .. } => match svc::socket_ops() {
                Some(o) => (o.tcp_send)(h as u64, buf_va, count),
                None => -ENETUNREACH,
            },
            FdKind::SockFresh
            | FdKind::SockListen { .. }
            | FdKind::InetStreamFresh { .. }
            | FdKind::InetListen { .. }
            | FdKind::InetDgram { .. }
            | FdKind::InetUdpRemote { .. } => -ENOTCONN,
            FdKind::Epoll { .. } => -EINVAL,
            FdKind::EventFd { ev } => eventfd::write(ev, buf_va, count),
            // A timerfd is not writable (Linux fs/timerfd.c returns -EINVAL).
            FdKind::TimerFd { .. } => -EINVAL,
            FdKind::Vfs { vfs_fd, .. } => match svc::file_ops() {
                Some(o) => (o.write)(vfs_fd as u64, buf_va, count),
                None => -EBADF,
            },
            FdKind::Closed => -EBADF,
        }
    }

    /// Read up to `len` bytes at file `offset` from a VFS-backed fd into the
    /// (kernel or user) VA `dst`, without disturbing the caller's view of the
    /// fd position beyond an explicit seek. Backs `pread64` and the L7
    /// fd-backed `mmap` (docs/LINUX-COMPAT.md L7): ld.so `pread`s ELF headers
    /// and `mmap`s library segments from the same fd. Only VFS files are
    /// readable this way; other fd kinds return -EBADF (ld.so maps regular
    /// files only). A short read leaves the tail of `dst` untouched (mmap
    /// pre-zeroes its frames).
    /// The VFS path behind `fd`, copied into `out`, returning its length - the
    /// hook a **file-backed mapping** needs so it can open the file *again* and own
    /// its own handle (`linux::filemap`). `ld.so` closes the fd right after `mmap`,
    /// so a mapping that kept the caller's descriptor would reference a closed and
    /// soon-reused one.
    ///
    /// `None` for anything that is not a VFS file: a mapping can only be backed by
    /// something the VFS can re-open.
    pub fn vfs_path(&self, fd: i64, out: &mut [u8]) -> Option<usize> {
        let slot = usize_fd(fd)?;
        match self.fds[slot] {
            FdKind::Vfs { path, path_len, .. } => {
                let n = (path_len as usize).min(out.len());
                out[..n].copy_from_slice(&path[..n]);
                Some(n)
            }
            _ => None,
        }
    }

    pub fn pread(&self, fd: i64, dst: u64, len: u64, offset: i64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        match self.fds[slot] {
            FdKind::Vfs { vfs_fd, .. } => match svc::file_ops() {
                Some(o) => {
                    (o.lseek)(vfs_fd as u64, offset, 0); // SEEK_SET
                    (o.read)(vfs_fd as u64, dst, len)
                }
                None => -EBADF,
            },
            FdKind::Closed => -EBADF,
            _ => -EBADF,
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
        if crate::uaccess::buf(path_va, path_len).is_none() {
            return -EFAULT;
        }
        // SAFETY: `uaccess::buf` bounded and faulted in `[path_va, path_va+path_len)`
        // in the active cell.
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
            self.set_open_flags(slot, flags);
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
        self.set_open_flags(slot, flags);
        slot as i64
    }

    /// Record the access mode, `O_CLOEXEC` and `O_NONBLOCK` an `openat` was given.
    ///
    /// **Creation-time `O_NONBLOCK` is now honoured** (docs/ARCHITECTURE-DEBT.md
    /// 2.4). It could not be before, and the reason is worth recording because it was
    /// not laziness: `poll` reported every descriptor ready for whatever was asked,
    /// so a program that opened a descriptor non-blocking, polled it, and then read it
    /// would have been told "ready", read `-EAGAIN`, and spun forever. Honouring the
    /// flag was only safe once `poll` computed real readiness - which is why the two
    /// landed in the same slice.
    ///
    /// `O_APPEND`/`O_ASYNC` are still **refused** by `fcntl` rather than silently
    /// accepted here (docs/ENGINEERING.md 7); a `VFS` open with them is a no-op
    /// because the VFS has no append mode, which the honesty table states.
    fn set_open_flags(&mut self, slot: usize, flags: u64) {
        self.flags[slot] = FdFlags::new((flags & O_ACCMODE) as u8);
        self.flags[slot].cloexec = flags & O_CLOEXEC != 0;
        self.flags[slot].nonblock = flags & O_NONBLOCK != 0;
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
            FdKind::Pipe { idx, writer } => {
                pipe::close_end(idx as usize, writer); // reclaimed when both ends close
            }
            FdKind::SockConn { rx, tx } => unixsock::drop_conn(rx, tx),
            FdKind::SockListen { lst } => unixsock::close(lst),
            FdKind::InetConn { rx, tx, .. } => inetsock::drop_conn(rx, tx),
            FdKind::InetListen { lst, .. } => inetsock::close_listener(lst),
            FdKind::InetDgram {
                ep, bound: true, ..
            } => inetsock::close_dgram(ep),
            // Remote (NIC-backed) sockets: release the bridge's handle. These are
            // NOT reference-counted across `dup`/`fork` - a duplicated remote
            // socket aliases one handle and the first close releases it (a
            // documented N4b deferral, docs/LINUX-COMPAT.md L8-INET remote).
            FdKind::InetUdpRemote { ep, .. } => {
                if let Some(o) = svc::socket_ops() {
                    (o.udp_close)(ep as u64);
                }
            }
            FdKind::InetTcpRemote { h, .. } => {
                if let Some(o) = svc::socket_ops() {
                    (o.tcp_close)(h as u64);
                }
            }
            FdKind::Epoll { ep } => epoll::close(ep),
            FdKind::EventFd { ev } => eventfd::close(ev),
            FdKind::TimerFd { tf } => timerfd::close(tf),
            _ => {}
        }
        self.fds[slot] = FdKind::Closed;
        self.flags[slot] = FdFlags::new(ACC_RDWR);
        0
    }

    /// dup(oldfd) - lowest free slot. Vfs entries share the underlying VFS fd
    /// (close-once semantics; acceptable for the L2 fixtures). Per POSIX the new
    /// descriptor does **not** inherit `FD_CLOEXEC`.
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
        self.flags[slot] = self.flags[old];
        self.flags[slot].cloexec = false;
        self.bump_if_pipe(slot);
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
            // dup2/dup3(flags = 0): the new descriptor is explicitly NOT
            // close-on-exec, whatever the old one was. A pipeline child dup2s a
            // pipe end onto fd 0/1 and then `execve`s, so getting this backwards
            // would close the pipeline.
            self.flags[new] = self.flags[old];
            self.flags[new].cloexec = false;
            self.bump_if_pipe(new);
        }
        new as i64
    }

    /// If slot holds a pipe end or a socket, add a reference to the shared
    /// backing (a dup shares the end/rings/listener, so close must be balanced).
    fn bump_if_pipe(&self, slot: usize) {
        match self.fds[slot] {
            FdKind::Pipe { idx, writer } => pipe::add_end(idx as usize, writer),
            FdKind::SockConn { rx, tx } | FdKind::InetConn { rx, tx, .. } => {
                pipe::add_end(rx as usize, false);
                pipe::add_end(tx as usize, true);
            }
            FdKind::SockListen { lst } => unixsock::addref(lst),
            FdKind::InetListen { lst, .. } => inetsock::addref_listener(lst),
            FdKind::InetDgram {
                ep, bound: true, ..
            } => inetsock::addref_dgram(ep),
            FdKind::Epoll { ep } => epoll::addref(ep),
            FdKind::EventFd { ev } => eventfd::addref(ev),
            FdKind::TimerFd { tf } => timerfd::addref(tf),
            _ => {}
        }
    }

    /// fcntl(fd, cmd, arg).
    ///
    /// **What changed and why** (docs/ENGINEERING.md 7): this used to end in
    /// `_ => 0`, so *every* command it did not implement - `F_SETLK`, `F_SETOWN`,
    /// `F_GETPIPE_SZ`, `F_ADD_SEALS`, anything a future libc probes - reported
    /// success while doing nothing. A feature probe that asks "can you lock this
    /// file?" was told yes. An unimplemented command now **fails**, with a
    /// distinguishable error per family, so the probe learns the truth:
    ///
    /// - file **locking** (`F_GETLK`/`F_SETLK`/`F_SETLKW` and their OFD forms) ->
    ///   `-ENOLCK`: there is no lock manager in this personality, which is exactly
    ///   what POSIX's "no locks available" says;
    /// - everything else unimplemented -> `-EINVAL`, the errno Linux itself uses
    ///   for an unrecognised `cmd`.
    ///
    /// `F_GETFL`/`F_SETFL` are now real: the access mode is the one the descriptor
    /// was opened with (not a hardcoded `O_RDWR`), and `O_NONBLOCK` is **tracked
    /// and honoured** - a would-block read/write on a non-blocking descriptor
    /// returns `-EAGAIN` instead of parking the cell or reporting 0. `O_APPEND` and
    /// `O_ASYNC` are **refused** (`-EINVAL`) rather than accepted-and-dropped: this
    /// personality does not reposition on write and delivers no SIGIO.
    ///
    /// **Creation-time** `O_NONBLOCK`/`SOCK_NONBLOCK` (`open`/`socket`/`socketpair`/
    /// `accept4`/`pipe2`) is now honoured too (docs/ARCHITECTURE-DEBT.md 2.4). It
    /// could not be before, and the reason is the interesting part: `poll` reported
    /// every open descriptor ready without consulting readiness at all, so a
    /// non-blocking program's poll-then-read loop would be told "ready", read
    /// `-EAGAIN`, and spin. glibc's resolver is exactly such a program - it creates
    /// its UDP socket with `SOCK_NONBLOCK` - and DNS worked *because* the flag was
    /// dropped and its `recvfrom` therefore blocked. Honouring the flag is only
    /// correct alongside a `poll` that computes real readiness **and waits** for it,
    /// which is why both landed in one slice: the resolver now blocks in `poll` until
    /// the reply is on the socket, then its non-blocking `recvfrom` succeeds.
    pub fn fcntl(&mut self, fd: i64, cmd: u64, arg: u64) -> i64 {
        const F_DUPFD: u64 = 0;
        const F_GETFD: u64 = 1;
        const F_SETFD: u64 = 2;
        const F_GETFL: u64 = 3;
        const F_SETFL: u64 = 4;
        const F_GETLK: u64 = 5;
        const F_SETLK: u64 = 6;
        const F_SETLKW: u64 = 7;
        const F_OFD_GETLK: u64 = 36;
        const F_OFD_SETLK: u64 = 37;
        const F_OFD_SETLKW: u64 = 38;
        const F_DUPFD_CLOEXEC: u64 = 1030;
        const F_GETPIPE_SZ: u64 = 1032;
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
                self.flags[dst] = self.flags[slot];
                self.flags[dst].cloexec = cmd == F_DUPFD_CLOEXEC;
                self.bump_if_pipe(dst);
                dst as i64
            }
            F_GETFD => {
                if self.flags[slot].cloexec {
                    FD_CLOEXEC as i64
                } else {
                    0
                }
            }
            F_SETFD => {
                self.flags[slot].cloexec = arg & FD_CLOEXEC != 0;
                0
            }
            F_GETFL => {
                (self.flags[slot].accmode as u64
                    | if self.flags[slot].nonblock {
                        O_NONBLOCK
                    } else {
                        0
                    }) as i64
            }
            F_SETFL => {
                // Linux ignores the access mode and the creation flags here, so
                // only the status bits matter. Refuse the two we cannot honour
                // rather than drop them silently.
                if arg & (O_APPEND | O_ASYNC) != 0 {
                    return -EINVAL;
                }
                self.flags[slot].nonblock = arg & O_NONBLOCK != 0;
                0
            }
            // A real answer where there is one: the pipe ring's actual capacity.
            F_GETPIPE_SZ if matches!(self.fds[slot], FdKind::Pipe { .. }) => pipe::PIPE_CAP as i64,
            // No lock manager exists - say so, with the errno POSIX reserves for it.
            F_GETLK | F_SETLK | F_SETLKW | F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW => -ENOLCK,
            other => {
                crate::println!("linux: fcntl cmd {other} unsupported");
                -EINVAL
            }
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
            FdKind::ProcMaps { .. } | FdKind::ProcAuxv { .. } | FdKind::Pipe { .. } => -ESPIPE,
            FdKind::SockFresh
            | FdKind::SockListen { .. }
            | FdKind::SockConn { .. }
            | FdKind::InetStreamFresh { .. }
            | FdKind::InetListen { .. }
            | FdKind::InetConn { .. }
            | FdKind::InetDgram { .. }
            | FdKind::InetUdpRemote { .. }
            | FdKind::InetTcpRemote { .. }
            | FdKind::Epoll { .. }
            | FdKind::EventFd { .. }
            | FdKind::TimerFd { .. } => -ESPIPE,
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
            FdKind::ProcMaps { .. } | FdKind::ProcAuxv { .. } => {
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
            FdKind::SockFresh
            | FdKind::SockListen { .. }
            | FdKind::SockConn { .. }
            | FdKind::InetStreamFresh { .. }
            | FdKind::InetListen { .. }
            | FdKind::InetConn { .. }
            | FdKind::InetDgram { .. }
            | FdKind::InetUdpRemote { .. }
            | FdKind::InetTcpRemote { .. }
            | FdKind::Epoll { .. } => {
                Stat::new(S_IFSOCK | 0o600, 0, 1, 1, 1000, 1000, 0, 4096, 0, 0)
            }
            // An eventfd is an anonymous inode, which Linux reports as a regular
            // file of size 0 - not a socket. glibc's `fstat` on it must not look
            // like something you can `recv` from.
            FdKind::EventFd { .. } => {
                Stat::new(dirent::S_IFREG | 0o600, 0, 1, 1, 1000, 1000, 0, 4096, 0, 0)
            }
            // A timerfd is likewise an anonymous inode: a regular file of size 0.
            FdKind::TimerFd { .. } => {
                Stat::new(dirent::S_IFREG | 0o600, 0, 1, 1, 1000, 1000, 0, 4096, 0, 0)
            }
            FdKind::Vfs { vfs_fd, .. } => {
                let Some(o) = svc::file_ops() else {
                    return -EBADF;
                };
                // FileOps writes the native abi::Stat into a kernel temp
                // (identity-mapped, writable there); convert to the Linux ABI.
                let mut native = crate::abi::Stat {
                    size: 0,
                    kind: 0,
                    ino: 0,
                };
                let r = (o.fstat)(vfs_fd as u64, &mut native as *mut _ as u64);
                if r < 0 {
                    return r;
                }
                let mode = dirent::mode_for_kind(native.kind);
                let blocks = native.size.div_ceil(512);
                // `native.ino` is the VFS inode - distinct per file, which glibc's
                // ld.so requires to tell two shared libraries apart (a shared inode
                // makes it treat the second as already-loaded; docs/LINUX-COMPAT.md).
                Stat::new(
                    mode,
                    native.size,
                    native.ino,
                    1,
                    1000,
                    1000,
                    0,
                    4096,
                    blocks,
                    0,
                )
            }
        };
        let Some(out) = crate::user::user_out::<Stat>(statbuf_va) else {
            return -EFAULT;
        };
        // SAFETY: `out` was validated non-null, `Stat`-aligned and inside the
        // calling cell's user VA range; its address space is active.
        unsafe { out.write(st) };
        0
    }

    /// The `(st_mode, size, ino)` a `fstat`/`statx` would report for `fd`, without
    /// writing a `struct stat`. Used by `statx` (docs/LINUX-COMPAT.md L3), which
    /// has its own ABI-independent buffer layout. `ino` is the VFS inode for a real
    /// file (distinct per file, so statx agrees with fstat), 0 for anonymous fds.
    pub fn mode_size(&mut self, fd: i64) -> Result<(u32, u64, u64), i64> {
        let Some(slot) = usize_fd(fd) else {
            return Err(-EBADF);
        };
        match self.fds[slot] {
            FdKind::Closed => Err(-EBADF),
            FdKind::Console(_) | FdKind::Null | FdKind::Zero | FdKind::Urandom => {
                Ok((dirent::S_IFCHR | 0o620, 0, 0))
            }
            FdKind::ProcMaps { .. } | FdKind::ProcAuxv { .. } => {
                Ok((dirent::S_IFREG | 0o444, self.auxv_len as u64, 0))
            }
            FdKind::Pipe { .. } => Ok((dirent::S_IFIFO | 0o600, 0, 0)),
            FdKind::SockFresh
            | FdKind::SockListen { .. }
            | FdKind::SockConn { .. }
            | FdKind::InetStreamFresh { .. }
            | FdKind::InetListen { .. }
            | FdKind::InetConn { .. }
            | FdKind::InetDgram { .. }
            | FdKind::InetUdpRemote { .. }
            | FdKind::InetTcpRemote { .. }
            | FdKind::Epoll { .. } => Ok((S_IFSOCK | 0o600, 0, 0)),
            FdKind::EventFd { .. } | FdKind::TimerFd { .. } => Ok((dirent::S_IFREG | 0o600, 0, 0)),
            FdKind::Vfs { vfs_fd, .. } => {
                let Some(o) = svc::file_ops() else {
                    return Err(-EBADF);
                };
                let mut native = crate::abi::Stat {
                    size: 0,
                    kind: 0,
                    ino: 0,
                };
                let r = (o.fstat)(vfs_fd as u64, &mut native as *mut _ as u64);
                if r < 0 {
                    return Err(r);
                }
                Ok((dirent::mode_for_kind(native.kind), native.size, native.ino))
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
        // SAFETY: `uaccess::slice` bounds it, faults it in and un-shares it; we are
        // servicing this cell's synchronous trap.
        let Some(out) = (unsafe { crate::uaccess::slice(buf_va, len as usize) }) else {
            return -EFAULT;
        };
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

    // ----------------------------------------------------- AF_UNIX sockets (L8)

    /// Validate an AF_UNIX SOCK_STREAM `(domain, type)`, or an errno. SOCK_DGRAM
    /// is refused (`-EPROTONOSUPPORT`) - datagram boundary preservation is a
    /// documented deferral (docs/LINUX-COMPAT.md L8). SOCK_CLOEXEC/SOCK_NONBLOCK
    /// in the high bits are accepted + ignored (the ends block cooperatively).
    fn check_stream(domain: u64, ty: u64) -> Result<(), i64> {
        if domain != AF_UNIX {
            return Err(-EAFNOSUPPORT);
        }
        match ty & SOCK_TYPE_MASK {
            SOCK_STREAM => Ok(()),
            SOCK_DGRAM => Err(-EPROTONOSUPPORT),
            _ => Err(-EPROTONOSUPPORT),
        }
    }

    /// socket(domain, type, protocol): an unbound socket. AF_UNIX stream (L8) or
    /// an AF_INET/AF_INET6 loopback stream/datagram socket (L8-INET).
    /// `SOCK_CLOEXEC` and `SOCK_NONBLOCK` (== `O_NONBLOCK`) in the type's high bits
    /// are both **honoured** (docs/ARCHITECTURE-DEBT.md 2.4).
    pub fn socket(&mut self, domain: u64, ty: u64) -> i64 {
        match domain {
            AF_UNIX => {
                if let Err(e) = Self::check_stream(domain, ty) {
                    return e;
                }
                let Some(slot) = self.free_slot(3) else {
                    return -EMFILE;
                };
                self.fds[slot] = FdKind::SockFresh;
                self.flags[slot] = FdFlags::new(ACC_RDWR);
                self.flags[slot].cloexec = ty & O_CLOEXEC != 0;
                self.flags[slot].nonblock = ty & O_NONBLOCK != 0;
                slot as i64
            }
            AF_INET | AF_INET6 => {
                let v6 = domain == AF_INET6;
                let Some(slot) = self.free_slot(3) else {
                    return -EMFILE;
                };
                self.fds[slot] = match ty & SOCK_TYPE_MASK {
                    SOCK_STREAM => FdKind::InetStreamFresh { v6, local_port: 0 },
                    SOCK_DGRAM => FdKind::InetDgram {
                        v6,
                        ep: 0,
                        bound: false,
                        peer_port: 0,
                    },
                    _ => return -EPROTONOSUPPORT,
                };
                self.flags[slot] = FdFlags::new(ACC_RDWR);
                self.flags[slot].cloexec = ty & O_CLOEXEC != 0;
                self.flags[slot].nonblock = ty & O_NONBLOCK != 0;
                slot as i64
            }
            _ => -EAFNOSUPPORT,
        }
    }

    /// epoll_create1(flags): an epoll instance as a new fd (L8-INET).
    /// `EPOLL_CLOEXEC` is honoured.
    pub fn epoll_create(&mut self, flags: u64) -> i64 {
        let Some(ep) = epoll::create() else {
            return -ENFILE;
        };
        let Some(slot) = self.free_slot(3) else {
            epoll::close(ep);
            return -EMFILE;
        };
        self.fds[slot] = FdKind::Epoll { ep };
        self.flags[slot] = FdFlags::new(ACC_RDWR);
        self.flags[slot].cloexec = flags & O_CLOEXEC != 0;
        slot as i64
    }

    /// `eventfd2(initval, flags)`: a counter as a new fd
    /// (docs/LINUX-COMPAT.md L8-EVENTFD). `EFD_CLOEXEC` and `EFD_NONBLOCK` are
    /// honoured on the descriptor (the flags the descriptor owns); `EFD_SEMAPHORE`
    /// belongs to the object and is passed to the registry. Any other flag bit is
    /// `-EINVAL` rather than ignored - a dropped flag is a silent wrong answer.
    pub fn eventfd_create(&mut self, initval: u64, flags: u64) -> i64 {
        use eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EFD_SEMAPHORE};
        if flags & !(EFD_CLOEXEC | EFD_NONBLOCK | EFD_SEMAPHORE) != 0 {
            return -EINVAL;
        }
        let Some(ev) = eventfd::create(initval, flags & EFD_SEMAPHORE != 0) else {
            return -ENFILE;
        };
        let Some(slot) = self.free_slot(3) else {
            eventfd::close(ev);
            return -EMFILE;
        };
        self.fds[slot] = FdKind::EventFd { ev };
        self.flags[slot] = FdFlags::new(ACC_RDWR);
        self.flags[slot].cloexec = flags & EFD_CLOEXEC != 0;
        self.flags[slot].nonblock = flags & EFD_NONBLOCK != 0;
        slot as i64
    }

    /// timerfd_create(clockid, flags): a disarmed timer descriptor
    /// (docs/LINUX-COMPAT.md L8-TIMERFD). `TFD_CLOEXEC`/`TFD_NONBLOCK` are the
    /// descriptor flags, like an eventfd's; an unsupported clock is `-EINVAL`.
    pub fn timerfd_create(&mut self, clockid: u64, flags: u64) -> i64 {
        use timerfd::{TFD_CLOEXEC, TFD_NONBLOCK};
        if flags & !(TFD_CLOEXEC | TFD_NONBLOCK) != 0 {
            return -EINVAL;
        }
        let Some(tf) = timerfd::create(clockid) else {
            // A full table is -ENFILE; an unsupported clock is -EINVAL. `create`
            // folds both into `None`, but only a bad clock is a caller error - the
            // table being full is the system's. Distinguish by re-checking the clock.
            return if clockid == timerfd::CLOCK_MONOTONIC || clockid == timerfd::CLOCK_REALTIME {
                -ENFILE
            } else {
                -EINVAL
            };
        };
        let Some(slot) = self.free_slot(3) else {
            timerfd::close(tf);
            return -EMFILE;
        };
        self.fds[slot] = FdKind::TimerFd { tf };
        self.flags[slot] = FdFlags::new(ACC_RDWR);
        self.flags[slot].cloexec = flags & TFD_CLOEXEC != 0;
        self.flags[slot].nonblock = flags & TFD_NONBLOCK != 0;
        slot as i64
    }

    /// `close_range(first, last, flags)`: close every open descriptor in the
    /// inclusive range (docs/ARCHITECTURE-DEBT.md 4.0, blocker 3).
    ///
    /// glibc falls back to a `close` loop on `-ENOSYS`, so this is a *performance*
    /// call rather than a functional one - but a loop over 64 descriptors is 64
    /// syscalls, and the call is trivial to serve correctly. Closed slots in the
    /// range are skipped, not an error, exactly as Linux does. `CLOSE_RANGE_CLOEXEC`
    /// (mark rather than close) is honoured; `CLOSE_RANGE_UNSHARE` needs an fd table
    /// this personality does not share separately from the address space, so it is
    /// refused `-EINVAL` rather than silently ignored.
    pub fn close_range(&mut self, first: u64, last: u64, flags: u64) -> i64 {
        const CLOSE_RANGE_UNSHARE: u64 = 1 << 1;
        const CLOSE_RANGE_CLOEXEC: u64 = 1 << 2;
        if flags & !CLOSE_RANGE_CLOEXEC != 0 {
            // Includes CLOSE_RANGE_UNSHARE, deliberately.
            let _ = CLOSE_RANGE_UNSHARE;
            return -EINVAL;
        }
        if first > last {
            return -EINVAL;
        }
        let lo = first as usize;
        // `last` is commonly `UINT_MAX` ("everything above"), so clamp rather than
        // iterate to it.
        let hi = (last as usize).min(NFD - 1);
        for slot in lo..=hi.max(lo) {
            if slot >= NFD || matches!(self.fds[slot], FdKind::Closed) {
                continue;
            }
            if flags & CLOSE_RANGE_CLOEXEC != 0 {
                self.flags[slot].cloexec = true;
            } else {
                self.close(slot as i64);
            }
        }
        0
    }

    /// The registry index behind `fd` if it is an eventfd - the hook `sys_read`
    /// uses to intercept a blocking read before the non-blocking table path, the
    /// same shape as [`Self::pipe_end`].
    pub fn eventfd_of(&self, fd: i64) -> Option<u8> {
        let slot = usize_fd(fd)?;
        match self.fds[slot] {
            FdKind::EventFd { ev } => Some(ev),
            _ => None,
        }
    }

    /// The registry index behind `fd` if it is a timerfd - the `sys_read` hook that
    /// intercepts a blocking read on a not-yet-expired timer, mirroring
    /// [`Self::eventfd_of`].
    pub fn timerfd_of(&self, fd: i64) -> Option<u8> {
        let slot = usize_fd(fd)?;
        match self.fds[slot] {
            FdKind::TimerFd { tf } => Some(tf),
            _ => None,
        }
    }

    /// epoll_ctl(epfd, op, fd, event): register/modify/remove a watched fd. Reads
    /// the `struct epoll_event` (`events` u32, then `data` u64 at the per-ISA
    /// offset - x86-64 packs it, ARM64/RISC-V align it).
    pub fn epoll_ctl(&mut self, epfd: i64, op: u64, fd: i64, event_va: u64) -> i64 {
        let Some(slot) = usize_fd(epfd) else {
            return -EBADF;
        };
        let FdKind::Epoll { ep } = self.fds[slot] else {
            return -EINVAL;
        };
        let (events, data) = if event_va != 0 {
            // `data` sits at a per-ISA offset the ABI does not promise to align, so
            // both fields are read unaligned - through `uaccess`, which bounds and
            // faults in the `struct epoll_event` first.
            let doff = arch::linux_abi::EPOLL_EVENT_DATA_OFFSET as u64;
            match (
                crate::uaccess::read_unaligned::<u32>(event_va),
                crate::uaccess::read_unaligned::<u64>(event_va + doff),
            ) {
                (Some(ev), Some(d)) => (ev, d),
                _ => return -EFAULT,
            }
        } else {
            (0, 0)
        };
        epoll::ctl(ep, op, fd as i32, events, data)
    }

    /// epoll_wait/epoll_pwait(epfd, events, maxevents, ...): report level-triggered
    /// readiness for the watched fds. Non-blocking (readiness is computed now); see
    /// `linux::epoll`.
    pub fn epoll_wait(&mut self, epfd: i64, events_va: u64, maxevents: usize) -> i64 {
        let Some(slot) = usize_fd(epfd) else {
            return -EBADF;
        };
        let FdKind::Epoll { ep } = self.fds[slot] else {
            return -EINVAL;
        };
        let mut snap = [(0i32, 0u32, 0u64); epoll::MAX_WATCH];
        let n = epoll::snapshot(ep, &mut snap);
        let size = arch::linux_abi::EPOLL_EVENT_SIZE as u64;
        let doff = arch::linux_abi::EPOLL_EVENT_DATA_OFFSET as u64;
        let mut out = 0usize;
        for &(wfd, wevents, data) in snap[..n].iter() {
            if out >= maxevents {
                break;
            }
            let mut re = 0u32;
            if wevents & epoll::EPOLLIN != 0 && self.pollin_ready(wfd as i64) {
                re |= epoll::EPOLLIN;
            }
            if wevents & epoll::EPOLLOUT != 0 && self.pollout_ready(wfd as i64) {
                re |= epoll::EPOLLOUT;
            }
            if re != 0 {
                let base = events_va + out as u64 * size;
                if !crate::uaccess::write_unaligned::<u32>(base, re)
                    || !crate::uaccess::write_unaligned::<u64>(base + doff, data)
                {
                    return -EFAULT;
                }
                out += 1;
            }
        }
        out as i64
    }

    /// How many of instance `epfd`'s watched fds are ready right now. The
    /// satisfiability test for a **blocking** `epoll_wait`
    /// (docs/ARCHITECTURE-DEBT.md 2.4): computed from kernel state only, so the
    /// scheduler can ask it while another cell's address space is active.
    pub fn epoll_ready(&self, epfd: i64) -> usize {
        let Some(slot) = usize_fd(epfd) else {
            return 0;
        };
        let FdKind::Epoll { ep } = self.fds[slot] else {
            return 0;
        };
        let mut snap = [(0i32, 0u32, 0u64); epoll::MAX_WATCH];
        let n = epoll::snapshot(ep, &mut snap);
        snap[..n]
            .iter()
            .filter(|&&(wfd, wevents, _)| {
                (wevents & epoll::EPOLLIN != 0 && self.pollin_ready(wfd as i64))
                    || (wevents & epoll::EPOLLOUT != 0 && self.pollout_ready(wfd as i64))
            })
            .count()
    }

    /// The union of wake sources instance `epfd`'s watched fds can be woken by, so
    /// the scheduler knows what to idle on (docs/ARCHITECTURE-DEBT.md 2.4). Empty
    /// means nothing could ever make this set ready, and `epoll_wait` must not park.
    pub fn epoll_sources(&self, epfd: i64) -> crate::idle::Sources {
        let Some(slot) = usize_fd(epfd) else {
            return 0;
        };
        let FdKind::Epoll { ep } = self.fds[slot] else {
            return 0;
        };
        let mut snap = [(0i32, 0u32, 0u64); epoll::MAX_WATCH];
        let n = epoll::snapshot(ep, &mut snap);
        snap[..n]
            .iter()
            .fold(0, |acc, &(wfd, _, _)| acc | self.fd_sources(wfd as i64))
    }

    /// What can make descriptor `fd` change readiness (docs/ARCHITECTURE-DEBT.md
    /// 2.4). This is the per-`FdKind` answer to "if a cell blocks on this, what wakes
    /// it?", and it is what keeps a blocking `poll`/`epoll_wait` from parking on a
    /// condition nothing can produce.
    ///
    /// A **remote** (NIC-backed) socket is woken by the network; a pipe or a
    /// loopback socket by another *process*; the console by console input. Everything
    /// whose readiness is constant (a regular file, `/dev/null`, a closed fd) has no
    /// source at all - it is either already ready or never will be, and in both cases
    /// parking is wrong.
    pub fn fd_sources(&self, fd: i64) -> crate::idle::Sources {
        use crate::idle;
        let Some(slot) = usize_fd(fd) else {
            return 0;
        };
        match self.fds[slot] {
            FdKind::Pipe { .. }
            | FdKind::SockConn { .. }
            | FdKind::InetConn { .. }
            | FdKind::SockListen { .. }
            | FdKind::InetListen { .. }
            | FdKind::InetDgram { .. }
            // An eventfd's counter only ever changes because another *cell* wrote
            // it (a sibling context writing it does not park the cell at all), so
            // the wake source is a peer, exactly as for a pipe.
            | FdKind::EventFd { .. } => idle::PEER,
            // A timerfd changes readiness only when its deadline passes - a cell-clock
            // deadline, honoured by the scheduler's timer slices (docs/NETSTACK.md 16).
            FdKind::TimerFd { .. } => idle::TIMER,
            FdKind::InetUdpRemote { .. } | FdKind::InetTcpRemote { .. } => idle::NET,
            FdKind::Console(0) => idle::CONSOLE,
            // An epoll fd's own readiness is the union of what it watches; asking
            // recursively would need a depth bound, and nesting epolls is outside
            // this personality's scope (docs/LINUX-COMPAT.md L8-INET).
            FdKind::Epoll { .. } => 0,
            _ => 0,
        }
    }

    /// socketpair(domain, type, protocol, sv): two connected AF_UNIX sockets
    /// backed by two direction rings (L8). Writes the fd pair into `sv_va`.
    /// `SOCK_CLOEXEC` is honoured on both ends.
    pub fn socketpair(&mut self, domain: u64, ty: u64, sv_va: u64) -> i64 {
        if let Err(e) = Self::check_stream(domain, ty) {
            return e;
        }
        let Some((a_rx, a_tx, b_rx, b_tx)) = unixsock::socketpair() else {
            return -ENFILE;
        };
        let Some(s0) = self.free_slot(3) else {
            unixsock::drop_conn(a_rx, a_tx);
            unixsock::drop_conn(b_rx, b_tx);
            return -EMFILE;
        };
        self.fds[s0] = FdKind::SockConn { rx: a_rx, tx: a_tx };
        let Some(s1) = self.free_slot(3) else {
            self.fds[s0] = FdKind::Closed;
            unixsock::drop_conn(a_rx, a_tx);
            unixsock::drop_conn(b_rx, b_tx);
            return -EMFILE;
        };
        self.fds[s1] = FdKind::SockConn { rx: b_rx, tx: b_tx };
        self.flags[s0] = FdFlags::new(ACC_RDWR);
        self.flags[s1] = FdFlags::new(ACC_RDWR);
        self.flags[s0].cloexec = ty & O_CLOEXEC != 0;
        self.flags[s1].cloexec = ty & O_CLOEXEC != 0;
        self.flags[s0].nonblock = ty & O_NONBLOCK != 0;
        self.flags[s1].nonblock = ty & O_NONBLOCK != 0;
        if !crate::uaccess::write::<i32>(sv_va, s0 as i32)
            || !crate::uaccess::write::<i32>(sv_va + 4, s1 as i32)
        {
            return -EFAULT;
        }
        0
    }

    /// bind(fd, addr, addrlen): register the socket's name in the global registry
    /// (L8). Marks the fd listening-capable; `listen` then validates it.
    pub fn bind(&mut self, fd: i64, addr_va: u64, addrlen: u64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        match self.fds[slot] {
            FdKind::SockFresh => {
                let mut key = [0u8; NAME_MAX];
                let Some(klen) = read_sun_key(addr_va, addrlen, &mut key) else {
                    return -EINVAL;
                };
                match unixsock::bind(&key[..klen]) {
                    Some(lst) => {
                        self.fds[slot] = FdKind::SockListen { lst };
                        0
                    }
                    None => -EADDRINUSE,
                }
            }
            // AF_INET stream: record the local port; `listen` registers it.
            FdKind::InetStreamFresh { v6, .. } => {
                let Some((av6, port, _, _)) = read_inaddr(addr_va, addrlen) else {
                    return -EINVAL;
                };
                if av6 != v6 {
                    return -EINVAL;
                }
                self.fds[slot] = FdKind::InetStreamFresh {
                    v6,
                    local_port: port,
                };
                0
            }
            // AF_INET datagram: register the UDP endpoint now.
            FdKind::InetDgram {
                v6, bound: false, ..
            } => {
                let Some((av6, port, _, _)) = read_inaddr(addr_va, addrlen) else {
                    return -EINVAL;
                };
                if av6 != v6 {
                    return -EINVAL;
                }
                match inetsock::register_dgram(v6, port) {
                    Some(ep) => {
                        self.fds[slot] = FdKind::InetDgram {
                            v6,
                            ep,
                            bound: true,
                            peer_port: 0,
                        };
                        0
                    }
                    None => -EADDRINUSE,
                }
            }
            _ => -EINVAL,
        }
    }

    /// listen(fd, backlog): a bound socket is accept-ready (the registry already
    /// carries its backlog); `backlog` is advisory here. For an AF_INET stream the
    /// listener is registered now (auto-binding an ephemeral port if unbound).
    pub fn listen(&mut self, fd: i64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        match self.fds[slot] {
            FdKind::SockListen { .. } | FdKind::InetListen { .. } => 0,
            FdKind::SockFresh => -EINVAL, // not bound
            FdKind::InetStreamFresh { v6, local_port } => {
                let port = if local_port != 0 {
                    local_port
                } else {
                    inetsock::ephemeral_port()
                };
                match inetsock::register_listener(v6, port) {
                    Some(lst) => {
                        self.fds[slot] = FdKind::InetListen { lst, v6, port };
                        0
                    }
                    None => -EADDRINUSE,
                }
            }
            _ => -ENOTSOCK,
        }
    }

    /// connect(fd, addr, addrlen): look up the peer's bound name and establish a
    /// connection (allocate the two rings, queue the server ends). Non-blocking:
    /// returns once the connection is queued (AF_UNIX stream semantics).
    pub fn connect(&mut self, fd: i64, addr_va: u64, addrlen: u64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        match self.fds[slot] {
            FdKind::SockConn { .. } | FdKind::InetConn { .. } | FdKind::InetTcpRemote { .. } => {
                return -EISCONN;
            }
            FdKind::SockFresh
            | FdKind::InetStreamFresh { .. }
            | FdKind::InetDgram { .. }
            | FdKind::InetUdpRemote { .. } => {}
            _ => return -EINVAL,
        }
        // A UDP socket already on the remote datapath: just re-point its default
        // destination (a `connect` on a datagram socket sets no state on the wire).
        if let FdKind::InetUdpRemote { ep, port, .. } = self.fds[slot] {
            let Some((_, dport, dst, loop_ok)) = read_inaddr(addr_va, addrlen) else {
                return -EINVAL;
            };
            if loop_ok {
                return -EINVAL; // cannot fall back to loopback once remote
            }
            self.fds[slot] = FdKind::InetUdpRemote {
                ep,
                port,
                peer_ip: dst,
                peer_port: dport,
            };
            return 0;
        }
        // AF_INET stream: look up the loopback listener, allocate the ring pair.
        if let FdKind::InetStreamFresh { v6, local_port } = self.fds[slot] {
            let Some((av6, port, dst, loop_ok)) = read_inaddr(addr_va, addrlen) else {
                return -EINVAL;
            };
            if av6 != v6 {
                return -EAFNOSUPPORT;
            }
            if !loop_ok {
                // A **remote** destination (rheo-net N4b): hand the active open to
                // the registered datapath. The kernel runs no TCP state machine -
                // the bridge does the handshake over the NIC and reports the
                // outcome; without a bridge the answer stays ENETUNREACH, exactly
                // as before N4b. IPv6 remote is a documented deferral (the N4b
                // datapath is IPv4).
                if v6 {
                    return -ENETUNREACH;
                }
                let Some(o) = svc::socket_ops() else {
                    return -ENETUNREACH;
                };
                let src_port = if local_port != 0 {
                    local_port
                } else {
                    inetsock::ephemeral_port()
                };
                let h = (o.tcp_connect)(dst, port, src_port, REMOTE_CONNECT_TIMEOUT_NS);
                if h < 0 {
                    return h;
                }
                self.fds[slot] = FdKind::InetTcpRemote {
                    h: h as u8,
                    local_port: src_port,
                    peer_ip: dst,
                    peer_port: port,
                };
                return 0;
            }
            let client_port = if local_port != 0 {
                local_port
            } else {
                inetsock::ephemeral_port()
            };
            return match inetsock::connect_stream(v6, port, client_port) {
                Ok((rx, tx)) => {
                    self.fds[slot] = FdKind::InetConn {
                        rx,
                        tx,
                        local_port: client_port,
                        peer_port: port,
                        v6,
                    };
                    0
                }
                Err(inetsock::ConnectErr::NoListener) => -ECONNREFUSED,
                Err(inetsock::ConnectErr::Backlog) => -EAGAIN,
                Err(inetsock::ConnectErr::NoRing) => -ENFILE,
            };
        }
        // AF_INET datagram: record a default peer (and ephemeral-bind if needed).
        if let FdKind::InetDgram { v6, ep, bound, .. } = self.fds[slot] {
            let Some((av6, port, dst, loop_ok)) = read_inaddr(addr_va, addrlen) else {
                return -EINVAL;
            };
            if av6 != v6 {
                return -EAFNOSUPPORT;
            }
            if !loop_ok {
                // Remote UDP (rheo-net N4b): move the socket onto the registered
                // datapath, recording the default destination.
                return self.go_remote_udp(slot, v6, ep, bound, dst, port);
            }
            let (ep, bound) = if bound {
                (ep, true)
            } else {
                match inetsock::register_dgram(v6, 0) {
                    Some(e) => (e, true),
                    None => return -EAGAIN,
                }
            };
            self.fds[slot] = FdKind::InetDgram {
                v6,
                ep,
                bound,
                peer_port: port,
            };
            return 0;
        }
        let mut key = [0u8; NAME_MAX];
        let Some(klen) = read_sun_key(addr_va, addrlen, &mut key) else {
            return -EINVAL;
        };
        match unixsock::connect(&key[..klen]) {
            Ok((rx, tx)) => {
                self.fds[slot] = FdKind::SockConn { rx, tx };
                0
            }
            Err(unixsock::ConnectErr::NoListener) => -ECONNREFUSED,
            Err(unixsock::ConnectErr::Backlog) => -EAGAIN,
            Err(unixsock::ConnectErr::NoRing) => -ENFILE,
        }
    }

    /// accept4(fd, addr, addrlen, flags): dequeue a pending connection into a new
    /// fd. Non-blocking: `-EAGAIN` if the backlog is empty (the cooperative
    /// single-process proof connects before accepting; a blocking cross-cell
    /// accept server is a later refinement, docs/LINUX-COMPAT.md L8).
    /// `SOCK_CLOEXEC` and `SOCK_NONBLOCK` in `flags` are both honoured
    /// (docs/ARCHITECTURE-DEBT.md 2.4).
    pub fn accept(&mut self, fd: i64, addr_va: u64, addrlen_va: u64, flags: u64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        // AF_INET stream: dequeue a pending connection, report the client's addr.
        if let FdKind::InetListen { lst, v6, port } = self.fds[slot] {
            let Some((rx, tx, client_port)) = inetsock::accept_stream(lst) else {
                return -EAGAIN;
            };
            let Some(nslot) = self.free_slot(3) else {
                inetsock::drop_conn(rx, tx);
                return -EMFILE;
            };
            self.fds[nslot] = FdKind::InetConn {
                rx,
                tx,
                local_port: port,
                peer_port: client_port,
                v6,
            };
            self.flags[nslot] = FdFlags::new(ACC_RDWR);
            self.flags[nslot].cloexec = flags & O_CLOEXEC != 0;
            self.flags[nslot].nonblock = flags & O_NONBLOCK != 0;
            write_inaddr(addr_va, addrlen_va, v6, client_port);
            return nslot as i64;
        }
        let FdKind::SockListen { lst } = self.fds[slot] else {
            return -EINVAL;
        };
        let Some((rx, tx)) = unixsock::accept(lst) else {
            return -EAGAIN;
        };
        let Some(nslot) = self.free_slot(3) else {
            unixsock::drop_conn(rx, tx);
            return -EMFILE;
        };
        self.fds[nslot] = FdKind::SockConn { rx, tx };
        self.flags[nslot] = FdFlags::new(ACC_RDWR);
        self.flags[nslot].cloexec = flags & O_CLOEXEC != 0;
        self.flags[nslot].nonblock = flags & O_NONBLOCK != 0;
        // The connecting peer is unnamed (no bind): report just the family.
        if addr_va != 0 && addrlen_va != 0 {
            // SAFETY: caller-provided sockaddr + socklen_t out-params.
            unsafe {
                (addr_va as *mut u16).write(AF_UNIX as u16);
                (addrlen_va as *mut u32).write(2);
            }
        }
        nslot as i64
    }

    /// getsockname/getpeername(fd, addr, addrlen, peer): for AF_UNIX report the
    /// bound name (or family-only); for AF_INET report the `sockaddr_in`/`_in6`
    /// (loopback IP + the local or, with `peer`, the remote port).
    pub fn getsockname(&self, fd: i64, addr_va: u64, addrlen_va: u64, peer: bool) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        // AF_INET variants report a sockaddr_in/_in6.
        match self.fds[slot] {
            FdKind::InetListen { v6, port, .. } => {
                write_inaddr(addr_va, addrlen_va, v6, port);
                return 0;
            }
            FdKind::InetStreamFresh { v6, local_port } => {
                write_inaddr(addr_va, addrlen_va, v6, local_port);
                return 0;
            }
            FdKind::InetConn {
                v6,
                local_port,
                peer_port,
                ..
            } => {
                write_inaddr(
                    addr_va,
                    addrlen_va,
                    v6,
                    if peer { peer_port } else { local_port },
                );
                return 0;
            }
            FdKind::InetDgram {
                v6,
                ep,
                bound: true,
                peer_port,
            } => {
                let (_, port) = inetsock::dgram_addr(ep);
                write_inaddr(addr_va, addrlen_va, v6, if peer { peer_port } else { port });
                return 0;
            }
            // Remote sockets report the datapath's own IPv4 address, not loopback
            // (rheo-net N4b): the bridge owns the local identity.
            FdKind::InetUdpRemote {
                port,
                peer_ip,
                peer_port,
                ..
            } => {
                let (ip, p) = if peer {
                    (peer_ip, peer_port)
                } else {
                    (local_ipv4(), port)
                };
                write_inaddr_v4(addr_va, addrlen_va, ip, p);
                return 0;
            }
            FdKind::InetTcpRemote {
                local_port,
                peer_ip,
                peer_port,
                ..
            } => {
                let (ip, p) = if peer {
                    (peer_ip, peer_port)
                } else {
                    (local_ipv4(), local_port)
                };
                write_inaddr_v4(addr_va, addrlen_va, ip, p);
                return 0;
            }
            _ => {}
        }
        // AF_UNIX.
        let (name, nlen) = match self.fds[slot] {
            FdKind::SockListen { lst } => unixsock::name_of(lst),
            FdKind::SockConn { .. } | FdKind::SockFresh => ([0u8; NAME_MAX], 0),
            _ => return -ENOTSOCK,
        };
        if addr_va == 0 {
            return -EINVAL;
        }
        // SAFETY: caller-provided sockaddr_un (>= 2 + nlen bytes) + socklen_t.
        unsafe {
            (addr_va as *mut u16).write(AF_UNIX as u16);
            if nlen > 0 {
                core::ptr::copy_nonoverlapping(name.as_ptr(), (addr_va + 2) as *mut u8, nlen);
            }
            if addrlen_va != 0 {
                (addrlen_va as *mut u32).write((2 + nlen) as u32);
            }
        }
        0
    }

    /// sendto(fd, buf, len, dest_addr, addrlen): a UDP datagram over loopback
    /// (L8-INET). Ephemeral-binds the socket if unbound. Best-effort: reports the
    /// whole datagram accepted even if no endpoint is bound at the destination
    /// (UDP semantics). Stream sockets never reach here (routed to `write`).
    pub fn sendto(&mut self, fd: i64, buf: u64, len: u64, dest_addr: u64, addrlen: u64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        // Already on the remote datapath (rheo-net N4b): send straight over it.
        if let FdKind::InetUdpRemote {
            ep,
            peer_ip,
            peer_port,
            ..
        } = self.fds[slot]
        {
            let Some(o) = svc::socket_ops() else {
                return -ENETUNREACH;
            };
            let (dst, dport) = if dest_addr != 0 {
                let Some((_, port, ip, loop_ok)) = read_inaddr(dest_addr, addrlen) else {
                    return -EINVAL;
                };
                if loop_ok {
                    return -ENETUNREACH; // a remote socket cannot address loopback
                }
                (ip, port)
            } else if peer_port != 0 {
                (peer_ip, peer_port)
            } else {
                return -EINVAL;
            };
            return (o.udp_send)(ep as u64, dst, dport, buf, len);
        }
        let FdKind::InetDgram {
            v6,
            ep,
            bound,
            peer_port,
        } = self.fds[slot]
        else {
            return -ENOTSOCK;
        };
        // Ensure a source port (ephemeral bind on first send).
        let ep = if bound {
            ep
        } else {
            match inetsock::register_dgram(v6, 0) {
                Some(e) => {
                    self.fds[slot] = FdKind::InetDgram {
                        v6,
                        ep: e,
                        bound: true,
                        peer_port,
                    };
                    e
                }
                None => return -EAGAIN,
            }
        };
        let (dv6, dport) = if dest_addr != 0 {
            let Some((av6, port, dst, loop_ok)) = read_inaddr(dest_addr, addrlen) else {
                return -EINVAL;
            };
            if !loop_ok {
                // First non-loopback destination: promote the socket onto the
                // registered remote datapath, then send there (rheo-net N4b).
                let r = self.go_remote_udp(slot, v6, ep, true, dst, port);
                if r < 0 {
                    return r;
                }
                return self.sendto(fd, buf, len, dest_addr, addrlen);
            }
            (av6, port)
        } else if peer_port != 0 {
            (v6, peer_port)
        } else {
            return -EINVAL; // no destination (would be EDESTADDRREQ)
        };
        let (_, src_port) = inetsock::dgram_addr(ep);
        let n = (len as usize).min(inetsock::DGRAM_MAX);
        if crate::uaccess::buf(buf, n).is_none() {
            return -EFAULT;
        }
        // SAFETY: `uaccess::buf` validated the range readable in the active cell.
        let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, n) };
        match inetsock::send_dgram(dv6, dport, src_port, bytes) {
            // Nothing is bound at the destination, so no reader exists now or
            // later: report it (docs/ENGINEERING.md 7). Linux delivers this as an
            // ICMP port-unreachable, which surfaces as `ECONNREFUSED` on the next
            // operation of a *connected* socket; there is no ICMP over this
            // in-kernel loopback queue, so the refusal is reported on the send
            // itself - earlier than Linux for an unconnected `sendto`, and
            // documented (docs/LINUX-COMPAT.md, `sendto` row).
            inetsock::DgramSend::NoEndpoint => -ECONNREFUSED,
            // A full queue is a genuine UDP drop: the datagram is accounted sent.
            inetsock::DgramSend::Delivered | inetsock::DgramSend::Dropped => len as i64,
        }
    }

    /// recvfrom(fd, buf, len, src_addr, addrlen): dequeue one UDP datagram over
    /// loopback (L8-INET), filling `src_addr` with the sender's loopback address +
    /// port. Non-blocking: `-EAGAIN` when the queue is empty.
    pub fn recvfrom(&mut self, fd: i64, buf: u64, len: u64, src_addr: u64, addrlen_va: u64) -> i64 {
        let Some(slot) = usize_fd(fd) else {
            return -EBADF;
        };
        // The remote datapath (rheo-net N4b): the bridge blocks on the wire (a
        // genuine `net_rx` park, not a spin) and fills in the sender's address.
        if let FdKind::InetUdpRemote { ep, .. } = self.fds[slot] {
            let Some(o) = svc::socket_ops() else {
                return -ENETUNREACH;
            };
            let mut src_ip = [0u8; 4];
            let mut src_port: u16 = 0;
            let n = (o.udp_recv)(
                ep as u64,
                buf,
                len,
                core::ptr::addr_of_mut!(src_ip) as u64,
                core::ptr::addr_of_mut!(src_port) as u64,
                // A zero deadline on a non-blocking descriptor: drain, never park.
                if self.flags[slot].nonblock {
                    0
                } else {
                    REMOTE_RECV_TIMEOUT_NS
                },
            );
            if n >= 0 && src_addr != 0 {
                write_inaddr_v4(src_addr, addrlen_va, src_ip, src_port);
            }
            return n;
        }
        let FdKind::InetDgram { v6, ep, bound, .. } = self.fds[slot] else {
            return -ENOTSOCK;
        };
        if !bound {
            return -EAGAIN; // never bound: nothing can have arrived
        }
        // SAFETY: as the other `uaccess::slice` sites - bounded, present, un-shared,
        // and we are inside this cell's synchronous trap.
        let Some(out) = (unsafe { crate::uaccess::slice(buf, len as usize) }) else {
            return -EFAULT;
        };
        match inetsock::recv_dgram(ep, out) {
            Some((src_port, n)) => {
                if src_addr != 0 {
                    write_inaddr(src_addr, addrlen_va, v6, src_port);
                }
                n as i64
            }
            None => -EAGAIN,
        }
    }

    /// If `fd` is a connected socket, its read ring (`rx`). The `sys_read`
    /// blocking path routes a socket read through the L6 pipe scheduler on it.
    pub fn sock_rx(&self, fd: i64) -> Option<usize> {
        match self.fds[usize_fd(fd)?] {
            FdKind::SockConn { rx, .. } | FdKind::InetConn { rx, .. } => Some(rx as usize),
            _ => None,
        }
    }

    /// If `fd` is a connected socket, its write ring (`tx`).
    pub fn sock_tx(&self, fd: i64) -> Option<usize> {
        match self.fds[usize_fd(fd)?] {
            FdKind::SockConn { tx, .. } | FdKind::InetConn { tx, .. } => Some(tx as usize),
            _ => None,
        }
    }

    /// True if `fd` is a datagram (UDP) socket - `sendto`/`recvfrom` route to the
    /// datagram path (`linux::inetsock`) rather than the stream write/read path.
    pub fn is_dgram(&self, fd: i64) -> bool {
        usize_fd(fd).is_some_and(|s| {
            matches!(
                self.fds[s],
                FdKind::InetDgram { .. } | FdKind::InetUdpRemote { .. }
            )
        })
    }

    /// Move a UDP socket from the loopback registry onto the **remote**
    /// (NIC-backed) datapath (rheo-net N4b), recording `peer_ip:peer_port` as its
    /// default destination. The local port carries over when the socket was already
    /// bound, so a `bind`-then-`sendto` program keeps its chosen source port; an
    /// unbound socket gets an ephemeral one. Returns 0 or `-errno`.
    fn go_remote_udp(
        &mut self,
        slot: usize,
        v6: bool,
        ep: u8,
        bound: bool,
        peer_ip: [u8; 4],
        peer_port: u16,
    ) -> i64 {
        // The N4b datapath is IPv4; a v6 remote destination stays ENETUNREACH.
        if v6 {
            return -ENETUNREACH;
        }
        let Some(o) = svc::socket_ops() else {
            return -ENETUNREACH;
        };
        let port = if bound {
            let (_, p) = inetsock::dgram_addr(ep);
            p
        } else {
            inetsock::ephemeral_port()
        };
        let h = (o.udp_bind)(port);
        if h < 0 {
            return h;
        }
        // Release the loopback endpoint - this socket now lives on the wire.
        if bound {
            inetsock::close_dgram(ep);
        }
        self.fds[slot] = FdKind::InetUdpRemote {
            ep: h as u8,
            port,
            peer_ip,
            peer_port,
        };
        0
    }

    /// POLLIN readiness for `fd` (used by epoll, level-triggered). A socket is
    /// readable when its rx ring holds data, its writers all closed (EOF), a
    /// listener has a pending connection, or a datagram is queued.
    pub fn pollin_ready(&self, fd: i64) -> bool {
        let Some(slot) = usize_fd(fd) else {
            return false;
        };
        match self.fds[slot] {
            FdKind::SockConn { rx, .. } | FdKind::InetConn { rx, .. } => {
                pipe::has_data(rx as usize) || pipe::writers(rx as usize) == 0
            }
            FdKind::Pipe { idx, writer: false } => {
                pipe::has_data(idx as usize) || pipe::writers(idx as usize) == 0
            }
            FdKind::InetListen { lst, .. } => inetsock::listener_has_pending(lst),
            FdKind::InetDgram {
                ep, bound: true, ..
            } => inetsock::dgram_has_data(ep),
            // Remote sockets (rheo-net N4b): readiness is a question for the bridge,
            // and both answers **pump the datapath** first - which is what makes a
            // blocking `poll` on a DNS socket become ready when the reply lands
            // (docs/ARCHITECTURE-DEBT.md 2.4). `tcp_pending` used to be missing from
            // `svc::SocketOps` entirely, and a hardcoded `true` here *was* that
            // absence: a poll on a remote TCP socket always claimed readable.
            FdKind::InetUdpRemote { ep, .. } => {
                svc::socket_ops().is_some_and(|o| (o.udp_pending)(ep as u64))
            }
            FdKind::InetTcpRemote { h, .. } => {
                svc::socket_ops().is_some_and(|o| (o.tcp_pending)(h as u64))
            }
            // Console input is readable when a byte is buffered, or at end of input
            // (EOF is a readable condition - a reader must be able to see it).
            FdKind::Console(0) => crate::input::has_data() || crate::input::at_eof(),
            FdKind::Vfs { .. }
            | FdKind::Null
            | FdKind::Zero
            | FdKind::Urandom
            | FdKind::ProcMaps { .. }
            | FdKind::ProcAuxv { .. } => true,
            // An epoll fd is readable when one of its watches is - which is what
            // makes `poll`ing an epoll fd work at all.
            FdKind::Epoll { .. } => self.epoll_ready(fd) > 0,
            // The whole point of an eventfd: readable exactly when the counter is
            // non-zero, which is what an epoll loop parks on to be woken.
            FdKind::EventFd { ev } => eventfd::readable(ev),
            // A timerfd is readable once it has expired - what an epoll loop parks on.
            FdKind::TimerFd { tf } => timerfd::readable(tf),
            _ => false,
        }
    }

    /// POLLOUT readiness for `fd`: a stream socket is writable while its tx ring
    /// has space (or its readers closed); regular/console/datagram fds always are.
    pub fn pollout_ready(&self, fd: i64) -> bool {
        let Some(slot) = usize_fd(fd) else {
            return false;
        };
        match self.fds[slot] {
            FdKind::SockConn { tx, .. } | FdKind::InetConn { tx, .. } => {
                pipe::has_space(tx as usize) || pipe::readers(tx as usize) == 0
            }
            FdKind::Pipe { idx, writer: true } => {
                pipe::has_space(idx as usize) || pipe::readers(idx as usize) == 0
            }
            FdKind::Console(1)
            | FdKind::Console(2)
            | FdKind::Vfs { .. }
            | FdKind::Null
            | FdKind::Zero
            | FdKind::InetDgram { bound: true, .. }
            | FdKind::InetUdpRemote { .. }
            | FdKind::InetTcpRemote { .. } => true,
            FdKind::EventFd { ev } => eventfd::writable(ev),
            _ => false,
        }
    }
}

/// The remote datapath's local IPv4 address (rheo-net N4b), or `0.0.0.0` with no
/// bridge installed.
fn local_ipv4() -> [u8; 4] {
    match svc::socket_ops() {
        Some(o) => (o.local_ip)(),
        None => [0; 4],
    }
}

/// Parse a `sockaddr_un` at `addr_va` (`addrlen` bytes) into a registry key
/// written to `out`, returning its length (docs/LINUX-COMPAT.md L8). A pathname
/// key is the `sun_path` up to its first NUL; an **abstract** name (leading NUL)
/// is taken verbatim (the leading NUL is part of the key). `None` on a non-UNIX
/// family or an empty/oversized name.
fn read_sun_key(addr_va: u64, addrlen: u64, out: &mut [u8; NAME_MAX]) -> Option<usize> {
    let len = addrlen as usize;
    if addr_va == 0 || len < 2 {
        return None;
    }
    // SAFETY: caller-provided sockaddr of `len` bytes in the active cell.
    let fam = unsafe { (addr_va as *const u16).read_unaligned() };
    if fam as u64 != AF_UNIX {
        return None;
    }
    let path_len = (len - 2).min(NAME_MAX);
    if path_len == 0 {
        return None;
    }
    // SAFETY: `sun_path` immediately follows the 2-byte family field.
    let src = unsafe { core::slice::from_raw_parts((addr_va + 2) as *const u8, path_len) };
    let key_len = if src[0] == 0 {
        path_len // abstract namespace: verbatim, leading NUL kept
    } else {
        src.iter().position(|&b| b == 0).unwrap_or(path_len)
    };
    if key_len == 0 {
        return None;
    }
    out[..key_len].copy_from_slice(&src[..key_len]);
    Some(key_len)
}

/// Parse a `sockaddr_in` (AF_INET) or `sockaddr_in6` (AF_INET6) at `addr_va`
/// (`addrlen` bytes) into `(is_v6, port, dest_is_loopback)` (docs/LINUX-COMPAT.md
/// L8-INET). `port` is host-order; `dest_is_loopback` is true for 127.0.0.0/8 or
/// ::1 (and for the wildcard 0.0.0.0 / ::, treated as loopback for a local
/// connect). `None` on an unknown family or a short buffer.
///
/// Layout: `sin_family` u16 @0, `sin_port` big-endian u16 @2, then the address
/// (v4: 4 bytes @4; v6: 16 bytes @8 after a 4-byte flowinfo).
///
/// The third tuple element is the **IPv4 octets** (all zero for a v6 address) -
/// what the rheo-net N4b remote datapath needs to address the peer.
fn read_inaddr(addr_va: u64, addrlen: u64) -> Option<(bool, u16, [u8; 4], bool)> {
    let len = addrlen as usize;
    if addr_va == 0 || len < 4 {
        return None;
    }
    // SAFETY: caller-provided sockaddr of `len` bytes in the active cell.
    let fam = unsafe { (addr_va as *const u16).read_unaligned() } as u64;
    // SAFETY: the port immediately follows the 2-byte family (big-endian).
    let port = u16::from_be(unsafe { ((addr_va + 2) as *const u16).read_unaligned() });
    match fam {
        AF_INET => {
            if len < 8 {
                return None;
            }
            // SAFETY: `sin_addr` (4 bytes) at offset 4.
            let a = unsafe { core::slice::from_raw_parts((addr_va + 4) as *const u8, 4) };
            let oct = [a[0], a[1], a[2], a[3]];
            let is_local = inetsock::is_loopback_v4(oct) || oct == [0, 0, 0, 0];
            Some((false, port, oct, is_local))
        }
        AF_INET6 => {
            if len < 24 {
                return None;
            }
            // SAFETY: `sin6_addr` (16 bytes) at offset 8.
            let a = unsafe { core::slice::from_raw_parts((addr_va + 8) as *const u8, 16) };
            let mut oct = [0u8; 16];
            oct.copy_from_slice(a);
            let is_local = inetsock::is_loopback_v6(oct) || oct == [0u8; 16];
            Some((true, port, [0; 4], is_local))
        }
        _ => None,
    }
}

/// Write a loopback `sockaddr_in`/`sockaddr_in6` for `(v6, port)` into `addr_va`,
/// setting `*addrlen_va` to the struct size (docs/LINUX-COMPAT.md L8-INET). The
/// caller passes a `sockaddr_storage`-sized buffer (glibc always does), so the
/// full 16/28 bytes are written.
fn write_inaddr(addr_va: u64, addrlen_va: u64, v6: bool, port: u16) {
    if addr_va == 0 {
        return;
    }
    let pbe = port.to_be();
    // SAFETY: caller-provided sockaddr buffer (>= 28 bytes) + socklen_t out-param.
    unsafe {
        if v6 {
            core::ptr::write_bytes(addr_va as *mut u8, 0, 28);
            (addr_va as *mut u16).write_unaligned(inetsock::AF_INET6 as u16);
            ((addr_va + 2) as *mut u16).write_unaligned(pbe);
            // sin6_addr = ::1 at offset 8 (15 zero bytes then 1).
            *((addr_va + 8 + 15) as *mut u8) = 1;
            if addrlen_va != 0 {
                (addrlen_va as *mut u32).write(28);
            }
        } else {
            core::ptr::write_bytes(addr_va as *mut u8, 0, 16);
            (addr_va as *mut u16).write_unaligned(inetsock::AF_INET as u16);
            ((addr_va + 2) as *mut u16).write_unaligned(pbe);
            // sin_addr = 127.0.0.1 at offset 4.
            let ip = [127u8, 0, 0, 1];
            core::ptr::copy_nonoverlapping(ip.as_ptr(), (addr_va + 4) as *mut u8, 4);
            if addrlen_va != 0 {
                (addrlen_va as *mut u32).write(16);
            }
        }
    }
}

/// Write a `sockaddr_in` for a **real** IPv4 address + port (rheo-net N4b): the
/// remote datapath's peers are not loopback, so `getsockname`/`getpeername`/
/// `recvfrom` on a remote socket report the genuine address rather than 127.0.0.1.
fn write_inaddr_v4(addr_va: u64, addrlen_va: u64, ip: [u8; 4], port: u16) {
    if addr_va == 0 {
        return;
    }
    let pbe = port.to_be();
    // SAFETY: caller-provided sockaddr buffer (>= 16 bytes) + socklen_t out-param.
    unsafe {
        core::ptr::write_bytes(addr_va as *mut u8, 0, 16);
        (addr_va as *mut u16).write_unaligned(inetsock::AF_INET as u16);
        ((addr_va + 2) as *mut u16).write_unaligned(pbe);
        core::ptr::copy_nonoverlapping(ip.as_ptr(), (addr_va + 4) as *mut u8, 4);
        if addrlen_va != 0 {
            (addrlen_va as *mut u32).write(16);
        }
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
