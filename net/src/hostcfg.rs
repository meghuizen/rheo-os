//! `net::hostcfg` - the **host-configuration store** (docs/NETSTACK.md, rheo-net
//! Phase N4c). One place holds the answers to "who am I on this link?": the IPv4
//! address, the netmask, the default gateway, the DNS servers, the search domains,
//! and the hostname. Everything above it reads the store instead of carrying a
//! hardcoded address.
//!
//! ## Why this exists
//! Before N4c the stack's identity was literal: `10.0.2.15` (SLIRP's guest
//! address) and `10.0.2.2` / `10.0.2.3` (its gateway and DNS responder) appeared
//! inline wherever a send needed a source address. That is fine for a proof and
//! wrong for a host: an address comes from **DHCP** ([`crate::dhcp`]), from
//! **static** configuration, or - when nothing answers - from **IPv4 link-local
//! autoconfiguration** ([`crate::zeroconf`]). All three write the same store, and
//! [`ConfigSource`] records which one won.
//!
//! ## What reads it
//! - [`HostConfig::next_hop`] is the **routing decision**: an on-link destination
//!   is reached directly, anything else through the gateway. `net::wire` used to
//!   note this as a deferred refinement ("host config + a routing table"); this is
//!   that host config. It is deliberately *one* route (on-link + default), not a
//!   route table - multi-route / policy routing stays deferred.
//! - [`crate::udp::UdpEndpoint::from_host_config`] takes its source address here.
//! - [`HostConfig::dns_config`] builds a [`crate::dns::Config`] whose resolvers and
//!   search domains come from the store, so the resolver stops being hand-wired.
//! - `net::service` seeds its identity from [`HostConfig::slirp`] rather than three
//!   inline literals.
//!
//! ## Shape
//! A plain struct plus accessors. **No kernel state, no global**: a cell owns its
//! `HostConfig` and passes `&`/`&mut` where it is needed. That keeps it testable
//! (the deterministic proof builds one by hand) and keeps configuration out of the
//! kernel, which is the whole doctrine here - DHCP, zeroconf and NTP are userspace
//! (docs/NETSTACK.md).
//!
//! It is in the crate's always-compiled half: it is a struct and some integer
//! arithmetic, so it needs neither librheo nor the NIC. Only
//! [`HostConfig::dns_config`] (which returns a `hosted`-only type) is gated.

use alloc::string::String;
use alloc::vec::Vec;

use crate::ip::Ipv4Addr;

/// Where a [`HostConfig`]'s address came from. Recorded so a caller can tell a
/// DHCP lease from a link-local fallback - they mean very different things
/// operationally (a link-local address cannot reach off-link at all).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ConfigSource {
    /// Nothing has configured this host yet.
    Unconfigured,
    /// Written by hand (a static address).
    Static,
    /// Won from a DHCP server ([`crate::dhcp`]).
    Dhcp,
    /// Self-assigned IPv4 link-local, 169.254/16 ([`crate::zeroconf`]). No
    /// gateway exists for such an address - it is link-only by definition.
    LinkLocal,
}

/// The IPv4 "unspecified" address, `0.0.0.0` - the source address a DHCP client
/// must use before it owns one (RFC 2131 §4.1).
pub const UNSPECIFIED: Ipv4Addr = Ipv4Addr([0, 0, 0, 0]);
/// The all-ones IPv4 broadcast address, `255.255.255.255`.
pub const BROADCAST: Ipv4Addr = Ipv4Addr([255, 255, 255, 255]);

/// The host's network configuration. See the module docs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostConfig {
    address: Option<Ipv4Addr>,
    netmask: Option<Ipv4Addr>,
    gateway: Option<Ipv4Addr>,
    dns_servers: Vec<Ipv4Addr>,
    search_domains: Vec<String>,
    hostname: Option<String>,
    source: ConfigSource,
    /// The DHCP lease length in seconds, when the address came from DHCP. Kept for
    /// reporting; the *timers* live in [`crate::dhcp::Client`], which owns the
    /// renewal state machine.
    lease_secs: Option<u32>,
}

impl Default for HostConfig {
    fn default() -> Self {
        HostConfig::new()
    }
}

impl HostConfig {
    /// An empty, unconfigured host.
    pub fn new() -> HostConfig {
        HostConfig {
            address: None,
            netmask: None,
            gateway: None,
            dns_servers: Vec::new(),
            search_domains: Vec::new(),
            hostname: None,
            source: ConfigSource::Unconfigured,
            lease_secs: None,
        }
    }

