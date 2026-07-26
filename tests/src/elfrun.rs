//! In-QEMU test kernel for the userland ELF loader (docs/USERLAND.md M1):
//! load a *separately-compiled* Rust program (`userland/`, linked at 4 GiB)
//! into a fresh cell's address space, run it in U-mode, and check it wrote a
//! line and exited with the expected code. This is the first program on
//! rheo-os that is not baked into the kernel image.

#![no_std]
#![no_main]

use core::mem::MaybeUninit;

use kernel::capability::{CapTable, ObjectTable};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::user::{self, Outcome};
use kernel::{arch, load, println};

// The built userland ELF, embedded per ISA. xtask builds `userland` (release)
// before the test kernels, so these paths exist at compile time.
#[cfg(target_arch = "x86_64")]
static HELLO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/x86_64-unknown-none/release/hello"
));
#[cfg(target_arch = "aarch64")]
static HELLO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/hello"
));
#[cfg(target_arch = "riscv64")]
static HELLO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/hello"
));

/// Must match `userland/src/main.rs`.
const EXPECTED_EXIT: u64 = 42;

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);

static mut KSTACK: KStack = KStack([0; 64 * 1024]);
static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
// The program never rings the doorbell, so its queue pair is never touched;
// `install` only needs a valid pointer to store.
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("elfrun: start on {}", arch::NAME);

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(HELLO, &mut aspace).expect("load hello ELF");
    let stack_top = load::map_stack(&mut aspace);
    println!(
        "elfrun: loaded hello ({} bytes), entry {:#x}, stack_top {:#x}",
        HELLO.len(),
        entry,
        stack_top
    );

    // SAFETY: single-threaded kernel init; the statics outlive the run.
    unsafe {
        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(entry, stack_top, 0, kernel_sp);

        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS);
        let qp = core::ptr::addr_of!(QP) as *const QueuePair;

        user::reset();
        user::install(
            0,
            &aspace,
            caps,
            objects,
            qp,
            core::ptr::addr_of_mut!(frame),
        );
        let (_idx, outcome) = user::run(0);

        match outcome {
            Outcome::Exited(code) => {
                assert!(
                    code == EXPECTED_EXIT,
                    "hello exited {code}, expected {EXPECTED_EXIT}"
                );
                println!("elfrun: hello exited {code} OK");
            }
            Outcome::Faulted(addr) => panic!("hello faulted at {addr:#x}"),
        }
    }

    println!("elfrun: PASS");
    arch::exit(arch::ExitCode::Success)
}
