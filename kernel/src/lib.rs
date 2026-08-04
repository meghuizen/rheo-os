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
/// The boot sequencer (see the module docs): portable, so `arch` need not
/// reach up into the subsystems that depend on it.
pub mod boot;
pub mod capability;
pub mod cell;
#[macro_use]
pub mod console;
pub mod elf;
pub mod engine;
pub mod event;
pub mod graph;
pub mod hw;
pub mod idle;
pub mod input;
pub mod ktimer;
pub mod lease;
pub mod linux;
pub mod load;
/// Per-CPU latency histograms with real percentiles (docs/SUBSTRATE.md
/// pillar 7). Integer-only, funded storage, off until a boot enables it.
pub mod metrics;
pub mod mm;
pub mod net_rx;
pub mod nproc;
/// The observability spine: one exported root indexing every telemetry plane, so
/// the machine can be watched from inside or outside it (docs/OBSERVABILITY.md).
pub mod obs;
mod panic;
pub mod pty;
pub mod queue;
pub mod rng;
pub mod sched;
/// Per-CPU state, the kernel spinlock, and secondary-core bring-up. The
/// **primitives** (`SpinLock`, `PerCpu<T>`, `cpu_index`) are always compiled so
/// that per-core subsystems are written once rather than once per build
/// configuration; only the bring-up half, which drives `arch::smp_*`, is behind
/// the `smp` feature. See the module header and docs/SUBSTRATE.md pillar 3.
pub mod smp;
pub mod svc;
pub mod telemetry;
pub mod time;
pub mod trace;
pub mod uaccess;
pub mod user;
pub mod user_progs;
