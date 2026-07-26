//! `net::tcp` - the native TCP state machine (docs/NETSTACK.md §11, rheo-net Phase
//! N2a). A correct, poll-driven transport built on `net::ip` + the reusable
//! [`Checksum`](crate::ip::Checksum) accumulator (the TCP checksum uses the same
//! pseudo-header shape as UDP), the [`crate::timer`] wheel, and the strand reactor.
//! It adds **no kernel object** and **no per-ISA code** - pure userspace over the
//! existing queue ABI, like the rest of the crate.
//!
//! ## Shape: a poll-driven core, not an ambient task (the smoltcp lesson)
//! [`Connection`] is a **synchronous, deterministic** state machine with no I/O and
//! no async inside. A driver feeds it received segments ([`Connection::on_segment`]
//! / [`Connection::on_wire_segment`]) and a monotonic `now` (nanoseconds), and
//! pulls the next segment to transmit out of [`Connection::poll`]; the connection
//! reports when it next needs attention via [`Connection::poll_at`] (its RTO /
//! TIME-WAIT deadline). This is exactly what makes it provable **without a live
//! peer**: a test wires two `Connection`s to a [`VirtualLink`], advances a logical
//! clock through a [`TimerWheel`](crate::timer::TimerWheel), and drives the full
//! lifecycle - handshake, bidirectional data, a dropped segment recovered by RTO,
//! and teardown - deterministically (the traceroute/DNS "deterministic core, thin
//! live driver" philosophy). [`TcpStream`]/[`TcpListener`] are the socket-shaped
//! vocabulary over the same core.
//!
//! ## Sequence-number arithmetic (correctness-critical)
//! TCP sequence numbers are 32-bit and **wrap**; comparisons use RFC 1323 serial-
//! number arithmetic ([`seq`]): `a` is "before" `b` iff `(a - b)` interpreted as a
//! signed 32-bit value is negative. Every window/ack test goes through these
//! helpers - getting this wrong is the classic TCP bug, so it lives in one place
//! with its own unit oracle.
//!
//! ## RTO / RTT (RFC 6298) + Karn's algorithm
//! The retransmission timeout is estimated per RFC 6298: a smoothed RTT (`SRTT`)
//! and its variance (`RTTVAR`), `RTO = SRTT + max(G, 4*RTTVAR)` clamped to
//! `[RTO_MIN, RTO_MAX]`. **Karn's algorithm**: an RTT sample is never taken from a
//! retransmitted segment (the ack is ambiguous), and the RTO **backs off
//! exponentially** (doubles, capped) on each timeout until a fresh ack re-measures
//! it. Unacked data is retransmitted from `snd_una` when the RTO fires.
//!
//! ## Congestion control - a trait seam (N2b slots in here)
//! [`CongestionControl`] is the seam: N2a ships only [`FixedWindow`] (a large fixed
//! cwnd, so flow control - the peer's advertised window - dominates). CUBIC/BBR are
//! N2b, a drop-in `impl CongestionControl`. The send window is
//! `min(peer_advertised_window, cwnd)`.
//!
//! ## What N2a simplifies / defers (honest)
//! - **No SACK, no window scaling, no timestamps option** - only the MSS option is
//!   emitted/parsed. Large bandwidth-delay products and selective repair are N2b.
//! - **No out-of-order reassembly**: an out-of-order segment is dropped and the
//!   receiver re-acks `rcv_nxt`, relying on retransmission (correct, not optimal).
//! - **Immediate ACKs** (no delayed-ACK timer) and **no keepalive** in N2a; the
//!   timer wheel supports both (they are just more logical timers) - documented,
//!   not wired. **Zero-window** probing is minimal: a zero advertised window simply
//!   stalls the sender until a window update arrives (no persist timer yet).
//! - **RST handling is minimal**: a received RST drops the connection to CLOSED.

use alloc::vec::Vec;

use crate::ip::{self, Checksum, Ipv4Addr};

/// RFC 1323 serial-number (wrapping u32) comparisons. `lt(a,b)` == "a is before b".
pub mod seq {
    /// `a < b` in serial-number order (RFC 1323): true iff `(a - b)` as an `i32`
    /// is negative.
    pub fn lt(a: u32, b: u32) -> bool {
        (a.wrapping_sub(b) as i32) < 0
    }
    /// `a <= b` in serial-number order.
    pub fn leq(a: u32, b: u32) -> bool {
        a == b || lt(a, b)
    }
    /// `a > b` in serial-number order.
    pub fn gt(a: u32, b: u32) -> bool {
        lt(b, a)
    }
    /// `a >= b` in serial-number order.
    pub fn geq(a: u32, b: u32) -> bool {
        leq(b, a)
    }
}

// ---- TCP flag bits ----
pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;
pub const URG: u8 = 0x20;

/// The minimum TCP header length (no options), in bytes.
pub const HEADER_LEN: usize = 20;
/// The default maximum segment size rheo-net advertises/uses (standard Ethernet
/// MTU minus the IPv4 + TCP headers: 1500 - 20 - 20).
pub const DEFAULT_MSS: u16 = 1460;

