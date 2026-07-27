//! The HTTP/2 connection state machine (RFC 9113 §5, §6) - docs/NETSTACK.md §19.
//!
//! ## Shape: the same synchronous seam as `net::tcp`
//! [`Connection`] has **no I/O and no async inside**, deliberately mirroring
//! [`crate::tcp::Connection`]: bytes in through [`Connection::on_bytes`], bytes out
//! through [`Connection::take_out`], and semantic results out through
//! [`Connection::next_event`]. That is what makes h2 provable with no live peer -
//! a client and a server `Connection` in one cell, wired by the in-cell
//! [`crate::tcp::VirtualLink`], run the whole protocol deterministically. It also
//! means the same code drives h2 over TCP, over a TLS record layer (ALPN `h2`), or
//! over the local fast path, because none of those are visible from here.
//!
//! ## Flow control (both levels, RFC 9113 §5.2, §6.9)
//! A sender may only emit DATA within `min(connection window, stream window)`.
//! [`Connection::send_data`] therefore **queues** the caller's bytes on the stream
//! and emits only what the windows currently allow; a WINDOW_UPDATE from the peer
//! credits the window and the queued remainder flows out on the next
//! [`Connection::flush`]. Receiving DATA debits both of our windows, and a peer
//! that overruns them is a `FLOW_CONTROL_ERROR`. A window that would exceed
//! 2^31-1 is likewise an error rather than a wrap.
//!
//! ## What N5a's HTTP/2 does not do (honest)
//! - **No server push** (`PUSH_PROMISE` is rejected as a protocol error - the
//!   client also advertises `SETTINGS_ENABLE_PUSH: 0`).
//! - **PRIORITY is parsed and ignored**, as RFC 9113 §5.3.1 now permits; there is
//!   no priority tree and no scheduler.
//! - **No trailers** (a second HEADERS block on a stream is a protocol error).
//! - **No `h2c` Upgrade dance**: a connection is either prior-knowledge h2c or
//!   ALPN-negotiated `h2` over TLS.
//! - Concurrency is bounded by [`MAX_STREAMS`]; there is no dynamic
//!   `SETTINGS_MAX_CONCURRENT_STREAMS` enforcement beyond that cap.
//! - The stream state machine tracks the states the request/response exchange
//!   needs (idle / open / half-closed each way / closed); the reserved states
//!   belong to push, which is not implemented.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use super::frame::{self, err, flag, kind, setting};
use super::hpack::{self, HpackError};

/// The HTTP/2 connection preface a client sends first (RFC 9113 §3.4).
pub const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// The most concurrent streams one connection will track.
pub const MAX_STREAMS: usize = 64;

/// A connection-level failure. Each carries the RFC 9113 §7 error code a real
/// endpoint would put in its GOAWAY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H2Error {
    /// A malformed frame, a frame on the wrong stream, or a state violation.
    Protocol,
    /// A frame whose length is illegal for its type.
    FrameSize,
    /// HPACK could not decode a header block.
    Compression(HpackError),
    /// A peer exceeded a flow-control window, or a window overflowed 2^31-1.
    FlowControl,
    /// The client preface was wrong.
    BadPreface,
    /// A feature this slice does not implement (server push).
    Unsupported,
}

impl H2Error {
    /// The RFC 9113 §7 error code for this failure.
    pub fn code(self) -> u32 {
        match self {
            H2Error::Protocol | H2Error::BadPreface => err::PROTOCOL_ERROR,
            H2Error::FrameSize => err::FRAME_SIZE_ERROR,
            H2Error::Compression(_) => err::COMPRESSION_ERROR,
            H2Error::FlowControl => err::FLOW_CONTROL_ERROR,
            H2Error::Unsupported => err::PROTOCOL_ERROR,
        }
    }
}

