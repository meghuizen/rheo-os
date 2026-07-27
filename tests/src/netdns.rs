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

use core::ptr::addr_of_mut;

use kernel::hw::virtio_net;
use kernel::svc::{self};
use kernel::user::Outcome;
use kernel::{arch, println};

#[path = "console_personality.rs"]
mod console_personality;
#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;

static DEMO: &[u8] = fixture::cell!("netdns-demo");

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
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
    svc::set_file_ops(console_personality::console_and_empty_fs());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(DEMO, "netdns-demo") };

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