// ---- RTO/RTT constants (nanoseconds), RFC 6298 ----
/// Clock granularity `G` for the RTO floor term (1 ms).
pub const RTO_G: u64 = 1_000_000;
/// Minimum RTO (200 ms; RFC 6298 suggests 1 s, we use a tighter emulation floor).
pub const RTO_MIN: u64 = 200_000_000;
/// Maximum RTO (60 s), the backoff cap.
pub const RTO_MAX: u64 = 60_000_000_000;
/// Initial RTO before any RTT sample (RFC 6298 §2.1: 1 s).
pub const RTO_INIT: u64 = 1_000_000_000;
/// Maximum Segment Lifetime used for the TIME-WAIT dwell (the state holds for
/// `2*MSL`). A tight emulation value - real stacks use ~30 s-2 min.
pub const MSL: u64 = 1_000_000_000;

/// The TCP connection states (RFC 793 fig. 6).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    Closed,
    Listen,
    SynSent,
    SynRcvd,
    Established,
    FinWait1,
    FinWait2,
    Closing,
    CloseWait,
    LastAck,
    TimeWait,
}

/// The TCP checksum over an **IPv4** pseudo-header and the whole segment (header,
/// options and payload). `seg` is the on-wire segment bytes; for computation its
/// checksum field (bytes 16..18) must be zero, for verification it is left in place
/// (a correct segment then folds to 0). Reuses the N1a [`Checksum`] accumulator
/// with the same `src, dst, zero, proto, len` pseudo-header UDP uses.
pub fn checksum_v4(src: Ipv4Addr, dst: Ipv4Addr, seg: &[u8]) -> u16 {
    let mut c = Checksum::new();
    c.add(&src.0);
    c.add(&dst.0);
    c.add(&[0, ip::proto::TCP]);
    c.add(&(seg.len() as u16).to_be_bytes());
    c.add(seg);
    c.finish()
}

/// True if the segment in `seg` verifies against the `src`/`dst` pseudo-header.
pub fn verify_checksum_v4(src: Ipv4Addr, dst: Ipv4Addr, seg: &[u8]) -> bool {
    seg.len() >= HEADER_LEN && checksum_v4(src, dst, seg) == 0
}

/// A TCP segment: the header fields, the (optional) MSS option, and an owned
/// payload. Owned bytes keep the loopback driver simple; the payload copy is
/// irrelevant to icount path length and would be elided on the zero-copy wire path
/// (documented N2b).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    /// The MSS option, emitted/parsed only on SYN segments (N2a's only option).
    pub mss: Option<u16>,
    pub payload: Vec<u8>,
}

impl Segment {
    /// The on-wire length this segment encodes to (header + MSS option + payload).
    pub fn encoded_len(&self) -> usize {
        let opt = if self.mss.is_some() { 4 } else { 0 };
        HEADER_LEN + opt + self.payload.len()
    }

    /// Encode the segment into `out` with a correct IPv4 checksum, returning the
    /// length written, or `None` if `out` is too small.
    pub fn encode(&self, src: Ipv4Addr, dst: Ipv4Addr, out: &mut [u8]) -> Option<usize> {
        let opt = if self.mss.is_some() { 4 } else { 0 };
        let hdr = HEADER_LEN + opt;
        let total = hdr + self.payload.len();
        if out.len() < total {
            return None;
        }
        out[0..2].copy_from_slice(&self.src_port.to_be_bytes());
        out[2..4].copy_from_slice(&self.dst_port.to_be_bytes());
        out[4..8].copy_from_slice(&self.seq.to_be_bytes());
        out[8..12].copy_from_slice(&self.ack.to_be_bytes());
        out[12] = ((hdr / 4) as u8) << 4; // data offset in 32-bit words, reserved 0
        out[13] = self.flags;
        out[14..16].copy_from_slice(&self.window.to_be_bytes());
        out[16..18].copy_from_slice(&[0, 0]); // checksum zero during computation
        out[18..20].copy_from_slice(&[0, 0]); // urgent pointer
        if let Some(m) = self.mss {
            out[20] = 2; // kind = MSS
            out[21] = 4; // length
            out[22..24].copy_from_slice(&m.to_be_bytes());
        }
        out[hdr..total].copy_from_slice(&self.payload);
        let ck = checksum_v4(src, dst, &out[..total]);
        out[16..18].copy_from_slice(&ck.to_be_bytes());
        Some(total)
    }

    /// Encode into a fresh `Vec` (the loopback path).
    pub fn to_vec(&self, src: Ipv4Addr, dst: Ipv4Addr) -> Vec<u8> {
        let mut v = alloc::vec![0u8; self.encoded_len()];
        self.encode(src, dst, &mut v)
            .expect("segment buffer sized by encoded_len");
        v
    }

