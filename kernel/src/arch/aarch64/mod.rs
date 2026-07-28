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
    PagingRoot, paging_activate, paging_activate_kernel, paging_cow_at, paging_cow_clear,
    paging_cow_protect_user, paging_for_each_user_leaf, paging_kernel_init, paging_map,
    paging_map_frame, paging_mapped, paging_new_root, paging_protect, paging_unmap_frame,
    paging_unmapped_span,
};
pub use paging::{mmio_map_window, pmem_map_window};

/// `uname` machine string for the Linux personality (docs/LINUX-COMPAT.md L2).
pub const LINUX_UNAME_MACHINE: &str = "aarch64";

/// clone(2) argument order (docs/LINUX-COMPAT.md L4): ARM64 selects
/// `CLONE_BACKWARDS`, so the raw order is `(flags, stack, parent_tid, tls,
/// child_tid)` - `tls` and `child_tid` are swapped relative to x86-64.
pub const CLONE_BACKWARDS: bool = true;

global_asm!(include_str!("../../../arch/aarch64/boot.S"));
global_asm!(include_str!("../../../arch/aarch64/vectors.S"));
global_asm!(include_str!("../../../arch/aarch64/context_switch.S"));
// PSCI is the platform's firmware interface and every build needs it - the CPU
// count comes from `AFFINITY_INFO`. Only the secondary trampoline is SMP-only.
global_asm!(include_str!("../../../arch/aarch64/psci.S"));
#[cfg(feature = "smp")]
global_asm!(include_str!("../../../arch/aarch64/smp.S"));

pub const NAME: &str = "ARM64";

/// Physical base of the frame pool: 64 MiB into RAM, above the kernel
/// image (checked against __kernel_end in frames::init).
pub const FRAME_POOL_BASE: usize = 0x4400_0000;

/// Exclusive top of this ISA's **user** virtual address range
/// (docs/SUBSTRATE.md pillar 2).
///
/// The boot trampoline programs `TCR_EL1.T0SZ = 16` (arch/aarch64/boot.S), so
/// TTBR0 - the low half, which is entirely user - translates a 48-bit VA:
/// `[0, 2^48)`. 256 TiB, the widest of the three ISAs.
///
/// The portable code above `arch` reads this rather than a single shared bound;
/// see the x86-64 counterpart for why one shared `2^38` was the wrong shape.
pub const USER_VA_TOP: usize = 1 << 48;

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

// ----------------------------------- console input + timer interrupt seam
// docs/LIBRHEO.md Phase D/F: the kernel's UART RX + timer interrupts on ARM64,
// both through **GICv3** (QEMU `virt`, gic-version=3: GICD @ 0x0800_0000, the
// boot CPU's GICR @ 0x080A_0000). The CPU interface is reached via system
// registers (ICC_*_EL1). Two sources: the **PL011 UART** (SPI 1 = INTID 33, via
// the distributor) for RX, and the **CNTV virtual timer** (PPI 11 = INTID 27,
// CPU-local via the redistributor) for `SYS_ARM_TIMER`. Interrupts fire only in
// kernel context: cells run at EL0 with IRQ masked (their TrapFrame SPSR sets I),
// and the kernel takes an IRQ only inside the `wfi` idle path (`daifclr`), so
// both `SYS_WAIT_INPUT` and `SYS_ARM_TIMER` are genuine 0%-CPU parks. Opt-in
// (`enable_uart_rx_irq` / `enable_timer_irq`, called only by the Phase D/F
// tests), so no other kernel is affected.
//
// *QEMU caveat, documented for honesty* (mirrors the RISC-V APLIC workaround):
// QEMU's PL011 loopback does not deliver to the RX FIFO, so the deterministic
// test cannot push a scripted byte through the device. Instead the byte is
// carried in a kernel static and the PL011's SPI (33) is raised directly via
// GICD_ISPENDR - the exact interrupt the PL011 would raise on receive - and the
// handler pushes the carried byte to the ring. The GIC delivery, the `wfi` idle,
// and the wakeup are all genuine; a live keystroke takes the full PL011->GIC path.

const GICD_BASE: usize = 0x0800_0000 | KERNEL_VA_BASE;
const GICR_BASE: usize = 0x080A_0000 | KERNEL_VA_BASE; // CPU 0's redistributor
/// Bytes between one CPU's redistributor and the next on QEMU `virt` (GICv3, no
/// VLPI): two 64 KiB frames - RD_base then SGI_base.
const GICR_STRIDE: usize = 0x2_0000;
/// This CPU's SGI/PPI frame. A redistributor is **per core**, so a secondary that
/// wrote CPU 0's would enable a PPI on the wrong core and leave its own masked -
/// which looks exactly like "the timer interrupt is not delivered" (docs/SMP.md
/// 10.0). Indexed by MPIDR affinity 0, the same value `cpu_index` reads.
fn gicr_base_this_cpu() -> usize {
    GICR_BASE + (mpidr_aff0() as usize) * GICR_STRIDE
}
fn gicr_sgi_base_this_cpu() -> usize {
    gicr_base_this_cpu() + 0x1_0000
}
const UART_INTID: u32 = 33; // PL011 SPI 1
const TIMER_INTID: u32 = 27; // CNTV PPI 11

static mut IRQ_ENABLED: bool = false;
static mut TIMER_ENABLED: bool = false;
static mut GIC_UP: bool = false;
/// A scripted byte awaiting delivery through the UART RX interrupt (the Phase D
/// test path). QEMU's PL011 loopback does not deliver to the RX FIFO, so the
/// byte is carried here and pushed to the ring by the IRQ handler after a genuine
/// GIC SPI interrupt (raised via GICD_ISPENDR) wakes `wfi` - the interrupt the
/// PL011 would raise. 0x100+ = empty; low byte = the pending byte.
static mut INJECT: u32 = 0;

/// Whether the UART RX interrupt is wired (false = poll path).
pub fn uart_irq_enabled() -> bool {
    // SAFETY: single CPU; set once before any cell runs.
    unsafe { *core::ptr::addr_of!(IRQ_ENABLED) }
}

/// Whether the CNTV timer interrupt is wired (false = busy-wait path).
pub fn timer_irq_enabled() -> bool {
    // SAFETY: single CPU; set once before any cell runs.
    unsafe { *core::ptr::addr_of!(TIMER_ENABLED) }
}

#[inline(always)]
fn mmio_w32(addr: usize, val: u32) {
    // SAFETY: `addr` is a mapped GIC MMIO register (device gigabyte, TTBR1).
    unsafe { (addr as *mut u32).write_volatile(val) };
}
#[inline(always)]
fn mmio_r32(addr: usize) -> u32 {
    // SAFETY: `addr` is a mapped GIC MMIO register.
    unsafe { (addr as *const u32).read_volatile() }
}

