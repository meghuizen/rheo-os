//! Headless shell smoke test: drive lsh with a fixed script instead of
//! live serial input, so CI exercises the whole PTY -> shell -> resource-
//! object path deterministically. The transcript is captured to the
//! serial log; the pass/fail gate is that the shell cell ran every command
//! and exited cleanly (a fault in the shell would surface as Faulted).

#![no_std]
#![no_main]

#[path = "harness.rs"]
mod harness;

use core::ptr::{addr_of, addr_of_mut};
use harness::{KernelStack, ShellStore, build_shell_cell};
use kernel::capability::{CapTable, ObjectTable};
use kernel::queue::QueuePair;
use kernel::user::{self, Outcome};
use kernel::user_progs::user_shell;
use kernel::{arch, println, pty};

#[unsafe(link_section = ".user.bss")]
static mut STORE: ShellStore = ShellStore::new();
static mut KSTACK: KernelStack = KernelStack::new();
static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();

// A scripted session covering every builtin. The second `reserve` pushes
// utilization over 100% and must be refused; `graph 6` must print 42.
static SCRIPT: &[u8] = b"help\n\
echo hello lattice\n\
uptime\n\
rand\n\
meminfo\n\
ps\n\
caps\n\
event 8\n\
graph 6\n\
reserve 3 10\n\
reserve 8 10\n\
lease\n\
cpuinfo\n\
lspci\n\
numa\n\
bogus-command\n\
exit\n";

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("shell-smoke: start on {}", arch::NAME);
    pty::install_script(SCRIPT);

    let outcome = unsafe {
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
        let (_idx, outcome) = user::run(0);
        outcome
    };

    match outcome {
        Outcome::Exited(0) => {
            println!("\r\nshell-smoke: PASS");
            arch::exit(arch::ExitCode::Success)
        }
        other => {
            println!("\r\nshell-smoke: FAIL ({other:?})");
            arch::exit(arch::ExitCode::Failure)
        }
    }
}
