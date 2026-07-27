//! In-QEMU test kernel for Linux-personality milestone L7 (docs/LINUX-COMPAT.md
//! L7): **dynamic linking** - running an UNMODIFIED, dynamically-linked glibc
//! binary.
//!
//! The fixture (`dhello`) is a stock ET_DYN/PIE C hello built with the ISA's
//! gcc and no `-static`/`-no-pie`. Its `PT_INTERP` names `ld-linux-*.so`; the
//! loader (`load::load_elf_linux`) loads the interpreter as a second ET_DYN at
//! `LINUX_INTERP_BASE`, sets `AT_BASE`/`AT_PHDR`/`AT_ENTRY`, and starts
//! execution in ld.so. ld.so then `mmap`s + relocates the program and
//! `libc.so.6` at runtime over the L7 fd-backed `mmap`. The real toolchain
//! `ld.so` + `libc.so.6` are seeded into a ramfs `/lib` (copied from the
//! toolchain at build time by `xtask::build_dyn_fixture`; never committed), so
//! the interpreter resolves them exactly as on Linux.
//!
//! Any ISA whose runtime `.so` could not be located at build time gets a 1-byte
//! placeholder `ld.so`; the test detects that and **skips-with-reason** (the
//! static L2-L6 coverage stays green).

#![no_std]
#![no_main]

extern crate alloc;

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
use kernel::{arch, load, println};
use posix::{BlockSource, Errno, RamFs, fs, mount, sys};

#[path = "vfs_personality.rs"]
mod vfs_personality;

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 8 * 1024 * 1024] = [0; 8 * 1024 * 1024];