/// Bring up the GICv3 distributor + this CPU's redistributor + the CPU interface
/// (system registers). Idempotent; shared by the UART and timer paths.
fn gic_init() {
    // SAFETY: kernel context; GIC MMIO + ICC system registers.
    unsafe {
        // The **distributor** is one shared block, so it is brought up once. The
        // redistributor and the CPU interface below are **per core** and are done
        // every time: a secondary calling this must wake its own redistributor and
        // program its own ICC_* registers, and a global "already up" flag covering
        // all three would leave it with no CPU interface at all (docs/SMP.md 10.0).
        if !*core::ptr::addr_of!(GIC_UP) {
            // Distributor: affinity routing (ARE) + Group1 enable.
            mmio_w32(GICD_BASE, (1 << 4) | (1 << 1) | 1);
            *core::ptr::addr_of_mut!(GIC_UP) = true;
        }
        // Redistributor: wake **this** CPU (clear GICR_WAKER.ProcessorSleep, then
        // wait for ChildrenAsleep to clear). Idempotent on a core already awake.
        let waker = gicr_base_this_cpu() + 0x0014;
        mmio_w32(waker, mmio_r32(waker) & !(1 << 1));
        while mmio_r32(waker) & (1 << 2) != 0 {}
        // CPU interface via system registers: SRE=1, PMR=0xFF, EOImode=0, Grp1 on.
        // All per core.
        asm!("msr S3_0_C12_C12_5, {0}", "isb", in(reg) 1u64); // ICC_SRE_EL1.SRE
        asm!("msr S3_0_C4_C6_0, {0}", in(reg) 0xFFu64); // ICC_PMR_EL1
        asm!("msr S3_0_C12_C12_4, xzr"); // ICC_CTLR_EL1 (EOImode 0)
        asm!("msr S3_0_C12_C12_7, {0}", "isb", in(reg) 1u64); // ICC_IGRPEN1_EL1
    }
}

/// Enable one interrupt in the distributor (SPI, INTID >= 32): Group1, priority,
/// route to the boot CPU (affinity 0), level-triggered, enabled.
fn gicd_enable_spi(intid: u32) {
    let n = (intid / 32) as usize;
    let bit = 1u32 << (intid % 32);
    mmio_w32(
        GICD_BASE + 0x0080 + 4 * n,
        mmio_r32(GICD_BASE + 0x0080 + 4 * n) | bit,
    ); // IGROUPR: group1
    // IPRIORITYR (byte per INTID): 0 = highest, below PMR 0xFF.
    let pri = GICD_BASE + 0x0400 + intid as usize;
    // SAFETY: byte MMIO register.
    unsafe { (pri as *mut u8).write_volatile(0x00) };
    // IROUTER (64-bit per SPI): affinity 0.0.0.0 = boot CPU.
    let router = GICD_BASE + 0x6000 + 8 * intid as usize;
    // SAFETY: 64-bit MMIO register.
    unsafe { (router as *mut u64).write_volatile(0) };
    mmio_w32(GICD_BASE + 0x0100 + 4 * n, bit); // ISENABLER: enable
}

/// Enable one PPI/SGI (INTID < 32) in this CPU's redistributor SGI frame.
fn gicr_enable_ppi(intid: u32) {
    let sgi = gicr_sgi_base_this_cpu();
    let bit = 1u32 << intid;
    mmio_w32(sgi + 0x0080, mmio_r32(sgi + 0x0080) | bit); // IGROUPR0: group1
    let pri = sgi + 0x0400 + intid as usize;
    // SAFETY: byte MMIO register.
    unsafe { (pri as *mut u8).write_volatile(0x00) };
    mmio_w32(sgi + 0x0100, bit); // ISENABLER0: enable
}

// PL011 registers used for RX interrupts.
const PL011_CR: *mut u32 = (PL011_BASE + 0x30) as *mut u32; // control
const PL011_IMSC: *mut u32 = (PL011_BASE + 0x38) as *mut u32; // interrupt mask
const PL011_ICR: *mut u32 = (PL011_BASE + 0x44) as *mut u32; // interrupt clear
// UARTCR bits: UARTEN(0), TXE(8), RXE(9).
const CR_ON: u32 = 1 | (1 << 8) | (1 << 9); // enabled, TX+RX

/// Bring up the PL011 RX interrupt through the GICv3 (SPI 33): the GIC route +
/// enable, then the PL011 RX interrupt mask. Called only by the Phase D test.
pub fn enable_uart_rx_irq() {
    gic_init();
    gicd_enable_spi(UART_INTID);
    // SAFETY: kernel context; PL011 MMIO.
    unsafe {
        // Enable the UART (UARTEN so loopback RX works), then unmask RX (RXIM
        // bit 4) + receive-timeout (RTIM bit 6) so a single byte below the FIFO
        // trigger still raises an interrupt.
        PL011_CR.write_volatile(CR_ON); // UARTEN | TXE | RXE
        PL011_ICR.write_volatile(0x7FF); // clear any stale interrupts
        PL011_IMSC.write_volatile((1 << 4) | (1 << 6)); // RXIM | RTIM
        *core::ptr::addr_of_mut!(IRQ_ENABLED) = true;
    }
}

// ------------------------------------------------- NIC receive interrupt
// docs/NETSTACK.md (rheo-net N2d): the kernel's third interrupt source. QEMU arm
// `virt` gives each of its 32 virtio-mmio transport slots an SPI (hw/arm/virt.c
// irqmap `[VIRT_MMIO] = 16`, so slot i is SPI 16+i = INTID 48+i). The driver
// records the slot it bound to; this enables that SPI in the same GICv3 the UART
// and timer use. Opt-in (called only by the `netwait` test), so no other kernel is
// affected.

/// SPI of virtio-mmio slot 0 on QEMU arm `virt` (irqmap `[VIRT_MMIO] = 16`).
const VIRTIO_MMIO_SPI_BASE: u32 = 16;

static mut NET_IRQ_ENABLED: bool = false;
static mut NET_INTID: u32 = 0;

/// Whether the NIC receive interrupt is wired (false = the kernel's poll
/// fallback, docs/NETSTACK.md).
pub fn net_irq_enabled() -> bool {
    // SAFETY: single CPU; set once before any cell runs.
    unsafe { *core::ptr::addr_of!(NET_IRQ_ENABLED) }
}

/// Whether the NIC's interrupt is already pending in the distributor (so
/// `idle_wait` services it without halting).
pub fn net_irq_pending() -> bool {
    if !net_irq_enabled() {
        return false;
    }
    // SAFETY: single CPU; GICD_ISPENDR is a mapped GIC MMIO register.
    let intid = unsafe { *core::ptr::addr_of!(NET_INTID) };
    let n = (intid / 32) as usize;
    mmio_r32(GICD_BASE + 0x0200 + 4 * n) & (1 << (intid % 32)) != 0
}

