//! Native processes (docs/LIBRHEO.md Phase F): `SYS_SPAWN` / `SYS_WAIT` and the
//! cooperative cross-cell scheduler that ties them together. This EXPOSES the
//! existing Cell object (docs/ARCHITECTURE.md 3 object 1) to a native cell as
//! mechanism - it adds no new kernel object: the parent/child tree, wait status,
//! and block/wake state are per-cell synthesized state here, exactly like the
//! Linux personality's `linux::proc` (docs/LINUX-COMPAT.md L6), which this
//! mirrors for `Personality::Native` cells.
//!
//! A spawned child is a fresh native cell with its **own** address space, its own
//! mapped queue pair (so it can run librheo's reactor), and **its own capability
//! table**: it does not share the parent's (docs/ARCHITECTURE-DEBT.md 2.3), so it
//! holds only what the spawn path explicitly mints into it. Spawning is gated by
//! a **cell-spawn capability** (an `ObjectKind::Cell` cap carrying WRITE): a cell
//! without it cannot create cells (no ambient authority). Scheduling generalizes the native
//! cross-cell `SYS_SWITCH`: the parent that `SYS_WAIT`s blocks and hands the CPU
//! to a runnable child; the child's exit makes it a zombie and reschedules,
//! waking the parent whose wait is now satisfiable. Cooperative, single CPU: a
//! cell yields only at a syscall boundary. The pre-existing native `run` /
//! `SYS_SWITCH` path is untouched - a cell that never spawns has no entry here.

use crate::abi::{FAULT_EXIT, SPAWN_CHAN_SLOT};
use crate::arch::{self, TrapFrame};
use crate::capability::{ObjectKind, READ, WRITE};
use crate::idle;
use crate::ktimer::{self, TimerClient};
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
    /// Parked on the condition in `Proc::block`.
    Blocked,
    /// Exited, holding its exit code, awaiting a parent `SYS_WAIT`.
    Zombie,
}

/// What a parked native cell is waiting for (docs/ARCHITECTURE-DEBT.md 2.4).
///
/// Before this existed, the only native block was `SYS_WAIT` (recorded in
/// `Proc::wait_for`) and the other three waiting verbs - `SYS_ARM_TIMER`,
/// `SYS_WAIT_INPUT`, `SYS_WAIT_NET` - waited **inside the trap** without ever
/// reaching the scheduler, so one cell's `sleep` idled the machine while its
/// siblings were runnable. They are now registrations: the cell records its
/// condition here, the CPU goes to a sibling, and [`complete_block`] finishes the
/// syscall with the blocked cell's own address space active.
///
/// The cell-visible semantics are unchanged: a `sleep` still sleeps for its full
/// duration, a `SYS_WAIT_INPUT` still returns the bytes it drained, a
/// `SYS_WAIT_NET` still returns the frame length or 0 at its deadline.
#[derive(Copy, Clone)]
enum Block {
    None,
    /// `SYS_WAIT` for the child cell `child`.
    Wait {
        child: usize,
    },
    /// `SYS_ARM_TIMER`: parked until `deadline_ns` (absolute, timer domain). The
    /// arbiter slot the deadline lives in is kept so re-registration after another
    /// client fires goes to the caller's own slot (a pacer's and a sleep's deadlines
    /// are different clients, docs/NETSTACK.md 21).
    Timer {
        deadline_ns: u64,
        client: TimerClient,
    },
    /// `SYS_WAIT_INPUT`: parked until a console byte is buffered, or input ends.
    Console {
        buf_va: u64,
        len: usize,
    },
    /// `SYS_WAIT_NET`: parked until a frame arrives, or `deadline_ns` (absolute,
    /// timer domain; 0 = indefinite) passes.
    Net {
        buf_va: u64,
        len: usize,
        deadline_ns: u64,
    },
}

#[derive(Copy, Clone)]
struct Proc {
    /// The **cell's** state. `Blocked` means *every* vcore is parked - the condition
    /// under which the cell as a whole cannot run. For a single-vcore cell that is
    /// exactly what it always meant.
    state: PState,
    /// Parent cell index, or -1 for the top of the tree (the first spawner).
    parent: i32,
    /// Per vcore: the child cell this context is blocked in `SYS_WAIT` for.
    /// Kept alongside `vblock` because `complete_block` reaps an awaited zombie on
    /// **every** switch-in, including a plain `SYS_YIELD` (pre-existing behaviour).
    vwait: [usize; user::MAX_VCORES],
    /// Per vcore: what that context is parked on (docs/SUBSTRATE.md pillar 3).
    ///
    /// **Per vcore, not per cell**, and that is the whole of "a vcore can block": with
    /// one `Block` for the cell, one context parking on a timer would record the wait
    /// for all of them, so a runnable sibling would look blocked and the scheduler
    /// would idle the machine with work available. It is the defect the Linux side
    /// already fixed one level up with per-context `pblock` (docs/LINUX-COMPAT.md).
    vblock: [Block; user::MAX_VCORES],
    /// Exit code while `Zombie` (0..=255, or `FAULT_EXIT` for a faulted child).
    code: u64,
}

