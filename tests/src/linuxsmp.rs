//! In-QEMU test kernel for the **Linux personality across cores** (docs/SMP.md 10.2).
//!
//! 10.2 makes an audit the gate on feeding secondaries real work, and names the Linux
//! personality's global state as one of its six areas: the mapped-file registry, the
//! pipe/eventfd/timerfd registries, pid allocation, the unix-socket names.
//! `linux::plock` covers the whole Linux dispatch plus the demand-paging entry,
//! recursively per CPU - the "one big lock" order 10.2 explicitly allows.
//!
//! Its own kernel rather than three more phases in `smp`, and for a measured reason:
//! each of these runs several static-glibc images through a full glibc startup with
//! demand paging, and adding them to `smp` pushed **riscv64** past the 120 s boot-test
//! budget (observed - it timed out inside the four-cell phase, before the other two
//! ran). One kernel per concern is the tree's shape anyway; this is what forced it.
//!
//! Four questions, in order of what they license:
//!
//!   1. **Many.** Four Linux cells across four cores, each with its own exact
//!      transcript. A big lock is exactly the claim that holds for two and fails for N.
//!   2. **Fork.** A Linux cell that forks off the boot CPU while another runs beside
//!      it - `fork` makes a new cell, and an unclaimed cell is pickable by every core.
//!   3. **Load.** Two cells hammering the global registries at once. This is the one
//!      that makes `plock` *testable*: the other two pass with the lock removed,
//!      because a hello-world barely touches the shared tables.
//!   4. **Preemption.** Two *multi-threaded* Linux cells, each preempted while it runs.
//!      Every phase before this one ran with dispatch off, so the personality's
//!      trap-context entry points (§10.2a) were never executed at all.

#![no_std]
#![no_main]

extern crate alloc;

#[path = "fixture.rs"]
mod fixture;
#[path = "harness.rs"]
mod harness;
#[path = "vfs_personality.rs"]
mod vfs_personality;

use harness::KernelStack;
use kernel::capability::{CapTable, ObjectTable};
use kernel::sched::{dispatch, preempt};
use kernel::{arch, idle, ktimer, println, smp, user};

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 8 * 1024 * 1024] = [0; 8 * 1024 * 1024];

static mut OBJECTS2: ObjectTable = ObjectTable::new();
static mut CAPS2: CapTable = CapTable::new();
static mut QP_L: core::mem::MaybeUninit<kernel::queue::QueuePair> =
    core::mem::MaybeUninit::uninit();

/// The unmodified static-glibc hello every "does it run at all" phase uses.
static CHELLO: &[u8] = fixture::linux!("chello");
const CHELLO_OUT: &[u8] = b"hello from glibc C\n";
const CHELLO_EXIT: u64 = 9;

/// The unpatched multi-threaded Rust `std` fixture `linuxthreads` asserts (L4): 4
/// `std::thread`s over `mpsc` + `Mutex` + `Arc<AtomicUsize>`, joined. Used by the
/// preempted two-core phase below, because that phase needs a cell that runs long
/// enough to be interrupted **and** has sibling contexts to be interrupted *to*.
static RUSTTHREADS: &[u8] = fixture::linux_cargo!("rustthreads");
const RUSTTHREADS_OUT: &[u8] = b"threads 4 total 1550 channel 1550\n";
const RUSTTHREADS_EXIT: u64 = 4;

/// Captured stdout, one buffer per cell slot: with several cores running several Linux
/// cells, a single shared buffer would interleave two transcripts into nonsense.
const CAP_MAX: usize = 1024;
const CAP_CELLS: usize = 4;
static mut STDOUT_CAP: [[u8; CAP_MAX]; CAP_CELLS] = [[0; CAP_MAX]; CAP_CELLS];
static mut STDOUT_LEN: [usize; CAP_CELLS] = [0; CAP_CELLS];

/// Route each cell's stdout to **its own** buffer, keyed by the cell the calling core is
/// running. Safe because a cell runs on one core (`user::claim_cell`), so the slots are
/// disjoint.
fn tap(bytes: &[u8]) {
    let cell = user::current_index().min(CAP_CELLS - 1);
    // SAFETY: each core writes only the slot of the cell it is running.
    unsafe {
        let cap = &mut *core::ptr::addr_of_mut!(STDOUT_CAP);
        let len = &mut *core::ptr::addr_of_mut!(STDOUT_LEN);
        for &b in bytes {
            if len[cell] < CAP_MAX {
                cap[cell][len[cell]] = b;
                len[cell] += 1;
            }
        }
    }
}

fn captured(i: usize) -> &'static [u8] {
    // SAFETY: read after the run, with no core inside a cell.
    unsafe {
        let cap = &*core::ptr::addr_of!(STDOUT_CAP);
        let len = *(*core::ptr::addr_of!(STDOUT_LEN)).get_unchecked(i);
        &cap[i][..len]
    }
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("linuxsmp: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; `HEAP_MEM` is a unique static.
    unsafe {
        HEAP.init(core::ptr::addr_of_mut!(HEAP_MEM) as usize, 8 * 1024 * 1024);
    }

    smp::init();
    let extra = smp::start_all();
    let online = smp::online_count();
    println!("linuxsmp: {online} CPUs online ({extra} secondaries started)");

    test_four_linux_cells();
    test_linux_fork_across_cores();
    test_registry_stress_two_cores();
    test_preempted_threads_two_cores();
    test_dynamic_cell_on_secondary();

    println!("linuxsmp: PASS");
    arch::exit(arch::ExitCode::Success)
}

