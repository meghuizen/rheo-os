//! The user-mode run loop and syscall dispatch (BUILD-ORDER.md step 5).
//! Portable: the arch trampolines save/restore U-mode state and call
//! `on_user_trap`; this module owns the policy - which cell is current,
//! what each syscall does, and how a cell's run ends.
//!
//! Cross-cell switching (`SYS_SWITCH`) is a directed hand-off between two
//! user cells with a real address-space switch each way - the mechanism
//! the P5 benchmark measures against seL4's cross-vspace IPC.
//!
//! Global mutable state with raw pointers is deliberate here: this is the
//! per-CPU scheduler, there is one CPU at this stage, and traps are
//! synchronous, so there is no concurrent access to guard against.

use crate::abi::{SYS_CYCLES, SYS_DOORBELL, SYS_EXIT, SYS_EXIT_GROUP, SYS_MMAP, SYS_SWITCH};
use crate::arch::{self, FaultCause, MapPerm, TrapFrame, TrapKind};
use crate::capability::{CapTable, ObjectTable};
use crate::mm::{AddressSpace, frames};
use crate::queue::{self, QueuePair};

/// Base VA of the per-cell anonymous mmap region (docs/USERLAND.md M2): 12 GiB,
/// above the image (1-4 GiB) and stack (8 GiB), free in every cell root.
const MMAP_BASE: usize = 0x3_0000_0000;
static mut MMAP_NEXT: usize = MMAP_BASE;

/// Why a cell's run ended.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    Exited(u64),
    Faulted(usize),
}

/// Which syscall ABI a cell speaks (docs/LINUX-COMPAT.md 2). Dispatch
/// branches on this BEFORE interpreting the syscall number - native numbers
/// 1-30 collide with Linux numbers (Linux x86-64 `write` = 1 is native
/// `SYS_DOORBELL`), so the tag, not the number, decides the table.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Personality {
    /// The native rheo-os ABI (kernel/src/abi.rs) - the default.
    Native,
    /// The Linux syscall ABI, translated by `crate::linux`.
    Linux,
}

#[derive(Copy, Clone)]
struct RunCell {
    aspace: *const AddressSpace,
    caps: *mut CapTable,
    objects: *const ObjectTable,
    qp: *const QueuePair,
    frame: *mut TrapFrame,
    outcome: Option<Outcome>,
    present: bool,
    personality: Personality,
}

const EMPTY: RunCell = RunCell {
    aspace: core::ptr::null(),
    caps: core::ptr::null_mut(),
    objects: core::ptr::null(),
    qp: core::ptr::null(),
    frame: core::ptr::null_mut(),
    outcome: None,
    present: false,
    personality: Personality::Native,
};

/// Number of runnable cell slots. Bumped 2 -> 8 for the Linux personality
/// (docs/LINUX-COMPAT.md L2): several Linux cells plus the peer used by the
/// native `SYS_SWITCH` test. `SYS_SWITCH`'s `cur ^ 1` pairing is unaffected.
pub const MAX_CELLS: usize = 8;

static mut CELLS: [RunCell; MAX_CELLS] = [EMPTY; MAX_CELLS];
static mut CURRENT: usize = 0;
static mut EXITED: usize = 0;

fn cells() -> &'static mut [RunCell; MAX_CELLS] {
    // SAFETY: single CPU, synchronous traps; no aliasing run concurrently.
    unsafe { &mut *core::ptr::addr_of_mut!(CELLS) }
}

/// Clear the run table (call before installing a fresh set of cells).
pub fn reset() {
    *cells() = [EMPTY; MAX_CELLS];
    // SAFETY: single CPU, between runs.
    unsafe {
        *core::ptr::addr_of_mut!(MMAP_NEXT) = MMAP_BASE;
    }
    crate::linux::reset();
}

/// The index of the currently running cell (docs/LINUX-COMPAT.md L2). The
/// Linux personality keys its per-cell state (fd table, brk, mmap cursor) on
/// this.
pub fn current_index() -> usize {
    unsafe { *core::ptr::addr_of!(CURRENT) }
}

