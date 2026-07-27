//! `net::zeroconf` - **zero-configuration networking**, userspace (docs/NETSTACK.md,
//! rheo-net Phase N4c). Two halves, both from scratch:
//!
//! - **IPv4 link-local autoconfiguration** (RFC 3927): when no DHCP server answers,
//!   claim an address out of `169.254/16` yourself - pick a candidate, **ARP-probe**
//!   it to see whether anybody already has it, re-pick on a conflict, then
//!   **ARP-announce** the one you kept. [`LinkLocal`].
//! - **mDNS** (RFC 6762): resolve and answer `.local` names on the link with no
//!   server at all, over multicast UDP to `224.0.0.251:5353`. [`mdns`].
//!
//! ## Why link-local is an ARP state machine, not an address generator
//! Picking `169.254.x.y` is the trivial part. The reason RFC 3927 exists is the
//! **conflict protocol**, because two hosts picking the same address is not
//! unlikely - it is expected on a busy link. So:
//!
//! 1. Pick a candidate (pseudo-randomly, from `169.254.1.0`-`169.254.254.255`; the
//!    first and last /24 are reserved, RFC 3927 §2.1).
//! 2. Send [`PROBE_COUNT`] **ARP probes**: an ARP *request* for the candidate whose
//!    **sender address is `0.0.0.0`**. That zero sender is the whole trick - a
//!    normal ARP request would claim the address it is asking about, and a probe
//!    must not claim anything yet.
//! 3. A conflict is *either* an ARP reply/packet whose **sender address is the
//!    candidate** (someone owns it) *or* an ARP probe from a **different MAC for the
//!    same candidate** (someone is picking it right now). Both mean: re-pick and
//!    start over.
//! 4. No conflict: send [`ANNOUNCE_COUNT`] **ARP announcements** - an ARP request
//!    with sender *and* target set to the claimed address - so every neighbour's
//!    cache learns it. The address is then in use.
//! 5. After claiming, a later conflict is **defended once** (re-announce). A second
//!    conflict within [`DEFEND_INTERVAL_NS`] means the peer is not backing off, so
//!    we yield and re-pick (RFC 3927 §2.5). Defending forever is how two hosts
//!    ARP-storm a link.
//!
//! ## Randomness class (stated explicitly)
//! The candidate address comes from **splitmix64** - the same non-secret generator
//! DHCP transaction ids use. This is deliberate and it is the correct class:
//! docs/NETSTACK.md §3 splits randomness into *public* (cookies, ids, backoff
//! jitter, and this) and *key material* (which comes from `crypto::kdf` over the
//! attested per-cell DRBG, never from here). A link-local address is broadcast to
//! the entire link the moment it is announced; it is the definition of public. RFC
//! 3927 §2.1 suggests seeding from the MAC so a host tends to reclaim its previous
//! address, which [`LinkLocal::new`] does by mixing the MAC into the seed.
//!
//! ## mDNS: what is here and what is not
//! mDNS is **DNS messages with different transport and different semantics**, so
//! the codec is [`crate::dns`]'s, unchanged - that is why N4c made the DNS codec
//! posture-independent instead of writing a second name parser. What differs:
//!
//! | | unicast DNS | mDNS |
//! |---|---|---|
//! | transport | unicast UDP to a resolver | multicast UDP to `224.0.0.251:5353` |
//! | message id | random, matched | `0` (queries are not correlated by id) |
//! | recursion-desired | set | never |
//! | query class | `IN` | `IN`, top bit = **QU** ("answer by unicast") |
//! | record class | `IN` | `IN`, top bit = **cache-flush** |
//! | TTL 0 | meaningless | **goodbye**: drop this record |
//! | names | any | `*.local` only |
//!
//! All six of those are implemented ([`mdns::build_query`],
//! [`mdns::build_response`], [`crate::dns::Question::unicast_response`],
//! [`crate::dns::Record::cache_flush`], [`crate::dns::Record::is_goodbye`],
//! [`mdns::is_local`]).
//!
//! ### Multicast, and why no IGMP is built here
//! Sending to `224.0.0.251` needs nothing special: the IPv4 multicast address maps
//! deterministically onto the Ethernet multicast MAC `01:00:5e` + the low 23 bits
//! of the address ([`mdns::multicast_mac`], RFC 1112 §6.4), and the frame goes out
//! like any other.
//!
//! Receiving is where a real host would send an **IGMP membership report** so
//! upstream switches/routers forward the group. Two reasons that is not built here:
//! `224.0.0.251` is in the **link-local multicast** block `224.0.0.0/24`, which is
//! never forwarded off the link and for which IGMP snooping is not required; and
//! our virtio-net driver negotiates neither `VIRTIO_NET_F_CTRL_RX` nor a MAC filter
//! table, so it has no receive filter to program - every frame the backend hands us
//! is delivered. **Full IGMP/MLD (and general multicast group management) is a later
//! phase** and is listed as such in docs/NETSTACK.md. Nothing here pretends
//! otherwise.
//!
//! ## Postures
//! Everything above is codec + synchronous state machine, so it is **always
//! compiled**. Only the async drivers ([`claim`], [`mdns::query`]) are behind the
//! `hosted` feature.
//!
//! ## Deferred (documented)
//! **DNS-SD** (RFC 6763: `PTR`/`SRV`/`TXT` service enumeration and
//! `_services._dns-sd._udp.local`) is **not** built - it needs three more record
//! types in the codec and a service registry, which is a phase of its own rather
//! than a cheap add-on. Also deferred: mDNS **known-answer suppression** and
//! duplicate-question suppression (RFC 6762 §7), **probing/conflict resolution for
//! mDNS names** (§8 - the analogue of the ARP probe, for names), the
//! one-second-per-probe/two-second-per-announce **timing schedule** (the state
//! machine counts probes; the delays are the driver's to schedule), IPv6
//! link-local + MLD, and `RATE_LIMIT_INTERVAL` for a host that keeps conflicting.

