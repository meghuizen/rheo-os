//! In-QEMU test kernel: hardware-enforced isolation (BUILD-ORDER.md step
//! 5). Real U-mode cells behind real page tables. Every check here is
//! enforced by the MMU faulting - not by a table lookup returning an
//! error - which is exactly the caveat the previous milestone's
//! comparison/RESULTS.md flagged as still open.
//!
//! Probes (each in a fresh cell, then asserted against the fault the MMU
//! must raise):
//! 1. read own scratch page        -> allowed (control: proves mappings work)
//! 2. read kernel memory           -> fault (privilege isolation)
//! 3. read a peer cell's page       -> fault (cross-cell isolation)
//! 4. write to a code page          -> fault (W^X: executable is not writable)
//! 5. execute a data page           -> fault (NX: writable is not executable)

#![no_std]
#![no_main]

#[path = "harness.rs"]
mod harness;

use harness::{CellStore, KernelStack, build_cell};
use kernel::capability::{CapTable, ObjectTable};
use kernel::user::{self, Outcome};
use kernel::user_progs::{PROBE_EXEC, PROBE_READ, PROBE_WRITE, user_prober};
use kernel::{arch, println};

// Cell stores live in the `.user` window so they can be mapped U; the
// kernel trap stack stays in ordinary supervisor `.bss`.
#[unsafe(link_section = ".user.bss")]
static mut STORE_A: CellStore = CellStore::new();
#[unsafe(link_section = ".user.bss")]
static mut STORE_B: CellStore = CellStore::new();
static mut KSTACK: KernelStack = KernelStack::new();
static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();

/// Build a fresh prober cell and run it; return how it ended.
fn probe(mode: u64, target: usize) -> Outcome {
    // SAFETY: single-threaded kernel; each probe fully completes before
    // the next. STORE_A is reused across probes (fresh address space each
    // time), which is fine because runs never overlap.
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS);
        let store_ptr = core::ptr::addr_of_mut!(STORE_A);
        let kernel_sp = (*core::ptr::addr_of!(KSTACK)).top();

        let (aspace, _obj, mut frame) = build_cell(
            &mut *store_ptr,
            objects,
            caps,
            kernel_sp,
            1,
            user_prober,
            mode,
            target as u64,
        );
        let qp = (*store_ptr).qp.qp.as_ptr();

        user::reset();
        user::install(
            0,
            &aspace,
            caps,
            objects,
            qp,
            core::ptr::addr_of_mut!(frame),
        );
        let (_idx, outcome) = user::run(0);
        outcome
    }
}

fn assert_fault_at(outcome: Outcome, want: usize, what: &str) {
    match outcome {
        Outcome::Faulted(addr) => {
            assert_eq!(
                addr, want,
                "{what}: faulted at {addr:#x}, expected {want:#x}"
            );
            println!("isolation-hw: {what} faulted at {addr:#x} OK");
        }
        Outcome::Exited(code) => panic!("{what}: no fault, cell exited with {code}"),
    }
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("isolation-hw: start on {}", arch::NAME);

    // Addresses used as probe targets.
    let (text_start, _text_end) = kernel::mm::user_text_range();
    let (own_scratch, peer_scratch, kernel_addr, own_stack) = unsafe {
        (
            core::ptr::addr_of!(STORE_A.scratch) as usize,
            core::ptr::addr_of!(STORE_B.scratch) as usize,
            core::ptr::addr_of!(KSTACK) as usize,
            core::ptr::addr_of!(STORE_A.stack) as usize,
        )
    };

    // 1. Control: a cell can read its own mapped scratch page. The prober
    // reaches its exit (status 1 = "access was permitted") without faulting.
    match probe(PROBE_READ, own_scratch) {
        Outcome::Exited(_) => println!("isolation-hw: own-scratch read allowed OK"),
        other => panic!("own-scratch read should succeed, got {other:?}"),
    }

    // 2. A cell cannot read kernel memory (no U bit on kernel pages).
    assert_fault_at(probe(PROBE_READ, kernel_addr), kernel_addr, "kernel-read");

    // 3. A cell cannot read another cell's page (not mapped in this root).
    assert_fault_at(
        probe(PROBE_READ, peer_scratch),
        peer_scratch,
        "cross-cell-read",
    );

    // 4. W^X: the executable code page is not writable.
    assert_fault_at(probe(PROBE_WRITE, text_start), text_start, "code-write");

    // 5. NX: a writable data page is not executable.
    assert_fault_at(probe(PROBE_EXEC, own_stack), own_stack, "data-exec");

    println!("isolation-hw: PASS");
    arch::exit(arch::ExitCode::Success)
}
