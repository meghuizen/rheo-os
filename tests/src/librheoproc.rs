//! In-QEMU test kernel for librheo Phase F (docs/LIBRHEO.md): the native
//! **process model** (spawn / wait), the **one-shot timer**, the librheo-native
//! **shell**, and the **embedded** minimal cell - on all three ISAs.
//!
//! Three scenarios run as loaded librheo cells over a ramfs `/bin`:
//!
//! 1. **direct spawn/wait/timer proof** (`librheo-orch`): an orchestrator cell,
//!    holding a cell-spawn capability, spawns `/bin/echo` and three `/bin/child`
//!    cells (argv fan-out), awaits each, reduces their exit codes to 12, and
//!    proves a `time::sleep` wakes on the timer. Exits `0x42`.
//! 2. **the shell** (`lrsh`): a shell built entirely on librheo, fed a scripted
//!    session; it runs the `echo` builtin, **spawns** `/bin/child` and prints its
//!    exit code, then `exit`s `0x42`. Exact transcript + exit asserted.
//! 3. **the embedded minimal cell** (`librheo-embed`, built
//!    `--no-default-features`): a spine-only cell doing a direct queue round-trip.
//!    Exits `0x42`; asserted substantially smaller (loadable size) than a
//!    full-featured librheo binary.
//!
//! A cell **without** a spawn capability (the embedded cell) cannot spawn - the
//! kernel refuses, no ambient authority.

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
use kernel::{arch, elf, input, load, println, time};
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

static ORCH: &[u8] = bin!("librheo-orch");
static ECHO: &[u8] = bin!("librheo-echo");
static CHILD: &[u8] = bin!("librheo-child");
static LRSH: &[u8] = bin!("lrsh");
static EMBED: &[u8] = bin!("librheo-embed");

const EXPECTED_EXIT: u64 = 0x42;

/// The shell is fed this scripted session (played as if typed): the `echo`
/// builtin, then a spawn of `/bin/child 8`, then `exit`.
static SCRIPT: &[u8] = b"echo hi there\nchild 8\nexit\n";

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: MaybeUninit<QueuePair> = MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

/// Sum of a librheo ELF's loadable segment sizes (the code+data footprint),
/// used to prove the embedded build is substantially smaller.
fn loadable_size(image: &[u8]) -> usize {
    let elf = elf::Elf::parse(image).expect("parse librheo ELF");
    let mut sum = 0usize;
    elf.for_each_load(|seg| {
        sum += seg.memsz;
        Some(())
    });
    sum
}

/// Load `image` as a fresh native librheo cell (its own address space, queue
/// pair, minted queue capability), optionally minting a **cell-spawn** capability
/// (`ObjectKind::Cell` + WRITE) and installing a scripted console-input source.
/// Runs it and returns its outcome. Spawned children share this cell's capability
/// bundle (installed by `nproc`).
fn run_cell(image: &[u8], spawn_cap: bool, script: Option<&'static [u8]>) -> Outcome {
    // Fresh capability tables per scenario (so a leftover spawn cap never leaks
    // to a cell that should not have one).
    // SAFETY: single-threaded; between runs.
    unsafe {
        *addr_of_mut!(OBJECTS) = ObjectTable::new();
        *addr_of_mut!(CAPS) = CapTable::new();
    }
    input::reset();
    if let Some(s) = script {
        input::install_script(s);
    }

    let mut aspace = AddressSpace::new(1);
    let entry = load::load_elf(image, &mut aspace).expect("load librheo ELF");
    let stack_top = load::map_stack(&mut aspace);
    let qp = load::map_queue(&mut aspace);

    // SAFETY: single-threaded init; the statics + `aspace`/`frame` outlive the
    // synchronous run (children live in kernel-owned `nproc` storage).
    unsafe {
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qobj = objects.create(ObjectKind::QueuePair).unwrap();
        let qcap = caps
            .mint(objects, qobj, READ | WRITE, BUDGET_UNLIMITED)
            .unwrap()
            .raw_low32();
        if spawn_cap {
            let cobj = objects.create(ObjectKind::Cell).unwrap();
            caps.mint(objects, cobj, WRITE, BUDGET_UNLIMITED).unwrap();
        }

        (*addr_of_mut!(QP)).write(qp);
        let qp_ptr = (*addr_of_mut!(QP)).as_ptr();
        let kernel_sp = addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(entry, stack_top, 0, kernel_sp);
        user::reset();
        user::install(0, &aspace, caps, objects, qp_ptr, addr_of_mut!(frame));
        user::set_queue_info(0, load::USER_QUEUE_VA as u64, qcap);
        user::run(0).1
    }
}

