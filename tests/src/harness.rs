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

use kernel::abi::Params;
use kernel::arch::{self, MapPerm, TrapFrame};
use kernel::capability::{BUDGET_UNLIMITED, CapTable, ObjectId, ObjectTable, READ, WRITE};
use kernel::mm::AddressSpace;
use kernel::queue::{CqEntry, QueuePair, RING_DEPTH, SqEntry};

/// Per-cell user page size budgets (each a whole number of 4 KiB pages).
const STACK_BYTES: usize = 32 * 1024;

/// One cell's user-visible backing store, all in `.user.bss`.
#[repr(C, align(4096))]
pub struct CellStore {
    pub stack: [u8; STACK_BYTES],
    pub params: Params,
    _pad_params: [u8; 4096 - core::mem::size_of::<Params>()],
    pub sq: [SqEntry; RING_DEPTH],
    pub cq: [CqEntry; RING_DEPTH],
    pub qp: QueuePairCell,
    /// A page the cell can be told to poke (isolation prober target).
    pub scratch: [u8; 4096],
}

/// QueuePair plus a page of slack so it occupies its own page(s).
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
            sq: [SqEntry::ZERO; RING_DEPTH],
            cq: [CqEntry::ZERO; RING_DEPTH],
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

    // Map the shared U-mode code (read+exec).
    let (text_start, text_end) = kernel::mm::user_text_range();
    aspace.map_user_range(text_start, text_end - text_start, MapPerm::UserRx);

    // Map this cell's data pages (read+write, never executable).
    let stack_addr = core::ptr::addr_of!(store.stack) as usize;
    aspace.map_user_range(stack_addr, STACK_BYTES, MapPerm::UserRw);
    let params_addr = core::ptr::addr_of!(store.params) as usize;
    aspace.map_user(params_addr & !0xFFF, MapPerm::UserRw);
    let sq_addr = core::ptr::addr_of!(store.sq) as usize;
    aspace.map_user_range(sq_addr, core::mem::size_of_val(&store.sq), MapPerm::UserRw);
    let cq_addr = core::ptr::addr_of!(store.cq) as usize;
    aspace.map_user_range(cq_addr, core::mem::size_of_val(&store.cq), MapPerm::UserRw);
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

    // Initialise the shared queue pair in place.
    let qp_ptr = store.qp.qp.as_mut_ptr();
    unsafe {
        qp_ptr.write(QueuePair::new(
            core::ptr::addr_of_mut!(store.sq) as *mut SqEntry,
            core::ptr::addr_of_mut!(store.cq) as *mut CqEntry,
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
