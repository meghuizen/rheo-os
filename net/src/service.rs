//! The **network service cell** and its concurrent client fan-out
//! (docs/NETSTACK.md the service-cell section, rheo-net Phase N4a).
//!
//! Doctrine says the network stack is userspace (docs/ARCHITECTURE.md 4.7,
//! docs/NETWORKING.md 5-9): a long-lived **service cell** owns it and other cells
//! reach the network by talking to that cell. Everything above this phase rides
//! here - app-protocol servers (N5), the remote-INET bridge for Linux binaries
//! (N4b), onion routing - so the load-bearing question is not "can one cell talk to
//! one cell" (Phase E answered that) but "**can one cell serve many, concurrently**".
//!
//! ## Shape
//!
//! - A [`Service`] holds **one cross-cell channel end per client** - N separate
//!   SPSC ring regions, each shared with exactly one client cell. It runs **one
//!   strand per client**, each parked on its own [`AsyncReceiver`]; the reactor
//!   scans the slots in order, so the strands round-robin and N requests are in
//!   flight at once.
//! - A [`Client`] is the thin end: it holds the channel it **inherited at spawn**
//!   (its own slot 0), sends a request and awaits the response.
//! - The protocol is **one word each way** ([`Request`] packed into the channel
//!   message's `tag`, the argument/result in its `val`) - the async channel's
//!   symmetric payload. Big requests (a DNS name, an HTTP header block) belong in a
//!   shared sealed grant, which the Phase E `ipc::share` path already carries; N4a
//!   keeps the protocol word-sized on purpose so the fan-out is what is proven, and
//!   names the request-in-a-grant form as the follow-on.
//!
//! ## Honest scope
//!
//! **Concurrent, not parallel.** One CPU, cooperative scheduling: the service's
//! strands interleave and the cells hand the CPU on at syscall boundaries
//! (`librheo::sys::yield_cell`). Real parallelism - a service strand running while a
//! client computes - needs SMP (task #27). Every claim here is about *concurrency*:
//! N requests in flight, N strands making progress, no client blocking another.
//!
//! **Fan-out composes over spawn.** The service is the *parent*: it spawns each
//! client with [`librheo::proc::spawn_on_channel`], handing that client its own
//! channel slot. A name-based rendezvous (an *unrelated* cell connecting to a
//! running service) is a genuinely new capability and is a documented follow-on
//! (docs/NETSTACK.md).

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use librheo::ipc::{AsyncReceiver, AsyncSender, Channel, Message};
use librheo::rt;

use crate::arp::ArpCache;
use crate::dns::{Cache, HostsTable, QType};
use crate::eth::Mac;
use crate::ip::{IpAddr, Ipv4Addr};

// ------------------------------------------------------------------ protocol

/// Request: echo `val` back transformed per-client ([`echo_transform`]) - real,
/// client-specific work with a reply the client can predict exactly.
pub const OP_ECHO: u8 = 1;
/// Request: resolve the catalogue name id in `val` to an IPv4 address, answered
/// from the **network-free** tiers of [`crate::dns`] (hosts table + TTL cache).
pub const OP_RESOLVE: u8 = 2;
/// Request: no more work from this client. The reply carries how many requests the
/// service served it; the serving strand then finishes.
pub const OP_BYE: u8 = 3;

/// The reply value for a request the service could not answer (an unknown opcode,
/// an unknown name id, or a live network op that did not complete). A sentinel, not
/// a rich error - N4a's protocol is one word wide.
pub const REPLY_NONE: u32 = 0;

/// A request/response header, packed into the channel message's `tag` so the
/// 32-bit `val` carries the argument (a name id) or the result (an IPv4 address).
/// The reply echoes the request's tag, so a client matches responses to requests.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Request {
    /// `OP_ECHO` / `OP_RESOLVE` / `OP_BYE`.
    pub op: u8,
    /// Which client sent it (its channel slot in the service).
    pub client: u8,
    /// Per-client sequence number, so a reply is unambiguous.
    pub seq: u64,
}

impl Request {
    /// Pack into a channel message tag.
    pub fn tag(&self) -> u64 {
        ((self.op as u64) << 56) | ((self.client as u64) << 48) | (self.seq & 0xffff_ffff_ffff)
    }

    /// Unpack from a channel message tag.
    pub fn from_tag(tag: u64) -> Request {
        Request {
            op: (tag >> 56) as u8,
            client: ((tag >> 48) & 0xff) as u8,
            seq: tag & 0xffff_ffff_ffff,
        }
    }
}

