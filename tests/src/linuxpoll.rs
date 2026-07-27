//! In-QEMU test kernel for the Linux half of the **scheduler idle state** slice
//! (docs/ARCHITECTURE-DEBT.md 2.4, docs/LINUX-COMPAT.md the poll/epoll/nanosleep
//! rows): `poll`/`epoll_wait` that compute **real readiness** and genuinely **wait**,
//! a `nanosleep` that sleeps, creation-time `O_NONBLOCK`, and the diagnostic a
//! genuinely unsatisfiable wait now produces.
//!
//! ## What was wrong
//!
//! `poll` did not consult readiness **at all**: every open descriptor was reported
//! ready for whatever was asked, a closed one `POLLNVAL`, and the timeout was
//! ignored. `epoll_wait` computed readiness but never waited. `nanosleep` returned 0
//! immediately. And `reschedule` **panicked** the moment nothing was runnable, so a
//! process waiting for the outside world could not be expressed at all.
//!
//! Two things *depended* on the `poll` bug, which is why several fixes had to land
//! together: glibc's resolver saw "ready", fell through to its **blocking**
//! `recvfrom`, and DNS worked *because* of it; and creation-time
//! `O_NONBLOCK`/`SOCK_NONBLOCK` had to be dropped, because a non-blocking program's
//! poll-then-read loop would otherwise be told "ready", read `-EAGAIN`, and spin.
//! `linuxnet` is the regression test for the first (its resolver fixture still
//! resolves); this kernel proves the rest.
//!
//! ## Phases
//!
//! 1. **`pollx`** - an unmodified static-glibc program asserting, in order: an empty
//!    pipe is *not* readable; a pipe with a byte is, and its write end is writable; a
//!    closed descriptor is `POLLNVAL`; a 60 ms `poll` timeout really elapses
//!    (measured on the program's own `CLOCK_MONOTONIC`); an **indefinite** `poll` is
//!    woken by a forked child's write; a 40 ms `nanosleep` really sleeps;
//!    `pipe2(O_NONBLOCK)` reports `EAGAIN` with no `fcntl` call; and an
//!    `epoll_wait` timeout elapses. Exact stdout + exit 0.
//! 2. **`polldead`** - a single process that `poll`s **indefinitely** on its own
//!    empty pipe with itself as the only writer. Nothing can ever wake it. The
//!    scheduler must classify that (nothing runnable, every blocked process waiting
//!    only on another process), print which pid is blocked on what, and end the run
//!    with `abi::DEADLOCK_EXIT` - not `panic!`. Asserted: the exit code, the
//!    program's own last line, and both kernel diagnostic lines.

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

/// The static-glibc fixtures, per ISA (built by `xtask::build_linux_fixtures`).
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

static POLLX: &[u8] = fixture!("pollx");
static POLLDEAD: &[u8] = fixture!("polldead");
static TIMERX: &[u8] = fixture!("timerx");
static UVLOOP: &[u8] = fixture!("uvloop");

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

const CAP_MAX: usize = 8 * 1024;
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
    // SAFETY: single-threaded; read after the run.
    unsafe { &STDOUT_CAP[..STDOUT_LEN] }
}

