//! In-QEMU test kernel for Linux-personality milestone L7 (docs/LINUX-COMPAT.md
//! L7): **dynamic linking** - running an UNMODIFIED, dynamically-linked glibc
//! binary.
//!
//! The fixture (`dhello`) is a stock ET_DYN/PIE C hello built with the ISA's
//! gcc and no `-static`/`-no-pie`. Its `PT_INTERP` names `ld-linux-*.so`; the
//! loader (`load::load_elf_linux`) loads the interpreter as a second ET_DYN at
//! `LINUX_INTERP_BASE`, sets `AT_BASE`/`AT_PHDR`/`AT_ENTRY`, and starts
//! execution in ld.so. ld.so then `mmap`s + relocates the program and
//! `libc.so.6` at runtime over the L7 fd-backed `mmap`. The real toolchain
//! `ld.so` + `libc.so.6` are seeded into a ramfs `/lib` (copied from the
//! toolchain at build time by `xtask::build_dyn_fixture`; never committed), so
//! the interpreter resolves them exactly as on Linux.
//!
//! Any ISA whose runtime `.so` could not be located at build time gets a 1-byte
//! placeholder `ld.so`; the test detects that and **skips-with-reason** (the
//! static L2-L6 coverage stays green).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
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
static mut HEAP_MEM: [u8; 8 * 1024 * 1024] = [0; 8 * 1024 * 1024];

/// The L7 fixtures, per ISA (built by `xtask::build_dyn_fixture`).
macro_rules! fixture {
    ($name:literal) => {{
        #[cfg(target_arch = "x86_64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/x86_64/",
                $name
            ))
        }
        #[cfg(target_arch = "aarch64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/aarch64/",
                $name
            ))
        }
        #[cfg(target_arch = "riscv64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/riscv64/",
                $name
            ))
        }
    }};
}

static DHELLO: &[u8] = fixture!("dhello");
static LD_SO: &[u8] = fixture!("ld.so");
static LIBC: &[u8] = fixture!("libc.so.6");

/// The dynamic-linker path named in each ISA's `PT_INTERP` (verified with
/// `readelf -p .interp`). ld.so's `libc.so.6` is found via `LD_LIBRARY_PATH`.
#[cfg(target_arch = "x86_64")]
const INTERP_PATH: &str = "/lib64/ld-linux-x86-64.so.2";
#[cfg(target_arch = "aarch64")]
const INTERP_PATH: &str = "/lib/ld-linux-aarch64.so.1";
#[cfg(target_arch = "riscv64")]
const INTERP_PATH: &str = "/lib/ld-linux-riscv64-lp64d.so.1";

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: core::mem::MaybeUninit<QueuePair> = core::mem::MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

// -- stdout capture, wired to the Linux personality's stdout tap --
const CAP_MAX: usize = 8 * 1024;
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

/// Run `image` (a dynamically-linked binary) with `argv`/`envp` under a fresh
/// Linux cell, capturing stdout; returns (outcome, captured bytes).
fn run(image: &[u8], argv: &[&[u8]], envp: &[&[u8]]) -> Outcome {
    let mut aspace = AddressSpace::new(1);
    let img = load::load_elf_linux(image, &mut aspace).expect("load dynamic Linux ELF");
    let sp = linux_stack::setup_stack(&mut aspace, &img, argv, envp);
    // SAFETY: single-threaded init; the statics outlive the synchronous run.
    unsafe {
        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(img.entry, sp, 0, kernel_sp);
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qp = core::ptr::addr_of!(QP) as *const QueuePair;
        user::reset();
        user::install(0, &aspace, caps, objects, qp, addr_of_mut!(frame));
        user::set_personality(0, Personality::Linux);
        linux::install_cell(0, &img);
        user::run(0).1
    }
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("linuxdyn: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 8 * 1024 * 1024);
    }

    // Skip-with-reason if the runtime ld.so could not be located at build time
    // (a 1-byte placeholder) - the static L2-L6 coverage stays the floor.
    if LD_SO.len() < 4096 {
        println!(
            "linuxdyn: SKIP on {} - toolchain ld.so/libc not available at build \
             time (static Linux coverage unaffected)",
            arch::NAME
        );
        println!("linuxdyn: PASS (skipped)");
        arch::exit(arch::ExitCode::Success);
    }

    // A ramfs at / holding the dynamic linker at its PT_INTERP path and
    // libc.so.6 under /lib, plus the VFS personality handler.
    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    sys::mkdir("/lib").expect("mkdir /lib");
    // x86-64's interp path is /lib64/...; create that dir too.
    if INTERP_PATH.starts_with("/lib64/") {
        sys::mkdir("/lib64").expect("mkdir /lib64");
    }
    fs::write(INTERP_PATH, LD_SO).expect("seed ld.so");
    fs::write("/lib/libc.so.6", LIBC).expect("seed libc.so.6");
    svc::set_file_ops(vfs_personality::ops());

    println!(
        "linuxdyn: dhello={} ld.so={} libc={} bytes; interp={}",
        DHELLO.len(),
        LD_SO.len(),
        LIBC.len(),
        INTERP_PATH,
    );

    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    // LD_LIBRARY_PATH=/lib so ld.so resolves DT_NEEDED libc.so.6 from the ramfs.
    let outcome = run(
        DHELLO,
        &[b"dhello"],
        &[b"LD_LIBRARY_PATH=/lib", b"PATH=/bin"],
    );
    linux::set_stdout_tap(None);

    let want_out = b"hello from dynamic glibc\n";
    match outcome {
        Outcome::Exited(code) => {
            let got = captured();
            assert!(
                got == want_out,
                "dhello: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_out),
            );
            assert!(code == 12, "dhello: exit {code}, expected 12");
        }
        Outcome::Faulted(addr) => panic!("dhello: faulted at {addr:#x}"),
    }
    println!("linuxdyn: dhello OK (PT_INTERP + ld.so + fd-backed mmap)");

    println!("linuxdyn: PASS");
    arch::exit(arch::ExitCode::Success)
}
