//! Cross-cell pipes for the Linux personality (docs/LINUX-COMPAT.md L6). Unlike
//! the L3 single-process pipe (which lived per-cell in the fd table), an L6 pipe
//! is a **global** bounded ring buffer so the two ends can be held by different
//! cells after `fork` - the classic shell pipeline `a | b`. It adds no kernel
//! object: the buffers are per-personality synthesized state, exactly like the
//! fd table (docs/LINUX-COMPAT.md 1). A cell's `FdKind::Pipe { idx, writer }`
//! indexes this table.
//!
//! This module is pure data: read/write are **non-blocking** and report
//! `WouldBlock`/`Epipe`; the cooperative blocking + cross-cell wake decision is
//! the process scheduler's (`linux::proc`), which parks a reader/writer and
//! re-evaluates the condition here when it reschedules. Because a cell yields
//! the CPU only at a syscall boundary and a writer must eventually block or
//! exit, a parked reader always gets its turn (docs/LINUX-COMPAT.md L6).

use core::ptr::addr_of_mut;

/// Number of live pipes across all cells. Enough for several concurrent shell
/// pipelines (each stage boundary is one pipe).
const PIPE_COUNT: usize = 16;
/// Per-pipe ring capacity. Large enough that the L6 suite's pipeline payloads
/// (`seq 1 100 | wc -l`, `ls | sort | head`) never fill it, so a writer rarely
/// blocks; blocking write is still handled for correctness.
/// Bytes one pipe's ring buffer holds. Reported verbatim by
/// `fcntl(F_GETPIPE_SZ)` - a real answer, not a guess.
pub const PIPE_CAP: usize = 64 * 1024;

struct Pipe {
    buf: [u8; PIPE_CAP],
    head: usize,
    len: usize,
    readers: u8,
    writers: u8,
    used: bool,
}

impl Pipe {
    const fn new() -> Pipe {
        Pipe {
            buf: [0; PIPE_CAP],
            head: 0,
            len: 0,
            readers: 0,
            writers: 0,
            used: false,
        }
    }
}

static mut PIPES: [Pipe; PIPE_COUNT] = [const { Pipe::new() }; PIPE_COUNT];

fn pipes() -> &'static mut [Pipe; PIPE_COUNT] {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut *addr_of_mut!(PIPES) }
}

/// Clear every pipe (called from `linux::reset`).
pub fn reset() {
    for p in pipes().iter_mut() {
        *p = Pipe::new();
    }
}

/// Allocate a fresh pipe with one read end and one write end; None if the table
/// is full.
pub fn alloc() -> Option<usize> {
    let ps = pipes();
    let idx = (0..PIPE_COUNT).find(|&i| !ps[i].used)?;
    ps[idx] = Pipe::new();
    ps[idx].used = true;
    ps[idx].readers = 1;
    ps[idx].writers = 1;
    Some(idx)
}

/// Add one end (a `dup`/`fork` of an existing pipe fd).
pub fn add_end(idx: usize, writer: bool) {
    let p = &mut pipes()[idx];
    if writer {
        p.writers = p.writers.saturating_add(1);
    } else {
        p.readers = p.readers.saturating_add(1);
    }
}

/// Drop one end (`close`, or a cell's fd-table teardown on exit). The slot is
/// reclaimed when both ends reach zero.
pub fn close_end(idx: usize, writer: bool) {
    let p = &mut pipes()[idx];
    if writer {
        p.writers = p.writers.saturating_sub(1);
    } else {
        p.readers = p.readers.saturating_sub(1);
    }
    if p.readers == 0 && p.writers == 0 {
        p.used = false;
        p.len = 0;
        p.head = 0;
    }
}

/// True if the pipe currently holds unread bytes.
pub fn has_data(idx: usize) -> bool {
    pipes()[idx].len > 0
}

/// True if the pipe has room for at least one more byte.
pub fn has_space(idx: usize) -> bool {
    pipes()[idx].len < PIPE_CAP
}

pub fn writers(idx: usize) -> u8 {
    pipes()[idx].writers
}

pub fn readers(idx: usize) -> u8 {
    pipes()[idx].readers
}

/// Non-blocking read outcome.
pub enum ReadNb {
    /// Read `n` bytes; `n == 0` means end-of-file (all write ends closed).
    Done(i64),
    /// Empty but write ends remain open - the caller may block.
    WouldBlock,
}

/// Read up to `count` bytes from pipe `idx` into `buf_va` (a VA in the active
/// cell). Drains the ring; EOF (Done(0)) only when the buffer is empty and no
/// write end remains.
pub fn read(idx: usize, buf_va: u64, count: u64) -> ReadNb {
    let p = &mut pipes()[idx];
    if p.len == 0 {
        return if p.writers == 0 {
            ReadNb::Done(0)
        } else {
            ReadNb::WouldBlock
        };
    }
    let n = p.len.min(count as usize);
    // Through `uaccess`, which bounds the destination, makes it present, and resolves
    // copy-on-write before the store. This used to build a slice straight from the
    // cell-supplied VA with no check of any kind - and a pipe read into a buffer the
    // reader has not touched since a fork is exactly the shape that faults at a kernel
    // PC (docs/ENGINEERING.md 11).
    // SAFETY: we are servicing this cell's synchronous trap, so nothing else holds a
    // reference to the range.
    let Some(buf) = (unsafe { crate::uaccess::slice(buf_va, n) }) else {
        return ReadNb::Done(-crate::linux::errno::EFAULT);
    };
    for b in buf.iter_mut() {
        *b = p.buf[p.head];
        p.head = (p.head + 1) % PIPE_CAP;
    }
    p.len -= n;
    ReadNb::Done(n as i64)
}

/// Non-blocking write outcome.
pub enum WriteNb {
    /// Wrote `n` bytes (`n >= 1`).
    Done(i64),
    /// Buffer full but read ends remain - the caller may block.
    WouldBlock,
    /// All read ends closed - the caller raises SIGPIPE / returns -EPIPE.
    Epipe,
}

/// Write up to `count` bytes from `buf_va` into pipe `idx`. A partial write is
/// allowed (the caller's libc loops); WouldBlock only when the buffer is full.
pub fn write(idx: usize, buf_va: u64, count: u64) -> WriteNb {
    let p = &mut pipes()[idx];
    if p.readers == 0 {
        return WriteNb::Epipe;
    }
    let free = PIPE_CAP - p.len;
    if free == 0 {
        return WriteNb::WouldBlock;
    }
    let n = free.min(count as usize);
    // Through `uaccess`, as the read side: bounded, present, and read only once it is.
    if crate::uaccess::buf(buf_va, n).is_none() {
        return WriteNb::Done(-crate::linux::errno::EFAULT);
    }
    // SAFETY: `uaccess::buf` validated `[buf_va, buf_va+n)` readable in the active
    // cell, and we are servicing that cell's synchronous trap.
    let buf = unsafe { core::slice::from_raw_parts(buf_va as *const u8, n) };
    for &b in buf {
        let tail = (p.head + p.len) % PIPE_CAP;
        p.buf[tail] = b;
        p.len += 1;
    }
    WriteNb::Done(n as i64)
}