impl Proc {
    const fn free() -> Proc {
        Proc {
            state: PState::Free,
            parent: -1,
            vwait: [0; user::MAX_VCORES],
            vblock: [Block::None; user::MAX_VCORES],
            code: 0,
        }
    }
}

/// Whether vcore `v` of cell `i` is parked.
///
/// **Read from the entity table** (docs/EXECUTION-MODEL.md 9, E3): parked-ness is one fact and
/// it lives on the entity beside its owner, rather than in a per-personality array that the
/// scheduler and the personality each had to keep in step. The *reason* stays here in
/// [`Proc::vblock`] - the table's `wake` is an index, and the detail of a wake source belongs to
/// whoever owns the source.
fn parked(i: usize, v: usize) -> bool {
    // SAFETY: a read of an entity this core is deciding about.
    unsafe { crate::sched::entity::table() }
        .get(user::entity_of(i, v))
        .map(|e| e.state == crate::sched::entity::State::Parked)
        .unwrap_or(false)
}

/// The wake-source index recorded on the entity when a vcore parks.
///
/// Nonzero for every real source, because invariant I4 is "parked with nothing that can ever
/// wake it" and `NO_WAKE` is what that violation looks like. The values are opaque outside this
/// module by design: the table carries *that* there is a source, this module carries *what* it
/// is.
fn wake_index(block: Block) -> u32 {
    match block {
        Block::None => crate::sched::entity::NO_WAKE,
        Block::Wait { .. } => 1,
        Block::Timer { .. } => 2,
        Block::Console { .. } => 3,
        Block::Net { .. } => 4,
    }
}

/// Whether every **live** vcore of cell `i` is parked - the condition for the cell-level
/// `PState::Blocked`. Trivially "is vcore 0 parked" for a single-vcore cell.
///
/// Live, because a cell outlives its first vcore's exit: an exited vcore is neither parked
/// nor runnable, and counting it as unparked would leave the cell `Runnable` with nothing
/// to enter.
fn all_parked(i: usize) -> bool {
    // One implementation, in the table (E3). It differs from the loop this replaced in one
    // case, and the table's answer is the right one: a cell whose vcores have **all exited** is
    // *finished*, not blocked, where `.all()` over an empty set said blocked. Unreachable from
    // the caller below - the vcore that just parked is live - and correct for any caller that
    // is not.
    // SAFETY: a read.
    unsafe { crate::sched::entity::table() }.all_parked(i as u16)
}

/// The first vcore of cell `i` this CPU may enter that is not parked, preferring the
/// one already running so a switch that need not happen does not.
///
/// Owner-checked (`user::vcore_on_this_cpu`): a cell's vcores may be spread over
/// several cores, and entering one another core is inside is what the per-CPU entry
/// guard exists to catch.
fn runnable_vcore(i: usize) -> Option<usize> {
    let n = user::cell_vcores(i).max(1);
    let here = user::current_vcore();
    (0..n)
        .map(|step| (here + step) % n)
        // `vcore_on_this_cpu` implies live, so an exited context is never picked.
        .find(|&v| !parked(i, v) && user::vcore_on_this_cpu(i, v))
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
            vwait: [0; user::MAX_VCORES],
            vblock: [Block::None; user::MAX_VCORES],
            code: 0,
        };
    }
}

// -------------------------------------------------------------------- spawn

