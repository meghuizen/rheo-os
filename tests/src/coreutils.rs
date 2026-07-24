//! In-QEMU test kernel for the coreutils cell (docs/USERLAND.md M5): load the
//! `rheo-coreutils` multicall program (real `std`, cross-compiled for the
//! rheo-os target) into a cell with a real `argv`, run one utility per
//! invocation over a ramfs, and assert its exit code and exact stdout. This
//! exercises the whole M5 path end to end: argv delivered through the crt0
//! (`std::env::args`), files read through `std::fs` over the VFS, output
//! through `std::io::stdout` - standard tools running on the OS.

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
static mut HEAP_MEM: [u8; 4 * 1024 * 1024] = [0; 4 * 1024 * 1024];

#[cfg(target_arch = "x86_64")]
static COREUTILS: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../targets/std-rheo/coreutils/target/rheo_os-x86_64/release/coreutils"
));
#[cfg(target_arch = "aarch64")]
static COREUTILS: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../targets/std-rheo/coreutils/target/rheo_os-aarch64/release/coreutils"
));
#[cfg(target_arch = "riscv64")]
static COREUTILS: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../targets/std-rheo/coreutils/target/rheo_os-riscv64/release/coreutils"
));

const DATA: &[u8] = b"coreutils on rheo-os\n";
const MULTI: &[u8] = b"one\ntwo\nthree\n";

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

/// Load the coreutils image into a fresh cell with `argv`, run it, and return
/// (exit-or-fault outcome). Each call is a fresh address space, so the
/// program's globals start clean.
fn run_util(argv: &[&[u8]]) -> Outcome {
    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(COREUTILS, &mut aspace).expect("load coreutils ELF");
    let sp = load::setup_stack(&mut aspace, argv, &[]);

    // SAFETY: single-threaded; the statics outlive the synchronous run, which
    // completes before `aspace`/`frame` drop.
    unsafe {
        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(entry, sp, 0, kernel_sp);
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qp = core::ptr::addr_of!(QP) as *const QueuePair;
        user::reset();
        user::install(0, &aspace, caps, objects, qp, addr_of_mut!(frame));
        user::run(0).1
    }
}

/// Run one utility and assert its exit code and exact stdout.
fn check(argv: &[&[u8]], want_code: u64, want_out: &[u8]) {
    vfs_personality::clear_stdout();
    let name = core::str::from_utf8(argv[0]).unwrap_or("?");
    match run_util(argv) {
        Outcome::Exited(code) => {
            assert!(
                code == want_code,
                "{name}: exit {code}, expected {want_code}"
            );
            let got = vfs_personality::captured_stdout();
            assert!(
                got == want_out,
                "{name}: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_out),
            );
        }
        Outcome::Faulted(addr) => panic!("{name}: faulted at {addr:#x}"),
    }
    println!("coreutils: {name} OK");
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("coreutils: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 4 * 1024 * 1024);
    }

    // A ramfs at / with a couple of seeded files, and the VFS personality.
    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    fs::write("/data.txt", DATA).expect("seed /data.txt");
    fs::write("/multi.txt", MULTI).expect("seed /multi.txt");
    svc::set_file_ops(vfs_personality::ops());

    println!(
        "coreutils: loaded multicall image ({} bytes)",
        COREUTILS.len()
    );

    // true / false: exit codes only.
    check(&[b"true"], 0, b"");
    check(&[b"false"], 1, b"");

    // echo, with and without the trailing newline.
    check(&[b"echo", b"hello", b"world"], 0, b"hello world\n");
    check(&[b"echo", b"-n", b"abc"], 0, b"abc");

    // cat a real file through std::fs over the VFS.
    check(&[b"cat", b"/data.txt"], 0, DATA);

    // wc -l counts the three lines.
    check(&[b"wc", b"-l", b"/multi.txt"], 0, b"3 /multi.txt\n");

    // head -n 2 takes the first two lines.
    check(&[b"head", b"-n", b"2", b"/multi.txt"], 0, b"one\ntwo\n");

    // seq generates an inclusive range.
    check(&[b"seq", b"1", b"3"], 0, b"1\n2\n3\n");

    // basename / dirname operate on the path string.
    check(&[b"basename", b"/a/b/c.txt"], 0, b"c.txt\n");
    check(&[b"dirname", b"/a/b/c.txt"], 0, b"/a/b\n");

    // ls / lists the seeded entries, sorted.
    check(&[b"ls", b"/"], 0, b"data.txt\nmulti.txt\n");

    println!("coreutils: PASS");
    arch::exit(arch::ExitCode::Success)
}
