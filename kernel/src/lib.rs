//! The rheo-os kernel library: boot stubs, arch layer, console, and the
//! trusted-core mechanisms (capabilities, queue pairs, cells).
//!
//! Kernel *binaries* (the boot demo in src/main.rs and the in-QEMU test
//! kernels in tests/) link this library and provide `kernel_main`; the
//! per-ISA boot stub in kernel/arch/<isa>/boot.S jumps to it with the
//! stack set up and the BSS cleared.

#![no_std]
// generic_const_exprs powers the compile-time rights-subset check from
// docs/KERNEL-RUST.md 2 (SubsetOf via a const expression). Incomplete
// feature, used only for that one bounded pattern.
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

pub mod abi;
pub mod arch;
pub mod capability;
pub mod cell;
#[macro_use]
pub mod console;
pub mod elf;
pub mod engine;
pub mod event;
pub mod graph;
pub mod hw;
pub mod input;
pub mod ktimer;
pub mod lease;
pub mod linux;
pub mod load;
pub mod mm;
pub mod net_rx;
pub mod nproc;
mod panic;
pub mod pty;
pub mod queue;
pub mod rng;
pub mod sched;
#[cfg(feature = "smp")]
pub mod smp;
pub mod svc;
pub mod time;
pub mod user;
pub mod user_progs;
