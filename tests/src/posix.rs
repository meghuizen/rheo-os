//! In-QEMU test kernel for the POSIX + filesystem stack (docs/FILESYSTEMS.md,
//! POSIX-PERSONALITY.md): a read-write ramfs mounted at `/`, a read-only ext4
//! image mounted at `/mnt`, exercised through the `std::fs`-shaped facade and
//! the POSIX errno surface. Proves the translation layer end to end on all
//! three ISAs (kernel-context; the FS-server-cell-over-queues wiring is the
//! follow-on, per the runtime milestone).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::addr_of_mut;

use ext4fs::Ext4Fs;
use kernel::{arch, println};
use posix::sys::Whence;
use posix::vfs::FileType;
use posix::{Errno, RamFs, fs, mount};

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 1024 * 1024] = [0; 1024 * 1024];

// The read-only ext4 image, embedded (no block driver yet). Regenerate with
// tests/fixtures/gen-ext4.sh.
static EXT4_IMG: &[u8] = include_bytes!("../fixtures/ext4.img");

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("posix: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 1024 * 1024);
    }

    posix::reset();
    mount::mount("/", Rc::new(RamFs::new()));
    mount::mount(
        "/mnt",
        Rc::new(Ext4Fs::new(Box::new(EXT4_IMG)).expect("mount ext4 image")),
    );

    test_ramfs_rw();
    test_dirs();
    test_seek_and_append();
    test_errno();
    test_ext4_ro();

    println!("posix: PASS");
    arch::exit(arch::ExitCode::Success)
}

fn names(entries: &[posix::DirEntry]) -> Vec<String> {
    entries.iter().map(|e| e.name.clone()).collect()
}

/// Write then read a file through the std::fs facade on the ramfs.
fn test_ramfs_rw() {
    fs::write("/log.txt", b"hello ramfs").expect("write");
    let s = fs::read_to_string("/log.txt").expect("read");
    assert!(s == "hello ramfs", "ramfs read back wrong: {s:?}");
    let md = fs::metadata("/log.txt").expect("stat");
    assert!(md.kind == FileType::Regular && md.len == 11, "bad metadata");
    println!("posix: ramfs write/read/stat OK");
}

/// Directories: create, populate, list (with `.`/`..` filtered).
fn test_dirs() {
    fs::create_dir("/etc").expect("mkdir");
    fs::write("/etc/motd", b"welcome").expect("write in dir");
    let root = names(&fs::read_dir("/").expect("read_dir /"));
    assert!(root.iter().any(|n| n == "log.txt"), "log.txt missing");
    assert!(root.iter().any(|n| n == "etc"), "etc missing");
    let etc = names(&fs::read_dir("/etc").expect("read_dir /etc"));
    assert!(
        etc.len() == 1 && etc[0] == "motd",
        "etc contents wrong: {etc:?}"
    );
    println!("posix: create_dir + read_dir OK");
}

/// File handle with seek, and O_APPEND semantics.
fn test_seek_and_append() {
    let f = fs::File::open("/log.txt").expect("open");
    f.seek(6, Whence::Set).expect("seek");
    let mut buf = [0u8; 5];
    let n = f.read(&mut buf).expect("read");
    assert!(&buf[..n] == b"ramfs", "seek+read wrong: {:?}", &buf[..n]);

    let a = fs::OpenOptions::new()
        .write(true)
        .append(true)
        .open("/log.txt")
        .expect("open append");
    a.write_all(b"!!!").expect("append");
    let s = fs::read_to_string("/log.txt").expect("reread");
    assert!(s == "hello ramfs!!!", "append wrong: {s:?}");
    println!("posix: File seek + O_APPEND OK");
}

/// The POSIX error surface: missing files, read-only mounts, duplicates.
fn test_errno() {
    assert!(
        fs::read("/nope").err() == Some(Errno::NoEnt),
        "expected ENOENT"
    );
    assert!(
        fs::remove_file("/nope").err() == Some(Errno::NoEnt),
        "expected ENOENT on unlink"
    );
    assert!(
        fs::create_dir("/etc").err() == Some(Errno::Exists),
        "expected EEXIST"
    );
    // The ext4 mount is read-only: creating/writing must fail with EROFS.
    assert!(
        fs::write("/mnt/nope.txt", b"x").err() == Some(Errno::Rofs),
        "expected EROFS writing ext4"
    );
    // Remove works on ramfs.
    fs::write("/tmpf", b"z").unwrap();
    fs::remove_file("/tmpf").expect("unlink");
    assert!(
        fs::read("/tmpf").err() == Some(Errno::NoEnt),
        "file not removed"
    );
    println!("posix: errno (ENOENT/EEXIST/EROFS/unlink) OK");
}

/// Read real files from the ext4 image, including a multi-block file.
fn test_ext4_ro() {
    let hello = fs::read_to_string("/mnt/hello.txt").expect("ext4 hello");
    assert!(hello == "hello from ext4\n", "ext4 hello wrong: {hello:?}");

    let fox = fs::read_to_string("/mnt/docs/fox.txt").expect("ext4 fox");
    assert!(
        fox == "The quick brown fox jumps over the lazy dog.\n",
        "ext4 fox wrong"
    );

    // Multi-block file (7800 bytes over 1 KiB blocks): checks extent-mapped
    // reads across block boundaries.
    let big = fs::read("/mnt/docs/big.txt").expect("ext4 big");
    assert!(big.len() == 7800, "ext4 big.txt size {}", big.len());
    assert!(
        big.starts_with(b"line 000: the lattice filesystem works\n"),
        "big start"
    );
    let line5 = b"line 005: the lattice filesystem works\n";
    assert!(&big[5 * 39..5 * 39 + 39] == line5, "big line 5 wrong");
    assert!(
        &big[199 * 39..199 * 39 + 39] == b"line 199: the lattice filesystem works\n",
        "big last line wrong"
    );

    let mnt = names(&fs::read_dir("/mnt").expect("read_dir /mnt"));
    assert!(
        mnt.iter().any(|n| n == "hello.txt"),
        "ext4 hello.txt missing"
    );
    assert!(mnt.iter().any(|n| n == "docs"), "ext4 docs missing");
    assert!(
        !mnt.iter().any(|n| n == "." || n == ".."),
        "dot entries leaked"
    );

    let docs = names(&fs::read_dir("/mnt/docs").expect("read_dir docs"));
    assert!(
        docs.iter().any(|n| n == "fox.txt") && docs.iter().any(|n| n == "big.txt"),
        "docs wrong"
    );

    let md = fs::metadata("/mnt/docs").expect("stat dir");
    assert!(md.kind == FileType::Dir, "docs not a dir");
    println!("posix: ext4 read-only (files, subdir, multi-block, readdir) OK");
}