/// What a peer's frames mean, in arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A complete header block (after any CONTINUATION frames) arrived.
    Headers {
        stream_id: u32,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        end_stream: bool,
    },
    /// A DATA frame arrived.
    Data {
        stream_id: u32,
        data: Vec<u8>,
        end_stream: bool,
    },
    /// The peer's SETTINGS arrived (already acknowledged by us).
    Settings,
    /// The peer acknowledged our SETTINGS.
    SettingsAck,
    /// A WINDOW_UPDATE arrived (`stream_id` 0 is the connection window).
    WindowUpdate { stream_id: u32, increment: u32 },
    /// The peer reset a stream.
    StreamReset { stream_id: u32, error_code: u32 },
    /// A PING arrived (`ack` distinguishes a reply to ours).
    Ping { payload: [u8; 8], ack: bool },
    /// The peer is going away.
    GoAway {
        last_stream_id: u32,
        error_code: u32,
    },
}

/// A stream's lifecycle (RFC 9113 §5.1), minus the push-only reserved states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Idle,
    Open,
    /// We have sent END_STREAM; we may still receive.
    HalfClosedLocal,
    /// The peer has sent END_STREAM; we may still send.
    HalfClosedRemote,
    Closed,
}

struct Stream {
    state: StreamState,
    /// How many DATA bytes we may still send (peer's advertised window).
    send_window: i64,
    /// How many DATA bytes the peer may still send us.
    recv_window: i64,
    /// Application bytes queued behind a closed flow-control window.
    pending: Vec<u8>,
    /// The queued bytes end the stream once they have all been sent.
    pending_end: bool,
    /// A header block has already been received (a second one would be trailers).
    got_headers: bool,
}

impl Stream {
    fn new(send_window: i64, recv_window: i64) -> Stream {
        Stream {
            state: StreamState::Idle,
            send_window,
            recv_window,
            pending: Vec::new(),
            pending_end: false,
            got_headers: false,
        }
    }
}

/// One HTTP/2 connection endpoint.
pub struct Connection {
    is_server: bool,
    out: Vec<u8>,
    inb: Vec<u8>,
    preface_done: bool,
    enc: hpack::Encoder,
    dec: hpack::Decoder,
    streams: BTreeMap<u32, Stream>,
    next_stream_id: u32,
    conn_send_window: i64,
    conn_recv_window: i64,
    /// The peer's `SETTINGS_INITIAL_WINDOW_SIZE` (what our streams may send).
    peer_initial_window: u32,
    /// Our own initial window (what we let each peer stream send us).
    our_initial_window: u32,
    peer_max_frame_size: u32,
    events: Vec<Event>,
    /// A header block being assembled across CONTINUATION frames.
    cont: Option<(u32, Vec<u8>, bool)>,
    goaway_received: bool,
}

impl Connection {
    /// A client endpoint. `initial_window` is what we advertise for our own
    /// receive windows. The connection preface and our SETTINGS are queued
    /// immediately, so the very first [`take_out`](Self::take_out) is what a real
    /// client writes on connect.
    pub fn client(initial_window: u32) -> Connection {
        let mut c = Connection::new(false, initial_window);
        c.out.extend_from_slice(PREFACE);
        c.out.extend_from_slice(&c_settings(initial_window));
        c
    }

    /// A server endpoint: it expects the client preface, and queues its own
    /// SETTINGS to be written as soon as the transport is up.
    pub fn server(initial_window: u32) -> Connection {
        let mut c = Connection::new(true, initial_window);
        c.out.extend_from_slice(&c_settings(initial_window));
        c
    }

