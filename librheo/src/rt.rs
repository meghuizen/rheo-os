//! The async engine (docs/LIBRHEO.md, docs/CONCURRENCY.md). Two pieces:
//!
//! - the **strand executor** (re-exported from `runtime::strand`): a strand is
//!   a stackless `Future` that "blocks" by parking on a token and is woken when
//!   a completion carries that token in `user_data`. Blocking exists only here,
//!   as a park - never a syscall that idles the vcore.
//! - the **reactor**: the cell's side of its queue pair. It owns the mapped
//!   ring + the capability id, submits ops tagged with a strand's token, and on
//!   `run` rings `SYS_DOORBELL`, drains the completion ring, and wakes each
//!   parked strand by the token its completion carries. This closes the
//!   CONCURRENCY.md 1 loop - "one wakeup, N strands resumed" - from userspace.

use alloc::collections::BTreeMap;
use core::future::Future;

use crate::cap::CapSet;
use crate::sys::{self, CqEntry, Qp};

/// Channel role: the initiator (SQ producer, CQ consumer) - mirrors
/// `ipc::ROLE_CLIENT`. Kept as a raw constant here so the spine (`rt`) does not
/// depend on the `full`-gated `ipc` module.
const CHAN_ROLE_CLIENT: u64 = 0;

pub use runtime::strand::{
    JoinHandle, StrandId, complete, has_pending, next_token, park_on, spawn, stats, yield_now,
};

/// The cell's reactor: its queue pair + the capability that authorises it.
pub struct Reactor {
    qp: Qp,
    cap_id: u32,
    /// Completions drained but not yet claimed by their awaiting strand,
    /// keyed by the token (`user_data`) the strand parked on.
    results: BTreeMap<u64, CqEntry>,
    /// A pending console read: `(buf_va, len, token)`. One reader at a time - a
    /// terminal has a single input stream (docs/LIBRHEO.md Phase D). Serviced by
    /// `block_on` when no queue completion is ready, by blocking in the kernel
    /// (`SYS_WAIT_INPUT`) until input arrives - the terminal idle path.
    console_req: Option<(u64, usize, u64)>,
    /// Byte count the last serviced console read returned.
    console_n: usize,
    /// A pending timer: `(deadline_ns, token)` (docs/LIBRHEO.md Phase F). One at
    /// a time (the nearest deadline); serviced by `block_on` when no queue
    /// completion is ready, by arming the kernel's one-shot deadline. Honors
    /// docs/POWER.md - the kernel waits only when a real deadline was requested.
    timer_req: Option<(u64, u64)>,
    /// A pending network receive: `(buf_va, len, timeout_ns, token)` (docs/NETSTACK.md, the
    /// async-receive path / rheo-net N2d). One reader at a time - a cell drives one
    /// NIC receive queue. Serviced by `block_on` when no queue completion is ready,
    /// by blocking in the kernel (`SYS_WAIT_NET`) until a frame arrives: where the
    /// NIC's RX interrupt is wired the kernel idles at WFI, so a cell waiting for a
    /// packet costs 0% CPU instead of re-submitting `OP_NET_RX` in a spin.
    net_rx_req: Option<(u64, usize, u64, u64)>,
    /// Frame length the last serviced network receive returned.
    net_rx_n: usize,
    /// Count of network receives the reactor delivered by a genuine park -> kernel
    /// block -> wake. One per `net::recv`, never N re-polls: the no-spin proof.
    net_wakeups: u64,
    /// A pending child wait: `(handle, token)` (docs/LIBRHEO.md Phase F). One
    /// outstanding at a time (a shell waits its children in sequence); serviced
    /// by `block_on` by blocking the parent in `SYS_WAIT` while its other strands
    /// have all parked - the parent's reactor keeps running.
    wait_req: Option<(u64, u64)>,
    /// Exit code the last serviced child wait returned.
    wait_code: u64,
    /// The cross-cell channels this cell drives, one per [`attach_channel_slot`]
    /// binding (docs/LIBRHEO.md Phase J; the multi-slot fan-out is
    /// docs/NETSTACK.md the service-cell section, rheo-net N4a). Slot 0 is the
    /// Phase E/J channel; a **service cell** binds one slot per client and runs one
    /// strand per slot, which is what serves N clients concurrently. Unlike the
    /// kernel queue, the kernel never drains these - the two cells drive the SPSC
    /// rings directly over the shared frames, so the symmetric async
    /// `Sender`/`Receiver` parks on the reactor and the idle path hands the CPU to
    /// the next runnable cell (the one cooperative cell-boundary switch that remains
    /// under the single-CPU model - the *in-cell* wait is a genuine park, not a spin).
    chans: [Option<ChanSlot>; MAX_CHANNELS],
    /// High-water mark of how many channel slots held an unconsumed inbound message
    /// at the same instant (docs/NETSTACK.md rheo-net N4a): the service's
    /// concurrency witness - N means all N clients' requests were in flight at once.
    chan_max_pending: usize,
}