/// `SYS_SPAWN(path_va, path_len, argv_va, envp_va, chan_spec)`: load the ELF at
/// `path` from the VFS into a new native cell, build its initial stack from the
/// caller's argv/envp, map it a queue pair + mint a queue capability into the
/// **child's own** capability table, and record the caller as parent. Returns the child's handle (its
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
    // The **eager** loader, deliberately: a native cell has no VMA list, so there is
    // nothing here to turn a recorded segment into a mapping. Demand paging is a Linux
    // personality feature (`load::exec_elf_from_vfs_demand`), and a native child given
    // a lazy image gets an address space full of holes with no diagnostic - which is
    // what happened when the two shared one function (docs/ENGINEERING.md 11).
    let Some(img) = load::exec_elf_from_vfs(ops, path_kva, path_len as u64, &mut child_aspace)
    else {
        return u64::MAX;
    };
    debug_assert!(
        img.nsegs == 0,
        "native spawn cannot map demand-paged segments"
    );
    let entry = img.entry;

    // Build the native SysV initial stack (argc/argv/envp). Its SP points at
    // argc; we pass that SP as the child's arg0 so librheo's `_start` finds its
    // arguments without a naked prologue (an explicit, native alternative to
    // walking the raw SP).
    let sp = load::setup_stack(&mut child_aspace, &argv[..argc], &envp[..envc]);

    // Map the child a queue-pair region. Its queue capability is minted **into
    // the child's own table** after `install_spawned` publishes that table - a
    // spawned cell no longer shares the parent's (docs/ARCHITECTURE-DEBT.md
    // 2.3), so the mint has to happen where the child can reach it.
    let qp = load::map_queue(&mut child_aspace);

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
            // The capability itself is minted into the child's table below,
            // for the same reason as the queue's.
            Some((child_chan_va, p_role ^ 1))
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

    // Install first (with an empty capability table and no queue cap yet), then
    // mint into the child's *own* table. Ordering matters now that the tables
    // are separate: `mint_into` reaches the table through the installed cell.
    // SAFETY: pointers are kernel-owned statics that outlive the child's run.
    unsafe {
        user::install_spawned(
            child,
            aspace_ptr,
            qp_ptr,
            frame_ptr,
            cur,
            load::USER_QUEUE_VA as u64,
            0,
        );
    }
    // The child's queue capability. Without it the child's very first doorbell
    // fails its grant check, so a failure here has to unwind the whole spawn
    // rather than hand back a cell that cannot use its ring.
    let Some(qp_cap_id) = user::mint_into(child, ObjectKind::QueuePair, READ | WRITE) else {
        user::free_cell(child);
        return u64::MAX;
    };
    user::set_queue_info(child, load::USER_QUEUE_VA as u64, qp_cap_id);

    // Record the inherited channel so the child's `SYS_CONNECT` reports its end.
    if let Some((va, role)) = child_chan {
        let Some(cap) = user::mint_into(child, ObjectKind::QueuePair, READ | WRITE) else {
            user::free_cell(child);
            return u64::MAX;
        };
        user::set_channel_info(child, va, cap, role);
    }
    procs()[child] = Proc {
        state: PState::Runnable,
        parent: cur as i32,
        vwait: [0; user::MAX_VCORES],
        vblock: [Block::None; user::MAX_VCORES],
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
    // Park the calling **vcore** and hand the CPU on (to the child, or to a sibling
    // vcore of this cell that is still runnable).
    let v = user::current_vcore();
    procs()[cur].vwait[v] = child;
    Sched::Switch(park_vcore(cur, v, Block::Wait { child }))
}

// ------------------------------------------------- the three converted waits
//
// docs/ARCHITECTURE-DEBT.md 2.4. Each of these used to wait in kernel context
// inside its own syscall; each now registers its condition and hands the CPU to a
// sibling. Each returns `None` when the caller is the **only** schedulable cell, in
// which case the syscall keeps its pre-existing in-trap wait byte for byte - there
// is nothing to reschedule to, and the in-trap wait *is* the idle. That is what
// makes this change additive for every single-cell kernel in the tree
// (docs/ENGINEERING.md 8).

/// `SYS_ARM_TIMER`: register the deadline and reschedule, or `None` to wait in
/// place. `deadline_ns` is the caller's **relative** duration.
pub fn block_timer(cur: usize, deadline_ns: u64, client: TimerClient) -> Option<*mut TrapFrame> {
    if deadline_ns == 0 || !can_reschedule(cur) {
        return None;
    }
    ensure_tracked(cur);
    ktimer::register(client, deadline_ns);
    let deadline = ktimer::now_ns().wrapping_add(deadline_ns.max(1));
    park(
        cur,
        Block::Timer {
            deadline_ns: deadline,
            client,
        },
    )
}

/// `SYS_WAIT_INPUT`: register the console wait and reschedule, or `None` to wait in
/// place. A buffer already holding data is completed here rather than parked, so a
/// cell reading available input never leaves the CPU.
///
/// # Safety
/// `buf_va` must be a writable `len`-byte buffer in the caller's address space; the
/// caller (the syscall dispatch) has range-checked it (docs/ENGINEERING.md 12).
pub unsafe fn block_console(cur: usize, buf_va: u64, len: usize) -> Option<*mut TrapFrame> {
    if len == 0 || !can_reschedule(cur) {
        return None;
    }
    // Bytes are already buffered: let `wait_input` return them directly rather than
    // parking. Deliberately a *peek* (`has_data`) and not a drain - draining here and
    // then letting `wait_input` drain again would write the first bytes to the
    // caller's buffer and immediately overwrite them with the next ones.
    if crate::input::has_data() {
        return None;
    }
    ensure_tracked(cur);
    park(cur, Block::Console { buf_va, len })
}

/// `SYS_WAIT_NET`: register the frame wait (with its deadline) and reschedule, or
/// `None` to wait in place.
///
/// # Safety
/// As [`block_console`]: `buf_va` is a range-checked writable buffer in the caller.
pub unsafe fn block_net(
    cur: usize,
    buf_va: u64,
    len: usize,
    timeout_ns: u64,
) -> Option<*mut TrapFrame> {
    if len == 0 || !can_reschedule(cur) || crate::net_rx::frame_pending() {
        return None;
    }
    // Two cases that must keep the in-trap wait, because parking on them could
    // never end (docs/ENGINEERING.md 11 - a wait whose condition cannot occur is a
    // wedge, and the in-trap path has the backstops):
    //  * no NIC installed at all - `wait_frame` answers 0 immediately;
    //  * an *indefinite* wait with no NIC RX interrupt - only `wait_frame`'s bounded
    //    poll (its `POLL_BUDGET` backstop) can end that, and a backstop the
    //    scheduler idle does not have must not be bypassed.
    if !crate::net_rx::nic_present() || (timeout_ns == 0 && !crate::arch::net_irq_enabled()) {
        return None;
    }
    ensure_tracked(cur);
    let deadline_ns = if timeout_ns == 0 {
        0
    } else {
        ktimer::register(TimerClient::RxDeadline, timeout_ns);
        ktimer::now_ns().wrapping_add(timeout_ns.max(1))
    };
    park(
        cur,
        Block::Net {
            buf_va,
            len,
            deadline_ns,
        },
    )
}

