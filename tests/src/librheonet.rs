//! In-QEMU test kernel for librheo Phase G (docs/LIBRHEO.md, docs/NETWORKING.md):
//! **raw-frame networking over a real virtio-net NIC**. A librheo cell asks the
//! driver for the NIC MAC, sends a broadcast ARP request for the SLIRP gateway
//! `10.0.2.2`, and receives SLIRP's ARP reply - a genuine TX-out / RX-in round
//! trip through the virtqueues. The cell exits `0x42` only on a well-formed reply
//! from `10.0.2.2`, so the exit code is the proof.
//!
//! The transport differs per machine: virtio-mmio on the riscv/arm `virt`
//! machines, virtio-pci on x86-64 q35 (driven through PCI config space). The
//! probe tries both, so all three ISAs exercise the same NIC path. The skip
//! branch fires only if no virtio-net device is attached at all.
//!
//! Wiring mirrors `librheodata` (queue pair + minted cap + `set_queue_info`); the
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

static DEMO: &[u8] = fixture::cell!("librheo-net");

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("librheonet: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    // Discover and install the virtio-net NIC.
    let dev = match virtio_net::probe() {
        Some(d) => d,
        None => {
            println!("librheonet: no virtio-net device attached - skipping");
            println!("librheonet: PASS");
            arch::exit(arch::ExitCode::Success)
        }
    };
    let m = dev.mac();
    println!(
        "librheonet: virtio-net found, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    );
    virtio_net::install(dev);

    svc::init();
    svc::set_file_ops(console_personality::console_and_empty_fs());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(DEMO, "librheo-net") };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "librheo-net exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!("librheonet: raw-frame ARP round trip over virtio-net, exit {code:#x} OK");
        }
        Outcome::Faulted(addr) => panic!("librheo-net faulted at {addr:#x}"),
    }

    // The NIC status pane (docs/OBSERVABILITY.md 11, S5): the round trip the cell
    // just performed must be visible as counter movement through the published
    // block - an ARP request went out and a reply was taken off the queue, so an
    // unrefreshed pane says tick 0 and a refreshed one carries both directions.
    // A minimum ARP frame is 28 bytes of payload; asserting >= 28 rather than an
    // exact length keeps SLIRP's padding out of the oracle.
    let pane = kernel::obs::net_pane();
    assert_eq!(
        pane.refreshed_tick, 0,
        "the NIC pane moved before anyone refreshed it - a driver is keeping a mirror warm"
    );
    kernel::obs::net_refresh();
    assert_eq!(
        pane.present, 1,
        "a NIC is installed and the pane says otherwise"
    );
    assert!(
        pane.tx_frames >= 1 && pane.tx_bytes >= 28,
        "the cell transmitted an ARP request and the pane counted {} frame(s) / {} byte(s)",
        pane.tx_frames,
        pane.tx_bytes
    );
    assert!(
        pane.rx_frames >= 1 && pane.rx_bytes >= 28,
        "the cell received an ARP reply and the pane counted {} frame(s) / {} byte(s)",
        pane.rx_frames,
        pane.rx_bytes
    );
    assert!(pane.refreshed_tick > 0, "the refresh did not stamp when");
    println!(
        "librheonet: NIC pane after refresh - tx {}f/{}B rx {}f/{}B, irqs {}, \
         interrupt_driven={} OK",
        pane.tx_frames,
        pane.tx_bytes,
        pane.rx_frames,
        pane.rx_bytes,
        pane.rx_irqs,
        pane.interrupt_driven
    );

    println!("librheonet: PASS");
    arch::exit(arch::ExitCode::Success)
}
