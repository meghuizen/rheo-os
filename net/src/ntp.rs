//! `net::ntp` - an **SNTP / NTPv4 client** (RFC 5905, client subset), userspace,
//! from scratch (docs/NETSTACK.md, rheo-net Phase N4c). The 48-byte packet codec,
//! the four-timestamp offset and round-trip-delay computation, poll backoff, and -
//! the part that matters doctrinally - a result expressed as a **bounded interval**
//! rather than a false instant.
//!
//! ## What this does and does **not** do (read this first)
//! This client computes an **offset** between a remote server's clock and a
//! timestamp pair we supply, and reports it with an error bound. It does **not**:
//!
//! - **discipline a system clock.** Nothing here steps or slews any clock. There is
//!   no `settimeofday` equivalent in this OS, by design: the kernel's wall clock is
//!   `kernel::time::wall()`, which already returns an interval and is explicitly
//!   unsynced. A caller that wants "the time" adds [`Estimate::center_unix_ns`] to
//!   its own reading and keeps the error bound. Anything that reads like "the system
//!   time is now correct" would be a lie.
//! - **authenticate anything.** There is no MAC, no NTS, no key. An unauthenticated
//!   time source is trivially spoofable by anyone on the path, so this is suitable
//!   for coarse convenience, never for anything security-relevant (certificate
//!   lifetimes, replay windows, audit ordering). **NTS (RFC 8915) and PTP are
//!   deferred**, per the design's time-and-identity posture
//!   (docs/TIME-IDENTITY.md): authenticated and high-precision time are their own
//!   phase.
//! - **filter or combine several servers.** RFC 5905's clock filter, selection,
//!   clustering and combining algorithms - the part of NTP that makes it robust
//!   against one lying server - are **not** implemented. This is the SNTP subset:
//!   one server, one sample, one offset. That is stated here rather than implied by
//!   omission.
//!
//! ## The interval, and why it is the honest shape
//! `kernel::time::wall()` models wall time as `[center - error, center + error]`
//! (docs/ARCHITECTURE.md 4.5) precisely so no caller mistakes a reading for a
//! precise instant. An NTP sample has exactly the same character: the offset is only
//! known to within the round trip, because we cannot tell how much of the delay was
//! outbound and how much inbound. So [`Sample::estimate`] returns an [`Estimate`]
//! with the same `center`/`error` shape, where
//!
//! ```text
//! error = delay/2 + root_dispersion + root_delay/2
//! ```
//!
//! The first term is the local uncertainty (worst case: the whole round trip was in
//! one direction), and the other two are the server's own declared distance from its
//! reference clock. Reporting the offset as a bare number would throw away exactly
//! the information the design says to keep.
//!
//! ## The packet
//! 48 bytes, all big-endian:
//!
//! ```text
//! [LI 2b | VN 3b | Mode 3b][stratum 1][poll 1][precision 1]
//! [root delay 4][root dispersion 4][reference id 4]
//! [reference timestamp 8][originate timestamp 8]
//! [receive timestamp 8][transmit timestamp 8]
//! ```
//!
//! An **NTP timestamp** is 64 bits of 32.32 fixed point: seconds since
//! **1900-01-01**, then a binary fraction. `root delay` and `root dispersion` are
//! 16.16 fixed point seconds. [`Timestamp`] and the two `*_ns` helpers do all the
//! scaling, so no caller multiplies by `2^32` by hand.
//!
//! ## The four timestamps
//! ```text
//! T1 = our transmit  (we write it into the request's transmit field)
//! T2 = server receive
//! T3 = server transmit
//! T4 = our receive
//!
//! offset = ((T2 - T1) + (T3 - T4)) / 2
//! delay  = (T4 - T1) - (T3 - T2)
//! ```
//! The server echoes our T1 back in the **originate** field, which is what lets a
//! reply be matched to a request - and checking it is what stops an off-path attacker
//! injecting an answer to a request it never saw ([`NtpError::OriginateMismatch`]).
//!
//! ## Where T1 and T4 come from (an honest limitation)
//! A cell's clock is `librheo::time::Instant` - opaque **monotonic ticks**, with no
//! userspace ticks-per-second and no wall-clock reading. So a cell cannot, today,
//! fill in T1 and T4 in NTP's units, which means the *live* path cannot compute a
//! real offset even if a server answered. The arithmetic is therefore exposed as
//! [`Sample::compute`] over caller-supplied timestamps, and it is proven against a
//! hand-computed known-answer test. A live sync additionally needs a
//! ticks-to-nanoseconds calibration in userspace; that is named as future work in
//! docs/NETSTACK.md rather than faked here. What [`Client`] *does* own is the poll
//! schedule, which is pure monotonic timing and works fine.
//!
//! ## Postures
//! Codec + arithmetic + the poll schedule are **always compiled**. Only [`query`],
//! the async round trip over the NIC, is behind the `hosted` feature.

