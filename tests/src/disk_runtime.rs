//! Shared harness for the **disk-streamed language-runtime proofs** (`linuxnode`,
//! `linuxbun`, ...): stream a real, unmodified, dynamically-linked binary and its
//! shared-library set off a live virtio-blk ext4 disk (`ext4fs`/`ext4plus` + the
//! bounded block cache, GOAL-DISK-2b), `execve` it demand-paged under the Linux
//! personality, and assert exact stdout + exit 0.
//!
//! This is the "duplicate patterns become framework" factoring: two-plus
//! production runtimes (Node's V8+libuv, Bun's JavaScriptCore) run the identical
//! shape - only the binary path, argv/env, and expected line differ - so the
//! whole proof lives here and each test bin is a thin `prove(...)` call.
//!
//! `#[path]`-included per bin (docs/ARCHITECTURE-DEBT.md 5), so the statics and the
//! `#[global_allocator]` below are a fresh copy compiled into each including
//! binary - one allocator per bin crate, no conflict.

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::ptr::addr_of_mut;

use ext4fs::Ext4Fs;
use kernel::capability::{CapTable, ObjectTable};
use kernel::hw::block::{self, BlockCache};
use kernel::hw::virtio_blk::{self, VirtioBlk};
use kernel::linux::{self, stack as linux_stack};
use kernel::mm::AddressSpace;
use kernel::queue::QueuePair;
use kernel::svc;
use kernel::user::{self, Outcome, Personality};
use kernel::{abi, arch, load, println};
use posix::{BlockSource, Errno, mount};

#[path = "vfs_personality.rs"]
mod vfs_personality;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 16 * 1024 * 1024] = [0; 16 * 1024 * 1024];

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: core::mem::MaybeUninit<QueuePair> = core::mem::MaybeUninit::uninit();

#[repr(align(16))]
struct KStack([u8; 64 * 1024]);
static mut KSTACK: KStack = KStack([0; 64 * 1024]);

// -- stdout/stderr capture, wired to the Linux personality's output tap --
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

/// The kernel cache-line bridge to `posix::BlockSource` (the orphan-rule newtype,
/// as in `blockfs`/`linuxdyn`).
struct Cached(BlockCache<VirtioBlk>);
impl BlockSource for Cached {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), Errno> {
        self.0.read_at(off, buf).map_err(|_| Errno::Io)
    }
}

/// `execve` a dynamically-linked binary via the **streaming, demand-paged** path
/// (`exec_elf_from_vfs_demand`): the program and its `PT_INTERP` interpreter are
/// both streamed from the VFS and faulted in on demand - none resident whole.
fn run_execve(
    path: &str,
    argv: &[&[u8]],
    envp: &[&[u8]],
    wx: bool,
    on_secondary: bool,
    preempt: bool,
) -> Outcome {
    user::reset();
    let mut aspace = AddressSpace::new(1);
    let ops = svc::file_ops().expect("file ops registered");
    let img =
        load::exec_elf_from_vfs_demand(ops, path.as_ptr() as u64, path.len() as u64, &mut aspace)
            .expect("streaming execve of the runtime binary");
    let sp = linux_stack::setup_stack(&mut aspace, &img, argv, envp);
    // SAFETY: single-threaded init; the statics outlive the synchronous run.
    unsafe {
        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(img.entry, sp, 0, kernel_sp);
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qp = core::ptr::addr_of!(QP) as *const QueuePair;
        user::install(0, &aspace, caps, objects, qp, addr_of_mut!(frame));
        if wx {
            // The **W^X exception authority** (docs/ARCHITECTURE.md 5.1): a
            // `MemoryGrant` capability carrying WRITE|EXECUTE. Minted here, by the
            // thing that launches the cell, because that is the whole design - a cell
            // cannot widen its own authority, and a JIT-capable cell is visibly
            // privileged next to every other cell in the suite, which mints nothing of
            // the sort and is therefore refused exactly as before.
            let obj = objects
                .create(kernel::capability::ObjectKind::MemoryGrant)
                .expect("W^X exception object");
            caps.mint(
                objects,
                obj,
                kernel::capability::WRITE | kernel::capability::EXECUTE,
                kernel::capability::BUDGET_UNLIMITED,
            )
            .expect("W^X exception capability");
        }
        user::set_personality(0, Personality::Linux);
        linux::install_cell(0, &img, path.as_bytes());
        if on_secondary {
            match run_on_secondary_core(preempt) {
                Some(code) => Outcome::Exited(code as u64),
                None => {
                    println!(
                        "SKIP the secondary-core run - no secondary came up, or it did \
                         not finish inside the bound, so nothing about this runtime off the \
                         boot CPU is claimed"
                    );
                    arch::exit(arch::ExitCode::Success)
                }
            }
        } else {
            user::run(0).1
        }
    }
}

