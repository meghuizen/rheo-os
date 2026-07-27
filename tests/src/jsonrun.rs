//! In-QEMU test kernel for rheo-json running on the OS (docs/JSON.md): load
//! `jsondemo` (a libc-linked program that parses an embedded JSON document)
//! and check it parses the fields and exits with the expected value. No VFS is
//! needed - the program only writes to stdout - so the personality here just
//! routes fds 1/2 to the console; file ops report ENOSYS.

#![no_std]
#![no_main]

use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{CapTable, ObjectTable};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc::{self, FileOps};
use kernel::user::{self, Outcome};
use kernel::{arch, load, println};

#[cfg(target_arch = "x86_64")]
static JSONDEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/x86_64-unknown-none/release/jsondemo"
));
#[cfg(target_arch = "aarch64")]
static JSONDEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/jsondemo"
));
#[cfg(target_arch = "riscv64")]
static JSONDEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/jsondemo"
));

/// version (4) + feature count (3).
const EXPECTED_EXIT: u64 = 7;

// Console-only personality: stdout/stderr to the UART, everything else ENOSYS.
fn c_write(fd: u64, buf_va: u64, len: u64) -> i64 {
    if fd == 1 || fd == 2 {
        let buf = unsafe { core::slice::from_raw_parts(buf_va as *const u8, len as usize) };
        for &b in buf {
            arch::serial_write_byte(b);
        }
        len as i64
    } else {
        -9 // EBADF
    }
}
fn c_open(_p: u64, _l: u64, _f: u64) -> i64 {
    -38 // ENOSYS
}
fn c_close(_fd: u64) -> i64 {
    -38
}
fn c_read(_fd: u64, _b: u64, _l: u64) -> i64 {
    -38
}
fn c_lseek(_fd: u64, _o: i64, _w: u64) -> i64 {
    -38
}
fn c_stat(_p: u64, _l: u64, _s: u64) -> i64 {
    -38
}
fn c_fstat(_fd: u64, _s: u64) -> i64 {
    -38
}
fn c_getdents(_p: u64, _l: u64, _b: u64, _bl: u64) -> i64 {
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
    println!("jsonrun: start on {}", arch::NAME);

    svc::set_file_ops(FileOps {
        open: c_open,
        close: c_close,
        read: c_read,
        write: c_write,
        lseek: c_lseek,
        stat: c_stat,
        fstat: c_fstat,
        getdents: c_getdents,
    });

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(JSONDEMO, &mut aspace).expect("load jsondemo ELF");
    let stack_top = load::map_stack(&mut aspace);
    println!("jsonrun: loaded jsondemo, entry {entry:#x}");

    // SAFETY: single-threaded init; the statics outlive the run.
    let outcome = unsafe {
        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(entry, stack_top, 0, kernel_sp);
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qp = core::ptr::addr_of!(QP) as *const QueuePair;
        user::reset();
        user::install(0, &aspace, caps, objects, qp, addr_of_mut!(frame));
        user::run(0).1
    };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "jsondemo exited {code}, expected {EXPECTED_EXIT}"
            );
            println!("jsonrun: jsondemo parsed the document on-OS and exited {code} OK");
        }
        Outcome::Faulted(addr) => panic!("jsondemo faulted at {addr:#x}"),
    }

    println!("jsonrun: PASS");
    arch::exit(arch::ExitCode::Success)
}
