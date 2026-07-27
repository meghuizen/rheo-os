//! Processes for the Linux personality (docs/LINUX-COMPAT.md L6): `fork`,
//! `execve`, `wait4`, and the cooperative multi-cell scheduler that ties them
//! together. Like the fd table, threads, and signals, this adds **no kernel
//! object** (docs/LINUX-COMPAT.md 1): PIDs, the parent/child tree, wait status,
//! and block/wake state are per-cell synthesized state here. `fork` is
//! "clone-cell-within-capability-bundle" (docs/POSIX-PERSONALITY.md 2) - a new
//! `user` cell in the parent's capability bundle, with the parent's committed
//! pages eager-copied (COW deferred) and its `LinuxState`/signal state deep
//! copied. Every underlying operation still goes through the cell's own grants.
//!
//! Scheduling generalizes the native cross-cell `SYS_SWITCH`: the process that
//! blocks (`wait4`, an empty/full pipe) or exits hands the CPU to the next
//! runnable cell via `user::switch_to_cell` (the same address-space switch),
//! and a blocked syscall is completed - its side effects applied and its return
//! value set - when the scheduler switches *into* that cell (its address space
//! active). Cooperative, single CPU: a cell yields only at a syscall boundary.
//! The native `run`/`SYS_SWITCH` path is untouched; all of this is behind the
//! `Personality::Linux` branch.

use crate::arch;
use crate::linux::errno::*;
use crate::linux::{Ctl, err, pipe, ret, signal, stack, thread};
use crate::load;
use crate::mm::AddressSpace;
use crate::user::{self, MAX_CELLS};
use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

/// What a parked process is waiting for (docs/LINUX-COMPAT.md L6). Completed by
/// `complete_block` when the scheduler switches into the cell.
#[derive(Copy, Clone)]
enum Block {
    None,
    /// `wait4`: parked until a child matching `want` (a pid, or <=0 for any)
    /// becomes a zombie. `wstatus_va` receives the encoded status.
    Wait {
        wstatus_va: u64,
        want: i64,
    },
    /// `read` on an empty pipe whose write ends are still open.
    PipeRead {
        buf_va: u64,
        count: u64,
        idx: usize,
    },
    /// `write` on a full pipe whose read ends are still open.
    PipeWrite {
        buf_va: u64,
        count: u64,
        idx: usize,
    },
    /// `nanosleep`/`clock_nanosleep`: parked until `deadline_ns` in the **cell's own
    /// clock domain** (`linux::cell_clock_ns`) - the domain the program's own
    /// `clock_gettime` reports, so a sleep of N ns is N ns *as the program measures
    /// it* (docs/ENGINEERING.md 11: clock domains are not interchangeable).
    Timer {
        deadline_ns: u64,
    },
    /// `read` on an empty console (stdin). Parked until a byte is buffered or input
    /// ends (docs/ARCHITECTURE-DEBT.md 2.4 - it used to answer 0, i.e. "end of
    /// input", which is a lie to every reader).
    Console {
        buf_va: u64,
        count: u64,
    },
    /// `poll`/`ppoll`: parked until one of the descriptors in the cell's copied poll
    /// set is ready, or `deadline_ns` (cell clock domain; 0 = indefinite) passes.
    Poll {
        fds_va: u64,
        nfds: usize,
        deadline_ns: u64,
        sources: crate::idle::Sources,
    },
    /// `epoll_wait`/`epoll_pwait`: as `Poll`, over the epoll instance's own watch
    /// list (which already lives in kernel state, so nothing is copied).
    Epoll {
        epfd: i64,
        events_va: u64,
        maxevents: usize,
        deadline_ns: u64,
        sources: crate::idle::Sources,
    },
}

/// Descriptors a single `poll` call may block on. A larger set keeps the
/// pre-existing non-blocking probe (documented): the array lives in the **cell's**
/// address space, which is not active while the scheduler judges satisfiability, so
/// the request has to be copied into kernel state - and the kernel is
/// allocation-free, so that copy is a fixed array.
pub const POLL_MAX: usize = 64;

/// One copied `pollfd` request (the `revents` field is recomputed at completion).
#[derive(Copy, Clone)]
struct PollReq {
    fd: i32,
    events: i16,
}

static mut POLLSET: [[PollReq; POLL_MAX]; MAX_CELLS] =
    [[PollReq { fd: -1, events: 0 }; POLL_MAX]; MAX_CELLS];

fn pollset(cell: usize) -> &'static mut [PollReq; POLL_MAX] {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut (*addr_of_mut!(POLLSET))[cell] }
}

