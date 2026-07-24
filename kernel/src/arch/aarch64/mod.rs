//! ARM64: QEMU virt machine, PL011 UART at 0x0900_0000, semihosting exit,
//! VBAR_EL1 traps, cntvct_el0, and the context-switch stub.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

/// Linux personality ABI (asm-generic table, shared with RISC-V;
/// docs/LINUX-COMPAT.md).
#[path = "../linux_abi_generic.rs"]
pub mod linux_abi;
mod paging;
pub use paging::{
    PagingRoot, paging_activate, paging_activate_kernel, paging_kernel_init, paging_map,
    paging_map_frame, paging_new_root, paging_protect, paging_unmap_frame,
};

/// `uname` machine string for the Linux personality (docs/LINUX-COMPAT.md L2).
pub const LINUX_UNAME_MACHINE: &str = "aarch64";

/// clone(2) argument order (docs/LINUX-COMPAT.md L4): ARM64 selects
/// `CLONE_BACKWARDS`, so the raw order is `(flags, stack, parent_tid, tls,
/// child_tid)` - `tls` and `child_tid` are swapped relative to x86-64.
pub const CLONE_BACKWARDS: bool = true;

global_asm!(include_str!("../../../arch/aarch64/boot.S"));
global_asm!(include_str!("../../../arch/aarch64/vectors.S"));
global_asm!(include_str!("../../../arch/aarch64/context_switch.S"));

pub const NAME: &str = "ARM64";

/// Physical base of the frame pool: 64 MiB into RAM, above the kernel
/// image (checked against __kernel_end in frames::init).
pub const FRAME_POOL_BASE: usize = 0x4400_0000;

/// Kernel linear-map offset (docs/MEMORY.md): the kernel runs in the high
/// canonical half over TTBR1_EL1, so a physical address is reached at
/// `pa | KERNEL_VA_BASE`. The whole low half (TTBR0_EL1) is left to user
/// programs. The boot trampoline builds this map before any Rust runs, and
/// the kernel is linked at `phys_to_virt(load address)` (link/aarch64.ld).
pub const KERNEL_VA_BASE: usize = 0xFFFF_0000_0000_0000;

/// Physical address -> kernel virtual address (the high linear map).
#[inline(always)]
pub fn phys_to_virt(pa: usize) -> usize {
    pa | KERNEL_VA_BASE
}

/// Kernel virtual address (high linear map) -> physical address.
#[inline(always)]
pub fn virt_to_phys(va: usize) -> usize {
    va & !KERNEL_VA_BASE
}

// ---------------------------------------------------------------- serial

// MMIO the kernel touches while a cell root (TTBR0) is active - the serial
// UART for cell stdout/stdin - must sit in the shared TTBR1 map, so its base
// is a high linear-map VA. Device MMIO used only at boot (PCIe ECAM, virtio)
// is likewise reached high for uniformity.
const PL011_BASE: usize = 0x0900_0000 | KERNEL_VA_BASE;
const PL011_DR: *mut u32 = PL011_BASE as *mut u32; // data register
const PL011_FR: *mut u32 = (PL011_BASE + 0x18) as *mut u32; // flag register
const FR_TXFF: u32 = 1 << 5; // transmit FIFO full

pub fn serial_init() {
    // QEMU's PL011 is usable as-is for TX; real init comes with the driver.
}

pub fn serial_write_byte(byte: u8) {
    unsafe {
        while PL011_FR.read_volatile() & FR_TXFF != 0 {}
        PL011_DR.write_volatile(byte as u32);
    }
}

const FR_RXFE: u32 = 1 << 4; // receive FIFO empty

/// Non-blocking read of one byte from the PL011, or None if none pending.
pub fn serial_read_byte() -> Option<u8> {
    unsafe {
        if PL011_FR.read_volatile() & FR_RXFE != 0 {
            None
        } else {
            Some(PL011_DR.read_volatile() as u8)
        }
    }
}

// ----------------------------------------------------------------- traps

unsafe extern "C" {
    static vector_table: u8;
}

pub fn trap_init() {
    unsafe {
        asm!(
            "msr vbar_el1, {0}",
            "isb",
            in(reg) core::ptr::addr_of!(vector_table),
        );
    }
}

static DOORBELLS: AtomicU64 = AtomicU64::new(0);

const EC_SVC64: u64 = 0x15;