use crate::arp::{self, ArpPacket};
use crate::eth::Mac;
use crate::ip::Ipv4Addr;

/// The IPv4 link-local prefix, `169.254/16` (RFC 3927 §2.1).
pub const LINK_LOCAL_PREFIX: [u8; 2] = [169, 254];
/// The lowest third octet a candidate may use: `169.254.0/24` is reserved.
pub const MIN_THIRD_OCTET: u8 = 1;
/// The highest third octet a candidate may use: `169.254.255/24` is reserved.
pub const MAX_THIRD_OCTET: u8 = 254;
/// ARP probes sent before a candidate is considered free (RFC 3927 §2.2.1).
pub const PROBE_COUNT: u32 = 3;
/// ARP announcements sent after a candidate is claimed (RFC 3927 §2.4).
pub const ANNOUNCE_COUNT: u32 = 2;
/// The window inside which a *second* conflict means "stop defending, yield the
/// address" (RFC 3927 §2.5 uses ten seconds).
pub const DEFEND_INTERVAL_NS: u64 = 10 * 1_000_000_000;

/// True if `ip` is in `169.254/16`.
pub fn is_link_local(ip: Ipv4Addr) -> bool {
    ip.0[0] == LINK_LOCAL_PREFIX[0] && ip.0[1] == LINK_LOCAL_PREFIX[1]
}

/// True if `ip` is a *usable* link-local address: in `169.254/16` and outside the
/// reserved first and last /24.
pub fn is_usable_link_local(ip: Ipv4Addr) -> bool {
    is_link_local(ip) && ip.0[2] >= MIN_THIRD_OCTET && ip.0[2] <= MAX_THIRD_OCTET
}

/// Where a [`LinkLocal`] claim has got to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClaimState {
    /// A candidate is chosen; probes still to send.
    Probing,
    /// Probing finished with no conflict; announcements still to send.
    Announcing,
    /// Announced - the address is in use by us.
    Claimed,
}

