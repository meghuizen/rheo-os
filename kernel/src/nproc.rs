//! Native processes (docs/LIBRHEO.md Phase F): `SYS_SPAWN` / `SYS_WAIT` and the
//! cooperative cross-cell scheduler that ties them together. This EXPOSES the
//! existing Cell object (docs/ARCHITECTURE.md 3 object 1) to a native cell as
//! mechanism - it adds no new kernel object: the parent/child tree, wait status,
//! and block/wake state are per-cell synthesized state here, exactly like the
//! Linux personality's `linux::proc` (docs/LINUX-COMPAT.md L6), which this
//! mirrors for `Personality::Native` cells.
//!
//! A spawned child is a fresh native cell with its **own** address space and
//! mapped queue pair (so it can run librheo's reactor), **sharing** the parent's
//! capability bundle (like `fork`). Spawning is gated by a **cell-spawn
//! capability** (an `ObjectKind::Cell` cap carrying WRITE): a cell without it
//! cannot create cells (no ambient authority). Scheduling generalizes the native
//! cross-cell `SYS_SWITCH`: the parent that `SYS_WAIT`s blocks and hands the CPU
//! to a runnable child; the child's exit makes it a zombie and reschedules,
//! waking the parent whose wait is now satisfiable. Cooperative, single CPU: a
//! cell yields only at a syscall boundary. The pre-existing native `run` /
//! `SYS_SWITCH` path is untouched - a cell that never spawns has no entry here.

use crate::abi::{FAULT_EXIT, SPAWN_CHAN_SLOT};
use crate::arch::{self, TrapFrame};
use crate::capability::{BUDGET_UNLIMITED, ObjectKind, ObjectTable, READ, WRITE};
use crate::load;
use crate::mm::AddressSpace;
use crate::queue::QueuePair;
use crate::user::{self, MAX_CELLS};
use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

/// The result of a `SYS_WAIT` (or of an exit that reschedules): resume the same
/// cell with a return value, or switch to a different cell's frame.
pub enum Sched {
    Ret(u64),
    Switch(*mut TrapFrame),
}

#[derive(Copy, Clone, PartialEq)]
enum PState {
    Free,
    Runnable,
    /// Parked in `SYS_WAIT` for the child cell in `wait_for`.
    Blocked,
    /// Exited, holding its exit code, awaiting a parent `SYS_WAIT`.
    Zombie,
}

#[derive(Copy, Clone)]
struct Proc {
    state: PState,
    /// Parent cell index, or -1 for the top of the tree (the first spawner).
    parent: i32,
    /// The child cell this proc is blocked in `SYS_WAIT` for (when `Blocked`).
    wait_for: usize,
    /// Exit code while `Zombie` (0..=255, or `FAULT_EXIT` for a faulted child).
    code: u64,
}

impl Proc {
    const fn free() -> Proc {
        Proc {
            state: PState::Free,
            parent: -1,
            wait_for: 0,
            code: 0,
        }
    }
}

static mut PROCS: [Proc; MAX_CELLS] = [const { Proc::free() }; MAX_CELLS];
/// Kernel-owned per-child storage: a spawned cell's address space, queue-pair
/// overlay, and trap frame all live here (the top cell's are test/loader-owned).
/// Fixed arrays, no allocation - the kernel stays allocation-free.
static mut NASPACE: [MaybeUninit<AddressSpace>; MAX_CELLS] =
    [const { MaybeUninit::uninit() }; MAX_CELLS];
static mut NQP: [MaybeUninit<QueuePair>; MAX_CELLS] = [const { MaybeUninit::uninit() }; MAX_CELLS];
static mut NFRAME: [MaybeUninit<TrapFrame>; MAX_CELLS] =
    [const { MaybeUninit::uninit() }; MAX_CELLS];

/// Bounded kernel scratch for copying a spawn's argv/envp strings out of the
/// caller's address space before the child's stack is built.
const SPAWN_STR_MAX: usize = 8 * 1024;
const SPAWN_PTR_MAX: usize = 64;
static mut SPAWN_STR: [u8; SPAWN_STR_MAX] = [0; SPAWN_STR_MAX];
static mut SPAWN_PATH: [u8; 512] = [0; 512];

fn procs() -> &'static mut [Proc; MAX_CELLS] {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut *addr_of_mut!(PROCS) }
}