/// Called from the "current EL, SPx, synchronous" vector. SVC is the
/// doorbell stand-in and returns (ELR already points past the svc);
/// every other exception class is fatal.
#[unsafe(no_mangle)]
extern "C" fn aarch64_sync_handler(esr: u64, elr: u64) {
    if (esr >> 26) & 0x3F == EC_SVC64 {
        DOORBELLS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    crate::println!("TRAP: sync exception, esr {esr:#x} at elr {elr:#x}");
    exit(super::ExitCode::Failure);
}

/// Called for the 15 vector slots that should never fire at this stage.
#[unsafe(no_mangle)]
extern "C" fn aarch64_fatal_handler(slot: u64, esr: u64, elr: u64) -> ! {
    crate::println!("TRAP: unexpected vector slot {slot}, esr {esr:#x} at elr {elr:#x}");
    exit(super::ExitCode::Failure)
}

/// One kernel-entry round trip via svc (the doorbell measurement floor).
pub fn doorbell_trap() {
    unsafe { asm!("svc #0") };
}

pub fn doorbell_count() -> u64 {
    DOORBELLS.load(Ordering::Relaxed)
}

// ----------------------------------------------------- hardware discovery

/// Discover the machine. QEMU's arm virt hands a bare ELF no firmware
/// table - x0 arrives as 0 and no DTB is placed in guest RAM - so we use
/// the fixed QEMU virt platform profile (hw/arm/virt.c) for memory and the
/// PCIe ECAM window. CPU topology needs a firmware table too: PSCI is the
/// only enumeration path and it is unusable from EL1 here (SMC traps with
/// no EL3, HVC needs EL2), so ARM64 reports the boot CPU only. On x86 and
/// RISC-V the full CPU count comes from ACPI / the device tree.
pub fn discover(inv: &mut crate::hw::Inventory) {
    inv.firmware = crate::hw::Firmware::Builtin;
    // QEMU virt: RAM at 0x4000_0000. We map (and therefore report) the
    // first gigabyte; larger -m would need the map extended.
    inv.add_mem(0x4000_0000, 0x4000_0000, crate::hw::MemKind::Ram, 0);
    // PCIe ECAM low window (QEMU virt with highmem-ecam=off), inside the
    // device gigabyte the kernel identity-maps.
    inv.ecam_base = 0x3f00_0000;
    inv.add_cpu(0, 0);
}

/// Feature names; bit i corresponds to index i in CpuReport.features.
pub fn cpu_feature_names() -> &'static [&'static str] {
    &[
        "fp", "asimd", "aes", "pmull", "sha1", "sha2", "crc32", "atomics", "sha3", "sm4",
        "dotprod", "sve",
    ]
}

/// Decode CPU features from the AArch64 ID registers (readable at EL1).
pub fn cpu_report(_inv: &crate::hw::Inventory) -> crate::hw::CpuReport {
    let mut report = crate::hw::CpuReport::EMPTY;
    let (isar0, pfr0, midr): (u64, u64, u64);
    unsafe {
        asm!("mrs {0}, id_aa64isar0_el1", out(reg) isar0);
        asm!("mrs {0}, id_aa64pfr0_el1", out(reg) pfr0);
        asm!("mrs {0}, midr_el1", out(reg) midr);
    }
    // PFR0: FP [19:16], AdvSIMD [23:20] present unless the field is 0xF.
    let fp = (pfr0 >> 16) & 0xF;
    let simd = (pfr0 >> 20) & 0xF;
    let sve = (pfr0 >> 32) & 0xF;
    // ISAR0 fields: nonzero means present.
    let aes = (isar0 >> 4) & 0xF;
    let sha1 = (isar0 >> 8) & 0xF;
    let sha2 = (isar0 >> 12) & 0xF;
    let crc32 = (isar0 >> 16) & 0xF;
    let atomics = (isar0 >> 20) & 0xF;
    let sha3 = (isar0 >> 32) & 0xF;
    let sm4 = (isar0 >> 40) & 0xF;
    let dp = (isar0 >> 44) & 0xF;

    let mut set = |bit: u32, on: bool| {
        if on {
            report.features |= 1 << bit;
        }
    };
    set(0, fp != 0xF);
    set(1, simd != 0xF);
    set(2, aes >= 1);
    set(3, aes >= 2); // AES value 2 => PMULL as well
    set(4, sha1 != 0);
    set(5, sha2 != 0);
    set(6, crc32 != 0);
    set(7, atomics != 0);
    set(8, sha3 != 0);
    set(9, sm4 != 0);
    set(10, dp != 0);
    set(11, sve != 0);

    let implementer = (midr >> 24) & 0xFF;
    let vendor: &[u8] = if implementer == 0x41 {
        b"ARM"
    } else {
        b"aarch64"
    };
    report.vendor[..vendor.len()].copy_from_slice(vendor);
    report
}