    fn new(is_server: bool, initial_window: u32) -> Connection {
        Connection {
            is_server,
            out: Vec::new(),
            inb: Vec::new(),
            preface_done: !is_server,
            enc: hpack::Encoder::new(hpack::DEFAULT_TABLE_SIZE, true),
            dec: hpack::Decoder::new(hpack::DEFAULT_TABLE_SIZE),
            streams: BTreeMap::new(),
            next_stream_id: if is_server { 2 } else { 1 },
            conn_send_window: frame::DEFAULT_INITIAL_WINDOW as i64,
            // The **connection** flow-control window always starts at 65535 and is
            // only ever changed by WINDOW_UPDATE - `SETTINGS_INITIAL_WINDOW_SIZE`
            // governs per-**stream** windows only (RFC 9113 §6.9.2). Tying the
            // connection window to the setting would make a small advertised
            // stream window silently cap the whole connection.
            conn_recv_window: frame::DEFAULT_INITIAL_WINDOW as i64,
            peer_initial_window: frame::DEFAULT_INITIAL_WINDOW,
            our_initial_window: initial_window,
            peer_max_frame_size: frame::DEFAULT_MAX_FRAME_SIZE,
            events: Vec::new(),
            cont: None,
            goaway_received: false,
        }
    }

    // ---- outbound ----

