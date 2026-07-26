//! In-QEMU test kernel for rheo-net **N2d** (docs/NETSTACK.md, the async-receive
//! path): **true async receive** - a NIC RX interrupt plus a park-until-frame
//! primitive, so a cell waiting for a packet costs no CPU instead of re-polling.
//!
//! The cell (`librheo-netwait`) drains the receive queue, spawns a witness strand,
//! sends a broadcast ARP request for the SLIRP gateway `10.0.2.2`, then parks in
//! `net::recv`. SLIRP's ARP reply is the wake event - a genuine received frame, on
//! a real virtio-net device, network-free and deterministic (the same proof shape
//! as `librheonet`, now taken through the blocking path).
//!
//! What this kernel asserts:
//!
//! - **the cell's own checks**, via its exit code `0x42`: the frame really is the
//!   gateway's ARP reply; the witness strand ran while the receiver was parked (so
//!   the receive suspended rather than holding the vcore); and the reactor recorded
//!   **exactly one wakeup per parked receive** - one park + one wake, never N
//!   re-polls (the no-spin property);
//! - **a genuine NIC interrupt** (`net_rx::irq_count() > 0`) on the ISAs where the
//!   RX interrupt is wired - it cannot be faked, the count is only incremented from
//!   the ISA's interrupt vector;
//! - **the idle-park** (`net_rx::did_idle()`) when the wait actually had to halt -
//!   the kernel stopped the CPU at WFI and the NIC's interrupt woke it.
//!
//! Per-ISA honesty (docs/NETSTACK.md has the table): RISC-V and ARM64 drive the
//! virtio-mmio device's interrupt line (AIA APLIC->IMSIC / GICv3 SPI). x86-64's NIC
//! is virtio-*pci* driven through the PCI config tunnel with no mapped BAR and no
//! usable IOAPIC line under QEMU TCG, so there the kernel wait falls back to a
//! bounded poll: the cell still parks once, but the CPU spins - reported, never
//! claimed as an idle.

#![no_std]
#![no_main]

extern crate alloc;

use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{BUDGET_UNLIMITED, CapTable, ObjectKind, ObjectTable, READ, WRITE};
use kernel::hw::virtio_net;
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc::{self, FileOps};
use kernel::user::{self, Outcome};
use kernel::{arch, load, net_rx, println};

#[cfg(target_arch = "x86_64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/x86_64-unknown-none/release/librheo-netwait"
));
#[cfg(target_arch = "aarch64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/librheo-netwait"
));
#[cfg(target_arch = "riscv64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/librheo-netwait"
));

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

// A console-only FileOps so the cell's `println!` (SYS_WRITE_FD on fd 1/2)
// reaches the serial line; every other file op is unused here.
fn con_open(_p: u64, _l: u64, _f: u64) -> i64 {
    -2
}
fn con_close(_fd: u64) -> i64 {
    0
}
fn con_read(_fd: u64, _b: u64, _l: u64) -> i64 {
    -9
}
fn con_write(fd: u64, buf_va: u64, len: u64) -> i64 {
    if fd == 1 || fd == 2 {
        let buf = unsafe { core::slice::from_raw_parts(buf_va as *const u8, len as usize) };
        for &b in buf {
            arch::serial_write_byte(b);
        }
        len as i64
    } else {
        -9
    }
}
fn con_lseek(_fd: u64, off: i64, _w: u64) -> i64 {
    off
}
fn con_stat(_p: u64, _l: u64, _s: u64) -> i64 {
    -38
}
fn con_fstat(_fd: u64, _s: u64) -> i64 {
    -38
}
fn con_getdents(_p: u64, _l: u64, _b: u64, _bl: u64) -> i64 {
    -38
}

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("netwait: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    // Discover and install the virtio-net NIC.
    let dev = match virtio_net::probe() {
        Some(d) => d,
        None => {
            println!("netwait: no virtio-net device attached - skipping");
            println!("netwait: PASS");
            arch::exit(arch::ExitCode::Success)
        }
    };
    let m = dev.mac();
    let slot = dev.mmio_slot();
    virtio_net::install(dev);

    // Bring up the NIC's RX interrupt (opt-in: only this kernel calls it, so every
    // other kernel boots exactly as before). Where the ISA cannot deliver it, the
    // kernel's wait falls back to a bounded poll - reported, not claimed.
    net_rx::reset();
    let irq = net_rx::enable_irq();
    // The timer interrupt too: a *bounded* receive arms a deadline so the wait can
    // halt the CPU and still wake (docs/NETSTACK.md). Opt-in, like the NIC IRQ.
    arch::enable_timer_irq();
    println!(
        "netwait: virtio-net MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, mmio slot {:?}, receive wait: {}",
        m[0],
        m[1],
        m[2],
        m[3],
        m[4],
        m[5],
        slot,
        if irq {
            "interrupt-driven (WFI idle)"
        } else {
            "kernel poll (no NIC RX interrupt on this ISA)"
        }
    );

    svc::init();
    svc::set_file_ops(FileOps {
        open: con_open,
        close: con_close,
        read: con_read,
        write: con_write,
        lseek: con_lseek,
        stat: con_stat,
        fstat: con_fstat,
        getdents: con_getdents,
    });

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(DEMO, &mut aspace).expect("load librheo-netwait ELF");
    let stack_top = load::map_stack(&mut aspace);
    let qp = load::map_queue(&mut aspace);

    // SAFETY: single-threaded init; the statics outlive the run.
    let outcome = unsafe {
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let object = objects.create(ObjectKind::QueuePair).unwrap();
        let cap = caps
            .mint(objects, object, READ | WRITE, BUDGET_UNLIMITED)
            .unwrap();
        let cap_id = cap.raw_low32();

        (*addr_of_mut!(QP)).write(qp);
        let qp_ptr = (*addr_of_mut!(QP)).as_ptr();

        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(entry, stack_top, 0, kernel_sp);
        user::reset();
        user::install(0, &aspace, caps, objects, qp_ptr, addr_of_mut!(frame));
        user::set_queue_info(0, load::USER_QUEUE_VA as u64, cap_id);
        user::run(0).1
    };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "librheo-netwait exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!("netwait: parked receive woke on a real frame, exit {code:#x} OK");
        }
        Outcome::Faulted(addr) => panic!("librheo-netwait faulted at {addr:#x}"),
    }

    // Kernel-side evidence. On an interrupt-driven ISA the NIC must have raised at
    // least one interrupt that the kernel took (the count is only incremented from
    // the interrupt vector, so it cannot be faked); the WFI idle-park is asserted
    // whenever the wait actually had to halt (the frame had not yet arrived).
    if net_rx::interrupt_driven() {
        assert!(
            net_rx::irq_count() > 0,
            "interrupt-driven ISA but the kernel never took a NIC interrupt"
        );
        println!(
            "netwait: NIC interrupts taken: {} (genuine device interrupt)",
            net_rx::irq_count()
        );
        assert!(
            net_rx::did_idle(),
            "interrupt-driven ISA but the receive wait never halted the CPU"
        );
        println!(
            "netwait: idle-park proven (the kernel halted the CPU inside the receive wait - \
             0% CPU, woken by an interrupt)"
        );
    } else {
        println!(
            "netwait: kernel poll fallback (no NIC RX interrupt on this ISA) - the cell parks \
             once, but the kernel spins while waiting"
        );
    }

    println!("netwait: PASS");
    arch::exit(arch::ExitCode::Success)
}
