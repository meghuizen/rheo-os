//! In-QEMU test kernel for rheo-net Phase N1e (docs/NETSTACK.md,
//! docs/NETWORKING.md): **first-class TTL / IPv6 hop limit + traceroute** through
//! the `net` crate. A cell loaded from the `nettrace-demo` ELF asks the driver
//! for the NIC MAC, then runs the **network-free core proof**: (1) TTL and hop
//! limit round-trip through build -> parse (default 64 + explicit values); (2) the
//! forwarding-plane `ip::decrement_ttl`/`decrement_hop_limit` primitives -
//! decrement + recompute a valid IPv4 checksum (oracle `0xB961`), drop-signal at
//! TTL 0/1; (3) an ICMP Time Exceeded build -> parse oracle (v4 `0xF4FF`, and the
//! v6 codec `0x1936`); and (4) the traceroute state machine fed synthetic
//! responses - a crafted 3-router path + destination Echo Reply is classified and
//! reconstructed into the exact ordered hop list, proving multi-hop discovery
//! **without real intermediate routers**. Then a **bonus live** 1-hop trace to the
//! gateway `10.0.2.2` (SLIRP is the destination at hop 1; a timeout is tolerated).
//! It exits `0x42` only if every deterministic check passes, so the exit code is
//! the proof.
//!
//! The transport differs per machine: virtio-mmio on the riscv/arm `virt`
//! machines, virtio-pci on x86-64 q35. The probe tries both, so all three ISAs
//! exercise the same NIC path. The skip branch fires only if no virtio-net device
//! is attached at all.
//!
//! Wiring mirrors `netdns` (queue pair + minted cap + `set_queue_info`); the NIC
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

static DEMO: &[u8] = fixture::cell!("nettrace-demo");

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("nettrace: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    // Discover and install the virtio-net NIC.
    let dev = match virtio_net::probe() {
        Some(d) => d,
        None => {
            println!("nettrace: no virtio-net device attached - skipping");
            println!("nettrace: PASS");
            arch::exit(arch::ExitCode::Success)
        }
    };
    let m = dev.mac();
    println!(
        "nettrace: virtio-net found, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    );
    virtio_net::install(dev);

    svc::init();
    svc::set_file_ops(console_personality::console_and_empty_fs());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(DEMO, "nettrace-demo") };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "nettrace-demo exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "nettrace: TTL/hop-limit + forwarding decrement + Time Exceeded + traceroute state machine, exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("nettrace-demo faulted at {addr:#x}"),
    }

    println!("nettrace: PASS");
    arch::exit(arch::ExitCode::Success)
}
