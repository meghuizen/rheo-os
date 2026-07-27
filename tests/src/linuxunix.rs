//! In-QEMU test kernel for Linux-personality milestone **L8** (docs/LINUX-COMPAT.md
//! L8): **AF_UNIX (Unix domain) sockets**. An unmodified static-glibc C program
//! (`af_unix`) runs as a `Personality::Linux` cell and exercises Unix domain
//! sockets two ways:
//!
//!   1. `socketpair(AF_UNIX, SOCK_STREAM)` + `fork` - the parent and the forked
//!      child (two cells) send + recv "ping"/"pong" in both directions over the
//!      two direction rings (the L6 cross-cell ring machinery);
//!   2. `socket`/`bind`/`listen`/`connect`/`accept` over an **abstract** name -
//!      a single-process loopback that sends + recvs "hello"/"world".
//!
//! Exact stdout + exit are asserted on all three ISAs, proving AF_UNIX end to end.
//! No kernel object is added: sockets are per-cell fds and the byte transport is
//! the L6 ring buffer (`kernel/src/linux/unixsock.rs`). The fixture is built from
//! source by `cargo xtask` (`build_linux_fixtures`); no binary lives in git.

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

/// The static-glibc AF_UNIX fixture (built by `xtask::build_linux_fixtures`).
static AF_UNIX: &[u8] = fixture::linux!("af_unix");

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
    println!("linuxunix: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 8 * 1024 * 1024);
    }

    // A ramfs at / + the VFS personality handler (fd 1/2 reach the console
    // through the personality; the fixture opens no files, but glibc startup
    // wants a working VFS surface).
    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    svc::set_file_ops(vfs_personality::ops());

    println!("linuxunix: af_unix={} bytes", AF_UNIX.len());

    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run(AF_UNIX, &[b"af_unix"]);
    linux::set_stdout_tap(None);

    let want_out = b"pair: pong\nconn: hello\nback: world\naf_unix OK\n";
    match outcome {
        Outcome::Exited(code) => {
            assert!(code == 0, "af_unix: exit {code}, expected 0");
            let got = captured();
            assert!(
                got == want_out,
                "af_unix: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_out),
            );
        }
        Outcome::Faulted(addr) => panic!("af_unix: faulted at {addr:#x}"),
    }
    println!("linuxunix: af_unix OK (socketpair+fork, bind/listen/connect/accept)");

    println!("linuxunix: PASS");
    arch::exit(arch::ExitCode::Success)
}