#[derive(Copy, Clone, PartialEq)]
enum PState {
    Free,
    /// Runnable (running now, or waiting for its turn).
    Runnable,
    /// Parked (see `Block`).
    Blocked,
    /// Exited, awaiting `wait4` by the parent (holds the encoded status).
    Zombie,
}

#[derive(Copy, Clone)]
struct Proc {
    state: PState,
    block: Block,
    /// Parent cell index, or -1 for the top of the tree.
    parent: i32,
    pid: u32,
    /// Encoded wait status while `Zombie` (WIFEXITED/WIFSIGNALED form).
    wstatus: u32,
}

impl Proc {
    const fn free() -> Proc {
        Proc {
            state: PState::Free,
            block: Block::None,
            parent: -1,
            pid: 0,
            wstatus: 0,
        }
    }
}

static mut PROCS: [Proc; MAX_CELLS] = [const { Proc::free() }; MAX_CELLS];
/// Kernel-owned address spaces for forked/execve'd cells (the top cell's space
/// is test/loader-owned; a child's or a post-`execve` image's is owned here).
static mut ASPACE: [MaybeUninit<AddressSpace>; MAX_CELLS] =
    [const { MaybeUninit::uninit() }; MAX_CELLS];
static mut NEXT_PID: u32 = 1001;

fn procs() -> &'static mut [Proc; MAX_CELLS] {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut *addr_of_mut!(PROCS) }
}

fn next_pid() -> u32 {
    // SAFETY: single CPU.
    unsafe {
        let p = &mut *addr_of_mut!(NEXT_PID);
        let v = *p;
        *p += 1;
        v
    }
}

/// Clear all process state (called from `linux::reset`).
pub fn reset() {
    for p in procs().iter_mut() {
        *p = Proc::free();
    }
    // SAFETY: single CPU, between runs.
    unsafe {
        *addr_of_mut!(NEXT_PID) = 1001;
    }
}

/// Initialize the process entry for the top cell (called from
/// `linux::install_cell`): pid 1000, no parent, runnable.
pub fn init_top(cell: usize) {
    procs()[cell] = Proc {
        state: PState::Runnable,
        block: Block::None,
        parent: -1,
        pid: 1000,
        wstatus: 0,
    };
}

/// The synthesized pid of cell `cell` (`getpid`). 1000 for the top process,
/// 1001+ for forked children (docs/LINUX-COMPAT.md 3).
pub fn pid(cell: usize) -> u32 {
    procs()[cell].pid
}

/// The parent's pid (`getppid`), or 0 for the top of the tree.
pub fn ppid(cell: usize) -> u32 {
    let p = procs()[cell].parent;
    if p < 0 { 0 } else { procs()[p as usize].pid }
}

// ------------------------------------------------------------------- fork

// clone(2) flag: a shared address space (thread), vs a new one (fork).
const CLONE_VM: u64 = 0x0000_0100;

/// True if a `clone` with `flags` is `fork` (a new process), not
/// `pthread_create` (a new thread in the same address space). glibc's `fork`
/// issues `clone` without `CLONE_VM` on every ISA (there is no separate `fork`
/// number in the asm-generic table), so this is the portable discriminator.
pub fn is_fork(flags: u64) -> bool {
    flags & CLONE_VM == 0
}

/// `fork`: create a new cell in the parent's capability bundle with an eager
/// private copy of the parent's memory and a deep copy of its personality state
/// (docs/LINUX-COMPAT.md L6). Returns the child pid to the parent (which keeps
/// running); the child's frame is primed to return 0. `-EAGAIN` if the cell
/// table is full (the documented `MAX_CELLS` cap).
///
/// Reading `parent_frame` (the calling thread's saved state) is the point.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn fork(cur: usize, parent_frame: *mut TrapFrame) -> i64 {
    let Some(child) = (0..MAX_CELLS).find(|&i| procs()[i].state == PState::Free) else {
        return -EAGAIN;
    };

    // Eager-copy the parent's committed pages into a fresh address space.
    // SAFETY: the parent is the running cell; its address space pointer is valid.
    let parent_aspace = unsafe { &*user::cell_aspace(cur) };
    let child_aspace = parent_aspace.fork_from((child as u16) + 32);
    // SAFETY: single CPU; ASPACE[child] is free (the slot was Free).
    unsafe { (*addr_of_mut!(ASPACE))[child].write(child_aspace) };
    let aspace_ptr = unsafe { (*addr_of_mut!(ASPACE))[child].as_ptr() };

    // The child's context 0 is the calling thread's frame, returning 0.
    // SAFETY: `parent_frame` is the caller's valid saved frame.
    let mut cf = unsafe { *parent_frame };
    arch::set_syscall_ret(&mut cf, 0);
    let child_pid = next_pid();
    let fs = thread::current_fs_base(cur);
    let frame_ptr = thread::init_forked(child, child_pid, fs, 0, cf);

    // Install the child (shares the parent's capability bundle), then deep-copy
    // the fd table / brk / cwd / mmap bookkeeping and the signal dispositions.
    // SAFETY: pointers are kernel-owned statics that outlive the run.
    unsafe { user::install_forked(child, aspace_ptr, frame_ptr, cur) };
    super::dup_state(cur, child);
    signal::fork_copy(cur, child);

    procs()[child] = Proc {
        state: PState::Runnable,
        block: Block::None,
        parent: cur as i32,
        pid: child_pid,
        wstatus: 0,
    };
    child_pid as i64
}