/// Mark `cur` blocked on `block` and hand the CPU on.
fn park(cur: usize, block: Block) -> Option<*mut TrapFrame> {
    Some(park_vcore(cur, user::current_vcore(), block))
}

/// Park vcore `v` of cell `cur` on `block` and hand the CPU on.
///
/// The cell becomes `Blocked` only when **every** vcore is parked, which is what makes a
/// single-vcore cell's behaviour identical to what it was: parking its one vcore parks
/// them all. A cell with a runnable sibling stays `Runnable`, so the scheduler keeps
/// seeing it - the whole point (docs/SUBSTRATE.md pillar 3).
fn park_vcore(cur: usize, v: usize, block: Block) -> *mut TrapFrame {
    procs()[cur].vblock[v] = block;
    // SAFETY: this core is parking the vcore it is inside.
    unsafe { crate::sched::entity::table() }.park(user::entity_of(cur, v), wake_index(block));
    if all_parked(cur) {
        procs()[cur].state = PState::Blocked;
    }
    reschedule(cur)
}

/// Whether blocking `cur` can hand the CPU to some **other** cell - either one that
/// is runnable now, or one that is itself parked on a wake source the scheduler idle
/// state can wait for (that cell will run again, so parking `cur` is progress).
/// False only in the genuinely single-cell case, where the syscall keeps its in-trap
/// wait unchanged and that wait *is* the idle.
fn can_reschedule(cur: usize) -> bool {
    // A runnable **sibling vcore of this same cell** counts, and is the cheapest place to
    // hand the CPU (docs/SUBSTRATE.md pillar 3). Without this the whole check falls to
    // "is another *cell* runnable", so a single multi-vcore cell would take the in-trap
    // wait and its sibling context would not run at all - the block would be per cell in
    // effect even though the state is per vcore.
    // Runnable, or itself parked on something the scheduler idle can wait for - the same
    // two-part rule the cell branch below uses, and for the same reason: a sibling that
    // will run again means parking is progress.
    if (0..user::cell_vcores(cur).max(1)).any(|v| {
        v != user::current_vcore()
            && user::vcore_on_this_cpu(cur, v)
            && (!parked(cur, v) || sources_of(cur, v) & idle::WAITABLE != 0)
    }) {
        return true;
    }
    (0..MAX_CELLS).any(|i| {
        i != cur
            && (schedulable(i)
                || (user::cell_present(i)
                    && procs()[i].state == PState::Blocked
                    && sources_of(i, 0) & idle::WAITABLE != 0))
    })
}

/// Give every present native cell a process entry before parking `cell`, so
/// `reschedule`'s round-robin (which looks for `Runnable`) can reach them.
///
/// A test kernel may install two native cells that never spawn (the Phase E/J
/// shape), which leaves both `Proc` slots `Free`. `SYS_YIELD`'s `schedulable`
/// already treats `Free` as runnable, but `reschedule` deliberately keeps its
/// pre-existing `Runnable` predicate (it must not start scheduling cells the Phase F
/// proofs never gave it), so the entries are materialised here instead. Idempotent.
fn ensure_tracked(cell: usize) {
    ensure_top(cell);
    for i in 0..MAX_CELLS {
        if i != cell && user::cell_present(i) && user::cell_is_native(i) {
            ensure_top(i);
        }
    }
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
    // A yield charges the CPU time used and ends the burst, but the caller **stays
    // runnable** - it is still competing (docs/SUBSTRATE.md pillar 3). Marking it
    // blocked here would withdraw its weight from the queue's total and make every
    // sibling's virtual clock advance too fast.
    crate::sched::dispatch::yielded();
    // **A sibling vcore of this same cell first** (docs/SUBSTRATE.md pillar 3), for the
    // reason the Linux preemption path tries a sibling context first: it is the cheaper
    // move by a wide margin - one address space, so no `activate()` and no TLB
    // consequence, only the FP file and the frame change hands - and it is the move a
    // strand runtime with more vcores than cores actually needs. Round-robin from the
    // running vcore so N vcores share the core evenly rather than two of them trading it.
    if let Some(to) = next_sibling_vcore(cur) {
        let frame = user::switch_native_vcore(cur, user::current_vcore(), to);
        crate::sched::dispatch::running(cur, 0);
        return Sched::Switch(frame);
    }
    // The order is the ready queue's when dispatch is enabled; `schedulable` stays
    // the sole authority on who may run. Note the range: a yield deliberately
    // excludes the caller (`1..MAX_CELLS`), so "yield to nobody" is `Ret(0)` rather
    // than a self-switch.
    let Some(next) = crate::sched::dispatch::pick_excluding_self(cur, MAX_CELLS, schedulable)
    else {
        return Sched::Ret(0);
    };
    // The native cross-cell switch, FP/SIMD register file included: this is a
    // hard-float cell's hand-off point (docs/LIBRHEO.md, docs/ENGINEERING.md 3),
    // and a service cell reaches it on every client round.
    let v = runnable_vcore(next).expect("`schedulable` required an enterable vcore");
    user::switch_native_cell_vcore(cur, next, v);
    crate::sched::dispatch::running(next, 0);
    complete_block(next, v);
    Sched::Switch(user::vcore_frame(next, v))
}

