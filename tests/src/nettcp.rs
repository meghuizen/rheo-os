//! In-QEMU test kernel for rheo-net Phase N2a (docs/NETSTACK.md §11): the native
//! **TCP** state machine + the **timer wheel**. A cell loaded from the `nettcp-demo`
//! ELF runs two TCP endpoints in one cell connected by an in-cell virtual link and
//! drives the **full lifecycle deterministically** - the three-way handshake, a
//! known payload transferred both directions with correct seq/ack, a dropped
//! segment recovered by RTO retransmission, and a clean FIN/FIN-ACK teardown to
//! CLOSED/TIME-WAIT - plus the checksum + segment-encode oracles and the
//! timer-wheel multiplex over the reactor's single one-shot deadline. It exits
//! `0x42` only if every check passes, so the exit code is the proof.
//!
//! Unlike `netl4`/`netcore`, this needs **no netdev**: the proof is entirely
//! in-cell / cross-endpoint (the TCP philosophy of the deterministic traceroute/DNS
//! proofs - a live peer is not required). The kernel is untouched: `net::tcp` and
//! `net::timer` are portable userspace over the existing reactor + one-shot-timer
//! ABI. A minimal console `FileOps` backs the cell's `println!` (fd 1/2 -> serial).

#![no_std]
#![no_main]

extern crate alloc;

use core::ptr::addr_of_mut;

use kernel::svc::{self};
use kernel::user::Outcome;
use kernel::{arch, println};

#[path = "console_personality.rs"]
mod console_personality;
#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;

static DEMO: &[u8] = fixture::cell!("nettcp-demo");

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("nettcp: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    svc::init();
    svc::set_file_ops(console_personality::console_and_empty_fs());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(DEMO, "nettcp-demo") };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "nettcp-demo exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "nettcp: TCP handshake + bidirectional data + drop/RTO recovery + teardown \
                 + timer wheel, exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("nettcp-demo faulted at {addr:#x}"),
    }

    println!("nettcp: PASS");
    arch::exit(arch::ExitCode::Success)
}