/// How many cross-cell channel ends one cell's reactor can drive. Matches the
/// kernel's `MAX_CELL_CHANNELS` (kernel/src/abi.rs).
pub const MAX_CHANNELS: usize = 4;

/// One bound cross-cell channel end plus the strand state parked on it
/// (docs/NETSTACK.md the service-cell section, rheo-net N4a).
struct ChanSlot {
    qp: Qp,
    role: u64,
    cap_id: u32,
    /// A strand parked in `chan_recv` on this end (its token), and the message
    /// the reactor delivered once it was available.
    recv_req: Option<u64>,
    recv_msg: Option<(u64, u32)>,
    /// A strand parked in `chan_send` because the ring was momentarily full
    /// (message + token). Woken when the peer drains space.
    send_req: Option<(u64, u32, u64)>,
    /// Count of recv deliveries the reactor drove on this end (park -> hand-off ->
    /// wake). Proof the wait is a genuine reactor park, not a busy switch spin.
    wakeups: u64,
}

impl Reactor {
    /// Submit `op` with `args` (up to 24 bytes) and `flags`, tagged with
    /// `token`. Spins through a doorbell drain if the ring is momentarily full.
    fn submit(&mut self, op: u8, flags: u8, args: &[u8], token: u64) {
        while !self.qp.submit(op, flags, self.cap_id, 0, token, args) {
            self.pump();
        }
    }

    /// Ring the doorbell, then drain every completion into `results` and wake
    /// the strand each one belongs to. Returns the number of completions.
    fn pump(&mut self) -> usize {
        sys::doorbell();
        let mut n = 0;
        while let Some(cqe) = self.qp.reap() {
            let token = cqe.user_data;
            self.results.insert(token, cqe);
            complete(token);
            n += 1;
        }
        n
    }

    fn take(&mut self, token: u64) -> Option<CqEntry> {
        self.results.remove(&token)
    }

    /// Register a pending console read (the strand parks on `token`).
    fn set_console_read(&mut self, buf: u64, len: usize, token: u64) {
        self.console_req = Some((buf, len, token));
    }

    /// Service a pending console read by **blocking in the kernel** until input
    /// arrives, then wake its strand. Returns false if none was pending. This is
    /// where the terminal idles: the kernel halts (or polls) inside
    /// `SYS_WAIT_INPUT` while every strand is parked.
    fn service_console(&mut self) -> bool {
        if let Some((buf, len, token)) = self.console_req.take() {
            self.console_n = sys::wait_input(buf as *mut u8, len);
            complete(token);
            true
        } else {
            false
        }
    }

    fn console_result(&self) -> usize {
        self.console_n
    }

    /// Register a pending one-shot timer (the strand parks on `token`).
    fn set_timer(&mut self, deadline_ns: u64, token: u64) {
        self.timer_req = Some((deadline_ns, token));
    }