/// Bring up the virtio-net RX interrupt for transport `slot` (SPI 16+slot) in the
/// GICv3. Returns whether it is wired. Called only by the `netwait` test path.
pub fn enable_virtio_net_irq(slot: usize) -> bool {
    if slot >= VIRTIO_MMIO_COUNT {
        return false;
    }
    gic_init();
    let intid = 32 + VIRTIO_MMIO_SPI_BASE + slot as u32;
    gicd_enable_spi(intid);
    // SAFETY: single CPU; set once before any cell runs.
    unsafe {
        *core::ptr::addr_of_mut!(NET_INTID) = intid;
        *core::ptr::addr_of_mut!(NET_IRQ_ENABLED) = true;
    }
    true
}

/// Bring up the CNTV virtual timer interrupt (PPI 27). Called only by the Phase F
/// test.
/// The EL1 register bits EL0 relies on, for **this core** - nothing, here.
///
/// The counterpart of RISC-V's `sstatus.SUM`/`FS` block. On ARM64 the equivalents
/// are already per core *and* already set on a secondary: the PSCI entry
/// (`arch/aarch64/smp.S`) writes `CPACR_EL1.FPEN` itself and adopts the primary's
/// `SCTLR_EL1`/`MAIR_EL1`/`TCR_EL1` verbatim, and PAN (the SUM analogue) is off. So
/// this is deliberately empty rather than absent: the portable caller
/// (`smp::secondary_run`) must not have to know which ISAs need it (docs/SMP.md 10.0).
pub fn user_mode_init_this_cpu() {}

pub fn enable_timer_irq() {
    enable_timer_irq_this_cpu();
    // SAFETY: set once by the primary, before any secondary reads it.
    unsafe { *core::ptr::addr_of_mut!(TIMER_ENABLED) = true };
}

/// Bring up **this core's** timer interrupt: its redistributor, its CPU interface,
/// its CNTV PPI. All per core - a secondary that wants a preemption slice must run
/// this for itself, because none of it is set by the PSCI entry (docs/SMP.md 10.0).
pub fn enable_timer_irq_this_cpu() {
    gic_init();
    gicr_enable_ppi(TIMER_INTID);
    // SAFETY: kernel context; disarm the timer until the first `timer_wait`.
    unsafe { asm!("msr cntv_ctl_el0, xzr") };
}

/// The GICv3 IRQ handler (called from the current-EL-SPx IRQ vector slot while
/// the kernel idles at `wfi`): ack via ICC_IAR1_EL1, service the source (drain
/// the PL011 into the ring, or mask the fired timer), then EOI via ICC_EOIR1_EL1.
#[unsafe(no_mangle)]
extern "C" fn aarch64_irq_handler() {
    service_irq();
}

/// Called from vectors.S when a GICv3 interrupt is taken while a **cell** is running
/// at EL0 (docs/SUBSTRATE.md pillar 3, timer preemption).
///
/// Services the interrupt exactly as the EL1 path does, then asks the portable
/// scheduler for the frame to resume. With no preemption pending that is `frame`
/// unchanged, so the cell resumes at the instruction it was interrupted at.
#[unsafe(no_mangle)]
extern "C" fn aarch64_user_irq(frame: *mut TrapFrame) -> *mut TrapFrame {
    if service_irq() == Serviced::Timer {
        // Recorded, not acted on here: the handler is the wrong context to run a
        // scheduler from. The portable hook below decides whether the CPU moves.
        crate::sched::preempt::note();
    }
    crate::user::on_user_interrupt(frame)
}

/// What the interrupt was, so an EL0 entry can tell a preemption slice from a device.
#[derive(PartialEq, Eq)]
enum Serviced {
    Timer,
    Other,
}

/// Acknowledge, service and EOI one GICv3 interrupt. Shared by the EL1 (`wfi` idle)
/// and EL0 (running cell) entries, because the *servicing* is identical - only what
/// the caller does afterwards differs.
fn service_irq() -> Serviced {
    let mut what = Serviced::Other;
    // SAFETY: kernel context; ICC system-register + MMIO access.
    unsafe {
        let intid: u64;
        asm!("mrs {0}, S3_0_C12_C12_0", out(reg) intid); // ICC_IAR1_EL1
        let id = (intid & 0xFF_FFFF) as u32;
        if id == UART_INTID {
            // Deliver a scripted byte (carried in INJECT; see `uart_inject_and_wait`).
            let inj = *core::ptr::addr_of!(INJECT);
            if inj < 0x100 {
                crate::input::rx_push(inj as u8);
                *core::ptr::addr_of_mut!(INJECT) = 0x100;
            }
            // Drain any real bytes the PL011 received (interactive path).
            while let Some(b) = serial_read_byte() {
                crate::input::rx_push(b);
            }
            PL011_ICR.write_volatile((1 << 4) | (1 << 6)); // clear RXIC | RTIC
        } else if id == TIMER_INTID {
            // Mask the timer output so it stops asserting; the arbiter re-arms.
            asm!("msr cntv_ctl_el0, {0}", in(reg) 0b11u64); // ENABLE | IMASK
            what = Serviced::Timer;
        } else if *core::ptr::addr_of!(NET_IRQ_ENABLED) && id == *core::ptr::addr_of!(NET_INTID) {
            // The NIC's receive line (docs/NETSTACK.md, rheo-net N2d): acknowledge
            // the device (its line drops) + record the arrival. The frame stays in
            // the receive virtqueue for the wait path to copy out.
            crate::net_rx::on_irq();
        }
        if id < 1020 {
            asm!("msr S3_0_C12_C12_1, {0}", in(reg) intid); // ICC_EOIR1_EL1
        }
    }
    what
}

/// Halt until an enabled GIC interrupt fires (a genuine 0%-CPU park). `wfi` wakes
/// on a pending interrupt even with IRQ masked; we then briefly unmask (daifclr)
/// so the pending IRQ is taken and serviced by the vector, then re-mask.
pub fn idle_wait() {
    // SAFETY: kernel context; IRQ toggled around a single serviced interrupt.
    unsafe {
        asm!("wfi");
        asm!("msr daifclr, #2"); // unmask IRQ -> pending IRQ taken + serviced here
        asm!("msr daifset, #2"); // mask IRQ again
    }
}