/// The per-client `OP_ECHO` transform: both ends compute it, so a client asserts
/// the service did *its* work and not another client's. Distinct per `client` by
/// construction (a client-keyed rotate + mix).
pub fn echo_transform(val: u32, client: u8) -> u32 {
    let k = client as u32 + 1;
    val.rotate_left(k) ^ 0x9e37_79b9u32.wrapping_mul(k)
}

/// A catalogue name id -> name mapping. The word-wide protocol carries an id, not a
/// string; a real service passes the name in a shared grant (the documented
/// follow-on, docs/NETSTACK.md).
pub const NAME_ALPHA: u32 = 1;
pub const NAME_BETA: u32 = 2;
pub const NAME_GAMMA: u32 = 3;
/// The name id whose resolution the service answers with a **real network op** (a
/// live ARP for the link's gateway). Degrades to [`REPLY_NONE`] where no NIC
/// answers, so the deterministic core never depends on it.
pub const NAME_GATEWAY: u32 = 9;

/// The catalogue: `(id, name)`. Ids, not strings, travel on the wire.
pub const CATALOGUE: [(u32, &str); 4] = [
    (NAME_ALPHA, "alpha.rheo.test"),
    (NAME_BETA, "beta.rheo.test"),
    (NAME_GAMMA, "gamma.rheo.test"),
    (NAME_GATEWAY, "gateway.rheo.test"),
];

/// The name for a catalogue id, if it is in [`CATALOGUE`].
pub fn catalogue_name(id: u32) -> Option<&'static str> {
    CATALOGUE.iter().find(|(i, _)| *i == id).map(|(_, n)| *n)
}

// -------------------------------------------------------------------- service

/// What a finished [`Service::serve`] observed - the evidence a test asserts.
#[derive(Clone, Debug)]
pub struct ServiceReport {
    /// Requests served per client, indexed by channel slot.
    pub served: Vec<u32>,
    /// The order the per-client strands processed requests in (client ids). Round-
    /// robin (`0,1,2,0,1,2,...`) is the interleave witness: strand k reaches round
    /// r only after strands `0..k` did, so no strand monopolised the vcore.
    pub order: Vec<u8>,
    /// High-water mark of client requests in flight at the same instant, measured
    /// by the reactor. Reaching N proves all N clients' requests were queued
    /// together before the first reply went out.
    pub max_in_flight: usize,
    /// Reactor park+wake deliveries per client channel - one per message received
    /// on that end, so a spin would leave this at 0.
    pub wakeups: Vec<u64>,
    /// Live network ops the service performed on a client's behalf (see
    /// [`NAME_GATEWAY`]). 0 = the bonus live path was never exercised or the link
    /// did not answer.
    pub live_ops: u32,
    /// The result the live network op produced (an IPv4 as `u32`), or
    /// [`REPLY_NONE`].
    pub live_result: u32,
}

/// The mutable state the per-client strands share. Single-vcore cooperative cell,
/// so a `RefCell` is enough - and no borrow is ever held across an `.await` (the
/// tiers a strand consults are synchronous; the one awaiting path, the live network
/// op, touches no shared borrow while suspended).
struct State {
    hosts: HostsTable,
    cache: Cache,
    served: Vec<u32>,
    order: Vec<u8>,
    live_ops: u32,
    live_result: u32,
}

impl State {
    /// Resolve a catalogue id from the network-free tiers: the hosts table first,
    /// then the TTL cache (exactly [`crate::dns::Resolver`]'s order, minus the
    /// network step a service strand must not take while others wait).
    fn resolve_local(&mut self, id: u32) -> u32 {
        let Some(name) = catalogue_name(id) else {
            return REPLY_NONE;
        };
        if let Some(ips) = self.hosts.lookup(name, QType::A)
            && let Some(IpAddr::V4(v4)) = ips.first()
        {
            return u32::from_be_bytes(v4.octets());
        }
        // `Cache` is tick-denominated; a seeded entry with a large TTL is live for
        // the whole run, so `now` is only ever compared against that.
        let now = librheo::time::now().ticks();
        if let Some(ips) = self.cache.get(name, QType::A.as_u16(), now)
            && let Some(IpAddr::V4(v4)) = ips.first()
        {
            return u32::from_be_bytes(v4.octets());
        }
        REPLY_NONE
    }
}

/// A network service cell serving many client cells concurrently
/// (docs/NETSTACK.md the service-cell section, rheo-net N4a). Build it with
/// [`Service::bind`], spawn the clients with [`Service::spawn_client`], then run
/// [`Service::serve`].
pub struct Service {
    state: Rc<RefCell<State>>,
    /// One `(sender, receiver)` pair per client channel slot.
    ends: Vec<(AsyncSender, AsyncReceiver)>,
    /// The identity the live network op uses.
    src_mac: Mac,
    src_ip: Ipv4Addr,
    /// The gateway the live network op resolves.
    gateway: Ipv4Addr,
}