/// Clear all native-process state (called from `user::reset`).
pub fn reset() {
    for p in procs().iter_mut() {
        *p = Proc::free();
    }
}

/// Register `cell` as the top of a native process tree the first time it spawns
/// (parent = -1, runnable). Idempotent.
fn ensure_top(cell: usize) {
    if procs()[cell].state == PState::Free {
        procs()[cell] = Proc {
            state: PState::Runnable,
            parent: -1,
            wait_for: 0,
            code: 0,
        };
    }
}

// -------------------------------------------------------------------- spawn

/// `SYS_SPAWN(path_va, path_len, argv_va, envp_va, chan_spec)`: load the ELF at
/// `path` from the VFS into a new native cell, build its initial stack from the
/// caller's argv/envp, map it a queue pair + mint a queue capability into the
/// shared bundle, and record the caller as parent. Returns the child's handle (its
/// cell index) or `u64::MAX` on failure. Gated by the cell-spawn capability.
///
/// `chan_spec` picks which of the caller's channel ends the child inherits: 0 =
/// the Phase J default (slot 0 if wired), else `SPAWN_CHAN_SLOT | slot << 8`
/// (docs/NETSTACK.md the service-cell section, rheo-net N4a) - a **service cell**
/// spawns client k with its own slot k, so each client gets a private ring.
///
/// Reading the caller's `frame` (for its kernel SP, shared by the cooperative
/// child) is the point of the call.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    cur: usize,
    path_va: u64,
    path_len: u64,
    argv_va: u64,
    envp_va: u64,
    chan_spec: u64,
    frame: *mut TrapFrame,
) -> u64 {
    // Cell-spawn authority: the caller must hold an ObjectKind::Cell capability
    // with WRITE - no ambient authority (docs/LIBRHEO.md Phase F).
    let caps_ptr = user::cell_caps(cur);
    let objs_ptr = user::cell_objects(cur);
    if caps_ptr.is_null() || objs_ptr.is_null() {
        return u64::MAX;
    }
    // SAFETY: single CPU, synchronous trap; the cell's tables are uniquely owned.
    let authorized = unsafe { (*caps_ptr).holds(&*objs_ptr, ObjectKind::Cell, WRITE) };
    if !authorized {
        return u64::MAX;
    }
    let Some(ops) = crate::svc::file_ops() else {
        return u64::MAX;
    };

    // Find a free cell slot (bounded by MAX_CELLS).
    let Some(child) =
        (0..MAX_CELLS).find(|&i| !user::cell_present(i) && procs()[i].state == PState::Free)
    else {
        return u64::MAX;
    };

    ensure_top(cur);

    // Copy the path + argv + envp out of the caller's (active) address space into
    // kernel scratch, so the child's stack can be built through the linear map
    // (its address space is not active yet). Layout in SPAWN_STR: argv then envp.
    let path_len = copy_path(path_va, path_len);
    if path_len == 0 {
        return u64::MAX;
    }
    let mut argv: [&[u8]; SPAWN_PTR_MAX] = [b""; SPAWN_PTR_MAX];
    let mut envp: [&[u8]; SPAWN_PTR_MAX] = [b""; SPAWN_PTR_MAX];
    let mut off = 0usize;
    let argc = copy_str_array(argv_va, &mut argv, &mut off);
    let envc = copy_str_array(envp_va, &mut envp, &mut off);

    // Stream the ELF into a fresh address space (bias per ELF type, like a Linux
    // load; librheo binaries are ET_EXEC so bias 0). The path is a kernel VA.
    let path_kva = addr_of_mut!(SPAWN_PATH) as u64;
    let mut child_aspace = AddressSpace::new((child as u16) + 64);
    let Some(img) = load::exec_elf_from_vfs(ops, path_kva, path_len as u64, &mut child_aspace)
    else {
        return u64::MAX;
    };
    let entry = img.entry;

    // Build the native SysV initial stack (argc/argv/envp). Its SP points at
    // argc; we pass that SP as the child's arg0 so librheo's `_start` finds its
    // arguments without a naked prologue (an explicit, native alternative to
    // walking the raw SP).
    let sp = load::setup_stack(&mut child_aspace, &argv[..argc], &envp[..envc]);

    // Map the child a queue-pair region + mint its queue capability into the
    // shared bundle (so the child's ring is grant-checked at doorbell time).
    let qp = load::map_queue(&mut child_aspace);
    // SAFETY: single CPU; the shared object/cap tables are uniquely owned for the
    // trap. `objects` is installed `*const` but owned mutably by the test kernel;
    // recovered here to create the child's queue object.
    let qp_cap_id = unsafe {
        let objects = &mut *(objs_ptr as *mut ObjectTable);
        let caps = &mut *caps_ptr;
        let Ok(obj) = objects.create(ObjectKind::QueuePair) else {
            return u64::MAX;
        };
        match caps.mint(objects, obj, READ | WRITE, BUDGET_UNLIMITED) {
            Ok(h) => h.raw_low32(),
            Err(_) => return u64::MAX,
        }
    };

    // Inherit one of the parent's cross-cell channels (docs/LIBRHEO.md Phase J;
    // the slot selector is docs/NETSTACK.md rheo-net N4a): map the same channel
    // frames into the child RW and mint a channel capability into the shared
    // bundle, so a spawned child streams over the Phase E channel (a spawned
    // pipeline stage, or a service's client). The child gets the **opposite** role
    // (parent consumer <-> child producer) at its own **slot 0**, so a client
    // binary is slot-agnostic. Only a parent that holds that channel triggers
    // this - an ordinary spawn (`chan_spec` 0, no slot-0 channel) is unchanged.
    let want_slot = if chan_spec & SPAWN_CHAN_SLOT != 0 {
        ((chan_spec >> 8) & 0xff) as usize
    } else {
        0
    };
    let (p_chan_va, p_role) = user::cell_chan_slot(cur, want_slot);
    if chan_spec & SPAWN_CHAN_SLOT != 0 && p_chan_va == 0 {
        return u64::MAX; // an explicit slot request must name a wired channel
    }
    let child_chan = if p_chan_va != 0 {
        // SAFETY: single CPU; the parent's address space is read (a page-table
        // walk) and the child's is edited (published when the child is switched
        // to). Both are uniquely owned for the trap.
        let child_chan_va = load::channel_slot_va(0) as u64;
        let n = unsafe {
            (*user::cell_aspace(cur)).share_rw_into(
                &mut child_aspace,
                p_chan_va as usize,
                QueuePair::REGION_SIZE,
                child_chan_va as usize,
            )
        };
        if n == 0 {
            None
        } else {
            // SAFETY: as the qp-cap mint above - the shared tables are uniquely
            // owned for the trap.
            let cap = unsafe {
                let objects = &mut *(objs_ptr as *mut ObjectTable);
                let caps = &mut *caps_ptr;
                let Ok(obj) = objects.create(ObjectKind::QueuePair) else {
                    return u64::MAX;
                };
                match caps.mint(objects, obj, READ | WRITE, BUDGET_UNLIMITED) {
                    Ok(h) => h.raw_low32(),
                    Err(_) => return u64::MAX,
                }
            };
            Some((child_chan_va, cap, p_role ^ 1))
        }
    } else {
        None
    };

    // Persist the child's kernel-owned storage.
    // SAFETY: single CPU; slot `child` is free, so these MaybeUninit cells are
    // unused and outlive the child's run.
    let (aspace_ptr, qp_ptr, frame_ptr) = unsafe {
        (*addr_of_mut!(NASPACE))[child].write(child_aspace);
        (*addr_of_mut!(NQP))[child].write(qp);
        let kernel_sp = arch::trapframe_kernel_sp(&*frame);
        let cf = arch::trapframe_new(entry, sp, sp, kernel_sp);
        (*addr_of_mut!(NFRAME))[child].write(cf);
        (
            (*addr_of_mut!(NASPACE))[child].as_ptr(),
            (*addr_of_mut!(NQP))[child].as_ptr(),
            (*addr_of_mut!(NFRAME))[child].as_mut_ptr(),
        )
    };

    // SAFETY: pointers are kernel-owned statics that outlive the child's run.
    unsafe {
        user::install_spawned(
            child,
            aspace_ptr,
            qp_ptr,
            frame_ptr,
            cur,
            load::USER_QUEUE_VA as u64,
            qp_cap_id,
        );
    }
    // Record the inherited channel so the child's `SYS_CONNECT` reports its end.
    if let Some((va, cap, role)) = child_chan {
        user::set_channel_info(child, va, cap, role);
    }
    procs()[child] = Proc {
        state: PState::Runnable,
        parent: cur as i32,
        wait_for: 0,
        code: 0,
    };
    child as u64
}

