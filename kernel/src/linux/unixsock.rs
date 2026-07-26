//! AF_UNIX (Unix domain) sockets for the Linux personality (docs/LINUX-COMPAT.md
//! L8). Like pipes, fds, threads, and processes, this adds **no kernel object**
//! (docs/LINUX-COMPAT.md 1): sockets are per-cell fds (`linux::fd`) and the byte
//! transport reuses the L6 cross-cell ring buffer (`linux::pipe`) - a SOCK_STREAM
//! connection is **two rings, one per direction**, exactly the shape a bidirectional
//! pipe-pair has. This module owns only the **global name registry + listener
//! accept queues**, per-personality synthesized state just like the pipe table.
//!
//! A `bind` registers a pathname (or abstract `\0`-prefixed name) in a listener
//! slot; `connect` looks it up, allocates the two direction rings, and queues a
//! pending connection whose server ends `accept` hands out. `socketpair` allocates
//! the two rings directly with no name. The ownership of each ring end is implicit
//! in the reader/writer refcount `pipe::alloc` already tracks (one reader, one
//! writer), so cross-cell blocking + wake reuse the L6 pipe scheduler unchanged.
//!
//! **SCM_RIGHTS fd-passing is deferred** (documented, docs/LINUX-COMPAT.md L8): the
//! seam is `sendmsg`'s `msg_control` - passing an fd would dup it into the peer
//! cell's fd table over the connection. It is not faked here.

use crate::linux::pipe;
use core::ptr::addr_of_mut;

/// Address family + socket types (asm-generic `socket.h`, shared by all ISAs).
pub const AF_UNIX: u64 = 1;
pub const SOCK_STREAM: u64 = 1;
pub const SOCK_DGRAM: u64 = 2;
/// The type argument carries SOCK_CLOEXEC/SOCK_NONBLOCK in the high bits; the
/// socket type is the low byte.
pub const SOCK_TYPE_MASK: u64 = 0xff;

/// Max concurrent bound/listening AF_UNIX names.
const LISTENERS: usize = 8;
/// Per-listener pending-connection backlog.
const BACKLOG: usize = 8;
/// Longest bound name (`sun_path` is 108; abstract names include a leading NUL).
pub const NAME_MAX: usize = 108;

/// The server-side ends of a queued connection: read `rx` (client->server),
/// write `tx` (server->client). These are `linux::pipe` ring indices.
#[derive(Copy, Clone)]
struct Pending {
    rx: u8,
    tx: u8,
}

struct Listener {
    used: bool,
    /// fd references (a bound fd, plus fork/dup copies). Freed at zero.
    refs: u16,
    name: [u8; NAME_MAX],
    name_len: usize,
    queue: [Pending; BACKLOG],
    qhead: usize,
    qlen: usize,
}

impl Listener {
    const fn new() -> Listener {
        Listener {
            used: false,
            refs: 0,
            name: [0; NAME_MAX],
            name_len: 0,
            queue: [Pending { rx: 0, tx: 0 }; BACKLOG],
            qhead: 0,
            qlen: 0,
        }
    }
}

static mut LISTENERS_TBL: [Listener; LISTENERS] = [const { Listener::new() }; LISTENERS];

fn listeners() -> &'static mut [Listener; LISTENERS] {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut *addr_of_mut!(LISTENERS_TBL) }
}

/// Clear the registry (called from `linux::reset`).
pub fn reset() {
    for l in listeners().iter_mut() {
        *l = Listener::new();
    }
}

fn name_eq(l: &Listener, key: &[u8]) -> bool {
    l.used && l.name_len == key.len() && l.name[..l.name_len] == *key
}

/// Bind `key` to a fresh listener; returns its index. `None` if the name is
/// already bound or the table is full.
pub fn bind(key: &[u8]) -> Option<u8> {
    if key.is_empty() || key.len() > NAME_MAX {
        return None;
    }
    let ls = listeners();
    if ls.iter().any(|l| name_eq(l, key)) {
        return None;
    }
    let idx = (0..LISTENERS).find(|&i| !ls[i].used)?;
    let l = &mut ls[idx];
    *l = Listener::new();
    l.used = true;
    l.refs = 1;
    l.name[..key.len()].copy_from_slice(key);
    l.name_len = key.len();
    Some(idx as u8)
}