/// **Preempt** native cell `cur` in favour of another runnable native cell,
/// returning the frame to resume, or `None` when there is no other cell to run.
///
/// Called from [`crate::user::on_user_interrupt`]. A native cell has a single
/// execution context, so unlike the Linux path there is no cheaper intra-cell move
/// to try first - the only preemption available is to another cell.
///
/// `cur` stays runnable (it was taken off the CPU, it did not block), and the switch
/// is [`user::switch_native_cell`] - **the** native cross-cell switch, which swaps
/// the FP/SIMD register file as well as the address space. Using the bare
/// `switch_to_cell` here would silently corrupt a hard-float cell's vector registers,
/// which is the exact defect the `SYS_YIELD` scar records (docs/LIBRHEO.md, "FP/SIMD
/// across the native cross-cell switch"): preemption is a **fourth** path into that
/// invariant, and it holds here for the same structural reason the other three do.
pub fn preempt_cell(cur: usize) -> Option<*mut TrapFrame> {
    // The vector-register file is saved **first**, before the wake scan and the pick
    // run any kernel code: on x86-64 an ordinary struct move or a `compiler_builtins`
    // `mem*` call uses vector registers even in a soft-float kernel, and a preemption
    // lands at an arbitrary instruction inside the cell's own vector code
    // (`user::on_user_interrupt` carries the full argument). `switch_native_cell`
    // would otherwise do the save here, after that work.
    // The **running vcore's** area, not the cell's first: a multi-vcore cell preempted
    // on this core is inside whichever context this CPU entered, and saving to vcore 0
    // would write one context's live registers over another's saved image
    // (docs/SUBSTRATE.md pillar 3): `cur` is whatever context this core is inside, and
    // saving to vcore 0 would write one context's live registers over another's image.
    user::save_native_fp_vcore(cur, user::current_vcore());
    wake_satisfiable();
    let next = crate::sched::dispatch::pick_excluding_self(cur, MAX_CELLS, schedulable)?;
    // Deliberately **not** `switch_native_cell`: its first action is the save that
    // already happened above, and doing it twice would overwrite the good image with
    // whatever the pick left in the registers. The invariant CLAUDE.md states - every
    // native cross-cell switch swaps the FP/SIMD register file - holds here in two
    // stages rather than one, which is why this is the only site allowed to say so.
    let v = runnable_vcore(next).expect("`schedulable` required an enterable vcore");
    user::switch_to_cell(next);
    user::restore_native_fp_vcore(next, v);
    user::set_current_vcore(v);
    complete_block(next, v);
    Some(user::vcore_frame(next, v))
}

/// The next vcore of cell `cur` this CPU may enter, scanning round-robin from the one
/// running, or `None` when the cell has no other vcore this core owns.
///
/// Owner-checked rather than assumed: a cell's vcores may be spread over several cores by
/// the placement path, and entering one another core is inside is the corruption the
/// per-CPU entry guard exists to catch (`user::double_entries`). An unclaimed sibling is
/// enterable, which is what makes a single-core boot's behaviour the obvious one.
fn next_sibling_vcore(cur: usize) -> Option<usize> {
    let n = user::cell_vcores(cur);
    if n < 2 {
        return None;
    }
    let here = user::current_vcore();
    (1..n)
        .map(|step| (here + step) % n)
        // **Not parked**, as well as owned. This check was absent while a vcore could not
        // block, and its absence was invisible until one could: a yield then entered a
        // sibling parked mid-`SYS_ARM_TIMER`, which resumed it at its syscall return with
        // the return register still holding the syscall *number* - no fault and no log,
        // just a wrong answer from a wait that had not finished. Found by the `schedidle`
        // vcore phase (docs/SUBSTRATE.md pillar 3).
        .find(|&v| !parked(cur, v) && user::vcore_on_this_cpu(cur, v))
}

/// Whether cell `i` can be resumed by a yield: present, native, and either a
/// runnable member of a native process tree or a cell with no tree state at all
/// (an installed Phase E/J peer, whose `Proc` slot is `Free`). A `Blocked` waiter
/// or a `Zombie` is skipped - `reschedule` owns waking those.
fn schedulable(i: usize) -> bool {
    user::cell_present(i)
        && user::cell_is_native(i)
        // A cell belongs to one core (docs/SMP.md 10.0). Constant-true on every
        // single-core boot, because nothing there ever claims a cell.
        && user::cell_on_this_cpu(i)
        && matches!(procs()[i].state, PState::Free | PState::Runnable)
        // And it must have a context this core may enter that is not parked. A `Free`
        // cell has no `Proc` entry, so nothing is parked and this is trivially true -
        // which is the pre-vcore behaviour for the Phase E/J cells that never spawn.
        && runnable_vcore(i).is_some()
}