#[cfg(feature = "hosted")]
use crate::ip::Ipv4Addr;

/// The NTP UDP port.
pub const PORT: u16 = 123;
/// An NTP packet without extensions or a MAC: exactly 48 bytes.
pub const PACKET_LEN: usize = 48;
/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01) -
/// 70 years including 17 leap days.
pub const NTP_TO_UNIX_SECS: u64 = 2_208_988_800;

/// Mode 3: a client's request.
pub const MODE_CLIENT: u8 = 3;
/// Mode 4: a server's reply.
pub const MODE_SERVER: u8 = 4;
/// Leap indicator 3: the server's clock is **not synchronized**. Its time is not
/// usable.
pub const LI_UNSYNC: u8 = 3;
/// The highest usable stratum (RFC 5905 §7.2: 16 means unsynchronized).
pub const MAX_STRATUM: u8 = 15;

/// Nanoseconds per second.
const NS_PER_SEC: i128 = 1_000_000_000;

/// An NTP timestamp: 32.32 fixed point seconds since 1900-01-01.
///
/// Held as the raw 64-bit wire value so no precision is lost, with helpers to move
/// between it, nanoseconds and the Unix epoch. It wraps in 2036 (the "NTP era"
/// problem); era disambiguation is deferred and noted in docs/NETSTACK.md.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Timestamp(pub u64);

impl Timestamp {
    /// The zero timestamp, which on the wire means "unset".
    pub const ZERO: Timestamp = Timestamp(0);

    /// From whole seconds since 1900 plus a raw 32-bit fraction.
    pub const fn from_parts(secs: u32, frac: u32) -> Timestamp {
        Timestamp(((secs as u64) << 32) | frac as u64)
    }

    /// From seconds since the **Unix** epoch plus nanoseconds.
    pub fn from_unix(secs: u64, nanos: u32) -> Timestamp {
        let ntp_secs = secs.wrapping_add(NTP_TO_UNIX_SECS) as u32;
        // frac = nanos * 2^32 / 1e9, rounded down.
        let frac = ((nanos as u128 * (1u128 << 32)) / 1_000_000_000u128) as u32;
        Timestamp::from_parts(ntp_secs, frac)
    }

    /// The whole-seconds part (since 1900).
    pub const fn secs(self) -> u32 {
        (self.0 >> 32) as u32
    }

    /// The raw 32-bit fraction.
    pub const fn frac(self) -> u32 {
        self.0 as u32
    }

    /// True if this is the zero (unset) timestamp.
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Nanoseconds since 1900, as a wide signed integer so differences never wrap.
    pub fn as_ns_since_1900(self) -> i128 {
        fixed32_to_ns(self.0 as i128)
    }

    /// Nanoseconds since the **Unix** epoch. Negative for a timestamp before 1970.
    pub fn as_unix_ns(self) -> i128 {
        self.as_ns_since_1900() - (NTP_TO_UNIX_SECS as i128) * NS_PER_SEC
    }
}

/// Convert a 32.32 fixed-point value (as a signed wide integer) to nanoseconds.
fn fixed32_to_ns(v: i128) -> i128 {
    // v / 2^32 seconds -> ns. Done as one multiply then one shift so nothing is
    // lost to intermediate truncation.
    (v * NS_PER_SEC) >> 32
}

/// Convert a 16.16 fixed-point value (root delay / root dispersion) to nanoseconds.
fn fixed16_to_ns(v: u32) -> u64 {
    (((v as u128) * (NS_PER_SEC as u128)) >> 16) as u64
}