    /// Service a pending timer by arming the kernel's one-shot deadline (which
    /// blocks until it elapses), then wake its strand. Returns false if none was
    /// pending. Cooperative: while parked, the vcore first drains every other
    /// ready strand; only when all have parked does `block_on` reach here.
    fn service_timer(&mut self) -> bool {
        if let Some((deadline_ns, token)) = self.timer_req.take() {
            sys::arm_timer(deadline_ns);
            complete(token);
            true
        } else {
            false
        }
    }

    /// Register a pending child wait (the strand parks on `token`).
    fn set_wait(&mut self, handle: u64, token: u64) {
        self.wait_req = Some((handle, token));
    }

    /// Service a pending child wait by blocking the parent in `SYS_WAIT` (the
    /// child runs cooperatively until it exits), then wake its strand. Returns
    /// false if none was pending.
    fn service_wait(&mut self) -> bool {
        if let Some((handle, token)) = self.wait_req.take() {
            self.wait_code = sys::wait(handle);
            complete(token);
            true
        } else {
            false
        }
    }

    fn wait_result(&self) -> u64 {
        self.wait_code
    }

    /// Register a pending network receive (the strand parks on `token`).
    fn set_net_rx(&mut self, buf: u64, len: usize, timeout_ns: u64, token: u64) {
        self.net_rx_req = Some((buf, len, timeout_ns, token));
    }

    /// Service a pending network receive by **blocking in the kernel** until a
    /// frame arrives, then wake its strand. Returns false if none was pending.
    /// This is where a networked cell idles: the kernel halts at WFI (where the
    /// NIC RX interrupt is wired) while every strand is parked.
    fn service_net_rx(&mut self) -> bool {
        if let Some((buf, len, timeout_ns, token)) = self.net_rx_req.take() {
            self.net_rx_n = sys::wait_net(buf as *mut u8, len, timeout_ns);
            self.net_wakeups += 1;
            complete(token);
            true
        } else {
            false
        }
    }

    fn net_rx_result(&self) -> usize {
        self.net_rx_n
    }

    /// Enqueue `(tag, val)` on channel `slot`'s producer ring (the client's SQ,
    /// the server's CQ). `false` if the ring is momentarily full or unbound.
    fn chan_produce(&self, slot: usize, tag: u64, val: u32) -> bool {
        let Some(c) = self.chans.get(slot).and_then(|c| c.as_ref()) else {
            return false;
        };
        if c.role == CHAN_ROLE_CLIENT {
            c.qp.submit(sys::OP_CHAN_MSG, 0, c.cap_id, 0, tag, &val.to_le_bytes())
        } else {
            c.qp.cq_push(CqEntry {
                flow_id: 0,
                user_data: tag,
                status: sys::STATUS_OK,
                result: val,
            })
        }
    }

    /// Dequeue one `(tag, val)` from channel `slot`'s consumer ring (the client's
    /// CQ, the server's SQ), or `None` if empty.
    fn chan_consume(&self, slot: usize) -> Option<(u64, u32)> {
        let c = self.chans.get(slot).and_then(|c| c.as_ref())?;
        if c.role == CHAN_ROLE_CLIENT {
            c.qp.reap().map(|e| (e.user_data, e.result))
        } else {
            c.qp.sq_pop().map(|e| {
                let mut b = [0u8; 4];
                b.copy_from_slice(&e.payload[0..4]);
                (e.user_data, u32::from_le_bytes(b))
            })
        }
    }

    /// Whether channel `slot` has an unconsumed inbound message - a
    /// non-destructive peek (docs/NETSTACK.md rheo-net N4a).
    fn chan_inbound(&self, slot: usize) -> bool {
        match self.chans.get(slot).and_then(|c| c.as_ref()) {
            Some(c) if c.role == CHAN_ROLE_CLIENT => c.qp.cq_pending(),
            Some(c) => c.qp.sq_pending(),
            None => false,
        }
    }