/// The installed frame pointer for cell `idx`. The Linux personality's thread
/// table uses this as its initial (thread 0) execution context
/// (docs/LINUX-COMPAT.md L4); clone-created threads get kernel-owned frames.
pub fn cell_frame(idx: usize) -> *mut TrapFrame {
    cells()[idx].frame
}

/// Run `f` against the current cell's address space, then re-activate it so
/// any new/removed mappings take effect (TLB flush). The mechanism the Linux
/// personality's memory syscalls (brk/mmap/munmap/mprotect) build on; mirrors
/// how `mmap_anon` publishes fresh mappings.
pub fn with_current_aspace<R>(f: impl FnOnce(&mut AddressSpace) -> R) -> R {
    let cell = cells()[current_index()];
    // SAFETY: single CPU; the running cell's address space is uniquely owned
    // for the duration of the synchronous trap.
    let aspace = unsafe { &mut *(cell.aspace as *mut AddressSpace) };
    let r = f(aspace);
    aspace.activate();
    r
}

/// Map fresh zeroed frames for `[va, va+len)` in the current cell with `perm`.
/// `va` and `len` need not be page-aligned; whole overlapping pages are mapped.
pub fn map_anon_at(va: usize, len: usize, perm: MapPerm) {
    if len == 0 {
        return;
    }
    let base = va & !(frames::FRAME_SIZE - 1);
    let end = va + len;
    with_current_aspace(|aspace| {
        let mut a = base;
        while a < end {
            let pa = frames::alloc();
            aspace.map_user_frame(a, pa, perm);
            a += frames::FRAME_SIZE;
        }
    });
}

/// Unmap every whole page in `[va, va+len)` in the current cell and free the
/// frames. Pages that were not mapped are skipped.
pub fn unmap_range(va: usize, len: usize) {
    if len == 0 {
        return;
    }
    let base = va & !(frames::FRAME_SIZE - 1);
    let end = va + len;
    with_current_aspace(|aspace| {
        let mut a = base;
        while a < end {
            if let Some(pa) = aspace.unmap(a) {
                frames::free(pa);
            }
            a += frames::FRAME_SIZE;
        }
    });
}

/// Commit every whole page in `[va, va+len)` in the current cell with `perm`:
/// pages already mapped are reprotected in place (their frame and contents
/// kept); pages not yet mapped get a fresh zeroed frame. This is the
/// demand-commit path for `mprotect` making a reserved region accessible
/// (docs/LINUX-COMPAT.md L4) - glibc reserves large `PROT_NONE` regions
/// (per-thread malloc arenas, thread-stack guards) and commits sub-ranges as it
/// grows, so eager backing on `mmap` would exhaust the frame pool.
pub fn commit_range(va: usize, len: usize, perm: MapPerm) {
    if len == 0 {
        return;
    }
    let base = va & !(frames::FRAME_SIZE - 1);
    let end = va + len;
    with_current_aspace(|aspace| {
        let mut a = base;
        while a < end {
            // Unmap returns the existing frame (if any) so a reprotect keeps
            // the page's contents; otherwise allocate a fresh zeroed frame.
            let pa = aspace.unmap(a).unwrap_or_else(frames::alloc);
            aspace.map_user_frame(a, pa, perm);
            a += frames::FRAME_SIZE;
        }
    });
}

/// Change the permission of every whole page in `[va, va+len)` in the current
/// cell, keeping the frames.
pub fn protect_range(va: usize, len: usize, perm: MapPerm) {
    if len == 0 {
        return;
    }
    let base = va & !(frames::FRAME_SIZE - 1);
    let end = va + len;
    with_current_aspace(|aspace| {
        let mut a = base;
        while a < end {
            aspace.protect(a, perm);
            a += frames::FRAME_SIZE;
        }
    });
}