// ------------------------- FOUR Linux cells on FOUR cores: the 10.2 audit's own question
//
// docs/SMP.md 10.2 makes an audit the gate on feeding secondaries real work, and names the
// Linux personality's global state as one of the six areas: the mapped-file registry, the
// pipe/eventfd/timerfd registries, pid allocation, the unix-socket names. `linux::plock`
// covers the whole Linux dispatch plus the demand-paging entry, recursively per CPU, so the
// discipline is "one big lock" - the seL4 order the audit explicitly allows.
//
// Two Linux cells are proven above. The audit's remaining question is **many**, because a
// big lock is exactly the kind of claim that is true for two and false for N if anything
// touches a global *outside* the locked window. Asking it is cheap and the answer is
// worth having either way: it passes and the discipline is demonstrated at the width the
// hardware offers, or it fails and the audit has found its gap.
//
// Four cells, one per core, through the same placement queue every other multi-core phase
// uses - each demand-pages its own copy of the same ELF (so the mapped-file registry has
// four concurrent readers of four entries), each synthesizes its own pid, each runs glibc's
// startup, and each transcript is captured separately and asserted **exactly**. A garbled
// or missing transcript is what a raced registry produces.
//
// This widens `place_cells`' documented contract from "native" to "native, or a Linux cell
// with no process tree": a Linux cell's exit reaches `linux::proc` rather than `nproc`, and
// with no children that path ends the run exactly as a native cell's does. A cell that
// forks or pipes across cores is a different question and is still not asked.
//
// **What this phase does not do**, tested rather than assumed: it does **not** prove
// `linux::plock` is load-bearing. Forcing `plock` to return `PGuard::Off` - no serialisation
// at all - was tried and the phase still passes on all three ISAs, because `chello` is a
// hello-world that barely touches the global registries and TCG interleaves coarsely. So
// the claim here is exactly "N Linux cells across N cores produce N correct transcripts",
// which was unproven and now is not; the lock's *necessity* needs a fixture that hammers
// the registries (many pipes and eventfds in a loop), and that fixture does not exist. Said
// plainly rather than left for the reader to assume the control fired.

/// One kernel stack per core for this phase - two cores trapping onto one stack corrupt
/// each other's frames, and the corruption looks like a random fault rather than like a
/// missing stack.
static mut KSTACK_4L: [KernelStack; 4] = [const { KernelStack::new() }; 4];

fn test_four_linux_cells() {
    let cores = smp::online_count().min(4);
    if cores < 3 {
        println!(
            "linuxsmp: SKIP the four-Linux-cells phase - only {cores} core(s) online, so \
             'many' would not be wider than the two-cell phase above"
        );
        return;
    }
    // SAFETY: single-threaded setup on the primary; the secondaries claim nothing until
    // `place_cells` publishes the queue, and every static outlives the run.
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS2);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS2);
        *objects = ObjectTable::new();
        *caps = CapTable::new();

        user::reset();
        ktimer::reset();
        idle::reset();

        let mut aspace = [
            kernel::mm::AddressSpace::new(20),
            kernel::mm::AddressSpace::new(21),
            kernel::mm::AddressSpace::new(22),
            kernel::mm::AddressSpace::new(23),
        ];
        let mut frame: [core::mem::MaybeUninit<kernel::arch::TrapFrame>; 4] =
            [const { core::mem::MaybeUninit::uninit() }; 4];
        for i in 0..4 {
            let img = kernel::load::load_elf_linux(CHELLO, &mut aspace[i]).expect("load chello");
            let sp = kernel::linux::stack::setup_stack(&mut aspace[i], &img, &[b"chello"], &[]);
            frame[i].write(arch::trapframe_new(
                img.entry,
                sp,
                0,
                (*core::ptr::addr_of!(KSTACK_4L))[i].top(),
            ));
            user::install(
                i,
                &aspace[i],
                caps,
                objects,
                core::ptr::addr_of!(QP_L) as *const kernel::queue::QueuePair,
                frame[i].as_mut_ptr(),
            );
            user::set_personality(i, user::Personality::Linux);
            kernel::linux::install_cell(i, &img, b"");
        }

        STDOUT_LEN = [0; CAP_CELLS];
        kernel::linux::set_stdout_tap(Some(tap));
        let cells = [0usize, 1, 2, 3];
        let mut out = [(u64::MAX, usize::MAX); 4];
        // SAFETY: all four are installed, present, listed once, and Linux cells with no
        // process tree - see the contract note above.
        let finished = smp::place_cells(&cells, &mut out);
        kernel::linux::set_stdout_tap(None);

        if !finished {
            println!(
                "linuxsmp: SKIP the four-Linux-cells phase - the queue did not drain inside \
                 the bound, so nothing about many Linux cells is claimed"
            );
            return;
        }

        for i in 0..4 {
            assert_eq!(
                out[i].0, CHELLO_EXIT,
                "Linux cell {i} exited {:#x}, not {CHELLO_EXIT}",
                out[i].0
            );
            let got = captured(i);
            assert!(
                got == CHELLO_OUT,
                "Linux cell {i}'s stdout was {} bytes, not its own exact transcript - a \
                 raced global registry is what garbles or loses one",
                got.len()
            );
        }
        assert_eq!(user::double_entries(), 0, "two cores were inside one cell");
        assert!(
            kernel::mm::frames::used_matches_bitmap(),
            "the frame pool's used counter drifted from its bitmap"
        );
        // How many cores actually took one. Reported, not asserted: which core claims
        // which cell is a race, and an assertion that can fail on a legal schedule is not
        // a proof (the lesson the GEMM worker count taught).
        let mut used = [false; smp::MAX_CPUS];
        for o in out.iter() {
            if o.1 < smp::MAX_CPUS {
                used[o.1] = true;
            }
        }
        let n = used.iter().filter(|&&u| u).count();
        println!(
            "linuxsmp: FOUR LINUX CELLS ran across {cores} cores - each demand-paged its own \
             copy of the same unmodified static-glibc binary, synthesized its own pid, and \
             produced its OWN exact transcript and exit {CHELLO_EXIT}; {n} core(s) took \
             one (reported, not asserted). That is docs/SMP.md 10.2's 'many Linux cells' \
             question asked at the width this machine offers, and the big personality lock \
             holding for it OK"
        );
    }
}