    /// Idle-path service for the async channel (docs/LIBRHEO.md Phase J): deliver
    /// a parked recv if a message is now available, complete a parked send if the
    /// ring drained, else hand the CPU to the peer so it can produce/consume.
    /// Reached from `block_on` only once every in-cell strand has parked - so the
    /// in-cell wait is a genuine reactor park (sibling strands ran meanwhile) and
    /// only the cell-boundary hand-off is a cooperative switch. Returns whether it
    /// made progress (a delivery, or a hand-off to the peer).
    fn service_channel(&mut self) -> bool {
        if self.chans.iter().all(|c| c.is_none()) {
            return false;
        }
        // Concurrency witness: how many ends hold an inbound message right now.
        let pending = (0..MAX_CHANNELS).filter(|&s| self.chan_inbound(s)).count();
        if pending > self.chan_max_pending {
            self.chan_max_pending = pending;
        }
        // Deliver / complete one slot per pass, scanning slots in order - that
        // round-robins the per-client strands of a service cell.
        for slot in 0..MAX_CHANNELS {
            let Some(token) = self.chans[slot].as_ref().and_then(|c| c.recv_req) else {
                continue;
            };
            if let Some(m) = self.chan_consume(slot) {
                let c = self.chans[slot].as_mut().expect("channel slot bound");
                c.recv_msg = Some(m);
                c.recv_req = None;
                c.wakeups += 1;
                complete(token);
                return true;
            }
        }
        for slot in 0..MAX_CHANNELS {
            let Some((tag, val, token)) = self.chans[slot].as_ref().and_then(|c| c.send_req) else {
                continue;
            };
            if self.chan_produce(slot, tag, val) {
                self.chans[slot]
                    .as_mut()
                    .expect("channel slot bound")
                    .send_req = None;
                complete(token);
                return true;
            }
        }
        // Nothing satisfiable locally: hand the CPU to the next runnable cell, then
        // let the next `block_on` pass re-check (a peer will have produced or
        // consumed). `yield_cell` is the round-robin generalisation of the Phase E
        // `cur^1` switch - with one peer it *is* that switch (docs/NETSTACK.md the
        // service-cell section, rheo-net N4a).
        if self
            .chans
            .iter()
            .flatten()
            .any(|c| c.recv_req.is_some() || c.send_req.is_some())
        {
            sys::yield_cell();
            return true;
        }
        false
    }
}

static mut REACTOR: Option<Reactor> = None;

/// The initial-stack pointer the kernel entered `_start` with (arg0), pointing
/// at the System V `argc` block (docs/LIBRHEO.md Phase F). 0 = no arguments (a
/// top-level cell installed with an empty stack). `proc::args`/`env` parse it.
static mut ARGS_PTR: u64 = 0;

/// Record the initial-stack pointer (called by `_start`).
///
/// # Safety
/// Called once at startup, before `proc::args`/`env` read it.
pub unsafe fn set_args(arg: u64) {
    unsafe {
        *core::ptr::addr_of_mut!(ARGS_PTR) = arg;
    }
}

/// The initial-stack pointer (the SysV `argc` block VA), or 0 if none.
pub fn args_ptr() -> u64 {
    // SAFETY: set once at startup, read-only afterwards.
    unsafe { *core::ptr::addr_of!(ARGS_PTR) }
}

/// Build the reactor from the cell's queue capability and mapped ring VA
/// (called by `_start`).
pub fn init(caps: &CapSet, qp_va: u64) {
    // SAFETY: `qp_va` is this cell's mapped, kernel-initialised ring region.
    let qp = unsafe { Qp::attach(qp_va as *mut u8) };
    let reactor = Reactor {
        qp,
        cap_id: caps.queue_cap_id(),
        results: BTreeMap::new(),
        console_req: None,
        console_n: 0,
        timer_req: None,
        wait_req: None,
        wait_code: 0,
        net_rx_req: None,
        net_rx_n: 0,
        net_wakeups: 0,
        chans: [const { None }; MAX_CHANNELS],
        chan_max_pending: 0,
    };
    // SAFETY: single-CPU cooperative cell; init runs once before any strand.
    unsafe {
        *core::ptr::addr_of_mut!(REACTOR) = Some(reactor);
    }
}

