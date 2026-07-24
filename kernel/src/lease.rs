//! Leases (docs/ARCHITECTURE.md 3 object 8, docs/SECURITY-IDENTITY.md 3):
//! an expiring grant with a fencing token. Locks between cells, failure
//! detection, and slow-consumer handling are all this one object.
//!
//! The honest contract (SECURITY-IDENTITY.md 3): revocation is eventual
//! within a bounded window. A lease carries a TTL and an epoch; a resource
//! guarded by leases records the highest fencing token it has honoured and
//! rejects any action carrying an older token, so a stale holder whose
//! clock drifted is detectable *at the resource* even before its lease is
//! known to have expired.

use crate::time;
use core::sync::atomic::{AtomicU64, Ordering};

/// Globally increasing fencing-token source (docs/SECURITY-IDENTITY.md 3).
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LeaseError {
    Expired,
    Fenced,
}

/// An expiring grant. `token` strictly increases across all leases;
/// `expiry` is a monotonic tick.
#[derive(Copy, Clone, Debug)]
pub struct Lease {
    pub token: u64,
    pub expiry: u64,
    pub epoch: u32,
}

impl Lease {
    /// Acquire a lease valid for `ttl_ticks` from now.
    pub fn acquire(ttl_ticks: u64, epoch: u32) -> Lease {
        Lease {
            token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
            expiry: time::monotonic().wrapping_add(ttl_ticks),
            epoch,
        }
    }

    /// Extend the lease by `ttl_ticks` from now (keeps the same token).
    pub fn renew(&mut self, ttl_ticks: u64) {
        self.expiry = time::monotonic().wrapping_add(ttl_ticks);
    }

    /// Whether the lease is still within its window at `now`.
    pub fn valid_at(&self, now: u64) -> bool {
        now < self.expiry
    }
}

/// A resource protected by lease fencing. Records the highest token it has
/// acted on; a request with an older token is a stale actor and is fenced.
pub struct FencedResource {
    highest_token: u64,
    epoch: u32,
}

impl FencedResource {
    pub const fn new() -> FencedResource {
        FencedResource {
            highest_token: 0,
            epoch: 0,
        }
    }

    /// Admit an action guarded by `lease`. Fails if the lease expired or a
    /// newer holder has already acted (fencing).
    pub fn act(&mut self, lease: &Lease) -> Result<(), LeaseError> {
        if !lease.valid_at(time::monotonic()) || lease.epoch != self.epoch {
            return Err(LeaseError::Expired);
        }
        if lease.token < self.highest_token {
            return Err(LeaseError::Fenced);
        }
        self.highest_token = lease.token;
        Ok(())
    }

    /// Revoke by epoch: every lease from an older epoch is now invalid
    /// (docs/SECURITY-IDENTITY.md 3, same eventual-within-a-window rule as
    /// capability revocation).
    pub fn revoke_epoch(&mut self) {
        self.epoch += 1;
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }
}

impl Default for FencedResource {
    fn default() -> Self {
        Self::new()
    }
}