fn expect_exit(label: &str, outcome: Outcome) {
    match outcome {
        Outcome::Exited(code) => assert!(
            code == EXPECTED_EXIT,
            "{label} exited {code:#x}, expected {EXPECTED_EXIT:#x}"
        ),
        Outcome::Faulted(addr) => panic!("{label} faulted at {addr:#x}"),
    }
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("librheoproc: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 4 * 1024 * 1024);
    }

    // A ramfs at / holding the coreutils the shell/orchestrator spawn, and the
    // VFS personality so the loader's `open`/`read`/`lseek` reach them.
    svc::init();
    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    sys::mkdir("/bin").expect("mkdir /bin");
    fs::write("/bin/echo", ECHO).expect("seed /bin/echo");
    fs::write("/bin/child", CHILD).expect("seed /bin/child");
    svc::set_file_ops(vfs_personality::ops());

    println!(
        "librheoproc: orch={} echo={} child={} lrsh={} embed={} bytes",
        ORCH.len(),
        ECHO.len(),
        CHILD.len(),
        LRSH.len(),
        EMBED.len()
    );

    // Bring up the timer interrupt where this ISA supports it (a no-op that
    // leaves the busy-wait fallback elsewhere), so the orchestrator's
    // `time::sleep` parks at WFI and wakes on the hardware timer IRQ.
    arch::enable_timer_irq();

    // --- scenario 1: direct spawn / wait / timer proof ---
    vfs_personality::clear_stdout();
    expect_exit("librheo-orch", run_cell(ORCH, true, None));
    println!(
        "librheoproc: orch OK (spawn+wait+map/reduce+timer; timer mode: {})",
        if time::timer_interrupt_driven() {
            "interrupt-driven (WFI idle)"
        } else {
            "cooperative busy-wait"
        }
    );

    // Idle-park proof: where the timer interrupt is wired, the orchestrator's
    // `time::sleep` must have genuinely idled at WFI (0% CPU, not a spin). In
    // the busy-wait build this is skipped (documented, honest).
    if time::timer_interrupt_driven() {
        assert!(
            time::timer_did_idle(),
            "interrupt-driven timer but the kernel never idled at WFI"
        );
        println!("librheoproc: timer idle-park proven (kernel idled at WFI, woke on timer IRQ)");
    }

    // --- scenario 2: the librheo-native shell ---
    vfs_personality::clear_stdout();
    let outcome = run_cell(LRSH, true, Some(SCRIPT));
    let got = vfs_personality::captured_stdout();
    let want: &[u8] = b"hi there\nchild 8\n[exit 8]\n";
    expect_exit("lrsh", outcome);
    assert!(
        got == want,
        "lrsh transcript mismatch\n  got:      {:?}\n  expected: {:?}",
        core::str::from_utf8(got),
        core::str::from_utf8(want),
    );
    println!("librheoproc: lrsh OK (builtin echo + spawn/wait a coreutil)");

    // --- scenario 3: the embedded minimal cell (no spawn capability) ---
    vfs_personality::clear_stdout();
    expect_exit("librheo-embed", run_cell(EMBED, false, None));
    let embed_sz = loadable_size(EMBED);
    let orch_sz = loadable_size(ORCH);
    assert!(
        embed_sz * 3 < orch_sz,
        "embedded cell not substantially smaller: embed={embed_sz} orch={orch_sz}"
    );
    println!(
        "librheoproc: embed OK (spine-only round-trip; loadable {embed_sz} vs {orch_sz} bytes)"
    );

    println!("librheoproc: PASS");
    arch::exit(arch::ExitCode::Success)
}
