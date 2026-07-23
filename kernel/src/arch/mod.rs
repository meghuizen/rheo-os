//! Per-ISA code lives in one module per architecture. Everything outside
//! this directory must be ISA-independent: a port that needs changes
//! elsewhere is an architecture bug (docs/TARGET-ARCHITECTURES.md 4).
//!
//! Each ISA module provides the same small surface:
//! - `NAME`: human-readable ISA name
//! - `serial_init()`: bring up the boot UART
//! - `serial_write_byte(u8)`: blocking write to the boot UART
//! - `exit(ExitCode) -> !`: leave QEMU with a pass/fail status
//!
//! The assembly entry points (boot stubs) are in kernel/arch/<isa>/boot.S
//! and are included from the matching module with `global_asm!`.

/// Test-exit status reported to QEMU (DEVELOPMENT.md 6). The xtask harness
/// maps the QEMU process exit code back to pass/fail per ISA.
pub enum ExitCode {
    Success,
    Failure,
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

pub use imp::{NAME, exit, serial_init, serial_write_byte};