// ---------------------------------------------------- virtio-mmio slots
// QEMU arm `virt`: 32 virtio-mmio transports at 0x0a00_0000, stride 0x200
// (within the 1 GiB device block the kernel identity-maps).
pub const VIRTIO_MMIO_BASE: usize = 0x0a00_0000 | KERNEL_VA_BASE;
pub const VIRTIO_MMIO_STRIDE: usize = 0x200;
pub const VIRTIO_MMIO_COUNT: usize = 32;

// ----------------------------------------------------- hardware RNG

/// True if FEAT_RNG (the RNDR/RNDRRS registers) is implemented:
/// ID_AA64ISAR0_EL1.RNDR field [63:60] is nonzero.
pub fn has_hwrng() -> bool {
    let isar0: u64;
    unsafe { asm!("mrs {0}, id_aa64isar0_el1", out(reg) isar0) };
    (isar0 >> 60) & 0xF != 0
}

pub fn hwrng_name() -> &'static str {
    if has_hwrng() { "RNDR" } else { "none" }
}

/// One 64-bit hardware random word from RNDR, or None if the entropy source
/// was not ready within the bounded retry budget (never blocks). RNDR sets
/// PSTATE.NZCV: Z=0 (NE) on success, Z=1 on failure.
pub fn hwrng_u64() -> Option<u64> {
    if !has_hwrng() {
        return None;
    }
    for _ in 0..64 {
        let (val, ok): (u64, u64);
        // SAFETY: FEAT_RNG present (checked above). RNDR is S3_3_C2_C4_0.
        unsafe {
            asm!(
                "mrs {v}, s3_3_c2_c4_0",
                "cset {ok}, ne",
                v = out(reg) val,
                ok = out(reg) ok,
                options(nostack),
            );
        }
        if ok != 0 {
            return Some(val);
        }
        core::hint::spin_loop();
    }
    None
}

/// PCI config read through the ECAM window.
pub fn pci_cfg_read32(ecam: u64, bus: u8, dev: u8, func: u8, off: u16) -> u32 {
    let a = ecam
        + ((bus as u64) << 20)
        + ((dev as u64) << 15)
        + ((func as u64) << 12)
        + (off as u64 & 0xFFC);
    unsafe { (phys_to_virt(a as usize) as *const u32).read_volatile() }
}

/// PCI config write through the ECAM window.
pub fn pci_cfg_write32(ecam: u64, bus: u8, dev: u8, func: u8, off: u16, val: u32) {
    let a = ecam
        + ((bus as u64) << 20)
        + ((dev as u64) << 15)
        + ((func as u64) << 12)
        + (off as u64 & 0xFFC);
    unsafe { (phys_to_virt(a as usize) as *mut u32).write_volatile(val) }
}

// -------------------------------------------------------------- user mode

/// Saved EL0 register state. Layout matches the offsets in vectors.S:
/// x0..x30, then SP_EL0, ELR_EL1, SPSR_EL1, the kernel sp, and TPIDR_EL0 (the
/// EL0 thread pointer, saved/restored per context so each thread of a cell
/// keeps its own TLS; docs/LINUX-COMPAT.md L4).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct TrapFrame {
    regs: [u64; 31],
    sp_el0: u64,
    elr: u64,
    spsr: u64,
    kernel_sp: u64,
    tpidr_el0: u64,
}

const REG_X0: usize = 0; // first argument / return value
const REG_X8: usize = 8; // syscall number

pub fn trapframe_new(entry: usize, user_sp: usize, arg: usize, kernel_sp: usize) -> TrapFrame {
    let mut regs = [0u64; 31];
    regs[REG_X0] = arg as u64;
    TrapFrame {
        regs,
        sp_el0: user_sp as u64,
        elr: entry as u64,
        spsr: 0, // EL0t, interrupts unmasked (none are enabled)
        kernel_sp: kernel_sp as u64,
        tpidr_el0: 0,
    }
}

/// A zeroed frame, for static per-context storage (docs/LINUX-COMPAT.md L4).
pub const fn trapframe_zeroed() -> TrapFrame {
    TrapFrame {
        regs: [0; 31],
        sp_el0: 0,
        elr: 0,
        spsr: 0,
        kernel_sp: 0,
        tpidr_el0: 0,
    }
}

