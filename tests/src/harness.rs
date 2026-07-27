//! Shared support for the user-mode test kernels: static `.user` storage
//! for a small fixed set of cells, and a builder that wires an address
//! space, capability table, queue pair, and trap frame into a runnable
//! cell. Kept in the test crate (not the kernel) because the exact set of
//! user pages is a per-test concern.
//!
//! Every backing store lives in the `.user` window so both the kernel
//! (through its supervisor identity map) and the owning cell (through its
//! U mappings) can reach it at the same address.

#![allow(static_mut_refs)]
// Shared across several test bins via #[path]; each uses only a subset.
#![allow(dead_code)]

use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::abi::{Params, ShellIo};
use kernel::arch::{self, MapPerm, TrapFrame};
use kernel::capability::{
    BUDGET_UNLIMITED, CapTable, ObjectId, ObjectKind, ObjectTable, READ, WRITE,
};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::user::Outcome;
use kernel::{linux, load, user};

/// Per-cell user page size budgets (each a whole number of 4 KiB pages).
const STACK_BYTES: usize = 32 * 1024;

/// Backing store for a shell cell: a stack plus a one-page ShellIo block.
#[repr(C, align(4096))]
pub struct ShellStore {
    pub stack: [u8; STACK_BYTES],
    pub io: ShellIo,
    _pad: [u8; 4096 - core::mem::size_of::<ShellIo>() % 4096],
}

impl ShellStore {
    pub const fn new() -> ShellStore {
        ShellStore {
            stack: [0; STACK_BYTES],
            io: ShellIo::ZERO,
            _pad: [0; 4096 - core::mem::size_of::<ShellIo>() % 4096],
        }
    }
}

/// Build a shell cell: map the shared code/rodata and the cell's stack and
/// ShellIo page, mint a few capabilities (so `caps` reports real state),
/// and produce the trap frame entering `entry` with the ShellIo VA as its
/// argument. Returns the address space, the ShellIo VA, and the frame.
///
/// # Safety
/// `store` must be a unique `.user` allocation that outlives the run.
pub unsafe fn build_shell_cell(
    store: &mut ShellStore,
    objects: &mut ObjectTable,
    caps: &mut CapTable,
    kernel_sp: usize,
    asid: u16,
    entry: extern "C" fn(usize) -> !,
) -> (AddressSpace, usize, TrapFrame) {
    let mut aspace = AddressSpace::new(asid);

    let (text_start, text_end) = kernel::mm::user_text_range();
    aspace.map_user_range(text_start, text_end - text_start, MapPerm::UserRx);
    let (ro_start, ro_end) = kernel::mm::user_rodata_range();
    if ro_end > ro_start {
        aspace.map_user_range(ro_start, ro_end - ro_start, MapPerm::UserRo);
    }

    let stack_addr = core::ptr::addr_of!(store.stack) as usize;
    aspace.map_user_range(stack_addr, STACK_BYTES, MapPerm::UserRw);
    let io_addr = core::ptr::addr_of!(store.io) as usize;
    aspace.map_user_range(io_addr, core::mem::size_of::<ShellIo>(), MapPerm::UserRw);

    // A handful of capabilities so the shell holds something real: its own
    // queue-pair object plus a couple of memory grants.
    for kind in [
        ObjectKind::QueuePair,
        ObjectKind::MemoryGrant,
        ObjectKind::MemoryGrant,
    ] {
        let obj = objects.create(kind).unwrap();
        caps.mint(objects, obj, READ | WRITE, BUDGET_UNLIMITED)
            .unwrap();
    }

    store.io = ShellIo::ZERO;
    // Seed the cell's per-cell DRBG from the root (docs/TIME-IDENTITY.md 4):
    // the kernel seeds at cell creation; the cell then draws bytes as a
    // library call over this state, no syscall.
    kernel::rng::derive_cell_drbg().fill_bytes(&mut store.io.rng_key);

    let stack_top = stack_addr + STACK_BYTES;
    let frame = arch::trapframe_new(entry as usize, stack_top, io_addr, kernel_sp);
    (aspace, io_addr, frame)
}

/// One cell's user-visible backing store, all in `.user.bss`. The queue pair
/// is now a single on-wire region (header + SQ + CQ) the `QueuePair` overlay
/// binds to (docs/LIBRHEO.md), replacing the old separate SQ/CQ arrays.
#[repr(C, align(4096))]
pub struct CellStore {
    pub stack: [u8; STACK_BYTES],
    pub params: Params,
    _pad_params: [u8; 4096 - core::mem::size_of::<Params>()],
    pub region: QueueRegion,
    pub qp: QueuePairCell,
    /// A page the cell can be told to poke (isolation prober target).
    pub scratch: [u8; 4096],
}