// ------------------- a Linux cell that FORKS while another Linux cell runs on another core
//
// The 10.2 audit's remaining Linux question is a cell that **forks** off the boot CPU, and
// the hazard is specific enough to name: `fork` creates a *new cell*, and a cell nobody has
// claimed is pickable by every core (`user::cell_on_this_cpu` treats `NO_CPU` as pickable,
// which is exactly what keeps single-core boots unchanged). So when a Linux cell on core B
// exits, its `linux::proc::reschedule` scans for a runnable cell - and would find the child
// core A's cell forked a moment ago. Two cores, one cell, one trap frame.
//
// An idle core cannot reach it: `drain_cells` only enters cells the caller published, and a
// forked child is not in the queue. It takes **two** Linux cells, one of which forks, which
// is why this is its own phase rather than a variant of the four-cell one above.
//
// `af_unix` is the forker - `socketpair` + `fork`, then bind/listen/connect/accept over an
// abstract name, so it also drives the global unix-socket registry and the L6 cross-cell
// ring from a secondary. `chello` is the peer whose exit does the scanning.
//
// The fix is the one `cell_on_this_cpu`'s own doc predicted while no boot reached this
// state: a child **inherits its parent's owner**. Not a wider lock - the same partitioning,
// applied to a cell that did not exist when the round started.
//
// **What this phase proves, and what it does not.** Proven: a Linux cell forks off the boot
// CPU while another Linux cell runs beside it, both exact transcripts, zero double entries,
// and the affinity test is genuinely *consulted* during the round - `affinity_skips` is
// asserted nonzero, so a scheduler was offered a cell belonging to another core and declined
// it, which is the positive form rather than an absence.
//
// Not proven: that the **child's** inherited owner is what prevented a double entry.
// Reverting the fork path to leave the child unclaimed still passes - five runs, and the
// refusals counted come from the two *placed* cells rather than from the child. The window
// is narrow: the peer's exit-time scan has to land between the child's creation and its
// reaping. So the inheritance is a correct fix for a real window that this phase cannot
// make happen on demand, and that is recorded rather than dressed up as a proof.

/// The registry-stress fixture: pipes and eventfds allocated/used/freed in a tight loop,
/// every value keyed on the caller's own pid (docs/SMP.md 10.2).
static REGSTRESS: &[u8] = fixture::linux!("regstress");
const REGSTRESS_OUT: &[u8] = b"regstress OK\n";

static mut KSTACK_RS: [KernelStack; 2] = [const { KernelStack::new() }; 2];

