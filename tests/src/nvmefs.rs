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

// The shared `.user`-window cell builder, for the D2 leg-1 BAR-grant phase at the end.
#[path = "harness.rs"]
mod harness;

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

    // --- D2 leg 1: a **cell** reads a real device register through a granted BAR
    // window (docs/DRIVERS.md 4.1).
    bar_grant_phase();

    println!("nvmefs: PASS");
    arch::exit(arch::ExitCode::Success)
}

/// Kernel state for the one BAR-reader cell this phase runs.
/// In `.user.bss`: the cell reads its own parameter block at the unprivileged level, so
/// it must live in the window a cell's page tables map (see the module docs of
/// `kernel/src/user_progs.rs`).
#[unsafe(link_section = ".user.bss")]
static mut BAR_STORE: harness::CellStore = harness::CellStore::new();
static mut BAR_KSTACK: harness::KernelStack = harness::KernelStack::new();
static mut BAR_OBJECTS: kernel::capability::ObjectTable = kernel::capability::ObjectTable::new();
static mut BAR_CAPS: kernel::capability::CapTable = kernel::capability::CapTable::new();

/// Build a cell with `bar` mapped in as device memory, point it at `offset` bytes into
/// that window, run it, and return `(outcome, the value it read, its status word, the
/// window's byte length)`.
///
/// `offset` is a parameter so the same cell can be aimed **past** the window's end, which
/// is the phase's own negative control.
///
/// # Safety
/// Single-threaded setup on the primary with no cell live.
unsafe fn run_bar_cell(
    bar: &kernel::hw::PciBar,
    offset: usize,
) -> (kernel::user::Outcome, u64, u64, usize) {
    // SAFETY: the caller's contract; the statics outlive the synchronous run.
    unsafe {
        let store = core::ptr::addr_of_mut!(BAR_STORE);
        let objects = &mut *core::ptr::addr_of_mut!(BAR_OBJECTS);
        let caps = &mut *core::ptr::addr_of_mut!(BAR_CAPS);
        *objects = kernel::capability::ObjectTable::new();
        *caps = kernel::capability::CapTable::new();
        let kernel_sp = (*core::ptr::addr_of_mut!(BAR_KSTACK)).top();
        let (mut aspace, _obj, mut frame) = harness::build_cell(
            &mut *store,
            objects,
            caps,
            kernel_sp,
            9,
            kernel::user_progs::user_bar_read,
            0x42,
            0,
        );
        // **The grant.** Mapped by the launcher, which is the whole point: the cell asks
        // for nothing and can ask for nothing - `SYS_GRANT(DeviceBar)` is still refused.
        let (va, len) = kernel::load::map_device_bar(&mut aspace, bar)
            .expect("BAR0 is memory, assigned, and within the window");
        assert!(len >= 0x0C, "the window must cover VS at offset 0x08");
        (*store).params.iters = (va + offset) as u64;
        (*store).params.ops = 0;
        (*store).params.status = 0;
        kernel::user::reset();
        kernel::user::install(
            0,
            &aspace,
            caps,
            objects,
            (*store).qp.qp.as_ptr(),
            core::ptr::addr_of_mut!(frame),
        );
        let out = kernel::user::run(0).1;
        (out, (*store).params.ops, (*store).params.status, len)
    }
}

