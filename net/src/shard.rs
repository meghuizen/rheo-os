//! Native sharded transport framing (docs/NETSTACK.md §13, Phase N2c): one
//! transport instance per shard, connections hashed to shards by their 4-tuple,
//! `connect`/`listen` routed to the owning shard - the Snap/Seastar
//! **shared-nothing** shape. Each [`Shard`] owns a **disjoint** set of
//! connections in its own [`BTreeMap`]; there is **no shared mutable stack
//! state** between shards, so a flood or a bug on one shard's connections cannot
//! reach another's (the DDoS-isolation story of docs/NETSTACK.md 1).
//!
//! **Honest under the single-CPU cooperative model (docs/CONCURRENCY.md, SMP is
//! task #27):** the shards **interleave on one core** - this is *structural*
//! isolation (disjoint ownership, no cross-shard aliasing), **not** parallel
//! throughput. A truly parallel per-core transport - each shard pinned to its
//! own hart/vcore, connections steered by hardware RSS - awaits SMP. What N2c
//! delivers is the *framing*: the hash-to-shard routing and the shared-nothing
//! ownership discipline, proven deterministically in-cell, so the parallel
//! version is a scheduling change, not a rewrite.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::ip::Ipv4Addr;
use crate::tcp::{Connection, FixedWindow};

/// A TCP connection identity: the canonical 4-tuple (local ip/port, remote
/// ip/port). Two endpoints of one connection are **mirror** tuples (local and
/// remote swapped), and generally hash to different shards - which is exactly
/// how a server fans its accepted connections across shards.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct FourTuple {
    pub local_ip: Ipv4Addr,
    pub local_port: u16,
    pub remote_ip: Ipv4Addr,
    pub remote_port: u16,
}

// Ordered by the canonical 12 bytes so `FourTuple` can key a `BTreeMap` (the
// per-shard connection table). `Ipv4Addr` does not derive `Ord`, so the order is
// defined here over `bytes()` rather than by touching the shared address type.
impl Ord for FourTuple {
    fn cmp(&self, other: &FourTuple) -> core::cmp::Ordering {
        self.bytes().cmp(&other.bytes())
    }
}
impl PartialOrd for FourTuple {
    fn partial_cmp(&self, other: &FourTuple) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl FourTuple {
    /// The 12 canonical bytes hashed for shard selection: local ip(4) + local
    /// port(2) + remote ip(4) + remote port(2), big-endian.
    fn bytes(&self) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[0..4].copy_from_slice(&self.local_ip.0);
        b[4..6].copy_from_slice(&self.local_port.to_be_bytes());
        b[6..10].copy_from_slice(&self.remote_ip.0);
        b[10..12].copy_from_slice(&self.remote_port.to_be_bytes());
        b
    }

    /// FNV-1a hash of the 4-tuple - the same from-scratch idiom `net::dns`'s
    /// blocklist uses. Deterministic and portable (no per-ISA code). A real
    /// deployment would key this with a per-epoch seed (docs/NETSTACK.md 8.3) to
    /// resist collision-crafting; the seed is a later-phase refinement.
    pub fn hash(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &x in self.bytes().iter() {
            h ^= x as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// The mirror tuple (local and remote swapped) - the peer end of this
    /// connection.
    pub fn mirror(&self) -> FourTuple {
        FourTuple {
            local_ip: self.remote_ip,
            local_port: self.remote_port,
            remote_ip: self.local_ip,
            remote_port: self.local_port,
        }
    }
}

/// One transport shard: it owns a **disjoint** subset of the connections (those
/// whose 4-tuple hashes to it) in its own map. No other shard can name or reach
/// these connections - the shared-nothing guarantee.
pub struct Shard {
    id: usize,
    conns: BTreeMap<FourTuple, Connection<FixedWindow>>,
}

impl Shard {
    fn new(id: usize) -> Shard {
        Shard {
            id,
            conns: BTreeMap::new(),
        }
    }

    /// This shard's index.
    pub fn id(&self) -> usize {
        self.id
    }

    /// How many connections this shard owns.
    pub fn len(&self) -> usize {
        self.conns.len()
    }

    /// Whether this shard owns no connections.
    pub fn is_empty(&self) -> bool {
        self.conns.is_empty()
    }

    /// Whether this shard owns the connection identified by `t`.
    pub fn contains(&self, t: &FourTuple) -> bool {
        self.conns.contains_key(t)
    }

    /// The connection `t`, if this shard owns it.
    pub fn get_mut(&mut self, t: &FourTuple) -> Option<&mut Connection<FixedWindow>> {
        self.conns.get_mut(t)
    }
}

/// The sharded transport: `n` shard instances, connections hashed to shards by
/// their 4-tuple. `connect`/`listen` route to the owning shard; cross-shard
/// traffic (a connection whose two ends live in different shards) crosses an
/// explicit boundary, never shared state.
pub struct Transport {
    shards: Vec<Shard>,
}

impl Transport {
    /// A transport with `n_shards` shards (n >= 1).
    pub fn new(n_shards: usize) -> Transport {
        let n = n_shards.max(1);
        let mut shards = Vec::with_capacity(n);
        for i in 0..n {
            shards.push(Shard::new(i));
        }
        Transport { shards }
    }

    /// The number of shards.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// The shard index that owns connection `t` (`hash(t) % n`). Deterministic:
    /// the same tuple always maps to the same shard.
    pub fn shard_index(&self, t: &FourTuple) -> usize {
        (t.hash() % self.shards.len() as u64) as usize
    }

    /// Active-open a connection (`t`), routed to its owning shard. Returns the
    /// shard index. `iss` is the initial send sequence (the caller supplies it,
    /// e.g. from the per-cell DRBG).
    pub fn connect(&mut self, t: FourTuple, iss: u32) -> usize {
        let idx = self.shard_index(&t);
        let conn = Connection::connect(t.local_ip, t.local_port, t.remote_ip, t.remote_port, iss);
        self.shards[idx].conns.insert(t, conn);
        idx
    }

    /// Passive-open (a listener accepting) a connection (`t`), routed to its
    /// owning shard. Returns the shard index.
    pub fn listen(&mut self, t: FourTuple, iss: u32) -> usize {
        let idx = self.shard_index(&t);
        let conn = Connection::listen(t.local_ip, t.local_port, t.remote_ip, t.remote_port, iss);
        self.shards[idx].conns.insert(t, conn);
        idx
    }

    /// A read-only view of shard `i`.
    pub fn shard(&self, i: usize) -> &Shard {
        &self.shards[i]
    }

    /// The connection `t`, wherever it is owned (routed to its shard by hash).
    pub fn get_mut(&mut self, t: &FourTuple) -> Option<&mut Connection<FixedWindow>> {
        let idx = self.shard_index(t);
        self.shards[idx].get_mut(t)
    }
}