/// Look up a bound listener by name.
pub fn lookup(key: &[u8]) -> Option<u8> {
    listeners()
        .iter()
        .position(|l| name_eq(l, key))
        .map(|i| i as u8)
}

/// The bound name of listener `lst` (for `getsockname`).
pub fn name_of(lst: u8) -> ([u8; NAME_MAX], usize) {
    let l = &listeners()[lst as usize];
    (l.name, l.name_len)
}

/// True if listener `lst` has a pending connection to accept.
pub fn has_pending(lst: u8) -> bool {
    listeners()[lst as usize].qlen > 0
}

/// A new fd references listener `lst` (dup/fork inheritance).
pub fn addref(lst: u8) {
    let l = &mut listeners()[lst as usize];
    l.refs = l.refs.saturating_add(1);
}

/// Drop a listener fd reference; free the slot (and its name) at zero.
pub fn close(lst: u8) {
    let l = &mut listeners()[lst as usize];
    l.refs = l.refs.saturating_sub(1);
    if l.refs == 0 {
        *l = Listener::new();
    }
}

/// Why a `connect` failed.
pub enum ConnectErr {
    /// No listener bound to the name (ECONNREFUSED).
    NoListener,
    /// The listener's backlog is full (EAGAIN).
    Backlog,
    /// The ring table is exhausted (ENFILE).
    NoRing,
}

/// Client-side connect to bound name `key`: allocate the two direction rings,
/// queue the **server** ends on the listener, and return the **client** ends
/// `(rx, tx)`. The queued connection is completed by a later `accept`.
pub fn connect(key: &[u8]) -> Result<(u8, u8), ConnectErr> {
    let lst = lookup(key).ok_or(ConnectErr::NoListener)? as usize;
    if listeners()[lst].qlen >= BACKLOG {
        return Err(ConnectErr::Backlog);
    }
    // c2s: client writes, server reads. s2c: server writes, client reads.
    let (c2s, s2c) = alloc_ring_pair().ok_or(ConnectErr::NoRing)?;
    let l = &mut listeners()[lst];
    let slot = (l.qhead + l.qlen) % BACKLOG;
    l.queue[slot] = Pending { rx: c2s, tx: s2c }; // server reads c2s, writes s2c
    l.qlen += 1;
    Ok((s2c, c2s)) // client reads s2c, writes c2s
}

/// Server-side accept on listener `lst`: dequeue a pending connection and return
/// the **server** ends `(rx, tx)`. `None` if the backlog is empty.
pub fn accept(lst: u8) -> Option<(u8, u8)> {
    let l = &mut listeners()[lst as usize];
    if l.qlen == 0 {
        return None;
    }
    let p = l.queue[l.qhead];
    l.qhead = (l.qhead + 1) % BACKLOG;
    l.qlen -= 1;
    Some((p.rx, p.tx))
}

/// Allocate a connected socketpair's two direction rings. Returns the two
/// sockets' `(rx, tx)` ends: `(a_rx, a_tx, b_rx, b_tx)` where socket A reads what
/// B writes and vice-versa. `None` if the ring table is exhausted.
pub fn socketpair() -> Option<(u8, u8, u8, u8)> {
    // Ring `a`: A reads, B writes. Ring `b`: B reads, A writes.
    let (a, b) = alloc_ring_pair()?;
    Some((a, b, b, a))
}

/// Allocate two fresh rings; `None` (and no leak) if the second cannot be had.
fn alloc_ring_pair() -> Option<(u8, u8)> {
    let a = pipe::alloc()? as u8;
    match pipe::alloc() {
        Some(b) => Some((a, b as u8)),
        None => {
            pipe::close_end(a as usize, false);
            pipe::close_end(a as usize, true);
            None
        }
    }
}

/// Drop both ends of a connected socket's rings (its `close`, and the cleanup
/// path when an fd slot cannot be allocated after a ring pair was reserved).
pub fn drop_conn(rx: u8, tx: u8) {
    pipe::close_end(rx as usize, false);
    pipe::close_end(tx as usize, true);
}