    /// Decode a TCP segment from on-wire bytes. Parses the data offset, walks the
    /// options for an MSS, and copies the payload. Returns `None` on a malformed
    /// header (too short, or a data offset outside the buffer). Does **not** verify
    /// the checksum - the caller does ([`verify_checksum_v4`]).
    pub fn decode(buf: &[u8]) -> Option<Segment> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        let data_off = ((buf[12] >> 4) as usize) * 4;
        if data_off < HEADER_LEN || data_off > buf.len() {
            return None;
        }
        // Walk options for the MSS (kind 2, len 4). NOP (1) and EOL (0) handled.
        let mut mss = None;
        let mut i = HEADER_LEN;
        while i < data_off {
            match buf[i] {
                0 => break,  // end of options
                1 => i += 1, // NOP
                _ => {
                    if i + 1 >= data_off {
                        break;
                    }
                    let len = buf[i + 1] as usize;
                    if len < 2 || i + len > data_off {
                        break;
                    }
                    if buf[i] == 2 && len == 4 {
                        mss = Some(u16::from_be_bytes([buf[i + 2], buf[i + 3]]));
                    }
                    i += len;
                }
            }
        }
        Some(Segment {
            src_port: u16::from_be_bytes([buf[0], buf[1]]),
            dst_port: u16::from_be_bytes([buf[2], buf[3]]),
            seq: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            ack: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
            flags: buf[13],
            window: u16::from_be_bytes([buf[14], buf[15]]),
            mss,
            payload: buf[data_off..].to_vec(),
        })
    }
}

/// The congestion-control seam (docs/NETSTACK.md §11). N2a ships only
/// [`FixedWindow`]; N2b adds CUBIC/BBR as drop-in impls. The [`Connection`]
/// consults [`cwnd`](CongestionControl::cwnd) each time it computes the usable send
/// window and calls [`on_ack`](CongestionControl::on_ack)/[`on_loss`] as acks land
/// and the RTO fires.
pub trait CongestionControl {
    /// New data was cumulatively acknowledged (`bytes_acked` payload bytes), with
    /// an RTT sample if one was measured for this ack (Karn: `None` on a
    /// retransmitted segment).
    fn on_ack(&mut self, bytes_acked: u32, rtt_ns: Option<u64>);
    /// A retransmission timeout fired (a loss signal).
    fn on_loss(&mut self);
    /// The current congestion window, in bytes.
    fn cwnd(&self) -> u32;
}

/// The trivial N2a congestion controller: a large fixed window, so the effective
/// send window is governed by the peer's advertised (flow-control) window. This is
/// the "keep CC out of N2a, just wire the seam" impl the plan asks for.
#[derive(Copy, Clone, Debug)]
pub struct FixedWindow {
    cwnd: u32,
}

impl FixedWindow {
    pub const fn new(cwnd: u32) -> FixedWindow {
        FixedWindow { cwnd }
    }
}

impl Default for FixedWindow {
    fn default() -> FixedWindow {
        FixedWindow { cwnd: 256 * 1024 }
    }
}

impl CongestionControl for FixedWindow {
    fn on_ack(&mut self, _bytes_acked: u32, _rtt_ns: Option<u64>) {}
    fn on_loss(&mut self) {}
    fn cwnd(&self) -> u32 {
        self.cwnd
    }
}

/// The receive/send buffer capacity (bytes). Bounds the advertised window (a u16)
/// and the send queue. 32 KiB keeps well inside the u16 window with headroom.
const BUF_CAP: usize = 32 * 1024;

/// A TCP connection: the RFC 793 state machine plus the RFC 6298 RTO estimator and
/// a sliding send/receive window. Generic over the [`CongestionControl`] seam.
pub struct Connection<C: CongestionControl = FixedWindow> {
    state: State,
    local_ip: Ipv4Addr,
    remote_ip: Ipv4Addr,
    local_port: u16,
    remote_port: u16,

    // --- send state ---
    iss: u32,
    snd_una: u32,
    snd_nxt: u32,
    /// The highest sequence number ever transmitted + 1 (the send high-water). A
    /// segment is a **first transmission** iff its start `>= snd_max`; only those
    /// arm an RTT probe (Karn: never sample a retransmit).
    snd_max: u32,
    snd_wnd: u32,
    /// Unacknowledged + unsent application bytes; `txq[0]` has sequence `txq_seq`.
    txq: Vec<u8>,
    txq_seq: u32,
    mss: u16,
    /// The application asked to close; a FIN is emitted once all data is sent.
    app_closing: bool,
    /// The sequence number our FIN occupies, once emitted.
    fin_seq: Option<u32>,
    fin_acked: bool,

    // --- receive state ---
    irs: u32,
    rcv_nxt: u32,
    /// In-order received bytes awaiting the application.
    rxq: Vec<u8>,
    rxq_head: usize,
    /// A pure ACK is owed (data or a FIN advanced our receive sequence).
    need_ack: bool,