/// Build a thread child's frame from the cloning parent's (docs/LINUX-COMPAT.md
/// L4): same code/return point (`elr`, past the parent's `svc`) and kernel
/// stack, a new user stack, `x0 = 0` so `clone` returns 0 in the child, and the
/// child's TLS in TPIDR_EL0 (restored on resume by the vector trampoline).
pub fn clone_child_frame(parent: &TrapFrame, child_sp: u64, tls: u64) -> TrapFrame {
    let mut f = *parent;
    f.regs[REG_X0] = 0;
    f.sp_el0 = child_sp;
    f.tpidr_el0 = tls;
    f
}

/// Save the live EL0 FP/SIMD state (V0-V31 + FPSR + FPCR) into `area`, for a
/// cooperative context switch between two threads of one cell
/// (docs/LINUX-COMPAT.md L4). FP is enabled at EL1 (CPACR_EL1.FPEN) and the
/// kernel is soft-float, so the registers still hold the trapped thread's
/// values.
///
/// # Safety
/// `area` must point to at least 528 writable, 16-byte-aligned bytes.
pub unsafe fn save_user_fp(area: *mut u8) {
    unsafe {
        asm!(
            // The kernel builds soft-float (no fp-armv8 feature), but FP/SIMD
            // is enabled in hardware (CPACR_EL1.FPEN); enable the instructions
            // for the assembler over just this block to save the user V-regs.
            ".arch armv8-a+fp+simd",
            "stp q0, q1, [{b}, #0]", "stp q2, q3, [{b}, #32]",
            "stp q4, q5, [{b}, #64]", "stp q6, q7, [{b}, #96]",
            "stp q8, q9, [{b}, #128]", "stp q10, q11, [{b}, #160]",
            "stp q12, q13, [{b}, #192]", "stp q14, q15, [{b}, #224]",
            "stp q16, q17, [{b}, #256]", "stp q18, q19, [{b}, #288]",
            "stp q20, q21, [{b}, #320]", "stp q22, q23, [{b}, #352]",
            "stp q24, q25, [{b}, #384]", "stp q26, q27, [{b}, #416]",
            "stp q28, q29, [{b}, #448]", "stp q30, q31, [{b}, #480]",
            "mrs {t}, fpcr", "str {t}, [{b}, #512]",
            "mrs {t}, fpsr", "str {t}, [{b}, #520]",
            b = in(reg) area, t = out(reg) _, options(nostack),
        );
    }
}

/// Restore EL0 FP/SIMD state saved by [`save_user_fp`].
///
/// # Safety
/// `area` must point to a valid 528-byte image written by `save_user_fp`.
pub unsafe fn restore_user_fp(area: *const u8) {
    unsafe {
        asm!(
            // See `save_user_fp`: enable FP/SIMD for the assembler here.
            ".arch armv8-a+fp+simd",
            "ldp q0, q1, [{b}, #0]", "ldp q2, q3, [{b}, #32]",
            "ldp q4, q5, [{b}, #64]", "ldp q6, q7, [{b}, #96]",
            "ldp q8, q9, [{b}, #128]", "ldp q10, q11, [{b}, #160]",
            "ldp q12, q13, [{b}, #192]", "ldp q14, q15, [{b}, #224]",
            "ldp q16, q17, [{b}, #256]", "ldp q18, q19, [{b}, #288]",
            "ldp q20, q21, [{b}, #320]", "ldp q22, q23, [{b}, #352]",
            "ldp q24, q25, [{b}, #384]", "ldp q26, q27, [{b}, #416]",
            "ldp q28, q29, [{b}, #448]", "ldp q30, q31, [{b}, #480]",
            "ldr {t}, [{b}, #512]", "msr fpcr, {t}",
            "ldr {t}, [{b}, #520]", "msr fpsr, {t}",
            b = in(reg) area, t = out(reg) _, options(nostack, readonly),
        );
    }
}

/// (syscall number in x8, arguments a0..a5 = x0..x5).
pub fn decode_syscall(frame: &TrapFrame) -> (u64, [u64; 6]) {
    (
        frame.regs[REG_X8],
        [
            frame.regs[REG_X0],
            frame.regs[REG_X0 + 1],
            frame.regs[REG_X0 + 2],
            frame.regs[REG_X0 + 3],
            frame.regs[REG_X0 + 4],
            frame.regs[REG_X0 + 5],
        ],
    )
}

pub fn set_syscall_ret(frame: &mut TrapFrame, value: u64) {
    frame.regs[REG_X0] = value;
}

/// x86-only `arch_prctl` TLS hook (docs/LINUX-COMPAT.md L1). Unreachable on
/// ARM64: the asm-generic syscall table has no `arch_prctl` number, and EL0
/// sets its own thread pointer with `msr tpidr_el0` (the kernel only uses
/// TPIDR_EL1/TPIDRRO_EL0), so glibc never asks the kernel. Present only so
/// the portable personality dispatch compiles on every ISA.
pub fn set_user_fs_base(_addr: u64) {}
pub fn user_fs_base() -> u64 {
    0
}

