//! In-QEMU test kernel for real `std` running on rheo-os (docs/USERLAND.md
//! M4): load a program built against the standard library (targets/std-rheo/
//! hello, cross-compiled for the rheo-os target with the patched rust-src),
//! run it in a cell, and check it prints via `std` `println!` and exits with
//! its `ExitCode`. The kernel routes fds 1/2 to the console (like jsonrun);
//! std's heap comes from `SYS_MMAP`, its stdio from the fd syscalls, and its
//! exit from `SYS_EXIT_GROUP` (crt0).

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
static STDHELLO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../targets/std-rheo/hello/target/rheo_os-x86_64/release/rheo-std-hello"
));
#[cfg(target_arch = "aarch64")]
static STDHELLO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../targets/std-rheo/hello/target/rheo_os-aarch64/release/rheo-std-hello"
));
#[cfg(target_arch = "riscv64")]
static STDHELLO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../targets/std-rheo/hello/target/rheo_os-riscv64/release/rheo-std-hello"
));

/// The std program returns `ExitCode::from(7)`.
const EXPECTED_EXIT: u64 = 7;

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
fn c_open(_p: u64, _l: u64, _f: u64) -> i64 {
    -38
}
fn c_close(_fd: u64) -> i64 {
    -38
}
/// Non-blocking stdin: drain whatever the serial RX FIFO holds right now and
/// return the count (0 if nothing is pending). Never waits for input, so a
/// std program's read/println logging can never block the cell.
fn c_read(fd: u64, buf_va: u64, len: u64) -> i64 {
    if fd != 0 {
        return -9;
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_va as *mut u8, len as usize) };
    let mut n = 0;
    while n < buf.len() {
        match arch::serial_read_byte() {
            Some(b) => {
                buf[n] = b;
                n += 1;
            }
            None => break,
        }
    }
    n as i64
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
    arch::init();
    println!("stdrun: start on {}", arch::NAME);

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
    let entry = load::load_elf(STDHELLO, &mut aspace).expect("load std hello ELF");
    let stack_top = load::map_stack(&mut aspace);
    println!(
        "stdrun: loaded std hello ({} bytes), entry {entry:#x}",
        STDHELLO.len()
    );

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
                "std hello exited {code}, expected {EXPECTED_EXIT}"
            );
            println!("stdrun: real std program ran on-OS (heap + println + ExitCode) OK");
        }
        Outcome::Faulted(addr) => panic!("std hello faulted at {addr:#x}"),
    }

    println!("stdrun: PASS");
    arch::exit(arch::ExitCode::Success)
}