    // --- RTO / RTT (RFC 6298), nanoseconds ---
    srtt: u64,
    rttvar: u64,
    rtt_valid: bool,
    rto_ns: u64,
    /// When set, the retransmission deadline for the oldest unacked segment.
    rto_deadline: Option<u64>,
    /// An outstanding RTT probe `(end_seq, send_time_ns)`; cleared on retransmit
    /// (Karn) so a retransmitted segment never yields an RTT sample.
    rtt_probe: Option<(u32, u64)>,

    /// The TIME-WAIT (2*MSL) expiry.
    time_wait_deadline: Option<u64>,

    cc: C,
}

impl<C: CongestionControl + Default> Connection<C> {
    /// Active open (a client `connect`): CLOSED -> SYN_SENT. `iss` is the initial
    /// send sequence (the caller supplies it - e.g. from the per-cell DRBG). The
    /// SYN is emitted on the next [`poll`](Self::poll).
    pub fn connect(
        local_ip: Ipv4Addr,
        local_port: u16,
        remote_ip: Ipv4Addr,
        remote_port: u16,
        iss: u32,
    ) -> Connection<C> {
        let mut c = Connection::blank(local_ip, local_port, remote_ip, remote_port, iss);
        c.state = State::SynSent;
        c
    }

    /// Passive open (a server `listen`): a connection in LISTEN awaiting a SYN.
    /// `iss` is our initial send sequence for the SYN-ACK.
    pub fn listen(
        local_ip: Ipv4Addr,
        local_port: u16,
        remote_ip: Ipv4Addr,
        remote_port: u16,
        iss: u32,
    ) -> Connection<C> {
        let mut c = Connection::blank(local_ip, local_port, remote_ip, remote_port, iss);
        c.state = State::Listen;
        c
    }

    fn blank(
        local_ip: Ipv4Addr,
        local_port: u16,
        remote_ip: Ipv4Addr,
        remote_port: u16,
        iss: u32,
    ) -> Connection<C> {
        Connection {
            state: State::Closed,
            local_ip,
            remote_ip,
            local_port,
            remote_port,
            iss,
            snd_una: iss,
            snd_nxt: iss,
            snd_max: iss,
            snd_wnd: DEFAULT_MSS as u32,
            txq: Vec::new(),
            txq_seq: iss.wrapping_add(1), // data begins after the SYN
            mss: DEFAULT_MSS,
            app_closing: false,
            fin_seq: None,
            fin_acked: false,
            irs: 0,
            rcv_nxt: 0,
            rxq: Vec::new(),
            rxq_head: 0,
            need_ack: false,
            srtt: 0,
            rttvar: 0,
            rtt_valid: false,
            rto_ns: RTO_INIT,
            rto_deadline: None,
            rtt_probe: None,
            time_wait_deadline: None,
            cc: C::default(),
        }
    }
}

impl<C: CongestionControl> Connection<C> {
    /// The current connection state.
    pub fn state(&self) -> State {
        self.state
    }

    /// True once the three-way handshake has completed.
    pub fn is_established(&self) -> bool {
        self.state == State::Established
    }

    /// The current congestion controller (for inspection / N2b).
    pub fn congestion(&self) -> &C {
        &self.cc
    }

    /// The negotiated MSS (min of our default and the peer's advertised MSS).
    pub fn mss(&self) -> u16 {
        self.mss
    }

    /// The current RTO estimate in nanoseconds (RFC 6298).
    pub fn rto_ns(&self) -> u64 {
        self.rto_ns
    }

    /// Free space in the receive buffer - what we advertise as the window.
    fn recv_free(&self) -> usize {
        BUF_CAP.saturating_sub(self.rxq.len() - self.rxq_head)
    }

    /// The window we advertise (free receive space, clamped to a u16).
    fn rcv_window(&self) -> u16 {
        self.recv_free().min(u16::MAX as usize) as u16
    }

    /// Free space in the send buffer.
    fn send_free(&self) -> usize {
        BUF_CAP.saturating_sub(self.txq.len())
    }

    /// One past the sequence number of the last queued data byte.
    fn snd_data_end(&self) -> u32 {
        self.txq_seq.wrapping_add(self.txq.len() as u32)
    }

    /// Queue application data for transmission (a socket `write`). Returns the
    /// number of bytes accepted (bounded by the send buffer). Only valid while the
    /// send side is open; a no-op afterwards.
    pub fn write(&mut self, data: &[u8]) -> usize {
        if self.app_closing
            || matches!(
                self.state,
                State::Closed | State::Listen | State::FinWait1 | State::FinWait2 | State::Closing
            )
        {
            return 0;
        }
        let n = data.len().min(self.send_free());
        self.txq.extend_from_slice(&data[..n]);
        n
    }