/// What observing an ARP packet meant for the claim.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Observation {
    /// Nothing to do with our candidate (or it was our own probe coming back).
    Unrelated,
    /// Somebody else has or wants our candidate; a new candidate has been picked
    /// and the state machine is back to [`ClaimState::Probing`].
    Conflict,
    /// A conflict *after* we claimed the address, and we are within our rights to
    /// defend it once: send [`LinkLocal::announce`] again.
    Defend,
}

/// The IPv4 link-local claim state machine (RFC 3927). See the module docs for the
/// protocol; drive it with [`probe`](Self::probe) / [`observe`](Self::observe) /
/// [`announce`](Self::announce), and read [`address`](Self::address) when
/// [`state`](Self::state) reaches [`ClaimState::Claimed`].
pub struct LinkLocal {
    mac: Mac,
    rng: u64,
    candidate: Ipv4Addr,
    state: ClaimState,
    probes_sent: u32,
    announces_sent: u32,
    conflicts: u32,
    /// When we last defended the claimed address (0 = never).
    last_defence_ns: u64,
}

impl LinkLocal {
    /// A claim state machine for `mac`, seeded with `seed`.
    ///
    /// The MAC is **mixed into the seed** so a host with a stable MAC tends to pick
    /// the same candidate across reboots (RFC 3927 §2.1's suggestion - it makes an
    /// address stable in practice without any storage). Pass a fixed `seed` for a
    /// reproducible test.
    pub fn new(mac: Mac, seed: u64) -> LinkLocal {
        // Fold the MAC into the seed: 6 bytes into the low 48 bits.
        let mut m = 0u64;
        for &b in &mac.0 {
            m = (m << 8) | b as u64;
        }
        let mut rng = seed ^ m.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let candidate = pick_candidate(&mut rng);
        LinkLocal {
            mac,
            rng,
            candidate,
            state: ClaimState::Probing,
            probes_sent: 0,
            announces_sent: 0,
            conflicts: 0,
            last_defence_ns: 0,
        }
    }

    /// The candidate (once [`ClaimState::Claimed`], the claimed address).
    pub fn address(&self) -> Ipv4Addr {
        self.candidate
    }

    /// The claim state.
    pub fn state(&self) -> ClaimState {
        self.state
    }

    /// How many conflicts have forced a re-pick.
    pub fn conflicts(&self) -> u32 {
        self.conflicts
    }

    /// Probes sent for the current candidate.
    pub fn probes_sent(&self) -> u32 {
        self.probes_sent
    }

    /// Announcements sent for the claimed address.
    pub fn announces_sent(&self) -> u32 {
        self.announces_sent
    }

    /// Build the next **ARP probe** frame: an ARP request for the candidate with a
    /// **`0.0.0.0` sender address**, so it asks without claiming. Returns `None`
    /// once [`PROBE_COUNT`] probes have been sent (the state machine has moved to
    /// [`ClaimState::Announcing`]).
    pub fn probe(&mut self) -> Option<[u8; arp::REQUEST_FRAME_LEN]> {
        if self.state != ClaimState::Probing {
            return None;
        }
        if self.probes_sent >= PROBE_COUNT {
            self.state = ClaimState::Announcing;
            return None;
        }
        self.probes_sent += 1;
        Some(arp::build_request(
            self.mac,
            crate::hostcfg::UNSPECIFIED,
            self.candidate,
        ))
    }

    /// Build the next **ARP announcement**: an ARP request whose sender *and*
    /// target are the claimed address, which is what populates neighbours' caches.
    /// Returns `None` once [`ANNOUNCE_COUNT`] have been sent, at which point the
    /// state is [`ClaimState::Claimed`] - so `while let Some(f) = ll.announce()`
    /// terminates.
    ///
    /// Announcing is **bounded**; defending is not the same act and has its own
    /// method ([`defend`](Self::defend)). Folding the two together is a real hazard:
    /// a driver draining `announce()` in a loop would never see `None`, because an
    /// unbounded defence frame is always available.
    pub fn announce(&mut self) -> Option<[u8; arp::REQUEST_FRAME_LEN]> {
        if self.state != ClaimState::Announcing {
            return None;
        }
        if self.announces_sent >= ANNOUNCE_COUNT {
            self.state = ClaimState::Claimed;
            return None;
        }
        self.announces_sent += 1;
        if self.announces_sent >= ANNOUNCE_COUNT {
            self.state = ClaimState::Claimed;
        }
        Some(arp::build_request(self.mac, self.candidate, self.candidate))
    }

