//! In-QEMU test kernel for Linux-personality milestone L4 (docs/LINUX-COMPAT.md
//! L4): an **unpatched multi-threaded Rust `std` binary** runs as a
//! `Personality::Linux` cell with multiple execution contexts scheduled
//! cooperatively at syscall boundaries.
//!
//! The fixture (`tests/linux-fixtures/rustthreads`) spawns four `std::thread`s
//! that share an `Arc<AtomicUsize>`, a `Mutex<u64>`, and an `mpsc` channel,
//! then joins them - exercising clone + futex + per-thread TLS + the CHILD_-
//! CLEARTID join handshake end to end. Its output is scheduling-independent, so
//! the exact stdout and exit code are asserted on all three ISAs.
//!
//! Built static-glibc ET_EXEC by `cargo xtask` (`build_linux_fixtures`); no
//! binary lives in git. `include_bytes!`d below.

#![no_std]
#![no_main]

use kernel::ktimer;
use kernel::linux::{self};
use kernel::user::Outcome;
use kernel::{arch, println};

#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;

/// `include_bytes!` the static-glibc multi-threaded Rust fixture (L4), built by
/// `xtask::build_linux_fixtures` for the ISA's `*-unknown-linux-gnu` target.
static RUSTTHREADS: &[u8] = fixture::linux_cargo!("rustthreads");

/// The static-glibc `pthread_cond_timedwait` fixture (built by
/// `xtask::build_linux_fixtures`), for the futex-timeout phase below.
static CONDWAIT: &[u8] = fixture::linux!("condwait");

// -- stdout capture, wired to the Linux personality's stdout tap --
const CAP_MAX: usize = 8 * 1024;
static mut STDOUT_CAP: [u8; CAP_MAX] = [0; CAP_MAX];
static mut STDOUT_LEN: usize = 0;

fn tap(bytes: &[u8]) {
    // SAFETY: single-threaded; the tap is called only during a cell run.
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
    println!("linuxthreads: start on {}", arch::NAME);
    println!(
        "linuxthreads: loaded multi-threaded Rust std ({} bytes)",
        RUSTTHREADS.len()
    );

    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run(RUSTTHREADS, &[b"rustthreads"]);
    linux::set_stdout_tap(None);

    let want_out = b"threads 4 total 1550 channel 1550\n";
    match outcome {
        Outcome::Exited(code) => {
            assert!(code == 4, "rustthreads exited {code}, expected 4");
            let got = captured();
            assert!(
                got == want_out,
                "rustthreads stdout mismatch:\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_out),
            );
        }
        Outcome::Faulted(addr) => panic!("rustthreads faulted at {addr:#x}"),
    }

    println!("linuxthreads: OK (clone + futex + TLS + join)");

    // --- the futex **timeout** (docs/LINUX-COMPAT.md L4, the `futex` row).
    // `pthread_cond_timedwait` on a condvar nobody signals, in a process with no
    // other thread: the only thing that can end the wait is its own deadline. The
    // timeout used to be ignored, and with nothing else runnable the kernel
    // answered 0 ("you were woken"), so glibc looped forever.
    //
    // The kernel arms the deadline through the timer arbiter's own slot, so the
    // wait is a genuine halt where the timer interrupt is wired - enabled here,
    // opt-in as everywhere else, and reported rather than claimed.
    ktimer::reset();
    arch::enable_timer_irq();
    let parks_before = ktimer::parks();
    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run(CONDWAIT, &[b"condwait"]);
    linux::set_stdout_tap(None);
    let want_cond: &[u8] =
        b"condwait: realtime timedout\ncondwait: monotonic timedout\ncondwait OK\n";
    match outcome {
        Outcome::Exited(code) => {
            let got = captured();
            assert!(
                got == want_cond,
                "condwait stdout mismatch:\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_cond),
            );
            assert!(code == 0, "condwait exited {code}, expected 0");
        }
        Outcome::Faulted(addr) => panic!("condwait faulted at {addr:#x}"),
    }
    // Both waits must have gone through the arbiter's futex slot - evidence the
    // deadline was a registered deadline, not a spin that happened to end.
    let regs = ktimer::registrations(ktimer::TimerClient::FutexWait);
    assert!(
        regs > 0,
        "condwait timed out without ever registering a futex deadline with the timer \
         arbiter - the wait was not honoured the way it claims to be"
    );
    let parks = ktimer::parks() - parks_before;
    println!(
        "linuxthreads: futex timeout OK - two pthread_cond_timedwait waits on a \
         never-signalled condvar returned ETIMEDOUT no earlier than their own \
         deadlines ({regs} arbiter futex deadlines, {parks} genuine CPU halts; \
         0 halts means this ISA has no wired one-shot and the deadline was honoured \
         by comparison)"
    );
    assert!(
        kernel::linux::thread::deadlock_waits() == 0,
        "condwait hit the unsatisfiable-futex path - the timeout was not honoured"
    );

    println!("linuxthreads: PASS");
    arch::exit(arch::ExitCode::Success)
}