/// Deliver a scripted byte through the real PL011 RX interrupt (loopback),
/// halting at `wfi` until the interrupt is taken - the same path a live keystroke
/// takes. Used by the deterministic Phase D test.
pub fn uart_inject_and_wait(b: u8) {
    // SAFETY: kernel context; a GIC MMIO write + the wfi idle path.
    unsafe {
        // Carry the byte for the handler, then raise the PL011's SPI (33) directly
        // via GICD_ISPENDR - the interrupt the PL011 would raise on receive (QEMU
        // PL011 loopback does not deliver to the RX FIFO). `wfi` halts until the
        // GIC delivers it; the handler (slot 5) pushes the byte to the ring + EOI.
        *core::ptr::addr_of_mut!(INJECT) = b as u32;
        let n = (UART_INTID / 32) as usize;
        mmio_w32(GICD_BASE + 0x0200 + 4 * n, 1 << (UART_INTID % 32)); // GICD_ISPENDR
        asm!("wfi");
        asm!("msr daifclr, #2"); // take + service the pending IRQ (delivers b)
        asm!("msr daifset, #2");
    }
}

/// The CNTVCT deadline the timer is currently armed for (0 = disarmed).
static mut TIMER_TARGET: u64 = 0;

/// Arm the CNTV virtual timer for `deadline_ns` from now, without waiting.
///
/// **Private mechanism of the timer arbiter** (`kernel/src/ktimer.rs`), the
/// kernel's single owner of the one-shot: a subsystem that arms this directly
/// cancels whatever another subsystem armed (docs/NETSTACK.md 16). Register a
/// deadline with the arbiter instead.
pub fn timer_arm(deadline_ns: u64) {
    // SAFETY: kernel context; generic-timer system registers.
    unsafe {
        let freq: u64;
        asm!("mrs {0}, cntfrq_el0", out(reg) freq);
        let now: u64;
        asm!("isb", "mrs {0}, cntvct_el0", out(reg) now);
        let delta = ((deadline_ns as u128 * freq as u128) / 1_000_000_000) as u64;
        let target = now.wrapping_add(delta.max(1));
        asm!("msr cntv_cval_el0, {0}", in(reg) target); // compare value
        asm!("msr cntv_ctl_el0, {0}", "isb", in(reg) 1u64); // ENABLE, IMASK=0
        *core::ptr::addr_of_mut!(TIMER_TARGET) = target;
    }
}

/// Whether the armed deadline has passed.
pub fn timer_expired() -> bool {
    // SAFETY: kernel context; reads the virtual counter + the recorded deadline.
    unsafe {
        let cur: u64;
        asm!("mrs {0}, cntvct_el0", out(reg) cur);
        cur >= *core::ptr::addr_of!(TIMER_TARGET)
    }
}

/// Disarm the CNTV timer (its output stops asserting).
pub fn timer_disarm() {
    // SAFETY: kernel context; generic-timer control register.
    unsafe { asm!("msr cntv_ctl_el0, xzr") };
}

/// Monotonic now in nanoseconds **in the timer's own domain** - the virtual
/// counter at CNTFRQ_EL0, the same domain [`timer_arm`]'s delta is expressed in
/// (here it happens to be the same counter [`cycles`] reads). The timer arbiter
/// (`kernel/src/ktimer.rs`) compares its deadlines against this.
pub fn timer_now_ns() -> u64 {
    // SAFETY: reading the generic-timer counter + frequency (always accessible).
    unsafe {
        let freq: u64;
        asm!("mrs {0}, cntfrq_el0", out(reg) freq);
        let now: u64;
        asm!("isb", "mrs {0}, cntvct_el0", out(reg) now);
        if freq == 0 {
            return now;
        }
        ((now as u128 * 1_000_000_000) / freq as u128) as u64
    }
}