/// Why an NTP reply was rejected. Each shape is its own value so a proof asserts
/// the *reason*, not just the refusal.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NtpError {
    /// Not 48 bytes.
    Short,
    /// The mode field is not [`MODE_SERVER`] - somebody sent us a request, or a
    /// broadcast/control packet.
    BadMode,
    /// The version is neither 3 nor 4.
    BadVersion,
    /// Stratum 0: a **Kiss-o'-Death** - the server is refusing service and the
    /// reference id says why (`DENY`, `RSTR`, `RATE`). A client that keeps polling
    /// through a KoD is abusing the server.
    KissOfDeath,
    /// Stratum above [`MAX_STRATUM`]: the server is not synchronized.
    BadStratum,
    /// Leap indicator 3: the server says its own clock is unsynchronized.
    Unsynchronized,
    /// The transmit timestamp is zero - there is no time in this reply.
    ZeroTransmit,
    /// The originate timestamp is not the T1 we sent: this reply does not belong to
    /// our request.
    OriginateMismatch,
    /// The timestamps are not monotone (`T3 < T2`, or `T4 < T1`), so the sample is
    /// nonsense.
    BadTimestamps,
    /// The raw-frame transport failed (`hosted` driver only).
    Net,
    /// No reply arrived in the budget (`hosted` driver only).
    Timeout,
}

/// A Kiss-o'-Death code (the 4-byte reference id of a stratum-0 reply).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct KodCode(pub [u8; 4]);

impl KodCode {
    /// "Access denied permanently."
    pub const DENY: KodCode = KodCode(*b"DENY");
    /// "Access denied by restriction."
    pub const RSTR: KodCode = KodCode(*b"RSTR");
    /// "Slow down - you are polling too fast."
    pub const RATE: KodCode = KodCode(*b"RATE");
}

/// A parsed NTP packet.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// Leap indicator (0 no warning, 1/2 a leap second, 3 unsynchronized).
    pub li: u8,
    /// Version number (3 or 4).
    pub vn: u8,
    /// Mode (3 client, 4 server).
    pub mode: u8,
    /// Distance from the reference clock in hops (1 = a directly attached clock).
    pub stratum: u8,
    /// The server's poll interval, as a power of two seconds.
    pub poll: i8,
    /// The server's clock precision, as a power of two seconds.
    pub precision: i8,
    /// Total round-trip delay to the reference clock, 16.16 fixed seconds.
    pub root_delay: u32,
    /// Maximum error relative to the reference clock, 16.16 fixed seconds.
    pub root_dispersion: u32,
    /// The reference identifier (a source name, or a KoD code at stratum 0).
    pub ref_id: [u8; 4],
    /// When the server's clock was last set.
    pub reference: Timestamp,
    /// T1 - the client's transmit time, echoed back.
    pub originate: Timestamp,
    /// T2 - when the server received the request.
    pub receive: Timestamp,
    /// T3 - when the server sent the reply.
    pub transmit: Timestamp,
}