    /// Build the **defence** frame for an address we already hold: the same ARP
    /// announcement, re-sent, without touching [`announces_sent`](Self::announces_sent).
    /// This is the deliberate answer to an [`Observation::Defend`] (RFC 3927 §2.5) -
    /// `None` unless the address is actually [`ClaimState::Claimed`].
    pub fn defend(&mut self) -> Option<[u8; arp::REQUEST_FRAME_LEN]> {
        if self.state != ClaimState::Claimed {
            return None;
        }
        Some(arp::build_request(self.mac, self.candidate, self.candidate))
    }

    /// Observe an ARP packet from the link and decide what it means.
    ///
    /// A conflict is either sender-address == our candidate (someone owns it) or a
    /// probe from a different MAC for our candidate (someone is picking it now). Our
    /// own frames are recognised by their sender MAC and ignored.
    ///
    /// - While probing/announcing, a conflict **re-picks** and returns
    ///   [`Observation::Conflict`].
    /// - Once claimed, the first conflict returns [`Observation::Defend`] (announce
    ///   again); a second within [`DEFEND_INTERVAL_NS`] re-picks and returns
    ///   [`Observation::Conflict`].
    pub fn observe(&mut self, pkt: &ArpPacket, now_ns: u64) -> Observation {
        if pkt.sha == self.mac {
            return Observation::Unrelated; // our own frame
        }
        let owns = pkt.spa == self.candidate;
        let probing_same = pkt.oper == arp::OP_REQUEST
            && pkt.spa == crate::hostcfg::UNSPECIFIED
            && pkt.tpa == self.candidate;
        if !owns && !probing_same {
            return Observation::Unrelated;
        }
        if self.state == ClaimState::Claimed {
            let defended_recently = self.last_defence_ns != 0
                && now_ns.saturating_sub(self.last_defence_ns) < DEFEND_INTERVAL_NS;
            if !defended_recently {
                self.last_defence_ns = now_ns.max(1);
                return Observation::Defend;
            }
        }
        self.repick();
        Observation::Conflict
    }

    /// Abandon the current candidate and start over with a different one. Guaranteed
    /// to change the address (it re-draws until it differs), so a caller can assert
    /// the re-pick happened.
    pub fn repick(&mut self) {
        let old = self.candidate;
        let mut next = pick_candidate(&mut self.rng);
        // splitmix64 has a 2^64 period, so this loop is a formality; bound it anyway.
        for _ in 0..64 {
            if next != old {
                break;
            }
            next = pick_candidate(&mut self.rng);
        }
        self.candidate = next;
        self.conflicts += 1;
        self.state = ClaimState::Probing;
        self.probes_sent = 0;
        self.announces_sent = 0;
        self.last_defence_ns = 0;
    }
}

/// Draw a candidate link-local address: `169.254.x.y` with `x` in
/// `1..=254` (the reserved first and last /24 excluded) and `y` in `0..=255`.
/// **Public randomness** - see the module docs.
pub fn pick_candidate(rng: &mut u64) -> Ipv4Addr {
    let r = crate::dhcp::splitmix64(rng);
    let span = (MAX_THIRD_OCTET - MIN_THIRD_OCTET) as u64 + 1; // 254 values
    let third = MIN_THIRD_OCTET + ((r >> 8) % span) as u8;
    let fourth = (r & 0xFF) as u8;
    Ipv4Addr([LINK_LOCAL_PREFIX[0], LINK_LOCAL_PREFIX[1], third, fourth])
}

/// How many ARP frames [`claim`] will look at inside one probe's listen window
/// before moving on. A **frame** count, not a poll count: a link that keeps handing
/// us unrelated ARP traffic must not extend the window indefinitely.
#[cfg(feature = "hosted")]
pub const PROBE_FRAME_BUDGET: u32 = 32;

