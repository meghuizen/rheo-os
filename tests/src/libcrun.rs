//! In-QEMU test kernel for the libc (docs/USERLAND.md M3): load a program
//! written against rheo-libc (`libcdemo`) - it uses the Rust heap, C
//! `malloc`/`free`, `println!`, and fd-based file I/O - and check it echoes a
//! seeded file and exits with the file's length. Same VFS + personality wiring
//! as `posixrun`; the difference is the program links the libc instead of
//! issuing raw syscalls.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{CapTable, ObjectTable};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc::{self, FileOps};
use kernel::user::{self, Outcome};
use kernel::{arch, load, println};
use posix::sys::Whence;
use posix::{RamFs, fs, mount, sys};

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[cfg(target_arch = "x86_64")]
static LIBCDEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/x86_64-unknown-none/release/libcdemo"
));
#[cfg(target_arch = "aarch64")]
static LIBCDEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/libcdemo"
));
#[cfg(target_arch = "riscv64")]
static LIBCDEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/libcdemo"
));

const GREETING: &[u8] = b"hi from libc\n";

// Personality handler (see posixrun): user fds 0/1/2 = console, 3+ = the
// posix fd table (offset by 3). Runs in kernel context, so raw user VAs work.

fn p_open(path_va: u64, path_len: u64, flags: u64) -> i64 {
    let bytes = unsafe { core::slice::from_raw_parts(path_va as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(bytes) else {
        return -22;
    };
    match sys::open(path, flags as u32) {
        Ok(fd) => (fd + 3) as i64,
        Err(e) => -(sys::errno(e) as i64),
    }
}

fn p_close(fd: u64) -> i64 {
    if fd < 3 {
        return 0;
    }
    match sys::close((fd - 3) as usize) {
        Ok(()) => 0,
        Err(e) => -(sys::errno(e) as i64),
    }
}

fn p_read(fd: u64, buf_va: u64, len: u64) -> i64 {
    if fd < 3 {
        return 0;
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_va as *mut u8, len as usize) };
    match sys::read((fd - 3) as usize, buf) {
        Ok(n) => n as i64,
        Err(e) => -(sys::errno(e) as i64),
    }
}

fn p_write(fd: u64, buf_va: u64, len: u64) -> i64 {
    let buf = unsafe { core::slice::from_raw_parts(buf_va as *const u8, len as usize) };
    if fd == 1 || fd == 2 {
        for &b in buf {
            arch::serial_write_byte(b);
        }
        return len as i64;
    }
    if fd < 3 {
        return -9;
    }
    match sys::write((fd - 3) as usize, buf) {
        Ok(n) => n as i64,
        Err(e) => -(sys::errno(e) as i64),
    }
}

fn p_lseek(fd: u64, off: i64, whence: u64) -> i64 {
    if fd < 3 {
        return -9;
    }
    let w = match whence {
        0 => Whence::Set,
        1 => Whence::Cur,
        _ => Whence::End,
    };
    match sys::lseek((fd - 3) as usize, off, w) {
        Ok(o) => o as i64,
        Err(e) => -(sys::errno(e) as i64),
    }
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
    println!("libcrun: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    fs::write("/greeting.txt", GREETING).expect("seed /greeting.txt");
    svc::set_file_ops(FileOps {
        open: p_open,
        close: p_close,
        read: p_read,
        write: p_write,
        lseek: p_lseek,
    });

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(LIBCDEMO, &mut aspace).expect("load libcdemo ELF");
    let stack_top = load::map_stack(&mut aspace);
    println!("libcrun: loaded libcdemo, entry {entry:#x}");

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
                code == GREETING.len() as u64,
                "libcdemo exited {code}, expected {} (greeting length)",
                GREETING.len()
            );
            println!("libcrun: libcdemo ran (heap + malloc + println + file I/O), exit {code} OK");
        }
        Outcome::Faulted(addr) => panic!("libcdemo faulted at {addr:#x}"),
    }

    println!("libcrun: PASS");
    arch::exit(arch::ExitCode::Success)
}
