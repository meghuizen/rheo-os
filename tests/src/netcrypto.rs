//! In-QEMU test kernel for rheo-net Phase N3a (docs/NETSTACK.md §3): the crypto
//! primitive layer. A cell loaded from the `netcrypto-demo` ELF (built with the
//! `crypto` feature) runs **every crypto primitive against its published RFC/NIST
//! test vector** - ChaCha20-Poly1305 (RFC 8439), SHA-256/384 (NIST/RFC 6234),
//! HKDF (RFC 5869), X25519 (RFC 7748), Ed25519 (RFC 8032), AES-GCM (NIST/GCM-spec)
//! - plus decrypt round trips, tamper rejections, the two-randomness-class API,
//!   and the nonce-safe `SealingKey`. It exits `0x42` only if every check passes;
//!   this kernel asserts that code on all three ISAs. So the exit code is the proof.
//!
//! Pure compute - **no netdev, no NIC** - but the cell still gets a mapped queue
//! pair + minted cap (librheo's `_start` discovers it via `SYS_QUEUE_INFO`) and a
//! console `FileOps` so its `println!` (fd 1/2) reaches the serial line. Wiring
//! mirrors `netcore`/`netsmoltcp` minus the virtio-net discovery.

#![no_std]
#![no_main]

extern crate alloc;

use core::ptr::addr_of_mut;

use kernel::svc;
use kernel::user::Outcome;
use kernel::{arch, println};

#[path = "console_personality.rs"]
mod console_personality;
#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;

static DEMO: &[u8] = fixture::cell!("netcrypto-demo");

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("netcrypto: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    svc::init();
    svc::set_file_ops(console_personality::console_and_empty_fs());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(DEMO, "netcrypto-demo") };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "netcrypto-demo exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!("netcrypto: all crypto primitives vector-proven, exit {code:#x} OK");
        }
        Outcome::Faulted(addr) => panic!("netcrypto-demo faulted at {addr:#x}"),
    }

    println!("netcrypto: PASS");
    arch::exit(arch::ExitCode::Success)
}
