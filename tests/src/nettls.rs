//! In-QEMU test kernel for rheo-net Phase N3b (docs/NETSTACK.md §15): the TLS 1.3
//! stack. A cell loaded from the `nettls-demo` ELF (built with the `tls` feature)
//! proves TLS 1.3 three ways - the **RFC 8448 §3 known-answer test** (the key
//! schedule derives the RFC's early/handshake/master secrets, client/server
//! handshake & application traffic secrets, write keys+IVs, and both Finished MACs
//! **byte-for-byte**), an **in-cell full 1-RTT handshake** (both cipher suites,
//! with a matching-key app-data round trip both ways + a tamper rejection), and a
//! **minimal X.509** parse + Ed25519 signature verify (pass) / tamper (reject). It
//! exits `0x42` only if every check passes; this kernel asserts that code on all
//! three ISAs. So the exit code is the proof.
//!
//! Pure compute - **no netdev, no NIC** - but the cell still gets a mapped queue
//! pair + minted cap (librheo's `_start` discovers it via `SYS_QUEUE_INFO`) and a
//! console `FileOps` so its `println!` (fd 1/2) reaches the serial line. Wiring
//! mirrors `netcrypto` minus the virtio-net discovery.

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

static DEMO: &[u8] = fixture::cell!("nettls-demo");

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("nettls: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    svc::init();
    svc::set_file_ops(console_personality::console_and_empty_fs());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(DEMO, "nettls-demo") };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "nettls-demo exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!("nettls: TLS 1.3 RFC 8448 KAT + handshake + X.509 proven, exit {code:#x} OK");
        }
        Outcome::Faulted(addr) => panic!("nettls-demo faulted at {addr:#x}"),
    }

    println!("nettls: PASS");
    arch::exit(arch::ExitCode::Success)
}
