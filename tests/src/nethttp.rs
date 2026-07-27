//! In-QEMU test kernel for rheo-net Phase N5a (docs/NETSTACK.md §19): **HTTP/1.1
//! and HTTP/2**. A cell loaded from the `nethttp-demo` ELF (built with the `tls`
//! feature, so the HTTPS composition is available) proves the whole application
//! layer deterministically and **network-free**, exiting `0x42` only if every one
//! of the following passes - so the exit code is the proof, on all three ISAs:
//!
//! - the HTTP/1.1 codec, with the **zero-copy borrow asserted** (a parsed header
//!   value's pointer lies inside the input buffer);
//! - **22 request-smuggling / robustness shapes** each rejected with their own
//!   error (`Content-Length` + `Transfer-Encoding`, duplicate `Content-Length`,
//!   `5, 5`, `+5`, `0x5`, a non-`chunked` final coding, bare LF, `host : x`,
//!   obs-fold, a non-token name, a control byte in a value, an oversized header
//!   block, too many fields, a double space, `HTTP/2.0` on a 1.x line, ...), plus
//!   four chunked-framing rejections;
//! - the branchless **SWAR scan == scalar oracle** over 20,000 fuzz buffers;
//! - an HTTP/1.1 **client talking to our HTTP/1.1 server over real `net::tcp`**
//!   across the in-cell `VirtualLink`: POST with headers + body, a
//!   `Content-Length` response, a **chunked** response reassembled exactly, a
//!   **second request on the same connection** (keep-alive), and a **404**;
//! - **HPACK against the RFC 7541 Appendix C** known-answer vectors - C.1
//!   integers, C.2.1-C.2.4 representations, and the C.3 / C.4 request sequences
//!   (C.4 Huffman-coded) decoded to the RFC's header lists **and** re-encoded to
//!   the RFC's exact bytes, with the dynamic table sizes 55/57/110/164 checked;
//! - **HTTP/2** over the same TCP pair: preface + SETTINGS exchange, HEADERS+DATA
//!   on stream 1, a second concurrent stream, a **flow-control-gated body**
//!   released by a WINDOW_UPDATE, RST_STREAM, PING/PING-ACK, GOAWAY, and four
//!   protocol-error rejections;
//! - **HTTPS**: one HTTP/1.1 exchange through the N3b TLS 1.3 record layer, with
//!   **ALPN** negotiating `http/1.1` and `h2`.
//!
//! A **live** GET is skipped with a reason (SLIRP has no HTTP server) - the cell
//! prints why and never fakes a response.
//!
//! Pure compute - **no netdev, no NIC** - but the cell still gets a mapped queue
//! pair + minted cap (librheo's `_start` discovers it via `SYS_QUEUE_INFO`) and a
//! console `FileOps` so its `println!` (fd 1/2) reaches the serial line. Wiring
//! mirrors `nettls`.

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

static DEMO: &[u8] = fixture::cell!("nethttp-demo");

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("nethttp: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    svc::init();
    svc::set_file_ops(console_personality::console_and_empty_fs());

    // SAFETY: single-threaded init; the harness's statics outlive the run.
    let outcome = unsafe { harness::run_elf_cell(DEMO, "nethttp-demo") };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "nethttp-demo exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "nethttp: h1 codec+smuggling+chunked, h1 over TCP, RFC 7541 HPACK KAT, \
                 h2 flow control, HTTPS+ALPN proven, exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("nethttp-demo faulted at {addr:#x}"),
    }

    println!("nethttp: PASS");
    arch::exit(arch::ExitCode::Success)
}
