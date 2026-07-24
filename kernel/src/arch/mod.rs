//! Per-ISA code lives in one module per architecture. Everything outside
//! this directory must be ISA-independent: a port that needs changes
//! elsewhere is an architecture bug (docs/TARGET-ARCHITECTURES.md 4).
//!
//! Surface each ISA module provides:
//! - `NAME`: human-readable ISA name
//! - `serial_init()` / `serial_write_byte(u8)`: the boot UART
//! - `trap_init()`: install exception vectors (faults print and exit)
//! - `cycles()`: monotonic tick counter for benchmarks
//! - `doorbell_trap()` / `doorbell_count()`: one in-kernel privilege-round
//!   trip (int3 / svc / ebreak), used by the boot self-test
//! - `context_switch` / `context_init`: cooperative context-switch loop
//! - `exit(ExitCode) -> !`: leave QEMU with a pass/fail status
//!
//! Paging + user mode (BUILD-ORDER.md steps 3, 5):
//! - `FRAME_POOL_BASE`: physical base of the frame pool (above the image)
//! - `PagingRoot`, `paging_new_root`, `paging_map`, `paging_activate`
//! - `paging_kernel_init`: build + activate the kernel address space,
//!   turning the MMU on with the kernel identity-mapped supervisor-only
//! - `TrapFrame`, `trapframe_new`, `decode_syscall`, `set_syscall_ret`
//! - `enter_user_first` / `return_to_kernel`: enter U/EL0/ring-3 and the
//!   matching unwind back to the kernel run loop
//!
//! The assembly (boot stub, vectors/trampolines, context switch) is in
//! kernel/arch/<isa>/ and included from the matching module.

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

/// Permission for a user page mapping. W^X is structural - there is no
/// writable-and-executable variant.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MapPerm {
    /// User read-only.
    UserRo,
    /// User read + write, never executable.
    UserRw,
    /// User read + execute, never writable.
    UserRx,
}

/// Why a U-mode context trapped back into the kernel.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TrapKind {
    /// A syscall instruction (ecall / svc / syscall).
    Syscall,
    /// A memory or instruction fault (the isolation tests rely on this).
    Fault,
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
    FRAME_POOL_BASE, NAME, PagingRoot, TrapFrame, VIRTIO_MMIO_BASE, VIRTIO_MMIO_COUNT,
    VIRTIO_MMIO_STRIDE, context_init, context_switch, cpu_feature_names, cpu_report, cycles,
    decode_syscall, discover, doorbell_count, doorbell_trap, enter_user_first, exit, has_hwrng,
    hwrng_name, hwrng_u64, paging_activate, paging_activate_kernel, paging_kernel_init, paging_map,
    paging_new_root, pci_cfg_read32, pci_cfg_write32, return_to_kernel, serial_init,
    serial_read_byte, serial_write_byte, set_syscall_ret, spin_loop, trap_init, trapframe_new,
};

/// Full arch bring-up for a kernel binary: console, exception vectors,
/// then the frame allocator and the kernel address space (MMU on).
pub fn init() {
    serial_init();
    trap_init();
    paging_kernel_init();
    crate::time::init();
    crate::hw::detect();
    crate::rng::init();
    crate::svc::init();
}