/// Drive the link-local claim over the NIC: probe, listen for a conflict, re-pick as
/// needed, then announce. Returns the claimed address.
///
/// `probe_window_ns` is the **duration** to listen after each probe - RFC 3927 §2.2.1
/// waits one to two seconds; a caller that only needs to show the frames went out can
/// pass much less. It is a real deadline, not a drain count: the wait
/// ([`crate::wire::recv_frame_timeout`]) parks in the kernel and the kernel spends it
/// halted wherever an interrupt can wake it, so the same value means the same span of
/// time on every ISA. (A drain *count* could not: one ISA's drain is an interrupt
/// park, another's is a poll, so the same number bought wildly different amounts of
/// listening - and unbounded amounts of CPU.)
///
/// The window restarts on each frame received, bounded by [`PROBE_FRAME_BUDGET`]
/// frames, so a chatty link cannot stretch the claim without limit.
///
/// This is the thin `hosted` driver over [`LinkLocal`] - it owns only the frame
/// transport and the timing. Absence of a conflict is *absence of evidence*: after the
/// probes go out we listen and, seeing no conflicting ARP, proceed - which is exactly
/// what RFC 3927 prescribes and is not a proof that the address is free. The conflict
/// *detection* is proven deterministically in the test with crafted ARP packets.
#[cfg(feature = "hosted")]
pub async fn claim(
    ll: &mut LinkLocal,
    probe_window_ns: u64,
    now_ns: u64,
) -> Result<Ipv4Addr, crate::wire::WireError> {
    use crate::eth;
    use crate::wire;

    // Bound the whole claim: each conflict costs one round of probes.
    for _ in 0..(1 + PROBE_COUNT) {
        while let Some(frame) = ll.probe() {
            wire::send_frame(&frame).await?;
            // Listen for `probe_window_ns` for a conflicting ARP.
            let mut buf = [0u8; wire::MAX_FRAME];
            for _ in 0..PROBE_FRAME_BUDGET {
                let n = wire::recv_frame_timeout(&mut buf, probe_window_ns).await?;
                if n == 0 {
                    break; // the window elapsed in silence
                }
                let Some(f) = eth::Frame::parse(&buf[..n]) else {
                    continue;
                };
                if f.ethertype() != eth::ethertype::ARP {
                    continue;
                }
                let Some(pkt) = ArpPacket::parse(f.payload()) else {
                    continue;
                };
                if ll.observe(&pkt, now_ns) == Observation::Conflict {
                    break;
                }
            }
            if ll.state() == ClaimState::Probing && ll.probes_sent() == 0 {
                break; // a conflict reset the round; start the new candidate
            }
        }
        // Bounded: `announce` returns None once ANNOUNCE_COUNT have gone out (a
        // *defence* is `LinkLocal::defend`, deliberately not this loop).
        while let Some(frame) = ll.announce() {
            wire::send_frame(&frame).await?;
        }
        if ll.state() == ClaimState::Claimed {
            return Ok(ll.address());
        }
    }
    Err(crate::wire::WireError::ArpTimeout)
}

/// **mDNS** (RFC 6762): `.local` name resolution on the link with no server, over
/// the [`crate::dns`] codec. See the parent module's docs for the table of
/// differences from unicast DNS, and for why no IGMP is built here.
pub mod mdns {
    use alloc::vec::Vec;

    use crate::dns::{self, DnsError, QType, Question, Record};
    use crate::eth::Mac;
    use crate::ip::Ipv4Addr;