impl Service {
    /// Bind every cross-cell channel end this cell holds (slots `0..count`) to the
    /// reactor - one per client. Returns `None` if the cell holds no channel (it was
    /// not wired as a service). `hosts`/`cached` seed the two network-free
    /// resolution tiers.
    pub fn bind(hosts: &[(&str, Ipv4Addr)], cached: &[(&str, Ipv4Addr)]) -> Option<Service> {
        let mut ends = Vec::new();
        for slot in 0..rt::MAX_CHANNELS {
            let Some(ch) = Channel::open_slot(slot) else {
                break;
            };
            ends.push(ch.split());
        }
        if ends.is_empty() {
            return None;
        }
        let mut hosts_table = HostsTable::new();
        for (name, ip) in hosts {
            hosts_table.insert(name, IpAddr::V4(*ip));
        }
        let mut cache = Cache::new(64);
        // A TTL far beyond the run: the cache tier is a deterministic hit, never a
        // timing race (the live path is the only thing that touches the network).
        let expires = librheo::time::now().ticks().saturating_add(u64::MAX / 4);
        for (name, ip) in cached {
            cache.insert(
                name,
                QType::A.as_u16(),
                alloc::vec![IpAddr::V4(*ip)],
                expires,
            );
        }
        let n = ends.len();
        // The identity comes from the **host-config store** (rheo-net N4c), not from
        // three addresses written inline here: `HostConfig::slirp()` is the one place
        // QEMU's guest/gateway/DNS addresses are named. `set_identity` overrides it
        // once the real NIC MAC is known, and a DHCP-configured service would pass
        // its own `HostConfig` instead.
        let host = crate::hostcfg::HostConfig::slirp();
        Some(Service {
            state: Rc::new(RefCell::new(State {
                hosts: hosts_table,
                cache,
                served: alloc::vec![0; n],
                order: Vec::new(),
                live_ops: 0,
                live_result: REPLY_NONE,
            })),
            ends,
            src_mac: Mac([0u8; 6]),
            src_ip: host.source_address(),
            gateway: host.gateway().unwrap_or(crate::hostcfg::UNSPECIFIED),
        })
    }

    /// How many clients this service can serve (its channel-end count).
    pub fn clients(&self) -> usize {
        self.ends.len()
    }

    /// Set the local identity + gateway the **bonus live** network op uses
    /// ([`NAME_GATEWAY`]). Without a real MAC the live op is skipped honestly.
    pub fn set_identity(&mut self, src_mac: Mac, src_ip: Ipv4Addr, gateway: Ipv4Addr) {
        self.src_mac = src_mac;
        self.src_ip = src_ip;
        self.gateway = gateway;
    }

    /// Spawn client `slot`'s cell from `path`, handing it **this service's channel
    /// slot `slot`** as its own slot 0 (docs/NETSTACK.md rheo-net N4a). `argv` is
    /// passed through; the client learns its id from it. Returns the child handle.
    pub fn spawn_client(
        &self,
        slot: usize,
        path: &str,
        argv: &[&str],
    ) -> Result<librheo::proc::Child, librheo::proc::SpawnError> {
        librheo::proc::spawn_on_channel(path, argv, &[], slot)
    }

    /// Serve every client concurrently until each has said [`OP_BYE`]: spawn **one
    /// strand per client**, each parked on its own channel receiver, and join them.
    /// While one strand awaits its client, the others run - the reactor scans the
    /// slots in order, so they round-robin (docs/NETSTACK.md rheo-net N4a).
    /// Concurrent, not parallel (one CPU, cooperative - SMP is task #27).
    pub async fn serve(self) -> ServiceReport {
        let Service {
            state,
            ends,
            src_mac,
            src_ip,
            gateway,
        } = self;
        let n = ends.len();
        let mut joins = Vec::with_capacity(n);
        for (slot, (tx, rx)) in ends.into_iter().enumerate() {
            let st = state.clone();
            joins.push(rt::spawn(async move {
                serve_client(slot, tx, rx, st, src_mac, src_ip, gateway).await
            }));
        }
        for j in joins {
            j.join().await;
        }
        let st = state.borrow();
        ServiceReport {
            served: st.served.clone(),
            order: st.order.clone(),
            max_in_flight: rt::chan_max_pending(),
            wakeups: (0..n).map(rt::chan_wakeups_on).collect(),
            live_ops: st.live_ops,
            live_result: st.live_result,
        }
    }
}