use crate::arch::TrapFrame;

// ------------------------------------------------------------------ execve

/// Bounded kernel scratch for copying `execve`'s argv/envp strings out of the
/// old address space before it is torn down (docs/LINUX-COMPAT.md L6).
const EXEC_STR_MAX: usize = 16 * 1024;
const EXEC_PTR_MAX: usize = 64;
static mut EXEC_STR: [u8; EXEC_STR_MAX] = [0; EXEC_STR_MAX];
static mut EXEC_PATH: [u8; 512] = [0; 512];

/// `execve(path, argv, envp)`: replace the calling cell's image with a fresh
/// one streamed from the VFS, keeping the same cell/pid and (per POSIX) the fd
/// table and cwd; reset signal handlers to default and start single-threaded
/// (docs/LINUX-COMPAT.md L6). Does not return to the caller on success - it
/// resumes at the new entry via `Ctl::Switch`.
///
/// Reading the caller's `frame` (for its kernel SP) is the point of the call.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn execve(cur: usize, path_va: u64, argv_va: u64, envp_va: u64, frame: *mut TrapFrame) -> Ctl {
    let Some(ops) = crate::svc::file_ops() else {
        return err(ENOENT);
    };

    // Copy the path + argv + envp out of the (still-active) old address space
    // into kernel buffers, so the new stack can be built after the old image is
    // gone. Layout in EXEC_STR: argv strings then envp strings, each NUL-kept.
    let path_len = copy_path(path_va);
    if path_len == 0 {
        return err(ENOENT);
    }
    let mut argv: [&[u8]; EXEC_PTR_MAX] = [b""; EXEC_PTR_MAX];
    let mut envp: [&[u8]; EXEC_PTR_MAX] = [b""; EXEC_PTR_MAX];
    let mut off = 0usize;
    let argc = copy_str_array(argv_va, &mut argv, &mut off);
    let envc = copy_str_array(envp_va, &mut envp, &mut off);
    // execve with an empty argv still needs argv[0] for AT_EXECFN/the shell.
    let argc = argc.max(1);
    if argc == 1 && argv[0].is_empty() {
        // Default argv[0] to the path's basename bytes (kernel buffer).
        // SAFETY: EXEC_PATH holds `path_len` bytes.
        argv[0] =
            unsafe { core::slice::from_raw_parts(addr_of_mut!(EXEC_PATH) as *const u8, path_len) };
    }

    // Stream the new ELF into a fresh address space. The path is a kernel VA, so
    // `open` works regardless of which address space is active.
    let path_kva = addr_of_mut!(EXEC_PATH) as u64;
    let mut new_aspace = AddressSpace::new((cur as u16) + 48);
    let Some(img) = load::exec_elf_from_vfs(ops, path_kva, path_len as u64, &mut new_aspace) else {
        return err(ENOENT);
    };

    // Build the new initial stack (argv/envp/auxv) - written through the kernel
    // linear map, so the new space need not be active yet.
    let sp = stack::setup_stack(&mut new_aspace, &img, &argv[..argc], &envp[..envc]);

    // Reset the cell's personality state for the new image: keep fds + cwd, new
    // heap/mmap/auxv, default signal handlers, single thread.
    super::exec_reinit(cur, img.image_end);
    signal::exec_reset(cur);

    // Tear down the old image, install the new one, and resume at its entry.
    // SAFETY: the old address space pointer is valid; its user frames are no
    // longer needed (argv/envp were copied out, the new stack is built).
    unsafe { (*user::cell_aspace(cur)).free_user_frames() };
    let kernel_sp = unsafe { arch::trapframe_kernel_sp(&*frame) };
    let nf = arch::trapframe_new(img.entry, sp, 0, kernel_sp);
    let frame_ptr = thread::reset_after_exec(cur, pid(cur), nf);
    // SAFETY: single CPU; ASPACE[cur] outlives the run.
    unsafe { (*addr_of_mut!(ASPACE))[cur].write(new_aspace) };
    let aspace_ptr = unsafe { (*addr_of_mut!(ASPACE))[cur].as_ptr() };
    user::set_cell_aspace(cur, aspace_ptr);
    user::set_cell_frame(cur, frame_ptr);
    procs()[cur].block = Block::None;
    user::switch_to_cell(cur); // activate the new address space
    thread::restore_current(cur); // load the (zeroed) FP + fs_base of the new image
    Ctl::Switch(frame_ptr)
}

