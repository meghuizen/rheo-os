//! In-QEMU test kernel for the Linux personality, milestone L0
//! (docs/LINUX-COMPAT.md 5): load `linuxhello` - a bare program speaking the
//! raw **Linux** syscall ABI - into a cell tagged `Personality::Linux`, run
//! it, and assert its exit code. The program exits 42 only if Linux
//! `write(1, ...)` returned the full byte count, so the exit code proves the
//! personality dispatch (tag branch before number decode), the Linux number
//! table for this ISA, and the write path in one assertion. The marker line
//! is also visible in the serial log.

#![no_std]
#![no_main]

use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

use kernel::capability::{CapTable, ObjectTable};
use kernel::linux::stack as linux_stack;
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::user::{self, Outcome, Personality};
use kernel::{arch, load, println};

/// `include_bytes!` a bare-target release artifact per ISA. The three arms
/// differ only in the target-triple directory the userland crate builds to.
macro_rules! fixture {
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

// linuxhello (L0): raw Linux write + exit_group.
static LINUXHELLO: &[u8] = fixture!("linuxhello");
// linuxauxv (L1): walks its own auxv, exits 42 iff AT_PAGESZ/AT_RANDOM are OK.
static LINUXAUXV: &[u8] = fixture!("linuxauxv");

/// Both fixtures exit 42 on success.
const EXPECTED_EXIT: u64 = 42;

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

/// Run a Linux-personality cell built with the Linux loader + auxv stack, and
/// assert it exits with `EXPECTED_EXIT`. `argv` is passed on the initial
/// stack. Each call uses a fresh address space so globals start clean.
fn run_linux(name: &str, image: &[u8], argv: &[&[u8]]) {
    let mut aspace = AddressSpace::new(1);
    let img = load::load_elf_linux(image, &mut aspace).expect("load Linux ELF");
    let sp = linux_stack::setup_stack(&mut aspace, &img, argv, &[]);
    println!(
        "linuxrun: loaded {name} ({} bytes), entry {:#x}, bias {:#x}",
        image.len(),
        img.entry,
        img.bias
    );

    // SAFETY: single-threaded init; the statics outlive the synchronous run.
    let outcome = unsafe {
        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(img.entry, sp, 0, kernel_sp);
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qp = core::ptr::addr_of!(QP) as *const QueuePair;
        user::reset();
        user::install(0, &aspace, caps, objects, qp, addr_of_mut!(frame));
        user::set_personality(0, Personality::Linux);
        user::run(0).1
    };

    match outcome {
        Outcome::Exited(code) => assert!(
            code == EXPECTED_EXIT,
            "{name} exited {code}, expected {EXPECTED_EXIT}"
        ),
        Outcome::Faulted(addr) => panic!("{name} faulted at {addr:#x}"),
    }
    println!("linuxrun: {name} OK");
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("linuxrun: start on {}", arch::NAME);

    // L0: raw Linux write + exit_group under Personality::Linux.
    run_linux("linuxhello", LINUXHELLO, &[b"linuxhello"]);
    // L1: the loaded program walks the auxv the kernel built and checks it.
    run_linux("linuxauxv", LINUXAUXV, &[b"linuxauxv"]);

    println!("linuxrun: PASS");
    arch::exit(arch::ExitCode::Success)
}
