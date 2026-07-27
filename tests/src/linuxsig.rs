//! In-QEMU test kernel for Linux-personality milestone L5 (docs/LINUX-COMPAT.md
//! L5): **synthesized POSIX signal delivery**. Three unpatched static-glibc C
//! fixtures run as `Personality::Linux` cells and exercise the three delivery
//! paths, with exact stdout + exit asserted on all three ISAs:
//!
//! - `sig_raise`: `sigaction(SIGUSR1)` + `raise(SIGUSR1)` - async delivery by
//!   trap-frame rewrite, handler runs, `rt_sigreturn` resumes to exit 0.
//! - `sig_segv`: `sigaction(SIGSEGV)` + a deliberate null-pointer write - the
//!   synchronous fault is delivered to the handler (frame rewrite) instead of
//!   killing the cell; the handler prints and `_exit(0)`.
//! - `sig_dfl`: `raise(SIGABRT)` with no handler - the default disposition
//!   terminates the cell reporting 128+signo = 134 (SIG_DFL semantics).
//!
//! Fixtures are built from source by `cargo xtask` (`build_linux_fixtures`); no
//! binary lives in git. They are `include_bytes!`d below.

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

/// `include_bytes!` a static-glibc C signal fixture built by
/// `xtask::build_linux_fixtures` into the gitignored per-arch build dir.
macro_rules! sig_fixture {
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

static SIG_RAISE: &[u8] = sig_fixture!("sig_raise");
static SIG_SEGV: &[u8] = sig_fixture!("sig_segv");
static SIG_DFL: &[u8] = sig_fixture!("sig_dfl");

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

/// Run a fixture and assert its exit code and exact stdout.
fn check(name: &str, image: &[u8], want_code: u64, want_out: &[u8]) {
    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run(image, &[name.as_bytes()]);
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
        Outcome::Faulted(addr) => panic!("{name}: faulted at {addr:#x} (signal not delivered)"),
    }
    println!("linuxsig: {name} OK (exit {want_code})");
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("linuxsig: start on {}", arch::NAME);
    println!(
        "linuxsig: fixtures raise={} segv={} dfl={} bytes",
        SIG_RAISE.len(),
        SIG_SEGV.len(),
        SIG_DFL.len()
    );

    // Async delivery + rt_sigreturn resume: handler sets got=SIGUSR1(10).
    check("sig_raise", SIG_RAISE, 0, b"handled 10\n");
    // Synchronous fault -> handler (not a killed cell); handler _exit(0).
    check("sig_segv", SIG_SEGV, 0, b"caught segv\n");
    // Default disposition: raise(SIGABRT), no handler -> terminate 128+6 = 134.
    check("sig_dfl", SIG_DFL, 134, b"");

    println!("linuxsig: PASS");
    arch::exit(arch::ExitCode::Success)
}
