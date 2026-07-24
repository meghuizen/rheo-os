//! In-QEMU test kernel for Linux-personality milestone L4 (docs/LINUX-COMPAT.md
//! L4): an **unpatched multi-threaded Rust `std` binary** runs as a
//! `Personality::Linux` cell with multiple execution contexts scheduled
//! cooperatively at syscall boundaries.
//!
//! The fixture (`tests/linux-fixtures/rustthreads`) spawns four `std::thread`s
//! that share an `Arc<AtomicUsize>`, a `Mutex<u64>`, and an `mpsc` channel,
//! then joins them - exercising clone + futex + per-thread TLS + the CHILD_-
//! CLEARTID join handshake end to end. Its output is scheduling-independent, so
//! the exact stdout and exit code are asserted on all three ISAs.
//!
//! Built static-glibc ET_EXEC by `cargo xtask` (`build_linux_fixtures`); no
//! binary lives in git. `include_bytes!`d below.

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

/// `include_bytes!` the static-glibc multi-threaded Rust fixture (L4), built by
/// `xtask::build_linux_fixtures` for the ISA's `*-unknown-linux-gnu` target.
macro_rules! rustthreads_bin {
    () => {{
        #[cfg(target_arch = "x86_64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/rustthreads/target/x86_64-unknown-linux-gnu/release/rustthreads"
            ))
        }
        #[cfg(target_arch = "aarch64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/rustthreads/target/aarch64-unknown-linux-gnu/release/rustthreads"
            ))
        }
        #[cfg(target_arch = "riscv64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/rustthreads/target/riscv64gc-unknown-linux-gnu/release/rustthreads"
            ))
        }
    }};
}

static RUSTTHREADS: &[u8] = rustthreads_bin!();

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

fn run(image: &[u8], argv: &[&[u8]]) -> Outcome {
    let mut aspace = AddressSpace::new(1);
    let img = load::load_elf_linux(image, &mut aspace).expect("load Linux ELF");
    let sp = linux_stack::setup_stack(&mut aspace, &img, argv, &[]);
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

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("linuxthreads: start on {}", arch::NAME);
    println!(
        "linuxthreads: loaded multi-threaded Rust std ({} bytes)",
        RUSTTHREADS.len()
    );

    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run(RUSTTHREADS, &[b"rustthreads"]);
    linux::set_stdout_tap(None);

    let want_out = b"threads 4 total 1550 channel 1550\n";
    match outcome {
        Outcome::Exited(code) => {
            assert!(code == 4, "rustthreads exited {code}, expected 4");
            let got = captured();
            assert!(
                got == want_out,
                "rustthreads stdout mismatch:\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_out),
            );
        }
        Outcome::Faulted(addr) => panic!("rustthreads faulted at {addr:#x}"),
    }

    println!("linuxthreads: PASS");
    arch::exit(arch::ExitCode::Success)
}