/// Copy the NUL-terminated path at user VA `va` into `EXEC_PATH`; returns its
/// length (0 on empty/oversized).
fn copy_path(va: u64) -> usize {
    if va == 0 {
        return 0;
    }
    // SAFETY: `va` is a C string in the active cell; bounded scan/copy.
    unsafe {
        let src = va as *const u8;
        let dst = addr_of_mut!(EXEC_PATH) as *mut u8;
        let mut n = 0usize;
        while n < 511 {
            let b = src.add(n).read();
            if b == 0 {
                break;
            }
            *dst.add(n) = b;
            n += 1;
        }
        *dst.add(n) = 0;
        n
    }
}

/// Copy a NULL-terminated C-string array at user VA `arr_va` into `EXEC_STR`
/// starting at `*off`, filling `out[i]` with a slice into `EXEC_STR`. Returns
/// the entry count (bounded by `EXEC_PTR_MAX` / `EXEC_STR_MAX`).
fn copy_str_array(arr_va: u64, out: &mut [&'static [u8]; EXEC_PTR_MAX], off: &mut usize) -> usize {
    if arr_va == 0 {
        return 0;
    }
    // Both the pointer array and every string it names are cell-supplied
    // addresses (docs/ENGINEERING.md 12): the array is bounded to the fixed
    // `EXEC_PTR_MAX` slots and range-checked, and each string's terminator scan
    // is bounded by how much of the cell's readable range remains at that
    // pointer. An out-of-range array or pointer stops the copy rather than
    // reading kernel memory.
    if crate::user::user_buf(arr_va, EXEC_PTR_MAX * 8).is_none() {
        return 0;
    }
    let mut count = 0usize;
    // SAFETY: `[arr_va, arr_va + EXEC_PTR_MAX*8)` was range-checked readable in
    // the active cell above, and each `src` span is checked below.
    unsafe {
        let base = addr_of_mut!(EXEC_STR) as *mut u8;
        for (i, slot) in out.iter_mut().enumerate() {
            let p = (arr_va as *const u64).add(i).read();
            if p == 0 {
                break;
            }
            let span = crate::user::user_read_span(p, EXEC_STR_MAX);
            if span == 0 {
                break;
            }
            let start = *off;
            let src = p as *const u8;
            let mut n = 0usize;
            while *off < EXEC_STR_MAX - 1 && n < span {
                let b = src.add(n).read();
                *base.add(*off) = b;
                *off += 1;
                if b == 0 {
                    break;
                }
                n += 1;
            }
            *slot = core::slice::from_raw_parts(base.add(start), n);
            count += 1;
        }
    }
    count
}

// -------------------------------------------------------------------- wait4

const WNOHANG: u64 = 1;

/// `wait4(pid, wstatus, options, rusage)`: reap a zombie child, or block until
/// one appears (docs/LINUX-COMPAT.md L6). `pid` <= 0 waits for any child; a
/// positive pid waits for that child. `rusage` is ignored.
pub fn wait4(cur: usize, upid: i64, wstatus_va: u64, options: u64) -> Ctl {
    let want = upid;
    if let Some(z) = find_zombie_child(cur, want) {
        return ret(reap(z, wstatus_va) as i64);
    }
    if !has_child(cur, want) {
        return err(ECHILD);
    }
    if options & WNOHANG != 0 {
        return ret(0);
    }
    procs()[cur].state = PState::Blocked;
    procs()[cur].block = Block::Wait { wstatus_va, want };
    reschedule(cur)
}

/// Reap zombie cell `z` into `wstatus_va` (in the active reaper's address
/// space); free the slot and return the reaped pid.
fn reap(z: usize, wstatus_va: u64) -> u32 {
    let pid = procs()[z].pid;
    if wstatus_va != 0 {
        // SAFETY: `wstatus_va` is a writable `int` in the active cell.
        unsafe { (wstatus_va as *mut i32).write(procs()[z].wstatus as i32) };
    }
    procs()[z] = Proc::free();
    user::free_cell(z);
    pid
}

fn find_zombie_child(cur: usize, want: i64) -> Option<usize> {
    (0..MAX_CELLS).find(|&i| {
        let p = &procs()[i];
        p.parent == cur as i32 && p.state == PState::Zombie && (want <= 0 || p.pid as i64 == want)
    })
}

fn has_child(cur: usize, want: i64) -> bool {
    (0..MAX_CELLS).any(|i| {
        let p = &procs()[i];
        p.parent == cur as i32 && p.state != PState::Free && (want <= 0 || p.pid as i64 == want)
    })
}

// ---------------------------------------------------------------- exit paths

/// A whole-process exit (`exit_group`, or the last thread's `exit`). For the top
/// cell this unwinds `run`; a forked child becomes a WIFEXITED zombie its parent
/// reaps, and the CPU is handed to the next runnable cell (docs/LINUX-COMPAT.md
/// L6).
pub fn exit_group(cell: usize, code: u64) -> Ctl {
    let status = ((code as u32) & 0xff) << 8; // WIFEXITED
    process_exit(cell, status, code)
}

/// Terminated by an uncaught fatal signal (WIFSIGNALED). For the top cell the
/// run ends reporting 128+signo (matching the L5 default-disposition behavior).
pub fn exit_signaled(cell: usize, signo: u32) -> Ctl {
    let status = signo & 0x7f; // WIFSIGNALED (WEXITSTATUS 0)
    process_exit(cell, status, 128 + signo as u64)
}

/// Shared exit: close fds (dropping pipe ends so peers see EOF/EPIPE), then
/// either unwind (top cell) or become a zombie and reschedule.
fn process_exit(cell: usize, status: u32, top_code: u64) -> Ctl {
    super::state(cell).fds.close_all();
    if cell == user::top_cell() {
        return Ctl::Exit(top_code);
    }
    // Reclaim the dead process's committed frames now (the status lives in the
    // zombie entry, not in user memory).
    // SAFETY: `cell`'s address space pointer is valid; it is torn down here and
    // never reactivated.
    unsafe { (*user::cell_aspace(cell)).free_user_frames() };
    procs()[cell].state = PState::Zombie;
    procs()[cell].wstatus = status;
    procs()[cell].block = Block::None;
    reschedule(cell)
}

// ------------------------------------------------------- pipe blocking (read)

/// True if some *other* present cell is runnable right now - so parking `cur`
/// on a pipe cannot deadlock. When false (e.g. a single-process cell holding
/// both ends), the caller returns -EAGAIN instead of blocking, matching the L3
/// non-blocking pipe (docs/LINUX-COMPAT.md L6).
pub fn runnable_peer_exists(cur: usize) -> bool {
    (0..MAX_CELLS)
        .any(|i| i != cur && user::cell_present(i) && procs()[i].state == PState::Runnable)
}

/// Park `cur` on an empty pipe read; the scheduler completes the read (with
/// `cur`'s address space active) when data arrives or all write ends close.
pub fn block_pipe_read(cur: usize, buf_va: u64, count: u64, idx: usize) -> Ctl {
    procs()[cur].state = PState::Blocked;
    procs()[cur].block = Block::PipeRead { buf_va, count, idx };
    reschedule(cur)
}

/// Park `cur` on a full pipe write; completed when space frees or read ends close.
pub fn block_pipe_write(cur: usize, buf_va: u64, count: u64, idx: usize) -> Ctl {
    procs()[cur].state = PState::Blocked;
    procs()[cur].block = Block::PipeWrite { buf_va, count, idx };
    reschedule(cur)
}

// ----------------------------------------------------------------- scheduler

/// Hand the CPU to the next runnable cell after `leaving` blocks or exits. Wakes
/// any blocked cell whose condition is now satisfiable, then round-robins to a
/// runnable cell and completes its pending block. Panics only on a true
/// deadlock (no runnable cell) - a scheduling bug, surfaced loudly rather than
/// hung.
fn reschedule(leaving: usize) -> Ctl {
    // Save the outgoing cell's live FP state (harmless if it is exiting).
    thread::save_current_fp(user::current_index());

    loop {
        // Wake blocked cells whose condition now holds.
        for i in 0..MAX_CELLS {
            if procs()[i].state == PState::Blocked && satisfiable(i) {
                procs()[i].state = PState::Runnable;
            }
        }

        let next = (1..=MAX_CELLS)
            .map(|k| (leaving + k) % MAX_CELLS)
            .find(|&i| user::cell_present(i) && procs()[i].state == PState::Runnable);
        if let Some(n) = next {
            user::switch_to_cell(n);
            thread::restore_current(n);
            complete_block(n);
            return Ctl::Switch(thread::current_frame(n));
        }

        // Nothing runnable. Idle on whatever the blocked cells are waiting for
        // instead of panicking (docs/ARCHITECTURE-DEBT.md 2.4): "every process is
        // waiting for the outside world" is a server's normal steady state, and
        // before this it was not an expressible one - which is what made a process
        // blocked awaiting a network reply, by definition the only runnable thing,
        // impossible.
        let src = blocked_sources();
        if src & crate::idle::WAITABLE == 0 {
            return report_deadlock(src);
        }
        // A cell-clock deadline cannot be armed on the hardware one-shot directly -
        // they are different counters on RISC-V - so an outstanding one is honoured
        // by bounded slices, each followed by a re-read of the cell clock. Exactly
        // the futex-timeout pattern (docs/LINUX-COMPAT.md, the `futex` row).
        if src & crate::idle::TIMER != 0 {
            crate::ktimer::register(crate::ktimer::TimerClient::CellSleep, SLEEP_SLICE_NS);
        }
        crate::idle::wait(src);
        if src & crate::idle::TIMER != 0 {
            crate::ktimer::cancel(crate::ktimer::TimerClient::CellSleep);
        }
    }
}

/// How long the scheduler halts before re-reading the cell clock for an outstanding
/// `nanosleep`/`poll` deadline. 1 ms: long enough that the halt is worth taking,
/// short enough that the overshoot past a deadline stays small. Same constant and
/// same reasoning as the futex timeout's park slice.
const SLEEP_SLICE_NS: u64 = 1_000_000;

/// The union of wake sources every blocked cell is waiting on ([`crate::idle`]).
fn blocked_sources() -> crate::idle::Sources {
    let mut src = 0;
    for i in 0..MAX_CELLS {
        if procs()[i].state == PState::Blocked {
            src |= sources_of(i);
        }
    }
    src
}

/// The wake sources cell `i`'s current block can be satisfied by.
fn sources_of(i: usize) -> crate::idle::Sources {
    use crate::idle;
    match procs()[i].block {
        Block::None => 0,
        // A pipe/socket ring or a child exit is another *process*'s doing.
        Block::Wait { .. } | Block::PipeRead { .. } | Block::PipeWrite { .. } => idle::PEER,
        Block::Timer { .. } => idle::TIMER,
        Block::Console { .. } => idle::CONSOLE,
        // Computed when the block was registered, from the descriptors it watches.
        Block::Poll { sources, .. } | Block::Epoll { sources, .. } => sources,
    }
}

/// The union of wake sources the Linux process scheduler is blocked on - the
/// classifier its idle/deadlock decision is made from, exposed so a test can assert
/// it directly (docs/ENGINEERING.md 1).
pub fn wake_sources() -> crate::idle::Sources {
    blocked_sources()
}

/// Nothing runnable and no blocked process has a wake source left: a genuine
/// deadlock. Name each blocked process and what it waits on, then end the run with
/// [`crate::abi::DEADLOCK_EXIT`] - a diagnostic instead of a kernel stack trace that
/// mentions no process (docs/ARCHITECTURE-DEBT.md 2.4).
fn report_deadlock(src: crate::idle::Sources) -> Ctl {
    crate::println!(
        "linux: DEADLOCK - no runnable process, no wake source (waiting on {})",
        crate::idle::describe(src)
    );
    for i in 0..MAX_CELLS {
        if procs()[i].state == PState::Blocked {
            crate::println!(
                "linux:   pid {} (cell {i}) blocked on {}",
                procs()[i].pid,
                block_name(i)
            );
        }
    }
    Ctl::Exit(crate::abi::DEADLOCK_EXIT)
}

/// The name of cell `i`'s block, for the deadlock diagnostic.
fn block_name(i: usize) -> &'static str {
    match procs()[i].block {
        Block::None => "nothing",
        Block::Wait { .. } => "wait4 (child exit)",
        Block::PipeRead { .. } => "read (empty pipe/socket)",
        Block::PipeWrite { .. } => "write (full pipe/socket)",
        Block::Timer { .. } => "nanosleep (deadline)",
        Block::Console { .. } => "read (console)",
        Block::Poll { .. } => "poll (fd readiness)",
        Block::Epoll { .. } => "epoll_wait (fd readiness)",
    }
}

