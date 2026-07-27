//! In-QEMU test kernel for rheo-net Phase N4a (docs/NETSTACK.md the service-cell
//! section): the **network service cell + concurrent client fan-out** - the keystone
//! every later service rides on (app-protocol servers, the remote-INET bridge for
//! Linux binaries, onion routing).
//!
//! One long-lived service cell serves **three** client cells **concurrently**, each
//! over its **own** cross-cell channel. This kernel wires the service (cell 0) with
//! three channel ends - three separate ring regions at channel slots 0-2 - plus a
//! **cell-spawn** capability; the service then spawns `/bin/netsvc-client` three
//! times, handing client k its slot k as that child's own slot 0, and runs one strand
//! per client.
//!
//! The service cell asserts the whole ledger itself and exits `0x42` only if: every
//! client got its **distinct correct** response (a per-client echo transform + a
//! per-client name resolved from the service's `net::dns` tiers); the per-client
//! strands genuinely **interleaved** (the exact round-robin processing order, plus
//! all three requests in flight at the same instant); every message arrived by a
//! genuine reactor **park + wake**; and all three children were **reaped** with their
//! distinct exit codes. The core is deterministic and network-free; the service also
//! performs one **bonus live** ARP for a client, reported and never asserted.
//!
//! Concurrent, not parallel: one CPU, cooperative scheduling (SMP is task #27).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use core::mem::MaybeUninit;
use core::ptr::{addr_of, addr_of_mut};

use kernel::capability::{BUDGET_UNLIMITED, CapTable, ObjectKind, ObjectTable, READ, WRITE};
use kernel::hw::virtio_net;
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc;
use kernel::user::{self, Outcome};
use kernel::{arch, load, println};
use posix::{RamFs, fs, mount, sys};

#[path = "vfs_personality.rs"]
mod vfs_personality;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 4 * 1024 * 1024] = [0; 4 * 1024 * 1024];

macro_rules! bin {
    ($name:literal) => {{
        #[cfg(target_arch = "x86_64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../target/x86_64-unknown-none/release/",
                $name
            ))
        }
        #[cfg(target_arch = "aarch64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../target/aarch64-unknown-none-softfloat/release/",
                $name
            ))
        }
        #[cfg(target_arch = "riscv64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../target/riscv64gc-unknown-none-elf/release/",
                $name
            ))
        }
    }};
}

static SERVICE: &[u8] = bin!("netsvc-demo");
static CLIENT: &[u8] = bin!("netsvc-client");

/// How many client cells the service serves (>= 3 is the phase's gate).
const CLIENTS: usize = 3;

/// The service cell returns this on full success (see netsvc-demo.rs).
const EXPECTED_EXIT: u64 = 0x42;

/// Channel role of the service end (1 = acceptor/server: SQ consumer, CQ producer).
const ROLE_SERVER: u64 = 1;

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("netservice: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 4 * 1024 * 1024);
    }

    // A ramfs at / holding the client program the service spawns, and the VFS
    // personality so the loader's open/read/lseek reach it.
    svc::init();
    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    sys::mkdir("/bin").expect("mkdir /bin");
    fs::write("/bin/netsvc-client", CLIENT).expect("seed /bin/netsvc-client");
    svc::set_file_ops(vfs_personality::ops());

    // The NIC (present when the harness attached a netdev) backs the service's
    // bonus live op; without it the service reports the live path skipped and the
    // deterministic core is unaffected.
    match virtio_net::probe() {
        Some(dev) => {
            let m = dev.mac();
            println!(
                "netservice: virtio-net found, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                m[0], m[1], m[2], m[3], m[4], m[5]
            );
            virtio_net::install(dev);
        }
        None => println!("netservice: no virtio-net - the bonus live op will report skipped"),
    }

    println!(
        "netservice: service={} client={} bytes",
        SERVICE.len(),
        CLIENT.len()
    );

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(SERVICE, &mut aspace).expect("load netsvc-demo");
    let stack_top = load::map_stack(&mut aspace);
    let qp = load::map_queue(&mut aspace);

    // ONE cross-cell channel region PER CLIENT, each mapped into the service at its
    // own channel slot (docs/NETSTACK.md rheo-net N4a). A spawned client inherits
    // slot k's frames at its own slot 0, so each client has a private SPSC ring with
    // the service - which is what makes the fan-out real rather than a shared bus.
    let mut channels = [[0usize; load::CHANNEL_PAGES]; CLIENTS];
    for (k, chan) in channels.iter_mut().enumerate() {
        *chan = load::alloc_channel();
        load::map_channel_into_slot(&mut aspace, chan, k);
        println!(
            "netservice: client-{k} channel at {:#x} ({} bytes)",
            load::channel_slot_va(k),
            QueuePair::REGION_SIZE
        );
    }

    // SAFETY: single-threaded init; the statics + aspace/frame outlive the run (the
    // spawned client cells live in kernel-owned nproc storage).
    let outcome = unsafe {
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qobj = objects.create(ObjectKind::QueuePair).unwrap();
        let qcap = caps
            .mint(objects, qobj, READ | WRITE, BUDGET_UNLIMITED)
            .unwrap()
            .raw_low32();
        // The cell-spawn capability (ObjectKind::Cell + WRITE - no ambient auth).
        let cell_obj = objects.create(ObjectKind::Cell).unwrap();
        caps.mint(objects, cell_obj, WRITE, BUDGET_UNLIMITED)
            .unwrap();
        // One channel capability per end.
        let mut chan_caps = [0u32; CLIENTS];
        for cap in chan_caps.iter_mut() {
            let cobj = objects.create(ObjectKind::QueuePair).unwrap();
            *cap = caps
                .mint(objects, cobj, READ | WRITE, BUDGET_UNLIMITED)
                .unwrap()
                .raw_low32();
        }

        (*addr_of_mut!(QP)).write(qp);
        let qp_ptr = (*addr_of_mut!(QP)).as_ptr();
        let kernel_sp = addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(entry, stack_top, 0, kernel_sp);
        user::reset();
        user::install(0, &aspace, caps, objects, qp_ptr, addr_of_mut!(frame));
        user::set_queue_info(0, load::USER_QUEUE_VA as u64, qcap);
        for (k, &cap) in chan_caps.iter().enumerate() {
            user::set_channel_slot(0, k, load::channel_slot_va(k) as u64, cap, ROLE_SERVER);
        }
        user::run(0).1
    };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "netsvc-demo exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "netservice: one service cell served {CLIENTS} client cells concurrently \
                 (private channel each, one strand per client, round-robin interleave, \
                 all reaped), exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("netsvc-demo faulted at {addr:#x}"),
    }

    println!("netservice: PASS");
    arch::exit(arch::ExitCode::Success)
}
