//! Traceroute: the TTL-increment state machine (docs/NETSTACK.md, the TTL /
//! hop-limit / traceroute section). N1b shipped only the settable-TTL hook; N1e
//! ships the whole loop.
//!
//! ## The probe method (documented choice)
//! rheo-net traceroute probes with an **ICMP echo request whose sequence number
//! equals the TTL**. That is the scheme Windows `tracert` uses, and it is the
//! robust one here: RFC 792 guarantees a Time Exceeded echoes back the offending
//! IP header **plus its first 8 bytes**, and for an ICMP-echo probe those 8 bytes
//! are the echo header - so the **sequence number (= the hop) survives inside the
//! router's reply**, giving a correlation key without any extra state. (Classic
//! Unix traceroute uses UDP to a per-hop destination port; that also works, but
//! the echo scheme keeps send + correlate in one module and reuses the existing
//! [`crate::icmp::IcmpEndpoint`].)
//!
//! ## Deterministic core, thin live driver
//! [`Tracer`] is a pure state machine with **no I/O**: it hands out the next probe
//! TTL, consumes already-correlated [`Response`]s, and reconstructs the ordered
//! hop list. That is what makes multi-hop discovery provable **without real
//! intermediate routers** - feed it a crafted sequence of Time Exceededs then a
//! destination reply and it reconstructs the path. [`Tracer::run`] is the thin
//! live driver over an [`IcmpEndpoint`] (send probe, [`recv_trace`], record).
//!
//! [`recv_trace`]: crate::icmp::IcmpEndpoint::recv_trace

use alloc::vec::Vec;

use crate::icmp::{self, IcmpEndpoint};
use crate::ip::{self, Ipv4Addr, Ipv4Header};
use crate::wire::WireError;

/// The default maximum number of hops a trace probes before giving up (the
/// classic traceroute bound).
pub const DEFAULT_MAX_HOPS: u8 = 30;

/// Traceroute parameters.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Stop after this many hops even if the destination was never reached.
    pub max_hops: u8,
    /// The ICMP echo identifier stamped on every probe (and matched on replies).
    pub ident: u16,
    /// Retransmits per hop before recording a timeout for that hop.
    pub attempts: u32,
}

impl Config {
    /// Defaults: 30 hops, 3 attempts per hop, the given echo `ident`.
    pub const fn new(ident: u16) -> Config {
        Config {
            max_hops: DEFAULT_MAX_HOPS,
            ident,
            attempts: 3,
        }
    }
}

/// One discovered hop on the path.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Hop {
    /// The TTL of the probe that reached it (== its position on the path).
    pub ttl: u8,
    /// The responding node's IPv4 address (`0.0.0.0` for a silent hop / timeout).
    pub addr: Ipv4Addr,
    /// True if this hop is the **destination** (an Echo Reply, not a Time
    /// Exceeded) - the trace terminates here.
    pub reached: bool,
}

/// A response the state machine consumes, already correlated to a probe by the
/// echo sequence number ([`classify`] produces these from received ICMP).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Response {
    /// An intermediate router's ICMP Time Exceeded (its embedded original echoes
    /// our probe's `seq`).
    TimeExceeded { seq: u16, from: Ipv4Addr },
    /// The destination's Echo Reply (its own header carries the `seq`).
    Reply { seq: u16, from: Ipv4Addr },
}

/// Correlate a received ICMP message to a traceroute probe (the pure classifier
/// [`crate::icmp::IcmpEndpoint::recv_trace`] runs). `msg` is the ICMP message
/// extracted from an IPv4 frame, `from` is that frame's IPv4 source (the
/// responding node), `ident` is our echo identifier. Returns:
/// - [`Response::Reply`] if `msg` is an Echo Reply (type 0) whose ident matches -
///   the destination answered;
/// - [`Response::TimeExceeded`] if `msg` is a Time Exceeded (type 11) whose
///   **embedded original** is one of our echo requests (ident matches) - the seq
///   comes from that embedded echo header, naming the hop;
/// - `None` for anything not ours / malformed.
pub fn classify(msg: &[u8], from: Ipv4Addr, ident: u16) -> Option<Response> {
    let outer = icmp::parse_echo(msg)?;
    // The destination: a direct Echo Reply carrying our ident.
    if outer.msg_type == icmp::ECHO_REPLY && outer.ident == ident {
        return Some(Response::Reply {
            seq: outer.seq,
            from,
        });
    }
    // An intermediate router: a Time Exceeded quoting our echo request back.
    if outer.msg_type == icmp::TIME_EXCEEDED {
        let err = icmp::parse_error(msg)?;
        let orig = err.original;
        // The quoted original must be at least an IP header + the 8-byte echo hdr.
        if orig.len() < ip::IPV4_HEADER_LEN + icmp::HEADER_LEN {
            return None;
        }
        let inner_ip = Ipv4Header::parse(orig)?;
        if inner_ip.protocol != ip::proto::ICMP {
            return None;
        }
        let inner = icmp::parse_echo(&orig[ip::IPV4_HEADER_LEN..])?;
        if inner.msg_type == icmp::ECHO_REQUEST && inner.ident == ident {
            return Some(Response::TimeExceeded {
                seq: inner.seq,
                from,
            });
        }
    }
    None
}

