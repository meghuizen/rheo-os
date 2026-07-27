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
use kernel::load;
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
static YIELDX: &[u8] = fixture::linux!("yieldx");
static STACKX: &[u8] = fixture::linux!("stackx");
static SYSX: &[u8] = fixture::linux!("sysx");
static MMAPDP: &[u8] = fixture::linux!("mmapdp");
static COWFORK: &[u8] = fixture::linux!("cowfork");
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
    // `execve` loads its image from the VFS deep inside a syscall, where a test can
    // measure nothing directly - so the loader counts what it records and what it
    // copies, and the ratio across this phase is the witness. `procdemo` execve's
    // `/bin/cecho`, so a lazy exec records most of that image's pages.
    let rec_before = load::recorded_pages();
    let eager_before = load::eager_pages();
    let outcome = run(PROCDEMO, &[b"procdemo"]);
    linux::set_stdout_tap(None);
    let (rec, eager) = (
        load::recorded_pages() - rec_before,
        load::eager_pages() - eager_before,
    );
    assert!(
        rec > eager,
        "linuxproc: this phase recorded {rec} image page(s) and copied {eager} - an \
         execve that streamed its image eagerly would be the other way round"
    );

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
    println!(
        "linuxproc: procdemo OK (fork+execve+wait4+pipe2); the two loads left {rec} \
         image page(s) to demand paging and copied {eager}"
    );

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

    // --- cross-process `kill` + `/proc/self/exe` (docs/ARCHITECTURE-DEBT.md 4).
    //
    // `kill` refused any pid but the caller's own with ESRCH, and answered
    // `kill(0)`/`kill(-1)` by silently delivering to the *caller* - "signal my
    // children" reported as done and delivered to the wrong process. Subprocess
    // management is the whole job of the program this personality exists to run.
    //
    // Two of the fixture's five kill phases discriminate; the other three passed
    // with the fix reverted, because the old stub happened to give the same
    // answers, and that is recorded in the fixture. The two that do: signalling a
    // live child (the delivery lands on a process that is not running, so it is
    // recorded pending and delivered by the scheduler when it switches in - the
    // only moment the target's stack and frame are reachable), and `kill(-1)`
    // sparing the top of the tree, which stands in for init.
    //
    // `readlinkat` was a hardcoded `-ENOENT` for every path, `/proc/self/exe`
    // included - the one
    // link real programs actually read, to re-exec themselves or to find
    // resources beside their own binary.
    //
    // This lands here because it needs `execve`: the path is recorded when a cell
    // execs, and a cell the test kernel loaded directly never named one - the
    // kernel answers `-ENOENT` there rather than inventing a path. So the fixture
    // execve's itself and the re-exec'd process reads the link.
    fs::write("/bin/killx", KILLX).expect("seed /bin/killx");
    let want_kill: &[u8] = b"kill: self probe ok, absent ESRCH, unknown group ESRCH\n\
        kill: child signalled, handler ran, reaped pid gone\n\
        kill: -1 spared init, ESRCH with no other process\n\
        exe: /bin/killx\n\
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
        "linuxproc: cross-process kill OK - a live child is probed and signalled, \
         its handler runs, the reaped pid is gone, and kill(-1) spares the top of \
         the tree instead of self-targeting; /proc/self/exe resolves to the \
         execve'd path, a real non-link reports EINVAL, an absent path ENOENT"
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
        wx: mmap PROT_WRITE|PROT_EXEC EPERM\n\
        wx: RW->RX flip works, mprotect to RWX EPERM\n\
        vma: freed span reused at the same address, and writable\n\
        vma: partial unmap split the mapping, both ends intact, hole reused\n\
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
         than the region is ENOMEM instead of running into ld.so, MAP_FIXED \
         over the cell's queue region is EINVAL, and W^X is honest - RWX is \
         EPERM rather than a success that silently drops EXEC, while the RW->RX \
         flip a JIT falls back to works"
    );

    // --- `sched_yield` crosses processes (docs/ARCHITECTURE-DEBT.md 4). The
    // scheduler here is cooperative, so a yield is one of its few preemption
    // points - and it only rescheduled among a cell's own L4 contexts. A
    // single-threaded process had no ready sibling, so the call returned
    // immediately: a forked child looping `sched_yield()` ran to completion
    // before its parent was scheduled at all.
    //
    // The witness is an ordering record neither side can fake. Parent and child
    // run the *identical* loop - write one marker byte to the same pipe, yield,
    // eight times - and a pipe is one cross-cell ring (L6), so the byte order in
    // the ring is the interleaving. `fork` returns into the parent first, so the
    // oracle is "PC" x 8. Pre-fix the parent's yields did nothing, so it wrote
    // all eight P's before blocking in wait4: "PPPPPPPPCCCCCCCC". The two differ
    // at the first transition, which is what makes this discriminating.
    let want_yield: &[u8] = b"yield: parent and child alternated PCPCPCPCPCPCPCPC\n\
        yieldx OK\n";
    let (code, out) = run_capture(YIELDX, &[b"yieldx"]);
    assert!(
        out == want_yield,
        "yieldx: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
        core::str::from_utf8(out),
        core::str::from_utf8(want_yield),
    );
    assert!(code == 0, "yieldx: exit {code}, expected 0");
    println!(
        "linuxproc: sched_yield OK - a forked child and its parent alternate \
         round for round, so a yield reaches the next runnable process and not \
         only a sibling context"
    );

    // --- the stack is sized from the image, not from a constant
    // (docs/ARCHITECTURE-DEBT.md 4.0, blocker 1). The loader ignored
    // `PT_GNU_STACK` and gave every Linux cell the same 8 MiB, so a binary that
    // recorded a larger request silently got less and overran - the fault landing
    // wherever the recursion happened to be deep enough, far from the cause.
    //
    // The fixture is linked `-Wl,-z,stacksize=12582912` (12 MiB, see xtask), so
    // its own header asks for more than the old default. Two things are asserted
    // in one run because either alone can pass while lying: `RLIMIT_STACK`
    // *reports* the larger size (glibc sizes thread stacks from that number), and
    // the stack is genuinely *there* - 9280 KiB of it is touched in 64 KiB frames,
    // which is above the old 8 MiB and inside the 12 MiB request.
    let want_stack: &[u8] = b"stack: RLIMIT_STACK covers the PT_GNU_STACK request\n\
        stack: touched 9280 KiB of stack in 145 frames\n\
        stackx OK\n";
    let (code, out) = run_capture(STACKX, &[b"stackx"]);
    assert!(
        out == want_stack,
        "stackx: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
        core::str::from_utf8(out),
        core::str::from_utf8(want_stack),
    );
    assert!(code == 0, "stackx: exit {code}, expected 0");
    println!(
        "linuxproc: PT_GNU_STACK OK - an image asking for 12 MiB of stack is \
         given 12 MiB and told 12 MiB, and 9280 KiB of it is actually written \
         through - past the old fixed 8 MiB default, which faulted here"
    );

    // --- the seven syscalls the real Claude Code binary issues that the
    // personality did not dispatch (docs/ARCHITECTURE-DEBT.md 4.0, blocker 3).
    // Measured from its startup `strace`, not guessed. Six are advisory; the
    // seventh, `eventfd2`, is the epoll event loop's only wakeup path, so
    // refusing it does not degrade the program - it removes the mechanism.
    //
    // Each refusal is asserted as a refusal. `sched_setscheduler(SCHED_FIFO)`
    // returning 0 would tell a program it had real-time scheduling on a
    // cooperative scheduler, which is the stub-that-reports-success class this
    // programme is removing (docs/ENGINEERING.md 7).
    //
    // The legacy-`open` line differs by ISA, deliberately: syscall 2 exists only
    // on the x86-64 table, and that ISA-only existence *was* the defect - glibc
    // issues `open` in preference to `openat` there, so a personality with only
    // `openat` refused every `open` on one ISA and nowhere else.
    let _ = sys::mkdir("/etc"); // an existing /etc is fine
    fs::write("/etc/sysx.txt", b"sysx").expect("seed /etc/sysx.txt");
    let open_line: &[u8] = if cfg!(target_arch = "x86_64") {
        b"open: legacy open(2) works\n"
    } else {
        b"open: no legacy open on this ABI (openat only)\n"
    };
    let want_rest: &[u8] =
        b"eventfd: empty not readable, 1+6 read as 7, drained, short read EINVAL\n\
        eventfd: dup shares the counter\n\
        eventfd: semaphore mode yields 1 per read\n\
        sysinfo: real totals, free <= total, mem_unit 1, procs >= 1\n\
        sched: SCHED_OTHER ok, SCHED_FIFO EPERM, range 0..0\n\
        close_range: closed the range and nothing beyond it\n\
        clone3/rseq: refused ENOSYS deliberately\n\
        sysx OK\n";
    let (code, out) = run_capture(SYSX, &[b"sysx"]);
    assert!(
        out.starts_with(open_line) && &out[open_line.len()..] == want_rest,
        "sysx: stdout mismatch\n  got:      {:?}\n  expected: {:?} then {:?}",
        core::str::from_utf8(out),
        core::str::from_utf8(open_line),
        core::str::from_utf8(want_rest),
    );
    assert!(code == 0, "sysx: exit {code}, expected 0");
    println!(
        "linuxproc: measured-syscall set OK - eventfd2 is a real shared counter \
         (empty is NOT pollable-readable, a dup shares it, EFD_SEMAPHORE \
         decrements), sysinfo reports the real frame pool, sched_setscheduler \
         accepts the policy in force and refuses real-time with EPERM, \
         close_range closes exactly its range, and clone3/rseq are refused \
         deliberately rather than falling through the unknown-number path"
    );

    // --- a file mapping costs what is touched, not what is reserved
    // (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2). `mmap` of a file used to read
    // every page into a fresh frame before returning. The kernel-side oracle is the
    // one the program cannot fake: how many pages **demand paging** actually
    // filled. The fixture maps 64 and touches 3, so the answer is 3.
    //
    // The re-read phase matters as much as the count: a handler that could not tell
    // "the page is absent" from "the page is present and the access was refused"
    // would repopulate on every touch, and the count would run away.
    let faults_before = linux::mem::faults();
    let mmap_faults_before = linux::mem::faults_mmap();
    let files_before = linux::filemap::in_use();
    let want_dp: &[u8] = b"dp: backing file written\n\
        dp: mapped 64 pages, fd closed\n\
        dp: pages 0, 37 and 63 read the right bytes\n\
        dp: 100 rereads of a filled page cost nothing\n\
        dp: writing a filled read-only page is SIGSEGV, not a refill\n\
        dp: a page still faults from the file after a forked sharer exited\n\
        dp: mprotect RW then a private write works\n\
        mmapdp OK\n";
    let (code, out) = run_capture(MMAPDP, &[b"mmapdp"]);
    assert!(
        out == want_dp,
        "mmapdp: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
        core::str::from_utf8(out),
        core::str::from_utf8(want_dp),
    );
    assert!(code == 0, "mmapdp: exit {code}, expected 0");
    // The oracle is the **mmap-region** count, not the total. The property being
    // proven is "this 64-page file mapping cost 5 fills", and the total stops
    // measuring that the moment anything else in the address space is demand-paged
    // too - it would then also be counting the program's own text and data, which is
    // a property of the loader, not of this mapping. Which region a page lies in is
    // kernel layout, so the cell cannot fake it either way.
    let filled = linux::mem::faults_mmap() - mmap_faults_before;
    assert!(
        filled == 5,
        "mmapdp: demand paging filled {filled} pages of the mmap region, want \
         exactly 5 (64 were mapped, 5 touched) - an eager mmap would be 64 and a \
         handler that repopulated a present page would be far more"
    );
    // The mapping owned a VFS handle across the caller's `close(fd)` and gave it
    // back at `munmap`: the registry is where it started.
    assert!(
        linux::filemap::in_use() == files_before,
        "mmapdp: the mapped-file registry did not return to {files_before} entries \
         ({} in use) - a mapping leaked its handle",
        linux::filemap::in_use()
    );
    println!(
        "linuxproc: mmapdp filled {filled} of 64 mapped pages in the mmap region, \
         {} demand fills across the whole address space",
        linux::mem::faults() - faults_before
    );
    println!(
        "linuxproc: demand paging OK - 64 file pages mapped, exactly {filled} filled \
         by fault (an eager mmap read all 64), pages 0/37/63 carry their own file \
         bytes so the offset arithmetic holds at the top of the mapping, 100 rereads \
         of a filled page cost nothing, a page still fills from the file after a \
         forked sharer exited (the fork takes a backing-store reference and the exit \
         gives it back), and the mapping outlived close(fd) then returned its handle"
    );

    // --- a fork must share pages, not copy them (docs/ARCHITECTURE-DEBT.md 4.0,
    // blocker 2). The fixture proves the *semantics* - three isolation properties
    // that each catch a different mistake, including the parent-write-protect half
    // that produces wrong values rather than a fault. The oracle here is the
    // *saving*: how many frames the pool lost across a fork of a process holding a
    // 1 MiB dirty heap. An eager fork paid for all of it.
    let cow_before = kernel::mm::cow_faults();
    let fork_before = kernel::mm::fork_pages();
    let fork_frames_before = kernel::mm::fork_frames();
    let want_cow: &[u8] = b"cow: 256 pages dirtied\n\
        cow: parent and child are isolated after a shared fork\n\
        cowfork OK\n";
    let (code, out) = run_capture(COWFORK, &[b"cowfork"]);
    assert!(
        out == want_cow,
        "cowfork: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
        core::str::from_utf8(out),
        core::str::from_utf8(want_cow),
    );
    assert!(code == 0, "cowfork: exit {code}, expected 0");
    let cow_breaks = kernel::mm::cow_faults() - cow_before;
    let (shared, copied) = {
        let now = kernel::mm::fork_pages();
        (now.0 - fork_before.0, now.1 - fork_before.1)
    };
    let fork_cost = kernel::mm::fork_frames() - fork_frames_before;
    // The property, stated as the kernel measured it rather than as a pool delta:
    // **every** page was shared and **none** copied. A pool delta around the whole
    // run cannot say this - it also counts the fixture's 8 MiB stack and its heap, and
    // the first version of this assertion reported 2431 frames for a fork that had in
    // fact copied nothing at all (docs/ENGINEERING.md 11).
    assert!(
        copied == 0 && shared >= 256,
        "cowfork: the fork shared {shared} page(s) and copied {copied} - it must share \
         every page of a 256-page dirty heap and copy none"
    );
    // What the fork actually cost: the child's page tables, and nothing else. An eager
    // fork of this process paid `shared` frames on top.
    assert!(
        fork_cost < shared / 8,
        "cowfork: the fork consumed {fork_cost} frame(s) to share {shared} page(s) - \
         that is not page tables, something is still copying"
    );
    // And the sharing was genuinely *broken* on write, rather than never established:
    // both sides wrote, so their pages each had to be privated.
    assert!(
        cow_breaks >= 5,
        "cowfork: only {cow_breaks} copy-on-write break(s) - the writes on both sides \
         should each have privated a page"
    );
    println!(
        "linuxproc: COW fork OK - the fork shared {shared} page(s), copied {copied}, \
         and consumed {fork_cost} frame(s) of page tables; {cow_breaks} page(s) were \
         privated on write and parent and child stayed isolated in both directions \
         (an eager fork would have copied all {shared})"
    );

    // The pre-fault path's cost, measured rather than assumed (docs/ENGINEERING.md 1).
    // Presence is ensured only on the helpers that hand back something to
    // dereference; putting it on the bare range checks instead cost a ~2,900x
    // amplification, because `unmap_range` bounds a range with the same predicate and
    // would materialise every page in it just before freeing it.
    println!(
        "linuxproc: kernel pre-faulted {} page(s) across this run ({} demand fills \
         total, {} of them in the mmap region)",
        kernel::user::prefaults(),
        linux::mem::faults(),
        linux::mem::faults_mmap()
    );

    println!("linuxproc: PASS");
    arch::exit(arch::ExitCode::Success)
}
