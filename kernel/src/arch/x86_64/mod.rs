//! x86-64: PVH boot entry, 16550 UART on port 0x3F8, isa-debug-exit,
//! IDT-based traps, rdtsc, and the context-switch stub.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

mod paging;
pub use paging::{
    PagingRoot, paging_activate, paging_activate_kernel, paging_kernel_init, paging_map,
    paging_new_root,
};

global_asm!(
    include_str!("../../../arch/x86_64/boot.S"),
    options(att_syntax)
);
global_asm!(
    include_str!("../../../arch/x86_64/vectors.S"),
    options(att_syntax)
);
global_asm!(
    include_str!("../../../arch/x86_64/context_switch.S"),
    options(att_syntax)
);
global_asm!(
    include_str!("../../../arch/x86_64/user.S"),
    options(att_syntax)
);

pub const NAME: &str = "x86-64";

/// Physical base of the frame pool: 64 MiB, above the kernel image and
/// within the low-1 GiB identity map (checked in frames::init).
pub const FRAME_POOL_BASE: usize = 0x0400_0000;

// ---------------------------------------------------------------- serial

const COM1: u16 = 0x3F8;

unsafe fn outb(port: u16, value: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") value) };
}

unsafe fn outl(port: u16, value: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") value) };
}

unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { asm!("in al, dx", in("dx") port, out("al") value) };
    value
}

unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe { asm!("in eax, dx", in("dx") port, out("eax") value) };
    value
}

pub fn serial_init() {
    unsafe {
        outb(COM1 + 1, 0x00); // no interrupts
        outb(COM1 + 3, 0x80); // DLAB on
        outb(COM1, 0x01); // divisor 1 = 115200 baud
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8N1, DLAB off
        outb(COM1 + 2, 0xC7); // FIFO on, cleared
    }
}

pub fn serial_write_byte(byte: u8) {
    unsafe {
        // Wait for the transmit holding register to empty (LSR bit 5).
        while inb(COM1 + 5) & 0x20 == 0 {}
        outb(COM1, byte);
    }
}

/// Non-blocking read of one byte from COM1, or None if none pending
/// (LSR bit 0 = data ready).
pub fn serial_read_byte() -> Option<u8> {
    unsafe {
        if inb(COM1 + 5) & 0x01 == 0 {
            None
        } else {
            Some(inb(COM1))
        }
    }
}

// ----------------------------------------------------------------- traps

/// One 16-byte interrupt gate. Layout per the Intel SDM.
#[repr(C)]
#[derive(Copy, Clone)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist_and_flags: u16,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

const IDT_ENTRIES: usize = 32; // CPU exceptions only, for now

static mut IDT: [IdtEntry; IDT_ENTRIES] = [IdtEntry {
    offset_low: 0,
    selector: 0,
    ist_and_flags: 0,
    offset_mid: 0,
    offset_high: 0,
    reserved: 0,
}; IDT_ENTRIES];

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

unsafe extern "C" {
    // Table of the 32 stub addresses, emitted by vectors.S.
    static VECTOR_STUBS: [u64; IDT_ENTRIES];
}

pub fn trap_init() {
    unsafe {
        let idt = &mut *core::ptr::addr_of_mut!(IDT);
        for (i, entry) in idt.iter_mut().enumerate() {
            let handler = VECTOR_STUBS[i];
            *entry = IdtEntry {
                offset_low: handler as u16,
                selector: 0x08,        // boot GDT 64-bit code segment
                ist_and_flags: 0x8E00, // present, interrupt gate, DPL 0
                offset_mid: (handler >> 16) as u16,
                offset_high: (handler >> 32) as u32,
                reserved: 0,
            };
        }
        let pointer = IdtPointer {
            limit: (core::mem::size_of::<IdtEntry>() * IDT_ENTRIES - 1) as u16,
            base: core::ptr::addr_of!(IDT) as u64,
        };
        asm!("lidt [{}]", in(reg) &pointer);
    }
}

static DOORBELLS: AtomicU64 = AtomicU64::new(0);