fn ts_at(buf: &[u8], off: usize) -> Timestamp {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    Timestamp(u64::from_be_bytes(b))
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// Parse a 48-byte NTP packet. Structural only - the *semantic* checks (mode,
/// version, stratum, leap indicator, KoD) are [`Packet::validate`], so a caller can
/// inspect a refused packet's fields.
pub fn parse(buf: &[u8]) -> Result<Packet, NtpError> {
    if buf.len() < PACKET_LEN {
        return Err(NtpError::Short);
    }
    let b0 = buf[0];
    Ok(Packet {
        li: b0 >> 6,
        vn: (b0 >> 3) & 0x07,
        mode: b0 & 0x07,
        stratum: buf[1],
        poll: buf[2] as i8,
        precision: buf[3] as i8,
        root_delay: u32_at(buf, 4),
        root_dispersion: u32_at(buf, 8),
        ref_id: [buf[12], buf[13], buf[14], buf[15]],
        reference: ts_at(buf, 16),
        originate: ts_at(buf, 24),
        receive: ts_at(buf, 32),
        transmit: ts_at(buf, 40),
    })
}

impl Packet {
    /// The Kiss-o'-Death code, if this is a stratum-0 refusal.
    pub fn kod(&self) -> Option<KodCode> {
        if self.stratum == 0 {
            Some(KodCode(self.ref_id))
        } else {
            None
        }
    }

    /// Root delay in nanoseconds.
    pub fn root_delay_ns(&self) -> u64 {
        fixed16_to_ns(self.root_delay)
    }

    /// Root dispersion in nanoseconds.
    pub fn root_dispersion_ns(&self) -> u64 {
        fixed16_to_ns(self.root_dispersion)
    }

    /// Reject a reply that must not be used as a time source. Order matters: the
    /// checks a *hostile or broken* peer would trip come before the ones a
    /// well-meaning one would.
    pub fn validate(&self, sent: Timestamp) -> Result<(), NtpError> {
        if self.mode != MODE_SERVER {
            return Err(NtpError::BadMode);
        }
        if self.vn != 3 && self.vn != 4 {
            return Err(NtpError::BadVersion);
        }
        if self.stratum == 0 {
            return Err(NtpError::KissOfDeath);
        }
        if self.stratum > MAX_STRATUM {
            return Err(NtpError::BadStratum);
        }
        if self.li == LI_UNSYNC {
            return Err(NtpError::Unsynchronized);
        }
        if self.transmit.is_zero() {
            return Err(NtpError::ZeroTransmit);
        }
        if !sent.is_zero() && self.originate != sent {
            return Err(NtpError::OriginateMismatch);
        }
        Ok(())
    }
}

/// Build a client request whose transmit timestamp is `transmit` (our T1).
///
/// Version 4, mode 3, everything else zero: stratum, poll and precision carry no
/// information from a client, and RFC 4330 §4 explicitly allows an SNTP client to
/// leave them zero. Keeping them zero also makes the encoding a fixed, assertable
/// 48 bytes.
pub fn build_client_request(transmit: Timestamp) -> [u8; PACKET_LEN] {
    let mut p = [0u8; PACKET_LEN];
    // LI 0, VN 4, Mode 3 -> 0b00_100_011 = 0x23.
    p[0] = (4 << 3) | MODE_CLIENT;
    p[40..48].copy_from_slice(&transmit.0.to_be_bytes());
    p
}

/// A time estimate as a **bounded interval**, the same shape as
/// `kernel::time::Interval`: the truth is asserted to lie in
/// `[center - error, center + error]` and nowhere is a bare instant offered. See
/// the module docs for why.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// The midpoint, in nanoseconds since the Unix epoch. Wide and signed so
    /// pre-1970 and far-future values are representable without wrapping.
    pub center_unix_ns: i128,
    /// The half-width of the interval, in nanoseconds. Never zero for a real
    /// sample - a zero bound would claim perfect knowledge.
    pub error_ns: u64,
}

impl Estimate {
    /// The earliest time the sample allows.
    pub fn lower_unix_ns(&self) -> i128 {
        self.center_unix_ns - self.error_ns as i128
    }

    /// The latest time the sample allows.
    pub fn upper_unix_ns(&self) -> i128 {
        self.center_unix_ns + self.error_ns as i128
    }

    /// The full interval width.
    pub fn width_ns(&self) -> u64 {
        self.error_ns.saturating_mul(2)
    }

    /// True if `unix_ns` is inside the interval.
    pub fn contains(&self, unix_ns: i128) -> bool {
        unix_ns >= self.lower_unix_ns() && unix_ns <= self.upper_unix_ns()
    }
}

/// One NTP measurement: the clock offset and the round-trip delay.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// How far the server's clock is **ahead** of ours, in nanoseconds (negative if
    /// behind).
    pub offset_ns: i64,
    /// The round-trip delay, in nanoseconds, with the server's own processing time
    /// removed.
    pub delay_ns: u64,
    /// The server's declared distance from its reference clock (root dispersion +
    /// half the root delay), in nanoseconds - the part of the error bound that is
    /// *its* uncertainty rather than ours.
    pub root_distance_ns: u64,
    /// The stratum the sample came from.
    pub stratum: u8,
}

