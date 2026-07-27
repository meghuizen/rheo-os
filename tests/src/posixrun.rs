//! In-QEMU test kernel for the POSIX syscall surface (docs/USERLAND.md M2):
//! load a native program (`userland` iodemo) that opens a file on a mounted
//! filesystem, reads it via the fd-based `read`, writes the bytes to stdout,
//! and exits with the byte count. The kernel forwards the file syscalls to a
//! personality handler backed by the `posix` VFS; `mmap`/`exit_group` are
//! kernel-native. Proves the multi-argument ABI + memory + file path.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{CapTable, ObjectTable};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc;
use kernel::user::{self, Outcome};
use kernel::{arch, load, println};
use posix::{RamFs, fs, mount};

#[path = "vfs_personality.rs"]
mod vfs_personality;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[cfg(target_arch = "x86_64")]
static IODEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/x86_64-unknown-none/release/iodemo"
));
#[cfg(target_arch = "aarch64")]
static IODEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/iodemo"
));
#[cfg(target_arch = "riscv64")]
static IODEMO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/iodemo"
));

/// Seeded into the VFS; iodemo reads it back and exits with its length.
const CONTENT: &[u8] = b"hello from the rheo-os VFS!\n";

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("posixrun: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    // A ramfs at / with one seeded file, and the personality wired up.
    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    fs::write("/hello.txt", CONTENT).expect("seed /hello.txt");
    svc::set_file_ops(vfs_personality::ops());

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(IODEMO, &mut aspace).expect("load iodemo ELF");
    let stack_top = load::map_stack(&mut aspace);
    println!("posixrun: loaded iodemo, entry {entry:#x}");

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
                code == CONTENT.len() as u64,
                "iodemo exited {code}, expected {} (file length)",
                CONTENT.len()
            );
            println!("posixrun: iodemo read {code} bytes via the VFS and exited OK");
        }
        Outcome::Faulted(addr) => panic!("iodemo faulted at {addr:#x}"),
    }

    println!("posixrun: PASS");
    arch::exit(arch::ExitCode::Success)
}
