//! In-QEMU test kernel for librheo Phase C (docs/LIBRHEO.md): parallel &
//! accelerated compute with QoS. It loads the `librheo-compute` ELF into a cell
//! **with a real mapped queue pair + a minted QueuePair capability** (so graph
//! submission rides the async queue) and asserts it exits with its distinctive
//! code. The cell:
//!
//! - runs a **parallel aggregation across strands** (`compute::map_reduce`) over
//!   an in-memory dataset and asserts the exact reduced value;
//! - builds and submits a **real dependency graph** to the CPU engine over
//!   `OP_GRAPH_SUBMIT` and asserts its computed result;
//! - requests a **feasible reservation** (asserts committed ppm > 0) and an
//!   **infeasible** one (asserts a clean typed rejection), exercising the QoS
//!   admission path (the EDF math in `sched.rs`);
//! - reports the **engine kind + measured throughput** to stdout.
//!
//! It only reaches its `0x42` exit if every stage passed, so the exit code is
//! the proof. Wiring mirrors `librhearun` (queue pair + minted cap +
//! `set_queue_info`); `svc::init` attaches (measures) the CPU engine the graph
//! runs on, and fds 1/2 route to the console for the markers.

#![no_std]
#![no_main]

use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{BUDGET_UNLIMITED, CapTable, ObjectKind, ObjectTable, READ, WRITE};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc::{self, FileOps};
use kernel::user::{self, Outcome};
use kernel::{arch, load, println};

#[cfg(target_arch = "x86_64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/x86_64-unknown-none/release/librheo-compute"
));
#[cfg(target_arch = "aarch64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/librheo-compute"
));
#[cfg(target_arch = "riscv64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/librheo-compute"
));

/// The demo returns this on full success (see librheo-compute.rs).
const EXPECTED_EXIT: u64 = 0x42;

fn c_write(fd: u64, buf_va: u64, len: u64) -> i64 {
    if fd == 1 || fd == 2 {
        let buf = unsafe { core::slice::from_raw_parts(buf_va as *const u8, len as usize) };
        for &b in buf {
            arch::serial_write_byte(b);
        }
        len as i64
    } else {
        -9
    }
}
fn c_stub_open(_p: u64, _l: u64, _f: u64) -> i64 {
    -38
}
fn c_stub_close(_fd: u64) -> i64 {
    -38
}
fn c_stub_read(_fd: u64, _b: u64, _l: u64) -> i64 {
    -38
}
fn c_stub_lseek(_fd: u64, _o: i64, _w: u64) -> i64 {
    -38
}
fn c_stub_stat(_p: u64, _l: u64, _s: u64) -> i64 {
    -38
}
fn c_stub_fstat(_fd: u64, _s: u64) -> i64 {
    -38
}
fn c_stub_getdents(_p: u64, _l: u64, _b: u64, _bl: u64) -> i64 {
    -38
}

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("librheocompute: start on {}", arch::NAME);

    // Seed the kernel DRBG (SYS_RANDOM) and attach/measure the CPU engine that
    // graph submissions run on.
    svc::init();
    svc::set_file_ops(FileOps {
        open: c_stub_open,
        close: c_stub_close,
        read: c_stub_read,
        write: c_write,
        lseek: c_stub_lseek,
        stat: c_stub_stat,
        fstat: c_stub_fstat,
        getdents: c_stub_getdents,
    });

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(DEMO, &mut aspace).expect("load librheo-compute ELF");
    let stack_top = load::map_stack(&mut aspace);
    let qp = load::map_queue(&mut aspace);
    println!(
        "librheocompute: loaded librheo-compute ({} bytes), entry {entry:#x}",
        DEMO.len()
    );

    // SAFETY: single-threaded init; the statics outlive the run.
    let outcome = unsafe {
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let object = objects.create(ObjectKind::QueuePair).unwrap();
        let cap = caps
            .mint(objects, object, READ | WRITE, BUDGET_UNLIMITED)
            .unwrap();
        let cap_id = cap.raw_low32();

        (*addr_of_mut!(QP)).write(qp);
        let qp_ptr = (*addr_of_mut!(QP)).as_ptr();

        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(entry, stack_top, 0, kernel_sp);
        user::reset();
        user::install(0, &aspace, caps, objects, qp_ptr, addr_of_mut!(frame));
        user::set_queue_info(0, load::USER_QUEUE_VA as u64, cap_id);
        user::run(0).1
    };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "librheo-compute exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "librheocompute: parallel compute + graph submit + reservation, exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("librheo-compute faulted at {addr:#x}"),
    }

    println!("librheocompute: PASS");
    arch::exit(arch::ExitCode::Success)
}