    /// Copy delivered in-order received data into `buf` (a socket `read`). Returns
    /// the byte count (0 if none is buffered).
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let avail = self.rxq.len() - self.rxq_head;
        let n = avail.min(buf.len());
        buf[..n].copy_from_slice(&self.rxq[self.rxq_head..self.rxq_head + n]);
        self.rxq_head += n;
        // Compact once fully drained to keep the buffer bounded.
        if self.rxq_head == self.rxq.len() {
            self.rxq.clear();
            self.rxq_head = 0;
        }
        n
    }

    /// Bytes of received data available to [`read`](Self::read).
    pub fn recv_available(&self) -> usize {
        self.rxq.len() - self.rxq_head
    }

    /// Bytes queued for send and not yet acknowledged.
    pub fn send_unacked(&self) -> usize {
        self.txq.len()
    }

    /// Begin an active close: a FIN is emitted once all queued data is sent
    /// (ESTABLISHED -> FIN_WAIT_1, or CLOSE_WAIT -> LAST_ACK).
    pub fn close(&mut self) {
        if matches!(
            self.state,
            State::Established | State::CloseWait | State::SynRcvd
        ) {
            self.app_closing = true;
        }
    }

    // ---- RTT / RTO ----

    fn sample_rtt(&mut self, r: u64) {
        if !self.rtt_valid {
            self.srtt = r;
            self.rttvar = r / 2;
            self.rtt_valid = true;
        } else {
            let diff = self.srtt.abs_diff(r);
            self.rttvar = (self.rttvar * 3 + diff) / 4;
            self.srtt = (self.srtt * 7 + r) / 8;
        }
        let rto = self.srtt + core::cmp::max(RTO_G, 4 * self.rttvar);
        self.rto_ns = rto.clamp(RTO_MIN, RTO_MAX);
    }

    /// Arm the RTO for outstanding data if it is not already running.
    fn arm_rto(&mut self, now: u64) {
        if self.rto_deadline.is_none() {
            self.rto_deadline = Some(now + self.rto_ns);
        }
    }

    // ---- receive path ----

    /// Feed a **decoded** segment (the loopback/deterministic path). The wire path
    /// uses [`on_wire_segment`](Self::on_wire_segment), which decodes + verifies the
    /// checksum first.
    pub fn on_segment(&mut self, now: u64, seg: &Segment) {
        if seg.flags & RST != 0 {
            // Minimal RST handling (N2a): drop the connection.
            self.state = State::Closed;
            self.rto_deadline = None;
            self.time_wait_deadline = None;
            return;
        }

        match self.state {
            State::Listen => {
                if seg.flags & SYN != 0 {
                    self.irs = seg.seq;
                    self.rcv_nxt = seg.seq.wrapping_add(1);
                    self.snd_wnd = seg.window as u32;
                    if let Some(m) = seg.mss {
                        self.mss = self.mss.min(m);
                    }
                    self.state = State::SynRcvd;
                }
                return;
            }
            State::SynSent => {
                if seg.flags & SYN != 0 {
                    self.irs = seg.seq;
                    self.rcv_nxt = seg.seq.wrapping_add(1);
                    self.snd_wnd = seg.window as u32;
                    if let Some(m) = seg.mss {
                        self.mss = self.mss.min(m);
                    }
                    if seg.flags & ACK != 0 {
                        self.process_ack(now, seg.ack, seg.window);
                        self.state = State::Established;
                    } else {
                        // Simultaneous open: SYN without ACK.
                        self.state = State::SynRcvd;
                    }
                    self.need_ack = true;
                }
                return;
            }
            _ => {}
        }

        // Synchronised states: process the ack, then data, then FIN.
        if seg.flags & ACK != 0 {
            self.process_ack(now, seg.ack, seg.window);
        }
        if self.state == State::SynRcvd && self.snd_una == self.iss.wrapping_add(1) {
            // Our SYN-ACK was acknowledged.
            self.state = State::Established;
        }

        if !seg.payload.is_empty() && self.accepts_data() {
            if seg.seq == self.rcv_nxt {
                let free = self.recv_free();
                let n = seg.payload.len().min(free);
                self.rxq.extend_from_slice(&seg.payload[..n]);
                self.rcv_nxt = self.rcv_nxt.wrapping_add(n as u32);
            }
            // In-order or not, owe an ack (a dup-ack drives the peer's recovery).
            self.need_ack = true;
        }

        if seg.flags & FIN != 0 {
            // FIN occupies the sequence just past its payload; accept it only in
            // order (no out-of-order buffering in N2a).
            let fin_seq = seg.seq.wrapping_add(seg.payload.len() as u32);
            if fin_seq == self.rcv_nxt {
                self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                self.need_ack = true;
                self.on_peer_fin(now);
            }
        }
    }

    /// Decode + checksum-verify an on-wire segment (sender = our remote, receiver =
    /// us), then dispatch it. Returns `false` (dropping the segment) if it is
    /// malformed or fails the checksum - so the receive path genuinely validates
    /// the TCP checksum.
    pub fn on_wire_segment(&mut self, now: u64, bytes: &[u8]) -> bool {
        if !verify_checksum_v4(self.remote_ip, self.local_ip, bytes) {
            return false;
        }
        let Some(seg) = Segment::decode(bytes) else {
            return false;
        };
        self.on_segment(now, &seg);
        true
    }

    /// Whether the state still accepts inbound data.
    fn accepts_data(&self) -> bool {
        matches!(
            self.state,
            State::Established | State::FinWait1 | State::FinWait2
        )
    }

    fn process_ack(&mut self, now: u64, ack: u32, window: u16) {
        self.snd_wnd = window as u32;
        // A valid ack advances (snd_una, snd_nxt].
        if !(seq::gt(ack, self.snd_una) && seq::leq(ack, self.snd_nxt)) {
            return;
        }
        // SYN consumes one sequence number before the data.
        if self.snd_una == self.iss {
            self.snd_una = self.snd_una.wrapping_add(1);
        }
        // Drop cumulatively-acked data from the send queue.
        let data_ack = if seq::lt(ack, self.snd_data_end()) {
            ack
        } else {
            self.snd_data_end()
        };
        let mut data_acked = 0u32;
        if seq::gt(data_ack, self.txq_seq) {
            let drop = data_ack.wrapping_sub(self.txq_seq) as usize;
            let drop = drop.min(self.txq.len());
            self.txq.drain(..drop);
            self.txq_seq = self.txq_seq.wrapping_add(drop as u32);
            data_acked = drop as u32;
        }
        if let Some(fs) = self.fin_seq
            && seq::gt(ack, fs)
        {
            self.fin_acked = true;
        }
        self.snd_una = ack;

        // RTT sample (Karn: only if the probe wasn't cleared by a retransmit).
        let mut rtt = None;
        if let Some((end, t)) = self.rtt_probe
            && seq::geq(ack, end)
        {
            let r = now.saturating_sub(t);
            self.sample_rtt(r);
            rtt = Some(r);
            self.rtt_probe = None;
        }
        self.cc.on_ack(data_acked, rtt);

        // Restart or clear the RTO.
        if self.snd_una == self.snd_nxt {
            self.rto_deadline = None;
        } else {
            self.rto_deadline = Some(now + self.rto_ns);
        }

        self.on_ack_state(now);
    }

    /// State transitions driven by our FIN being acknowledged.
    fn on_ack_state(&mut self, now: u64) {
        match self.state {
            State::FinWait1 if self.fin_acked => self.state = State::FinWait2,
            State::Closing if self.fin_acked => self.enter_time_wait(now),
            State::LastAck if self.fin_acked => self.state = State::Closed,
            _ => {}
        }
    }

    /// State transitions driven by receiving the peer's FIN.
    fn on_peer_fin(&mut self, now: u64) {
        match self.state {
            State::Established | State::SynRcvd => self.state = State::CloseWait,
            State::FinWait1 => self.state = State::Closing,
            State::FinWait2 => self.enter_time_wait(now),
            _ => {}
        }
    }

    fn enter_time_wait(&mut self, now: u64) {
        self.state = State::TimeWait;
        self.time_wait_deadline = Some(now + 2 * MSL);
        self.rto_deadline = None;
    }

    // ---- send path ----

    fn base_segment(&self, seq: u32, ack: u32, flags: u8, payload: Vec<u8>) -> Segment {
        Segment {
            src_port: self.local_port,
            dst_port: self.remote_port,
            seq,
            ack,
            flags,
            window: self.rcv_window(),
            mss: if flags & SYN != 0 {
                Some(DEFAULT_MSS)
            } else {
                None
            },
            payload,
        }
    }

    fn emit(&self, seg: Segment) -> Vec<u8> {
        seg.to_vec(self.local_ip, self.remote_ip)
    }

    /// Record a transmitted segment `[start, end)` and, **only for a first
    /// transmission** (`start >= snd_max`), arm an RTT probe (Karn's algorithm - a
    /// retransmitted segment, whose `start < snd_max`, never yields a sample).
    fn note_sent(&mut self, start: u32, end: u32, now: u64) {
        if seq::geq(start, self.snd_max) && self.rtt_probe.is_none() {
            self.rtt_probe = Some((end, now));
        }
        if seq::gt(end, self.snd_max) {
            self.snd_max = end;
        }
    }

    /// Produce the next segment to transmit, or `None` if there is nothing to send
    /// right now. Priority: TIME-WAIT expiry, RTO retransmit, SYN/SYN-ACK, new
    /// data, FIN, then a pure ACK. The driver calls this repeatedly (draining all
    /// immediate output) and, when it returns `None`, advances the clock to
    /// [`poll_at`](Self::poll_at).
    pub fn poll(&mut self, now: u64) -> Option<Vec<u8>> {
        // TIME-WAIT expiry.
        if self.state == State::TimeWait
            && let Some(dl) = self.time_wait_deadline
            && now >= dl
        {
            self.state = State::Closed;
            self.time_wait_deadline = None;
            return None;
        }

        // RTO: rewind to snd_una and re-send the oldest unacked segment.
        if let Some(dl) = self.rto_deadline
            && now >= dl
            && seq::lt(self.snd_una, self.snd_nxt)
        {
            self.snd_nxt = self.snd_una;
            self.rtt_probe = None; // Karn: no RTT sample from a retransmit
            self.rto_ns = (self.rto_ns * 2).min(RTO_MAX); // exponential backoff
            self.cc.on_loss();
            self.rto_deadline = Some(now + self.rto_ns);
        }

        // SYN (active open) / SYN-ACK (passive open).
        if self.snd_nxt == self.iss {
            match self.state {
                State::SynSent => {
                    let seg = self.base_segment(self.iss, 0, SYN, Vec::new());
                    self.snd_nxt = self.iss.wrapping_add(1);
                    self.note_sent(self.iss, self.snd_nxt, now);
                    self.arm_rto(now);
                    return Some(self.emit(seg));
                }
                State::SynRcvd => {
                    let seg = self.base_segment(self.iss, self.rcv_nxt, SYN | ACK, Vec::new());
                    self.snd_nxt = self.iss.wrapping_add(1);
                    self.note_sent(self.iss, self.snd_nxt, now);
                    self.arm_rto(now);
                    return Some(self.emit(seg));
                }
                _ => {}
            }
        }

        // New data, bounded by the usable window (peer advertised ∧ cwnd) and MSS.
        if seq::lt(self.snd_nxt, self.snd_data_end()) {
            let win = self.snd_wnd.min(self.cc.cwnd());
            let right = self.snd_una.wrapping_add(win);
            if seq::lt(self.snd_nxt, right) {
                let off = self.snd_nxt.wrapping_sub(self.txq_seq) as usize;
                let avail = self.snd_data_end().wrapping_sub(self.snd_nxt) as usize;
                let room = right.wrapping_sub(self.snd_nxt) as usize;
                let n = avail.min(room).min(self.mss as usize);
                if n > 0 && off + n <= self.txq.len() {
                    let payload = self.txq[off..off + n].to_vec();
                    let start = self.snd_nxt;
                    let seg = self.base_segment(start, self.rcv_nxt, ACK | PSH, payload);
                    let end = start.wrapping_add(n as u32);
                    self.snd_nxt = end;
                    self.note_sent(start, end, now);
                    self.arm_rto(now);
                    self.need_ack = false;
                    return Some(self.emit(seg));
                }
            }
        }

        // FIN (first send or retransmit).
        if self.app_closing {
            match self.fin_seq {
                None => {
                    if self.snd_nxt == self.snd_data_end() {
                        let fs = self.snd_nxt;
                        let seg = self.base_segment(fs, self.rcv_nxt, FIN | ACK, Vec::new());
                        self.fin_seq = Some(fs);
                        self.snd_nxt = fs.wrapping_add(1);
                        self.note_sent(fs, self.snd_nxt, now);
                        self.arm_rto(now);
                        self.need_ack = false;
                        self.state = match self.state {
                            State::Established => State::FinWait1,
                            State::CloseWait => State::LastAck,
                            s => s,
                        };
                        return Some(self.emit(seg));
                    }
                }
                Some(fs) => {
                    if self.snd_nxt == fs {
                        let seg = self.base_segment(fs, self.rcv_nxt, FIN | ACK, Vec::new());
                        self.snd_nxt = fs.wrapping_add(1);
                        self.note_sent(fs, self.snd_nxt, now);
                        self.arm_rto(now);
                        return Some(self.emit(seg));
                    }
                }
            }
        }

        // A pure ACK owed for received data/FIN.
        if self.need_ack {
            self.need_ack = false;
            let seg = self.base_segment(self.snd_nxt, self.rcv_nxt, ACK, Vec::new());
            return Some(self.emit(seg));
        }

        None
    }

    /// The next time the connection needs a [`poll`](Self::poll) with no input:
    /// its RTO deadline or TIME-WAIT expiry, whichever is sooner. `None` means it
    /// is quiescent (all immediate output is produced by `poll` returning `Some`).
    pub fn poll_at(&self) -> Option<u64> {
        let mut m: Option<u64> = None;
        let mut take = |d: Option<u64>| {
            if let Some(d) = d {
                m = Some(m.map_or(d, |cur: u64| cur.min(d)));
            }
        };
        take(self.rto_deadline);
        if self.state == State::TimeWait {
            take(self.time_wait_deadline);
        }
        m
    }
}