/// Back `len` bytes of fresh zeroed RW pages into the current cell and return
/// the base VA (0 on empty request). A bump allocator over the anon region -
/// the primitive the libc's malloc grows its heap with (docs/USERLAND.md M2).
fn mmap_anon(cur: usize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let cell = cells()[cur];
    // SAFETY: single CPU; the running cell's address space is uniquely owned
    // for the duration of the run, and adding not-present -> present leaf
    // entries to the active root is safe (re-activated below to publish them).
    let aspace = unsafe { &mut *(cell.aspace as *mut AddressSpace) };
    let pages = len.div_ceil(frames::FRAME_SIZE);
    let base = unsafe { *core::ptr::addr_of!(MMAP_NEXT) };
    for i in 0..pages {
        let pa = frames::alloc();
        aspace.map_user_frame(base + i * frames::FRAME_SIZE, pa, MapPerm::UserRw);
    }
    unsafe {
        *core::ptr::addr_of_mut!(MMAP_NEXT) = base + pages * frames::FRAME_SIZE;
    }
    // Publish the new mappings on the active root (fence / tlbi / cr3 reload).
    aspace.activate();
    base
}

/// Register a runnable cell in slot `idx`. The pointers must outlive the
/// run (the test kernels own the backing storage as statics).
///
/// # Safety
/// All pointers must be valid and uniquely owned for the duration of the
/// run; `frame` must have been produced by `arch::trapframe_new`.
pub unsafe fn install(
    idx: usize,
    aspace: *const AddressSpace,
    caps: *mut CapTable,
    objects: *const ObjectTable,
    qp: *const QueuePair,
    frame: *mut TrapFrame,
) {
    cells()[idx] = RunCell {
        aspace,
        caps,
        objects,
        qp,
        frame,
        outcome: None,
        present: true,
        personality: Personality::Native,
    };
}

/// Tag an installed cell with a syscall personality (call after `install`,
/// before `run`). Native is the default; a Linux cell's traps are handled by
/// `crate::linux` instead of the native dispatch.
pub fn set_personality(idx: usize, p: Personality) {
    assert!(cells()[idx].present, "set_personality on empty slot {idx}");
    cells()[idx].personality = p;
}

/// Run starting from cell `idx` until some cell exits or faults. Returns
/// which cell ended the run and how. Cross-cell switches keep running
/// inside the trampoline; only an exit or fault unwinds back here.
pub fn run(idx: usize) -> (usize, Outcome) {
    let cell = cells()[idx];
    assert!(cell.present, "run of empty cell slot {idx}");
    unsafe {
        *core::ptr::addr_of_mut!(CURRENT) = idx;
        (*cell.aspace).activate();
        arch::enter_user_first(cell.frame);
    }
    // enter_user_first returns via return_to_kernel after an exit/fault.
    // Restore the kernel address space so setup code can again reach all
    // of RAM (a cell root only maps that cell's user pages).
    arch::paging_activate_kernel();
    let exited = unsafe { *core::ptr::addr_of!(EXITED) };
    (
        exited,
        cells()[exited].outcome.expect("no outcome recorded"),
    )
}

/// Record why the current cell's run ended and signal an unwind by
/// returning a null frame (the arch trampoline calls return_to_kernel).
fn finish(outcome: Outcome) -> *mut TrapFrame {
    let cur = unsafe { *core::ptr::addr_of!(CURRENT) };
    cells()[cur].outcome = Some(outcome);
    unsafe {
        *core::ptr::addr_of_mut!(EXITED) = cur;
    }
    core::ptr::null_mut()
}

