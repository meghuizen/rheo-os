//! In-QEMU test kernel for librheo Phase J (docs/LIBRHEO.md): a **cross-cell
//! stdout pipeline between a spawned cell and its parent**. An orchestrator cell
//! (the consumer) `spawn_piped`s `/bin/pipesrc` (the producer); the child
//! **inherits the parent's Phase E channel** at spawn (the kernel maps the same
//! frames into it, opposite role); the child streams a known byte sequence to the
//! parent over the async `Sender`/`Receiver` - not through the kernel - and the
//! parent verifies it received exactly the child's output, reaps it, and exits
//! `0x42`. Asserted on all three ISAs.
//!
//! The orchestrator is cell 0; its single spawned child lands in slot 1, a valid
//! `SYS_SWITCH` `cur^1` pair, so the async channel's cooperative cell-boundary
//! hand-off works. A ramfs at `/` holds `/bin/pipesrc`; the orchestrator holds a
//! **cell-spawn** capability and a **channel** wired by this kernel.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use core::mem::MaybeUninit;
use core::ptr::{addr_of, addr_of_mut};

use kernel::capability::{BUDGET_UNLIMITED, CapTable, ObjectKind, ObjectTable, READ, WRITE};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc;
use kernel::user::{self, Outcome};
use kernel::{arch, load, println};
use posix::{RamFs, fs, mount, sys};

#[path = "vfs_personality.rs"]
mod vfs_personality;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 4 * 1024 * 1024] = [0; 4 * 1024 * 1024];

macro_rules! bin {
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

static PIPE: &[u8] = bin!("librheo-pipe");
static PIPESRC: &[u8] = bin!("librheo-pipesrc");

const EXPECTED_EXIT: u64 = 0x42;

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("librheopipe: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 4 * 1024 * 1024);
    }

    // A ramfs at / holding the producer the orchestrator spawns, and the VFS
    // personality so the loader's open/read/lseek reach it.
    svc::init();
    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    sys::mkdir("/bin").expect("mkdir /bin");
    fs::write("/bin/pipesrc", PIPESRC).expect("seed /bin/pipesrc");
    svc::set_file_ops(vfs_personality::ops());

    println!(
        "librheopipe: pipe={} pipesrc={} bytes",
        PIPE.len(),
        PIPESRC.len()
    );

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(PIPE, &mut aspace).expect("load librheo-pipe");
    let stack_top = load::map_stack(&mut aspace);
    let qp = load::map_queue(&mut aspace);

    // Wire a cross-cell channel into the orchestrator (role 1 = server/consumer);
    // the spawned child inherits it (opposite role) at spawn time.
    let channel = load::alloc_channel();
    load::map_channel_into(&mut aspace, &channel);
    println!(
        "librheopipe: orchestrator channel at {:#x} ({} bytes)",
        load::USER_CHANNEL_VA,
        QueuePair::REGION_SIZE
    );

    // SAFETY: single-threaded init; the statics + aspace/frame outlive the run
    // (the spawned child lives in kernel-owned nproc storage).
    let outcome = unsafe {
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qobj = objects.create(ObjectKind::QueuePair).unwrap();
        let qcap = caps
            .mint(objects, qobj, READ | WRITE, BUDGET_UNLIMITED)
            .unwrap()
            .raw_low32();
        // The orchestrator's channel capability.
        let cobj = objects.create(ObjectKind::QueuePair).unwrap();
        let chan_cap = caps
            .mint(objects, cobj, READ | WRITE, BUDGET_UNLIMITED)
            .unwrap()
            .raw_low32();
        // The cell-spawn capability (ObjectKind::Cell + WRITE - no ambient auth).
        let cell_obj = objects.create(ObjectKind::Cell).unwrap();
        caps.mint(objects, cell_obj, WRITE, BUDGET_UNLIMITED)
            .unwrap();

        (*addr_of_mut!(QP)).write(qp);
        let qp_ptr = (*addr_of_mut!(QP)).as_ptr();
        let kernel_sp = addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(entry, stack_top, 0, kernel_sp);
        user::reset();
        user::install(0, &aspace, caps, objects, qp_ptr, addr_of_mut!(frame));
        user::set_queue_info(0, load::USER_QUEUE_VA as u64, qcap);
        user::set_channel_info(0, load::USER_CHANNEL_VA as u64, chan_cap, 1);
        user::run(0).1
    };

    match outcome {
        Outcome::Exited(code) => {
            assert!(
                code == EXPECTED_EXIT,
                "librheo-pipe exited {code:#x}, expected {EXPECTED_EXIT:#x}"
            );
            println!(
                "librheopipe: cross-cell stdout pipeline ran (spawned child streamed \
                 its output over the inherited channel), exit {code:#x} OK"
            );
        }
        Outcome::Faulted(addr) => panic!("librheo-pipe faulted at {addr:#x}"),
    }

    println!("librheopipe: PASS");
    arch::exit(arch::ExitCode::Success)
}