    /// The IPv4 mDNS group address, `224.0.0.251` (RFC 6762 §3).
    pub const GROUP: Ipv4Addr = Ipv4Addr([224, 0, 0, 251]);
    /// The mDNS UDP port, 5353.
    pub const PORT: u16 = 5353;
    /// The suffix every mDNS name ends in.
    pub const LOCAL_SUFFIX: &str = "local";
    /// The mDNS **QU** bit in a question's class: "answer me by unicast"
    /// (RFC 6762 §5.4).
    pub const QU_BIT: u16 = 0x8000;
    /// The mDNS **cache-flush** bit in a record's class (RFC 6762 §10.2).
    pub const CACHE_FLUSH_BIT: u16 = 0x8000;
    /// The TTL RFC 6762 §10 recommends for a record containing the host's own name
    /// or address - short, because a host can move.
    pub const TTL_HOSTNAME: u32 = 120;
    /// The TTL RFC 6762 §10 recommends for other (service-ish) records.
    pub const TTL_OTHER: u32 = 4500;
    /// The response flags: a response (QR set) that is authoritative (AA set). mDNS
    /// responders are authoritative for their own names by definition.
    pub const RESPONSE_FLAGS: u16 = 0x8400;
    /// The largest message the builders emit.
    pub const MAX_MESSAGE: usize = 512;

    /// The Ethernet multicast MAC an IPv4 multicast address maps onto:
    /// `01:00:5e` then the **low 23 bits** of the address (RFC 1112 §6.4). Note the
    /// top bit of the fourth octet is dropped - that is why `224.0.0.251` and
    /// `225.0.0.251` share a MAC, and why a receiver must still check the IP.
    pub fn multicast_mac(ip: Ipv4Addr) -> Mac {
        Mac([0x01, 0x00, 0x5e, ip.0[1] & 0x7f, ip.0[2], ip.0[3]])
    }

    /// True if `name` is an mDNS name - its last label is `local` (case-insensitive,
    /// with or without a trailing dot).
    pub fn is_local(name: &str) -> bool {
        let n = dns::normalize(name);
        n == LOCAL_SUFFIX
            || (n.len() > LOCAL_SUFFIX.len() + 1
                && n.ends_with(LOCAL_SUFFIX)
                && n.as_bytes()[n.len() - LOCAL_SUFFIX.len() - 1] == b'.')
    }

    /// Build an mDNS **query** for `name`/`qtype`.
    ///
    /// Differs from a unicast DNS query in exactly three places, and each one
    /// matters: the id is **0** (mDNS does not correlate replies by id, so a random
    /// id would just be noise), the flags are **0** (recursion-desired is
    /// meaningless with no server), and the class carries the **QU** bit when
    /// `unicast_response` is set, asking the responder to reply straight to us
    /// instead of to the whole group.
    pub fn build_query(name: &str, qtype: QType, unicast_response: bool) -> Option<Vec<u8>> {
        let mut buf = [0u8; MAX_MESSAGE];
        let qclass = if unicast_response {
            dns::CLASS_IN | QU_BIT
        } else {
            dns::CLASS_IN
        };
        let len = dns::build_question_message(0, 0, name, qtype.as_u16(), qclass, &mut buf)?;
        Some(buf[..len].to_vec())
    }

    /// Build an mDNS **response** carrying one A record for `name` -> `ip`.
    ///
    /// `cache_flush` sets the top bit of the record class, telling receivers to
    /// **replace** any cached records for this name/type rather than add to them -
    /// which is what a host announcing its own address wants. A `ttl` of 0 makes
    /// this a **goodbye** (RFC 6762 §10.1). The question section is empty
    /// (`qdcount = 0`), which RFC 6762 §6 permits and which is what an unsolicited
    /// announcement looks like.
    pub fn build_response(
        name: &str,
        ip: Ipv4Addr,
        ttl: u32,
        cache_flush: bool,
    ) -> Option<Vec<u8>> {
        let mut buf = [0u8; MAX_MESSAGE];
        buf[0..2].copy_from_slice(&0u16.to_be_bytes()); // id 0
        buf[2..4].copy_from_slice(&RESPONSE_FLAGS.to_be_bytes());
        buf[4..6].copy_from_slice(&0u16.to_be_bytes()); // qdcount 0
        buf[6..8].copy_from_slice(&1u16.to_be_bytes()); // ancount 1
        buf[8..12].copy_from_slice(&[0, 0, 0, 0]); // ns/ar 0
        let mut pos = dns::write_name(name, &mut buf, 12)?;
        let class = if cache_flush {
            dns::CLASS_IN | CACHE_FLUSH_BIT
        } else {
            dns::CLASS_IN
        };
        if pos + 10 + 4 > buf.len() {
            return None;
        }
        buf[pos..pos + 2].copy_from_slice(&dns::TYPE_A.to_be_bytes());
        pos += 2;
        buf[pos..pos + 2].copy_from_slice(&class.to_be_bytes());
        pos += 2;
        buf[pos..pos + 4].copy_from_slice(&ttl.to_be_bytes());
        pos += 4;
        buf[pos..pos + 2].copy_from_slice(&4u16.to_be_bytes()); // rdlength
        pos += 2;
        buf[pos..pos + 4].copy_from_slice(&ip.0);
        pos += 4;
        Some(buf[..pos].to_vec())
    }