/// The L7 fixtures, per ISA (built by `xtask::build_dyn_fixture`).
macro_rules! fixture {
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

static DHELLO: &[u8] = fixture!("dhello");
static LD_SO: &[u8] = fixture!("ld.so");
static LIBC: &[u8] = fixture!("libc.so.6");
/// The **multi-library** fixtures (GOAL-DYN-MULTILIB): `dmath` links libm as well
/// as libc, so `ld.so` must load two shared libraries and resolve one's versions
/// against the other. A 1-byte placeholder means the toolchain libm was
/// unavailable at build time; the multi-library phase then skips-with-reason.
static DMATH: &[u8] = fixture!("dmath");
static LIBM: &[u8] = fixture!("libm.so.6");
/// The **four-library** fixtures: a dynamic C++ hello (`dcpp`) links libstdc++ +
/// libgcc_s + libc (+ libm transitively). Placeholders mean the toolchain g++ or
/// the C++ runtime libs were unavailable at build time (e.g. no cross-g++ for an
/// ISA); the C++ phase then skips-with-reason.
static DCPP: &[u8] = fixture!("dcpp");
static LIBSTDCPP: &[u8] = fixture!("libstdc++.so.6");
static LIBGCC: &[u8] = fixture!("libgcc_s.so.1");

/// The dynamic-linker path named in each ISA's `PT_INTERP` (verified with
/// `readelf -p .interp`). ld.so's `libc.so.6` is found via `LD_LIBRARY_PATH`.
#[cfg(target_arch = "x86_64")]
const INTERP_PATH: &str = "/lib64/ld-linux-x86-64.so.2";
#[cfg(target_arch = "aarch64")]
const INTERP_PATH: &str = "/lib/ld-linux-aarch64.so.1";
#[cfg(target_arch = "riscv64")]
const INTERP_PATH: &str = "/lib/ld-linux-riscv64-lp64d.so.1";

static mut OBJECTS: ObjectTable = ObjectTable::new();
static mut CAPS: CapTable = CapTable::new();
static mut QP: core::mem::MaybeUninit<QueuePair> = core::mem::MaybeUninit::uninit();

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

/// Run `image` (a dynamically-linked binary) with `argv`/`envp` under a fresh
/// Linux cell, capturing stdout; the **initial-load** path (`load_elf_linux`, whole
/// image in a kernel buffer). Returns the outcome.
fn run(image: &[u8], argv: &[&[u8]], envp: &[&[u8]]) -> Outcome {
    // Reset **before** loading: `user::reset` clears the mapped-file registry the
    // loader registers the image in, so resetting afterwards leaves the cell's records
    // naming a released entry and the whole image faults in as zeros
    // (docs/ENGINEERING.md 11).
    user::reset();
    let mut aspace = AddressSpace::new(1);
    let img = load::load_elf_linux(image, &mut aspace).expect("load dynamic Linux ELF");
    run_image(img, aspace, argv, envp)
}

/// Run a dynamically-linked binary via the **streaming `execve`** path
/// (`exec_elf_from_vfs_demand`): the program and its `PT_INTERP` interpreter are
/// both streamed from the VFS and demand-paged, exactly as a shell `execve`ing a
/// dynamic program does. Proves the streaming loader handles `PT_INTERP`
/// (docs/LINUX-COMPAT.md L7, docs/ARCHITECTURE-DEBT.md 4.0 blocker 2).
fn run_execve(path: &str, argv: &[&[u8]], envp: &[&[u8]]) -> Outcome {
    user::reset();
    let mut aspace = AddressSpace::new(1);
    let ops = svc::file_ops().expect("file ops registered");
    // `path` is a `'static` &str (kernel .rodata), which is where
    // `exec_elf_from_vfs_demand` requires the path to live.
    let img =
        load::exec_elf_from_vfs_demand(ops, path.as_ptr() as u64, path.len() as u64, &mut aspace)
            .expect("streaming execve of a dynamic Linux ELF");
    run_image(img, aspace, argv, envp)
}

/// Shared tail: assert both files are demand-paged, lay out the stack, install the
/// Linux cell and run it.
fn run_image(
    img: load::LinuxImage,
    mut aspace: AddressSpace,
    argv: &[&[u8]],
    envp: &[&[u8]],
) -> Outcome {
    // Both files must be demand-paged, not just the program. A dynamically linked
    // binary is the program plus its **interpreter**, and the interpreter comes from
    // the VFS rather than from a kernel buffer - a different loader path, so it needs
    // its own evidence. The oracle is the address: `ld.so` is loaded at
    // `LINUX_INTERP_BASE`, which nothing else occupies.
    let interp_segs = img
        .recorded()
        .iter()
        .filter(|s| s.base >= load::LINUX_INTERP_BASE)
        .count();
    let prog_segs = img.nsegs - interp_segs;
    assert!(
        prog_segs > 0 && interp_segs > 0,
        "linuxdyn: {prog_segs} program + {interp_segs} interpreter segment(s) recorded \
         - both files must be demand-paged"
    );
    println!(
        "linuxdyn: {prog_segs} program + {interp_segs} ld.so segment(s) left to demand \
         paging ({} image pages recorded, {} copied since boot)",
        load::recorded_pages(),
        load::eager_pages()
    );
    let sp = linux_stack::setup_stack(&mut aspace, &img, argv, envp);
    // SAFETY: single-threaded init; the statics outlive the synchronous run.
    unsafe {
        let kernel_sp = core::ptr::addr_of!(KSTACK.0) as usize + 64 * 1024;
        let mut frame = arch::trapframe_new(img.entry, sp, 0, kernel_sp);
        let objects = &mut *addr_of_mut!(OBJECTS);
        let caps = &mut *addr_of_mut!(CAPS);
        let qp = core::ptr::addr_of!(QP) as *const QueuePair;
        user::install(0, &aspace, caps, objects, qp, addr_of_mut!(frame));
        user::set_personality(0, Personality::Linux);
        linux::install_cell(0, &img);
        user::run(0).1
    }
}

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("linuxdyn: start on {}", arch::NAME);

    // SAFETY: once, before any allocation.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 8 * 1024 * 1024);
    }

    // Skip-with-reason if the runtime ld.so could not be located at build time
    // (a 1-byte placeholder) - the static L2-L6 coverage stays the floor.
    if LD_SO.len() < 4096 {
        println!(
            "linuxdyn: SKIP on {} - toolchain ld.so/libc not available at build \
             time (static Linux coverage unaffected)",
            arch::NAME
        );
        println!("linuxdyn: PASS (skipped)");
        arch::exit(arch::ExitCode::Success);
    }

    // A ramfs at / holding the dynamic linker at its PT_INTERP path and
    // libc.so.6 under /lib, plus the VFS personality handler.
    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    sys::mkdir("/lib").expect("mkdir /lib");
    // x86-64's interp path is /lib64/...; create that dir too.
    if INTERP_PATH.starts_with("/lib64/") {
        sys::mkdir("/lib64").expect("mkdir /lib64");
    }
    fs::write(INTERP_PATH, LD_SO).expect("seed ld.so");
    fs::write("/lib/libc.so.6", LIBC).expect("seed libc.so.6");
    // The dynamic program on the VFS at /bin/dhello, so it can be `execve`d
    // (streamed) as well as loaded directly.
    sys::mkdir("/bin").expect("mkdir /bin");
    fs::write("/bin/dhello", DHELLO).expect("seed /bin/dhello");
    svc::set_file_ops(vfs_personality::ops());

    println!(
        "linuxdyn: dhello={} ld.so={} libc={} bytes; interp={}",
        DHELLO.len(),
        LD_SO.len(),
        LIBC.len(),
        INTERP_PATH,
    );

    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    // LD_LIBRARY_PATH=/lib so ld.so resolves DT_NEEDED libc.so.6 from the ramfs.
    let outcome = run(
        DHELLO,
        &[b"dhello"],
        &[b"LD_LIBRARY_PATH=/lib", b"PATH=/bin"],
    );
    linux::set_stdout_tap(None);

    let want_out = b"hello from dynamic glibc\n";
    match outcome {
        Outcome::Exited(code) => {
            let got = captured();
            assert!(
                got == want_out,
                "dhello: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_out),
            );
            assert!(code == 12, "dhello: exit {code}, expected 12");
        }
        Outcome::Faulted(addr) => panic!("dhello: faulted at {addr:#x}"),
    }
    println!("linuxdyn: dhello OK (PT_INTERP + ld.so + fd-backed mmap)");

    // Multi-library phase (GOAL-DYN-MULTILIB): `dmath` links libm **and** libc, so
    // ld.so must load two shared libraries and resolve one's version references
    // against the other. This exercises what a single-library `dhello` cannot:
    // ld.so dedups shared objects by `(st_dev, st_ino)`, so if the VFS reports the
    // same inode for two different files, ld.so treats the second as already-loaded
    // and never maps it - which broke every multi-library binary until the stat
    // block carried a real inode (docs/LINUX-COMPAT.md, docs/ENGINEERING.md 11).
    multilib_phase();

    // Four-library phase: a dynamic C++ hello links libstdc++ + libgcc_s + libc
    // (+ libm), and runs C++ runtime init (static constructors, iostream setup,
    // exception-unwind tables) - the production shape a real application has, well
    // beyond `dmath`'s two libraries.
    cpp_phase();

    // Phase 2: the same dynamic binary via the **streaming `execve`** path -
    // program + interpreter both streamed from the VFS and demand-paged, the way a
    // shell launching a dynamic program does. Proves the streaming loader handles
    // PT_INTERP (docs/ARCHITECTURE-DEBT.md 4.0 blocker 2, task GOAL-DISK-2).
    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run_execve(
        "/bin/dhello",
        &[b"dhello"],
        &[b"LD_LIBRARY_PATH=/lib", b"PATH=/bin"],
    );
    linux::set_stdout_tap(None);
    match outcome {
        Outcome::Exited(code) => {
            let got = captured();
            assert!(
                got == want_out,
                "dhello (execve): stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_out),
            );
            assert!(code == 12, "dhello (execve): exit {code}, expected 12");
        }
        Outcome::Faulted(addr) => panic!("dhello (execve): faulted at {addr:#x}"),
    }
    println!("linuxdyn: dhello via streaming execve OK (PT_INTERP streamed from the VFS)");

    // Phase 3 (GOAL-DISK-2b): the same dynamic binary `execve`d from a real ext4
    // image on a **live virtio-blk disk**, mounted through ext4fs/ext4plus + the
    // bounded block cache. This composes the two proven capabilities - the
    // streaming PT_INTERP loader (phase 2) and block-cached ext4 (#167) - end to
    // end: the program, its interpreter and libc all stream off the disk on
    // demand, none resident whole. If no ext4 disk is attached (a placeholder
    // image: no e2fsprogs or toolchain libs at build time), skip-with-reason.
    disk_phase(want_out);

    println!("linuxdyn: PASS");
    arch::exit(arch::ExitCode::Success)
}

