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

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("libcrun: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    fs::write("/greeting.txt", GREETING).expect("seed /greeting.txt");
    svc::set_file_ops(vfs_personality::ops());

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
