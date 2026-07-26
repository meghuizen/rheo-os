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

use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{BUDGET_UNLIMITED, CapTable, ObjectKind, ObjectTable, READ, WRITE};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc::{self, FileOps};
use kernel::user::{self, Outcome};
use kernel::{arch, load, println};

#[cfg(target_arch = "x86_64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/x86_64-unknown-none/release/nethttp-demo"
));
#[cfg(target_arch = "aarch64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/nethttp-demo"
));
#[cfg(target_arch = "riscv64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/nethttp-demo"
));

const EXPECTED_EXIT: u64 = 0x42;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

// A console-only FileOps so the cell's `println!` (SYS_WRITE_FD on fd 1/2)
// reaches the serial line; every other file op is unused here.
fn con_open(_p: u64, _l: u64, _f: u64) -> i64 {
    -2
}
fn con_close(_fd: u64) -> i64 {
    0
}
fn con_read(_fd: u64, _b: u64, _l: u64) -> i64 {
    -9
}
fn con_write(fd: u64, buf_va: u64, len: u64) -> i64 {
    if fd == 1 || fd == 2 {
        let buf = unsafe { core::slice::from_raw_parts(buf_va as *const u8, len as usize) };
        for &b in buf {
            arch::serial_write_byte(b);
        }
        len as i64
    } else {
        -9
    }
}
fn con_lseek(_fd: u64, off: i64, _w: u64) -> i64 {
    off
}
fn con_stat(_p: u64, _l: u64, _s: u64) -> i64 {
    -38
}
fn con_fstat(_fd: u64, _s: u64) -> i64 {
    -38
}
fn con_getdents(_p: u64, _l: u64, _b: u64, _bl: u64) -> i64 {
    -38
}

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("nethttp: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    svc::init();
    svc::set_file_ops(FileOps {
        open: con_open,
        close: con_close,
        read: con_read,
        write: con_write,
        lseek: con_lseek,
        stat: con_stat,
        fstat: con_fstat,
        getdents: con_getdents,
    });

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(DEMO, &mut aspace).expect("load nethttp-demo ELF");
    let stack_top = load::map_stack(&mut aspace);
    let qp = load::map_queue(&mut aspace);

    // SAFETY: single-threaded init; the statics outlive the run.
    let outcome = unsafe {
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let object = objects.create(ObjectKind::QueuePair).unwrap();
        let cap = caps
            .mint(objects, object, READ | WRITE, BUDGET_UNLIMITED)
            .unwrap();
        let cap_id = cap.raw_low32();

        (*addr_of_mut!(QP)).write(qp);
        let qp_ptr = (*addr_of_mut!(QP)).as_ptr();

        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(entry, stack_top, 0, kernel_sp);
        user::reset();
        user::install(0, &aspace, caps, objects, qp_ptr, addr_of_mut!(frame));
        user::set_queue_info(0, load::USER_QUEUE_VA as u64, cap_id);
        user::run(0).1
    };

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