/// Whether blocked cell `i`'s wait condition is now satisfiable.
fn satisfiable(i: usize) -> bool {
    match procs()[i].block {
        Block::Wait { want, .. } => find_zombie_child(i, want).is_some() || !has_child(i, want),
        Block::PipeRead { idx, .. } => pipe::has_data(idx) || pipe::writers(idx) == 0,
        Block::PipeWrite { idx, .. } => pipe::has_space(idx) || pipe::readers(idx) == 0,
        Block::Timer { deadline_ns } => super::cell_clock_ns(false) >= deadline_ns,
        Block::Console { .. } => crate::input::has_data() || crate::input::at_eof(),
        Block::Poll {
            nfds, deadline_ns, ..
        } => {
            poll_ready_count(i, nfds) > 0
                || (deadline_ns != 0 && super::cell_clock_ns(false) >= deadline_ns)
        }
        Block::Epoll {
            epfd, deadline_ns, ..
        } => {
            super::state(i).fds.epoll_ready(epfd) > 0
                || (deadline_ns != 0 && super::cell_clock_ns(false) >= deadline_ns)
        }
        Block::None => false,
    }
}

/// How many of cell `i`'s copied poll requests are ready right now. Computed
/// entirely from **kernel** state - the copied request array plus the cell's own fd
/// table - because the cell's address space is not active while the scheduler judges
/// this. Readiness for a remote (NIC-backed) socket pumps the datapath, which is
/// what lets a `poll` on a DNS socket become ready when the reply lands.
fn poll_ready_count(i: usize, nfds: usize) -> usize {
    let set = pollset(i);
    let st = super::state(i);
    let mut ready = 0;
    for r in set.iter().take(nfds.min(POLL_MAX)) {
        if r.fd < 0 {
            continue;
        }
        if super::poll_revents(&st.fds, r.fd as i64, r.events) != 0 {
            ready += 1;
        }
    }
    ready
}