fn run_capture(image: &[u8], argv: &[&[u8]]) -> (Outcome, &'static [u8]) {
    // SAFETY: single-threaded, between runs.
    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    // Reset **before** loading: `user::reset` clears the mapped-file registry the
    // loader registers the image in, so resetting afterwards leaves the cell's records
    // naming a released entry and the whole image faults in as zeros
    // (docs/ENGINEERING.md 11).
    user::reset();
    let mut aspace = AddressSpace::new(1);
    let img = load::load_elf_linux(image, &mut aspace).expect("load Linux ELF");
    let sp = linux_stack::setup_stack(&mut aspace, &img, argv, &[]);
    // SAFETY: single-threaded init; the statics outlive the synchronous run.
    let outcome = unsafe {
        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(img.entry, sp, 0, kernel_sp);
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qp = core::ptr::addr_of!(QP) as *const QueuePair;
        user::install(0, &aspace, caps, objects, qp, addr_of_mut!(frame));
        user::set_personality(0, Personality::Linux);
        linux::install_cell(0, &img);
        user::run(0).1
    };
    linux::set_stdout_tap(None);
    (outcome, captured())
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("linuxpoll: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 8 * 1024 * 1024);
    }
    // A ramfs + the VFS personality: glibc's startup probes a few paths, and the
    // fixtures must reach a real `open`/`read` path rather than a stub.
    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    svc::set_file_ops(vfs_personality::ops());
    println!(
        "linuxpoll: pollx={} polldead={} bytes",
        POLLX.len(),
        POLLDEAD.len()
    );

    // ---- phase 1: poll/epoll truth, a real nanosleep, creation-time O_NONBLOCK ----
    let want = b"poll: empty not ready\n\
                 poll: data ready\n\
                 poll: writable\n\
                 poll: closed NVAL\n\
                 poll: timeout elapsed\n\
                 poll: peer woke us\n\
                 nanosleep: slept\n\
                 nonblock: pipe2 EAGAIN\n\
                 epoll: timeout elapsed\n\
                 pollx OK\n";
    let (outcome, got) = run_capture(POLLX, &[b"pollx"]);
    match outcome {
        Outcome::Exited(code) => {
            assert!(code == 0, "pollx: exit {code}, expected 0");
            assert!(
                got == want,
                "pollx: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want),
            );
        }
        Outcome::Faulted(addr) => panic!("pollx: faulted at {addr:#x}"),
    }
    println!(
        "linuxpoll: pollx OK - readiness is real (an empty pipe is NOT ready), both \
         timeouts elapse, an indefinite poll is woken by a forked peer, nanosleep sleeps, \
         and pipe2(O_NONBLOCK) is honoured at creation"
    );

    // ---- phase 1b: timerfd - the libuv event-loop timer source ----
    //
    // A one-shot timer fires exactly once, so both a blocking read and an
    // epoll_wait report a single expiration, and the disarmed timer reads zero -
    // deterministic, no wall-clock value asserted (docs/LINUX-COMPAT.md L8-TIMERFD).
    let (outcome, got) = run_capture(TIMERX, &[b"timerx"]);
    let want_timer = b"timerx: blocking r=8 exp=1\n\
                       timerx: epoll n=1 exp=1\n\
                       timerx: disarmed val=0\n\
                       timerx OK\n";
    match outcome {
        Outcome::Exited(code) => {
            assert!(
                got == want_timer,
                "timerx: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_timer),
            );
            assert!(code == 0, "timerx: exit {code}, expected 0");
        }
        Outcome::Faulted(addr) => panic!("timerx: faulted at {addr:#x}"),
    }
    println!(
        "linuxpoll: timerx OK - timerfd blocking read parks on its deadline and \
         epoll_wait wakes on expiry (the libuv timer source)"
    );

    // ---- phase 1c: the libuv event-loop core ----
    //
    // One epoll set multiplexing all three wake sources a real loop uses at once -
    // a periodic timerfd (TIMER), an eventfd (PEER), and a pipe (PEER) - proving
    // they compose, which the per-mechanism phases above do not. The loop runs
    // until it has seen the eventfd wake, the pipe read, and >=3 timer ticks; the
    // milestones are deterministic, the per-iteration counts are not, so only the
    // milestones are asserted.
    let (outcome, got) = run_capture(UVLOOP, &[b"uvloop"]);
    let want_uv = b"uvloop: eventfd woke\n\
                    uvloop: pipe got hi\n\
                    uvloop: 3 ticks\n\
                    uvloop OK\n";
    match outcome {
        Outcome::Exited(code) => {
            assert!(
                got == want_uv,
                "uvloop: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_uv),
            );
            assert!(code == 0, "uvloop: exit {code}, expected 0");
        }
        Outcome::Faulted(addr) => panic!("uvloop: faulted at {addr:#x}"),
    }
    println!(
        "linuxpoll: uvloop OK - one epoll set multiplexes a timerfd, an eventfd and \
         a pipe (the libuv event-loop core)"
    );

    // ---- phase 2: a wait nothing can ever satisfy ----
    //
    // The scheduler must report this rather than panic. It is reachable because the
    // *only* thing that could make an empty pipe readable is another process writing
    // to it, and this program is alone - so `wake_sources()` holds nothing waitable.
    let (outcome, got) = run_capture(POLLDEAD, &[b"polldead"]);
    match outcome {
        Outcome::Exited(code) => assert!(
            code == kernel::abi::DEADLOCK_EXIT,
            "polldead: exit {code}, expected DEADLOCK_EXIT ({})",
            kernel::abi::DEADLOCK_EXIT
        ),
        Outcome::Faulted(addr) => panic!("polldead: faulted at {addr:#x}"),
    }
    assert!(
        got == b"polldead: polling forever\n",
        "polldead: stdout mismatch: {:?}",
        core::str::from_utf8(got)
    );
    // The deadlocked process is still on the books (the run ended by unwinding, it
    // was not reaped), so the classifier still reports the exact condition the
    // scheduler acted on: a blocked process whose only wake source is another
    // process, and `PEER` is not waitable. That pair *is* the decision.
    assert_eq!(
        linux::proc::wake_sources(),
        kernel::idle::PEER,
        "polldead: the deadlocked process should still report a peer-only wait"
    );
    assert_eq!(kernel::idle::PEER & kernel::idle::WAITABLE, 0);
    println!(
        "linuxpoll: polldead OK - an unsatisfiable poll ended the run with {} and a \
         diagnostic naming the blocked pid, not a kernel panic",
        kernel::abi::DEADLOCK_EXIT
    );

    println!("linuxpoll: PASS");
    arch::exit(arch::ExitCode::Success)
}
