//! In-QEMU test kernel for Linux-personality milestone L6 (docs/LINUX-COMPAT.md
//! L6): **processes** - `fork`, `execve`, `wait4`, and cross-cell `pipe2`.
//!
//! A single unpatched static-glibc program (`procdemo`) runs as the top
//! `Personality::Linux` cell and exercises the whole chain: it creates a pipe,
//! `fork`s, the child redirects stdout to the pipe and `execve`s a second
//! static-glibc binary (`/bin/cecho`, served from the VFS), the parent drains
//! the pipe, `wait4`s the child, and prints a deterministic transcript. Exact
//! stdout + exit are asserted on all three ISAs, proving fork + execve + wait4
//! + pipes end-to-end.
//!
//! Fixtures are built from source by `cargo xtask` (`build_linux_fixtures`); no
//! binary lives in git. `cecho` is written into a ramfs so the child's
//! `execve` loads it through the real VFS path (`load::exec_elf_from_vfs`).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use core::ptr::addr_of_mut;

use kernel::linux::{self};
use kernel::svc;
use kernel::user::Outcome;
use kernel::{arch, println};
use posix::{RamFs, fs, mount, sys};

#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;
#[path = "vfs_personality.rs"]
mod vfs_personality;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 8 * 1024 * 1024] = [0; 8 * 1024 * 1024];

/// The static-glibc fixtures, per ISA (built by `xtask::build_linux_fixtures`).
static PROCDEMO: &[u8] = fixture::linux!("procdemo");
static CECHO: &[u8] = fixture::linux!("cecho");
static RSH: &[u8] = fixture::linux!("rsh");
static FCNTLX: &[u8] = fixture::linux!("fcntlx");
static KILLX: &[u8] = fixture::linux!("killx");
static MMAPX: &[u8] = fixture::linux!("mmapx");
static COREUTILS: &[u8] = fixture::linux!("cu/bin/coreutils");

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

/// Run `image` with `argv` under a fresh Linux cell, capturing stdout; returns
/// (exit code, captured bytes). Used by the P11 shell suite.
fn run_capture(image: &[u8], argv: &[&[u8]]) -> (u64, &'static [u8]) {
    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run(image, argv);
    linux::set_stdout_tap(None);
    let code = match outcome {
        Outcome::Exited(c) => c,
        Outcome::Faulted(addr) => panic!("P11 shell faulted at {addr:#x}"),
    };
    (code, captured())
}

