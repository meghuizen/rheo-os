//! The interactive lsh boot kernel: bring the system up and hand the
//! serial console to a shell cell running in user mode. Run it with
//! `cargo xtask run --bin lsh --arch <isa>` and type at the prompt
//! (Ctrl-D or `exit` to leave). The shell is a real U-mode cell; every
//! builtin that reports resource state goes through a syscall to a genuine
//! kernel object (docs/SHELL.md).

#![no_std]
#![no_main]

#[path = "harness.rs"]
mod harness;

use core::ptr::{addr_of, addr_of_mut};
use harness::{KernelStack, ShellStore, build_shell_cell};
use kernel::arch;
use kernel::capability::{CapTable, ObjectTable};
use kernel::queue::QueuePair;
use kernel::user;
use kernel::user_progs::user_shell;

#[unsafe(link_section = ".user.bss")]
static mut STORE: ShellStore = ShellStore::new();
static mut KSTACK: KernelStack = KernelStack::new();
static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();

    // SAFETY: single CPU, one shell cell; statics are used only here.
    unsafe {
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let store = addr_of_mut!(STORE);
        let ksp = (*addr_of!(KSTACK)).top();

        let (aspace, _io, mut frame) =
            build_shell_cell(&mut *store, objects, caps, ksp, 1, user_shell);

        user::reset();
        user::install(
            0,
            &aspace,
            caps,
            objects,
            core::ptr::null::<QueuePair>(),
            addr_of_mut!(frame),
        );
        user::run(0);
    }

    arch::exit(arch::ExitCode::Success)
}