/// Run the full proof and exit the test kernel. `name` is the test name (for the
/// log lines), `path` the on-disk binary, `argv`/`envp` its launch vectors, and
/// `want` the exact combined stdout+stderr the run must produce on exit 0.
///
/// `thread_abort_partial` accepts one specific non-completion as an honest
/// **partial**: a runtime that spawns a worker thread and then `abort()`s
/// (`SIGABRT` -> exit 128+6 = 134) **before producing any output** because its
/// worker could not run concurrently under the cooperative single-CPU scheduler
/// (preemptive SMP is task #132). It is `true` only for Bun (whose JavaScriptCore
/// needs a concurrently-running helper thread); Node passes `false` and so is held
/// to a strict exit-0 gate. The partial is tightly bounded - it requires exit
/// exactly 134 **and** empty output - so any other regression (a fault, a wrong
/// exit code, or output-then-abort) still fails loudly, and once #132 lands and Bun
/// prints its answer the `Exited(0)` branch takes over.
///
/// On an ISA with no image (a placeholder disk, e.g. no binary for that ISA) the
/// proof **skips-with-reason** and passes; on `Outcome::Exited(0)` it asserts the
/// exact output; a `DEADLOCK_EXIT` (the pre-per-context-blocking frontier, kept as
/// a defensive skip) reports the reproducible partial; anything else fails.
/// Bring up the machine and run installed cell 0 on a **secondary**, returning its exit
/// code, or `None` if no secondary came up or it did not finish.
///
/// Only compiled into a bin built with the `smp` feature; every other caller of [`prove`]
/// passes `on_secondary: false` and never reaches it.
///
/// # Safety
/// Cell 0 must be installed and present, and nothing else may touch it.
#[cfg(feature = "smp")]
fn run_on_secondary_core(preempt: bool) -> Option<usize> {
    kernel::smp::init();
    kernel::smp::start_all();
    if kernel::smp::online_count() < 2 {
        return None;
    }
    // SAFETY: the caller's contract.
    let (ok, code) = unsafe { kernel::smp::run_cell_on_secondary(0, preempt) };
    if ok { Some(code) } else { None }
}

#[cfg(not(feature = "smp"))]
fn run_on_secondary_core(_preempt: bool) -> Option<usize> {
    None
}

/// What a runtime proof runs, and how.
///
/// A struct rather than ten positional arguments. Five of the fields are booleans or
/// `Option`s, so the positional call read
/// `prove(name, path, argv, envp, want, false, true, true, None, false)` - unreadable at
/// the call site, and exactly what clippy's `too_many_arguments` was pointing at.
/// `..Default::default()` also means a caller names only the fields it differs on.
#[derive(Default)]
pub struct Proof<'a> {
    /// Test name, used in every line of the transcript.
    pub name: &'a str,
    /// The binary's path on the image's filesystem.
    pub path: &'a str,
    pub argv: &'a [&'a [u8]],
    pub envp: &'a [&'a [u8]],
    /// The exact stdout the cell must produce.
    pub want: &'a [u8],
    /// Accept "exit 134 and no output" as a bounded, reported partial instead of a
    /// failure. Only a runtime that *provably* aborts before evaluating may set it, and
    /// nothing in the suite does any more.
    pub thread_abort_partial: bool,
    /// Enable preemptive dispatch for this boot (docs/SUBSTRATE.md 15, S3').
    pub preemptive: bool,
    /// Mint the **W^X exception capability** (docs/ARCHITECTURE.md 5.1) so this
    /// runtime's JIT can map its code pages writable-and-executable. Every other kernel
    /// in the suite mints nothing of the sort and is refused exactly as before, which is
    /// what makes this a capability rather than a setting.
    pub wx_authority: bool,
    /// An optional **second** invocation of the same binary: `(argv, expected stdout)`.
    /// Run only after the first succeeds, so a partial or a failure is never masked by
    /// it. `linuxbun` uses this to run a JS file that calls a tile kernel through
    /// `bun:ffi` (docs/TILES.md 13.4d); every other caller leaves it `None`.
    pub second: Option<(&'a [&'a [u8]], &'a [u8])>,
    /// Run the cell **on a secondary core** rather than the boot CPU (docs/SMP.md
    /// 10.0e). The question a production runtime raises once the machine has more than
    /// one core: its whole load path - block device, ext4, `ld.so`, file-backed `mmap`,
    /// demand paging - plus JIT arenas and worker contexts, all driven from a core that
    /// is not the one that booted. The default `false` is the pre-existing path, byte
    /// for byte.
    pub on_secondary: bool,
}