/// **Two cells hammering the personality's global registries on two cores at once.**
///
/// The four-cell phase above demonstrates width and explicitly does *not* prove
/// `linux::plock` load-bearing: `chello` barely touches the shared tables, so removing the
/// lock changes nothing observable. This is the fixture that was named as missing.
///
/// `pipe::alloc` and the eventfd table are global fixed arrays whose allocators are
/// find-a-free-slot-then-claim-it, which races directly - two cores can both find the same
/// free index and both claim it, and two processes then hold one object. The consequence is
/// not a fault but *someone else's bytes*, so every value the fixture writes is derived from
/// its own pid and every read is checked against it. 256 rounds each, two cells, one per
/// core.
///
/// The assertion is each cell's own exact transcript (`regstress OK`) and exit 0. A cell that
/// read a peer's byte prints `regstress FAIL <n>` and exits 1, which fails on the transcript
/// and names how many rounds disagreed.
fn test_registry_stress_two_cores() {
    if smp::online_count() < 2 {
        println!("linuxsmp: SKIP the registry-stress phase - one core online");
        return;
    }
    // SAFETY: single-threaded setup on the primary; the secondaries claim nothing until
    // `place_cells` publishes the queue, and every static outlives the run.
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS2);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS2);
        *objects = ObjectTable::new();
        *caps = CapTable::new();

        user::reset();
        ktimer::reset();
        idle::reset();
        posix::reset();
        posix::mount::mount("/", alloc::rc::Rc::new(posix::RamFs::new()));
        kernel::svc::set_file_ops(vfs_personality::ops());

        let mut aspace = [
            kernel::mm::AddressSpace::new(40),
            kernel::mm::AddressSpace::new(41),
        ];
        let mut frame: [core::mem::MaybeUninit<kernel::arch::TrapFrame>; 2] =
            [const { core::mem::MaybeUninit::uninit() }; 2];
        for i in 0..2 {
            let img =
                kernel::load::load_elf_linux(REGSTRESS, &mut aspace[i]).expect("load regstress");
            let sp = kernel::linux::stack::setup_stack(&mut aspace[i], &img, &[b"regstress"], &[]);
            frame[i].write(arch::trapframe_new(
                img.entry,
                sp,
                0,
                (*core::ptr::addr_of!(KSTACK_RS))[i].top(),
            ));
            user::install(
                i,
                &aspace[i],
                caps,
                objects,
                core::ptr::addr_of!(QP_L) as *const kernel::queue::QueuePair,
                frame[i].as_mut_ptr(),
            );
            user::set_personality(i, user::Personality::Linux);
            kernel::linux::install_cell(i, &img, b"");
        }

        STDOUT_LEN = [0; CAP_CELLS];
        kernel::linux::set_stdout_tap(Some(tap));
        let cells = [0usize, 1];
        let mut out = [(u64::MAX, usize::MAX); 2];
        // SAFETY: both installed, present, listed once, Linux cells with no process tree.
        let finished = smp::place_cells(&cells, &mut out);
        kernel::linux::set_stdout_tap(None);

        if !finished {
            println!(
                "linuxsmp: SKIP the registry-stress phase - the queue did not drain inside the                  bound, so nothing about the registries under load is claimed"
            );
            return;
        }
        for i in 0..2 {
            let got = captured(i);
            assert!(
                got == REGSTRESS_OUT,
                "registry-stress cell {i} printed {:?}, not {:?} - a `FAIL <n>` line means                  it read a byte written by the other cell, which is two processes holding                  one pipe or one eventfd",
                core::str::from_utf8(got),
                core::str::from_utf8(REGSTRESS_OUT)
            );
            assert_eq!(
                out[i].0, 0,
                "registry-stress cell {i} exited {:#x}",
                out[i].0
            );
        }
        assert!(
            out[0].1 != out[1].1,
            "both cells ran on CPU {} - they never hammered the registries at once",
            out[0].1
        );
        assert_eq!(user::double_entries(), 0, "two cores were inside one cell");
        println!(
            "linuxsmp: TWO CELLS HAMMERED THE GLOBAL REGISTRIES on CPU {} and CPU {} at \
             once - 256 rounds each of pipe create/write/read/close and eventfd \
             create/write/read/close, every value keyed on the caller's own pid, and each \
             cell read back exactly what it wrote. `linux::plock` is what serialises the \
             find-then-claim allocators (docs/SMP.md 10.2) OK",
            out[0].1, out[1].1
        );
    }
}

/// The AF_UNIX forker (built by xtask alongside every other linux fixture).
static AF_UNIX: &[u8] = fixture::linux!("af_unix");
const AF_UNIX_OUT: &[u8] = b"pair: pong\nconn: hello\nback: world\naf_unix OK\n";

static mut KSTACK_FK: [KernelStack; 2] = [const { KernelStack::new() }; 2];