/// A **virtual link** between two [`Connection`]s in one cell (docs/NETSTACK.md
/// §11): it carries an emitted segment to the peer's receive path, optionally
/// **dropping** one chosen segment to prove RTO recovery. This is the loopback the
/// deterministic proof drives - no NIC, no IP routing, fully in-cell.
#[derive(Default)]
pub struct VirtualLink {
    /// When set, drop the next data-bearing segment that crosses the link, once.
    drop_next_data: bool,
    delivered: u64,
    dropped: u64,
}

impl VirtualLink {
    pub fn new() -> VirtualLink {
        VirtualLink::default()
    }

    /// Arm a one-shot drop of the **next data-bearing segment** to cross the link
    /// (a pure ACK/SYN/FIN passes through). Proves RTO recovery: the segment is
    /// lost, and only a retransmit after the RTO delivers it.
    pub fn drop_next_data_segment(&mut self) {
        self.drop_next_data = true;
    }

    /// Segments genuinely delivered to a peer (checksum-validated).
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// Segments dropped by the armed one-shot.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Carry `bytes` (an emitted segment) to `dst`'s receive path at time `now`,
    /// unless the one-shot data drop is armed. Returns whether it was delivered.
    pub fn transfer<C: CongestionControl>(
        &mut self,
        bytes: &[u8],
        dst: &mut Connection<C>,
        now: u64,
    ) -> bool {
        if self.drop_next_data
            && let Some(s) = Segment::decode(bytes)
            && !s.payload.is_empty()
        {
            self.dropped += 1;
            self.drop_next_data = false;
            return false;
        }
        if dst.on_wire_segment(now, bytes) {
            self.delivered += 1;
            true
        } else {
            false
        }
    }
}

