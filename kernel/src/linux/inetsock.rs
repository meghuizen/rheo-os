//! AF_INET / AF_INET6 **loopback** sockets for the Linux personality
//! (docs/LINUX-COMPAT.md L8-INET, docs/NETSTACK.md). Like AF_UNIX (`unixsock`),
//! pipes, fds, threads and processes, this adds **no kernel object**
//! (docs/LINUX-COMPAT.md 1): INET sockets are per-cell fds (`linux::fd`) and the
//! byte transport reuses the L6 cross-cell ring buffer (`linux::pipe`).
//!
//! ## Loopback-only scope (honest)
//! The kernel is **allocation-free**; the native transports (`net::tcp` /
//! `net::udp`) are `no_std`+**alloc** userspace crates and cannot be linked
//! kernel-resident. For **loopback** (127.0.0.0/8, ::1) a TCP connection between
//! two local endpoints reduces to a **reliable, in-order byte stream** - exactly
//! what the L6 ring pair already provides (as it does for AF_UNIX SOCK_STREAM) -
//! and UDP to an in-order **datagram queue**. So this module implements INET
//! sockets over loopback deterministically and network-free, keying the address
//! namespace by `(is_v6, port)`.
//!
//! **This module is still loopback-only, but the personality is not.** The
//! sentence that used to sit here - "a non-loopback destination is refused
//! `ENETUNREACH`" - was true when it was written and stopped being true at
//! rheo-net N4b: `linux::fd` now forwards every non-loopback operation to the
//! registered `svc::SocketOps` bridge, which drives the full `net::tcp` /
//! `net::udp` machinery over virtio-net from *outside* the kernel
//! (docs/NETSTACK.md 18). `ENETUNREACH` is what remains when **no** bridge is
//! registered, or for a non-loopback **IPv6** destination (the N4b datapath is
//! IPv4). Nothing here changed; the routing decision moved one level up.
//!
//! ## What lives here
//! Two per-personality synthesized registries (fixed statics, like the pipe /
//! unixsock tables): a **stream listener** table (port -> accept backlog of the
//! server ring ends + the connecting client's port) and a **datagram endpoint**
//! table (port -> a bounded queue of `(src_port, bytes)` datagrams). Both are
//! keyed by `(is_v6, port)`; v4 and v6 are separate namespaces (no dual-stack).

use crate::linux::pipe;
use core::ptr::addr_of_mut;

/// Address families (asm-generic `socket.h`, shared by all ISAs).
pub const AF_INET: u64 = 2;
pub const AF_INET6: u64 = 10;

/// Max concurrent INET stream listeners.
const LISTENERS: usize = 8;
/// Per-listener pending-connection backlog.
const BACKLOG: usize = 8;
/// Max concurrent bound UDP endpoints.
const DGRAM_EPS: usize = 8;
/// Per-endpoint queued datagrams.
const DGRAM_QUEUE: usize = 8;
/// Largest UDP payload carried over the loopback datagram queue.
pub const DGRAM_MAX: usize = 2048;

/// True if `octets` (an IPv4 address, network order) is in 127.0.0.0/8.
pub fn is_loopback_v4(octets: [u8; 4]) -> bool {
    octets[0] == 127
}

/// True if `octets` (an IPv6 address, network order) is ::1.
pub fn is_loopback_v6(octets: [u8; 16]) -> bool {
    octets[..15].iter().all(|&b| b == 0) && octets[15] == 1
}

// ------------------------------------------------------------ stream listeners

/// A queued connection's **server** ends plus the connecting client's port
/// (reported by `accept` in the `sockaddr_in`). `rx`/`tx` are `linux::pipe`
/// ring indices.
#[derive(Copy, Clone)]
struct Pending {
    rx: u8,
    tx: u8,
    client_port: u16,
}

struct Listener {
    used: bool,
    refs: u16,
    v6: bool,
    port: u16,
    queue: [Pending; BACKLOG],
    qhead: usize,
    qlen: usize,
}