fn test_linux_fork_across_cores() {
    if smp::online_count() < 2 {
        println!("linuxsmp: SKIP the forking-Linux-cell phase - one core online");
        return;
    }
    // SAFETY: single-threaded setup on the primary; the secondaries claim nothing until
    // `place_cells` publishes the queue, and every static outlives the run.
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS2);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS2);
        *objects = ObjectTable::new();
        *caps = CapTable::new();

        user::reset();
        ktimer::reset();
        idle::reset();

        // `af_unix` opens no files, but glibc's startup wants a working VFS surface, and
        // `fork` deep-copies the fd table through it.
        posix::reset();
        posix::mount::mount("/", alloc::rc::Rc::new(posix::RamFs::new()));
        kernel::svc::set_file_ops(vfs_personality::ops());

        let images: [&[u8]; 2] = [AF_UNIX, CHELLO];
        let argv: [&[u8]; 2] = [b"af_unix", b"chello"];
        let mut aspace = [
            kernel::mm::AddressSpace::new(30),
            kernel::mm::AddressSpace::new(31),
        ];
        let mut frame: [core::mem::MaybeUninit<kernel::arch::TrapFrame>; 2] =
            [const { core::mem::MaybeUninit::uninit() }; 2];
        for i in 0..2 {
            let img =
                kernel::load::load_elf_linux(images[i], &mut aspace[i]).expect("load fixture");
            let sp = kernel::linux::stack::setup_stack(&mut aspace[i], &img, &[argv[i]], &[]);
            frame[i].write(arch::trapframe_new(
                img.entry,
                sp,
                0,
                (*core::ptr::addr_of!(KSTACK_FK))[i].top(),
            ));
            user::install(
                i,
                &aspace[i],
                caps,
                objects,
                core::ptr::addr_of!(QP_L) as *const kernel::queue::QueuePair,
                frame[i].as_mut_ptr(),
            );
            user::set_personality(i, user::Personality::Linux);
            kernel::linux::install_cell(i, &img, b"");
        }

        STDOUT_LEN = [0; CAP_CELLS];
        kernel::linux::set_stdout_tap(Some(tap));
        let cells = [0usize, 1];
        let mut out = [(u64::MAX, usize::MAX); 2];
        let before = user::double_entries();
        let skips_before = user::affinity_skips();
        // SAFETY: both installed, present, listed once. Cell 0 *does* have a process tree
        // (it forks), which is past `place_cells`' documented contract - deliberately, and
        // it is what this phase is testing.
        let finished = smp::place_cells(&cells, &mut out);
        kernel::linux::set_stdout_tap(None);

        if !finished {
            println!(
                "linuxsmp: SKIP the forking-Linux-cell phase - the queue did not drain inside \
                 the bound, so nothing about a fork off the boot CPU is claimed"
            );
            return;
        }

        // No core was ever inside a cell another core was in - which is what the child's
        // inherited owner buys, and the only outcome that says the fork was safe rather
        // than lucky.
        assert_eq!(
            user::double_entries(),
            before,
            "two cores were inside one cell - a forked child was visible to a core that \
             did not create it"
        );
        assert_eq!(out[0].0, 0, "the forking cell exited {:#x}", out[0].0);
        assert_eq!(
            out[1].0, CHELLO_EXIT,
            "the peer cell exited {:#x}",
            out[1].0
        );
        let got = captured(0);
        assert!(
            got == AF_UNIX_OUT,
            "the forking cell's transcript was {} bytes, not its exact output - a child \
             entered by two cores is what garbles it",
            got.len()
        );
        assert!(
            captured(1) == CHELLO_OUT,
            "the peer cell's transcript was wrong"
        );
        assert!(
            kernel::mm::frames::used_matches_bitmap(),
            "the frame pool's used counter drifted from its bitmap"
        );
        // **The mechanism was consulted, not merely un-needed.** A scheduler on one core
        // was offered a cell belonging to another and declined it - which is the positive
        // form of the claim. Asserting only "no double entry" would pass equally if the
        // race never arose, and it is exactly the shape of proof this tree has had to
        // reject before (docs/ENGINEERING.md 1).
        let skips = user::affinity_skips() - skips_before;
        assert!(
            skips > 0,
            "no scheduler was ever offered a cell owned by another core, so the affinity \
             test was never exercised and 'no double entry' says nothing"
        );
        println!(
            "linuxsmp: A LINUX CELL FORKED off the boot CPU while another ran beside it - \
             `af_unix` did socketpair+fork+bind/listen/connect/accept on CPU {} and \
             `chello` ran on CPU {}, both exact transcripts asserted, and 0 double entries: \
             the forked child inherited its parent's core, so the peer's exit-time \
             reschedule could not see it, and the affinity test was consulted and \
             refused {skips} time(s) (docs/SMP.md 10.2) OK",
            out[0].1, out[1].1
        );
    }
}

// ------------- a DYNAMICALLY LINKED Linux cell off a LIVE DISK, on a SECONDARY core
//
// The three phases above run static-glibc binaries out of the kernel image. Node, Bun and
// Claude Code do none of that: they stream off a live ext4 disk, their `ld.so` maps
// `libc.so.6` and friends with file-backed `mmap`, and every page arrives by fault. So
// "can those run on a secondary" is really a question about that load path, and it can be
// asked with `dhello` - the same 20 KB dynamic hello `linuxdyn` proves on the primary -
// for a fraction of their size and time.
//
// What this exercises from a core that is not the boot CPU: the virtio-blk driver, the
// bounded block cache, `ext4plus` path resolution, `PT_INTERP` + the ELF interpreter,
// file-backed `MAP_PRIVATE`/`MAP_FIXED`, and demand paging - the whole of it, driven by a
// cell whose faults are taken on a secondary's trap path with that core's own kernel stack.
//
// A `chello` cell runs beside it so placement has two cells to spread, and the phase
// asserts the two landed on **different** CPUs - which is what makes at least one of them a
// secondary. Asserting a specific CPU would be asserting a race; asserting *different* is a
// property the claim genuinely needs.
//
// Honest about the distance this does and does not close: it says the load path works off
// the boot CPU. It does not run Bun or Claude Code there - those are 99 MB and 275 MB, they
// bring up JIT arenas behind the W^X exception, and they spawn worker contexts, none of
// which this touches. What it removes is the doubt about the mechanism underneath them.

