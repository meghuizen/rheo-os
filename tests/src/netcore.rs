//! In-QEMU test kernel for rheo-net Phase N1a (docs/NETSTACK.md,
//! docs/NETWORKING.md): the **L2/L3 core through the new `net` crate**. A cell
//! loaded from the `netcore-demo` ELF asks the driver for the NIC MAC, resolves
//! the SLIRP gateway `10.0.2.2` via `net::arp::resolve` (a genuine ARP request
//! out / reply in through the virtqueues, built with `net::eth`/`net::arp` -
//! replacing `librheonet`'s hand-built frame), then round-trips IPv4 + IPv6
//! headers and the ones-complement checksum in memory. It exits `0x42` only if
//! every check passes, so the exit code is the proof.
//!
//! The transport differs per machine: virtio-mmio on the riscv/arm `virt`
//! machines, virtio-pci on x86-64 q35. The probe tries both, so all three ISAs
//! exercise the same NIC path. The skip branch fires only if no virtio-net device
//! is attached at all.
//!
//! Wiring mirrors `librheonet` (queue pair + minted cap + `set_queue_info`); the
//! NIC is discovered + installed like `blockfs` discovers virtio-blk. A minimal
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

static DEMO: &[u8] = fixture::cell!("netcore-demo");

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("netcore: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    // Discover and install the virtio-net NIC.
    let dev = match virtio_net::probe() {
        Some(d) => d,
        None => {
            println!("netcore: no virtio-net device attached - skipping");
            println!("netcore: PASS");
            arch::exit(arch::ExitCode::Success)
        }
    };
    let m = dev.mac();
    println!(
        "netcore: virtio-net found, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    );
    virtio_net::install(dev);

    svc::init();
    svc::set_file_ops(console_personality::console_and_empty_fs());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(DEMO, "netcore-demo") };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "netcore-demo exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "netcore: eth/arp/ip core (ARP round trip through the stack), exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("netcore-demo faulted at {addr:#x}"),
    }

    println!("netcore: PASS");
    arch::exit(arch::ExitCode::Success)
}
