//! In-QEMU test kernel for the Linux personality (docs/LINUX-COMPAT.md 5).
//! Grows with each milestone:
//!
//! - L0: `linuxhello`, a bare program speaking the raw Linux ABI (write+exit).
//! - L1: `linuxauxv`, which walks the auxv the kernel built and validates it.
//! - L2: **unpatched static-glibc binaries** - a Rust `std` hello
//!   (String/Vec/println!) and a C hello (gcc), each built for the ISA's
//!   `*-unknown-linux-gnu` target and run as a `Personality::Linux` cell. Their
//!   exact stdout is captured (via a stdout tap) and their exit codes asserted,
//!   so the run proves the core syscall set, the fd table, memory (brk/mmap),
//!   and glibc startup end to end.
//!
//! All fixtures are built from source by `cargo xtask` (no binaries in git) and
//! `include_bytes!`d here.

#![no_std]
#![no_main]

use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{CapTable, ObjectTable};
use kernel::linux::{self, stack as linux_stack};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::user::{self, Outcome, Personality};
use kernel::{arch, load, println};

/// `include_bytes!` a bare-target release artifact per ISA (L0/L1 fixtures,
/// built by the `userland` crate). The arms differ only in the target-triple
/// directory.
macro_rules! bare_fixture {
    ($name:literal) => {{
        #[cfg(target_arch = "x86_64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../target/x86_64-unknown-none/release/",
                $name
            ))
        }
        #[cfg(target_arch = "aarch64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../target/aarch64-unknown-none-softfloat/release/",
                $name
            ))
        }
        #[cfg(target_arch = "riscv64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../target/riscv64gc-unknown-none-elf/release/",
                $name
            ))
        }
    }};
}

/// `include_bytes!` the static-glibc Rust fixture (L2), built by
/// `xtask::build_linux_fixtures` to the ISA's `*-unknown-linux-gnu` target.
macro_rules! glibc_rust {
    () => {{
        #[cfg(target_arch = "x86_64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/rusthello/target/x86_64-unknown-linux-gnu/release/rusthello"
            ))
        }
        #[cfg(target_arch = "aarch64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/rusthello/target/aarch64-unknown-linux-gnu/release/rusthello"
            ))
        }
        #[cfg(target_arch = "riscv64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/rusthello/target/riscv64gc-unknown-linux-gnu/release/rusthello"
            ))
        }
    }};
}

/// `include_bytes!` the static-glibc C fixture (L2).
macro_rules! glibc_c {
    () => {{
        #[cfg(target_arch = "x86_64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/x86_64/chello"
            ))
        }
        #[cfg(target_arch = "aarch64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/aarch64/chello"
            ))
        }
        #[cfg(target_arch = "riscv64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/riscv64/chello"
            ))
        }
    }};
}

static LINUXHELLO: &[u8] = bare_fixture!("linuxhello");
static LINUXAUXV: &[u8] = bare_fixture!("linuxauxv");
static RUSTHELLO: &[u8] = glibc_rust!();
static CHELLO: &[u8] = glibc_c!();

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

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

/// Load and run one Linux-personality cell, returning its outcome. Installs
/// the per-cell Linux state (fd table, brk, mmap cursor) after load.
fn run_linux(name: &str, image: &[u8], argv: &[&[u8]]) -> Outcome {
    let mut aspace = AddressSpace::new(1);
    let img = load::load_elf_linux(image, &mut aspace).expect("load Linux ELF");
    let sp = linux_stack::setup_stack(&mut aspace, &img, argv, &[]);
    println!(
        "linuxrun: loaded {name} ({} bytes), entry {:#x}, bias {:#x}, image_end {:#x}",
        image.len(),
        img.entry,
        img.bias,
        img.image_end
    );

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
        linux::install_cell(0, img.image_end);
        user::run(0).1
    }
}

/// Run a fixture and assert it exits with `expect_exit`.
fn expect_exit(name: &str, image: &[u8], argv: &[&[u8]], expect_exit: u64) {
    match run_linux(name, image, argv) {
        Outcome::Exited(code) => assert!(
            code == expect_exit,
            "{name} exited {code}, expected {expect_exit}"
        ),
        Outcome::Faulted(addr) => panic!("{name} faulted at {addr:#x}"),
    }
    println!("linuxrun: {name} OK (exit {expect_exit})");
}

/// Run a fixture and assert both its exit code and its exact stdout.
fn expect_stdout(name: &str, image: &[u8], argv: &[&[u8]], code: u64, out: &[u8]) {
    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run_linux(name, image, argv);
    linux::set_stdout_tap(None);
    match outcome {
        Outcome::Exited(c) => assert!(c == code, "{name} exited {c}, expected {code}"),
        Outcome::Faulted(addr) => panic!("{name} faulted at {addr:#x}"),
    }
    let got = captured();
    assert!(
        got == out,
        "{name} stdout mismatch: got {} bytes, expected {}",
        got.len(),
        out.len()
    );
    println!("linuxrun: {name} OK (exit {code}, stdout matched)");
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("linuxrun: start on {}", arch::NAME);

    // L0/L1: bare programs speaking the raw Linux ABI; both exit 42.
    expect_exit("linuxhello", LINUXHELLO, &[b"linuxhello"], 42);
    expect_exit("linuxauxv", LINUXAUXV, &[b"linuxauxv"], 42);

    // L2: unpatched static-glibc binaries. Exact stdout + exit asserted.
    expect_stdout(
        "rusthello",
        RUSTHELLO,
        &[b"rusthello"],
        7,
        b"rust glibc: squares sum 30\n",
    );
    expect_stdout("chello", CHELLO, &[b"chello"], 9, b"hello from glibc C\n");

    println!("linuxrun: PASS");
    arch::exit(arch::ExitCode::Success)
}