#[inline]
fn with_reactor<R>(f: impl FnOnce(&mut Reactor) -> R) -> R {
    // SAFETY: single CPU; the reactor is never borrowed across an `.await`
    // (submit and take are separate, synchronous sections).
    unsafe {
        let r = (*core::ptr::addr_of_mut!(REACTOR))
            .as_mut()
            .expect("librheo: reactor used before init");
        f(r)
    }
}

/// Submit `op` with `args`, park until it completes, and return the completion
/// (`status`, `result`, ...). The async replacement for a blocking syscall:
/// the vcore runs other strands while this one is parked.
pub async fn submit_and_await(op: u8, args: [u8; 24]) -> CqEntry {
    submit_and_await_flags(op, 0, args).await
}

/// Like [`submit_and_await`] but carrying op `flags` (e.g.
/// [`sys::FLAG_INLINE`](crate::sys::FLAG_INLINE) for a sub-threshold write).
pub async fn submit_and_await_flags(op: u8, flags: u8, args: [u8; 24]) -> CqEntry {
    let token = next_token();
    with_reactor(|r| r.submit(op, flags, &args, token));
    park_on(token).await;
    with_reactor(|r| r.take(token)).expect("librheo: completion missing after wake")
}

/// Block-and-wake console read: register a request, park until the reactor
/// services it (the kernel idles until input where the UART RX interrupt is
/// wired, polls otherwise), and return the byte count (0 = end of input). The
/// terminal's async input substrate (`term`, docs/LIBRHEO.md Phase D): while
/// this strand is parked the vcore runs the others, and only when they have all
/// parked does the reactor block in the kernel for a byte.
///
/// # Safety
/// `buf` must point at `len` writable bytes that outlive the await (the kernel
/// writes them during `SYS_WAIT_INPUT`).
pub async fn read_console(buf: *mut u8, len: usize) -> usize {
    let token = next_token();
    with_reactor(|r| r.set_console_read(buf as u64, len, token));
    park_on(token).await;
    with_reactor(|r| r.console_result())
}

/// Async sleep: park until `deadline_ns` nanoseconds of monotonic time elapse
/// (docs/LIBRHEO.md Phase F). While parked the vcore runs the other strands;
/// only when they have all parked does the reactor arm the kernel's one-shot
/// deadline. `time::sleep`/`timeout`/`interval` build on this.
pub async fn sleep_ns(deadline_ns: u64) {
    let token = next_token();
    with_reactor(|r| r.set_timer(deadline_ns, token));
    park_on(token).await;
}

/// Bind this cell's end of a cross-cell shared channel to the reactor
/// (docs/LIBRHEO.md Phase J): `chan_va` is the mapped ring region, `role` its
/// end (`0` = client/SQ-producer, `1` = server/CQ-producer), `cap_id` the
/// channel capability. After this, [`chan_send`]/[`chan_recv`] park on the
/// reactor - the symmetric async `Sender`/`Receiver`. `ipc::Channel::split`
/// calls this.
///
/// # Safety note
/// `chan_va` must be this cell's mapped, kernel-initialised shared ring region
/// (the same frames the peer maps). `Qp::attach` panics on an ABI mismatch.
pub fn attach_channel(chan_va: u64, role: u64, cap_id: u32) {
    attach_channel_slot(0, chan_va, role, cap_id);
}

