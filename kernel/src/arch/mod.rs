//! Per-ISA code lives in one module per architecture. Everything outside
//! this directory must be ISA-independent: a port that needs changes
//! elsewhere is an architecture bug (docs/TARGET-ARCHITECTURES.md 4).
//!
//! Each ISA module provides the same small surface:
//! - `NAME`: human-readable ISA name
//! - `serial_init()` / `serial_write_byte(u8)`: the boot UART
//! - `trap_init()`: install exception vectors (faults print and exit)
//! - `cycles()`: monotonic tick counter for benchmarks (see below)
//! - `doorbell_trap()` / `doorbell_count()`: one real privilege-boundary
//!   round trip (int3 / svc / ebreak), the measurement floor for a
//!   doorbell-style kernel entry
//! - `context_switch` / `context_init`: the cooperative context-switch
//!   inner loop (assembly, per docs/TOOLING.md 1)
//! - `exit(ExitCode) -> !`: leave QEMU with a pass/fail status
//!
//! The assembly (boot stub, vectors, context switch) is in
//! kernel/arch/<isa>/ and included from the matching module with
//! `global_asm!`.
//!
//! On `cycles()`: each ISA reads its native counter (rdtsc / cntvct_el0 /
//! rdcycle). Tick meaning differs per ISA and per QEMU mode; benchmarks
//! must self-calibrate with a known instruction loop (see the bench
//! kernel) instead of assuming a tick:instruction ratio.

/// Test-exit status reported to QEMU (DEVELOPMENT.md 6). The xtask harness
/// maps the QEMU process exit code back to pass/fail per ISA.
pub enum ExitCode {
    Success,
    Failure,
}

/// A saved cooperative execution context: everything lives on its stack,
/// so the context is just the stack pointer.
#[repr(C)]
pub struct Context {
    pub sp: usize,
}

#[cfg(target_arch = "x86_64")]
#[path = "x86_64/mod.rs"]
mod imp;

#[cfg(target_arch = "aarch64")]
#[path = "aarch64/mod.rs"]
mod imp;

#[cfg(target_arch = "riscv64")]
#[path = "riscv64/mod.rs"]
mod imp;

pub use imp::{
    NAME, context_init, context_switch, cycles, doorbell_count, doorbell_trap, exit, serial_init,
    serial_write_byte, spin_loop, trap_init,
};

/// Full arch bring-up for a kernel binary: console first (so trap reports
/// can print), then the exception vectors.
pub fn init() {
    serial_init();
    trap_init();
}