impl Listener {
    const fn new() -> Listener {
        Listener {
            used: false,
            refs: 0,
            v6: false,
            port: 0,
            queue: [Pending {
                rx: 0,
                tx: 0,
                client_port: 0,
            }; BACKLOG],
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

/// A monotonic ephemeral-port allocator (49152..). Non-secret (a local port
/// number), so the plain counter is fine.
static mut EPHEMERAL: u16 = 49152;

/// Allocate a fresh ephemeral local port for an unbound client `connect`/`sendto`.
pub fn ephemeral_port() -> u16 {
    // SAFETY: single CPU.
    unsafe {
        let p = &mut *addr_of_mut!(EPHEMERAL);
        let v = *p;
        *p = if *p >= 60000 { 49152 } else { *p + 1 };
        v
    }
}

fn listener_at(v6: bool, port: u16) -> Option<usize> {
    listeners()
        .iter()
        .position(|l| l.used && l.v6 == v6 && l.port == port)
}

/// Register `(v6, port)` as a stream listener; returns its index. `None` if the
/// address is already bound (EADDRINUSE) or the table is full.
pub fn register_listener(v6: bool, port: u16) -> Option<u8> {
    if listener_at(v6, port).is_some() {
        return None;
    }
    let ls = listeners();
    let idx = (0..LISTENERS).find(|&i| !ls[i].used)?;
    let l = &mut ls[idx];
    *l = Listener::new();
    l.used = true;
    l.refs = 1;
    l.v6 = v6;
    l.port = port;
    Some(idx as u8)
}

/// The `(v6, port)` a listener is bound to (for `getsockname`).
pub fn listener_addr(lst: u8) -> (bool, u16) {
    let l = &listeners()[lst as usize];
    (l.v6, l.port)
}

/// A new fd references listener `lst` (dup/fork inheritance).
pub fn addref_listener(lst: u8) {
    let l = &mut listeners()[lst as usize];
    l.refs = l.refs.saturating_add(1);
}

/// Drop a listener fd reference; free the slot at zero.
pub fn close_listener(lst: u8) {
    let l = &mut listeners()[lst as usize];
    l.refs = l.refs.saturating_sub(1);
    if l.refs == 0 {
        *l = Listener::new();
    }
}

/// Why a stream `connect` failed.
pub enum ConnectErr {
    /// No listener bound to `(v6, port)` (ECONNREFUSED).
    NoListener,
    /// The listener's backlog is full (EAGAIN).
    Backlog,
    /// The ring table is exhausted (ENFILE).
    NoRing,
}

/// Client-side stream connect to `(v6, port)` from local `client_port`: allocate
/// the two direction rings, queue the **server** ends, and return the **client**
/// ends `(rx, tx)`. Completed by a later `accept`.
pub fn connect_stream(v6: bool, port: u16, client_port: u16) -> Result<(u8, u8), ConnectErr> {
    let lst = listener_at(v6, port).ok_or(ConnectErr::NoListener)?;
    if listeners()[lst].qlen >= BACKLOG {
        return Err(ConnectErr::Backlog);
    }
    let (c2s, s2c) = alloc_ring_pair().ok_or(ConnectErr::NoRing)?;
    let l = &mut listeners()[lst];
    let slot = (l.qhead + l.qlen) % BACKLOG;
    l.queue[slot] = Pending {
        rx: c2s, // server reads client->server
        tx: s2c, // server writes server->client
        client_port,
    };
    l.qlen += 1;
    Ok((s2c, c2s)) // client reads s2c, writes c2s
}

/// Server-side accept on listener `lst`: dequeue a pending connection, returning
/// the **server** ends `(rx, tx)` and the connecting client's port. `None` if the
/// backlog is empty.
pub fn accept_stream(lst: u8) -> Option<(u8, u8, u16)> {
    let l = &mut listeners()[lst as usize];
    if l.qlen == 0 {
        return None;
    }
    let p = l.queue[l.qhead];
    l.qhead = (l.qhead + 1) % BACKLOG;
    l.qlen -= 1;
    Some((p.rx, p.tx, p.client_port))
}

/// True if listener `lst` has a pending connection (poll/epoll readiness).
pub fn listener_has_pending(lst: u8) -> bool {
    listeners()[lst as usize].qlen > 0
}

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

/// Drop both ends of a connected stream socket's rings (its `close`).
pub fn drop_conn(rx: u8, tx: u8) {
    pipe::close_end(rx as usize, false);
    pipe::close_end(tx as usize, true);
}

// ---------------------------------------------------------- datagram endpoints

#[derive(Copy, Clone)]
struct Datagram {
    src_port: u16,
    len: u16,
    buf: [u8; DGRAM_MAX],
}

impl Datagram {
    const fn new() -> Datagram {
        Datagram {
            src_port: 0,
            len: 0,
            buf: [0; DGRAM_MAX],
        }
    }
}

struct DgramEp {
    used: bool,
    refs: u16,
    v6: bool,
    port: u16,
    /// The queued datagrams, **funded when the endpoint binds** rather than resident
    /// (docs/EXECUTION-MODEL.md 9.8).
    ///
    /// This was `[Datagram; 8]` inline, and with a 2 KiB payload each that made `DGRAMS`
    /// **131,520 bytes** of `.bss` - eight endpoints' worth of queue held whether or not
    /// a single UDP socket was ever bound. Almost no boot binds one. The queue is a real
    /// resource, so the cell that binds the endpoint pays for it while it is bound.
    q: crate::mm::kmeta::Funded<Datagram>,
    qhead: usize,
    qlen: usize,
}

impl DgramEp {
    const fn new() -> DgramEp {
        DgramEp {
            used: false,
            refs: 0,
            v6: false,
            port: 0,
            q: crate::mm::kmeta::Funded::new(),
            qhead: 0,
            qlen: 0,
        }
    }
}

static mut DGRAMS: [DgramEp; DGRAM_EPS] = [const { DgramEp::new() }; DGRAM_EPS];

fn dgrams() -> &'static mut [DgramEp; DGRAM_EPS] {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut *addr_of_mut!(DGRAMS) }
}

fn dgram_at(v6: bool, port: u16) -> Option<usize> {
    dgrams()
        .iter()
        .position(|e| e.used && e.v6 == v6 && e.port == port)
}

/// Register a UDP endpoint on `(v6, port)` (port 0 = pick an ephemeral one);
/// returns its index. `None` if the address is already bound or the table is full.
pub fn register_dgram(v6: bool, port: u16) -> Option<u8> {
    let port = if port == 0 { ephemeral_port() } else { port };
    if dgram_at(v6, port).is_some() {
        return None;
    }
    let es = dgrams();
    let idx = (0..DGRAM_EPS).find(|&i| !es[i].used)?;
    let e = &mut es[idx];
    // The slot is free, so its queue was released; fund a fresh one, charged to the cell
    // that is binding. A cell that cannot afford the queue cannot have the endpoint -
    // `None` here is the caller's existing "no endpoint available" answer.
    e.q.set_owner(crate::mm::kmeta::Owner::cell(crate::user::current_index()));
    if !e.q.reserve(DGRAM_QUEUE) {
        e.q.release();
        return None;
    }
    // SAFETY: single CPU, synchronous trap.
    unsafe { *core::ptr::addr_of_mut!(QUEUES_FUNDED) += 1 };
    e.qhead = 0;
    e.qlen = 0;
    e.used = true;
    e.refs = 1;
    e.v6 = v6;
    e.port = port;
    Some(idx as u8)
}

/// Datagram queues funded since boot - the witness that the endpoint path runs at all.
///
/// Not cleared by `reset`: a harness resets at the *start* of a run, so a per-run counter
/// reports only the last phase (the lesson this tree relearned four times).
static mut QUEUES_FUNDED: u64 = 0;

/// Queues released. Paired with [`QUEUES_FUNDED`] on purpose: a leak here **strands**
/// the frames - `close_dgram` overwrites the descriptor that named them - so the table
/// cannot see it and a `frames_held()` witness reports zero for a real leak. Counting
/// both ends of the pair is what makes the release observable at all.
static mut QUEUES_RELEASED: u64 = 0;

/// (queues funded, queues released) since boot. Equal once every endpoint is closed.
pub fn queue_counters() -> (u64, u64) {
    // SAFETY: reads.
    unsafe {
        (
            *core::ptr::addr_of!(QUEUES_FUNDED),
            *core::ptr::addr_of!(QUEUES_RELEASED),
        )
    }
}

/// Frames every bound datagram endpoint's queue holds. **0 once every endpoint is
/// closed** - the release-path property.
pub fn dgram_frames() -> usize {
    dgrams().iter().map(|e| e.q.frames_held()).sum()
}

/// The `(v6, port)` an endpoint is bound to.
pub fn dgram_addr(ep: u8) -> (bool, u16) {
    let e = &dgrams()[ep as usize];
    (e.v6, e.port)
}

/// A new fd references endpoint `ep` (dup/fork inheritance).
pub fn addref_dgram(ep: u8) {
    let e = &mut dgrams()[ep as usize];
    e.refs = e.refs.saturating_add(1);
}

/// Drop an endpoint fd reference; free the slot at zero.
pub fn close_dgram(ep: u8) {
    let e = &mut dgrams()[ep as usize];
    e.refs = e.refs.saturating_sub(1);
    if e.refs == 0 {
        // Release before overwriting: the queue holds frames now, and assigning a fresh
        // `DgramEp` over the descriptor would strand them with no drop glue to notice.
        if e.q.frames_held() > 0 {
            // SAFETY: single CPU, synchronous trap.
            unsafe { *core::ptr::addr_of_mut!(QUEUES_RELEASED) += 1 };
        }
        e.q.release();
        *e = DgramEp::new();
    }
}

/// What a loopback datagram send did. The three cases are **not** the same
/// answer to the caller (docs/ENGINEERING.md 7): a full queue is a drop, which UDP
/// permits and which a sender may legitimately be told succeeded; **no endpoint at
/// all** is a delivery that can never happen, and reporting success for it is a
/// lie that surfaces far from its cause (it is what made glibc's resolver, aimed at
/// its built-in fallback nameserver `127.0.0.1:53`, fail confusingly).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum DgramSend {
    /// Queued at the destination endpoint.
    Delivered,
    /// A bound endpoint exists but its queue was full: dropped, as UDP permits.
    Dropped,
    /// Nothing is bound at `(v6, dst_port)`: no reader exists, now or later.
    NoEndpoint,
}

/// Send `bytes` (a datagram) from `src_port` to the endpoint bound at
/// `(v6, dst_port)`. `bytes` is truncated to `DGRAM_MAX`. See [`DgramSend`] for
/// why "nothing is bound there" is reported distinctly from "dropped".
pub fn send_dgram(v6: bool, dst_port: u16, src_port: u16, bytes: &[u8]) -> DgramSend {
    let Some(ep) = dgram_at(v6, dst_port) else {
        return DgramSend::NoEndpoint;
    };
    let e = &mut dgrams()[ep];
    if e.qlen >= DGRAM_QUEUE {
        return DgramSend::Dropped;
    }
    let slot = (e.qhead + e.qlen) % DGRAM_QUEUE;
    let n = bytes.len().min(DGRAM_MAX);
    let Some(d) = e.q.get_mut(slot) else {
        return DgramSend::Dropped;
    };
    d.src_port = src_port;
    d.len = n as u16;
    d.buf[..n].copy_from_slice(&bytes[..n]);
    e.qlen += 1;
    DgramSend::Delivered
}

/// True if endpoint `ep` has a queued datagram (poll/epoll readiness).
pub fn dgram_has_data(ep: u8) -> bool {
    dgrams()[ep as usize].qlen > 0
}

/// Receive one datagram from endpoint `ep` into `out`; returns `(src_port, n)`
/// copied (a datagram is consumed whole, truncated to `out.len()`). `None` if the
/// queue is empty.
pub fn recv_dgram(ep: u8, out: &mut [u8]) -> Option<(u16, usize)> {
    let e = &mut dgrams()[ep as usize];
    if e.qlen == 0 {
        return None;
    }
    let Some(d) = e.q.get(e.qhead) else {
        return None;
    };
    e.qhead = (e.qhead + 1) % DGRAM_QUEUE;
    e.qlen -= 1;
    let n = (d.len as usize).min(out.len());
    out[..n].copy_from_slice(&d.buf[..n]);
    Some((d.src_port, n))
}

/// Clear both registries (called from `linux::reset`).
pub fn reset() {
    for l in listeners().iter_mut() {
        *l = Listener::new();
    }
    for e in dgrams().iter_mut() {
        // Release before overwriting - the queue is frames now (the S1' rule).
        if e.q.frames_held() > 0 {
            // SAFETY: between runs.
            unsafe { *core::ptr::addr_of_mut!(QUEUES_RELEASED) += 1 };
        }
        e.q.release();
        *e = DgramEp::new();
    }
    // SAFETY: single CPU, between runs.
    unsafe {
        *addr_of_mut!(EPHEMERAL) = 49152;
    }
}