/// Copy the NUL-terminated path at user VA `va` (length hint `len`) into
/// `SPAWN_PATH`; returns its length (0 on empty/oversized).
fn copy_path(va: u64, len: u64) -> usize {
    if va == 0 {
        return 0;
    }
    let want = (len as usize).min(511);
    // SAFETY: `va` is a byte string in the active cell; bounded copy.
    unsafe {
        let src = va as *const u8;
        let dst = addr_of_mut!(SPAWN_PATH) as *mut u8;
        let mut n = 0usize;
        while n < want {
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

/// Copy a NULL-terminated C-string pointer array at user VA `arr_va` into
/// `SPAWN_STR` starting at `*off`, filling `out[i]` with a slice into it.
fn copy_str_array(arr_va: u64, out: &mut [&'static [u8]; SPAWN_PTR_MAX], off: &mut usize) -> usize {
    if arr_va == 0 {
        return 0;
    }
    let mut count = 0usize;
    // SAFETY: `arr_va` is a NULL-terminated pointer array in the active cell.
    unsafe {
        let base = addr_of_mut!(SPAWN_STR) as *mut u8;
        for slot in out.iter_mut() {
            let p = (arr_va as *const u64).add(count).read();
            if p == 0 {
                break;
            }
            let start = *off;
            let src = p as *const u8;
            let mut n = 0usize;
            while *off < SPAWN_STR_MAX - 1 {
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

// --------------------------------------------------------------------- wait

/// `SYS_WAIT(handle)`: reap the child cell `handle` if it is a zombie, else block
/// the caller until it exits (docs/LIBRHEO.md Phase F). Returns the child's exit
/// code, or `u64::MAX` if `handle` names no child of the caller.
pub fn wait(cur: usize, handle: u64, _frame: *mut TrapFrame) -> Sched {
    let child = handle as usize;
    if child >= MAX_CELLS
        || procs()[child].parent != cur as i32
        || procs()[child].state == PState::Free
    {
        return Sched::Ret(u64::MAX);
    }
    if procs()[child].state == PState::Zombie {
        return Sched::Ret(reap(child));
    }
    // Block the caller and hand the CPU to a runnable cell (the child).
    procs()[cur].state = PState::Blocked;
    procs()[cur].wait_for = child;
    Sched::Switch(reschedule(cur))
}

// ---------------------------------------------------------------------- yield

/// `SYS_YIELD()`: hand the CPU to the **next runnable native cell** in
/// round-robin order; the caller stays runnable (docs/NETSTACK.md the service-cell
/// section, rheo-net N4a). This is the N-cell generalisation of `SYS_SWITCH`'s
/// directed `cur^1` hand-off, which cannot reach client 3 from client 2 and so
/// livelocks a service serving N>1 clients. It adds **no kernel object**: it is the
/// same cooperative cross-cell scheduler `SYS_WAIT`/child-exit already drive
/// (`reschedule` above), exposed as a plain yield, and transfers no authority -
/// the cells involved share one capability bundle.
///
/// Where the caller has no native process tree (two cells a test kernel wired but
/// never spawned - Phase E/J), the round-robin degenerates to the `cur^1` peer and
/// behaviour is unchanged. Returns `Sched::Ret(0)` when the caller is the only
/// schedulable cell (yield to nobody = resume).
pub fn yield_cell(cur: usize) -> Sched {
    let Some(next) = (1..MAX_CELLS)
        .map(|k| (cur + k) % MAX_CELLS)
        .find(|&i| schedulable(i))
    else {
        return Sched::Ret(0);
    };
    // The native cross-cell switch, FP/SIMD register file included: this is a
    // hard-float cell's hand-off point (docs/LIBRHEO.md, docs/ENGINEERING.md 3),
    // and a service cell reaches it on every client round.
    user::switch_native_cell(cur, next);
    complete_block(next);
    Sched::Switch(user::cell_frame(next))
}

/// Whether cell `i` can be resumed by a yield: present, native, and either a
/// runnable member of a native process tree or a cell with no tree state at all
/// (an installed Phase E/J peer, whose `Proc` slot is `Free`). A `Blocked` waiter
/// or a `Zombie` is skipped - `reschedule` owns waking those.
fn schedulable(i: usize) -> bool {
    user::cell_present(i)
        && user::cell_is_native(i)
        && matches!(procs()[i].state, PState::Free | PState::Runnable)
}

/// Reap zombie child `z`: free its cell slot and kernel-owned storage, and
/// return its exit code.
fn reap(z: usize) -> u64 {
    let code = procs()[z].code;
    procs()[z] = Proc::free();
    user::free_cell(z);
    // The child's frames were freed at exit (`process_exit`); the MaybeUninit
    // storage is reused on the next spawn into this slot.
    code
}

// ---------------------------------------------------------------- exit paths

/// A spawned native child's exit (docs/LIBRHEO.md Phase F). Returns `Some(frame)`
/// to resume (the child became a zombie and the CPU was handed off), or `None`
/// if `cell` is not a spawned child (the caller then unwinds `run` as before).
pub fn on_exit(cell: usize, code: u64) -> Option<*mut TrapFrame> {
    if procs()[cell].state == PState::Free || procs()[cell].parent < 0 {
        return None; // not a spawned child (top cell or a non-spawning cell)
    }
    Some(process_exit(cell, code & 0xff))
}

/// A spawned native child's fault (docs/LIBRHEO.md Phase F): reaped with
/// `FAULT_EXIT` (native cells have no signals). `None` if not a spawned child
/// (its fault stays terminal).
pub fn on_fault(cell: usize) -> Option<*mut TrapFrame> {
    if procs()[cell].state == PState::Free || procs()[cell].parent < 0 {
        return None;
    }
    Some(process_exit(cell, FAULT_EXIT))
}

/// Make `cell` a zombie holding `code`, reclaim its frames, and reschedule.
fn process_exit(cell: usize, code: u64) -> *mut TrapFrame {
    // SAFETY: `cell`'s address space pointer is valid; its user frames are no
    // longer needed and it is never reactivated.
    unsafe { (*user::cell_aspace(cell)).free_user_frames() };
    procs()[cell].state = PState::Zombie;
    procs()[cell].code = code;
    reschedule(cell)
}

// ----------------------------------------------------------------- scheduler

/// Hand the CPU to the next runnable native cell after `leaving` blocks or exits.
/// Wakes any parent whose awaited child is now a zombie, round-robins to a
/// runnable cell, and completes its pending `SYS_WAIT`. Panics only on a true
/// deadlock (a scheduling bug, surfaced loudly).
fn reschedule(leaving: usize) -> *mut TrapFrame {
    // Wake blocked parents whose awaited child is now a zombie.
    for i in 0..MAX_CELLS {
        if procs()[i].state == PState::Blocked {
            let w = procs()[i].wait_for;
            if procs()[w].state == PState::Zombie {
                procs()[i].state = PState::Runnable;
            }
        }
    }

    let next = (1..=MAX_CELLS)
        .map(|k| (leaving + k) % MAX_CELLS)
        .find(|&i| user::cell_present(i) && procs()[i].state == PState::Runnable);
    let Some(n) = next else {
        panic!("nproc: no runnable cell (native process scheduler deadlock)");
    };

    // Save the outgoing cell's live FP/SIMD state (harmless if it is exiting)
    // and load the incoming cell's - the native analogue of the Linux
    // personality's `thread::save_current_fp`/`restore_current` in `linux::proc`.
    user::switch_native_cell(leaving, n);
    complete_block(n);
    user::cell_frame(n)
}

/// If cell `n` is a woken waiter (Runnable, `wait_for` pointing at a now-zombie
/// child), complete its `SYS_WAIT`: reap the child and set its return value (its
/// address space is now active). A freshly-scheduled child running for the first
/// time has `wait_for == 0` / no matching zombie, so nothing is completed.
fn complete_block(n: usize) {
    let child = procs()[n].wait_for;
    if child < MAX_CELLS
        && procs()[child].state == PState::Zombie
        && procs()[child].parent == n as i32
    {
        let code = reap(child);
        procs()[n].wait_for = 0;
        let frame = user::cell_frame(n);
        // SAFETY: `frame` is `n`'s saved trap frame.
        unsafe { arch::set_syscall_ret(&mut *frame, code) };
    }
}