impl Sample {
    /// Compute a sample from the four timestamps.
    ///
    /// ```text
    /// offset = ((T2 - T1) + (T3 - T4)) / 2
    /// delay  = (T4 - T1) - (T3 - T2)
    /// ```
    ///
    /// The arithmetic runs in `i128` **nanoseconds** derived from the raw 32.32
    /// values, so no intermediate wraps and no float. A negative computed delay
    /// (which a broken or lying server can produce) is clamped to zero rather than
    /// underflowing an unsigned type - and `T3 < T2` or `T4 < T1` is rejected
    /// outright as [`NtpError::BadTimestamps`].
    pub fn compute(pkt: &Packet, t1: Timestamp, t4: Timestamp) -> Result<Sample, NtpError> {
        let t1n = t1.as_ns_since_1900();
        let t2n = pkt.receive.as_ns_since_1900();
        let t3n = pkt.transmit.as_ns_since_1900();
        let t4n = t4.as_ns_since_1900();
        if t3n < t2n || t4n < t1n {
            return Err(NtpError::BadTimestamps);
        }
        let offset = ((t2n - t1n) + (t3n - t4n)) / 2;
        let delay = (t4n - t1n) - (t3n - t2n);
        let delay = if delay < 0 { 0 } else { delay };
        Ok(Sample {
            offset_ns: offset as i64,
            delay_ns: delay as u64,
            root_distance_ns: pkt
                .root_dispersion_ns()
                .saturating_add(pkt.root_delay_ns() / 2),
            stratum: pkt.stratum,
        })
    }

    /// Turn the sample into a bounded [`Estimate`] of the true time **at `t4`** (the
    /// moment the reply landed): the center is our reading corrected by the offset,
    /// and the error is
    /// `delay/2 + root_dispersion + root_delay/2` - see the module docs.
    pub fn estimate(&self, t4: Timestamp) -> Estimate {
        Estimate {
            center_unix_ns: t4.as_unix_ns() + self.offset_ns as i128,
            error_ns: (self.delay_ns / 2).saturating_add(self.root_distance_ns),
        }
    }
}

/// The default minimum poll interval as a power of two seconds (2^6 = 64 s, RFC
/// 5905 §7.2's `MINPOLL`). Polling faster than this is what earns a Kiss-o'-Death.
pub const MIN_POLL_EXP: u8 = 6;
/// The default maximum poll interval (2^10 = 1024 s, `MAXPOLL`).
pub const MAX_POLL_EXP: u8 = 10;

/// An SNTP client: the poll schedule plus the last accepted sample.
///
/// The schedule is the part that genuinely works on a cell's monotonic clock: it is
/// pure "how long until the next poll", which [`librheo::time::sleep`] can honour.
/// The offset it accumulates is a **userspace correction**, not a clock adjustment -
/// see the module docs.
pub struct Client {
    poll_exp: u8,
    min_poll_exp: u8,
    max_poll_exp: u8,
    /// The T1 of the request currently in flight (0 = none).
    in_flight: Timestamp,
    last: Option<Sample>,
    /// The correction a caller adds to its own wall reading, with its bound.
    offset_ns: i64,
    error_ns: u64,
    accepted: u32,
    rejected: u32,
}

impl Default for Client {
    fn default() -> Self {
        Client::new()
    }
}

impl Client {
    /// A client with the default `MINPOLL`/`MAXPOLL` schedule.
    pub fn new() -> Client {
        Client {
            poll_exp: MIN_POLL_EXP,
            min_poll_exp: MIN_POLL_EXP,
            max_poll_exp: MAX_POLL_EXP,
            in_flight: Timestamp::ZERO,
            last: None,
            offset_ns: 0,
            error_ns: 0,
            accepted: 0,
            rejected: 0,
        }
    }

    /// A client with an explicit poll range (both as powers of two seconds).
    pub fn with_poll_range(min_exp: u8, max_exp: u8) -> Client {
        let min = min_exp.min(max_exp);
        let max = max_exp.max(min_exp);
        Client {
            poll_exp: min,
            min_poll_exp: min,
            max_poll_exp: max,
            ..Client::new()
        }
    }

    /// The current poll interval in nanoseconds.
    pub fn poll_interval_ns(&self) -> u64 {
        (1u64 << self.poll_exp).saturating_mul(1_000_000_000)
    }

    /// The current poll exponent.
    pub fn poll_exp(&self) -> u8 {
        self.poll_exp
    }

    /// The last accepted sample.
    pub fn last_sample(&self) -> Option<&Sample> {
        self.last.as_ref()
    }

    /// The accumulated **userspace** offset correction in nanoseconds, with its
    /// error bound. Nothing has been applied to any clock; the caller adds this to
    /// its own reading and keeps the bound.
    pub fn correction(&self) -> (i64, u64) {
        (self.offset_ns, self.error_ns)
    }

