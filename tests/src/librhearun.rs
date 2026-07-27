//! In-QEMU test kernel for librheo Phase A (docs/LIBRHEO.md): load the
//! `librheo-demo` ELF into a cell **with a real mapped queue pair + a minted
//! QueuePair capability**, run it, and assert it exits with its distinctive
//! code. The demo does heap + per-cell RNG + a capability-typed handle and -
//! the headline - an **async queue round trip**: strands submit `OP_ECHO` over
//! the real ring, park on the completion token, and are woken by librheo's
//! userland reactor draining the completions. The demo only reaches its OK
//! exit code if every echo returned correct, so the exit code is the proof.
//!
//! Wiring mirrors `stdrun`: fds 1/2 route to the console (the demo's marker),
//! and `SYS_RANDOM` is served by the kernel DRBG (`svc::init`). The new pieces
//! are `load::map_queue` (maps the ring at 16 GiB), a minted `QueuePair` cap,
//! and `user::set_queue_info` so `SYS_QUEUE_INFO` reports `(qp_va, cap_id)`.

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
    "/../target/x86_64-unknown-none/release/librheo-demo"
));
#[cfg(target_arch = "aarch64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/librheo-demo"
));
#[cfg(target_arch = "riscv64")]
static DEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/librheo-demo"
));

/// The demo returns this on full success (see librheo-demo.rs).
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
    kernel::boot::init();
    println!("librhearun: start on {}", arch::NAME);

    // Seed the kernel DRBG (SYS_RANDOM, used once by librheo to seed its own).
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
    let entry = load::load_elf(DEMO, &mut aspace).expect("load librheo-demo ELF");
    let stack_top = load::map_stack(&mut aspace);
    // Map the cell's queue pair (ring region at 16 GiB) + mint its capability.
    let qp = load::map_queue(&mut aspace);
    println!(
        "librhearun: loaded librheo-demo ({} bytes), entry {entry:#x}, queue at {:#x}",
        DEMO.len(),
        load::USER_QUEUE_VA
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
                "librheo-demo exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "librhearun: librheo-demo ran (heap+rng+cap+async queue echo), exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("librheo-demo faulted at {addr:#x}"),
    }

    println!("librhearun: PASS");
    arch::exit(arch::ExitCode::Success)
}