/// Bind channel end `slot` of this cell to the reactor (docs/NETSTACK.md the
/// service-cell section, rheo-net N4a). Slot 0 is [`attach_channel`]'s Phase E/J
/// channel; a **service cell** binds one slot per client and drives them
/// concurrently, one parked strand each. `ipc::Channel::split` calls this with the
/// slot the channel was opened at.
///
/// # Safety note
/// `chan_va` must be this cell's mapped, kernel-initialised shared ring region for
/// that slot (the same frames the peer maps). `Qp::attach` panics on an ABI mismatch.
pub fn attach_channel_slot(slot: usize, chan_va: u64, role: u64, cap_id: u32) {
    assert!(slot < MAX_CHANNELS, "librheo: channel slot out of range");
    // SAFETY: `chan_va` is the cell's mapped channel region (see the contract).
    let qp = unsafe { Qp::attach(chan_va as *mut u8) };
    with_reactor(|r| {
        r.chans[slot] = Some(ChanSlot {
            qp,
            role,
            cap_id,
            recv_req: None,
            recv_msg: None,
            send_req: None,
            wakeups: 0,
        })
    });
}

/// Whether a cross-cell channel is bound to this cell's reactor (slot 0).
pub fn chan_attached() -> bool {
    chan_attached_slot(0)
}

/// Whether channel `slot` is bound to this cell's reactor.
pub fn chan_attached_slot(slot: usize) -> bool {
    with_reactor(|r| r.chans.get(slot).is_some_and(|c| c.is_some()))
}

/// How many channel receives the reactor delivered by a genuine park -> hand-off
/// -> wake, summed over every bound end (docs/LIBRHEO.md Phase J). A busy-switch
/// spin would never touch this; a proof the async receivers actually parked.
pub fn chan_wakeups() -> u64 {
    with_reactor(|r| r.chans.iter().flatten().map(|c| c.wakeups).sum())
}

/// Reactor-driven receive deliveries on channel `slot` alone - the per-client
/// wakeup count a service asserts (docs/NETSTACK.md rheo-net N4a).
pub fn chan_wakeups_on(slot: usize) -> u64 {
    with_reactor(|r| {
        r.chans
            .get(slot)
            .and_then(|c| c.as_ref())
            .map_or(0, |c| c.wakeups)
    })
}

/// The high-water mark of how many channel ends held an unconsumed inbound
/// message at the same instant (docs/NETSTACK.md the service-cell section,
/// rheo-net N4a). For a service serving N clients, reaching N proves all N
/// requests were genuinely in flight together - the concurrency witness.
pub fn chan_max_pending() -> usize {
    with_reactor(|r| r.chan_max_pending)
}

/// Send `(tag, val)` to the peer cell over the async channel (docs/LIBRHEO.md
/// Phase J). Enqueues on this end's producer ring; if it is momentarily full,
/// parks until the reactor drains space (the peer consuming). `ipc::AsyncSender`
/// wraps this.
pub async fn chan_send(tag: u64, val: u32) {
    chan_send_on(0, tag, val).await
}

/// [`chan_send`] on channel `slot` (docs/NETSTACK.md rheo-net N4a).
pub async fn chan_send_on(slot: usize, tag: u64, val: u32) {
    if with_reactor(|r| r.chan_produce(slot, tag, val)) {
        return;
    }
    let token = next_token();
    with_reactor(|r| {
        if let Some(c) = r.chans[slot].as_mut() {
            c.send_req = Some((tag, val, token));
        }
    });
    park_on(token).await;
}

/// Receive one `(tag, val)` from the peer cell over the async channel
/// (docs/LIBRHEO.md Phase J). **Always parks** on the reactor: the vcore runs
/// other strands while this one waits, and only when they have all parked does
/// the reactor hand the CPU to the peer (the cooperative cell-boundary switch) -
/// so an idle receiver costs no spin and every message is a reactor wake.
/// `ipc::AsyncReceiver` wraps this.
pub async fn chan_recv() -> (u64, u32) {
    chan_recv_on(0).await
}

