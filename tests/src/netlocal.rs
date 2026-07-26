//! In-QEMU test kernel for rheo-net Phase N1d (docs/NETSTACK.md §10): the
//! **native local fast path** (`net::local`). Load `netlocal-demo` into **two**
//! cells, wire them with a **shared typed queue pair** (one ring region mapped
//! into both), and let them exchange a payload over `net::local` - a zero-copy
//! cell-to-cell transport with **no IP/Ethernet**. Assert the round trip on all
//! three ISAs.
//!
//! Cell 0 is the client (channel role 0), cell 1 the server (role 1). The client
//! draws a known 4 KiB payload into a buffer grant, seals + shares it (zero-copy
//! delegation), sends the peer VA + length over the local stream, and awaits the
//! server's checksum of the SAME frames; it asserts the checksums match (proving
//! zero-copy) and exits `0x42`. Needs **no netdev** - the transport is entirely
//! in-cell / cross-cell (this is the "skip the IP stack" local path).
//!
//! Wiring mirrors `librheowl` (two cells + a shared channel at 24 GiB). The kernel
//! is untouched: `net::local` is portable userspace over the existing IPC ABI.

#![no_std]
#![no_main]

use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{BUDGET_UNLIMITED, CapTable, ObjectKind, ObjectTable, READ, WRITE};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc::{self, FileOps};
use kernel::user::{self, Outcome};
use kernel::{arch, load, println};

#[cfg(target_arch = "x86_64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/x86_64-unknown-none/release/netlocal-demo"
));
#[cfg(target_arch = "aarch64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/netlocal-demo"
));
#[cfg(target_arch = "riscv64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/netlocal-demo"
));

/// The client returns this on full success (see netlocal-demo.rs).
const EXPECTED_EXIT: u64 = 0x42;

fn c_write(fd: u64, buf_va: u64, len: u64) -> i64 {
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
fn c_stub_open(_p: u64, _l: u64, _f: u64) -> i64 {
    -38
}
fn c_stub_close(_fd: u64) -> i64 {
    -38
}
fn c_stub_read(_fd: u64, _b: u64, _l: u64) -> i64 {
    -38
}
fn c_stub_lseek(_fd: u64, _o: i64, _w: u64) -> i64 {
    -38
}
fn c_stub_stat(_p: u64, _l: u64, _s: u64) -> i64 {
    -38
}
fn c_stub_fstat(_fd: u64, _s: u64) -> i64 {
    -38
}
fn c_stub_getdents(_p: u64, _l: u64, _b: u64, _bl: u64) -> i64 {
    -38
}

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS0: CapTable = CapTable::new();
static mut CAPS1: CapTable = CapTable::new();
static mut QP0: MaybeUninit<QueuePair> = MaybeUninit::uninit();
static mut QP1: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK0: KStack = KStack([0; 64 * 1024]);
static mut KSTACK1: KStack = KStack([0; 64 * 1024]);

/// Mint a QueuePair capability into `caps` for a fresh object.
///
/// # Safety
/// `objects`/`caps` must be uniquely owned for the call (single-threaded init).
unsafe fn mint_queue_cap(objects: &mut ObjectTable, caps: &mut CapTable) -> u32 {
    let object = objects.create(ObjectKind::QueuePair).unwrap();
    caps.mint(objects, object, READ | WRITE, BUDGET_UNLIMITED)
        .unwrap()
        .raw_low32()
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("netlocal: start on {}", arch::NAME);

    svc::init();
    svc::set_file_ops(FileOps {
        open: c_stub_open,
        close: c_stub_close,
        read: c_stub_read,
        write: c_write,
        lseek: c_stub_lseek,
        stat: c_stub_stat,
        fstat: c_stub_fstat,
        getdents: c_stub_getdents,
    });

    // Two loaded cells, each with its own address space + kernel queue pair.
    let mut aspace0 = AddressSpace::new(1);
    let mut aspace1 = AddressSpace::new(2);
    let entry0 = load::load_elf(DEMO, &mut aspace0).expect("load netlocal-demo (client)");
    let entry1 = load::load_elf(DEMO, &mut aspace1).expect("load netlocal-demo (server)");
    let sp0 = load::map_stack(&mut aspace0);
    let sp1 = load::map_stack(&mut aspace1);
    let qp0 = load::map_queue(&mut aspace0);
    let qp1 = load::map_queue(&mut aspace1);

    // One shared channel region mapped into both cells at USER_CHANNEL_VA.
    let channel = load::alloc_channel();
    load::map_channel_into(&mut aspace0, &channel);
    load::map_channel_into(&mut aspace1, &channel);
    println!(
        "netlocal: two cells wired; shared channel at {:#x} ({} bytes), demo {} bytes",
        load::USER_CHANNEL_VA,
        QueuePair::REGION_SIZE,
        DEMO.len()
    );

    // SAFETY: single-threaded init; the statics outlive the run.
    let outcome = unsafe {
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps0 = &mut *addr_of_mut!(CAPS0);
        let caps1 = &mut *addr_of_mut!(CAPS1);

        let qp0_cap = mint_queue_cap(objects, caps0);
        let qp1_cap = mint_queue_cap(objects, caps1);
        let chan0_cap = mint_queue_cap(objects, caps0);
        let chan1_cap = mint_queue_cap(objects, caps1);

        (*addr_of_mut!(QP0)).write(qp0);
        (*addr_of_mut!(QP1)).write(qp1);
        let qp0_ptr = (*addr_of_mut!(QP0)).as_ptr();
        let qp1_ptr = (*addr_of_mut!(QP1)).as_ptr();

        let ksp0 = core::ptr::addr_of!(KSTACK0.0) as usize + 64 * 1024;
        let ksp1 = core::ptr::addr_of!(KSTACK1.0) as usize + 64 * 1024;
        let mut frame0 = arch::trapframe_new(entry0, sp0, 0, ksp0);
        let mut frame1 = arch::trapframe_new(entry1, sp1, 0, ksp1);

        user::reset();
        user::install(0, &aspace0, caps0, objects, qp0_ptr, addr_of_mut!(frame0));
        user::install(1, &aspace1, caps1, objects, qp1_ptr, addr_of_mut!(frame1));
        user::set_queue_info(0, load::USER_QUEUE_VA as u64, qp0_cap);
        user::set_queue_info(1, load::USER_QUEUE_VA as u64, qp1_cap);
        // Client = cell 0 (role 0), server = cell 1 (role 1).
        user::set_channel_info(0, load::USER_CHANNEL_VA as u64, chan0_cap, 0);
        user::set_channel_info(1, load::USER_CHANNEL_VA as u64, chan1_cap, 1);

        // Start the client; its send/await switches to the server and back.
        user::run(0).1
    };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "netlocal-demo client exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "netlocal: net::local zero-copy transfer ran (shared channel + sealed \
                 grant, no IP stack), exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("netlocal-demo faulted at {addr:#x}"),
    }

    println!("netlocal: PASS");
    arch::exit(arch::ExitCode::Success)
}