/// The trap dispatcher, called from each arch's U-mode trampoline. Returns
/// the frame to resume (the same cell, or the peer on a switch), or a null
/// pointer to signal an unwind back to the kernel (exit or fault) - the
/// arch wrapper is responsible for calling return_to_kernel on null, so
/// that per-ISA state (e.g. x86 GS) is restored symmetrically first.
///
/// Dereferencing the raw `frame` (the state the trampoline just saved) is
/// the whole point of the dispatcher.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn on_user_trap(
    kind: TrapKind,
    cause: FaultCause,
    fault_addr: usize,
    frame: *mut TrapFrame,
) -> *mut TrapFrame {
    if kind == TrapKind::Fault {
        let cur = unsafe { *core::ptr::addr_of!(CURRENT) };
        // A Linux cell with an installed, unblocked handler for the fault's
        // signal gets synchronous delivery (SIGSEGV/SIGBUS/SIGILL/SIGFPE) by
        // trap-frame rewrite (docs/LINUX-COMPAT.md L5); otherwise it terminates
        // reporting 128+signo. A NATIVE cell fault is always terminal
        // (Outcome::Faulted) - signal delivery is behind the Linux branch only.
        if cells()[cur].personality == Personality::Linux {
            return match crate::linux::deliver_fault(cur, cause, fault_addr, frame) {
                crate::linux::FaultOutcome::Resume(f) => f,
                crate::linux::FaultOutcome::Terminate(code) => finish(Outcome::Exited(code)),
            };
        }
        return finish(Outcome::Faulted(fault_addr));
    }

    let (nr, args) = arch::decode_syscall(unsafe { &*frame });
    let arg = args[0];
    let cur = unsafe { *core::ptr::addr_of!(CURRENT) };

    // Linux cells never reach native dispatch: the personality tag decides
    // the syscall table before the number means anything (the ABIs collide).
    if cells()[cur].personality == Personality::Linux {
        return match crate::linux::handle(cur, nr, &args, frame) {
            crate::linux::Ctl::Ret(v) => {
                arch::set_syscall_ret(unsafe { &mut *frame }, v);
                frame
            }
            crate::linux::Ctl::Exit(code) => finish(Outcome::Exited(code)),
            // A thread switch (futex/yield/clone-exit): resume a different
            // context of the same cell. That context's frame already carries
            // its saved state and pending return value; FP/TLS were swapped by
            // the thread scheduler before this point (docs/LINUX-COMPAT.md L4).
            crate::linux::Ctl::Switch(next) => next,
        };
    }

    match nr {
        SYS_DOORBELL => {
            let cell = cells()[cur];
            // SAFETY: the pointers were validated at install time.
            unsafe {
                queue::kernel_process(&*cell.qp, &mut *cell.caps, &*cell.objects);
            }
            frame
        }
        SYS_CYCLES => {
            arch::set_syscall_ret(unsafe { &mut *frame }, arch::cycles());
            frame
        }
        SYS_SWITCH => {
            let peer = cur ^ 1;
            let peer_cell = cells()[peer];
            assert!(peer_cell.present, "SYS_SWITCH with no peer cell");
            unsafe {
                *core::ptr::addr_of_mut!(CURRENT) = peer;
                (*peer_cell.aspace).activate();
            }
            peer_cell.frame
        }
        SYS_EXIT | SYS_EXIT_GROUP => finish(Outcome::Exited(arg)),
        SYS_MMAP => {
            let base = mmap_anon(cur, args[0] as usize);
            arch::set_syscall_ret(unsafe { &mut *frame }, base as u64);
            frame
        }
        // Shell / resource / file syscalls are handled by the system-service
        // module; an unrecognised number faults the cell.
        other => match crate::svc::handle(other, &args) {
            Some(ret) => {
                arch::set_syscall_ret(unsafe { &mut *frame }, ret);
                frame
            }
            None => finish(Outcome::Faulted(0)),
        },
    }
}

/// Number of runnable cells the kernel is tracking (shell `ps`).
pub fn cell_count() -> usize {
    cells().iter().filter(|c| c.present).count()
}

/// Live capability count in the current cell's table (shell `caps`).
pub fn current_caps_live() -> usize {
    let cur = unsafe { *core::ptr::addr_of!(CURRENT) };
    let cell = cells()[cur];
    if cell.caps.is_null() {
        0
    } else {
        // SAFETY: caps was validated at install time.
        unsafe { (*cell.caps).live_count() }
    }
}