/// [`chan_recv`] on channel `slot` (docs/NETSTACK.md the service-cell section,
/// rheo-net N4a). A service cell runs one strand per slot, each parked here; the
/// reactor scans the slots in order, so the strands round-robin.
pub async fn chan_recv_on(slot: usize) -> (u64, u32) {
    let token = next_token();
    with_reactor(|r| {
        if let Some(c) = r.chans[slot].as_mut() {
            c.recv_req = Some(token);
        }
    });
    park_on(token).await;
    with_reactor(|r| r.chans[slot].as_mut().and_then(|c| c.recv_msg.take()))
        .expect("librheo: channel recv woke with no message")
}

/// Block-and-wake network receive: register a request, park until the reactor
/// services it (the kernel idles at WFI until the NIC's RX interrupt fires where
/// it is wired, and polls otherwise), and return the frame length (0 = the wait
/// gave up / the `timeout_ns` deadline elapsed - 0 waits indefinitely). The async
/// receive substrate under `net::recv` (docs/NETSTACK.md, the
/// async-receive path / rheo-net N2d): while this strand is parked the vcore runs
/// the others, and only when they have all parked does the reactor block in the
/// kernel for a frame - **one park and one wake per frame**, never a re-poll spin.
///
/// # Safety
/// `buf` must point at `len` writable bytes that outlive the await (the kernel
/// writes them during `SYS_WAIT_NET`).
pub async fn recv_frame(buf: *mut u8, len: usize, timeout_ns: u64) -> usize {
    let token = next_token();
    with_reactor(|r| r.set_net_rx(buf as u64, len, timeout_ns, token));
    park_on(token).await;
    with_reactor(|r| r.net_rx_result())
}

/// How many network receives the reactor delivered by a genuine park -> kernel
/// block -> wake (docs/NETSTACK.md, the async-receive path). A re-poll spin would
/// register many `OP_NET_RX` submissions and never touch this; one wakeup per
/// received frame is the proof that `net::recv` parks.
pub fn net_wakeups() -> u64 {
    with_reactor(|r| r.net_wakeups)
}

/// Async wait for a spawned child (docs/LIBRHEO.md Phase F): register the wait,
/// park until the reactor blocks the parent in `SYS_WAIT` and the child exits,
/// and return the child's exit code. While parked the vcore runs the other
/// strands. `proc::Child::wait` builds on this.
pub async fn wait_child(handle: u64) -> u64 {
    let token = next_token();
    with_reactor(|r| r.set_wait(handle, token));
    park_on(token).await;
    with_reactor(|r| r.wait_result())
}

/// Drive `root` (and every strand it spawns) to completion, servicing the
/// queue whenever no strand is ready. The userland event loop: run ready
/// strands; when they have all parked, ring the doorbell + drain + wake; and if
/// nothing was ready there, block for console input (the terminal idle path).
pub fn block_on<F: Future<Output = ()> + 'static>(root: F) {
    spawn(root);
    let mut guard: u32 = 0;
    loop {
        runtime::strand::run();
        if !has_pending() {
            break;
        }
        let woke = with_reactor(|r| r.pump());
        if woke > 0 {
            guard = 0; // queue completions woke strands: progress
        } else if with_reactor(|r| r.service_console()) {
            guard = 0; // blocked for console input and woke its strand: progress
        } else if with_reactor(|r| r.service_timer()) {
            guard = 0; // armed a one-shot deadline and woke its strand: progress
        } else if with_reactor(|r| r.service_wait()) {
            guard = 0; // blocked in SYS_WAIT for a child and woke its strand
        } else if with_reactor(|r| r.service_net_rx()) {
            guard = 0; // blocked in SYS_WAIT_NET for a frame and woke its strand
        } else if with_reactor(|r| r.service_channel()) {
            guard = 0; // delivered a channel message or handed the CPU to the peer
        } else {
            // No completion, no console read: allow a few settling iterations
            // (join hand-offs), then declare no progress.
            guard += 1;
            assert!(guard < 4, "librheo: reactor made no progress");
        }
        assert!(guard < 100_000, "librheo: reactor ran away (deadlock)");
    }
}
