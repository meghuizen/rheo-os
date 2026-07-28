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
    // The completion wait halts only when the timer arbiter has a hardware one-shot
    // to fall back on - a halt whose sole wake source is the device's own interrupt
    // cannot end when the device stops (see `hw/nvme.rs`). Bringing the timer up is
    // a per-kernel opt-in here as elsewhere (`enable_uart_rx_irq`,
    // `enable_virtio_net_irq`), and this is the kernel that asserts the wait parks,
    // so this is the kernel that provides the backstop.
    arch::enable_timer_irq();

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

    // --- queue depth --------------------------------------------------------
    //
    // The property that separates NVMe from a paravirtual block transport: more
    // than one command outstanding at a time, one doorbell for the batch, and
    // completions matched by command id rather than assumed to arrive in order.
    //
    // Asserted with a read big enough to fill a batch. The bytes matter as much as
    // the count: a batch that reaped the wrong completion for a slot would still
    // report the right *number* of commands, and only the contents catch it - so
    // this reads a span whose every page differs and checks it against the same
    // span read one page at a time.
    let depth_before = nvme::max_inflight();
    let mut batched = [0u8; 8 * 4096];
    dev2.read(16, &mut batched).expect("batched read");
    let after = nvme::max_inflight();
    assert!(
        after >= 8,
        "nvmefs: max {after} command(s) outstanding - the driver is still one-at-a-time"
    );

    // The same span, one page per call, so each read is its own single-command
    // batch. Byte-identical or the batch matched a completion to the wrong slot.
    for p in 0..8 {
        let mut one = [0u8; 4096];
        dev2.read(16 + (p as u64) * 8, &mut one).expect("page read");
        let lo = p * 4096;
        assert!(
            one == batched[lo..lo + 4096],
            "nvmefs: page {p} of the batch differs from the same page read alone"
        );
    }
    println!(
        "nvmefs: queue depth OK - {} command(s) outstanding at once (was {depth_before} before), \
         one doorbell per batch, and every page of a {}-page batch matches the same page \
         read singly; {} completion(s) arrived out of submission order",
        after,
        batched.len() / 4096,
        nvme::out_of_order()
    );

    // --- how the wait ends ---------------------------------------------------
    //
    // Every other wait in this kernel parks rather than spinning
    // (docs/ARCHITECTURE-DEBT.md 2.4); this one could not, because a polled
    // completion has no wake source. With MSI-X programmed there is one.
    //
    // Reported per ISA rather than asserted uniformly, because only x86-64 has an
    // MSI target today: ARM64 needs a GICv3 ITS and RISC-V an IMSIC target, both
    // real drivers. What *is* asserted is that the two never disagree - a driver
    // claiming an interrupt path must have taken interrupts, and one that halted
    // must have had somewhere to be woken from.
    let irqs = nvme::irq_count();
    let parks = nvme::irq_parks();
    if parks > 0 {
        assert!(
            irqs > 0,
            "nvmefs: {parks} halt(s) waiting for a completion but 0 interrupts taken - \
             the wait had no wake source and only the deadline ended it"
        );
        println!(
            "nvmefs: completions are INTERRUPT-DRIVEN - {irqs} MSI-X interrupt(s) taken, \
             {parks} halt(s) instead of spinning"
        );
    } else {
        assert!(
            irqs == 0,
            "nvmefs: {irqs} interrupt(s) taken but the wait never halted"
        );
        println!(
            "nvmefs: completions are POLLED on this ISA (no MSI target - ARM64 needs a \
             GICv3 ITS, RISC-V an IMSIC target); the wait spins and says so"
        );
    }

    println!("nvmefs: PASS");
    arch::exit(arch::ExitCode::Success)
}