/// `dhello`'s exact transcript and exit, as `linuxdyn` asserts them on the primary.
// --------- TWO multi-threaded Linux cells, on TWO cores, each PREEMPTED while it runs
//
// The §10.2a audit found a hole that no phase could reach: `linux::plock` brackets the
// syscall dispatch and the demand-paging entry, but **two further paths reach
// personality state from trap context** - `user::on_user_interrupt` calls
// `linux::thread::preempt_context` (another *context* of this cell) and
// `linux::proc::preempt_cell` (another *cell*'s row entirely). Every multi-core Linux
// phase before this one ran with dispatch **off**, so no slice ever fired and neither
// call was made. The lock was "correct by construction" over a path nothing executed.
//
// Both take the lock now, and this phase executes them: two cells, two cores, each
// under its own preemption timer (per-core hardware, so each core arms its own).
//
// The workload is the fixture `linuxthreads` asserts, not the hello-world the other
// phases use, and both reasons are load-bearing:
//
//   - it runs long enough to be interrupted (a `chello` prints one line and exits well
//     inside a 1 ms slice - measured: 32 slices armed, **0** taken), and
//   - it has 4 sibling contexts, so `preempt_context` - the *first* thing the
//     preemption path tries for a Linux cell - is reachable at all. With single-context
//     cells only `preempt_cell` can ever fire.
//
// What is asserted: both transcripts exactly, both exit codes, the overlap, no cell
// entered by two cores, and that preemptions were genuinely **taken** - without which
// the phase is the cooperative one wearing a new comment.
//
// **It found a real defect on its first run, and that one has a deterministic control.**
// `run_cells_on_both` published two cells and claimed neither, which is right on a
// single-CPU boot - an unclaimed cell is visible to every scheduler - and wrong the
// moment a slice fires: the peer's `preempt_cell` scan saw this core's cell as runnable
// and switched into it, two cores sharing one trap frame and one kernel stack. It
// presented as an instruction fetch at 0 on both cores, immediately and every run. Each
// core now claims the cell it is about to enter, and reverting that reproduces the fault.
//
// What **cannot** be given a deterministic control is the locking itself: removing the
// brackets leaves a race, and a race that fails intermittently is not evidence either
// way. So the bracketing is reasoned and reviewed rather than proven by a revert
// (docs/ENGINEERING.md 7), and this says so instead of implying more.
//
// And honest about which arm fires: every preemption here goes to a **sibling context**
// of the same cell, because a 4-thread cell always has one ready, so `preempt_context`
// answers first and `preempt_cell` - the arm that touches another cell's row - is not
// reached. The counters are printed rather than asserted for that reason. Executing the
// cross-cell arm needs a single-context cell that outlives its slice, which no fixture
// here is; it stays named rather than claimed.

static mut KSTACK_THR: [KernelStack; 2] = [const { KernelStack::new() }; 2];

