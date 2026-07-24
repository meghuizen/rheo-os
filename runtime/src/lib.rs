//! The strand runtime (docs/BUILD-ORDER.md step 7, docs/CONCURRENCY.md): the
//! userspace library that gives a cell native async and a heap, built on the
//! OS's own primitives and philosophy rather than a POSIX threading model.
//!
//! - `heap`: a free-list allocator so `alloc` works in a cell (the kernel
//!   itself is allocation-free; the runtime brings the heap).
//! - `strand`: an async executor. A **strand** is a user-level task (a
//!   `Future`); "blocking" is structural - a strand parks on a token and the
//!   runtime runs the next one. The token is exactly the queue-pair
//!   completion's `user_data`, so the reactor unparks a strand by id with no
//!   syscall and no kernel thread (CONCURRENCY.md 1). No blocking ever loses
//!   a vcore.
//! - `channel`: an mpsc channel over strands - "blocking" send/recv that is
//!   really park/wake, the OS-native replacement for a mutex+condvar queue.
//! - `rights`: capability rights encoded at the type level (KERNEL-RUST.md 2),
//!   so a wrong-rights access is a compile error, not just a runtime
//!   grant-check failure.
//!
//! The Rust type system, traits, generics, and async/await all work natively
//! here: `spawn` takes any `Future`, monomorphised per call; `channel<T>` is
//! generic; rights are const-generic. This is "full Rust" running on the OS's
//! terms.

#![no_std]
#![feature(generic_const_exprs)]
#![allow(incomplete_features)]

extern crate alloc;

pub mod channel;
pub mod heap;
pub mod rights;
pub mod strand;

pub use heap::Heap;
pub use strand::{StrandId, complete, has_pending, next_token, park_on, reset, run, spawn, stats};