fn run(image: &[u8], argv: &[&[u8]]) -> Outcome {
    // SAFETY: single-threaded init; the harness's statics outlive the run.
    unsafe { harness::run_linux_cell(image, argv) }
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("linuxproc: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 8 * 1024 * 1024);
    }

    // A ramfs at / holding the execve target at /bin/cecho, plus the VFS
    // personality handler so `open`/`read`/`lseek` reach it.
    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    sys::mkdir("/bin").expect("mkdir /bin");
    fs::write("/bin/cecho", CECHO).expect("seed /bin/cecho");
    svc::set_file_ops(vfs_personality::ops());

    println!(
        "linuxproc: procdemo={} cecho={} bytes",
        PROCDEMO.len(),
        CECHO.len()
    );

    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run(PROCDEMO, &[b"procdemo"]);
    linux::set_stdout_tap(None);

    let want_out = b"child said: hi there\nchild exit: 0\n";
    match outcome {
        Outcome::Exited(code) => {
            assert!(code == 7, "procdemo: exit {code}, expected 7");
            let got = captured();
            assert!(
                got == want_out,
                "procdemo: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_out),
            );
        }
        Outcome::Faulted(addr) => panic!("procdemo: faulted at {addr:#x}"),
    }
    println!("linuxproc: procdemo OK (fork+execve+wait4+pipe2)");

    // --- P11 gate (docs/POSIX-PERSONALITY.md 5): a shell running a coreutils
    // suite. `rsh` (a minimal static-glibc shell) forks/execve's the unpatched
    // uutils/coreutils multicall from the VFS, wiring pipes and && / ||. Each
    // entry is run as `rsh -c "<cmdline>"`; we compare exact stdout + exit and
    // MEASURE the pass rate against the >= 80% gate.
    fs::write("/bin/coreutils", COREUTILS).expect("seed /bin/coreutils");

    // (cmdline, expected exit, expected stdout). The suite exercises pipes,
    // && / ||, and a spread of utilities.
    let suite: &[(&[u8], u64, &[u8])] = &[
        (b"seq 1 5 | wc -l", 0, b"5\n"),
        (b"echo hi | cat", 0, b"hi\n"),
        (b"true && echo ok", 0, b"ok\n"),
        (b"false || echo rescued", 0, b"rescued\n"),
        (b"true && echo ok || echo no", 0, b"ok\n"),
        (b"false && echo no || echo yes", 0, b"yes\n"),
        (b"basename /a/b/c.txt", 0, b"c.txt\n"),
        (b"dirname /a/b/c.txt", 0, b"/a/b\n"),
        (b"seq 1 4 | head -n2", 0, b"1\n2\n"),
        (b"echo one two three | wc -w", 0, b"3\n"),
        (b"pwd", 0, b"/\n"),
        (b"echo hello | wc -c", 0, b"6\n"),
    ];

    let mut passed = 0usize;
    for &(cmd, want_exit, want_out) in suite {
        let (code, out) = run_capture(RSH, &[b"rsh", b"-c", cmd]);
        let ok = code == want_exit && out == want_out;
        if ok {
            passed += 1;
        }
        println!(
            "P11 [{}] '{}' -> exit {} out {:?}",
            if ok { "PASS" } else { "FAIL" },
            core::str::from_utf8(cmd).unwrap_or("?"),
            code,
            core::str::from_utf8(out),
        );
    }
    let total = suite.len();
    let pct = passed * 100 / total;
    println!("linuxproc: P11 coreutils suite {passed}/{total} = {pct}% (gate >= 80%)");
    assert!(pct >= 80, "P11 gate not met: {passed}/{total} = {pct}%");

    // --- `fcntl` honesty (docs/LINUX-COMPAT.md, the `fcntl` row). This lands
    // here because its last phase needs `execve`: the fixture marks one
    // descriptor FD_CLOEXEC, leaves another alone, and execve's ITSELF from the
    // VFS, so the child observes which descriptors survived. The earlier phases
    // are pure fd-table semantics (refused commands with distinct errnos,
    // O_NONBLOCK honoured, F_GETFL real).
    fs::write("/bin/fcntlx", FCNTLX).expect("seed /bin/fcntlx");
    let want_fcntl: &[u8] = b"fcntl: setlk ENOLCK\n\
        fcntl: badcmd EINVAL\n\
        fcntl: nonblock EAGAIN\n\
        fcntl: stdin EAGAIN\n\
        fcntl: getfl ok\n\
        fcntl: blocking read ok\n\
        fcntl: exec child\n\
        fcntl: cloexec closed\n\
        fcntl: plain survived\n\
        fcntl OK\n";
    let (code, out) = run_capture(FCNTLX, &[b"fcntlx"]);
    assert!(
        out == want_fcntl,
        "fcntlx: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
        core::str::from_utf8(out),
        core::str::from_utf8(want_fcntl),
    );
    assert!(code == 0, "fcntlx: exit {code}, expected 0");
    println!(
        "linuxproc: fcntl OK - F_SETLK -> ENOLCK, an unknown cmd -> EINVAL, \
         F_SETFL(O_NONBLOCK) honoured on a pipe and on stdin, F_GETFL real, \
         FD_CLOEXEC closed across execve while a plain fd survived"
    );

    // --- `/proc/self/exe` (docs/ARCHITECTURE-DEBT.md 4). `readlinkat` was a
    // hardcoded `-ENOENT` for every path, `/proc/self/exe` included - the one
    // link real programs actually read, to re-exec themselves or to find
    // resources beside their own binary.
    //
    // This lands here because it needs `execve`: the path is recorded when a cell
    // execs, and a cell the test kernel loaded directly never named one - the
    // kernel answers `-ENOENT` there rather than inventing a path. So the fixture
    // execve's itself and the re-exec'd process reads the link.
    fs::write("/bin/killx", KILLX).expect("seed /bin/killx");
    let want_kill: &[u8] = b"exe: /bin/killx\n\
        exe: non-link EINVAL, absent ENOENT\n\
        killx OK\n";
    let (code, out) = run_capture(KILLX, &[b"killx"]);
    assert!(
        out == want_kill,
        "killx: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
        core::str::from_utf8(out),
        core::str::from_utf8(want_kill),
    );
    assert!(code == 0, "killx: exit {code}, expected 0");
    println!(
        "linuxproc: /proc/self/exe OK - resolves to the execve'd path, a real \
         non-link reports EINVAL, an absent path ENOENT"
    );

    // --- the mmap region is bounded (docs/ARCHITECTURE-DEBT.md 4, blocker 2).
    // `mmap` is a forward bump cursor with no accounting and it used to be
    // *unbounded*, so a long enough run of allocations left the 12 GiB region,
    // crossed the cell's queue-pair region at 16 GiB and its channel regions at
    // 24 GiB, and reached the ELF interpreter at 64 GiB where ld.so and libc.so.6
    // live - handing a program addresses that alias its own dynamic linker, with
    // no error. 4 GiB of mappings is enough to get there, which a ~100 MB binary
    // reaches easily.
    //
    // The bound is the answer to the *failure mode*, not to placement: a real VMA
    // list with first-fit and reuse of freed spans is still open. What is asserted
    // is that an impossible request is refused with an errno the caller can act
    // on, that a caller-chosen MAP_FIXED cannot replace the kernel's own rings,
    // and that an ordinary mapping still works.
    let want_mmap: &[u8] = b"mmap: small anonymous mapping usable\n\
        mmap: oversized reservation ENOMEM\n\
        mmap: MAP_FIXED over the queue region EINVAL\n\
        mmapx OK\n";
    let (code, out) = run_capture(MMAPX, &[b"mmapx"]);
    assert!(
        out == want_mmap,
        "mmapx: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
        core::str::from_utf8(out),
        core::str::from_utf8(want_mmap),
    );
    assert!(code == 0, "mmapx: exit {code}, expected 0");
    println!(
        "linuxproc: mmap bound OK - an ordinary mapping works, a request larger \
         than the region is ENOMEM instead of running into ld.so, and MAP_FIXED \
         over the cell's queue region is EINVAL"
    );

    println!("linuxproc: PASS");
    arch::exit(arch::ExitCode::Success)
}
