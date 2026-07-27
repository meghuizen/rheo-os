//! In-QEMU test kernel for the Linux-personality **L8-INET** milestone
//! (docs/LINUX-COMPAT.md L8-INET, docs/NETSTACK.md): **AF_INET / AF_INET6 sockets
//! over the loopback interface**. An unmodified static-glibc C program (`inet`)
//! runs as a `Personality::Linux` cell and exercises internet-domain sockets over
//! 127.0.0.1 / ::1:
//!
//!   1. TCP over 127.0.0.1 - `socket`/`bind`/`listen`/`accept` a server +
//!      `socket`/`connect` a client, exchanging "hello"/"world" both directions;
//!   2. a minimal **epoll** (`epoll_create1`/`epoll_ctl`/`epoll_wait`) reports the
//!      connected socket readable once the peer has written;
//!   3. UDP over 127.0.0.1 - `sendto`/`recvfrom` a datagram on a bound endpoint;
//!   4. TCP over ::1 (AF_INET6) - the same exchange, proving the `sockaddr_in6`
//!      path.
//!
//! Exact stdout + exit are asserted on all three ISAs. This is a **loopback**
//! proof: it is deterministic and network-free (no NIC), proving the INET socket
//! ABI; NIC-backed remote INET (the full `net::tcp` segment/RTO state machine over
//! virtio-net) is a later phase. No kernel object is added - INET sockets are
//! per-cell fds and the byte transport reuses the L6 ring buffer
//! (`kernel/src/linux/inetsock.rs`). The fixture is built from source by
//! `cargo xtask` (`build_linux_fixtures`); no binary lives in git.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use core::ptr::addr_of_mut;

use kernel::linux::{self};
use kernel::svc;
use kernel::user::Outcome;
use kernel::{arch, println};
use posix::{RamFs, mount};

#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;
#[path = "vfs_personality.rs"]
mod vfs_personality;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 8 * 1024 * 1024] = [0; 8 * 1024 * 1024];

/// The static-glibc INET fixture (built by `xtask::build_linux_fixtures`).
static INET: &[u8] = fixture::linux!("inet");

// -- stdout capture, wired to the Linux personality's stdout tap --
const CAP_MAX: usize = 4 * 1024;
static mut STDOUT_CAP: [u8; CAP_MAX] = [0; CAP_MAX];
static mut STDOUT_LEN: usize = 0;

fn tap(bytes: &[u8]) {
    // SAFETY: single-threaded; the tap runs only during a cell run.
    unsafe {
        for &b in bytes {
            if STDOUT_LEN < CAP_MAX {
                STDOUT_CAP[STDOUT_LEN] = b;
                STDOUT_LEN += 1;
            }
        }
    }
}

fn captured() -> &'static [u8] {
    unsafe { &STDOUT_CAP[..STDOUT_LEN] }
}

fn run(image: &[u8], argv: &[&[u8]]) -> Outcome {
    // SAFETY: single-threaded init; the harness's statics outlive the run.
    unsafe { harness::run_linux_cell(image, argv) }
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("linuxinet: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 8 * 1024 * 1024);
    }

    // A ramfs at / + the VFS personality handler (glibc startup wants a working
    // VFS surface; the fixture opens no files).
    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    svc::set_file_ops(vfs_personality::ops());

    println!("linuxinet: inet={} bytes", INET.len());

    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run(INET, &[b"inet"]);
    linux::set_stdout_tap(None);

    let want_out = b"tcp4: hello\nepoll: ready\ntcp4: world\nudp4: ping\ntcp6: hi\ninet OK\n";
    match outcome {
        Outcome::Exited(code) => {
            assert!(code == 0, "inet: exit {code}, expected 0");
            let got = captured();
            assert!(
                got == want_out,
                "inet: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_out),
            );
        }
        Outcome::Faulted(addr) => panic!("inet: faulted at {addr:#x}"),
    }
    println!("linuxinet: inet OK (TCP+UDP+epoll over 127.0.0.1, TCP over ::1)");

    println!("linuxinet: PASS");
    arch::exit(arch::ExitCode::Success)
}
