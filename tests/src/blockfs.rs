//! In-QEMU test kernel for the block-device path (docs/FILESYSTEMS.md 1):
//! discover a virtio-blk device over virtio-mmio, read a real ext4 image off
//! the *live disk* (attached by the harness with `-drive`), mount it, and read
//! files through the VFS + std::fs facade. This closes the loop from a storage
//! transport to a filesystem.
//!
//! The transport differs per machine: virtio-mmio on the riscv/arm `virt`
//! machines, virtio-pci on x86-64 q35 (driven through PCI config space). The
//! probe tries both, so all three ISAs exercise the same block path. The
//! skip branch below only fires if no virtio-blk device is attached at all.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use core::ptr::addr_of_mut;

use kernel::hw::block::BlockDevice;
use kernel::hw::virtio_blk;
use kernel::{arch, println};
use posix::{Ext4, fs, mount};

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("blockfs: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    let dev = match virtio_blk::probe() {
        Some(d) => d,
        None => {
            println!("blockfs: no virtio-blk device attached - skipping");
            println!("blockfs: PASS");
            arch::exit(arch::ExitCode::Success)
        }
    };

    let sectors = dev.capacity_sectors();
    println!(
        "blockfs: virtio-blk found, {} sectors ({} KiB)",
        sectors,
        sectors / 2
    );

    // Read the whole disk into RAM, then hand it to the ext4 parser. (A
    // production FS would stream through a block cache; reading a small test
    // image whole proves the transport + parser end to end.)
    let bytes = sectors as usize * 512;
    let mut disk = alloc::vec![0u8; bytes];
    dev.read(0, &mut disk).expect("read disk");
    let img: &'static [u8] = alloc::vec::Vec::leak(disk);

    // Full stack: block device -> ext4 -> VFS -> std::fs facade.
    posix::reset();
    mount::mount("/", Rc::new(Ext4::new(img).expect("parse ext4 from disk")));

    let hello = fs::read_to_string("/hello.txt").expect("read hello");
    assert!(hello == "hello from ext4\n", "hello wrong: {hello:?}");

    let big = fs::read("/docs/big.txt").expect("read big");
    assert!(big.len() == 7800, "big size {}", big.len());
    assert!(
        big.starts_with(b"line 000: the lattice filesystem works\n"),
        "big start"
    );
    assert!(
        &big[199 * 39..199 * 39 + 39] == b"line 199: the lattice filesystem works\n",
        "big last line"
    );

    println!("blockfs: ext4 read off live virtio-blk disk (hello.txt + multi-block big.txt) OK");
    println!("blockfs: PASS");
    arch::exit(arch::ExitCode::Success)
}