/// Halt the CPU until an enabled interrupt fires - the timer one-shot the arbiter
/// armed, or any other wired source. Called only by `kernel/src/ktimer.rs`, which
/// owns the hardware one-shot (docs/NETSTACK.md 16).
pub fn timer_park() {
    idle_wait(); // wfi, then take + service the pending IRQ (masks the timer)
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

/// Discover the machine. QEMU's arm `virt` hands a bare ELF no firmware table -
/// x0 arrives as 0 and no DTB is placed in guest RAM - so memory and the PCIe
/// ECAM window come from the fixed QEMU virt platform profile (hw/arm/virt.c).
///
/// **The CPU list does not**: it is enumerated from PSCI `AFFINITY_INFO`. See the
/// body for why the earlier deferral was wrong on both of its grounds.
pub fn discover(inv: &mut crate::hw::Inventory) {
    // **The CPU list is asked for, not assumed.**
    //
    // This used to be `add_cpu(0, 0)` - one CPU, always, on a machine that can be
    // booted with any number - and the comment above it explained why enumeration
    // was deferred. The deferral had a cost that showed up as a wrong answer, not
    // as a missing feature: the NVMe driver sized its per-core queue pairs from
    // `ncpus`, so on a four-core ARM64 boot cores 1..3 had no queue of their own,
    // silently shared core 0's, and the same sector read back different bytes on
    // successive reads with no fault and no log (docs/SUBSTRATE.md S5). A field
    // that is constant is a field that lies (docs/ENGINEERING.md 11).
    //
    // Two sources are tried, in order of how much they say:
    //
    // 1. **A device tree**, if one was passed. QEMU hands `virt` a DTB in x0 for a
    //    Linux-image boot and `boot.S` has always kept it in `BOOT_DTB`; nothing
    //    read it. It describes CPUs, memory and PCIe together, so where it exists
    //    it is the better answer. For the bare-ELF `-kernel` boot this tree uses,
    //    QEMU passes **x0 = 0** and places no tree - which is the observation the
    //    old "no firmware table" comment was really recording.
    //
    // 2. **PSCI `AFFINITY_INFO`**, which is what that boot does have. The old note
    //    said PSCI "is not an enumeration API (`CPU_ON` starts a CPU you already
    //    name)" - true of `CPU_ON` and not of `AFFINITY_INFO`, which answers
    //    ON/OFF for an affinity the platform implements and INVALID_PARAMETERS for
    //    one it does not. Asking it about each candidate affinity *is* enumeration.
    //    The other half of the deferral - that it would move the PSCI helper out of
    //    the `smp` cargo feature - was a reason not to, and the wrong way round:
    //    how many CPUs a machine has is a property of the hardware, not of a build
    //    configuration, which is the same reasoning that already keeps
    //    `mpidr_aff0` ungated.
    //
    // SAFETY: `BOOT_DTB` is written once by `boot.S` before any Rust runs.
    let dtb = unsafe { core::ptr::addr_of!(BOOT_DTB).read() } as usize;
    let mut source = crate::hw::Firmware::Builtin;
    if dtb != 0 {
        crate::hw::fdt::parse(phys_to_virt(dtb), inv);
        if inv.ncpus > 0 {
            source = crate::hw::Firmware::DeviceTree;
        }
    }
    if inv.ncpus == 0 {
        let found = psci_enumerate_cpus(inv);
        if found > 0 {
            source = crate::hw::Firmware::Psci;
        }
    }
    if inv.ncpus == 0 {
        // Neither source answered: the machine still has the core this is running
        // on. Reported as `Builtin`, so "asked and was told" stays distinguishable
        // from "assumed".
        inv.add_cpu(mpidr_aff0(), 0);
    }
    inv.firmware = source;

    // The rest of the QEMU `virt` map stays a built-in profile, and deliberately so
    // rather than for want of a parser: each of these is already verified against
    // this machine by a test that drives real hardware through it, while a device
    // tree's answers would be new inputs on paths (the identity-mapped device
    // gigabyte, an SMMU present only with `iommu=smmuv3`) where being wrong is a
    // boot failure rather than a wrong number. They are filled only where discovery
    // left a hole, so a tree that *does* describe them wins.
    if inv.nmem == 0 {
        // QEMU virt: RAM at 0x4000_0000. We map (and therefore report) the
        // first gigabyte; larger -m would need the map extended.
        inv.add_mem(0x4000_0000, 0x4000_0000, crate::hw::MemKind::Ram, 0);
    }
    if inv.ecam_base == 0 {
        // PCIe ECAM low window (QEMU virt with highmem-ecam=off), inside the
        // device gigabyte the kernel identity-maps.
        inv.ecam_base = 0x3f00_0000;
    }
    // The SMMUv3 register base is fixed in the QEMU virt map (docs/GPU-
    // HARDWARE.md 4). It is only physically present when the machine is
    // booted with `iommu=smmuv3` (the `iommu` test does); no other code
    // reads `iommu_base`, and the SMMUv3 driver touches these registers
    // only on that test, so reporting the address here is safe.
    inv.iommu_base = 0x0905_0000;
}

/// Ask PSCI which CPUs this machine has, adding each to `inv`. Returns how many.
///
/// `AFFINITY_INFO(mpidr, 0)` answers ON/OFF/ON_PENDING for an affinity the platform
/// implements and an error for one it does not, so the candidate set can be walked
/// and the answers believed. Candidates are the MPIDR encodings QEMU's `virt` uses:
/// Aff0 within a cluster, Aff1 selecting the cluster. A cluster whose *first* core
/// is absent ends the walk, so a single-core machine costs two refused calls rather
/// than a scan of the whole space.
///
/// Bounded by construction and by `hw::MAX_CPUS`. Returns 0 with nothing added if
/// no conduit answers, which is the honest result on a machine without PSCI - the
/// caller then falls back and says so.
fn psci_enumerate_cpus(inv: &mut crate::hw::Inventory) -> usize {
    let Some(conduit) = psci_conduit() else {
        return 0;
    };
    /// Cores per cluster to probe. QEMU's `virt` packs up to 8 (16 with GICv3)
    /// into Aff0 before moving to Aff1; 16 covers both without assuming which.
    const PER_CLUSTER: u64 = 16;
    /// Clusters to probe. 4 x 16 is far past `MAX_CPUS` and past anything QEMU
    /// models here, and the walk stops at the first empty cluster anyway.
    const CLUSTERS: u64 = 4;

    let mut found = 0usize;
    for cluster in 0..CLUSTERS {
        let mut in_cluster = 0usize;
        for core in 0..PER_CLUSTER {
            if found >= crate::hw::MAX_CPUS {
                return found;
            }
            let mpidr = (cluster << 8) | core;
            // SAFETY: the helper guards the call with its own exception vector, so
            // a conduit that is UNDEFINED at EL1 returns the sentinel rather than
            // faulting the kernel.
            let r = unsafe { psci_call_guarded(conduit, PSCI_AFFINITY_INFO, mpidr, 0, 0) };
            // 0 = ON, 1 = OFF, 2 = ON_PENDING. Anything else (the sentinel, or a
            // negative PSCI error such as INVALID_PARAMETERS) means "no CPU here".
            if r > 2 {
                break;
            }
            inv.add_cpu(mpidr as u32, 0);
            found += 1;
            in_cluster += 1;
        }
        if in_cluster == 0 {
            break; // an empty cluster ends the walk
        }
    }
    found
}

unsafe extern "C" {
    /// The flattened-device-tree pointer QEMU passes in x0, saved by `boot.S`.
    static BOOT_DTB: u64;
}

// ------------------------------------------------------------------- SMP
// docs/SMP.md 7, task #27. **A real secondary core.** The kernel runs at EL1 with
// no EL2/EL3 (QEMU `virt`, secure=off, virtualization=off), and this port used to
// issue PSCI as `smc #0`, which with no EL3 to service it is UNDEFINED at EL1 and
// traps straight back - recorded, correctly as far as it went, as "PSCI is
// unusable here".
//
// That was a conclusion about the *instruction*, not about PSCI. QEMU's `virt`
// implements PSCI itself and chooses the conduit from the machine configuration:
// **HVC** for the plain machine, SMC when `virtualization=on` hands the guest an
// EL2 of its own. The default configuration - the one this tree boots - answers
// PSCI on HVC, the one conduit the port never tried. Bring-up now **probes** with
// PSCI_VERSION over each conduit and uses whichever answers
// (docs/ENGINEERING.md 1), then does a genuine `CPU_ON` into an MMU-on secondary
// trampoline sharing the primary's page tables (`kernel/arch/aarch64/smp.S`).
// Both conduits stay guarded, so a machine that answers on neither reports
// skip-with-reason instead of dying.

/// MPIDR affinity 0 of each CPU, indexed by registry index; `u32::MAX` = unused.
/// Filled by each CPU as it establishes its identity.
#[cfg(feature = "smp")]
static mut MPIDR_OF_CPU: [u32; crate::hw::MAX_CPUS] = [u32::MAX; crate::hw::MAX_CPUS];

/// This CPU's MPIDR_EL1 affinity level 0 - a **read-only hardware id**, so a
/// core's identity comes from the silicon rather than from what the primary told
/// it.
///
/// Not gated on the `smp` feature: the GIC redistributor is indexed by it, and
/// which redistributor a core owns is a property of the hardware, not of a build
/// configuration. On a single-CPU boot it reads 0 and every use below is the
/// pre-existing constant exactly.
fn mpidr_aff0() -> u32 {
    let mpidr: u64;
    // SAFETY: reads MPIDR_EL1, a read-only id register available at EL1.
    unsafe { asm!("mrs {0}, mpidr_el1", out(reg) mpidr, options(nomem, nostack)) };
    (mpidr & 0xFF) as u32
}

/// This CPU's registry index, resolved from its **own** MPIDR against the table
/// each CPU filled in [`smp_set_this_cpu`].
///
/// `tpidr_el1` - the register RISC-V's `tp` equivalent would be - is already the
/// owner of the current `TrapFrame` pointer in `kernel/arch/aarch64/vectors.S`, so
/// it is not free for a per-CPU index. A search over a tiny fixed table costs one
/// system-register read and is on no hot path (only code that opted into SMP
/// reaches `smp::this_cpu`). Falls back to 0 - the boot CPU - for a CPU that has
/// not registered, which is exactly the pre-bring-up single-CPU answer.
#[cfg(feature = "smp")]
pub fn cpu_index() -> usize {
    let me = mpidr_aff0();
    // SAFETY: each CPU writes only its own slot, and the write happens-before the
    // reads that matter (a secondary registers before marking itself online).
    let table = unsafe { *core::ptr::addr_of!(MPIDR_OF_CPU) };
    for (i, &id) in table.iter().enumerate() {
        if id == me {
            return i;
        }
    }
    0
}

/// Record that the CPU running this call is registry index `index`, by writing its
/// own MPIDR affinity into that slot. Runs on the CPU it describes.
#[cfg(feature = "smp")]
pub fn smp_set_this_cpu(index: usize) {
    let me = mpidr_aff0();
    // SAFETY: single writer per slot; no other CPU reads it before the secondary
    // signals itself up.
    unsafe {
        (*core::ptr::addr_of_mut!(MPIDR_OF_CPU))[index] = me;
    }
}

/// The boot CPU's hardware id: MPIDR_EL1 affinity 0, read from the hardware.
#[cfg(feature = "smp")]
pub fn boot_cpu_hw_id() -> u32 {
    mpidr_aff0()
}

unsafe extern "C" {
    /// Guarded PSCI call (arch/aarch64/psci.S): `conduit` 0 = `hvc #0`, 1 =
    /// `smc #0`, behind a temporary exception vector that catches a trap. Returns
    /// the PSCI return value, or [`PSCI_TRAPPED`] if the conduit trapped.
    fn psci_call_guarded(conduit: u64, fnid: u64, a1: u64, a2: u64, a3: u64) -> u64;
}

#[cfg(feature = "smp")]
unsafe extern "C" {
    /// Physical (low LMA) address of the MMU-on secondary entry trampoline.
    static SMP_SECONDARY_ENTRY_PA: u64;
    /// `.boot.bss` slots (identity low) where the primary publishes MAIR/TCR/SCTLR
    /// for the secondary to adopt verbatim - see the header of smp.S.
    static AP_SYSREGS: u64;
}

/// Sentinel returned by [`psci_call_guarded`] when the conduit trapped to EL1.
const PSCI_TRAPPED: u64 = 0xFFFF_FFFF_FFFF_FFFF;

/// PSCI function ids (SMC Calling Convention / PSCI 1.1): `PSCI_VERSION` is the
/// cheapest read-only call, which is what makes it a good conduit probe.
const PSCI_VERSION: u64 = 0x8400_0000;
#[cfg(feature = "smp")]
const PSCI_CPU_ON: u64 = 0xC400_0003;
/// `PSCI_AFFINITY_INFO` (64-bit): "is there a CPU at this affinity, and is it on?"
/// A present CPU answers ON (0), OFF (1) or ON_PENDING (2); an affinity the
/// platform does not implement answers INVALID_PARAMETERS (-2). That distinction
/// is what makes it an **enumeration** call and not merely a status call.
const PSCI_AFFINITY_INFO: u64 = 0xC400_0004;

/// The conduit that actually answered PSCI_VERSION, cached: 0 = HVC, 1 = SMC,
/// [`u64::MAX`] = neither (probed once).
static mut PSCI_CONDUIT: u64 = u64::MAX;
static mut PSCI_PROBED: bool = false;
/// The PSCI version the working conduit reported, for the bring-up report.
static mut PSCI_VERSION_SEEN: u64 = 0;

/// Find the PSCI conduit by **calling** it, not by assuming one. Tries HVC then
/// SMC with `PSCI_VERSION`; a conduit counts as working only if it returns without
/// trapping and reports a sane (non-zero, non-error) version. Cached.
fn psci_conduit() -> Option<u64> {
    // SAFETY: single CPU at bring-up; a plain static read.
    if unsafe { *core::ptr::addr_of!(PSCI_PROBED) } {
        // SAFETY: as above.
        let c = unsafe { *core::ptr::addr_of!(PSCI_CONDUIT) };
        return if c == u64::MAX { None } else { Some(c) };
    }
    let mut found = u64::MAX;
    let mut version = 0;
    for conduit in [0u64, 1u64] {
        // SAFETY: the helper guards the call with its own exception vector, so a
        // conduit that is UNDEFINED at EL1 returns the sentinel instead of
        // faulting the kernel.
        let v = unsafe { psci_call_guarded(conduit, PSCI_VERSION, 0, 0, 0) };
        // A trapped conduit returns the sentinel; a serviced one returns
        // major<<16|minor. Reject 0 and the negative error range.
        if v != PSCI_TRAPPED && v != 0 && (v as i64) > 0 {
            found = conduit;
            version = v;
            break;
        }
    }
    // SAFETY: single CPU at bring-up.
    unsafe {
        *core::ptr::addr_of_mut!(PSCI_CONDUIT) = found;
        *core::ptr::addr_of_mut!(PSCI_VERSION_SEEN) = version;
        *core::ptr::addr_of_mut!(PSCI_PROBED) = true;
    }
    if found == u64::MAX { None } else { Some(found) }
}

/// Publish this (primary) CPU's MAIR/TCR/SCTLR for the secondary trampoline, so
/// the secondary comes up in **exactly** the primary's translation regime rather
/// than in a re-derived approximation of it (the same reasoning as the x86 AP's
/// control registers, where a divergence cost a triple fault).
#[cfg(feature = "smp")]
fn publish_ap_sysregs() {
    let (mair, tcr, sctlr): (u64, u64, u64);
    // SAFETY: kernel context; reads of this CPU's own EL1 system registers.
    unsafe {
        asm!("mrs {0}, mair_el1", out(reg) mair, options(nomem, nostack));
        asm!("mrs {0}, tcr_el1", out(reg) tcr, options(nomem, nostack));
        asm!("mrs {0}, sctlr_el1", out(reg) sctlr, options(nomem, nostack));
    }
    // AP_SYSREGS is in `.boot.bss`, whose symbol address is its physical address;
    // the kernel runs high, so it is written through the linear map.
    // SAFETY: single CPU at bring-up; three aligned 8-byte writes to kernel BSS.
    unsafe {
        let base = phys_to_virt(core::ptr::addr_of!(AP_SYSREGS) as usize) as *mut u64;
        base.write(mair);
        base.add(1).write(tcr);
        base.add(2).write(sctlr);
    }
}

/// Start the secondary core with MPIDR affinity `hw_id` via PSCI `CPU_ON`, over
/// whichever conduit the probe found. Returns `Ok(())` once PSCI accepted the
/// call (the portable `smp::bring_up_one` then waits, bounded, for the core to run
/// kernel code and mark itself online), or the observed reason it was refused.
#[cfg(feature = "smp")]
pub fn smp_start_secondary(hw_id: u32) -> Result<(), &'static str> {
    let Some(conduit) = psci_conduit() else {
        return Err(
            "PSCI answered on neither conduit: hvc #0 and smc #0 both trapped to EL1 (no PSCI \
             implementation and no EL2/EL3 firmware in this QEMU config)",
        );
    };
    // Report what answered, not what the machine type implies: the conduit and the
    // PSCI version the probe actually got back (docs/ENGINEERING.md 1).
    // SAFETY: single CPU; set by the probe above.
    let version = unsafe { *core::ptr::addr_of!(PSCI_VERSION_SEEN) };
    crate::println!(
        "aarch64: PSCI answered on {} - version {}.{} (probed, not assumed)",
        if conduit == 0 { "hvc #0" } else { "smc #0" },
        version >> 16,
        version & 0xFFFF
    );
    publish_ap_sysregs();
    // SAFETY: a read of a link-time constant in `.rodata`.
    let entry = unsafe { *core::ptr::addr_of!(SMP_SECONDARY_ENTRY_PA) };
    // SAFETY: guarded call; `entry` is the trampoline's own physical address.
    let status = unsafe { psci_call_guarded(conduit, PSCI_CPU_ON, hw_id as u64, entry, 0) };
    match status {
        PSCI_TRAPPED => Err("PSCI CPU_ON trapped although PSCI_VERSION did not"),
        0 => Ok(()), // SUCCESS
        s if s == (-1i64 as u64) => Err("PSCI CPU_ON returned NOT_SUPPORTED"),
        s if s == (-2i64 as u64) => Err("PSCI CPU_ON returned INVALID_PARAMETERS"),
        s if s == (-4i64 as u64) => {
            Err("PSCI CPU_ON returned ALREADY_ON (QEMU pre-started the core)")
        }
        s if s == (-7i64 as u64) => Err("PSCI CPU_ON returned INVALID_ADDRESS"),
        _ => Err("PSCI CPU_ON returned an error status"),
    }
}

