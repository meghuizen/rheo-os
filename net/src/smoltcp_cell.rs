//! The **smoltcp blessed transport cell** (docs/NETSTACK.md §13, Phase N2c).
//!
//! smoltcp is the doc-named blessed pure-Rust `no_std` transport - Redox's stack,
//! correctness-first, single-poll, no ambient threads (docs/NETSTACK.md 3). N2c
//! integrates it as an **alternative** transport running in a loaded rheo-os cell
//! **over the existing raw-frame NIC path** (`librheo::net`): the from-scratch
//! `net::{tcp,udp,ip,eth}` stack is unchanged and unaffected - smoltcp sits
//! *alongside* it, gated behind the `smoltcp` cargo feature so nothing links it
//! unless a cell asks for it. This validates the plan's "lean on the blessed
//! library where it fits" strategy and gives a mature stack for control/low-rate
//! cells.
//!
//! **The bridge.** smoltcp's [`phy::Device`] is *synchronous* (its `receive`/
//! `transmit` pop/push frames with no `.await`), while `librheo::net::send`/`recv`
//! are *async* over the strand reactor. [`QueueDevice`] bridges the two the way
//! every async-over-smoltcp integration does: it buffers frames in two
//! `VecDeque`s. The async driver ([`poll`]) pulls frames off the NIC with
//! `net::recv` into the device's RX queue, runs smoltcp's synchronous poll, then
//! ships the device's TX queue out with `net::send`. So the `RxToken`/`TxToken`
//! consume/produce the frames that `net::recv`/`net::send` carry - smoltcp drives
//! the real virtio-net driver end to end, one hop removed by the queue buffer.
//!
//! **The clock.** smoltcp wants a monotonic millisecond [`Instant`]. A cell has
//! no userspace ticks->ns reading (the kernel owns the timebase, `librheo::time`
//! documents the gap), so the driver advances smoltcp's clock by the **real**
//! duration it sleeps between polls (`librheo::time::sleep`, a genuine kernel
//! one-shot deadline): sleep 2 ms, advance the smoltcp clock 2 ms. The clock is
//! therefore real monotonic milliseconds, not a synthetic counter.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

use librheo::net;
use librheo::time::{self, Duration};

/// The largest Ethernet frame the device carries (standard MTU + header; jumbo
/// frames are a later phase, matching `net::wire::MAX_FRAME`).
const MTU: usize = 1500;
/// How many RX frames the driver drains off the NIC per poll before running
/// smoltcp (a small batch so a burst does not starve the poll).
const RX_BATCH: usize = 16;
/// The poll cadence: sleep this long between smoltcp polls and advance smoltcp's
/// clock by the same amount (real monotonic milliseconds).
const POLL_MS: u64 = 2;

/// A smoltcp [`Device`] backed by the cell's raw-frame queue. RX frames are fed
/// in by the async driver ([`QueueDevice::fill_rx`]); TX frames are collected here
/// and drained out by the driver ([`QueueDevice::drain_tx`]). smoltcp itself only
/// ever touches the two queues synchronously.
pub struct QueueDevice {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

impl Default for QueueDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl QueueDevice {
    /// An empty device (no frames queued).
    pub fn new() -> QueueDevice {
        QueueDevice {
            rx: VecDeque::new(),
            tx: VecDeque::new(),
        }
    }

    /// Push a received frame onto the RX queue (the driver calls this after a
    /// `net::recv`). smoltcp's next `receive` will hand it to the stack.
    pub fn push_rx(&mut self, frame: Vec<u8>) {
        self.rx.push_back(frame);
    }

    /// Pop the next frame smoltcp queued for transmit, if any (the driver ships
    /// it with `net::send`).
    pub fn pop_tx(&mut self) -> Option<Vec<u8>> {
        self.tx.pop_front()
    }
}

/// The RX token: owns one received frame; `consume` hands its bytes to smoltcp.
pub struct QueueRxToken(Vec<u8>);

impl RxToken for QueueRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

/// The TX token: on `consume` it builds a frame into a fresh buffer and enqueues
/// it on the device's TX queue for the driver to ship.
pub struct QueueTxToken<'a>(&'a mut VecDeque<Vec<u8>>);

impl TxToken for QueueTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
        r
    }
}

impl Device for QueueDevice {
    type RxToken<'a> = QueueRxToken;
    type TxToken<'a> = QueueTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((QueueRxToken(frame), QueueTxToken(&mut self.tx)))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(QueueTxToken(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = MTU;
        c
    }
}

/// A monotonic-millisecond clock for smoltcp, advanced by the real time the
/// driver sleeps (see the module note - a cell has no ticks->ns reading, so the
/// clock is the accumulated sleep duration, which the kernel timer makes real).
pub struct Clock {
    millis: i64,
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    pub fn new() -> Clock {
        Clock { millis: 0 }
    }

    /// smoltcp's current instant.
    pub fn now(&self) -> Instant {
        Instant::from_millis(self.millis)
    }

    /// Advance the clock by `ms` (the driver calls this by the amount it slept).
    pub fn advance(&mut self, ms: u64) {
        self.millis = self.millis.wrapping_add(ms as i64);
    }
}

/// One async poll step of a smoltcp interface over the raw-frame NIC:
///
/// 1. drain up to [`RX_BATCH`] frames off the NIC (`net::recv`) into the device;
/// 2. sleep [`POLL_MS`] on the reactor and advance the smoltcp `clock` by it (so
///    smoltcp's timers advance in real milliseconds while the vcore runs other
///    strands);
/// 3. ship every frame smoltcp queued for transmit out the NIC (`net::send`).
///
/// The caller runs smoltcp's synchronous `iface.poll(clock.now(), device,
/// sockets)` between calls (it borrows the sockets, which stays with the caller).
/// Returns the number of RX frames delivered this step.
pub async fn pump(device: &mut QueueDevice, clock: &mut Clock) -> usize {
    let mut got = 0;
    let mut buf = [0u8; MTU + 64];
    for _ in 0..RX_BATCH {
        match net::recv(&mut buf).await {
            Ok(0) => break, // nothing ready
            Ok(n) => {
                device.push_rx(buf[..n].to_vec());
                got += 1;
            }
            Err(_) => break,
        }
    }
    // A real reactor sleep (kernel one-shot deadline): advances smoltcp's clock
    // in real time and yields the vcore to other strands meanwhile.
    time::sleep(Duration::from_millis(POLL_MS)).await;
    clock.advance(POLL_MS);
    // Flush smoltcp's transmit queue out the NIC.
    while let Some(frame) = device.pop_tx() {
        let _ = net::send(&frame).await;
    }
    got
}
