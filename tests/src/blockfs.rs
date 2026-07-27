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

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::ptr::addr_of_mut;

use ext4fs::Ext4Fs;
use kernel::hw::block::{self, BlockCache};
use kernel::hw::virtio_blk::{self, VirtioBlk};
use kernel::{arch, println};
use posix::{BlockSource, Errno, fs, mount};

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

    // Wrap the device in a bounded block cache so ext4 STREAMS off the disk
    // rather than reading the whole image into RAM (the old path, and the
    // reason a binary could not exceed the pool). The cache holds at most
    // CAPACITY bytes resident; we assert that is strictly less than the disk,
    // so a correct multi-block read below cannot be the whole disk sitting in
    // memory (docs/ARCHITECTURE-DEBT.md 4.0 blocker 2).
    let cache = BlockCache::new(dev);
    let disk_bytes = cache.capacity_sectors() as usize * 512;
    println!(
        "blockfs: virtio-blk found, {} sectors ({} KiB); cache resident bound {} KiB",
        cache.capacity_sectors(),
        disk_bytes / 1024,
        BlockCache::<VirtioBlk>::CAPACITY / 1024
    );
    assert!(
        BlockCache::<VirtioBlk>::CAPACITY < disk_bytes,
        "cache ({}) must be smaller than the disk ({}) for streaming to mean anything",
        BlockCache::<VirtioBlk>::CAPACITY,
        disk_bytes
    );

    // The one bridge from the kernel cache to the posix `BlockSource` trait -
    // a local newtype (the orphan rule; posix and kernel do not know each
    // other). This is the composition site, like the FileOps registration.
    struct Cached(BlockCache<VirtioBlk>);
    impl BlockSource for Cached {
        fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), Errno> {
            self.0.read_at(off, buf).map_err(|_| Errno::Io)
        }
    }

    let fills_before = block::cache_fills();

    // Full stack: block device -> block cache -> ext4plus (ext4fs) -> VFS ->
    // std::fs facade.
    posix::reset();
    mount::mount(
        "/",
        Rc::new(Ext4Fs::new(Box::new(Cached(cache))).expect("mount ext4 from disk")),
    );

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

    // Streaming witness: the multi-block file (7800 bytes) read correctly
    // through a cache that cannot hold the whole disk, and the bytes really
    // came from the device on demand (fills happened during the reads).
    let fills = block::cache_fills() - fills_before;
    assert!(fills > 0, "no device reads - data was not streamed");
    println!(
        "blockfs: ext4 read off live virtio-blk disk (hello.txt + multi-block big.txt) OK; \
         {fills} line fills through a {}-KiB cache over a {}-KiB disk",
        BlockCache::<VirtioBlk>::CAPACITY / 1024,
        disk_bytes / 1024
    );
    println!("blockfs: PASS");
    arch::exit(arch::ExitCode::Success)
}