/// Called from the common stub in vectors.S with the interrupt-frame CS so
/// the RPL distinguishes a kernel trap from a user fault. Vector 3
/// (breakpoint) is the in-kernel doorbell self-test and returns; a fault
/// from ring 3 is recorded and unwinds; anything else is fatal.
#[unsafe(no_mangle)]
extern "C" fn x86_trap_handler(vector: u64, error_code: u64, rip: u64, cs: u64) {
    if vector == 3 {
        DOORBELLS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if cs & 3 == 3 {
        // Fault from ring 3: the faulting address for a #PF is in CR2.
        let cr2: u64;
        unsafe { asm!("mov {0}, cr2", out(reg) cr2) };
        let addr = if vector == 14 {
            cr2 as usize
        } else {
            rip as usize
        };
        let _ = crate::user::on_user_trap(super::TrapKind::Fault, addr, core::ptr::null_mut());
        return_to_kernel();
    }
    crate::println!("TRAP: vector {vector} error {error_code:#x} at rip {rip:#x}");
    exit(super::ExitCode::Failure);
}

/// One kernel-entry round trip via int3 (the doorbell measurement floor).
pub fn doorbell_trap() {
    unsafe { asm!("int3") };
}

pub fn doorbell_count() -> u64 {
    DOORBELLS.load(Ordering::Relaxed)
}

// ----------------------------------------------------- hardware discovery

unsafe extern "C" {
    static BOOT_INFO: u64;
}

/// The PVH hvm_start_info pointer QEMU passed in ebx.
pub fn boot_firmware_ptr() -> usize {
    unsafe { core::ptr::addr_of!(BOOT_INFO).read() as usize }
}

/// Discover the machine via ACPI (RSDP handed over by the PVH start info).
pub fn discover(inv: &mut crate::hw::Inventory) {
    inv.firmware = crate::hw::Firmware::Acpi;
    crate::hw::acpi::parse(boot_firmware_ptr(), inv);
}

/// Feature names; bit i corresponds to index i in CpuReport.features.
pub fn cpu_feature_names() -> &'static [&'static str] {
    &[
        "sse", "sse2", "sse3", "ssse3", "sse4.1", "sse4.2", "avx", "avx2", "avx512f", "aes", "sha",
        "rdrand", "rdseed", "xsave", "fsgsbase", "nx", "pcid", "pdpe1gb", "x2apic",
    ]
}

/// Decode CPU vendor + features via CPUID.
pub fn cpu_report(_inv: &crate::hw::Inventory) -> crate::hw::CpuReport {
    use core::arch::x86_64::__cpuid_count;
    let mut report = crate::hw::CpuReport::EMPTY;
    let v = __cpuid_count(0, 0);
    // Vendor string is ebx, edx, ecx (12 bytes).
    report.vendor[0..4].copy_from_slice(&v.ebx.to_le_bytes());
    report.vendor[4..8].copy_from_slice(&v.edx.to_le_bytes());
    report.vendor[8..12].copy_from_slice(&v.ecx.to_le_bytes());

    let l1 = __cpuid_count(1, 0);
    let l7 = __cpuid_count(7, 0);
    let le = __cpuid_count(0x8000_0001, 0);

    let mut set = |bit: u32, on: bool| {
        if on {
            report.features |= 1 << bit;
        }
    };
    set(0, l1.edx & (1 << 25) != 0); // sse
    set(1, l1.edx & (1 << 26) != 0); // sse2
    set(2, l1.ecx & (1 << 0) != 0); // sse3
    set(3, l1.ecx & (1 << 9) != 0); // ssse3
    set(4, l1.ecx & (1 << 19) != 0); // sse4.1
    set(5, l1.ecx & (1 << 20) != 0); // sse4.2
    set(6, l1.ecx & (1 << 28) != 0); // avx
    set(7, l7.ebx & (1 << 5) != 0); // avx2
    set(8, l7.ebx & (1 << 16) != 0); // avx512f
    set(9, l1.ecx & (1 << 25) != 0); // aes
    set(10, l7.ebx & (1 << 29) != 0); // sha
    set(11, l1.ecx & (1 << 30) != 0); // rdrand
    set(12, l7.ebx & (1 << 18) != 0); // rdseed
    set(13, l1.ecx & (1 << 26) != 0); // xsave
    set(14, l7.ebx & (1 << 0) != 0); // fsgsbase
    set(15, le.edx & (1 << 20) != 0); // nx
    set(16, l1.ecx & (1 << 17) != 0); // pcid
    set(17, le.edx & (1 << 26) != 0); // 1 GiB pages
    set(18, l1.ecx & (1 << 21) != 0); // x2apic
    report
}