    /// The **static SLIRP profile**: QEMU's user-mode network hands every guest
    /// `10.0.2.15/24` with the gateway at `10.0.2.2` and a DNS responder at
    /// `10.0.2.3`. This is the one place those three addresses are written down -
    /// callers that used to inline them now name this profile, so the values are
    /// configuration rather than magic numbers scattered through the stack.
    pub fn slirp() -> HostConfig {
        let mut c = HostConfig::new();
        c.set_static(
            Ipv4Addr::new(10, 0, 2, 15),
            Ipv4Addr::new(255, 255, 255, 0),
            Some(Ipv4Addr::new(10, 0, 2, 2)),
        );
        c.add_dns_server(Ipv4Addr::new(10, 0, 2, 3));
        c
    }

    /// Write a static address/netmask/gateway, marking the source
    /// [`ConfigSource::Static`].
    pub fn set_static(&mut self, address: Ipv4Addr, netmask: Ipv4Addr, gateway: Option<Ipv4Addr>) {
        self.address = Some(address);
        self.netmask = Some(netmask);
        self.gateway = gateway;
        self.source = ConfigSource::Static;
        self.lease_secs = None;
    }

    /// Apply a DHCP lease ([`crate::dhcp::Lease`]): address, netmask, router,
    /// DNS servers and the domain (as a search domain), marking the source
    /// [`ConfigSource::Dhcp`]. Previous DNS servers / search domains are replaced,
    /// not appended - a new lease is the authority on both.
    pub fn apply_lease(&mut self, lease: &crate::dhcp::Lease) {
        self.address = Some(lease.address);
        self.netmask = lease.netmask;
        self.gateway = lease.router;
        self.dns_servers = lease.dns.clone();
        self.search_domains.clear();
        if let Some(d) = &lease.domain {
            self.search_domains.push(crate::dns::normalize(d));
        }
        if let Some(h) = &lease.hostname {
            self.hostname = Some(crate::dns::normalize(h));
        }
        self.source = ConfigSource::Dhcp;
        self.lease_secs = Some(lease.lease_secs);
    }

    /// Apply a claimed IPv4 link-local address ([`crate::zeroconf`]): `169.254/16`
    /// with a `255.255.0.0` netmask and - importantly - **no gateway**, because a
    /// link-local address is not routable off the link (RFC 3927 §1.5). Any
    /// previously configured gateway is cleared so nothing tries to route through
    /// it. DNS servers are left alone: a link-local host may still have been told
    /// about resolvers by other means (and mDNS needs none).
    pub fn apply_link_local(&mut self, address: Ipv4Addr) {
        self.address = Some(address);
        self.netmask = Some(Ipv4Addr::new(255, 255, 0, 0));
        self.gateway = None;
        self.source = ConfigSource::LinkLocal;
        self.lease_secs = None;
    }

    /// Forget everything (back to [`ConfigSource::Unconfigured`]) - what a DHCP
    /// lease expiry does.
    pub fn clear(&mut self) {
        *self = HostConfig::new();
    }

    /// Add a DNS server (order is preserved; the resolver tries them in order).
    pub fn add_dns_server(&mut self, ip: Ipv4Addr) {
        self.dns_servers.push(ip);
    }

    /// Add a search domain (normalized: lowercased, trailing dot stripped).
    pub fn add_search_domain(&mut self, domain: &str) {
        self.search_domains.push(crate::dns::normalize(domain));
    }

    /// Set the hostname (normalized).
    pub fn set_hostname(&mut self, name: &str) {
        self.hostname = Some(crate::dns::normalize(name));
    }

    /// The configured address, if any.
    pub fn address(&self) -> Option<Ipv4Addr> {
        self.address
    }

    /// The address to use as an IPv4 source: the configured one, or `0.0.0.0`
    /// while unconfigured (which is exactly what a DHCP client must send from).
    pub fn source_address(&self) -> Ipv4Addr {
        self.address.unwrap_or(UNSPECIFIED)
    }

    /// The configured netmask, if any.
    pub fn netmask(&self) -> Option<Ipv4Addr> {
        self.netmask
    }

    /// The default gateway, if any. `None` for a link-local host.
    pub fn gateway(&self) -> Option<Ipv4Addr> {
        self.gateway
    }

    /// The DNS servers, in order.
    pub fn dns_servers(&self) -> &[Ipv4Addr] {
        &self.dns_servers
    }

    /// The search domains, in order.
    pub fn search_domains(&self) -> &[String] {
        &self.search_domains
    }

    /// The hostname, if set.
    pub fn hostname(&self) -> Option<&str> {
        self.hostname.as_deref()
    }

