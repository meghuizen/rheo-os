//! In-QEMU test kernel for rheo-net Phase N1c (docs/NETSTACK.md,
//! docs/NETWORKING.md): the **caching DNS client** through the `net` crate. A
//! cell loaded from the `netdns-demo` ELF asks the driver for the NIC MAC, then:
//! (1) parses a hand-crafted **compressed** DNS response and asserts the exact
//! A record (a pointer-compression oracle), (2) proves crafted pointer loops
//! return an error not a hang, (3) resolves names from a static **hosts** table,
//! a **blocklist**, and a pre-seeded **cache** - all **network-free**, asserted
//! via a query counter that must stay zero - plus a standalone LRU + TTL cache
//! unit, and (4) does a **bonus live** resolve of `example.com` over SLIRP's DNS
//! (10.0.2.3), tolerating a timeout if this sandbox has no outbound DNS. It
//! exits `0x42` only if every deterministic check passes, so the exit code is
//! the proof.
//!
//! The transport differs per machine: virtio-mmio on the riscv/arm `virt`
//! machines, virtio-pci on x86-64 q35. The probe tries both, so all three ISAs
//! exercise the same NIC path. The skip branch fires only if no virtio-net device
//! is attached at all.
//!
//! Wiring mirrors `netl4` (queue pair + minted cap + `set_queue_info`); the NIC
//! is discovered + installed like `blockfs` discovers virtio-blk. A minimal
//! console `FileOps` backs the cell's `println!` (fd 1/2 -> serial).

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
use kernel::{arch, load, println};

#[cfg(target_arch = "x86_64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/x86_64-unknown-none/release/netdns-demo"
));
#[cfg(target_arch = "aarch64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/netdns-demo"
));
#[cfg(target_arch = "riscv64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/netdns-demo"
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
    println!("netdns: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    // Discover and install the virtio-net NIC.
    let dev = match virtio_net::probe() {
        Some(d) => d,
        None => {
            println!("netdns: no virtio-net device attached - skipping");
            println!("netdns: PASS");
            arch::exit(arch::ExitCode::Success)
        }
    };
    let m = dev.mac();
    println!(
        "netdns: virtio-net found, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    );
    virtio_net::install(dev);

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
    let entry = load::load_elf(DEMO, &mut aspace).expect("load netdns-demo ELF");
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
                "netdns-demo exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "netdns: caching DNS client (codec + hosts + blocklist + cache), exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("netdns-demo faulted at {addr:#x}"),
    }

    println!("netdns: PASS");
    arch::exit(arch::ExitCode::Success)
}