/// One client's serving strand: park on that client's channel, answer each request
/// on it, and finish on [`OP_BYE`]. Every shared-state borrow is synchronous - the
/// only `.await`s are the channel recv/send and the optional live network op.
async fn serve_client(
    slot: usize,
    tx: AsyncSender,
    rx: AsyncReceiver,
    state: Rc<RefCell<State>>,
    src_mac: Mac,
    src_ip: Ipv4Addr,
    gateway: Ipv4Addr,
) {
    // Per-strand (not shared) ARP cache: the live network op awaits, and holding a
    // shared borrow across an await would be a bug waiting to happen.
    let mut arp = ArpCache::new();
    loop {
        let msg = rx.recv().await;
        let req = Request::from_tag(msg.tag);
        {
            let mut st = state.borrow_mut();
            st.order.push(slot as u8);
            if let Some(c) = st.served.get_mut(slot) {
                *c += 1;
            }
        }
        let reply = match req.op {
            OP_ECHO => echo_transform(msg.val, slot as u8),
            OP_RESOLVE if msg.val == NAME_GATEWAY => {
                // The bonus live op: one real network round trip on this client's
                // behalf. An ARP for the gateway proves the service reaches the
                // wire; `recv` genuinely parks the strand (rheo-net N2d), so its
                // siblings keep running. Degrades to REPLY_NONE with no NIC.
                let result = if src_mac == Mac([0u8; 6]) {
                    REPLY_NONE
                } else {
                    match crate::arp::resolve(&mut arp, src_mac, src_ip, gateway).await {
                        Ok(_) => u32::from_be_bytes(gateway.octets()),
                        Err(_) => REPLY_NONE,
                    }
                };
                let mut st = state.borrow_mut();
                st.live_ops += 1;
                st.live_result = result;
                result
            }
            OP_RESOLVE => state.borrow_mut().resolve_local(msg.val),
            OP_BYE => state.borrow().served.get(slot).copied().unwrap_or(0),
            _ => REPLY_NONE,
        };
        tx.send(Message {
            tag: msg.tag,
            val: reply,
        })
        .await;
        if req.op == OP_BYE {
            return;
        }
    }
}

// --------------------------------------------------------------------- client

/// The client end of a [`Service`] (docs/NETSTACK.md rheo-net N4a): the channel
/// this cell **inherited at spawn** (its slot 0), bound to the reactor. Send a
/// request, await its response - while parked, this cell's other strands run.
pub struct Client {
    tx: AsyncSender,
    rx: AsyncReceiver,
    id: u8,
    seq: u64,
}

impl Client {
    /// Bind this cell's inherited channel as a service client with identity `id`
    /// (which the service also knows as the channel slot it spawned this cell on).
    /// `None` if no channel was inherited.
    pub fn open(id: u8) -> Option<Client> {
        let ch = Channel::open()?;
        let (tx, rx) = ch.split();
        Some(Client { tx, rx, id, seq: 0 })
    }

    /// This client's identity.
    pub fn id(&self) -> u8 {
        self.id
    }

    /// Send one request and await its response, checking the reply's tag matches.
    /// `None` if the service replied to a different request (a protocol error).
    pub async fn call(&mut self, op: u8, val: u32) -> Option<u32> {
        self.seq += 1;
        let req = Request {
            op,
            client: self.id,
            seq: self.seq,
        };
        let tag = req.tag();
        self.tx.send(Message { tag, val }).await;
        let reply = self.rx.recv().await;
        if reply.tag == tag {
            Some(reply.val)
        } else {
            None
        }
    }

    /// Ask the service to echo `val` transformed for this client. The client can
    /// predict the answer exactly ([`echo_transform`]) - real per-client work.
    pub async fn echo(&mut self, val: u32) -> Option<u32> {
        self.call(OP_ECHO, val).await
    }

    /// Ask the service to resolve catalogue name `id` to an IPv4 address (as a
    /// big-endian `u32`); [`REPLY_NONE`] if it cannot.
    pub async fn resolve(&mut self, id: u32) -> Option<u32> {
        self.call(OP_RESOLVE, id).await
    }

    /// Tell the service this client is done; the reply is how many requests it
    /// served this client.
    pub async fn bye(&mut self) -> Option<u32> {
        self.call(OP_BYE, 0).await
    }
}

/// The `String` form of a catalogue name (a convenience for a cell that prints).
pub fn name_of(id: u32) -> String {
    String::from(catalogue_name(id).unwrap_or("?"))
}
