//! Services & IPC (docs/LIBRHEO.md Phase E, docs/IO.md 6): a **cross-cell typed
//! queue pair** plus **buffer-grant passing** - the Wayland-class compositor
//! substrate. IO.md 6: "Cross-cell calls are the same ABI: connect = capability
//! exchange yielding a typed queue pair whose protocol is declared in the IDL."
//!
//! A [`Channel`] is one shared ring region mapped into two cells (the two ends
//! of a connection). The initiator ("client") drives the SQ as producer and the
//! CQ as consumer; the acceptor ("server") drives the reverse. The kernel never
//! touches the ring - the two cells drive the SPSC rings directly over the
//! shared frames, so a message is a pure shared-memory write + an
//! [`switch`](crate::sys::switch) hand-off (the single-CPU cooperative model).
//!
//! A **buffer grant** is passed by delegating a *sealed* [`Grant`](crate::mem::Grant)
//! to the peer ([`share`]): the kernel maps the same frames into the peer
//! read-only and mints a capability there - zero-copy shared memory, the dmabuf
//! equivalent. The client fills+seals a buffer, shares it, sends the handle over
//! the channel; the server reads the same frames with no copy.
//!
//! **Scope (honest).** The channel is synchronous with an explicit peer hand-off
//! (`recv`/`await_completion` loop over `switch`); folding it into the strand
//! reactor as a park-on-channel-completion (a fully symmetric async `Sender<T>`/
//! `Receiver<T>`) is the documented refinement. Spawn-driven connect (a cell
//! spawning its peer and exchanging the channel cap) is Phase F; here the test
//! kernel wires the two cells and their shared channel.

use crate::mem::Grant;
use crate::sys::{self, CqEntry, Qp, SqEntry};

/// Channel role: the initiator (SQ producer, CQ consumer).
pub const ROLE_CLIENT: u64 = 0;
/// Channel role: the acceptor (SQ consumer, CQ producer).
pub const ROLE_SERVER: u64 = 1;

/// A typed cross-cell queue pair: the two ends live in two different cells over
/// one shared ring region (docs/IO.md 6). Obtained with [`Channel::open`], which
/// discovers this cell's end (VA + capability + role) via `SYS_CONNECT`.
pub struct Channel {
    qp: Qp,
    cap_id: u32,
    role: u64,
}

impl Channel {
    /// Discover and attach this cell's end of the shared channel. `None` if no
    /// channel is wired for this cell.
    pub fn open() -> Option<Channel> {
        let info = sys::connect()?;
        // SAFETY: `chan_va` is this cell's mapped, kernel-initialised shared ring
        // region (the peer maps the same frames at the same VA).
        let qp = unsafe { Qp::attach(info.chan_va as *mut u8) };
        Some(Channel {
            qp,
            cap_id: info.cap_id as u32,
            role: info.role,
        })
    }

    /// This end's role (`ROLE_CLIENT` / `ROLE_SERVER`).
    pub fn role(&self) -> u64 {
        self.role
    }
    /// Whether this end is the initiator (client).
    pub fn is_client(&self) -> bool {
        self.role == ROLE_CLIENT
    }

    /// The channel capability id (the kernel-minted QueuePair cap authorising
    /// this end; the connection's IO.md-6 "capability exchange" result).
    pub fn cap_id(&self) -> u32 {
        self.cap_id
    }

    /// Hand the CPU to the peer cell so it can produce/consume. Resumes here once
    /// the peer switches back.
    pub fn switch_to_peer(&self) {
        sys::switch();
    }

    // ---- client side (SQ producer, CQ consumer) ----

    /// Client: send a message to the peer (up to 24 bytes of `payload`, tagged
    /// with `user_data`). `false` if the ring is momentarily full.
    pub fn send(&self, opcode: u8, user_data: u64, payload: &[u8]) -> bool {
        self.qp
            .submit(opcode, 0, self.cap_id, 0, user_data, payload)
    }

    /// Client: pop a completion the server pushed, or `None`.
    pub fn try_reap(&self) -> Option<CqEntry> {
        self.qp.reap()
    }

    /// Client: block until a completion arrives, switching to the peer whenever
    /// none is ready (the cooperative frame-callback await).
    pub fn await_completion(&self) -> CqEntry {
        loop {
            if let Some(c) = self.try_reap() {
                return c;
            }
            self.switch_to_peer();
        }
    }

    // ---- server side (SQ consumer, CQ producer) ----

    /// Server: pop a message from the client, or `None`.
    pub fn try_recv(&self) -> Option<SqEntry> {
        self.qp.sq_pop()
    }

    /// Server: block until a message arrives, switching to the peer whenever none
    /// is ready.
    pub fn recv(&self) -> SqEntry {
        loop {
            if let Some(m) = self.try_recv() {
                return m;
            }
            self.switch_to_peer();
        }
    }

    /// Server: push a completion back to the client (`user_data` echoes the
    /// message, `result` carries a value - e.g. a flip/present acknowledgement).
    /// `false` if the ring is full.
    pub fn complete(&self, user_data: u64, status: u32, result: u32) -> bool {
        self.qp.cq_push(CqEntry {
            flow_id: 0,
            user_data,
            status,
            result,
        })
    }
}

/// A buffer grant delegated to the peer (docs/LIBRHEO.md Phase E): the peer VA at
/// which the *same frames* are mapped read-only, plus the capability minted
/// there. The client sends `peer_va` over the [`Channel`]; the server reads the
/// buffer at `peer_va` with no copy.
#[derive(Copy, Clone)]
pub struct SharedBuffer {
    /// VA in the peer's address space where the shared frames are mapped RO.
    pub peer_va: u64,
    /// The MemoryGrant capability id minted in the peer's table.
    pub peer_cap_id: u32,
}

/// Delegate a **sealed** [`Grant`] to the peer cell (zero-copy buffer passing).
/// The grant must be sealed (immutable = shareable) and its capability carry
/// DELEGATE. `None` if the kernel refuses (unsealed, wrong rights, no peer).
pub fn share(grant: &Grant) -> Option<SharedBuffer> {
    let mut info = sys::ShareInfo {
        peer_va: 0,
        peer_cap_id: 0,
    };
    let r = sys::grant_share(grant.cap_id(), &mut info as *mut sys::ShareInfo as u64);
    if r != 0 {
        return None;
    }
    Some(SharedBuffer {
        peer_va: info.peer_va,
        peer_cap_id: info.peer_cap_id as u32,
    })
}

/// View a peer's delegated buffer as a read-only byte slice - the receiving end
/// of [`share`]. The frames were mapped read-only into this cell by the kernel
/// at `peer_va` when the client shared them, so this is zero-copy.
///
/// # Safety
/// `peer_va` must be the address the kernel reported for a buffer shared *to*
/// this cell, and `[peer_va, peer_va+len)` must lie within the shared grant.
pub unsafe fn recv_buffer(peer_va: u64, len: usize) -> &'static [u8] {
    unsafe { core::slice::from_raw_parts(peer_va as *const u8, len) }
}