/// **A `BarWindow` grant, and a cell using it** (docs/DRIVERS.md 4.1 - the first leg of
/// D2's device capability trio).
///
/// `MemKind::DeviceBar` was refused at the syscall boundary, so no cell could reach a
/// device's registers at all. That refusal **stays**: a cell cannot mint device authority
/// for itself, exactly as it cannot mint the W^X exception or the cell-spawn capability.
/// What is new is that the *launcher* can map a BAR into a cell as uncached device memory
/// (`load::map_device_bar`, `MapPerm::UserDevice`), which is what owning a device starts
/// with.
///
/// The proof is a value the cell cannot fabricate: the NVMe controller's **VS** (version)
/// register, read by the cell at the unprivileged level through its granted window, and
/// compared against the same register read by the kernel through its own mapping. A cell
/// that guessed would have to guess QEMU's exact NVMe revision.
fn bar_grant_phase() {
    use kernel::hw::{EngineKind, PciDevice};
    use kernel::user::Outcome;

    let inv = kernel::hw::inventory();
    let Some(dev) = inv
        .pci
        .iter()
        .take(inv.npci)
        .find(|d: &&PciDevice| d.engine == EngineKind::Nvme)
    else {
        println!("nvmefs: SKIP the BAR-grant phase - no NVMe function enumerated");
        return;
    };
    let bar0 = dev.bars[0];
    if bar0.base == 0 || bar0.size == 0 {
        println!("nvmefs: SKIP the BAR-grant phase - BAR0 unassigned");
        return;
    }

    // What the register actually holds, read by the kernel through its own mapping. The
    // oracle, and it is not a constant: it is whatever this QEMU's controller reports.
    let kregs = arch::mmio_map_window(bar0.base as usize, bar0.size as usize);
    assert!(kregs != 0, "nvmefs: could not map BAR0 for the oracle read");
    // SAFETY: `kregs` is a kernel MMIO mapping of BAR0, and VS is at offset 0x08 of
    // every NVMe register file (NVMe 1.4 3.1).
    let want = unsafe { core::ptr::read_volatile((kregs + 0x08) as *const u32) };
    assert!(
        want != 0 && want != u32::MAX,
        "nvmefs: VS reads {want:#x} through the kernel mapping - not a live register"
    );

    // Now the same read from a cell, through a window the launcher granted it.
    // VS is at offset 0x08 of every NVMe register file (NVMe 1.4 3.1).
    // SAFETY: single-threaded setup; no cell is live.
    let (code, got, status, len) = unsafe { run_bar_cell(&bar0, 0x08) };
    assert!(
        matches!(code, Outcome::Exited(0x42)),
        "nvmefs: the BAR-reader cell exited {code:?}"
    );
    assert!(
        status == 1,
        "nvmefs: the BAR-reader cell did not report done"
    );
    assert_eq!(
        got, want as u64,
        "nvmefs: the cell read {got:#x} from VS through its granted BAR window; the \
         kernel reads {want:#x} through its own mapping"
    );
    println!(
        "nvmefs: BAR-WINDOW GRANT OK - an unprivileged cell mapped {} bytes of the NVMe \
         controller's BAR0 as uncached device memory and read VS = {want:#x} out of it, \
         matching the kernel's own read of the same register. `SYS_GRANT(DeviceBar)` \
         stays refused: the window is minted by the launcher, so a cell cannot give \
         itself a device (docs/DRIVERS.md 4.1)",
        bar0.size
    );

    // **The control, and it is not decoration.** A device mapping that ran past the BAR
    // would hand the cell whatever the next physical page happens to be - which for a
    // device is another device's registers. So aim the same cell one page past the window
    // and require it to *fault*: that is what says the grant is bounded, and it is also
    // what says the read above was a genuine memory access through the page tables rather
    // than a value the cell could have produced without a mapping.
    // SAFETY: single-threaded setup; no cell is live.
    let (code, _got, status, _len) = unsafe { run_bar_cell(&bar0, len) };
    assert!(
        matches!(code, Outcome::Faulted(_)),
        "nvmefs: reading one page past the granted BAR window ended {code:?}, not a fault \
         - the window is unbounded"
    );
    assert!(
        status == 0,
        "nvmefs: the cell reported done after a read it should not have completed"
    );
    println!(
        "nvmefs: and the window is BOUNDED - the same cell aimed at +{len:#x} (the first \
         byte past the {len}-byte mapping) faults instead of reading the next device's \
         registers"
    );
}