/// The shared queue-pair ring region, page-aligned so it maps cleanly.
#[repr(C, align(4096))]
pub struct QueueRegion(pub [u8; QueuePair::REGION_SIZE]);

/// QueuePair overlay plus a page of slack so it occupies its own page(s).
#[repr(C, align(4096))]
pub struct QueuePairCell {
    pub qp: core::mem::MaybeUninit<QueuePair>,
    _pad: [u8; 4096 - core::mem::size_of::<QueuePair>()],
}

impl CellStore {
    pub const fn new() -> CellStore {
        CellStore {
            stack: [0; STACK_BYTES],
            params: Params::ZERO,
            _pad_params: [0; 4096 - core::mem::size_of::<Params>()],
            region: QueueRegion([0; QueuePair::REGION_SIZE]),
            qp: QueuePairCell {
                qp: core::mem::MaybeUninit::uninit(),
                _pad: [0; 4096 - core::mem::size_of::<QueuePair>()],
            },
            scratch: [0; 4096],
        }
    }
}

/// A shared kernel stack for the U-mode trap handler (supervisor memory,
/// so it lives in ordinary `.bss`, not `.user`).
#[repr(align(16))]
pub struct KernelStack([u8; 64 * 1024]);
impl KernelStack {
    pub const fn new() -> KernelStack {
        KernelStack([0; 64 * 1024])
    }
    pub fn top(&self) -> usize {
        core::ptr::addr_of!(self.0) as usize + self.0.len()
    }
}

/// Build a runnable cell around `store`: map its user pages, mint a queue
/// capability, initialise the queue pair and params, and produce the trap
/// frame that enters `entry`.
///
/// # Safety
/// `store` must be a unique `.user` allocation that outlives the run.
#[allow(clippy::too_many_arguments)]
pub unsafe fn build_cell(
    store: &mut CellStore,
    objects: &mut ObjectTable,
    caps: &mut CapTable,
    kernel_sp: usize,
    asid: u16,
    entry: extern "C" fn(usize) -> !,
    workload: u64,
    iters: u64,
) -> (AddressSpace, ObjectId, TrapFrame) {
    let mut aspace = AddressSpace::new(asid);

    // Map the shared U-mode code (read+exec) and shared read-only
    // constants (read-only) into this cell.
    let (text_start, text_end) = kernel::mm::user_text_range();
    aspace.map_user_range(text_start, text_end - text_start, MapPerm::UserRx);
    let (ro_start, ro_end) = kernel::mm::user_rodata_range();
    if ro_end > ro_start {
        aspace.map_user_range(ro_start, ro_end - ro_start, MapPerm::UserRo);
    }

    // Map this cell's data pages (read+write, never executable).
    let stack_addr = core::ptr::addr_of!(store.stack) as usize;
    aspace.map_user_range(stack_addr, STACK_BYTES, MapPerm::UserRw);
    let params_addr = core::ptr::addr_of!(store.params) as usize;
    aspace.map_user(params_addr & !0xFFF, MapPerm::UserRw);
    let region_addr = core::ptr::addr_of!(store.region) as usize;
    aspace.map_user_range(region_addr, QueuePair::REGION_SIZE, MapPerm::UserRw);
    let qp_addr = core::ptr::addr_of!(store.qp) as usize;
    aspace.map_user(qp_addr & !0xFFF, MapPerm::UserRw);
    // Each cell owns its scratch page (read+write). Another cell's scratch
    // is deliberately *not* mapped here - reaching it must fault.
    let scratch_addr = core::ptr::addr_of!(store.scratch) as usize;
    aspace.map_user(scratch_addr & !0xFFF, MapPerm::UserRw);

    // Queue object + capability (READ|WRITE, unmetered).
    let object = objects
        .create(kernel::capability::ObjectKind::QueuePair)
        .unwrap();
    let cap = caps
        .mint(objects, object, READ | WRITE, BUDGET_UNLIMITED)
        .unwrap();

    // Initialise the on-wire region and overlay a queue pair on it. The
    // region is identity-mapped for a `.user` cell (kernel VA == user VA), so
    // `init` writes the header and binds the overlay at the same VA.
    let qp_ptr = store.qp.qp.as_mut_ptr();
    unsafe {
        qp_ptr.write(QueuePair::init(
            core::ptr::addr_of_mut!(store.region) as *mut u8
        ));
    }

    store.params = Params {
        workload,
        iters,
        qp_addr: qp_addr as u64,
        cap_id: cap.raw_low32() as u64,
        ..Params::ZERO
    };

    let stack_top = stack_addr + STACK_BYTES;
    let frame = arch::trapframe_new(entry as usize, stack_top, params_addr, kernel_sp);
    (aspace, object, frame)
}