/// PCI config read via the CF8/CFC I/O ports (mechanism #1). The ECAM base
/// is unused on x86 - the ports reach bus 0 without any MMIO mapping.
pub fn pci_cfg_read32(_ecam: u64, bus: u8, dev: u8, func: u8, off: u16) -> u32 {
    let addr = 0x8000_0000u32
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | (off as u32 & 0xFC);
    unsafe {
        outl(0xCF8, addr);
        inl(0xCFC)
    }
}

// -------------------------------------------------------------- user mode

/// Saved ring-3 register state. Layout matches the offsets in user.S.
#[repr(C)]
pub struct TrapFrame {
    rax: u64,
    rbx: u64,
    rcx: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    rbp: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rip: u64,
    rflags: u64,
    rsp: u64,
    kernel_sp: u64,
    _pad: u64,
}

pub fn trapframe_new(entry: usize, user_sp: usize, arg: usize, kernel_sp: usize) -> TrapFrame {
    TrapFrame {
        rax: 0,
        rbx: 0,
        rcx: 0,
        rdx: 0,
        rsi: 0,
        rdi: arg as u64, // first argument
        rbp: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
        rip: entry as u64,
        rflags: 0x202, // IF set, reserved bit 1
        rsp: user_sp as u64,
        kernel_sp: kernel_sp as u64,
        _pad: 0,
    }
}

pub fn decode_syscall(frame: &TrapFrame) -> (u64, u64) {
    (frame.rax, frame.rdi) // number in rax, argument in rdi
}

pub fn set_syscall_ret(frame: &mut TrapFrame, value: u64) {
    frame.rax = value;
}

unsafe extern "C" {
    pub fn enter_user_first(frame: *mut TrapFrame);
    fn return_to_kernel_asm() -> !;
}

pub fn return_to_kernel() -> ! {
    // SAFETY: only called while a cell is running (inside enter_user_first).
    unsafe { return_to_kernel_asm() }
}

/// Called from user.S on a SYSCALL. Returns the frame to resume, or null
/// to unwind (the stub jumps to return_to_kernel_asm on null).
#[unsafe(no_mangle)]
extern "C" fn x86_user_trap(kind: u64, fault_addr: u64, frame: *mut TrapFrame) -> *mut TrapFrame {
    let k = if kind == 0 {
        super::TrapKind::Syscall
    } else {
        super::TrapKind::Fault
    };
    crate::user::on_user_trap(k, fault_addr as usize, frame)
}

// Single CPU: the syscall stub reaches these as plain globals, no GS.
#[unsafe(no_mangle)]
static mut KERNEL_RSP: u64 = 0;
#[unsafe(no_mangle)]
static mut USER_RSP_SCRATCH: u64 = 0;
#[unsafe(no_mangle)]
static mut CUR_FRAME: u64 = 0;
#[unsafe(no_mangle)]
static mut KERNEL_CTX: u64 = 0;

// ---------------------------------------------------------- GDT/TSS/MSRs