    /// Samples accepted / rejected so far.
    pub fn counts(&self) -> (u32, u32) {
        (self.accepted, self.rejected)
    }

    /// Build the next request, recording its T1 so the reply's originate field can be
    /// checked.
    pub fn build_request(&mut self, t1: Timestamp) -> [u8; PACKET_LEN] {
        self.in_flight = t1;
        build_client_request(t1)
    }

    /// Feed a reply received at `t4`: validate it, compute the sample, and update the
    /// correction and the poll schedule.
    ///
    /// A good sample resets the poll interval to `MINPOLL`... except after a
    /// **Kiss-o'-Death**, where the interval is **doubled toward `MAXPOLL`** instead:
    /// a KoD means "you are asking too often", so backing off is the only correct
    /// response, and ignoring it is how a client gets blocked.
    pub fn on_reply(&mut self, buf: &[u8], t4: Timestamp) -> Result<Sample, NtpError> {
        let pkt = parse(buf)?;
        if let Err(e) = pkt.validate(self.in_flight) {
            self.rejected += 1;
            if e == NtpError::KissOfDeath {
                self.back_off();
            }
            return Err(e);
        }
        let sample = Sample::compute(&pkt, self.in_flight, t4)?;
        let est = sample.estimate(t4);
        self.offset_ns = sample.offset_ns;
        self.error_ns = est.error_ns;
        self.last = Some(sample);
        self.accepted += 1;
        self.in_flight = Timestamp::ZERO;
        self.poll_exp = self.min_poll_exp;
        Ok(sample)
    }

    /// No reply arrived: double the poll interval up to `MAXPOLL`.
    pub fn on_timeout(&mut self) {
        self.rejected += 1;
        self.back_off();
    }

    fn back_off(&mut self) {
        if self.poll_exp < self.max_poll_exp {
            self.poll_exp += 1;
        }
    }
}

/// How many datagrams [`query`] will look at before giving up: this only bounds "a
/// chatty link kept handing us datagrams from somebody else". The *time* budget is the
/// caller's `timeout_ns`.
#[cfg(feature = "hosted")]
pub const RECV_ATTEMPTS: u32 = 4;

/// The default reply deadline for [`query`]: **one second**, RFC 5905's usual
/// client timeout. A duration, not a poll count.
#[cfg(feature = "hosted")]
pub const REPLY_TIMEOUT_NS: u64 = 1_000_000_000;

/// Send one NTP request to `server` and return the reply bytes.
///
/// The thin `hosted` driver: it does the UDP round trip and nothing else - no
/// timestamping, because a cell has no wall clock to timestamp with (see the module
/// docs). The caller supplies T1, T4 and the reply deadline `timeout_ns`
/// ([`REPLY_TIMEOUT_NS`] is the sensible default).
///
/// The deadline is a **duration**, not a poll count: the receive parks in the kernel
/// ([`crate::udp::UdpEndpoint::recv_from_timeout`]), so waiting for a server that will
/// never answer costs a known amount of time and (where an interrupt can wake us) no
/// CPU at all.
///
/// QEMU's SLIRP runs no NTP service on the guest-visible side, so this returns
/// [`NtpError::Timeout`] there. The proof reports that as a skip and asserts the
/// arithmetic against a known-answer test instead; no time is ever synthesised.
#[cfg(feature = "hosted")]
pub async fn query(
    udp: &mut crate::udp::UdpEndpoint,
    server: Ipv4Addr,
    t1: Timestamp,
    reply: &mut [u8],
    timeout_ns: u64,
) -> Result<usize, NtpError> {
    let req = build_client_request(t1);
    let src_port = 0xC000 | (librheo::rng::next_u64() as u16 & 0x3FFF);
    udp.send_to(server, PORT, src_port, &req)
        .await
        .map_err(|_| NtpError::Net)?;
    let mut buf = [0u8; 128];
    for _ in 0..RECV_ATTEMPTS {
        match udp.recv_from_timeout(&mut buf, timeout_ns).await {
            Ok(r) if r.src_ip == server && r.src_port == PORT && r.len >= PACKET_LEN => {
                let len = core::cmp::min(r.len, reply.len());
                reply[..len].copy_from_slice(&buf[..len]);
                return Ok(len);
            }
            Ok(_) => continue,
            Err(_) => return Err(NtpError::Timeout),
        }
    }
    Err(NtpError::Timeout)
}
