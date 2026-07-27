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
use crate::linux::vma;
use crate::linux::{Ctl, err, pipe, ret, signal, stack, thread};
use crate::load;
use crate::mm::AddressSpace;
use crate::user::{self, MAX_CELLS};
use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

/// What a parked context is waiting for (docs/LINUX-COMPAT.md L6). Completed by
/// `complete_pblock` when the scheduler switches into the context. Stored
/// **per-context** in `thread.rs` (per-context blocking, docs/LINUX-COMPAT.md L4):
/// one context of a cell can block on `epoll_wait` while a sibling keeps running,
/// which is what an event-loop program (Node's V8 + libuv) needs.
#[derive(Copy, Clone)]
pub(crate) enum Block {
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
    /// `read` on an `eventfd` whose counter is zero (docs/LINUX-COMPAT.md
    /// L8-EVENTFD). Parked until some other cell writes it; `ev` is the registry
    /// index, which is kernel state, so the scheduler can judge satisfiability
    /// without the waiter's address space active.
    EventFdRead {
        buf_va: u64,
        count: u64,
        ev: u8,
    },
    /// `read` on a `timerfd` that has not yet expired (docs/LINUX-COMPAT.md
    /// L8-TIMERFD). Parked until its cell-clock deadline passes - the same wake
    /// class as `Timer` (`nanosleep`), completed by writing the expiration count.
    /// `tf` is the registry index (kernel state), so the scheduler judges
    /// satisfiability without the waiter's address space active.
    TimerFdRead {
        buf_va: u64,
        count: u64,
        tf: u8,
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

/// The cell running process `pid`, if it is **alive** - `Runnable` or `Blocked`,
/// never a `Zombie` (a zombie has no address space left to deliver into, and
/// `kill(2)` on one is a no-op in Linux too, not an error the caller can use).
///
/// The lookup `kill`/`kill(pid, 0)` needs: before this, signalling anything but
/// the caller's own pid was `-ESRCH` (docs/ARCHITECTURE-DEBT.md 4).
pub fn cell_of_pid(pid: u32) -> Option<usize> {
    (0..MAX_CELLS).find(|&i| {
        let p = &procs()[i];
        p.pid == pid && matches!(p.state, PState::Runnable | PState::Blocked)
    })
}

/// True if `pid` names any process this table still knows about, **including a
/// zombie**. `kill(pid, 0)` is an existence probe, and a not-yet-reaped child
/// still exists as far as its parent is concerned.
pub fn pid_exists(pid: u32) -> bool {
    (0..MAX_CELLS).any(|i| procs()[i].pid == pid && procs()[i].state != PState::Free)
}

/// Run `f` for every **alive** process, in cell order. `skip_top` excludes the
/// top of the process tree, which is what stands in for init here: Linux's
/// `kill(-1, sig)` signals every process the caller may signal *except* init,
/// and this tree has exactly one process with no parent.
pub fn for_each_live(skip_top: bool, mut f: impl FnMut(usize)) {
    for i in 0..MAX_CELLS {
        if skip_top && i == user::top_cell() {
            continue;
        }
        if matches!(procs()[i].state, PState::Runnable | PState::Blocked) {
            f(i);
        }
    }
}

/// How many Linux processes are alive right now - the `procs` field `sysinfo`
/// reports. Counts runnable and blocked, not zombies: a zombie is not a process
/// any more, it is a status waiting to be read.
pub fn live_count() -> usize {
    let mut n = 0;
    for_each_live(false, |_| n += 1);
    n
}

/// Mark `cell` as terminated by an uncaught fatal signal **without** handing the
/// CPU anywhere - the remote half of [`exit_signaled`].
///
/// `exit_signaled` ends with `reschedule`, which is right when the dying process
/// is the one that trapped. A `kill` from *another* process must not reschedule
/// inside the killer's syscall: the killer is still running and has its own
/// return value to deliver. Returns true if the target was the top cell, whose
/// death the caller must turn into an unwind rather than a zombie.
pub fn mark_signaled_remote(cell: usize, signo: u32) -> bool {
    super::state(cell).fds.close_all();
    if cell == user::top_cell() {
        return true;
    }
    // SAFETY: `cell`'s address space pointer is valid; it is torn down here and
    // never reactivated.
    unsafe { (*user::cell_aspace(cell)).free_user_frames() };
    procs()[cell].state = PState::Zombie;
    procs()[cell].wstatus = signo & 0x7f; // WIFSIGNALED
    false
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

/// Apply the `madvise` fork advice the parent recorded to the freshly forked
/// child (docs/SUBSTRATE.md 10a).
///
/// `fork_from` shares every committed page copy-on-write, which is the right
/// default and the wrong answer for two ranges a caller has explicitly marked:
///
/// - **`MADV_WIPEONFORK`**: the child must see zeroes, not a copy. Unmapping the
///   range in the child is exactly that - the record is still there (it was
///   deep-copied with the rest of the VMA list), so the child's first touch faults
///   and the handler fills a fresh **zeroed** frame. This is what stops a
///   `fork`ed userspace CSPRNG from producing the parent's stream.
/// - **`MADV_DONTFORK`**: the child must not have the mapping at all, so the
///   range is unmapped *and* its record removed.
///
/// Done after `dup_state` (which copies the VMA list) so the child has its own
/// records to consult and edit.
fn apply_fork_advice(parent: usize, child: usize) {
    // The **parent's** records are the authority for what was asked, and the
    // **child's** address space and list are what change - two different objects,
    // which is what lets this iterate the one while mutating the other rather than
    // copying the ranges into scratch first.
    //
    // It used to collect into two `[(usize, usize); MAX_VMAS]` arrays, which was a
    // silent truncation waiting to happen (a marked range past the array's end was
    // dropped with no diagnostic - a `MADV_WIPEONFORK` region that quietly kept its
    // parent's random state, exactly the failure that bit is there to prevent). With
    // the list funded there is no array dimension to size it against anyway.
    let parent_state = super::state(parent);
    let any = parent_state
        .vmas
        .with_advice(vma::ADV_WIPEONFORK | vma::ADV_DONTFORK)
        .next()
        .is_some();
    if !any {
        return;
    }

    // The unmapping must happen in the **child's** address space, which is not the
    // active one - so it goes through the child's `AddressSpace` directly rather
    // than through `user::unmap_range` (which operates on the running cell).
    // SAFETY: the child was just installed; its address space pointer is valid and
    // nothing else is touching it (it has not run).
    let child_aspace = unsafe { &mut *user::cell_aspace_mut(child) };
    for m in parent_state.vmas.with_advice(vma::ADV_WIPEONFORK) {
        child_aspace.free_user_range(m.base, m.len);
    }
    let child_state = super::state(child);
    for m in parent_state.vmas.with_advice(vma::ADV_DONTFORK) {
        child_aspace.free_user_range(m.base, m.len);
        child_state.vmas.remove(m.base, m.len);
    }
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

    // Share the parent's committed pages copy-on-write into a fresh address space, so
    // a fork costs page tables and not the resident set (docs/ARCHITECTURE-DEBT.md 4.0,
    // blocker 2). `&mut` because the *parent* is write-protected too - the half that
    // produces wrong values rather than a fault when it is missing.
    // SAFETY: the parent is the running cell; its address space pointer is valid, and
    // it is the active address space, which is what lets `fork_from` flush its TLB.
    let parent_aspace = unsafe { &mut *user::cell_aspace_mut(cur) };
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
    // The child's personality state includes a **funded** VMA table, so copying it
    // can genuinely fail on the child's frame budget. A half-copied table would
    // leave the child faulting on mappings it believes it has, so the fork is undone
    // and refused - the errno Linux uses when it cannot fund a new process.
    if !super::dup_state(cur, child) {
        // SAFETY: the child was installed above and has not run; this is the same
        // teardown `process_exit` performs, before the slot is handed back.
        unsafe { (*user::cell_aspace(child)).free_user_frames() };
        procs()[child] = Proc::free();
        thread::release_cell(child);
        super::state(child).vmas.teardown();
        user::free_cell(child);
        return -EAGAIN;
    }
    signal::fork_copy(cur, child);
    apply_fork_advice(cur, child);

    procs()[child] = Proc {
        state: PState::Runnable,
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
    let Some(img) = load::exec_elf_from_vfs_demand(ops, path_kva, path_len as u64, &mut new_aspace)
    else {
        return err(ENOENT);
    };
    // Record what this cell is now running, so `readlinkat("/proc/self/exe")` has
    // a truthful answer instead of a hardcoded `-ENOENT`
    // (docs/ARCHITECTURE-DEBT.md 4). SAFETY: EXEC_PATH holds `path_len` bytes.
    crate::linux::set_exe_path(cur, unsafe {
        core::slice::from_raw_parts(addr_of_mut!(EXEC_PATH) as *const u8, path_len)
    });

    // Build the new initial stack (argv/envp/auxv) - written through the kernel
    // linear map, so the new space need not be active yet.
    let sp = stack::setup_stack(&mut new_aspace, &img, &argv[..argc], &envp[..envc]);

    // Reset the cell's personality state for the new image: keep fds + cwd, new
    // heap/mmap/auxv, default signal handlers, single thread.
    super::exec_reinit(cur, &img);
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
            let Some(p) = crate::uaccess::read::<u64>(arr_va + (i as u64) * 8) else {
                break;
            };
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
    thread::set_current_pblock(cur, Block::Wait { wstatus_va, want });
    park_or_switch(cur)
}

/// Reap zombie cell `z` into `wstatus_va` (in the active reaper's address
/// space); free the slot and return the reaped pid.
fn reap(z: usize, wstatus_va: u64) -> u32 {
    let pid = procs()[z].pid;
    if wstatus_va != 0 {
        // Through `uaccess`, not a raw store: the parent's stack is copy-on-write
        // after a fork, so a present page here can still be read-only and a kernel
        // store to it faults at a kernel PC (docs/ENGINEERING.md 11). A refusal loses
        // the status rather than the machine, and `wait4` still reaps.
        if !crate::uaccess::write(wstatus_va, procs()[z].wstatus as i32) {
            crate::println!("linux: wait4 could not store the status at {wstatus_va:#x}");
        }
    }
    procs()[z] = Proc::free();
    // The slot is genuinely handed back here, so its funded per-cell tables go with
    // it: the context tables (which `exit` deliberately kept, because a zombie's
    // status is read after it) and the VMA table, whose records `process_exit`
    // already dropped. Without this a reaped child's metadata frames stay charged to
    // a cell that no longer exists (docs/SUBSTRATE.md pillar 1).
    thread::release_cell(z);
    super::state(z).vmas.teardown();
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
    // Its *records* are resources too: a file-backed mapping holds a reference to a
    // `filemap` backing store, and dropping the frames without dropping the records
    // leaks that reference until the slot happens to be reused - and `dup_state`
    // overwrites a reused slot wholesale, so "happens to be reused" never releases it
    // at all. Clearing here is what makes the reference lifetime symmetric with
    // `vmas.inherit_files()` in `dup_state`: taken at fork, given back at exit.
    //
    // `teardown` rather than `clear`, because the table's *own* frames are a
    // resource too now that it is funded: this releases them and uncharges the dead
    // cell, so the frame the exit reclaims includes the bookkeeping it caused
    // (docs/SUBSTRATE.md pillar 1).
    super::state(cell).vmas.teardown();
    procs()[cell].state = PState::Zombie;
    procs()[cell].wstatus = status;
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
    thread::set_current_pblock(cur, Block::PipeRead { buf_va, count, idx });
    park_or_switch(cur)
}

/// Park `cur` on an eventfd read whose counter is zero; the scheduler completes
/// the read (with `cur`'s address space active) once a peer writes the counter.
pub fn block_eventfd_read(cur: usize, buf_va: u64, count: u64, ev: u8) -> Ctl {
    thread::set_current_pblock(cur, Block::EventFdRead { buf_va, count, ev });
    park_or_switch(cur)
}

/// Park `cur` on a not-yet-expired timerfd read; the scheduler completes the read
/// (writing the expiration count, `cur`'s address space active) once the deadline
/// passes. The wait is honoured by the scheduler's timer slices, like `nanosleep`.
pub fn block_timerfd_read(cur: usize, buf_va: u64, count: u64, tf: u8) -> Ctl {
    thread::set_current_pblock(cur, Block::TimerFdRead { buf_va, count, tf });
    park_or_switch(cur)
}

/// Park `cur` on a full pipe write; completed when space frees or read ends close.
pub fn block_pipe_write(cur: usize, buf_va: u64, count: u64, idx: usize) -> Ctl {
    thread::set_current_pblock(cur, Block::PipeWrite { buf_va, count, idx });
    park_or_switch(cur)
}

// ----------------------------------------------------------------- scheduler

/// `sched_yield` across **processes**: leave `cur` runnable and hand the CPU to
/// the next runnable cell, round-robin (docs/ARCHITECTURE-DEBT.md 4).
///
/// [`thread::sched_yield`] only ever rescheduled among a cell's own L4 contexts,
/// so a forked child looping `sched_yield()` ran to completion before its parent
/// was scheduled at all: this scheduler is cooperative, a yield is one of its few
/// preemption points, and there the yield did nothing. Every other cross-cell
/// hand-off here goes through [`reschedule`]; this is that hand-off with the
/// caller left **runnable** instead of blocked - what the native `SYS_YIELD` does
/// for native cells (docs/NETSTACK.md 17).
///
/// `reschedule`'s round-robin visits `cur` last, so a process that is the only
/// runnable one is simply picked again: a yield is never a block, and never a
/// deadlock.
pub fn yield_cell(cur: usize) -> Ctl {
    // Set the return value into the saved frame *before* the switch, so `cur`'s
    // frame already carries the 0 it resumes with whichever cell runs next.
    // `complete_block` cannot do it - a yield registers no block.
    let frame = thread::current_frame(cur);
    // SAFETY: `frame` is `cur`'s current-context saved state, in kernel memory.
    unsafe { arch::set_syscall_ret(&mut *frame, 0) };
    reschedule(cur)
}

/// Hand the CPU to the next runnable cell after `leaving` blocks or exits. Wakes
/// any blocked cell whose condition is now satisfiable, then round-robins to a
/// runnable cell and completes its pending block. Panics only on a true
/// deadlock (no runnable cell) - a scheduling bug, surfaced loudly rather than
/// **Preempt** cell `cur` in favour of another runnable Linux cell, returning the
/// frame to resume, or `None` when there is no other cell to run.
///
/// Called from [`crate::user::on_user_interrupt`] after `thread::preempt_context`
/// found no ready sibling *within* `cur`. Three differences from [`reschedule`],
/// each deliberate:
///
/// - `cur` **stays `Runnable`**. It did not block; it was taken off the CPU and is
///   still competing. Marking it blocked would be a claim the scheduler then acts on
///   by never running it again.
/// - Nothing is charged here. `on_user_interrupt` already charged the slice and
///   recorded the stop as involuntary before choosing where to go.
/// - **Pending signals are not delivered on this path.** Delivery rewrites the
///   target's saved frame and can conclude the target must die, which is a
///   `Ctl::Exit` - a control flow the interrupt-return path cannot express, since it
///   must hand back a frame to resume. So a signal that arrived while the target was
///   off the CPU is delivered at its next *ordinary* resume, which happens at every
///   syscall boundary. Nothing is lost, only deferred - and deferring is what keeps
///   preemption from being able to end a process at an arbitrary instruction.
pub(crate) fn preempt_cell(cur: usize) -> Option<*mut TrapFrame> {
    thread::save_current_fp(cur);
    for i in 0..MAX_CELLS {
        if procs()[i].state == PState::Blocked && satisfiable(i) {
            procs()[i].state = PState::Runnable;
        }
    }
    let n = crate::sched::dispatch::pick_excluding_self(cur, MAX_CELLS, |i| {
        user::cell_present(i) && procs()[i].state == PState::Runnable
    })?;
    let idx = first_satisfiable_context(n).unwrap_or_else(|| thread::current_context(n));
    thread::set_current(n, idx);
    user::switch_to_cell(n);
    thread::restore_current(n);
    complete_pblock(n, idx);
    Some(thread::current_frame(n))
}

/// hung.
fn reschedule(leaving: usize) -> Ctl {
    // Save the outgoing cell's live FP state (harmless if it is exiting).
    thread::save_current_fp(user::current_index());
    // Charge the CPU time the leaving cell just used to its vcore and end its burst
    // (docs/SUBSTRATE.md pillar 3, migration S3'). It reached here by parking on a
    // wake source or exiting, so the relinquish is **voluntary** - which is exactly
    // the transition BORE scores, and the reason it can be observed here rather
    // than inferred: this kernel has no path from running to not-running that does
    // not pass through a named call.
    crate::sched::dispatch::relinquish();

    loop {
        // Wake blocked cells whose condition now holds.
        for i in 0..MAX_CELLS {
            if procs()[i].state == PState::Blocked && satisfiable(i) {
                procs()[i].state = PState::Runnable;
            }
        }

        // The **order** comes from the EEVDF+BORE ready queue when it is enabled;
        // the predicate below remains the sole authority on *whether* a cell may
        // run. With dispatch disabled this is the pre-migration round-robin,
        // expression for expression (docs/SUBSTRATE.md 15).
        let next = crate::sched::dispatch::pick(leaving, MAX_CELLS, |i| {
            user::cell_present(i) && procs()[i].state == PState::Runnable
        });
        if let Some(n) = next {
            // Resume the context whose per-context block is satisfiable (a
            // multi-threaded cell can have several parked); a cell that only
            // yielded has none, so its current context resumes unchanged. Set it
            // current *before* `restore_current`, which reloads that context's FP.
            let idx = first_satisfiable_context(n).unwrap_or_else(|| thread::current_context(n));
            thread::set_current(n, idx);
            user::switch_to_cell(n);
            thread::restore_current(n);
            // Record who is running, from which the next relinquish computes what to
            // charge. The returned slice is what a preemption timer is armed with;
            // it is armed by the trap-return path, which is the only place that
            // knows the cell is about to actually execute.
            crate::sched::dispatch::running(n, idx);
            complete_pblock(n, idx);
            // A signal another process sent while `n` was not running is
            // delivered *here* and nowhere else: delivery is a rewrite of the
            // target's own saved frame, pushing a `rt_sigframe` onto the target's
            // own user stack, so it needs the target's address space active -
            // which it is, from `switch_to_cell` two lines up. This runs after
            // `complete_block` so the interrupted syscall's return value is
            // already in the frame and gets saved into the ucontext, exactly as
            // a signal interrupting a completed syscall should.
            match signal::on_resume(n) {
                signal::Resumed::Ran => {}
                // An uncaught fatal default: the target dies instead of resuming.
                // Do it without recursing back into `reschedule` - mark it and go
                // round the loop for the next runnable process.
                signal::Resumed::Fatal(signo) => {
                    if mark_signaled_remote(n, signo) {
                        return Ctl::Exit(128 + signo as u64);
                    }
                    continue;
                }
            }
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

/// The calling context just recorded a proc-level block ([`thread::set_current_pblock`]).
/// Decide what runs next, **per-context** (docs/LINUX-COMPAT.md L4):
///
/// 1. a `Ready` **sibling** context of this cell, if any - so one thread blocking on
///    `epoll_wait` does not freeze the thread that will wake it (the Node V8 + libuv
///    case: main parks on an eventfd a worker must write);
/// 2. else a **sibling that is already satisfiable** - a peer wrote the fd before it
///    got a turn to park, so complete it and switch to it;
/// 3. else every context of this cell is blocked and none is satisfiable, so the
///    whole cell parks and the CPU goes to another process (the pre-existing
///    cross-cell [`reschedule`]).
///
/// A **single-context** cell has no sibling and its lone context is the one that
/// just blocked, so it always falls straight to (3) - byte-for-byte the behaviour
/// before per-context blocking, which is what keeps every non-threaded program
/// (and the whole existing test suite) unchanged.
fn park_or_switch(cell: usize) -> Ctl {
    if let Some(sib) = thread::pick_ready_sibling(cell) {
        return thread::switch_current_to(cell, sib);
    }
    if let Some(idx) = first_satisfiable_context(cell) {
        let frame = thread::resume_pblocked(cell, idx);
        complete_pblock(cell, idx);
        return Ctl::Switch(frame);
    }
    procs()[cell].state = PState::Blocked;
    reschedule(cell)
}

/// A futex WAIT found no `Ready` sibling to switch to. If a **sibling context** is
/// parked on a proc-level condition that is satisfiable *right now*, resume it - the
/// mixed futex+event-loop case (Node's teardown: the main thread writes an eventfd,
/// then futex-waits for the worker thread that is parked on that eventfd; the worker
/// is now satisfiable and, once resumed, will `FUTEX_WAKE` the main thread). The
/// futex waiter stays `Blocked` until that wake. Returns `None` if no sibling is
/// immediately satisfiable, so the futex path keeps its survivable `EAGAIN` (a lone
/// futex spinner is unchanged - it has no such sibling).
pub fn resume_satisfiable_sibling(cell: usize) -> Option<Ctl> {
    let idx = first_satisfiable_context(cell)?;
    let frame = thread::resume_pblocked(cell, idx);
    complete_pblock(cell, idx);
    Some(Ctl::Switch(frame))
}

/// The first context of `cell` parked on a proc-level condition that is satisfiable
/// right now, if any - the scheduler's per-context wake test.
fn first_satisfiable_context(cell: usize) -> Option<usize> {
    (0..thread::capacity(cell)).find(|&i| {
        thread::is_pblocked(cell, i) && satisfiable_block(cell, thread::pblock_of(cell, i))
    })
}

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

/// The union of wake sources blocked cell `cell` is waiting on - the union over
/// all of its parked contexts' per-context conditions (per-context blocking).
fn sources_of(cell: usize) -> crate::idle::Sources {
    let mut s = 0;
    for i in 0..thread::capacity(cell) {
        if thread::is_pblocked(cell, i) {
            s |= sources_of_block(thread::pblock_of(cell, i));
        }
    }
    s
}

/// The wake sources one block condition can be satisfied by.
fn sources_of_block(b: Block) -> crate::idle::Sources {
    use crate::idle;
    match b {
        Block::None => 0,
        // A pipe/socket ring or a child exit is another *process*'s doing.
        Block::Wait { .. }
        | Block::PipeRead { .. }
        | Block::PipeWrite { .. }
        | Block::EventFdRead { .. } => idle::PEER,
        // A timerfd read and a nanosleep both wait on a cell-clock deadline.
        Block::Timer { .. } | Block::TimerFdRead { .. } => idle::TIMER,
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
            thread::dump_contexts(i);
        }
    }
    Ctl::Exit(crate::abi::DEADLOCK_EXIT)
}

/// The name of cell `i`'s block, for the deadlock diagnostic - the first parked
/// context's condition (per-context blocking).
fn block_name(i: usize) -> &'static str {
    let b = (0..thread::capacity(i))
        .find(|&k| thread::is_pblocked(i, k))
        .map(|k| thread::pblock_of(i, k))
        .unwrap_or(Block::None);
    match b {
        Block::None => "nothing",
        Block::Wait { .. } => "wait4 (child exit)",
        Block::PipeRead { .. } => "read (empty pipe/socket)",
        Block::PipeWrite { .. } => "write (full pipe/socket)",
        Block::EventFdRead { .. } => "read (eventfd counter zero)",
        Block::TimerFdRead { .. } => "read (timerfd not expired)",
        Block::Timer { .. } => "nanosleep (deadline)",
        Block::Console { .. } => "read (console)",
        Block::Poll { .. } => "poll (fd readiness)",
        Block::Epoll { .. } => "epoll_wait (fd readiness)",
    }
}

/// Whether blocked cell `i` has any context whose per-context condition is now
/// satisfiable (the reschedule wake test).
fn satisfiable(i: usize) -> bool {
    first_satisfiable_context(i).is_some()
}

/// Whether one block condition of cell `cell` is now satisfiable. Judged entirely
/// from kernel state (fd tables, pipe/eventfd/timerfd registries, the cell clock),
/// so the scheduler can ask it while another cell's address space is active.
fn satisfiable_block(cell: usize, b: Block) -> bool {
    match b {
        Block::Wait { want, .. } => {
            find_zombie_child(cell, want).is_some() || !has_child(cell, want)
        }
        Block::PipeRead { idx, .. } => pipe::has_data(idx) || pipe::writers(idx) == 0,
        Block::PipeWrite { idx, .. } => pipe::has_space(idx) || pipe::readers(idx) == 0,
        Block::EventFdRead { ev, .. } => super::eventfd::readable(ev),
        Block::TimerFdRead { tf, .. } => super::timerfd::readable(tf),
        Block::Timer { deadline_ns } => super::cell_clock_ns(false) >= deadline_ns,
        Block::Console { .. } => crate::input::has_data() || crate::input::at_eof(),
        Block::Poll {
            nfds, deadline_ns, ..
        } => {
            poll_ready_count(cell, nfds) > 0
                || (deadline_ns != 0 && super::cell_clock_ns(false) >= deadline_ns)
        }
        Block::Epoll {
            epfd, deadline_ns, ..
        } => {
            super::state(cell).fds.epoll_ready(epfd) > 0
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

/// Apply woken context `idx` of cell `n`'s pending syscall (its address space is now
/// active, and `idx` is current) and set its return value, then clear its
/// per-context block and mark it ready. A `None` block is a no-op (a cell that only
/// yielded resumes its current context with nothing to complete).
fn complete_pblock(n: usize, idx: usize) {
    let block = thread::pblock_of(n, idx);
    if matches!(block, Block::None) {
        return;
    }
    thread::clear_pblock_ready(n, idx);
    let frame = thread::frame_ptr(n, idx);
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
        Block::EventFdRead { buf_va, count, ev } => {
            match super::eventfd::read(ev, buf_va, count) {
                Ok(super::eventfd::ReadNb::Done) => 8,
                // Woken only when readable, so this cannot normally happen; report
                // -EAGAIN rather than a fabricated byte count if it ever does.
                Ok(super::eventfd::ReadNb::WouldBlock) => -super::errno::EAGAIN,
                Err(e) => e,
            }
        }
        Block::TimerFdRead { buf_va, count, tf } => match super::timerfd::read(tf, buf_va, count) {
            Ok(super::timerfd::ReadNb::Done) => 8,
            Ok(super::timerfd::ReadNb::WouldBlock) => -super::errno::EAGAIN,
            Err(e) => e,
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
        // `revents` sits at an odd 2-byte offset in the caller's `pollfd`, so the
        // write is unaligned by the ABI's own layout - and it goes through `uaccess`,
        // which resolves the page's writability (the array may be COW after a fork).
        crate::uaccess::write_unaligned::<i16>(fds_va + (k as u64) * 8 + 6, revents);
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
    thread::set_current_pblock(cur, Block::Timer { deadline_ns });
    park_or_switch(cur)
}

/// Park `cur` on an empty console read - blocking `stdin`.
pub fn block_console(cur: usize, buf_va: u64, count: u64) -> Ctl {
    thread::set_current_pblock(cur, Block::Console { buf_va, count });
    park_or_switch(cur)
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
        let base = fds_va + (k as u64) * 8;
        *slot = PollReq {
            fd: crate::uaccess::read_unaligned::<i32>(base).unwrap_or(-1),
            events: crate::uaccess::read_unaligned::<i16>(base + 4).unwrap_or(0),
        };
    }
    let sources = poll_sources(cur, nfds, deadline_ns);
    if sources & (crate::idle::WAITABLE | crate::idle::PEER) == 0 {
        return None;
    }
    thread::set_current_pblock(
        cur,
        Block::Poll {
            fds_va,
            nfds,
            deadline_ns,
            sources,
        },
    );
    Some(park_or_switch(cur))
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
    thread::set_current_pblock(
        cur,
        Block::Epoll {
            epfd,
            events_va,
            maxevents,
            deadline_ns,
            sources,
        },
    );
    Some(park_or_switch(cur))
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