#[repr(C, packed)]
struct Tss {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

static mut TSS: Tss = Tss {
    reserved0: 0,
    rsp: [0; 3],
    reserved1: 0,
    ist: [0; 7],
    reserved2: 0,
    reserved3: 0,
    iomap_base: 0,
};

// GDT: null, kernel code64, kernel data, user data, user code64, TSS (2).
static mut GDT: [u64; 7] = [0; 7];

#[repr(C, packed)]
struct DescPtr {
    limit: u16,
    base: u64,
}

static mut SYSCALL_KSTACK: [u8; 64 * 1024] = [0; 64 * 1024];

/// Set up ring 3: a full GDT with user segments and a TSS, then the
/// SYSCALL/SYSRET MSRs. Called from paging_kernel_init.
pub(super) fn user_init() {
    unsafe {
        let kstack_top = core::ptr::addr_of!(SYSCALL_KSTACK) as u64 + (64 * 1024);
        *core::ptr::addr_of_mut!(KERNEL_RSP) = kstack_top;

        let tss = &mut *core::ptr::addr_of_mut!(TSS);
        tss.rsp[0] = kstack_top; // ring 3 -> ring 0 fault stack
        tss.iomap_base = core::mem::size_of::<Tss>() as u16;

        let gdt = &mut *core::ptr::addr_of_mut!(GDT);
        gdt[0] = 0;
        gdt[1] = 0x00AF_9A00_0000_FFFF; // 0x08 kernel code64
        gdt[2] = 0x00CF_9200_0000_FFFF; // 0x10 kernel data
        gdt[3] = 0x00CF_F200_0000_FFFF; // 0x18 user data (DPL3)
        gdt[4] = 0x00AF_FA00_0000_FFFF; // 0x20 user code64 (DPL3)
        let tss_base = core::ptr::addr_of!(TSS) as u64;
        let tss_limit = (core::mem::size_of::<Tss>() - 1) as u64;
        gdt[5] = tss_limit
            | ((tss_base & 0xFF_FFFF) << 16)
            | (0x89u64 << 40)
            | (((tss_limit >> 16) & 0xF) << 48)
            | (((tss_base >> 24) & 0xFF) << 56);
        gdt[6] = tss_base >> 32;

        let gdt_ptr = DescPtr {
            limit: (core::mem::size_of::<[u64; 7]>() - 1) as u16,
            base: core::ptr::addr_of!(GDT) as u64,
        };
        // Load the GDT, reload CS via a far return, set the data segments,
        // and load the task register.
        asm!(
            "lgdt [{ptr}]",
            "push 0x08",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ax, 0x10",
            "mov ds, ax",
            "mov es, ax",
            "mov ss, ax",
            "mov ax, 0x28",
            "ltr ax",
            ptr = in(reg) &gdt_ptr,
            tmp = out(reg) _,
            out("ax") _,
        );

        // EFER.SCE (enable SYSCALL); NXE was set in paging_kernel_init.
        let efer = paging_rdmsr(0xC000_0080) | 1;
        paging_wrmsr(0xC000_0080, efer);
        // STAR: SYSCALL loads CS=0x08/SS=0x10; SYSRET base 0x10 -> user
        // SS=0x18, CS=0x20.
        paging_wrmsr(0xC000_0081, (0x10u64 << 48) | (0x08u64 << 32));
        // LSTAR: the SYSCALL entry point.
        paging_wrmsr(0xC000_0082, syscall_entry as *const () as u64);
        // SFMASK: clear IF and DF on entry.
        paging_wrmsr(0xC000_0084, 0x600);
    }
}

unsafe extern "C" {
    fn syscall_entry();
}

unsafe fn paging_rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nostack));
    }
    ((hi as u64) << 32) | lo as u64
}

unsafe fn paging_wrmsr(msr: u32, value: u64) {
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack),
        );
    }
}

// -------------------------------------------------------------- counters

pub fn cycles() -> u64 {
    let lo: u32;
    let hi: u32;
    // lfence keeps rdtsc from being reordered around the measured code.
    unsafe { asm!("lfence", "rdtsc", out("eax") lo, out("edx") hi) };
    ((hi as u64) << 32) | lo as u64
}

/// Calibration loop with a known instruction count: exactly 2
/// instructions per iteration (dec + jnz). Benchmarks use it to convert
/// counter ticks into approximate instruction counts under QEMU -icount.
pub fn spin_loop(iters: u64) {
    if iters == 0 {
        return;
    }
    let mut n = iters;
    unsafe {
        asm!(
            "2:",
            "dec {0}",
            "jnz 2b",
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
/// Frame layout must match context_switch.S: 6 callee-saved registers,
/// then the return address; an extra 8 bytes keeps the SysV entry
/// alignment (rsp % 16 == 8 at function entry).
///
/// # Safety
/// `stack_top` must be the 16-aligned top of a stack of adequate size.
pub unsafe fn context_init(stack_top: *mut u8, entry: extern "C" fn() -> !) -> super::Context {
    unsafe {
        let sp = stack_top.sub(64) as *mut u64;
        for i in 0..6 {
            sp.add(i).write(0); // r15, r14, r13, r12, rbx, rbp
        }
        sp.add(6).write(entry as usize as u64); // return address
        super::Context { sp: sp as usize }
    }
}

// ------------------------------------------------------------------ exit

/// isa-debug-exit at port 0xF4: QEMU exits with (value << 1) | 1,
/// so Success -> 33 and Failure -> 35. The xtask harness knows these.
pub fn exit(code: super::ExitCode) -> ! {
    let value: u32 = match code {
        super::ExitCode::Success => 0x10,
        super::ExitCode::Failure => 0x11,
    };
    unsafe {
        outl(0xF4, value);
    }
    // Only reached without the exit device (e.g. interactive run).
    loop {
        unsafe { asm!("hlt") };
    }
}
