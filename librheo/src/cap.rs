//! Capability handles (docs/LIBRHEO.md, docs/KERNEL-RUST.md 2). The security
//! spine: a capability is a typed handle whose rights live in its type, so
//! *widening* rights is a compile error (the kernel still grant-checks every
//! use). This re-exports the zero-cost `Rights<MASK>`/`SubsetOf`/`Cap` layer
//! from `runtime`, and adds `CapSet` - the bundle a cell obtains at startup.
//!
//! In Phase A the only object a loaded cell holds is its queue pair, minted by
//! the kernel and reported through `SYS_QUEUE_INFO`. Later phases add memory
//! grants, files, streams, leases, etc. to the set.

pub use runtime::rights::{
    Cap, EXECUTE, Executable, Full, MAP, READ, ReadOnly, ReadWrite, Rights, SubsetOf, WRITE,
};

use crate::sys::Qp;

/// A queue-pair capability: read+write, so the cell can submit to and reap
/// from its ring. The kernel enforces the actual rights on every submission.
pub type QueueCap = Cap<Qp, ReadWrite>;

/// The set of capabilities a cell holds. Phase A: just the queue pair. The
/// typed handle carries the 32-bit ABI id in its low bits (what the kernel's
/// grant check reads from a queue entry).
pub struct CapSet {
    queue: QueueCap,
}

impl CapSet {
    /// Build the set from the `cap_id` the kernel reported for the queue.
    pub fn new(queue_cap_id: u32) -> CapSet {
        CapSet {
            queue: Cap::from_handle(queue_cap_id as u64),
        }
    }

    /// The queue-pair capability (read+write).
    pub fn queue(&self) -> &QueueCap {
        &self.queue
    }

    /// The 32-bit ABI id to stamp into a submission entry's `cap_id`.
    pub fn queue_cap_id(&self) -> u32 {
        self.queue.handle() as u32
    }
}

static mut CAP_SET: Option<CapSet> = None;

/// Install the process capability set (called once by `_start`).
///
/// # Safety
/// Called once, before any use of [`cap_set`].
pub unsafe fn install(cs: CapSet) {
    unsafe {
        *core::ptr::addr_of_mut!(CAP_SET) = Some(cs);
    }
}

/// The capabilities this cell holds.
pub fn cap_set() -> &'static CapSet {
    // SAFETY: installed once at startup, read-only afterwards.
    unsafe {
        (*core::ptr::addr_of!(CAP_SET))
            .as_ref()
            .expect("librheo: cap set used before startup")
    }
}