/// Apply a woken cell's pending syscall (its address space is now active) and
/// set its return value, then clear the block.
fn complete_block(n: usize) {
    let block = procs()[n].block;
    procs()[n].block = Block::None;
    let frame = thread::current_frame(n);
    let r: i64 = match block {
        Block::None => return,
        Block::Wait { wstatus_va, want } => match find_zombie_child(n, want) {
            Some(z) => reap(z, wstatus_va) as i64,
            None => -ECHILD,
        },
        Block::PipeRead { buf_va, count, idx } => match pipe::read(idx, buf_va, count) {
            pipe::ReadNb::Done(n) => n,
            pipe::ReadNb::WouldBlock => 0, // woken only when satisfiable; treat as EOF
        },
        Block::PipeWrite { buf_va, count, idx } => match pipe::write(idx, buf_va, count) {
            pipe::WriteNb::Done(n) => n,
            pipe::WriteNb::WouldBlock => 0,
            pipe::WriteNb::Epipe => -EPIPE,
        },
        // A completed sleep returns 0 (the `rem` out-parameter is only written on an
        // interruption, and no signal can interrupt a sleep here - documented).
        Block::Timer { .. } => 0,
        // SAFETY: `buf_va`/`count` were bounded against **this** cell's user VA
        // range when the block was registered, and `n`'s address space is active
        // again here (`switch_to_cell` above).
        Block::Console { buf_va, count } => unsafe {
            crate::input::drain(buf_va, count as usize) as i64
        },
        Block::Poll { fds_va, nfds, .. } => write_poll_result(n, fds_va, nfds),
        Block::Epoll {
            epfd,
            events_va,
            maxevents,
            ..
        } => super::state(n).fds.epoll_wait(epfd, events_va, maxevents),
    };
    // SAFETY: `frame` is `n`'s current-context saved state.
    unsafe { arch::set_syscall_ret(&mut *frame, r as u64) };
}