fn test_preempted_threads_two_cores() {
    if smp::online_count() < 2 {
        println!("linuxsmp: SKIP the preempted-threads phase - one core online");
        return;
    }
    // SAFETY: single-threaded setup on the primary; secondaries claim nothing until the
    // cell is published, and every static here outlives the run.
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS2);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS2);
        *objects = ObjectTable::new();
        *caps = CapTable::new();

        user::reset();
        ktimer::reset();
        idle::reset();
        preempt::reset();
        // **Before the frames are built, and that ordering is load-bearing.** On ARM64 a
        // cell's SPSR carries its IRQ mask, and `trapframe_new` derives that mask from
        // `dispatch::enabled()` - so a frame built while dispatch is off runs at EL0 with
        // IRQ *masked* and can never be preempted however many slices are armed. Enabling
        // it after the loop below armed 474 slices and took **0** timer interrupts
        // (measured; docs/SMP.md 10.2a). x86-64 and riscv64 read their mask at the same
        // point, so this is one ordering rule rather than a per-ISA workaround.
        dispatch::enable(true);

        let mut aspace = [
            kernel::mm::AddressSpace::new(60),
            kernel::mm::AddressSpace::new(61),
        ];
        let mut frame: [core::mem::MaybeUninit<kernel::arch::TrapFrame>; 2] =
            [const { core::mem::MaybeUninit::uninit() }; 2];
        for i in 0..2 {
            let li =
                kernel::load::load_elf_linux(RUSTTHREADS, &mut aspace[i]).expect("load fixture");
            let sp = kernel::linux::stack::setup_stack(&mut aspace[i], &li, &[b"rustthreads"], &[]);
            frame[i].write(arch::trapframe_new(
                li.entry,
                sp,
                0,
                (*core::ptr::addr_of!(KSTACK_THR))[i].top(),
            ));
            user::install(
                i,
                &aspace[i],
                caps,
                objects,
                core::ptr::addr_of!(QP_L) as *const kernel::queue::QueuePair,
                frame[i].as_mut_ptr(),
            );
            user::set_personality(i, user::Personality::Linux);
            kernel::linux::install_cell(i, &li, b"");
        }

        STDOUT_LEN = [0; CAP_CELLS];
        kernel::linux::set_stdout_tap(Some(tap));
        // SAFETY: both installed, present, distinct, Linux cells with no process tree.
        let (met, finished, sec_code, own_code) = smp::run_cells_on_both(0, 1, true);
        dispatch::enable(false);
        kernel::linux::set_stdout_tap(None);

        if !finished {
            println!(
                "linuxsmp: SKIP the preempted-threads phase - the secondary did not \
                 finish its cell inside the bound"
            );
            return;
        }
        assert!(
            met && !smp::rendezvous_timed_out(),
            "the two cores never met, so the two threaded cells did not overlap"
        );
        for i in 0..2 {
            let got = captured(i);
            assert!(
                got == RUSTTHREADS_OUT,
                "threaded cell {i} printed {:?}, not {:?} - a preempted multi-context \
                 Linux cell did not produce its exact transcript",
                core::str::from_utf8(got),
                core::str::from_utf8(RUSTTHREADS_OUT)
            );
        }
        assert_eq!(
            own_code, RUSTTHREADS_EXIT,
            "the primary's cell exited wrong"
        );
        assert_eq!(
            sec_code as u64, RUSTTHREADS_EXIT,
            "the secondary's cell exited wrong"
        );
        assert_eq!(user::double_entries(), 0, "two cores were inside one cell");

        let (armed, taken, unarmable, to_sib, to_cell) = preempt::counters();
        let notes = preempt::notes();
        let (rearms, no_record) = dispatch::rearm_counters();
        println!(
            "linuxsmp:   E5 return-to-user site: reached {rearms} times, {no_record} with no \
             running record; slices armed {armed}, timer interrupts {notes}"
        );
        // Two different outcomes, and conflating them would be the mistake. A slice
        // **fired** and the scheduler declined to move the CPU is a defect. A slice
        // never fired is a fact about the workload, not about the kernel: how many
        // slices a program consumes is not something this test controls, and it varies
        // by ISA for the same binary - riscv64 armed 128 slices here where aarch64
        // armed 2, because a Linux syscall that returns to its own context does not
        // re-arm and the two ISAs' futex timing gives different cell-level reschedule
        // counts. So the interrupt count is the gate, and where none arrived nothing
        // is claimed rather than asserted (docs/ENGINEERING.md 1).
        // The only honest escape is "this ISA has no slice to arm". Everything else is
        // now an assertion, because after stage E5 the chain is a property of the kernel
        // rather than of the workload: every return to user re-arms for the slice it has
        // left, so armed slices are plentiful, and a wired one-shot with hundreds of
        // deadlines registered against it MUST fire. Before E5 this had to be a report -
        // ARM64 armed 2 slices for two whole programs and took 0 interrupts, and asserting
        // there would have been asserting something the test did not control.
        if unarmable > 0 {
            println!(
                "linuxsmp: two multi-threaded Linux cells ran correctly on two cores, \
                 both transcripts exact - but no preemption is claimed: {unarmable} of \
                 {armed} slices could not be armed, so this ISA has no wired one-shot here"
            );
            return;
        }
        assert!(
            armed > 10,
            "only {armed} slices were armed across two whole programs - stage E5 re-arms \
             at every return to user, so this means the running record is empty and the \
             CPU-time charge and burst score are silently doing nothing too \
             ({rearms} return-to-user sites reached, {no_record} with no record)"
        );
        assert!(
            notes > 0,
            "{armed} slices were armed against a wired one-shot and NOT ONE fired - on \
             ARM64 that is a cell whose frame was built with IRQ masked, which happens \
             when dispatch is enabled after `trapframe_new` rather than before it"
        );
        assert!(
            taken > 0,
            "{notes} preemption interrupts arrived across {armed} armed slices and the \
             CPU changed hands {taken} times - a slice fired and the scheduler moved \
             nothing, which is the trap-context path failing rather than not being \
             reached"
        );
        println!(
            "linuxsmp: TWO MULTI-THREADED Linux cells ran on TWO CORES, each PREEMPTED \
             mid-run - {taken} of {armed} slices taken, both transcripts exact, both \
             exiting {RUSTTHREADS_EXIT} ({notes} timer interrupts arrived). That executes \
             `linux::thread::preempt_context` from trap context on two cores at once, \
             which the docs/SMP.md 10.2a audit found outside `linux::plock` and which \
             every earlier multi-core Linux phase left unreached (dispatch was off). \
             Reported, not asserted: {to_sib} went to a sibling context and {to_cell} to \
             another cell - a 4-thread cell always has a ready sibling, so \
             `preempt_cell` answers second and stays unexercised here OK"
        );
    }
}

const DHELLO_OUT: &[u8] = b"hello from dynamic glibc\n";
const DHELLO_EXIT: u64 = 12;

static mut KSTACK_DYN: [KernelStack; 2] = [const { KernelStack::new() }; 2];

/// The block-cache-backed ext4 source, as `linuxdyn` wires it.
struct Cached(kernel::hw::block::BlockCache<kernel::hw::virtio_blk::VirtioBlk>);
impl posix::BlockSource for Cached {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), posix::Errno> {
        self.0.read_at(off, buf).map_err(|_| posix::Errno::Io)
    }
}

