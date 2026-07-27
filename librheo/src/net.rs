//! Raw-frame networking (docs/LIBRHEO.md Phase G, docs/NETWORKING.md) - async
//! send/receive of **raw Ethernet frames** over the same submit/complete
//! machinery as `io`: an `OP_NET_*` submission parks a strand on the completion
//! token and the vcore runs other strands until the reactor wakes it.
//!
//! This is the NIC data path - the queue plumbing the kernel owns
//! (docs/NETWORKING.md 1: "NIC queues are the primitive"). Everything above raw
//! frames - ARP/IP/TCP/QUIC/TLS - is a **service / a transport library in a
//! cell** (docs/NETWORKING.md 2), not part of this foundation, and stays
//! deferred: [`connect`]/[`listen`] remain [`Unsupported`] stubs pending a
//! socket object + the blessed transport cell. What is real here is `send`,
//! `recv`/`try_recv`, and `mac` bridged to the kernel's virtio-net driver.
//!
//! Receive is **symmetric with send**: [`recv`] parks the strand until a frame
//! arrives (the kernel idles at WFI on the NIC's RX interrupt where it is wired -
//! docs/NETSTACK.md, the async-receive path / rheo-net N2d), and [`try_recv`] is
//! the non-blocking drain a transport uses to batch a burst.

use crate::rt;
use crate::sys;

/// A 6-byte Ethernet MAC address.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Mac(pub [u8; 6]);

/// A networking error (the transport-level failures a raw frame can hit).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NetError;

/// The error the (still-deferred) socket calls return - IP/TCP is a service.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Unsupported;

/// A socket address (the shape a future socket API takes). Inert today.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SocketAddr {
    pub ip: [u8; 4],
    pub port: u16,
}

fn put_u64(a: &mut [u8; 24], off: usize, v: u64) {
    a[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(a: &mut [u8; 24], off: usize, v: u32) {
    a[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// The NIC's MAC address (async: an `OP_NET_MAC` completion).
pub async fn mac() -> Result<Mac, NetError> {
    let mut m = [0u8; 6];
    let mut a = [0u8; 24];
    put_u64(&mut a, 0, m.as_mut_ptr() as u64);
    let cqe = rt::submit_and_await(sys::OP_NET_MAC, a).await;
    if cqe.status == sys::STATUS_OK && cqe.result == 6 {
        Ok(Mac(m))
    } else {
        Err(NetError)
    }
}

/// Send one raw Ethernet `frame` (`OP_NET_TX`). Returns the byte count.
pub async fn send(frame: &[u8]) -> Result<usize, NetError> {
    let mut a = [0u8; 24];
    put_u64(&mut a, 0, frame.as_ptr() as u64);
    put_u32(&mut a, 8, frame.len() as u32);
    let cqe = rt::submit_and_await(sys::OP_NET_TX, a).await;
    if cqe.status == sys::STATUS_OK {
        Ok(cqe.result as usize)
    } else {
        Err(NetError)
    }
}

/// Receive one raw Ethernet frame into `buf`, **parking** until one arrives
/// (docs/NETSTACK.md, the async-receive path / rheo-net N2d). This is the true
/// async receive: the strand parks on the reactor's network slot, the vcore runs
/// the cell's other strands, and only when they have all parked does the reactor
/// block in the kernel (`SYS_WAIT_NET`) - which idles the CPU at WFI until the
/// NIC's RX interrupt fires, where that interrupt is wired (RISC-V and ARM64
/// today; x86-64 falls back to a bounded kernel poll - docs/NETSTACK.md has the
/// per-ISA table). **One park and one wake per frame**, not a re-poll spin.
///
/// Returns the frame length, or `0` only if the kernel's wait gave up (no NIC
/// installed, or the poll fallback's budget expired). Use [`try_recv`] for a
/// non-blocking drain (batching a burst, or a caller that must not block).
pub async fn recv(buf: &mut [u8]) -> Result<usize, NetError> {
    Ok(rt::recv_frame(buf.as_mut_ptr(), buf.len(), 0).await)
}

/// Like [`recv`], but bounded: park for a frame, giving up after `timeout_ns`
/// nanoseconds and returning `0`. This is the primitive a transport needs for a
/// retransmission timeout - "a frame, or the RTO, whichever comes first". Where
/// both the NIC and the timer interrupt are wired the kernel arms the deadline and
/// halts once, waking on either source, so the wait is still a 0%-CPU park.
pub async fn recv_timeout(buf: &mut [u8], timeout_ns: u64) -> Result<usize, NetError> {
    Ok(rt::recv_frame(buf.as_mut_ptr(), buf.len(), timeout_ns).await)
}

/// Non-blocking drain: try to take one raw Ethernet frame into `buf`
/// (`OP_NET_RX`). Returns the frame length, or `0` if no packet is available -
/// the caller decides whether to retry, batch, or park with [`recv`]. This is the
/// batching primitive (a transport draining a burst of frames per poll must not
/// block on the last one).
pub async fn try_recv(buf: &mut [u8]) -> Result<usize, NetError> {
    let mut a = [0u8; 24];
    put_u64(&mut a, 0, buf.as_mut_ptr() as u64);
    put_u32(&mut a, 8, buf.len() as u32);
    let cqe = rt::submit_and_await(sys::OP_NET_RX, a).await;
    if cqe.status == sys::STATUS_OK {
        Ok(cqe.result as usize)
    } else {
        Err(NetError)
    }
}

/// Would open an async stream socket. [`Unsupported`] - IP/TCP is a service.
pub fn connect(_addr: SocketAddr) -> Result<(), Unsupported> {
    Err(Unsupported)
}

/// Would bind/listen for connections. [`Unsupported`] - IP/TCP is a service.
pub fn listen(_addr: SocketAddr) -> Result<(), Unsupported> {
    Err(Unsupported)
}