/// Recompute `revents` for cell `n`'s copied poll set and write it back into the
/// caller's `pollfd` array (its address space is active), returning the ready count.
/// A timeout that expired with nothing ready writes all-zero `revents` and returns
/// 0, exactly as `poll(2)` specifies.
fn write_poll_result(n: usize, fds_va: u64, nfds: usize) -> i64 {
    let set = *pollset(n);
    let st = super::state(n);
    let mut ready = 0i64;
    for (k, r) in set.iter().take(nfds.min(POLL_MAX)).enumerate() {
        let revents = if r.fd < 0 {
            0
        } else {
            super::poll_revents(&st.fds, r.fd as i64, r.events)
        };
        // SAFETY: the array was bounded to `nfds` entries in this cell's user VA
        // range when the block was registered, and that space is active again.
        unsafe {
            let p = (fds_va as *mut i16).add(k * 4 + 3);
            p.write(revents);
        }
        if revents != 0 {
            ready += 1;
        }
    }
    ready
}

// ------------------------------------------------- the four Linux blocking waits
//
// docs/ARCHITECTURE-DEBT.md 2.4. Each registers its condition and hands the CPU on;
// `complete_block` finishes the syscall with the caller's address space active.
// `None` means "do not park" - the caller keeps its pre-existing behaviour, which is
// what makes each of these additive.

