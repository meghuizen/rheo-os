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
//! **Scope (honest).** The synchronous [`Channel`] is an explicit peer hand-off
//! (`recv`/`await_completion` loop over `switch`) - Phase E's compositor uses it.
//! **Phase J** adds the documented refinement alongside it: [`Channel::split`]
//! yields a fully symmetric async [`AsyncSender`]/[`AsyncReceiver`] that **park
//! on the strand reactor** (docs/LIBRHEO.md Phase J). Two cells' strands then
//! exchange messages without either busy-switching: the in-cell wait is a genuine
//! reactor park (sibling strands run while a strand awaits), and only the
//! cell-boundary hand-off remains a cooperative `switch` under the single-CPU
//! model (a true parallel producer/consumer awaits SMP, task #27 - honest).
//! Spawn-driven connect (a cell spawning its peer and exchanging the channel cap)
//! is Phase F; here the test kernel wires the two cells and their shared channel.

use crate::mem::Grant;
use crate::rt;
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
    chan_va: u64,
    slot: usize,
    count: usize,
}

impl Channel {
    /// Discover and attach this cell's end of the shared channel (slot 0). `None`
    /// if no channel is wired for this cell.
    pub fn open() -> Option<Channel> {
        Channel::open_slot(0)
    }

    /// Discover and attach this cell's channel end at `slot` (docs/NETSTACK.md the
    /// service-cell section, rheo-net N4a). Slot 0 is the Phase E/J channel; a
    /// **service cell** holds one end per client (slots `0..count()`), each a
    /// separate shared ring region with one client cell. `None` if that slot holds
    /// no channel.
    pub fn open_slot(slot: usize) -> Option<Channel> {
        let info = sys::connect_slot(slot)?;
        // SAFETY: `chan_va` is this cell's mapped, kernel-initialised shared ring
        // region for this slot (the peer maps the same frames).
        let qp = unsafe { Qp::attach(info.chan_va as *mut u8) };
        Some(Channel {
            qp,
            cap_id: info.cap_id as u32,
            role: info.role,
            chan_va: info.chan_va,
            slot,
            count: info.count as usize,
        })
    }

    /// How many channel ends this cell holds in total (1 for a Phase E/J cell, N
    /// for a service cell serving N clients). 0 if none are wired.
    pub fn count() -> usize {
        sys::connect_slot(0).map_or(0, |i| i.count as usize)
    }

    /// This end's channel slot.
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// How many channel ends this cell holds, as reported when this end was opened.
    pub fn peer_count(&self) -> usize {
        self.count
    }

    /// Split into a symmetric async [`AsyncSender`] + [`AsyncReceiver`] that
    /// **park on the strand reactor** instead of spinning on
    /// [`switch_to_peer`](Self::switch_to_peer) (docs/LIBRHEO.md Phase J). Binds
    /// this end's ring to the reactor; both halves then drive it. This is the
    /// documented refinement of the synchronous [`Channel`]: two cells' strands
    /// exchange messages without either busy-switching - the in-cell wait is a
    /// genuine reactor park (sibling strands run meanwhile), only the
    /// cell-boundary hand-off stays a cooperative switch under the single-CPU
    /// model. Consumes the channel (the async halves own it from here).
    pub fn split(self) -> (AsyncSender, AsyncReceiver) {
        rt::attach_channel_slot(self.slot, self.chan_va, self.role, self.cap_id);
        (
            AsyncSender { slot: self.slot },
            AsyncReceiver { slot: self.slot },
        )
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

// ------------------------------------------------- symmetric async channel

/// A typed message carried over the async channel (docs/LIBRHEO.md Phase J):
/// an application `tag` (the SPSC entry's `user_data`) and a `val` word. Both
/// travel losslessly in each direction (the SQ payload / the CQ `user_data` +
/// `result`), so the API is symmetric regardless of which end sends.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Message {
    pub tag: u64,
    pub val: u32,
}

/// The sending half of a [`Channel::split`] (docs/LIBRHEO.md Phase J). `send`
/// enqueues on this end's producer ring and parks on the reactor only if the
/// ring is momentarily full - never a busy switch.
pub struct AsyncSender {
    slot: usize,
}

impl AsyncSender {
    /// Send `msg` to the peer cell. Parks (yielding the vcore to sibling
    /// strands) only if the ring is full; the reactor completes it when the peer
    /// drains space.
    pub async fn send(&self, msg: Message) {
        rt::chan_send_on(self.slot, msg.tag, msg.val).await
    }

    /// The channel slot this half drives.
    pub fn slot(&self) -> usize {
        self.slot
    }
}

/// The receiving half of a [`Channel::split`] (docs/LIBRHEO.md Phase J). `recv`
/// always parks on the reactor and is woken by the reactor's channel service -
/// an idle receiver costs no spin, and the wakeup is genuinely reactor-driven.
pub struct AsyncReceiver {
    slot: usize,
}

impl AsyncReceiver {
    /// Receive one [`Message`] from the peer cell, parking on the reactor until
    /// it arrives. While parked the vcore runs the cell's other strands.
    pub async fn recv(&self) -> Message {
        let (tag, val) = rt::chan_recv_on(self.slot).await;
        Message { tag, val }
    }

    /// The channel slot this half drives.
    pub fn slot(&self) -> usize {
        self.slot
    }
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
