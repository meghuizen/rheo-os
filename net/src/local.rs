//! `net::local` - the **local fast path** (docs/NETSTACK.md §2, "skip parts of
//! the stack"). A zero-copy, cell-to-cell transport that bypasses IP/Ethernet
//! entirely: it is a thin typed API over `librheo::ipc::Channel` (a shared
//! cross-cell queue pair) plus sealed-grant buffer passing (the dmabuf
//! equivalent). This is the native peer of the Linux-personality AF_UNIX socket
//! (`kernel/src/linux/unixsock.rs`) - both are the "local, skip the IP stack"
//! transport, one for native cells and one for unmodified Linux binaries.
//!
//! The **datapath selector** ([`select`]) is the load-bearing mechanism: a
//! connection chooses [`Datapath::Local`] (a same-host peer, zero-copy IPC) vs
//! [`Datapath::Wire`] (the full IP stack) *at connect time*. For N1d the wire
//! side is a stub ([`Datapath::Wire`] is returned but a wire connect is a later
//! phase); the working, proven path is `local`.

use librheo::ipc::{self, Channel, SharedBuffer};
use librheo::mem::Grant;
use librheo::sys::{CqEntry, SqEntry};

use crate::ip::IpAddr;

/// Which datapath a connection uses - the "skip parts of the stack" choice.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Datapath {
    /// A same-host peer: zero-copy cross-cell IPC, no IP/Ethernet.
    Local,
    /// A remote address: the full wire (IP) stack.
    Wire,
}

/// A connection target. `Local` names a same-host peer reached over the cell's
/// cap-bundle channel; `Remote` an IP address that must traverse the wire.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Target {
    /// The same-host peer wired into this cell's channel.
    Local,
    /// A remote endpoint (IP), routed over the wire stack.
    Remote(IpAddr),
}

/// Choose the datapath for `target` (docs/NETSTACK.md §2): a local peer takes the
/// zero-copy IPC fast path; a remote address takes the wire stack. This is the
/// single decision point a `connect` consults.
pub fn select(target: &Target) -> Datapath {
    match target {
        Target::Local => Datapath::Local,
        Target::Remote(_) => Datapath::Wire,
    }
}

/// A local (same-host) connection - the zero-copy cell-to-cell transport
/// (docs/NETSTACK.md §2). A thin typed wrapper over `librheo::ipc::Channel`: the
/// two ends live in two cells over one shared ring region, and a payload can be
/// handed over as a **sealed buffer grant** so the peer reads the *same frames*
/// with no copy.
pub struct LocalStream {
    ch: Channel,
}

impl LocalStream {
    /// The client (initiator) end of the local connection wired into this cell.
    /// `None` if no channel is wired or this end is the acceptor.
    pub fn connect() -> Option<LocalStream> {
        let ch = Channel::open()?;
        if ch.is_client() {
            Some(LocalStream { ch })
        } else {
            None
        }
    }

    /// The server (acceptor) end of the local connection wired into this cell.
    /// `None` if no channel is wired or this end is the initiator.
    pub fn accept() -> Option<LocalStream> {
        let ch = Channel::open()?;
        if ch.is_client() {
            None
        } else {
            Some(LocalStream { ch })
        }
    }

    /// This connection always uses the local zero-copy datapath.
    pub fn datapath(&self) -> Datapath {
        Datapath::Local
    }

    /// Whether this end is the initiator (client).
    pub fn is_client(&self) -> bool {
        self.ch.is_client()
    }

    /// The underlying channel (escape hatch for advanced use).
    pub fn channel(&self) -> &Channel {
        &self.ch
    }

    // ---- client side ----

    /// Send an inline message (up to 24 bytes) to the peer, tagged `user_data`.
    pub fn send(&self, opcode: u8, user_data: u64, payload: &[u8]) -> bool {
        self.ch.send(opcode, user_data, payload)
    }

    /// Block until the peer's completion arrives (cooperative hand-off).
    pub fn await_completion(&self) -> CqEntry {
        self.ch.await_completion()
    }

    /// Hand the CPU to the peer cell.
    pub fn switch_to_peer(&self) {
        self.ch.switch_to_peer();
    }

    // ---- server side ----

    /// Block until a message arrives from the peer.
    pub fn recv(&self) -> SqEntry {
        self.ch.recv()
    }

    /// Push a completion back to the peer (`result` carries a value - e.g. an ack
    /// or a computed checksum of a shared buffer).
    pub fn complete(&self, user_data: u64, status: u32, result: u32) -> bool {
        self.ch.complete(user_data, status, result)
    }

    // ---- zero-copy buffer passing ----

    /// Delegate a **sealed** [`Grant`] to the peer cell zero-copy: the kernel maps
    /// the same frames read-only into the peer and mints a capability there. The
    /// returned [`SharedBuffer`] carries the peer VA + cap; hand it over with
    /// [`send`](Self::send) so the peer can read the buffer with no copy.
    pub fn share(grant: &Grant) -> Option<SharedBuffer> {
        ipc::share(grant)
    }

    /// View a peer's delegated buffer as a read-only byte slice - the receiving
    /// end of [`share`](Self::share), zero-copy (the frames are the sender's own).
    ///
    /// # Safety
    /// `peer_va` must be the address the kernel reported for a buffer shared *to*
    /// this cell, and `[peer_va, peer_va+len)` must lie within that shared grant.
    pub unsafe fn recv_buffer(peer_va: u64, len: usize) -> &'static [u8] {
        unsafe { ipc::recv_buffer(peer_va, len) }
    }
}