/// Where the secondary trampoline hands control to Rust, on the secondary core, at
/// EL1 with the MMU on and the primary's page tables live. It hands the portable
/// driver this core's hardware identity - read from its own MPIDR, not passed in -
/// and returns to an asm `wfe` park.
#[cfg(feature = "smp")]
#[unsafe(no_mangle)]
extern "C" fn aarch64_secondary_main() {
    crate::smp::secondary_run(mpidr_aff0());
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

/// The host bridge's 32-bit MMIO window for BAR assignment
/// (docs/GPU-HARDWARE.md 3). QEMU `virt` puts PCIe MMIO at
/// 0x1000_0000..0x3EFF_0000 (below the 0x3F00_0000 ECAM).
pub fn pci_mmio_window() -> (u64, u64) {
    (0x1000_0000, 0x2E00_0000)
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
        // EL0t. IRQ (SPSR.I, bit 7) is masked unless the scheduler needs to be
        // able to take the CPU away from this cell.
        //
        // Masked was right while every scheduler was cooperative: cells took no
        // interrupts at EL0 and the kernel serviced the UART/timer IRQs in its own
        // `wfi` idle path (docs/LIBRHEO.md Phase D/F). It is also *why* nothing could
        // stop a compute-bound cell - the preemption timer physically could not be
        // delivered. With queue-driven dispatch enabled the mask comes off and the
        // EL0 IRQ vector slot (`el0_irq`, previously a fatal slot) handles it
        // (docs/SUBSTRATE.md pillar 3).
        //
        // Read at frame-construction time rather than flipped later, because a frame
        // built for a cooperative boot must keep the pre-migration bits exactly.
        spsr: if crate::sched::dispatch::enabled() {
            0
        } else {
            1 << 7
        },
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

/// Bytes reserved per cell for a saved U-mode FP/SIMD image (V0-V31 + FPCR +
/// FPSR = 528 bytes; rounded up for alignment/headroom). SVE state is much
/// larger and not enabled.
pub const FP_AREA_LEN: usize = 1024;

/// Initialize a cell's FP/SIMD save area to a clean state (all V-regs, FPCR,
/// FPSR zero - the AArch64 reset default). Unlike x86's MXCSR, a zeroed FPCR
/// masks all exceptions, so a zeroed area is already a valid clean image; this
/// just makes re-install explicit.
///
/// # Safety
/// `area` must point to at least `FP_AREA_LEN` writable bytes.
pub unsafe fn fp_area_init(area: *mut u8) {
    unsafe { core::ptr::write_bytes(area, 0, FP_AREA_LEN) };
}

/// Portable `SIMD_*` tier mask a cell reads (docs/TILES.md 4). ARM64 reports
/// NEON (Advanced SIMD) when present - it is enabled at EL0 (CPACR_EL1.FPEN)
/// and is the cell's baseline vector unit; the tile executor auto-vectorises to
/// it. SVE is not enabled (wide, VL-dependent state), so no SVE tier.
pub fn fp_simd_tiers() -> u64 {
    let pfr0: u64;
    // SAFETY: reading an ID register at EL1.
    unsafe { asm!("mrs {0}, id_aa64pfr0_el1", out(reg) pfr0, options(nomem, nostack)) };
    let simd = (pfr0 >> 20) & 0xF; // AdvSIMD field; 0xF = absent
    if simd != 0xF {
        crate::abi::SIMD_NEON
    } else {
        0
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

// ---------------------------------------------------- signal frame (L5)

/// User VA of the injected `rt_sigreturn` trampoline page (docs/LINUX-COMPAT.md
/// L5). ARM64/RISC-V have no SA_RESTORER path; the kernel normally supplies the
/// restorer via the vDSO, which does not exist here, so a 2-instruction page is
/// mapped into every Linux cell (by `linux::stack::setup_stack`) and the signal
/// handler's LR is pointed at it. A free low page, well below any load base.
pub const SIGTRAMP_VA: usize = 0x2000;

/// The asm-generic kernel `struct sigaction` has no `sa_restorer` field (the
/// restorer comes from the injected trampoline, not the caller); `sa_mask`
/// follows `sa_flags` directly (docs/LINUX-COMPAT.md L5).
pub const SIGACTION_HAS_RESTORER: bool = false;

/// The `rt_sigreturn` trampoline: `mov x8, #139 (rt_sigreturn); svc #0`.
/// Encoded little-endian (movz x8,#139 = 0xD2801168; svc #0 = 0xD4000001).
pub fn sig_tramp_code() -> &'static [u8] {
    &[0x68, 0x11, 0x80, 0xD2, 0x01, 0x00, 0x00, 0xD4]
}

/// The interrupted user stack pointer (for building a signal frame, L5).
pub fn user_sp(frame: &TrapFrame) -> u64 {
    frame.sp_el0
}

/// The kernel stack pointer saved in `frame` (loaded on trap entry). `execve`
/// reuses it when building the new image's entry frame (docs/LINUX-COMPAT.md L6).
pub fn trapframe_kernel_sp(frame: &TrapFrame) -> usize {
    frame.kernel_sp as usize
}

// rt_sigframe layout on the user stack: { siginfo(128); ucontext }.
const INFO_OFF: u64 = 0;
const UC_OFF: u64 = 128;
const UC_SIGMASK_OFF: u64 = 40; // uc_sigmask within the ucontext
const MC_OFF: u64 = UC_OFF + 176; // uc_mcontext within the frame
const MC_REGS: u64 = MC_OFF + 8; // sigcontext.regs[0] (x0)
const MC_SP: u64 = MC_OFF + 8 + 31 * 8;
const MC_PC: u64 = MC_SP + 8;
const MC_PSTATE: u64 = MC_PC + 8;
const FRAME_RESERVE: u64 = 1024;

/// Build a Linux `rt_sigframe` on the user stack and rewrite `frame` to enter
/// the handler (docs/LINUX-COMPAT.md L5). The cell's address space is active
/// (trap context), so user VAs are written directly.
///
/// # Safety
/// `spec.stack_top` must be a valid, writable user stack VA in the active cell.
pub fn setup_rt_frame(frame: &mut TrapFrame, spec: &super::SigFrameSpec) {
    let base = (spec.stack_top - FRAME_RESERVE) & !0xF; // 16-aligned SP
    // SAFETY: [base, base+FRAME_RESERVE) is writable user stack in the active cell.
    unsafe {
        let w = |off: u64, v: u64| ((base + off) as *mut u64).write(v);
        // siginfo: si_signo, si_errno, si_code, si_addr.
        ((base + INFO_OFF) as *mut i32).write(spec.signo as i32);
        ((base + INFO_OFF + 4) as *mut i32).write(0);
        ((base + INFO_OFF + 8) as *mut i32).write(spec.si_code);
        w(INFO_OFF + 16, spec.si_addr);
        w(UC_OFF, 0); // uc_flags
        w(UC_OFF + 8, 0); // uc_link
        w(UC_OFF + UC_SIGMASK_OFF, spec.saved_mask);
        w(MC_OFF, spec.si_addr); // sigcontext.fault_address
        for i in 0..31usize {
            w(MC_REGS + (i as u64) * 8, frame.regs[i]);
        }
        w(MC_SP, frame.sp_el0);
        w(MC_PC, frame.elr);
        w(MC_PSTATE, frame.spsr);
    }
    frame.regs[0] = spec.signo as u64; // x0: signo
    frame.regs[1] = base + INFO_OFF; // x1: siginfo*
    frame.regs[2] = base + UC_OFF; // x2: ucontext*
    frame.regs[30] = SIGTRAMP_VA as u64; // lr -> rt_sigreturn trampoline
    frame.sp_el0 = base;
    frame.elr = spec.handler;
}

/// Restore a `TrapFrame` saved by [`setup_rt_frame`] on `rt_sigreturn` and
/// return the saved signal mask. On entry the handler's SP (frame base) is in
/// `frame.sp_el0` (the trampoline does not move it).
pub fn restore_rt_frame(frame: &mut TrapFrame) -> u64 {
    let base = frame.sp_el0;
    // SAFETY: `base` is the frame VA in the active cell, written by setup_rt_frame.
    unsafe {
        for i in 0..31usize {
            frame.regs[i] = ((base + MC_REGS + (i as u64) * 8) as *const u64).read();
        }
        frame.sp_el0 = ((base + MC_SP) as *const u64).read();
        frame.elr = ((base + MC_PC) as *const u64).read();
        frame.spsr = ((base + MC_PSTATE) as *const u64).read();
        ((base + UC_OFF + UC_SIGMASK_OFF) as *const u64).read()
    }
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
    let ec = (esr >> 26) & 0x3F;
    let kind = if ec == EC_SVC64 {
        super::TrapKind::Syscall
    } else {
        super::TrapKind::Fault
    };
    let resume = crate::user::on_user_trap(kind, fault_cause(ec), far as usize, frame);
    if resume.is_null() {
        return_to_kernel();
    }
    resume
}

/// Map an EL0 exception class (ESR_EL1.EC) to a portable fault cause
/// (docs/LINUX-COMPAT.md L5). Data/instruction aborts are SIGSEGV; PC/SP
/// alignment faults SIGBUS; illegal execution / unknown SIGILL; FP-trap SIGFPE.
fn fault_cause(ec: u64) -> super::FaultCause {
    match ec {
        0x20 | 0x21 | 0x24 | 0x25 => super::FaultCause::Segv, // instr / data abort
        0x22 | 0x26 => super::FaultCause::Bus,                // PC / SP alignment
        0x00 | 0x0E => super::FaultCause::Ill,                // unknown / illegal state
        0x2C => super::FaultCause::Fpe,                       // trapped FP
        _ => super::FaultCause::Segv,
    }
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

// ------------------------------------------------------ MSI-X (NVMe completions)

/// Where a PCIe device should write, and what, to raise a completion interrupt.
///
/// **`None` here**, so a caller polls and reports that it is polling rather than
/// programming a device to write to nowhere. What is missing is not a constant but
/// a driver: this ISA needs a GICv3 ITS (device table, ITT, MAPD/MAPTI commands),
/// where x86-64's MSI is just a write to the local-APIC message region. Named in
/// docs/SUBSTRATE.md S5 as the remaining work rather than papered over.
pub fn msi_target(_dest_hw_id: u32, _slot: usize) -> Option<(u64, u32)> {
    None
}

/// Completion interrupts taken - always 0 while [`msi_target`] returns `None`.
pub fn msi_irq_count() -> u64 {
    0
}

/// Let any pending interrupt be delivered, then mask again - bounded by
/// construction, unlike [`idle_wait`]. See the x86-64 twin for why a probe of an
/// interrupt path must not be able to halt.
pub fn irq_window() {
    // SAFETY: kernel context; unmask IRQ for one instruction, then mask again.
    unsafe {
        asm!(
            "msr daifclr, #2; nop; msr daifset, #2",
            options(nomem, nostack)
        )
    };
}

/// Software-enable this core's local interrupt controller. Nothing to do here
/// while [`msi_target`] returns `None`: the per-core enabling these ISAs need is
/// already done where their interrupt controllers are brought up.
pub fn irq_ready_this_cpu() {}
