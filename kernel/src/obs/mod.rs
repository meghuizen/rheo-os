//! **Observability**: the spine that indexes what the kernel knows about itself
//! (docs/OBSERVABILITY.md).
//!
//! # What this is, and what it is not
//!
//! Five planes answer five different questions, and each one already has - or
//! gets - the cheapest structure that answers it:
//!
//! | Plane | Question | Where it lives |
//! |---|---|---|
//! | Text | what did it say | [`crate::telemetry`] |
//! | Event | what happened, in order | this module |
//! | Distribution | how long did it take | [`crate::metrics`] |
//! | Counter | how many | this module |
//! | Snapshot | what is it doing **now** | this module |
//!
//! Collapsing them is what makes observability expensive: a histogram stored as
//! events costs a record per sample, and a live gauge stored as a log line costs a
//! parse per read. So this module does not absorb the two that already exist - it
//! *indexes* them, by publishing their addresses in one root a reader can find.
//!
//! # Why the root, rather than a syscall
//!
//! The most useful moments to inspect a kernel are the ones where it is least able
//! to answer a question: wedged, faulting, or halfway through bringing a core up.
//! A plane that is plain memory behind one exported symbol is readable in all of
//! them - by a host debugger, by a hypervisor, out of a crash dump - and readable
//! with **zero** guest instructions, so watching does not perturb what is being
//! watched. A syscall could do none of that. The syscall surface exists too, for a
//! collector cell that has no other way in, but it is the second reader, not the
//! first.
//!
//! # Cost
//!
//! Nothing here runs until a boot turns a window on. The published root is one
//! page of `.data`; the planes it indexes allocate only when used. That is not a
//! politeness - it is what lets the existing kernels stay byte-for-byte what they
//! were, which is the only way a framework this size lands without invalidating
//! every proof already in the tree.

pub mod root;

pub use root::{publish, refresh_online, root_va};