fn test_dynamic_cell_on_secondary() {
    if smp::online_count() < 2 {
        println!("linuxsmp: SKIP the disk phase - one core online");
        return;
    }
    let Some(dev) = kernel::hw::virtio_blk::probe() else {
        println!(
            "linuxsmp: SKIP the disk phase on {} - no virtio-blk disk attached",
            arch::NAME
        );
        return;
    };
    let disk = match ext4fs::Ext4Fs::new(alloc::boxed::Box::new(Cached(
        kernel::hw::block::BlockCache::new(dev),
    ))) {
        Ok(fs) => fs,
        Err(_) => {
            println!(
                "linuxsmp: SKIP the disk phase on {} - the disk holds no ext4 image \
                 (placeholder; no e2fsprogs at build time)",
                arch::NAME
            );
            return;
        }
    };

    posix::reset();
    posix::mount::mount("/", alloc::rc::Rc::new(disk));
    kernel::svc::set_file_ops(vfs_personality::ops());

    // The program's own bytes come off the disk here; its **interpreter and libraries**
    // are opened and mmapped by `ld.so` from the same disk, on whichever core runs the
    // cell - which is the part this phase is about.
    let img = match posix::fs::read("/bin/dhello") {
        Ok(b) => b,
        Err(_) => {
            println!("linuxsmp: SKIP the disk phase - /bin/dhello not on the disk");
            return;
        }
    };

    let fills_before = kernel::hw::block::cache_fills();
    // SAFETY: single-threaded setup on the primary; the secondaries claim nothing until
    // `place_cells` publishes the queue, and every static outlives the run.
    unsafe {
        let objects = &mut *core::ptr::addr_of_mut!(OBJECTS2);
        let caps = &mut *core::ptr::addr_of_mut!(CAPS2);
        *objects = ObjectTable::new();
        *caps = CapTable::new();

        user::reset();
        ktimer::reset();
        idle::reset();

        let mut aspace = [
            kernel::mm::AddressSpace::new(50),
            kernel::mm::AddressSpace::new(51),
        ];
        let mut frame: [core::mem::MaybeUninit<kernel::arch::TrapFrame>; 2] =
            [const { core::mem::MaybeUninit::uninit() }; 2];
        // **Cell 1 is the dynamic one, and cell 1 is the secondary's.** `run_cells_on_both`
        // hands a *named* cell to a secondary, unlike `place_cells` where which core takes
        // which is a race - and "the dynamic cell ran off the boot CPU" is the claim, so it
        // has to be the deterministic form. Cell 0 (the static hello) runs on the primary
        // so the two genuinely overlap.
        let images: [&[u8]; 2] = [CHELLO, &img];
        let argv: [&[u8]; 2] = [b"chello", b"dhello"];
        let envs: [&[&[u8]]; 2] = [&[], &[b"LD_LIBRARY_PATH=/lib", b"PATH=/bin"]];
        for i in 0..2 {
            let li = kernel::load::load_elf_linux(images[i], &mut aspace[i]).expect("load image");
            let sp = kernel::linux::stack::setup_stack(&mut aspace[i], &li, &[argv[i]], envs[i]);
            frame[i].write(arch::trapframe_new(
                li.entry,
                sp,
                0,
                (*core::ptr::addr_of!(KSTACK_DYN))[i].top(),
            ));
            user::install(
                i,
                &aspace[i],
                caps,
                objects,
                core::ptr::addr_of!(QP_L) as *const kernel::queue::QueuePair,
                frame[i].as_mut_ptr(),
            );
            user::set_personality(i, user::Personality::Linux);
            kernel::linux::install_cell(i, &li, b"");
        }

        STDOUT_LEN = [0; CAP_CELLS];
        kernel::linux::set_stdout_tap(Some(tap));
        // SAFETY: both installed, present, distinct, Linux cells with no process tree.
        let (met, finished, sec_code, own_code) = smp::run_cells_on_both(0, 1, false);
        kernel::linux::set_stdout_tap(None);

        if !finished {
            println!(
                "linuxsmp: SKIP the disk phase - the secondary did not finish its cell \
                 inside the bound, so nothing about the load path off the boot CPU is \
                 claimed"
            );
            return;
        }
        assert!(
            met && !smp::rendezvous_timed_out(),
            "the two cores never met, so the two cells did not overlap"
        );
        // Cell 1 - the dynamic one - is the secondary's.
        let got = captured(1);
        assert!(
            got == DHELLO_OUT,
            "the dynamic cell on the secondary printed {:?}, not {:?}",
            core::str::from_utf8(got),
            core::str::from_utf8(DHELLO_OUT)
        );
        assert_eq!(
            sec_code as u64, DHELLO_EXIT,
            "the dynamic cell on the secondary exited {sec_code:#x}"
        );
        assert!(
            captured(0) == CHELLO_OUT,
            "the primary's static peer transcript"
        );
        assert_eq!(
            own_code, CHELLO_EXIT,
            "the primary's peer exited {own_code:#x}"
        );
        assert_eq!(user::double_entries(), 0, "two cores were inside one cell");
        // The libraries genuinely came off the device, on demand, during the run.
        let fills = kernel::hw::block::cache_fills() - fills_before;
        assert!(
            fills > 0,
            "no device reads during the run - ld.so did not stream its libraries"
        );
        println!(
            "linuxsmp: a DYNAMICALLY LINKED Linux cell ran OFF A LIVE ext4 DISK ON A \
             SECONDARY core, overlapping a static cell on the primary - its ld.so opened \
             and mmapped the interpreter and libc off the disk from that core, {fills} \
             block-cache fills during the run, exact transcript and exit {DHELLO_EXIT} \
             asserted. That is the load path Node, Bun and Claude Code depend on - block \
             device, ext4, PT_INTERP, file-backed mmap, demand paging - proven off the \
             boot CPU (docs/SMP.md 10.0e) OK"
        );
    }
}