    /// Where the address came from.
    pub fn source(&self) -> ConfigSource {
        self.source
    }

    /// The DHCP lease length in seconds, if the address came from DHCP.
    pub fn lease_secs(&self) -> Option<u32> {
        self.lease_secs
    }

    /// True once an address is configured.
    pub fn is_configured(&self) -> bool {
        self.address.is_some()
    }

    /// The netmask as a prefix length (`255.255.255.0` -> 24). `None` if no mask is
    /// configured. Counts set bits, so a (non-contiguous, illegal) mask still gives
    /// a number rather than panicking - validation is [`Self::netmask_is_valid`].
    pub fn prefix_len(&self) -> Option<u32> {
        let m = self.netmask?;
        Some(u32::from_be_bytes(m.0).count_ones())
    }

    /// True if the configured netmask is a contiguous run of high bits (the only
    /// legal shape). A DHCP server handing over a non-contiguous mask is
    /// misconfigured, and a caller may want to reject the lease.
    pub fn netmask_is_valid(&self) -> bool {
        match self.netmask {
            None => false,
            Some(m) => {
                let v = u32::from_be_bytes(m.0);
                // A contiguous high-bit mask satisfies `!v + 1` being a power of
                // two (or v being all-ones): the complement must be a low-bit run.
                let inv = !v;
                inv & inv.wrapping_add(1) == 0
            }
        }
    }

    /// True if `dst` is on this host's own link (same address/netmask). Always
    /// false while unconfigured, so nothing is mistakenly treated as local.
    pub fn is_on_link(&self, dst: Ipv4Addr) -> bool {
        let (Some(a), Some(m)) = (self.address, self.netmask) else {
            return false;
        };
        let a = u32::from_be_bytes(a.0);
        let m = u32::from_be_bytes(m.0);
        let d = u32::from_be_bytes(dst.0);
        (a & m) == (d & m)
    }

    /// The **routing decision** for `dst`: the address whose MAC should be
    /// resolved to reach it.
    ///
    /// - `dst` itself when it is on-link (or is the all-ones broadcast, which is
    ///   never routed).
    /// - the gateway otherwise.
    /// - `None` when `dst` is off-link and no gateway exists (an unconfigured or
    ///   link-local host) - the caller must fail the send rather than guess.
    pub fn next_hop(&self, dst: Ipv4Addr) -> Option<Ipv4Addr> {
        if dst == BROADCAST {
            return Some(BROADCAST);
        }
        if self.is_on_link(dst) {
            return Some(dst);
        }
        self.gateway
    }

    /// The link's directed broadcast address (`address | !netmask`), e.g.
    /// `10.0.2.255` for `10.0.2.15/24`. `None` while unconfigured.
    pub fn broadcast(&self) -> Option<Ipv4Addr> {
        let (Some(a), Some(m)) = (self.address, self.netmask) else {
            return None;
        };
        let a = u32::from_be_bytes(a.0);
        let m = u32::from_be_bytes(m.0);
        Some(Ipv4Addr((a | !m).to_be_bytes()))
    }

    /// Expand `name` against the search domains, resolv.conf style: a name with no
    /// dot is tried as `name.<search>` for each search domain **first**, then bare;
    /// a name that already contains a dot (or ends in one, i.e. is explicitly
    /// absolute) is returned unchanged. The caller resolves the returned candidates
    /// in order and takes the first that answers.
    pub fn qualify(&self, name: &str) -> Vec<String> {
        let n = crate::dns::normalize(name);
        let mut out = Vec::new();
        if name.ends_with('.') || n.contains('.') || self.search_domains.is_empty() {
            out.push(n);
            return out;
        }
        for d in &self.search_domains {
            let mut q = String::with_capacity(n.len() + 1 + d.len());
            q.push_str(&n);
            q.push('.');
            q.push_str(d);
            out.push(q);
        }
        out.push(n);
        out
    }

    /// Build a resolver [`Config`](crate::dns::Config) from this store: the DNS
    /// servers become the upstream resolvers, and the hostname (when set) is added
    /// to the hosts table pointing at our own address, so the host can resolve
    /// itself without a query. Everything else keeps the resolver's defaults.
    #[cfg(feature = "hosted")]
    pub fn dns_config(&self) -> crate::dns::Config {
        let mut cfg = crate::dns::Config::new();
        for s in &self.dns_servers {
            cfg.resolvers.push(*s);
        }
        if let (Some(h), Some(a)) = (&self.hostname, self.address) {
            cfg.hosts.insert(h, crate::ip::IpAddr::V4(a));
        }
        cfg
    }
}