    /// Build the **goodbye** for `name` -> `ip`: the same response with TTL 0, which
    /// tells every cache on the link to drop the record now rather than wait it out.
    pub fn build_goodbye(name: &str, ip: Ipv4Addr) -> Option<Vec<u8>> {
        build_response(name, ip, 0, true)
    }

    /// Parse an mDNS query's questions (the [`crate::dns`] codec, unchanged). Each
    /// [`Question`] carries the QU bit in its class - read it with
    /// [`Question::unicast_response`].
    pub fn parse_query(msg: &[u8]) -> Result<Vec<Question>, DnsError> {
        dns::parse_questions(msg)
    }

    /// Parse an mDNS response's answer records (the [`crate::dns`] codec,
    /// unchanged). Each [`Record`] carries the cache-flush bit in its class - read it
    /// with [`Record::cache_flush`] - and a TTL of 0 is a goodbye
    /// ([`Record::is_goodbye`]).
    pub fn parse_response(msg: &[u8]) -> Result<Vec<Record>, DnsError> {
        Ok(dns::parse_response(msg)?.answers)
    }

    /// Should this responder answer `question`, given that it owns `our_name`?
    ///
    /// Three conditions, all required: the name matches ours (case-insensitively,
    /// trailing dot ignored), the name is a **`.local`** name (a responder must never
    /// answer a non-local name over mDNS - that is somebody else's namespace), and
    /// the query type is one we hold an A record for.
    pub fn should_respond(question: &Question, our_name: &str) -> bool {
        if !is_local(our_name) || !is_local(&question.name) {
            return false;
        }
        if dns::normalize(&question.name) != dns::normalize(our_name) {
            return false;
        }
        question.class() == dns::CLASS_IN
            && (question.qtype == dns::TYPE_A || question.qtype == 255/* ANY */)
    }

    /// How many datagrams [`query`] will look at inside its window before giving up.
    /// A **frame** count (the analogue of `ntp::RECV_ATTEMPTS`), so unrelated link
    /// traffic cannot stretch the wait; the wait itself is the caller's duration.
    #[cfg(feature = "hosted")]
    pub const RECV_FRAME_BUDGET: u32 = 32;

