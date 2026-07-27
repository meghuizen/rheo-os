//! In-QEMU test kernel for rheo-net Phase N2c (docs/NETSTACK.md §13,
//! docs/NETWORKING.md): the **smoltcp blessed transport cell** + the **native
//! sharded transport**. A cell loaded from the `netsmoltcp-demo` ELF (built with
//! the `smoltcp` feature) asks the driver for the NIC MAC, then proves, exiting
//! `0x42` only if every step passes:
//!   (B) the native `net::shard::Transport` - 2 shards, connections hashed to
//!       shards, a cross-shard TCP handshake + byte transfer over the in-cell
//!       virtual link (deterministic, network-free);
//!   (A) smoltcp: a deterministic in-cell TCP + UDP exchange over smoltcp's
//!       `Loopback` device, then a **live** smoltcp UDP DNS round trip to SLIRP's
//!       built-in responder `10.0.2.3:53` over a `QueueDevice` bound to
//!       `librheo::net` (proving smoltcp drives our real virtio-net driver).
//! So the exit code is the proof.
//!
//! The transport differs per machine: virtio-mmio on the riscv/arm `virt`
//! machines, virtio-pci on x86-64 q35. The probe tries both, so all three ISAs
//! exercise the same NIC path. The skip branch fires only if no virtio-net device
//! is attached at all.
//!
//! Wiring mirrors `netcore` (queue pair + minted cap + `set_queue_info`); the
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

static DEMO: &[u8] = fixture::cell!("netsmoltcp-demo");

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("netsmoltcp: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    // Discover and install the virtio-net NIC.
    let dev = match virtio_net::probe() {
        Some(d) => d,
        None => {
            println!("netsmoltcp: no virtio-net device attached - skipping");
            println!("netsmoltcp: PASS");
            arch::exit(arch::ExitCode::Success)
        }
    };
    let m = dev.mac();
    println!(
        "netsmoltcp: virtio-net found, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    );
    virtio_net::install(dev);

    svc::init();
    svc::set_file_ops(console_personality::console_and_empty_fs());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(DEMO, "netsmoltcp-demo") };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "netsmoltcp-demo exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "netsmoltcp: smoltcp cell (Loopback + live SLIRP UDP) + native sharded transport, exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("netsmoltcp-demo faulted at {addr:#x}"),
    }

    println!("netsmoltcp: PASS");
    arch::exit(arch::ExitCode::Success)
}