/// A socket-shaped **stream** view over a [`Connection`] (docs/NETSTACK.md §11).
/// The connect/read/write/close vocabulary; the segment transport (the loopback
/// [`VirtualLink`] in the deterministic proof, a wire link in N2b) is driven by the
/// owner via [`poll`](Self::poll)/[`on_wire_segment`](Self::on_wire_segment).
pub struct TcpStream<C: CongestionControl = FixedWindow> {
    conn: Connection<C>,
}

impl<C: CongestionControl + Default> TcpStream<C> {
    /// Actively open a connection (SYN_SENT).
    pub fn connect(
        local_ip: Ipv4Addr,
        local_port: u16,
        remote_ip: Ipv4Addr,
        remote_port: u16,
        iss: u32,
    ) -> TcpStream<C> {
        TcpStream {
            conn: Connection::connect(local_ip, local_port, remote_ip, remote_port, iss),
        }
    }
}

impl<C: CongestionControl> TcpStream<C> {
    /// Queue data to send (`write`); returns bytes accepted.
    pub fn write(&mut self, data: &[u8]) -> usize {
        self.conn.write(data)
    }
    /// Read delivered data (`read`); returns bytes copied.
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        self.conn.read(buf)
    }
    /// Begin a graceful close (send a FIN).
    pub fn close(&mut self) {
        self.conn.close()
    }
    /// The connection state.
    pub fn state(&self) -> State {
        self.conn.state()
    }
    /// True once established.
    pub fn is_established(&self) -> bool {
        self.conn.is_established()
    }
    /// Next segment to transmit, if any.
    pub fn poll(&mut self, now: u64) -> Option<Vec<u8>> {
        self.conn.poll(now)
    }
    /// Feed a received on-wire segment (checksum-verified).
    pub fn on_wire_segment(&mut self, now: u64, bytes: &[u8]) -> bool {
        self.conn.on_wire_segment(now, bytes)
    }
    /// When the connection next needs attention.
    pub fn poll_at(&self) -> Option<u64> {
        self.conn.poll_at()
    }
    /// The underlying connection (escape hatch).
    pub fn connection(&mut self) -> &mut Connection<C> {
        &mut self.conn
    }
}