    /// Take everything queued for the transport.
    pub fn take_out(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.out)
    }

    /// Bytes currently queued for the transport.
    pub fn pending_out(&self) -> usize {
        self.out.len()
    }

    /// Allocate the next stream id for this endpoint (odd for a client, even for a
    /// server, RFC 9113 §5.1.1).
    pub fn open_stream(&mut self) -> u32 {
        let id = self.next_stream_id;
        self.next_stream_id += 2;
        self.streams.insert(
            id,
            Stream::new(
                self.peer_initial_window as i64,
                self.our_initial_window as i64,
            ),
        );
        id
    }

    /// Send a header block on `stream_id`, HPACK-encoded, in one HEADERS frame
    /// (this slice never needs CONTINUATION on send - header lists are small - but
    /// it decodes CONTINUATION on receive).
    pub fn send_headers(&mut self, stream_id: u32, fields: &[(&[u8], &[u8])], end_stream: bool) {
        let block = self.enc.encode(fields);
        let mut flags = flag::END_HEADERS;
        if end_stream {
            flags |= flag::END_STREAM;
        }
        let f = frame::build(kind::HEADERS, flags, stream_id, &block);
        self.out.extend_from_slice(&f);
        let s = self.streams.entry(stream_id).or_insert_with(|| {
            Stream::new(
                self.peer_initial_window as i64,
                self.our_initial_window as i64,
            )
        });
        s.state = match (s.state, end_stream) {
            (StreamState::Idle, false) => StreamState::Open,
            (StreamState::Idle, true) => StreamState::HalfClosedLocal,
            (StreamState::HalfClosedRemote, true) => StreamState::Closed,
            (st, true) => {
                if st == StreamState::Open {
                    StreamState::HalfClosedLocal
                } else {
                    st
                }
            }
            (st, false) => st,
        };
    }

    /// Queue `data` for `stream_id` and emit as much as the connection and stream
    /// flow-control windows allow. Returns how many bytes went on the wire right
    /// now; the rest stays queued until a WINDOW_UPDATE opens the window.
    pub fn send_data(&mut self, stream_id: u32, data: &[u8], end_stream: bool) -> usize {
        if let Some(s) = self.streams.get_mut(&stream_id) {
            s.pending.extend_from_slice(data);
            s.pending_end |= end_stream;
        }
        self.flush()
    }

    /// Emit whatever queued DATA the current windows permit, across all streams.
    /// Returns the number of payload bytes written.
    pub fn flush(&mut self) -> usize {
        let ids: Vec<u32> = self.streams.keys().copied().collect();
        let mut sent_total = 0usize;
        for id in ids {
            loop {
                let (n, end) = {
                    let Some(s) = self.streams.get(&id) else {
                        break;
                    };
                    if s.pending.is_empty() {
                        // An empty final DATA frame is still needed to carry
                        // END_STREAM if the caller asked for it.
                        if s.pending_end
                            && s.state != StreamState::Closed
                            && !matches!(s.state, StreamState::HalfClosedLocal)
                        {
                            (0usize, true)
                        } else {
                            break;
                        }
                    } else {
                        let win = self
                            .conn_send_window
                            .min(s.send_window)
                            .min(self.peer_max_frame_size as i64);
                        if win <= 0 {
                            break;
                        }
                        let n = (win as usize).min(s.pending.len());
                        (n, s.pending_end && n == s.pending.len())
                    }
                };
                let payload: Vec<u8> = {
                    let s = self.streams.get_mut(&id).expect("stream present");
                    let p: Vec<u8> = s.pending.drain(..n).collect();
                    s.send_window -= n as i64;
                    if end {
                        s.pending_end = false;
                        s.state = match s.state {
                            StreamState::HalfClosedRemote => StreamState::Closed,
                            _ => StreamState::HalfClosedLocal,
                        };
                    }
                    p
                };
                self.conn_send_window -= n as i64;
                let f = frame::build(
                    kind::DATA,
                    if end { flag::END_STREAM } else { 0 },
                    id,
                    &payload,
                );
                self.out.extend_from_slice(&f);
                sent_total += n;
                if end || n == 0 {
                    break;
                }
            }
        }
        sent_total
    }

    /// Grant the peer `increment` more bytes on `stream_id` (0 = connection), and
    /// credit our own receive accounting by the same amount.
    pub fn send_window_update(&mut self, stream_id: u32, increment: u32) {
        if stream_id == 0 {
            self.conn_recv_window += increment as i64;
        } else if let Some(s) = self.streams.get_mut(&stream_id) {
            s.recv_window += increment as i64;
        }
        let f = frame::build_window_update(stream_id, increment);
        self.out.extend_from_slice(&f);
    }

    /// Reset a stream.
    pub fn send_rst_stream(&mut self, stream_id: u32, error_code: u32) {
        if let Some(s) = self.streams.get_mut(&stream_id) {
            s.state = StreamState::Closed;
            s.pending.clear();
            s.pending_end = false;
        }
        let f = frame::build_rst_stream(stream_id, error_code);
        self.out.extend_from_slice(&f);
    }

    /// Send GOAWAY with the highest stream id we have processed.
    pub fn send_goaway(&mut self, error_code: u32) {
        let last = self.streams.keys().next_back().copied().unwrap_or(0);
        let f = frame::build_goaway(last, error_code, &[]);
        self.out.extend_from_slice(&f);
    }

    /// Send a PING.
    pub fn send_ping(&mut self, payload: [u8; 8]) {
        let f = frame::build_ping(payload, false);
        self.out.extend_from_slice(&f);
    }

    // ---- inbound ----

    /// Feed transport bytes. Parses every complete frame present, updates state,
    /// queues any protocol replies (SETTINGS ack, PING ack) and appends events.
    pub fn on_bytes(&mut self, bytes: &[u8]) -> Result<(), H2Error> {
        self.inb.extend_from_slice(bytes);
        if !self.preface_done {
            if self.inb.len() < PREFACE.len() {
                // Reject early if what we have already diverges.
                if !PREFACE.starts_with(&self.inb[..]) {
                    return Err(H2Error::BadPreface);
                }
                return Ok(());
            }
            if &self.inb[..PREFACE.len()] != PREFACE {
                return Err(H2Error::BadPreface);
            }
            self.inb.drain(..PREFACE.len());
            self.preface_done = true;
        }
        loop {
            let Some(h) = frame::FrameHeader::decode(&self.inb) else {
                return Ok(());
            };
            let total = frame::HEADER_LEN + h.length as usize;
            if self.inb.len() < total {
                return Ok(());
            }
            let payload: Vec<u8> = self.inb[frame::HEADER_LEN..total].to_vec();
            self.inb.drain(..total);
            self.on_frame(h, &payload)?;
        }
    }

    /// Pop the oldest pending event.
    pub fn next_event(&mut self) -> Option<Event> {
        if self.events.is_empty() {
            None
        } else {
            Some(self.events.remove(0))
        }
    }

    /// Number of queued events.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    fn on_frame(&mut self, h: frame::FrameHeader, payload: &[u8]) -> Result<(), H2Error> {
        // A header block must not be interleaved with any other frame type
        // (RFC 9113 §6.2): only CONTINUATION on the same stream may follow.
        if self.cont.is_some() && h.kind != kind::CONTINUATION {
            return Err(H2Error::Protocol);
        }
        match h.kind {
            kind::SETTINGS => self.on_settings(h, payload),
            kind::HEADERS => self.on_headers(h, payload),
            kind::CONTINUATION => self.on_continuation(h, payload),
            kind::DATA => self.on_data(h, payload),
            kind::WINDOW_UPDATE => self.on_window_update(h, payload),
            kind::RST_STREAM => self.on_rst_stream(h, payload),
            kind::PING => self.on_ping(h, payload),
            kind::GOAWAY => self.on_goaway(h, payload),
            // PRIORITY is parsed for length and deliberately ignored
            // (RFC 9113 §5.3.1 deprecates the priority scheme).
            kind::PRIORITY => {
                if h.length != 5 || h.stream_id == 0 {
                    return Err(H2Error::FrameSize);
                }
                Ok(())
            }
            kind::PUSH_PROMISE => Err(H2Error::Unsupported),
            // An unknown frame type must be ignored (RFC 9113 §4.1).
            _ => Ok(()),
        }
    }

    fn on_settings(&mut self, h: frame::FrameHeader, payload: &[u8]) -> Result<(), H2Error> {
        if h.stream_id != 0 {
            return Err(H2Error::Protocol);
        }
        if h.flags & flag::ACK != 0 {
            if h.length != 0 {
                return Err(H2Error::FrameSize);
            }
            self.events.push(Event::SettingsAck);
            return Ok(());
        }
        let pairs = frame::parse_settings(payload).ok_or(H2Error::FrameSize)?;
        for (id, v) in pairs {
            match id {
                setting::INITIAL_WINDOW_SIZE => {
                    if v as i64 > frame::MAX_WINDOW {
                        return Err(H2Error::FlowControl);
                    }
                    // RFC 9113 §6.9.2: the delta applies to every existing stream.
                    let delta = v as i64 - self.peer_initial_window as i64;
                    self.peer_initial_window = v;
                    for s in self.streams.values_mut() {
                        s.send_window += delta;
                        if s.send_window > frame::MAX_WINDOW {
                            return Err(H2Error::FlowControl);
                        }
                    }
                }
                setting::MAX_FRAME_SIZE => {
                    if !(frame::DEFAULT_MAX_FRAME_SIZE..=0x00ff_ffff).contains(&v) {
                        return Err(H2Error::Protocol);
                    }
                    self.peer_max_frame_size = v;
                }
                setting::HEADER_TABLE_SIZE
                | setting::MAX_CONCURRENT_STREAMS
                | setting::MAX_HEADER_LIST_SIZE => {}
                // We never offer push, and a peer may only send 0 or 1.
                setting::ENABLE_PUSH if v > 1 => return Err(H2Error::Protocol),
                setting::ENABLE_PUSH => {}
                // Unknown settings are ignored (RFC 9113 §6.5.2).
                _ => {}
            }
        }
        let ack = frame::build_settings_ack();
        self.out.extend_from_slice(&ack);
        self.events.push(Event::Settings);
        Ok(())
    }

    fn on_headers(&mut self, h: frame::FrameHeader, payload: &[u8]) -> Result<(), H2Error> {
        if h.stream_id == 0 {
            return Err(H2Error::Protocol);
        }
        let block = frame::strip_padding(payload, h.flags, true).ok_or(H2Error::Protocol)?;
        if self.streams.len() >= MAX_STREAMS && !self.streams.contains_key(&h.stream_id) {
            return Err(H2Error::Protocol);
        }
        {
            let iw = self.peer_initial_window as i64;
            let ow = self.our_initial_window as i64;
            let s = self
                .streams
                .entry(h.stream_id)
                .or_insert_with(|| Stream::new(iw, ow));
            if s.got_headers {
                // A second header block would be trailers - not supported.
                return Err(H2Error::Unsupported);
            }
            s.got_headers = true;
            s.state = match (s.state, h.flags & flag::END_STREAM != 0) {
                (StreamState::Idle, false) => StreamState::Open,
                (StreamState::Idle, true) => StreamState::HalfClosedRemote,
                (StreamState::Open, true) => StreamState::HalfClosedRemote,
                (StreamState::HalfClosedLocal, true) => StreamState::Closed,
                (st, _) => st,
            };
        }
        let end_stream = h.flags & flag::END_STREAM != 0;
        if h.flags & flag::END_HEADERS != 0 {
            self.deliver_headers(h.stream_id, block.to_vec(), end_stream)
        } else {
            self.cont = Some((h.stream_id, block.to_vec(), end_stream));
            Ok(())
        }
    }

    fn on_continuation(&mut self, h: frame::FrameHeader, payload: &[u8]) -> Result<(), H2Error> {
        let Some((sid, mut acc, end_stream)) = self.cont.take() else {
            return Err(H2Error::Protocol);
        };
        if sid != h.stream_id {
            return Err(H2Error::Protocol);
        }
        acc.extend_from_slice(payload);
        if h.flags & flag::END_HEADERS != 0 {
            self.deliver_headers(sid, acc, end_stream)
        } else {
            self.cont = Some((sid, acc, end_stream));
            Ok(())
        }
    }

    fn deliver_headers(
        &mut self,
        stream_id: u32,
        block: Vec<u8>,
        end_stream: bool,
    ) -> Result<(), H2Error> {
        let headers = self.dec.decode(&block).map_err(H2Error::Compression)?;
        self.events.push(Event::Headers {
            stream_id,
            headers,
            end_stream,
        });
        Ok(())
    }

    fn on_data(&mut self, h: frame::FrameHeader, payload: &[u8]) -> Result<(), H2Error> {
        if h.stream_id == 0 {
            return Err(H2Error::Protocol);
        }
        let data = frame::strip_padding(payload, h.flags, false).ok_or(H2Error::Protocol)?;
        // Flow control is charged on the **whole** payload including padding
        // (RFC 9113 §6.1), which is why the debit uses `h.length`.
        self.conn_recv_window -= h.length as i64;
        if self.conn_recv_window < 0 {
            return Err(H2Error::FlowControl);
        }
        let end_stream = h.flags & flag::END_STREAM != 0;
        {
            let Some(s) = self.streams.get_mut(&h.stream_id) else {
                return Err(H2Error::Protocol);
            };
            if matches!(s.state, StreamState::Idle | StreamState::Closed) {
                return Err(H2Error::Protocol);
            }
            s.recv_window -= h.length as i64;
            if s.recv_window < 0 {
                return Err(H2Error::FlowControl);
            }
            if end_stream {
                s.state = match s.state {
                    StreamState::HalfClosedLocal => StreamState::Closed,
                    _ => StreamState::HalfClosedRemote,
                };
            }
        }
        self.events.push(Event::Data {
            stream_id: h.stream_id,
            data: data.to_vec(),
            end_stream,
        });
        Ok(())
    }

    fn on_window_update(&mut self, h: frame::FrameHeader, payload: &[u8]) -> Result<(), H2Error> {
        if h.length != 4 {
            return Err(H2Error::FrameSize);
        }
        let inc =
            u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) & 0x7fff_ffff;
        // A zero increment is a protocol error (RFC 9113 §6.9).
        if inc == 0 {
            return Err(H2Error::Protocol);
        }
        if h.stream_id == 0 {
            self.conn_send_window += inc as i64;
            if self.conn_send_window > frame::MAX_WINDOW {
                return Err(H2Error::FlowControl);
            }
        } else {
            let Some(s) = self.streams.get_mut(&h.stream_id) else {
                return Err(H2Error::Protocol);
            };
            s.send_window += inc as i64;
            if s.send_window > frame::MAX_WINDOW {
                return Err(H2Error::FlowControl);
            }
        }
        self.events.push(Event::WindowUpdate {
            stream_id: h.stream_id,
            increment: inc,
        });
        // The credit may have unblocked queued DATA.
        self.flush();
        Ok(())
    }

    fn on_rst_stream(&mut self, h: frame::FrameHeader, payload: &[u8]) -> Result<(), H2Error> {
        if h.length != 4 {
            return Err(H2Error::FrameSize);
        }
        if h.stream_id == 0 {
            return Err(H2Error::Protocol);
        }
        let code = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        if let Some(s) = self.streams.get_mut(&h.stream_id) {
            s.state = StreamState::Closed;
            s.pending.clear();
            s.pending_end = false;
        }
        self.events.push(Event::StreamReset {
            stream_id: h.stream_id,
            error_code: code,
        });
        Ok(())
    }

    fn on_ping(&mut self, h: frame::FrameHeader, payload: &[u8]) -> Result<(), H2Error> {
        if h.length != 8 || h.stream_id != 0 {
            return Err(H2Error::FrameSize);
        }
        let mut p = [0u8; 8];
        p.copy_from_slice(payload);
        let ack = h.flags & flag::ACK != 0;
        if !ack {
            let f = frame::build_ping(p, true);
            self.out.extend_from_slice(&f);
        }
        self.events.push(Event::Ping { payload: p, ack });
        Ok(())
    }

    fn on_goaway(&mut self, h: frame::FrameHeader, payload: &[u8]) -> Result<(), H2Error> {
        if h.length < 8 || h.stream_id != 0 {
            return Err(H2Error::Protocol);
        }
        self.goaway_received = true;
        self.events.push(Event::GoAway {
            last_stream_id: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                & 0x7fff_ffff,
            error_code: u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]),
        });
        Ok(())
    }

    // ---- inspection (the deterministic proof reads these) ----

    /// True if this endpoint is a server.
    pub fn is_server(&self) -> bool {
        self.is_server
    }
    /// The connection-level send window.
    pub fn conn_send_window(&self) -> i64 {
        self.conn_send_window
    }
    /// A stream's send window, if the stream exists.
    pub fn stream_send_window(&self, stream_id: u32) -> Option<i64> {
        self.streams.get(&stream_id).map(|s| s.send_window)
    }
    /// A stream's state, if the stream exists.
    pub fn stream_state(&self, stream_id: u32) -> Option<StreamState> {
        self.streams.get(&stream_id).map(|s| s.state)
    }
    /// DATA bytes still queued on a stream behind flow control.
    pub fn stream_pending(&self, stream_id: u32) -> usize {
        self.streams.get(&stream_id).map_or(0, |s| s.pending.len())
    }
    /// Number of tracked streams.
    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }
    /// True once a GOAWAY has been received.
    pub fn goaway_received(&self) -> bool {
        self.goaway_received
    }
    /// The HPACK encoder's dynamic table (RFC 7541 Appendix C size checks).
    pub fn hpack_encoder_table(&self) -> &hpack::Table {
        self.enc.table()
    }
    /// The HPACK decoder's dynamic table.
    pub fn hpack_decoder_table(&self) -> &hpack::Table {
        self.dec.table()
    }
}

/// Our SETTINGS: advertise our receive window, and disable push (this slice does
/// not implement it, so it must not be offered).
fn c_settings(initial_window: u32) -> Vec<u8> {
    frame::build_settings(&[
        (setting::ENABLE_PUSH, 0),
        (setting::INITIAL_WINDOW_SIZE, initial_window),
        (setting::MAX_CONCURRENT_STREAMS, MAX_STREAMS as u32),
    ])
}