/// `SYS_EXIT` from a vcore of a cell that has **live siblings**: end just this vcore and
/// hand the CPU on. Returns the frame to resume, or `None` when this was the cell's last
/// vcore - in which case the caller ends the cell exactly as it did before vcores existed.
///
/// The last vcore out ends the cell; an earlier one only ends itself. Without that a cell
/// with four vcores dies when the first finishes, which is the process/thread split the
/// Linux personality already has one level up (docs/SMP.md 10.0a).
///
/// The hand-off goes through [`reschedule`], which is what makes a *parked* sibling work:
/// the exiting vcore leaves, nothing is runnable, and the scheduler idles on the sibling's
/// own wake source until it is satisfiable - the same path a block takes.
pub fn retire_vcore(cur: usize, code: u64) -> Option<*mut TrapFrame> {
    let v = user::current_vcore();
    // Hand off only to a sibling **this core may enter**: live, owned here, and either
    // runnable or parked on something the scheduler idle can wait for - the same two-part
    // rule `can_reschedule` uses.
    //
    // If every live sibling belongs to another core there is nothing for this core to do,
    // and that is **not** a deadlock: it is this core's run ending while the cell continues
    // elsewhere. Returning `None` unwinds with this vcore's own outcome, which is what the
    // placement path reports - the first version called `reschedule` unconditionally and
    // got `DEADLOCK_EXIT` for exactly this case, on the two-cores-one-cell phase.
    let handoff = (0..user::cell_vcores(cur)).any(|w| {
        w != v
            && user::vcore_on_this_cpu(cur, w)
            && (!parked(cur, w) || sources_of(cur, w) & idle::WAITABLE != 0)
    });
    if !handoff {
        return None;
    }
    // `reschedule` picks only `Runnable` procs, and a cell that has never spawned or
    // blocked has no `Proc` entry at all - the reason `ensure_tracked` exists. Without it
    // this hands the CPU to nobody and the run ends in `DEADLOCK_EXIT` (observed).
    ensure_tracked(cur);
    // The register file is saved before anything else runs, for the reason
    // `preempt_cell` gives: on x86-64 even a soft-float kernel's struct moves use vector
    // registers. Harmless for an exiting context, and it keeps one rule at every site.
    user::save_native_fp_vcore(cur, v);
    user::retire_vcore(cur, v, user::Outcome::Exited(code));
    // An exited vcore is not parked, so its slot needs clearing: `refresh_deadlines` and
    // `blocked_sources` iterate parked vcores, and a stale entry would arm a deadline for
    // a context that can never be entered.
    // `retire_vcore` above already set the entity `Exited`, which is neither parked nor
    // runnable - so the parked flag has nowhere left to be stale. The block detail is still
    // this module's and is cleared here.
    procs()[cur].vblock[v] = Block::None;
    procs()[cur].vwait[v] = 0;
    Some(reschedule(cur))
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
/// Wakes any blocked cell whose condition now holds, round-robins to a runnable
/// cell, and completes its pending block.
///
/// When nothing is runnable but at least one cell is parked on a **wake source**,
/// this **idles** ([`crate::idle`]) until a source can have fired and looks again -
/// which is the state that used to `panic!("no runnable cell")`
/// (docs/ARCHITECTURE-DEBT.md 2.4). "Every cell is waiting for the outside world" is
/// the normal steady state of a server, not a scheduling bug. Only when nothing is
/// runnable *and* no blocked cell has any source left is it a genuine deadlock, and
/// that is reported (see [`report_deadlock`]) rather than panicked.
fn reschedule(leaving: usize) -> *mut TrapFrame {
    // The leaving cell blocked or exited: charge its CPU time and end its burst
    // voluntarily (docs/SUBSTRATE.md pillar 3, migration S3').
    crate::sched::dispatch::relinquish();
    loop {
        wake_satisfiable();

        // Order from the ready queue when enabled, the pre-migration round-robin
        // when not; the predicate stays the authority on runnability.
        let next = crate::sched::dispatch::pick(leaving, MAX_CELLS, |i| {
            user::cell_present(i)
                && procs()[i].state == PState::Runnable
                // A cell is only pickable if it has a vcore this core may enter that
                // is not parked. For a single-vcore cell that is "vcore 0 is not
                // parked", which `PState::Runnable` already implied, so nothing
                // changes there.
                && runnable_vcore(i).is_some()
        });
        if let Some(n) = next {
            // Which **context** of `n` to enter. A cell that blocked one vcore and left
            // a sibling runnable is entered at the sibling; a cell whose parked vcore
            // was just woken is entered at that vcore, which is what completes its
            // syscall in its own address space.
            let v = runnable_vcore(n).expect("the pick predicate required one");
            if n != leaving {
                // Save the outgoing cell's live FP/SIMD state (harmless if it is
                // exiting) and load the incoming context's - the native analogue of the
                // Linux personality's `thread::save_current_fp`/`restore_current`.
                user::switch_native_cell_vcore(leaving, n, v);
            } else if v != user::current_vcore() {
                // Same cell, different context: the cheap switch - no `activate()`,
                // only the FP file and the frame.
                user::switch_native_vcore(n, user::current_vcore(), v);
            }
            crate::sched::dispatch::running(n, 0);
            complete_block(n, v);
            return user::vcore_frame(n, v);
        }

        // Nothing runnable. Idle on whatever the blocked cells are waiting for.
        let src = blocked_sources();
        if src & idle::WAITABLE == 0 {
            return report_deadlock(leaving, src);
        }
        refresh_deadlines();
        idle::wait(src);
    }
}

/// Promote every blocked cell whose condition now holds to `Runnable`.
fn wake_satisfiable() {
    for i in 0..MAX_CELLS {
        if procs()[i].state == PState::Free || procs()[i].state == PState::Zombie {
            continue;
        }
        let mut woke = false;
        for v in 0..user::cell_vcores(i).max(1) {
            if parked(i, v) && satisfiable(i, v) {
                // SAFETY: waking an entity whose source this core has just judged satisfiable.
                unsafe { crate::sched::entity::table() }.wake(user::entity_of(i, v));
                woke = true;
            }
        }
        // A cell is runnable again as soon as **one** of its vcores is. For a
        // single-vcore cell that is the old `Blocked -> Runnable` transition exactly.
        if woke && procs()[i].state == PState::Blocked {
            procs()[i].state = PState::Runnable;
        }
    }
}

/// Whether parked vcore `v` of cell `i` has its condition met now.
fn satisfiable(i: usize, v: usize) -> bool {
    match procs()[i].vblock[v] {
        Block::None => false,
        // Pre-existing behaviour, now expressed through `Block`: a `SYS_WAIT`er wakes
        // when its awaited child is a zombie.
        Block::Wait { child } => child < MAX_CELLS && procs()[child].state == PState::Zombie,
        Block::Timer { deadline_ns, .. } => reached(deadline_ns),
        Block::Console { .. } => crate::input::has_data() || crate::input::at_eof(),
        Block::Net { deadline_ns, .. } => {
            crate::net_rx::frame_pending() || (deadline_ns != 0 && reached(deadline_ns))
        }
    }
}

/// Re-register the **nearest** outstanding deadline in each arbiter slot before the
/// scheduler idles.
///
/// The arbiter has one slot per *client kind*, not per cell (docs/NETSTACK.md 16 -
/// the client set is closed so the kernel stays allocation-free). Two cells sleeping
/// at once therefore share `CellSleep`, and whichever registered last would be the
/// only deadline the hardware is armed for - so the other cell would wake late, by
/// however much the two deadlines differ. The scheduler multiplexes them: each
/// blocked cell's deadline is held in its own `Block` (absolute, timer domain), and
/// the nearest of them is what the slot gets before every idle. Satisfiability is
/// judged from the `Block`, never from the slot, so a cell can never be woken by
/// another cell's deadline either.
fn refresh_deadlines() {
    let now = ktimer::now_ns();
    let mut nearest = [u64::MAX; ktimer::CLIENTS];
    let mut net_nearest = u64::MAX;
    for (i, v) in parked_vcores() {
        match procs()[i].vblock[v] {
            Block::Timer {
                deadline_ns,
                client,
            } => {
                let s = &mut nearest[client as usize];
                if deadline_ns < *s {
                    *s = deadline_ns;
                }
            }
            Block::Net { deadline_ns, .. } if deadline_ns != 0 && deadline_ns < net_nearest => {
                net_nearest = deadline_ns;
            }
            _ => {}
        }
    }
    for (slot, &deadline) in nearest.iter().enumerate() {
        if deadline != u64::MAX {
            ktimer::register(client_of(slot), deadline.wrapping_sub(now).max(1));
        }
    }
    if net_nearest != u64::MAX {
        ktimer::register(
            TimerClient::RxDeadline,
            net_nearest.wrapping_sub(now).max(1),
        );
    }
}

/// The [`TimerClient`] for arbiter slot index `slot`. The enum is `#[repr]`-free, so
/// the mapping is written out rather than transmuted.
fn client_of(slot: usize) -> TimerClient {
    match slot {
        0 => TimerClient::RxPoll,
        1 => TimerClient::RxDeadline,
        2 => TimerClient::CellSleep,
        3 => TimerClient::NetTimer,
        4 => TimerClient::Pacer,
        _ => TimerClient::FutexWait,
    }
}

/// Whether the absolute timer-domain deadline `deadline_ns` has passed. Compared in
/// the **timer's own** domain (`ktimer::now_ns`), never the instruction counter -
/// they are different counters on RISC-V (docs/ENGINEERING.md 11).
fn reached(deadline_ns: u64) -> bool {
    ktimer::now_ns().wrapping_sub(deadline_ns) < (1 << 63)
}

/// The union of the wake sources every blocked cell is waiting on
/// ([`crate::idle`]). Zero means nothing is blocked; a value with no
/// [`idle::WAITABLE`] bit means every blocked cell is waiting on another cell, which
/// with nothing runnable is a deadlock.
fn blocked_sources() -> idle::Sources {
    let mut src = 0;
    for (i, v) in parked_vcores() {
        src |= sources_of(i, v);
    }
    src
}

/// Every parked `(cell, vcore)` pair. Iterated rather than gated on the cell-level
/// `PState::Blocked`, because a cell with one parked vcore and one runnable is **not**
/// blocked and its parked context's deadline still has to be armed - the arming is what
/// wakes it. Yields exactly vcore 0 of each blocked cell when nothing has extra vcores,
/// which is the pre-vcore behaviour.
fn parked_vcores() -> impl Iterator<Item = (usize, usize)> {
    (0..MAX_CELLS).flat_map(|i| {
        (0..user::cell_vcores(i).max(1))
            .filter(move |&v| parked(i, v))
            .map(move |v| (i, v))
    })
}

/// The wake sources vcore `v` of cell `i`'s current block can be satisfied by.
fn sources_of(i: usize, v: usize) -> idle::Sources {
    match procs()[i].vblock[v] {
        Block::None => 0,
        Block::Wait { .. } => idle::PEER,
        Block::Timer { .. } => idle::TIMER,
        Block::Console { .. } => idle::CONSOLE,
        // A bounded receive also waits on its deadline.
        Block::Net { deadline_ns, .. } => {
            idle::NET | if deadline_ns != 0 { idle::TIMER } else { 0 }
        }
    }
}

/// The union of wake sources the native scheduler is currently blocked on - the
/// classifier the run loop's idle/deadlock decision is made from, exposed so a test
/// can assert it directly (docs/ENGINEERING.md 1: assert the decision, not the
/// consequence).
pub fn wake_sources() -> idle::Sources {
    blocked_sources()
}

/// No cell is runnable and no blocked cell has a wake source left: a genuine
/// deadlock. Print what each blocked cell is waiting for and end the run with
/// [`crate::abi::DEADLOCK_EXIT`], rather than `panic!`ing with a kernel stack trace
/// that says nothing about the cells (docs/ARCHITECTURE-DEBT.md 2.4).
fn report_deadlock(leaving: usize, src: idle::Sources) -> *mut TrapFrame {
    crate::println!(
        "nproc: DEADLOCK - no runnable native cell, no wake source (leaving={leaving}, waiting on {})",
        idle::describe(src)
    );
    for (i, v) in parked_vcores() {
        crate::println!(
            "nproc:   cell {i} vcore {v} blocked on {}",
            block_name(i, v)
        );
    }
    user::deadlock_finish()
}

/// The name of vcore `v` of cell `i`'s block, for the deadlock diagnostic.
fn block_name(i: usize, v: usize) -> &'static str {
    match procs()[i].vblock[v] {
        Block::None => "nothing",
        Block::Wait { .. } => "SYS_WAIT (child exit)",
        Block::Timer { .. } => "SYS_ARM_TIMER (deadline)",
        Block::Console { .. } => "SYS_WAIT_INPUT (console)",
        Block::Net { .. } => "SYS_WAIT_NET (frame)",
    }
}