/// A socket-shaped **listener** view over a [`Connection`] (docs/NETSTACK.md §11):
/// a passive open. Once the handshake completes, [`accept`](Self::accept) hands
/// back the established [`TcpStream`].
pub struct TcpListener<C: CongestionControl = FixedWindow> {
    conn: Connection<C>,
}

impl<C: CongestionControl + Default> TcpListener<C> {
    /// Passively open (LISTEN), ready to accept a SYN from `remote`.
    pub fn bind(
        local_ip: Ipv4Addr,
        local_port: u16,
        remote_ip: Ipv4Addr,
        remote_port: u16,
        iss: u32,
    ) -> TcpListener<C> {
        TcpListener {
            conn: Connection::listen(local_ip, local_port, remote_ip, remote_port, iss),
        }
    }
}

impl<C: CongestionControl> TcpListener<C> {
    /// Next segment to transmit, if any (drives the SYN-ACK).
    pub fn poll(&mut self, now: u64) -> Option<Vec<u8>> {
        self.conn.poll(now)
    }
    /// Feed a received on-wire segment.
    pub fn on_wire_segment(&mut self, now: u64, bytes: &[u8]) -> bool {
        self.conn.on_wire_segment(now, bytes)
    }
    /// When the connection next needs attention.
    pub fn poll_at(&self) -> Option<u64> {
        self.conn.poll_at()
    }
    /// The connection state.
    pub fn state(&self) -> State {
        self.conn.state()
    }
    /// Consume the listener and view its established connection as a stream.
    pub fn accept(self) -> TcpStream<C> {
        TcpStream { conn: self.conn }
    }
    /// The underlying connection (escape hatch).
    pub fn connection(&mut self) -> &mut Connection<C> {
        &mut self.conn
    }
}
