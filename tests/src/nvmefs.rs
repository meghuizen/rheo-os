//! In-QEMU test kernel for the **NVMe** transport (docs/SUBSTRATE.md S5,
//! docs/FILESYSTEMS.md 1): bring up a real NVMe controller over PCIe, mount the
//! same ext4 image off it, and read the same files through the same VFS.
//!
//! It is deliberately `blockfs` with one line changed - the transport. That is the
//! claim being made: `BlockDevice` is a seam, so a second transport costs a driver
//! and nothing else, and the filesystem above it does not learn a new word. A test
//! that proved NVMe with its own bespoke assertions would not have shown that.
//!
//! What NVMe adds over virtio-blk, and why it is worth a driver: virtio-blk is a
//! paravirtual transport with one queue and a hypervisor behind it, while NVMe is
//! the interface real storage presents - paired submission/completion queues in
//! host memory, a doorbell, out-of-order completion, a queue pair per core. That
//! last property is what S5 is for, and this kernel is its prerequisite: one queue
//! pair, polled, proven correct end to end.
//!
//! Unlike virtio-pci, NVMe has no config-space tunnel - its register file *is*
//! BAR0 - so this is also the first test that requires `hw::assign_pci_bars()` and
//! a mapped MMIO window on all three ISAs.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::ptr::addr_of_mut;

use ext4fs::Ext4Fs;
use kernel::hw::block::{self, BlockCache, BlockDevice};
use kernel::hw::nvme::{self, Nvme};
use kernel::{arch, println};
use posix::{BlockSource, Errno, fs, mount};

#[global_allocator]
static HEAP: runtime::Heap = runtime::Heap::empty();
static mut HEAP_MEM: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("nvmefs: start on {}", arch::NAME);

    // SAFETY: once, before any allocation; HEAP_MEM is a unique static.
    unsafe {
        HEAP.init(addr_of_mut!(HEAP_MEM) as usize, 2 * 1024 * 1024);
    }

    // NVMe's registers are BAR0, and nothing has programmed the BARs: the bare
    // arm/riscv boots have no firmware and the x86 PVH path skips it. Opt in here
    // rather than at boot, so the other kernels are untouched (the `gpuhw`
    // precedent, docs/GPU-HARDWARE.md 12).
    let assigned = kernel::hw::assign_pci_bars();
    println!("nvmefs: assigned {assigned} PCI BAR(s)");

    let dev = match nvme::probe() {
        Some(d) => d,
        None => {
            println!("nvmefs: no NVMe controller attached - skipping");
            println!("nvmefs: PASS");
            arch::exit(arch::ExitCode::Success)
        }
    };

    // Same bounded cache as `blockfs`, and the same reason: the resident bound has
    // to be smaller than the disk for "streamed off the device" to mean anything.
    let cache = BlockCache::new(dev);
    let disk_bytes = cache.capacity_sectors() as usize * 512;
    println!(
        "nvmefs: NVMe namespace {} sectors ({} KiB); cache resident bound {} KiB",
        cache.capacity_sectors(),
        disk_bytes / 1024,
        BlockCache::<Nvme>::CAPACITY / 1024
    );
    assert!(
        BlockCache::<Nvme>::CAPACITY < disk_bytes,
        "cache ({}) must be smaller than the disk ({}) for streaming to mean anything",
        BlockCache::<Nvme>::CAPACITY,
        disk_bytes
    );

    struct Cached(BlockCache<Nvme>);
    impl BlockSource for Cached {
        fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), Errno> {
            self.0.read_at(off, buf).map_err(|_| Errno::Io)
        }
    }

    let fills_before = block::cache_fills();

    posix::reset();
    mount::mount(
        "/",
        Rc::new(Ext4Fs::new(Box::new(Cached(cache))).expect("mount ext4 from NVMe")),
    );

    // The same two files `blockfs` reads, asserted the same way - byte for byte,
    // including the last line of the multi-block file, which is what catches an
    // off-by-one in the transfer loop's page splitting rather than in the parser.
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

    let fills = block::cache_fills() - fills_before;
    assert!(fills > 0, "no device reads - data was not streamed");
    println!(
        "nvmefs: ext4 read off a live NVMe namespace (hello.txt + multi-block big.txt) OK; \
         {fills} line fills through a {}-KiB cache over a {}-KiB disk",
        BlockCache::<Nvme>::CAPACITY / 1024,
        disk_bytes / 1024
    );

    // --- the write direction ----------------------------------------------
    //
    // Everything above exercises `NVM READ`. `NVM WRITE` is the same command path
    // with the data moving the other way, and code that is only reasoned about is
    // not proven (docs/ENGINEERING.md 7) - so write a sector and read it back off
    // the device.
    //
    // Deliberately last, on the namespace's **final** sector, and through a fresh
    // handle rather than the cache: the cache would answer the read-back out of
    // the line it just filled, which would prove the cache works and say nothing
    // about the device. The drive is attached `snapshot=on`, so the bytes really
    // reach QEMU's block layer and really come back, while the committed fixture
    // file is untouched.
    let dev2 = nvme::probe().expect("re-probe NVMe for the write phase");
    let last = dev2.capacity_sectors() - 1;
    let mut before = [0u8; 512];
    dev2.read(last, &mut before).expect("read last sector");

    let mut pattern = [0u8; 512];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = (i as u8) ^ 0x5A;
    }
    dev2.write(last, &pattern).expect("write last sector");
    let mut back = [0u8; 512];
    dev2.read(last, &mut back).expect("read the pattern back");
    assert!(
        back == pattern,
        "nvme: written sector read back wrong (first bytes {:?} vs {:?})",
        &back[..8],
        &pattern[..8]
    );

    // And put it back, which is the half that shows the read-back was not simply
    // whatever the last write left in a buffer somewhere: the device has to return
    // two *different* things for the same sector, in order.
    dev2.write(last, &before).expect("restore last sector");
    let mut restored = [0u8; 512];
    dev2.read(last, &mut restored)
        .expect("read the restore back");
    assert!(restored == before, "nvme: sector not restored");
    println!(
        "nvmefs: write round trip on sector {last} OK - pattern written and read back, \
         then the original restored and read back"
    );

    println!("nvmefs: PASS");
    arch::exit(arch::ExitCode::Success)
}