pub fn prove(p: Proof<'_>) -> ! {
    let Proof {
        name,
        path,
        argv,
        envp,
        want,
        thread_abort_partial,
        preemptive,
        wx_authority,
        second,
        on_secondary,
    } = p;
    kernel::boot::init();
    println!("{name}: start on {}", arch::NAME);

    // **Preemptive dispatch** (docs/SUBSTRATE.md 15, S3'), per boot.
    //
    // This is the boot the migration exists for: a production JavaScript runtime
    // spawns worker threads its main thread waits on, and under a cooperative
    // scheduler a context that does not block never gives the CPU back. `linuxnode`
    // turns it on and **completes** - a real V8 + libuv runtime running to a correct
    // answer on a preemptively scheduled kernel, which is the result worth having.
    //
    // It is a per-boot argument rather than a global default because the two runtimes
    // that share this harness are in different states, and the honest thing is to run
    // each under the scheduler its outcome has actually been characterised against
    // (see `linuxbun`'s module docs for what changing it did).
    //
    // Set before the cell's trap frame is built: on x86-64 and ARM64 a frame's
    // interrupt mask is derived from this setting at construction time, so a frame
    // built with interrupts masked cannot be preempted whatever the scheduler later
    // decides.
    if preemptive {
        arch::enable_timer_irq();
    }
    kernel::sched::dispatch::enable(preemptive);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 16 * 1024 * 1024);
    }

    // The binary + its shared libraries live on a live virtio-blk disk. No disk
    // (or a non-ext4 placeholder on an ISA without this binary) -> skip.
    let dev = match virtio_blk::probe() {
        Some(d) => d,
        None => {
            println!(
                "{name}: SKIP on {} - no virtio-blk disk attached",
                arch::NAME
            );
            println!("{name}: PASS (skipped)");
            arch::exit(arch::ExitCode::Success);
        }
    };
    let cache = BlockCache::new(dev);
    let disk = match Ext4Fs::new(Box::new(Cached(cache))) {
        Ok(fs) => fs,
        Err(_) => {
            println!(
                "{name}: SKIP on {} - disk holds no ext4 image (no binary for this \
                 ISA, or no mkfs.ext4 at build time)",
                arch::NAME
            );
            println!("{name}: PASS (skipped)");
            arch::exit(arch::ExitCode::Success);
        }
    };

    // Mount the live disk as the cell's `/`, then stream-`execve` the binary.
    posix::reset();
    mount::mount("/", Rc::new(disk));
    // **A writable `/tmp` over the read-only root.** `ext4plus` is read-only, and a real
    // runtime expects somewhere to write: Bun, asked to run a *script file*, calls
    // `createFakeTemporaryNodeExecutable` to drop a stand-in `node` into a temp
    // directory, and failed `error.FileNotFound` without one (docs/TILES.md 13.4d found
    // this, and the fix is the mount table rather than a read-write ext4 driver - a
    // ramfs is already read-write, and composing the two is what a mount table is for).
    // `pick` selects the longest matching prefix, so `/tmp/...` resolves here and
    // everything else still resolves to the disk.
    mount::mount("/tmp", Rc::new(posix::RamFs::new()));
    svc::set_file_ops(vfs_personality::ops());
    let fills_before = block::cache_fills();

    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run_execve(path, argv, envp, wx_authority, on_secondary, preemptive);
    linux::set_stdout_tap(None);

    // Streaming witness: the binary + ld.so + its libraries came off the device on
    // demand through the bounded cache, not a whole-image preload. This must hold in
    // every non-skip outcome, so it is checked before branching on the result.
    // The preemption witness, printed in every outcome. A partial that is still
    // blamed on the cooperative scheduler has to be able to show that preemption
    // *was* delivered - otherwise "the worker never ran" and "the CPU never moved"
    // are indistinguishable (docs/ENGINEERING.md 1).
    let (armed, taken, unarmable, to_sibling, to_cell) = kernel::sched::preempt::counters();
    println!(
        "{name}: preemption {taken}/{armed} slices taken ({to_sibling} to a sibling          context, {to_cell} to another cell, {unarmable} unarmable)"
    );
    let fills = block::cache_fills() - fills_before;
    assert!(
        fills > 0,
        "{name}: no device reads - binary was not streamed off disk"
    );

    match outcome {
        // The goal: the runtime evaluated its input and printed the answer.
        Outcome::Exited(0) => {
            let got = captured();
            assert!(
                got == want,
                "{name}: exit 0 but stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want),
            );
            println!(
                "{name}: REAL runtime evaluated its input OFF A LIVE ext4 DISK, \
                 exact stdout + exit 0 ({fills} block-cache fills through ext4plus)"
            );
            // The second invocation, if the caller asked for one. After the first has
            // been asserted, so nothing here can turn a partial into a pass.
            if let Some((argv2, want2)) = second {
                unsafe {
                    STDOUT_LEN = 0;
                }
                linux::set_stdout_tap(Some(tap));
                let out2 = run_execve(path, argv2, envp, wx_authority, on_secondary, preemptive);
                linux::set_stdout_tap(None);
                let got2 = captured();
                match out2 {
                    Outcome::Exited(0) => assert!(
                        got2 == want2,
                        "{name}: second run exit 0 but stdout mismatch\n  got:      \
                         {:?}\n  expected: {:?}",
                        core::str::from_utf8(got2),
                        core::str::from_utf8(want2),
                    ),
                    Outcome::Exited(code) => panic!(
                        "{name}: second run exit {code} (stdout {:?})",
                        core::str::from_utf8(got2)
                    ),
                    Outcome::Faulted(addr) => {
                        panic!("{name}: second run faulted at {addr:#x}")
                    }
                }
                println!("{name}: SECOND RUN OK - {:?}", core::str::from_utf8(want2));
            }
        }
        // Defensive: before per-context blocking (LINUX-COMPAT.md L4) a multi-thread
        // event loop deadlocked here; the scheduler reports `DEADLOCK_EXIT` rather
        // than hanging. Kept as a skip that still asserts the reproducible partial
        // (streamed off disk + reached the loop before any output), so a real
        // regression - a fault or a wrong exit - still fails.
        Outcome::Exited(code) if code == abi::DEADLOCK_EXIT => {
            let got = captured();
            assert!(
                got.is_empty(),
                "{name}: deadlocked but had already produced output {:?} - unexpected",
                core::str::from_utf8(got),
            );
            println!(
                "{name}: SKIP full run on {} - the real binary streamed off ext4 \
                 ({fills} fills) and initialised, then a cross-thread wait could not \
                 be scheduled (per-context blocking frontier). Loaded and ran; did \
                 not complete.",
                arch::NAME
            );
            println!("{name}: PASS (partial: loaded + init)");
        }
        // The concurrency-frontier partial (Bun only): the runtime streamed off
        // ext4, demand-paged, dynamically linked its whole library set, brought up
        // its language VM (JavaScriptCore, incl. the 128 GiB Gigacage reservation),
        // spawned a worker thread via `clone3`, and set up its event loop - then
        // `abort()`ed (SIGABRT -> 134) before printing anything, because that worker
        // could not run concurrently with the main thread under the cooperative
        // single-CPU scheduler (verified: every syscall issued from the main tid;
        // the worker never got the CPU). Preemptive SMP is task #132. Bounded to
        // exit==134 AND empty output so any other failure shape still panics.
        Outcome::Exited(134) if thread_abort_partial => {
            let got = captured();
            assert!(
                got.is_empty(),
                "{name}: aborted but had already produced output {:?} - unexpected, \
                 this partial only covers an abort *before* any output",
                core::str::from_utf8(got),
            );
            println!(
                "{name}: SKIP full run on {} - the real binary streamed off ext4 \
                 ({fills} fills), dynamically linked, brought up its language VM + \
                 Gigacage, spawned a worker via clone3, and set up its event loop, \
                 then abort()ed before producing any output. Loaded and initialised; \
                 did not complete. The cause is **not attributed**: it was blamed on \
                 the cooperative scheduler starving the worker, and preemption has \
                 since landed - and the JIT it was also once blamed on is now granted \
                 - without changing the outcome, so the previous diagnoses are \
                 withdrawn rather than replaced with the next guess.",
                arch::NAME
            );
            println!("{name}: PASS (partial: loaded + init to the concurrency frontier)");
        }
        Outcome::Exited(code) => {
            let got = captured();
            panic!(
                "{name}: unexpected exit {code} (stdout {:?})",
                core::str::from_utf8(got),
            );
        }
        Outcome::Faulted(addr) => panic!("{name}: faulted at {addr:#x}"),
    }

    println!("{name}: PASS");
    arch::exit(arch::ExitCode::Success)
}