    /// Send one mDNS query to the group and collect the answers that come back,
    /// waiting up to `timeout_ns` for each frame.
    ///
    /// The `hosted` driver. It multicasts the query (built above) to
    /// `224.0.0.251:5353` at [`multicast_mac`]`(GROUP)` - no ARP, a multicast MAC is
    /// computed, not resolved - then waits for responses on port 5353 and returns
    /// the A records whose name matches. The wait is a **duration**
    /// ([`crate::wire::recv_frame_timeout`], which parks in the kernel), not a poll
    /// count, so it means the same thing on every ISA.
    ///
    /// Under QEMU's SLIRP there is **no mDNS peer on the emulated link**, so this
    /// returns an empty vector; the proof reports that as a skip rather than
    /// pretending. On a real link it is a working resolver.
    #[cfg(feature = "hosted")]
    pub async fn query(
        src_mac: Mac,
        src_ip: Ipv4Addr,
        name: &str,
        timeout_ns: u64,
    ) -> Result<Vec<Record>, crate::wire::WireError> {
        use crate::wire::{self, Ipv4Framing, WireError};

        let payload = build_query(name, QType::A, false).ok_or(WireError::TooBig)?;
        let mut datagram = [0u8; wire::MAX_FRAME - wire::L4_OFFSET];
        let dlen = crate::udp::build_v4(src_ip, GROUP, PORT, PORT, &payload, &mut datagram)
            .ok_or(WireError::TooBig)?;
        let framing = Ipv4Framing {
            dst_mac: multicast_mac(GROUP),
            src_mac,
            // RFC 6762 §11: mDNS packets carry TTL 255 so a receiver can tell a
            // genuinely on-link packet from a forged off-link one.
            ttl: 255,
            protocol: crate::ip::proto::UDP,
            src_ip,
            dst_ip: GROUP,
        };
        let mut frame = [0u8; wire::MAX_FRAME];
        let flen = wire::frame_ipv4(&framing, &datagram[..dlen], &mut frame)?;
        wire::send_frame(&frame[..flen]).await?;

        let want = dns::normalize(name);
        let mut out = Vec::new();
        let mut buf = [0u8; wire::MAX_FRAME];
        for _ in 0..RECV_FRAME_BUDGET {
            let n = wire::recv_frame_timeout(&mut buf, timeout_ns).await?;
            if n == 0 {
                break; // the window elapsed with nothing on the link
            }
            let Some(parsed) = wire::parse_ipv4(&buf[..n]) else {
                continue;
            };
            if parsed.header.protocol != crate::ip::proto::UDP {
                continue;
            }
            let (start, end) = parsed.l4;
            let datagram = &buf[start..end];
            let Some(hdr) = crate::udp::UdpHeader::parse(datagram) else {
                continue;
            };
            if hdr.src_port != PORT {
                continue;
            }
            let Some(msg) = hdr.payload(datagram) else {
                continue;
            };
            if let Ok(records) = parse_response(msg) {
                for r in records {
                    if r.rtype == dns::TYPE_A && dns::normalize(&r.name) == want {
                        out.push(r);
                    }
                }
                if !out.is_empty() {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    /// A tiny mDNS responder: the names this host answers for, and its address.
    /// Feed it a received query with [`Responder::respond`]; it hands back the
    /// response bytes to send, or `None` if the query is not ours.
    pub struct Responder {
        name: alloc::string::String,
        address: Ipv4Addr,
        ttl: u32,
        answered: u32,
    }

    impl Responder {
        /// A responder for `name` (which must be a `.local` name) at `address`.
        pub fn new(name: &str, address: Ipv4Addr) -> Responder {
            Responder {
                name: dns::normalize(name),
                address,
                ttl: TTL_HOSTNAME,
                answered: 0,
            }
        }

        /// The name this responder owns.
        pub fn name(&self) -> &str {
            &self.name
        }

        /// How many queries have been answered.
        pub fn answered(&self) -> u32 {
            self.answered
        }

        /// Answer a received mDNS query message, if any of its questions is for our
        /// name. Returns the response bytes plus whether the asker set **QU** (so
        /// the caller knows to reply by unicast rather than to the group).
        pub fn respond(&mut self, query: &[u8]) -> Option<(Vec<u8>, bool)> {
            let questions = parse_query(query).ok()?;
            for q in &questions {
                if should_respond(q, &self.name) {
                    let msg = build_response(&self.name, self.address, self.ttl, true)?;
                    self.answered += 1;
                    return Some((msg, q.unicast_response()));
                }
            }
            None
        }

        /// The unsolicited announcement this host sends when it comes up (or after
        /// its address changes): the same authoritative, cache-flushing response,
        /// with nobody having asked.
        pub fn announcement(&self) -> Option<Vec<u8>> {
            build_response(&self.name, self.address, self.ttl, true)
        }

        /// The goodbye this host sends when it goes away.
        pub fn goodbye(&self) -> Option<Vec<u8>> {
            build_goodbye(&self.name, self.address)
        }
    }
}