/// The kernel cache-line bridge to `posix::BlockSource` (the orphan-rule newtype,
/// as in `blockfs`).
struct Cached(BlockCache<VirtioBlk>);
impl BlockSource for Cached {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), Errno> {
        self.0.read_at(off, buf).map_err(|_| Errno::Io)
    }
}

/// Run `dmath` (linked against libm as well as libc) and assert its exact output
/// and exit code - the multi-library dynamic-linking proof. Skips-with-reason if
/// the toolchain libm or dmath was unavailable at build time (placeholder).
fn multilib_phase() {
    if LIBM.len() < 4096 || DMATH.len() < 4096 {
        println!(
            "linuxdyn: SKIP multi-library phase on {} - libm.so.6/dmath not \
             available at build time (single-library coverage unaffected)",
            arch::NAME
        );
        return;
    }
    fs::write("/lib/libm.so.6", LIBM).expect("seed libm.so.6");
    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run(DMATH, &[b"dmath"], &[b"LD_LIBRARY_PATH=/lib", b"PATH=/bin"]);
    linux::set_stdout_tap(None);
    let want = b"dmath: sqrt16=4\n";
    match outcome {
        Outcome::Exited(code) => {
            let got = captured();
            assert!(
                got == want,
                "dmath: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want),
            );
            assert!(code == 4, "dmath: exit {code}, expected 4 (sqrt(16))");
        }
        Outcome::Faulted(addr) => panic!("dmath: faulted at {addr:#x}"),
    }
    println!(
        "linuxdyn: dmath OK (multi-library: ld.so loaded libc AND libm, distinct \
         inodes, cross-object version resolution)"
    );
}