/// Apply woken cell `n`'s pending syscall - its address space is now active - and
/// set its return value, then clear the block.
///
/// `SYS_WAIT` keeps its pre-existing shape exactly: a zombie the cell was waiting
/// for is reaped on **every** switch-in (including a plain `SYS_YIELD`), keyed on
/// `wait_for`, because that is what the Phase F/N4a proofs observe.
fn complete_block(n: usize, v: usize) {
    let child = procs()[n].vwait[v];
    if child < MAX_CELLS
        && procs()[child].state == PState::Zombie
        && procs()[child].parent == n as i32
    {
        let code = reap(child);
        procs()[n].vwait[v] = 0;
        procs()[n].vblock[v] = Block::None;
        let frame = user::vcore_frame(n, v);
        // SAFETY: `frame` is the saved trap frame of `n`'s vcore `v`.
        unsafe { arch::set_syscall_ret(&mut *frame, code) };
        return;
    }
    let block = procs()[n].vblock[v];
    procs()[n].vblock[v] = Block::None;
    let r: u64 = match block {
        Block::None | Block::Wait { .. } => return,
        Block::Timer { client, .. } => {
            ktimer::cancel(client);
            0
        }
        // SAFETY: `buf_va`/`len` were range-checked against **this** cell's user VA
        // range when the block was registered, and `n`'s address space is active
        // again here (`switch_native_cell` above, or `n == leaving`).
        Block::Console { buf_va, len } => unsafe { crate::input::drain(buf_va, len) as u64 },
        Block::Net {
            buf_va,
            len,
            deadline_ns,
        } => {
            if deadline_ns != 0 {
                ktimer::cancel(TimerClient::RxDeadline);
            }
            // SAFETY: as `Block::Console` above.
            unsafe { crate::net_rx::complete_wait(buf_va, len) as u64 }
        }
    };
    let frame = user::vcore_frame(n, v);
    // SAFETY: `frame` is the saved trap frame of `n`'s vcore `v`.
    unsafe { arch::set_syscall_ret(&mut *frame, r) };
}