/// Park `cur` until `deadline_ns` (cell clock domain) - `nanosleep`.
pub fn block_timer(cur: usize, deadline_ns: u64) -> Ctl {
    procs()[cur].state = PState::Blocked;
    procs()[cur].block = Block::Timer { deadline_ns };
    reschedule(cur)
}

/// Park `cur` on an empty console read - blocking `stdin`.
pub fn block_console(cur: usize, buf_va: u64, count: u64) -> Ctl {
    procs()[cur].state = PState::Blocked;
    procs()[cur].block = Block::Console { buf_va, count };
    reschedule(cur)
}

/// Copy `cur`'s `poll` request set into kernel state and park on it. `None` if the
/// set is larger than [`POLL_MAX`] (the caller then keeps the non-blocking probe) or
/// if no watched descriptor has any wake source at all - parking on that could never
/// end, so the caller answers immediately instead (a wedge refused, not created).
///
/// # Safety
/// `[fds_va, fds_va + nfds*8)` must be a `pollfd` array bounded to `cur`'s user VA
/// range (the Linux dispatch does that for every `poll`, docs/ENGINEERING.md 12).
pub unsafe fn block_poll(cur: usize, fds_va: u64, nfds: usize, deadline_ns: u64) -> Option<Ctl> {
    if nfds > POLL_MAX {
        return None;
    }
    let set = pollset(cur);
    for (k, slot) in set.iter_mut().enumerate().take(nfds) {
        // SAFETY: the array was bounded to `nfds` 8-byte entries in the active cell.
        unsafe {
            let p = (fds_va as *const i32).add(k * 2);
            *slot = PollReq {
                fd: p.read(),
                events: (fds_va as *const i16).add(k * 4 + 2).read(),
            };
        }
    }
    let sources = poll_sources(cur, nfds, deadline_ns);
    if sources & (crate::idle::WAITABLE | crate::idle::PEER) == 0 {
        return None;
    }
    procs()[cur].state = PState::Blocked;
    procs()[cur].block = Block::Poll {
        fds_va,
        nfds,
        deadline_ns,
        sources,
    };
    Some(reschedule(cur))
}

/// Park `cur` on an epoll instance. `None` when no watched descriptor has a wake
/// source (as [`block_poll`]).
pub fn block_epoll(
    cur: usize,
    epfd: i64,
    events_va: u64,
    maxevents: usize,
    deadline_ns: u64,
) -> Option<Ctl> {
    let sources = super::state(cur).fds.epoll_sources(epfd)
        | if deadline_ns != 0 {
            crate::idle::TIMER
        } else {
            0
        };
    if sources & (crate::idle::WAITABLE | crate::idle::PEER) == 0 {
        return None;
    }
    procs()[cur].state = PState::Blocked;
    procs()[cur].block = Block::Epoll {
        epfd,
        events_va,
        maxevents,
        deadline_ns,
        sources,
    };
    Some(reschedule(cur))
}

/// The wake sources cell `cur`'s copied poll set can be woken by.
fn poll_sources(cur: usize, nfds: usize, deadline_ns: u64) -> crate::idle::Sources {
    let set = pollset(cur);
    let st = super::state(cur);
    let mut src = if deadline_ns != 0 {
        crate::idle::TIMER
    } else {
        0
    };
    for r in set.iter().take(nfds.min(POLL_MAX)) {
        if r.fd >= 0 {
            src |= st.fds.fd_sources(r.fd as i64);
        }
    }
    src
}