unsafe extern "C" {
    pub fn enter_user_first(frame: *mut TrapFrame);
    fn return_to_kernel_asm() -> !;
}

pub fn return_to_kernel() -> ! {
    // SAFETY: only called while a cell is running (inside enter_user_first).
    unsafe { return_to_kernel_asm() }
}

/// Called from vectors.S on every EL0 trap. SVC (ELR already points past
/// the instruction) is a syscall; any other exception class is a fault.
#[unsafe(no_mangle)]
extern "C" fn aarch64_user_trap(esr: u64, far: u64, frame: *mut TrapFrame) -> *mut TrapFrame {
    let kind = if (esr >> 26) & 0x3F == EC_SVC64 {
        super::TrapKind::Syscall
    } else {
        super::TrapKind::Fault
    };
    let resume = crate::user::on_user_trap(kind, far as usize, frame);
    if resume.is_null() {
        return_to_kernel();
    }
    resume
}

// -------------------------------------------------------------- counters

pub fn cycles() -> u64 {
    let value: u64;
    // isb keeps cntvct from being reordered around the measured code.
    unsafe { asm!("isb", "mrs {0}, cntvct_el0", out(reg) value) };
    value
}

/// Convert `cycles()` (virtual counter ticks) to nanoseconds for the Linux
/// personality's `clock_gettime` (docs/LINUX-COMPAT.md L2). CNTFRQ_EL0 gives
/// the counter frequency in Hz.
pub fn ticks_to_ns(ticks: u64) -> u64 {
    let freq: u64;
    // SAFETY: reading the frequency system register (always accessible).
    unsafe { asm!("mrs {0}, cntfrq_el0", out(reg) freq) };
    if freq == 0 {
        return ticks;
    }
    ((ticks as u128 * 1_000_000_000) / freq as u128) as u64
}

/// Calibration loop with a known instruction count: exactly 2
/// instructions per iteration (subs + b.ne). Benchmarks use it to convert
/// counter ticks into approximate instruction counts under QEMU -icount.
pub fn spin_loop(iters: u64) {
    if iters == 0 {
        return;
    }
    let mut n = iters;
    unsafe {
        asm!(
            "2:",
            "subs {0}, {0}, #1",
            "b.ne 2b",
            inout(reg) n,
            options(nomem, nostack),
        )
    };
    let _ = n;
}

// -------------------------------------------------------- context switch

unsafe extern "C" {
    fn context_switch_asm(old_sp: *mut usize, new_sp: *const usize);
}

/// Switch from the current context (saved into `old`) to `new`.
///
/// # Safety
/// `new` must have been produced by `context_init` or a prior switch, and
/// its stack must still be alive.
pub unsafe fn context_switch(old: &mut super::Context, new: &super::Context) {
    unsafe { context_switch_asm(&mut old.sp, &new.sp) };
}

/// Prime a fresh stack so the first switch into it enters `entry`.
/// Frame layout must match context_switch.S: x19-x28, x29, then x30
/// (the return address), 96 bytes total.
///
/// # Safety
/// `stack_top` must be the 16-aligned top of a stack of adequate size.
pub unsafe fn context_init(stack_top: *mut u8, entry: extern "C" fn() -> !) -> super::Context {
    unsafe {
        let sp = stack_top.sub(96) as *mut u64;
        for i in 0..11 {
            sp.add(i).write(0); // x19..x28, x29
        }
        sp.add(11).write(entry as usize as u64); // x30: return address
        super::Context { sp: sp as usize }
    }
}

// ------------------------------------------------------------------ exit

/// Semihosting SYS_EXIT (DEVELOPMENT.md 6). QEMU must run with
/// `-semihosting-config enable=on,target=native`; it exits with our code.
pub fn exit(code: super::ExitCode) -> ! {
    const ADP_STOPPED_APPLICATION_EXIT: u64 = 0x20026;
    let status: u64 = match code {
        super::ExitCode::Success => 0,
        super::ExitCode::Failure => 1,
    };
    let block: [u64; 2] = [ADP_STOPPED_APPLICATION_EXIT, status];
    unsafe {
        asm!(
            "hlt #0xF000",
            in("w0") 0x18u32, // SYS_EXIT
            in("x1") block.as_ptr(),
        );
    }
    // Only reached without semihosting (e.g. interactive run).
    loop {
        unsafe { asm!("wfe") };
    }
}