/// The traceroute state machine (deterministic; no I/O).
pub struct Tracer {
    dst: Ipv4Addr,
    config: Config,
    hops: Vec<Hop>,
    next_ttl: u8,
    done: bool,
}

impl Tracer {
    /// A tracer toward `dst` with `config`. The first probe is TTL 1.
    pub fn new(dst: Ipv4Addr, config: Config) -> Tracer {
        Tracer {
            dst,
            config,
            hops: Vec::new(),
            next_ttl: 1,
            done: false,
        }
    }

    /// The destination being traced.
    pub fn dst(&self) -> Ipv4Addr {
        self.dst
    }

    /// The echo identifier stamped on probes.
    pub fn ident(&self) -> u16 {
        self.config.ident
    }

    /// True once the destination replied or `max_hops` was exhausted.
    pub fn done(&self) -> bool {
        self.done
    }

    /// The ordered hop list discovered so far.
    pub fn hops(&self) -> &[Hop] {
        &self.hops
    }

    /// The TTL (== echo sequence) of the next probe to send, or `None` when the
    /// trace is finished (destination reached or `max_hops` exhausted).
    pub fn next_probe(&self) -> Option<u8> {
        if self.done || self.next_ttl == 0 || self.next_ttl > self.config.max_hops {
            None
        } else {
            Some(self.next_ttl)
        }
    }

    /// Feed one correlated response. Records the hop (seq -> ttl, keeping the
    /// first responder seen for a TTL), advances past it, and - for a `Reply` from
    /// the destination - terminates the trace.
    pub fn record(&mut self, resp: Response) {
        let (seq, from, reached) = match resp {
            Response::TimeExceeded { seq, from } => (seq, from, false),
            Response::Reply { seq, from } => (seq, from, true),
        };
        let ttl = seq as u8;
        self.insert(ttl, from, reached);
        if reached {
            self.done = true;
        }
    }

    /// Record a per-hop timeout (no response for the probe at `ttl`): a silent hop
    /// (address `0.0.0.0`), so one unresponsive router does not stall the trace.
    pub fn record_timeout(&mut self, ttl: u8) {
        self.insert(ttl, Ipv4Addr::new(0, 0, 0, 0), false);
    }

    /// Insert or update a hop and advance `next_ttl` past it.
    fn insert(&mut self, ttl: u8, addr: Ipv4Addr, reached: bool) {
        if let Some(h) = self.hops.iter_mut().find(|h| h.ttl == ttl) {
            // Fill in a previously-silent hop if a real responder arrives late.
            if h.addr == Ipv4Addr::new(0, 0, 0, 0) {
                h.addr = addr;
                h.reached = reached;
            }
        } else {
            self.hops.push(Hop { ttl, addr, reached });
        }
        if ttl >= self.next_ttl {
            self.next_ttl = ttl.saturating_add(1);
        }
    }

    /// Drive the trace **live** over `ep`: for each hop, set the probe TTL, send an
    /// ICMP echo (seq == TTL), and wait for a response (bounded, retried per
    /// `attempts`); a per-hop miss is recorded as a timeout so the trace
    /// continues. Stops at the destination or `max_hops`. `payload` is the echo
    /// body. Returns `WireError::Net` only on a hard transport failure.
    pub async fn run(&mut self, ep: &mut IcmpEndpoint, payload: &[u8]) -> Result<(), WireError> {
        while let Some(ttl) = self.next_probe() {
            ep.set_ttl(ttl);
            let mut got = None;
            for _ in 0..self.config.attempts.max(1) {
                if ep
                    .send_echo(self.dst, self.config.ident, ttl as u16, payload)
                    .await
                    .is_err()
                {
                    continue;
                }
                match ep.recv_trace(self.config.ident).await {
                    Ok(r) => {
                        got = Some(r);
                        break;
                    }
                    Err(WireError::Net) => return Err(WireError::Net),
                    Err(_) => continue,
                }
            }
            match got {
                Some(r) => self.record(r),
                None => self.record_timeout(ttl),
            }
        }
        Ok(())
    }
}
