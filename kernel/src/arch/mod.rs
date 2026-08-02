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
    /// User read + write + execute - **the capability-gated W^X exception**
    /// (docs/ARCHITECTURE.md 5.1).
    ///
    /// A fourth variant rather than a flag on `UserRw`, so no code path can produce
    /// a writable-executable mapping by forgetting a check: producing one requires
    /// naming this, and the only place that names it is the `mprotect`/`mmap` arm
    /// that has already verified the calling cell holds a `MemoryGrant` capability
    /// carrying `RIGHT_WRITE | RIGHT_EXECUTE`. Every `match` over `MapPerm` in the
    /// tree had to be updated to add it, which is the point - an exhaustive match is
    /// how the compiler makes a security-relevant addition impossible to miss.
    UserRwx,
    /// User read + write over **device MMIO** - a mapped BAR window
    /// (docs/DRIVERS.md 4.1, the `BarWindow` grant).
    ///
    /// A distinct variant rather than a flag, for the same reason `UserRwx` is one:
    /// producing a device mapping requires naming it, and the only place that names it
    /// is the launcher-side BAR grant. It differs from `UserRw` in the memory
    /// *attribute*, not the permissions - device registers must not be cached,
    /// speculated into, or write-combined, because a read of a status register is a
    /// side-effecting bus transaction rather than a load of a value.
    ///
    /// Per ISA: x86-64 sets `PCD|PWT` (PAT entry 3 = UC); ARM64 selects MAIR attr 0
    /// (Device-nGnRnE, the attribute its own kernel MMIO window uses) and drops the
    /// inner-shareable hint, which is meaningless for device memory; RISC-V's base Sv39
    /// PTE carries **no** cacheability bits at all, so the mapping is the same as
    /// `UserRw` there and the attribute is a property of the physical region instead
    /// (Svpbmt would add one - not present in QEMU 8.2, and named rather than faked).
    UserDevice,
}

/// Why a U-mode context trapped back into the kernel.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TrapKind {
    /// A syscall instruction (ecall / svc / syscall).
    Syscall,
    /// A memory or instruction fault (the isolation tests rely on this).
    Fault,
}

/// The kind of a U-mode fault, classified per ISA (vector / ESR EC / scause)
/// and mapped to a POSIX signal by the Linux personality (docs/LINUX-COMPAT.md
/// L5). Only meaningful when `TrapKind::Fault`; the syscall path passes an
/// arbitrary value. Kept portable so `crate::user`/`crate::linux` never name a
/// per-ISA trap cause (the arch layer owns the decode).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FaultCause {
    /// Bad memory access (page/access fault, protection): SIGSEGV.
    Segv,
    /// Misaligned/unsupported access: SIGBUS.
    Bus,
    /// Illegal / undefined instruction: SIGILL.
    Ill,
    /// Arithmetic exception (divide error): SIGFPE.
    Fpe,
}

/// What the arch layer needs to build a Linux `rt_sigframe` on the user stack
/// and rewrite a `TrapFrame` to enter a signal handler (docs/LINUX-COMPAT.md
/// L5). The register/ucontext layout is ISA-specific, so the frame is built in
/// the arch layer; the portable personality (`crate::linux::signal`) fills this
/// in and calls [`setup_rt_frame`].
pub struct SigFrameSpec {
    /// Signal number (1..64).
    pub signo: u32,
    /// User VA of the handler to enter.
    pub handler: u64,
    /// User `sa_restorer` (x86-64 supplies one via glibc). Ignored on
    /// ARM64/RISC-V, which return through the injected trampoline (`SIGTRAMP_VA`).
    pub restorer: u64,
    /// The signal mask to save into the ucontext (restored by `rt_sigreturn`).
    pub saved_mask: u64,
    /// `siginfo.si_code`.
    pub si_code: i32,
    /// `siginfo.si_addr` (fault address for synchronous signals; 0 otherwise).
    pub si_addr: u64,
    /// Top of the stack region to build the frame on (current SP, or the
    /// sigaltstack top when SA_ONSTACK is honored).
    pub stack_top: u64,
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

pub use imp::linux_abi;
pub use imp::{
    CLONE_BACKWARDS, FP_AREA_LEN, FRAME_POOL_BASE, LINUX_UNAME_MACHINE, NAME, PagingRoot,
    SIGACTION_HAS_RESTORER, SIGTRAMP_VA, TrapFrame, USER_VA_TOP, VIRTIO_MMIO_BASE,
    VIRTIO_MMIO_COUNT, VIRTIO_MMIO_STRIDE, clone_child_frame, context_init, context_switch,
    cpu_class_this_cpu, cpu_feature_names, cpu_model_this_cpu, cpu_report, cpu_topology_bits,
    cycles, decode_syscall, discover, doorbell_count, doorbell_trap, enable_timer_irq,
    enable_timer_irq_this_cpu, enable_uart_rx_irq, enable_virtio_net_irq, enter_user_first, exit,
    fp_area_init, fp_simd_tiers, has_hwrng, hwrng_name, hwrng_u64, idle_wait, irq_ready_this_cpu,
    irq_window, mmio_map_window, msi_irq_count, msi_route, msi_target, net_irq_enabled,
    net_irq_pending, paging_activate, paging_activate_kernel, paging_cow_at, paging_cow_clear,
    paging_cow_protect_user, paging_flush_asid, paging_for_each_user_leaf, paging_kernel_init,
    paging_map, paging_map_frame, paging_mapped, paging_new_root, paging_protect,
    paging_tlb_tagged, paging_unmap_frame, paging_unmapped_span, pci_cfg_read32, pci_cfg_write32,
    pci_mmio_window, phys_to_virt, pmem_map_window, restore_rt_frame, restore_user_fp,
    return_to_kernel, save_user_fp, serial_init, serial_read_byte, serial_write_byte,
    set_syscall_ret, set_user_fs_base, setup_rt_frame, sig_tramp_code, spin_loop,
    thread_director_present, ticks_to_ns, timer_arm, timer_disarm, timer_expired,
    timer_irq_enabled, timer_now_ns, timer_park, tlb_flushes, trap_init, trapframe_kernel_sp,
    trapframe_new, trapframe_zeroed, uart_inject_and_wait, uart_irq_enabled, user_fs_base,
    user_mode_init_this_cpu, user_sp, virt_to_phys,
};

/// SMP surface (docs/SMP.md, task #27), exported only under the `smp` feature so
/// the non-SMP kernels link a byte-identical `kernel` lib.
#[cfg(feature = "smp")]
pub use imp::{
    boot_cpu_hw_id, cpu_index, smp_prepare_secondary, smp_secondary_count, smp_set_this_cpu,
    smp_start_secondary,
};

/// The **arch's own** bring-up: serial console, exception vectors, then the
/// kernel address space with the MMU on.
///
/// It deliberately stops there. The portable subsystems that must also start
/// before a cell runs (clock, hardware discovery, DRBG, `svc`) are sequenced by
/// [`crate::boot::init`], which is what every kernel binary calls. Starting them
/// from here made `arch` reference four modules that depend on `arch` - three
/// module cycles from three lines, none of them per-ISA
/// (docs/ARCHITECTURE-DEBT.md 3.6). Nothing above `arch` is named here now.
pub fn init() {
    serial_init();
    trap_init();
    paging_kernel_init();
}