// ===========================================================================
// Loading and running a cell from an ELF image
// ===========================================================================
//
// Fifteen kernels had written the same native launch by hand and seven more the
// same Linux launch - byte-identical once comments and the image's variable name
// are normalised away, thirty-five lines each
// (docs/ARCHITECTURE-DEBT.md 5). Both are here once.
//
// The kernel state a launch needs (`ObjectTable`, `CapTable`, the `QueuePair`
// overlay, the trap-handler stack) lives here too, because every one of those
// twenty-two kernels touched it *only* inside the block being replaced - checked,
// not assumed: each referenced these names exactly thirteen times and all
// thirteen were in the launch. A kernel that needs the tables afterwards (to
// assert on capability state, say) keeps its own and does not use these
// entry points.

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();
static mut KSTACK: KernelStack = KernelStack::new();

/// Load `image` into a fresh native cell with its own address space, stack and
/// mapped queue pair, run it to completion, and return its [`Outcome`].
///
/// The cell gets what librheo's `_start` expects to find: a queue-pair region
/// mapped at `load::USER_QUEUE_VA` with a minted `QueuePair` capability
/// (READ|WRITE, unmetered), reported through `SYS_QUEUE_INFO`. `what` names the
/// program in the load-failure panic.
///
/// # Safety
/// Single-threaded init only: this installs into cell slot 0 after
/// `user::reset()`, so it must not run while another cell is live. The statics it
/// uses outlive the synchronous run.
pub unsafe fn run_elf_cell(image: &[u8], what: &str) -> Outcome {
    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(image, &mut aspace).unwrap_or_else(|| panic!("load {what} ELF"));
    let stack_top = load::map_stack(&mut aspace);
    let qp = load::map_queue(&mut aspace);

    // SAFETY: the caller guarantees single-threaded init; every pointer below
    // refers to a static or to a local that outlives the `user::run` call.
    unsafe {
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let object = objects.create(ObjectKind::QueuePair).unwrap();
        let cap = caps
            .mint(objects, object, READ | WRITE, BUDGET_UNLIMITED)
            .unwrap();
        let cap_id = cap.raw_low32();

        (*addr_of_mut!(QP)).write(qp);
        let qp_ptr = (*addr_of_mut!(QP)).as_ptr();

        let kernel_sp = (*addr_of_mut!(KSTACK)).top();
        let mut frame = arch::trapframe_new(entry, stack_top, 0, kernel_sp);
        user::reset();
        user::install(0, &aspace, caps, objects, qp_ptr, addr_of_mut!(frame));
        user::set_queue_info(0, load::USER_QUEUE_VA as u64, cap_id);
        user::run(0).1
    }
}

/// Load `image` as a `Personality::Linux` cell with a System V initial stack
/// built from `argv`, run it, and return its [`Outcome`].
///
/// No queue pair is mapped: a Linux binary reaches the kernel through the Linux
/// syscall ABI, not the queue (docs/LINUX-COMPAT.md). The `QueuePair` pointer is
/// still installed because `user::install` takes one; nothing dereferences it on
/// this path.
///
/// # Safety
/// As [`run_elf_cell`].
pub unsafe fn run_linux_cell(image: &[u8], argv: &[&[u8]]) -> Outcome {
    // Reset **before** loading, not after. `user::reset` clears the personality's
    // mapped-file registry, and the loader registers the image in it - so resetting
    // afterwards would leave the cell's records naming a released entry and every page
    // of the image would fault in as zeros (docs/ENGINEERING.md 11).
    user::reset();
    let mut aspace = AddressSpace::new(1);
    let img = load::load_elf_linux(image, &mut aspace).expect("load Linux ELF");
    let sp = linux::stack::setup_stack(&mut aspace, &img, argv, &[]);

    // SAFETY: as above.
    unsafe {
        let kernel_sp = (*addr_of_mut!(KSTACK)).top();
        let mut frame = arch::trapframe_new(img.entry, sp, 0, kernel_sp);
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qp = core::ptr::addr_of!(QP) as *const QueuePair;
        user::install(0, &aspace, caps, objects, qp, addr_of_mut!(frame));
        user::set_personality(0, user::Personality::Linux);
        linux::install_cell(0, &img);
        user::run(0).1
    }
}
