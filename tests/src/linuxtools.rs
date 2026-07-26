//! In-QEMU test kernel for Linux-personality milestone L3 (docs/LINUX-COMPAT.md
//! L3): the **unpatched upstream uutils/coreutils** multicall binary, built
//! from crates.io (pinned, static-glibc ET_EXEC) for the ISA's
//! `*-unknown-linux-gnu` target, runs as a `Personality::Linux` cell.
//!
//! Each invocation is one utility (`coreutils <util> [args]`, dispatched by the
//! crate's own multicall main). Files are served by the `posix` VFS over a
//! seeded ramfs (the shared `vfs_personality` handler); stdout is captured via
//! the Linux personality's stdout tap so the test asserts each utility's exact
//! output and exit code. This is the literal "the Linux Rust coreutils run on
//! this OS" deliverable.
//!
//! The fixture is built by `cargo xtask` (`build_coreutils_fixture`); no binary
//! lives in git. It is `include_bytes!`d below.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{CapTable, ObjectTable};
use kernel::linux::{self, stack as linux_stack};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc;
use kernel::user::{self, Outcome, Personality};
use kernel::{arch, load, println};
use posix::{RamFs, fs, mount, sys};

#[path = "vfs_personality.rs"]
mod vfs_personality;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 4 * 1024 * 1024] = [0; 4 * 1024 * 1024];

/// The static-glibc coreutils multicall binary, per ISA (built by
/// `xtask::build_coreutils_fixture` into the gitignored fixture build dir).
macro_rules! coreutils_bin {
    () => {{
        #[cfg(target_arch = "x86_64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/x86_64/cu/bin/coreutils"
            ))
        }
        #[cfg(target_arch = "aarch64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/aarch64/cu/bin/coreutils"
            ))
        }
        #[cfg(target_arch = "riscv64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/riscv64/cu/bin/coreutils"
            ))
        }
    }};
}

static COREUTILS: &[u8] = coreutils_bin!();

// Seeded ramfs contents. `multi.txt` is deliberately unsorted so `sort` has
// visible work and `head`/`wc` have a known shape.
const DATA: &[u8] = b"coreutils on rheo-os\n";
const MULTI: &[u8] = b"banana\napple\ncherry\n";

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

// -- stdout capture, wired to the Linux personality's stdout tap --
const CAP_MAX: usize = 16 * 1024;
static mut STDOUT_CAP: [u8; CAP_MAX] = [0; CAP_MAX];
static mut STDOUT_LEN: usize = 0;

fn tap(bytes: &[u8]) {
    // SAFETY: single-threaded; the tap is called only during a cell run.
    unsafe {
        for &b in bytes {
            if STDOUT_LEN < CAP_MAX {
                STDOUT_CAP[STDOUT_LEN] = b;
                STDOUT_LEN += 1;
            }
        }
    }
}

fn captured() -> &'static [u8] {
    unsafe { &STDOUT_CAP[..STDOUT_LEN] }
}

/// Load the coreutils image into a fresh Linux cell with `argv`, run it, return
/// the outcome. A fresh address space per call so the program starts clean.
fn run_util(argv: &[&[u8]]) -> Outcome {
    let mut aspace = AddressSpace::new(1);
    let img = load::load_elf_linux(COREUTILS, &mut aspace).expect("load coreutils ELF");
    let sp = linux_stack::setup_stack(&mut aspace, &img, argv, &[]);

    // SAFETY: single-threaded; the statics outlive the synchronous run.
    unsafe {
        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(img.entry, sp, 0, kernel_sp);
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qp = core::ptr::addr_of!(QP) as *const QueuePair;
        user::reset();
        user::install(0, &aspace, caps, objects, qp, addr_of_mut!(frame));
        user::set_personality(0, Personality::Linux);
        linux::install_cell(0, img.image_end);
        user::run(0).1
    }
}

/// Run one utility and assert its exit code and exact stdout.
fn check(argv: &[&[u8]], want_code: u64, want_out: &[u8]) {
    unsafe {
        STDOUT_LEN = 0;
    }
    let name = core::str::from_utf8(argv[1]).unwrap_or("?");
    linux::set_stdout_tap(Some(tap));
    let outcome = run_util(argv);
    linux::set_stdout_tap(None);
    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == want_code,
                "{name}: exit {code}, expected {want_code}"
            );
            let got = captured();
            assert!(
                got == want_out,
                "{name}: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_out),
            );
        }
        Outcome::Faulted(addr) => panic!("{name}: faulted at {addr:#x}"),
    }
    println!("linuxtools: {name} OK");
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("linuxtools: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 4 * 1024 * 1024);
    }

    // A ramfs at / seeded with two files and a directory, plus the VFS
    // personality handler (open/read/getdents/stat forwarded here).
    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    fs::write("/data.txt", DATA).expect("seed /data.txt");
    fs::write("/multi.txt", MULTI).expect("seed /multi.txt");
    sys::mkdir("/dir").expect("seed /dir");
    svc::set_file_ops(vfs_personality::ops());

    println!(
        "linuxtools: loaded upstream coreutils multicall ({} bytes)",
        COREUTILS.len()
    );

    // argv[0] = "coreutils" so the multicall main dispatches on argv[1]:
    // uutils 0.0.29 takes the binary name straight from `std::env::args`
    // (argv[0]), which the kernel supplies on the initial stack. Exact stdout +
    // exit asserted for each utility.
    check(&[b"coreutils", b"true"], 0, b"");
    check(&[b"coreutils", b"false"], 1, b"");
    check(
        &[b"coreutils", b"echo", b"hello", b"world"],
        0,
        b"hello world\n",
    );
    check(&[b"coreutils", b"cat", b"/data.txt"], 0, DATA);
    check(&[b"coreutils", b"seq", b"1", b"3"], 0, b"1\n2\n3\n");
    check(
        &[b"coreutils", b"head", b"-n", b"2", b"/multi.txt"],
        0,
        b"banana\napple\n",
    );
    check(
        &[b"coreutils", b"wc", b"-l", b"/multi.txt"],
        0,
        b"3 /multi.txt\n",
    );
    check(&[b"coreutils", b"basename", b"/a/b/c.txt"], 0, b"c.txt\n");
    check(&[b"coreutils", b"dirname", b"/a/b/c.txt"], 0, b"/a/b\n");
    // `sort` re-enabled at L4 (docs/LINUX-COMPAT.md): uu_sort parallelizes with
    // rayon, which spawns worker threads (clone/futex) - proving a real
    // threaded upstream coreutil works on the multi-context cell.
    check(
        &[b"coreutils", b"sort", b"/multi.txt"],
        0,
        b"apple\nbanana\ncherry\n",
    );
    check(
        &[b"coreutils", b"ls", b"/"],
        0,
        b"data.txt\ndir\nmulti.txt\n",
    );
    check(&[b"coreutils", b"pwd"], 0, b"/\n");

    println!("linuxtools: PASS");
    arch::exit(arch::ExitCode::Success)
}
