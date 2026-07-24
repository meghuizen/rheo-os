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
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::user::{self, Outcome, Personality};
use kernel::{arch, load, println};

#[cfg(target_arch = "x86_64")]
static LINUXHELLO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/x86_64-unknown-none/release/linuxhello"
));
#[cfg(target_arch = "aarch64")]
static LINUXHELLO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/aarch64-unknown-none-softfloat/release/linuxhello"
));
#[cfg(target_arch = "riscv64")]
static LINUXHELLO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/riscv64gc-unknown-none-elf/release/linuxhello"
));

/// 42 only if the Linux write returned the full count (see linuxhello).
const EXPECTED_EXIT: u64 = 42;

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("linuxrun: start on {}", arch::NAME);

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(LINUXHELLO, &mut aspace).expect("load linuxhello ELF");
    let stack_top = load::map_stack(&mut aspace);
    println!(
        "linuxrun: loaded linuxhello ({} bytes), entry {entry:#x}",
        LINUXHELLO.len()
    );

    // SAFETY: single-threaded init; the statics outlive the run.
    let outcome = unsafe {
        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(entry, stack_top, 0, kernel_sp);
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qp = core::ptr::addr_of!(QP) as *const QueuePair;
        user::reset();
        user::install(0, &aspace, caps, objects, qp, addr_of_mut!(frame));
        user::set_personality(0, Personality::Linux);
        user::run(0).1
    };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "linuxhello exited {code}, expected {EXPECTED_EXIT} (write count check failed?)"
            );
            println!("linuxrun: Linux-ABI program ran under Personality::Linux OK");
        }
        Outcome::Faulted(addr) => panic!("linuxhello faulted at {addr:#x}"),
    }

    println!("linuxrun: PASS");
    arch::exit(arch::ExitCode::Success)
}
