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
use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{CapTable, ObjectTable};
use kernel::linux::{self, stack as linux_stack};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc;
use kernel::user::{self, Outcome, Personality};
use kernel::{arch, load, println};
use posix::{RamFs, mount};

#[path = "vfs_personality.rs"]
mod vfs_personality;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 8 * 1024 * 1024] = [0; 8 * 1024 * 1024];

/// The static-glibc INET fixture (built by `xtask::build_linux_fixtures`).
macro_rules! fixture {
    ($name:literal) => {{
        #[cfg(target_arch = "x86_64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/x86_64/",
                $name
            ))
        }
        #[cfg(target_arch = "aarch64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/aarch64/",
                $name
            ))
        }
        #[cfg(target_arch = "riscv64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/riscv64/",
                $name
            ))
        }
    }};
}

static INET: &[u8] = fixture!("inet");

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

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
    let mut aspace = AddressSpace::new(1);
    let img = load::load_elf_linux(image, &mut aspace).expect("load Linux ELF");
    let sp = linux_stack::setup_stack(&mut aspace, &img, argv, &[]);
    // SAFETY: single-threaded init; the statics outlive the synchronous run.
    unsafe {
        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(img.entry, sp, 0, kernel_sp);
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qp = core::ptr::addr_of!(QP) as *const QueuePair;
        user::reset();
        user::install(0, &aspace, caps, objects, qp, addr_of_mut!(frame));
        user::set_personality(0, Personality::Linux);
        linux::install_cell(0, img.image_end);
        user::run(0).1
    }
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