/// Run `dcpp` (a dynamic C++ hello linking libstdc++ + libgcc_s + libc + libm)
/// and assert its exact output and exit code - the four-library, C++-runtime
/// proof. Skips-with-reason if the toolchain g++/C++ runtime libs were
/// unavailable at build time (e.g. no cross-g++ for this ISA).
fn cpp_phase() {
    if DCPP.len() < 4096 || LIBSTDCPP.len() < 4096 || LIBGCC.len() < 4096 {
        println!(
            "linuxdyn: SKIP C++ phase on {} - g++/libstdc++/libgcc_s not available \
             at build time (single- and multi-library coverage unaffected)",
            arch::NAME
        );
        return;
    }
    fs::write("/lib/libm.so.6", LIBM).ok();
    fs::write("/lib/libstdc++.so.6", LIBSTDCPP).expect("seed libstdc++.so.6");
    fs::write("/lib/libgcc_s.so.1", LIBGCC).expect("seed libgcc_s.so.1");
    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run(DCPP, &[b"dcpp"], &[b"LD_LIBRARY_PATH=/lib", b"PATH=/bin"]);
    linux::set_stdout_tap(None);
    let want = b"dcpp: hello from dynamic C++ (23)\n";
    match outcome {
        Outcome::Exited(code) => {
            let got = captured();
            assert!(
                got == want,
                "dcpp: stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want),
            );
            assert!(code == 23, "dcpp: exit {code}, expected 23");
        }
        Outcome::Faulted(addr) => panic!("dcpp: faulted at {addr:#x}"),
    }
    println!(
        "linuxdyn: dcpp OK (four-library: libstdc++ + libgcc_s + libc + libm, \
         C++ runtime init)"
    );
}

fn disk_phase(want_out: &[u8]) {
    let dev = match virtio_blk::probe() {
        Some(d) => d,
        None => {
            println!(
                "linuxdyn: SKIP disk phase on {} - no virtio-blk disk attached",
                arch::NAME
            );
            return;
        }
    };
    let cache = BlockCache::new(dev);
    let disk = match Ext4Fs::new(Box::new(Cached(cache))) {
        Ok(fs) => fs,
        Err(_) => {
            println!(
                "linuxdyn: SKIP disk phase on {} - the virtio-blk disk holds no ext4 \
                 image (placeholder; no e2fsprogs/toolchain at build time)",
                arch::NAME
            );
            return;
        }
    };

    // Mount the live ext4 disk as the cell's `/`, then stream-`execve` from it.
    posix::reset();
    mount::mount("/", Rc::new(disk));
    let fills_before = block::cache_fills();
    unsafe {
        STDOUT_LEN = 0;
    }
    linux::set_stdout_tap(Some(tap));
    let outcome = run_execve(
        "/bin/dhello",
        &[b"dhello"],
        &[b"LD_LIBRARY_PATH=/lib", b"PATH=/bin"],
    );
    linux::set_stdout_tap(None);
    match outcome {
        Outcome::Exited(code) => {
            let got = captured();
            assert!(
                got == want_out,
                "dhello (disk): stdout mismatch\n  got:      {:?}\n  expected: {:?}",
                core::str::from_utf8(got),
                core::str::from_utf8(want_out),
            );
            assert!(code == 12, "dhello (disk): exit {code}, expected 12");
        }
        Outcome::Faulted(addr) => panic!("dhello (disk): faulted at {addr:#x}"),
    }
    // Streaming witness: the program + ld.so + libc came off the device on
    // demand through the bounded cache, not a whole-image preload.
    let fills = block::cache_fills() - fills_before;
    assert!(
        fills > 0,
        "no device reads - the binary was not streamed off disk"
    );
    println!(
        "linuxdyn: dhello via streaming execve OFF A LIVE ext4 DISK OK \
         ({fills} block-cache fills through ext4plus)"
    );
}
