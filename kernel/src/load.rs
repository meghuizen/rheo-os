//! Loading a userland ELF into a cell's address space (docs/USERLAND.md M1).
//! For each `PT_LOAD` segment the loader allocates zeroed frames, copies the
//! file bytes in (kernel RAM is identity-mapped, so a frame's physical
//! address is directly writable during load), and maps each page at the
//! segment's virtual address with its W^X permission. It then maps a stack.
//!
//! Unlike the fixed 2 MiB `.user` window the hand-written programs use, this
//! maps arbitrary user VAs (`arch::paging_map_frame`), so a program links at
//! its own base (4 GiB) and is fully self-contained - no dependence on kernel
//! `.text`, which is what makes a separately-compiled binary runnable.

use crate::arch::{self, MapPerm};
use crate::elf::{self, Elf, PF_W, PF_X, Segment};
use crate::mm::AddressSpace;
use crate::mm::frames::{self, FRAME_SIZE};
use crate::queue::QueuePair;
use core::ptr::addr_of_mut;

/// Top of the initial user stack (docs/USERLAND.md): 8 GiB, free in every
/// cell root. The stack grows down from here.
pub const USER_STACK_TOP: usize = 0x2_0000_0000;
/// Initial stack size: 32 KiB.
pub const USER_STACK_PAGES: usize = 8;

/// Base VA of a loaded cell's queue-pair region (docs/LIBRHEO.md): 16 GiB,
/// above the image (1-4 GiB), stack (8 GiB), and anon mmap (12 GiB and up),
/// free in every cell root. `SYS_QUEUE_INFO` reports it to the cell.
pub const USER_QUEUE_VA: usize = 0x4_0000_0000;

/// Map a fresh queue-pair region into `aspace` at [`USER_QUEUE_VA`], write its
/// on-wire header, and return a [`QueuePair`] overlay bound to the **user** VA
/// (docs/LIBRHEO.md). The header is written through the kernel linear map (the
/// cell's address space is not active during load); the returned overlay's
/// pointers are user VAs, valid when the kernel drains the ring during the
/// cell's `SYS_DOORBELL` trap (its address space active). The caller mints the
/// QueuePair capability and records `(USER_QUEUE_VA, cap_id)` via
/// `user::set_queue_info`.
pub fn map_queue(aspace: &mut AddressSpace) -> QueuePair {
    map_queue_for(aspace, 0)
}

/// The VA of vcore `v`'s queue-pair region (docs/SUBSTRATE.md S5).
///
/// One region per vcore, packed from [`USER_QUEUE_VA`], because a ring is
/// single-producer: two contexts sharing one would have to serialise their
/// submissions, and once they run on two cores that serialisation is a cross-core
/// write to shared indices. `vcore_queue_va(0)` is `USER_QUEUE_VA` exactly, so every
/// single-vcore cell is where it always was.
pub const fn vcore_queue_va(v: usize) -> usize {
    USER_QUEUE_VA + v * QueuePair::REGION_SIZE
}

/// How many contexts of one cell can be given a **mapped queue ring**.
///
/// The one per-context limit that survived retiring `MAX_VCORES`, and it survived
/// because it is a real resource rather than an array dimension: each ring needs its own
/// `QueuePair::REGION_SIZE` of the cell's address space, and the window between
/// [`USER_QUEUE_VA`] and the next fixed region is 4 GiB. A cell may hold more contexts
/// than this - the scheduler, the FP areas and the per-context records are all bounded
/// by its frame budget now - it just cannot give every one of them a ring here.
///
/// Stated as a named constant so the limit says what it is. `MAX_VCORES` was the wrong
/// place for it twice over: it made an address-space question look like a scheduler
/// question, and it bounded the contexts *without* rings by the same number.
pub const MAX_QUEUE_VCORES: usize = 0x1_0000_0000 / QueuePair::REGION_SIZE;

// The whole per-vcore queue window has to stay inside the region the cell's recorded
// layout reserves for it, which `user::install` sizes at the channel base. A
// compile-time check rather than a comment, since the two constants live in different
// files.
const _: () = assert!(
    vcore_queue_va(MAX_QUEUE_VCORES) <= USER_QUEUE_VA + 0x1_0000_0000,
    "the per-vcore queue window must not reach the next fixed region"
);

/// [`map_queue`] for a named vcore: its own region at [`vcore_queue_va`].
pub fn map_queue_for(aspace: &mut AddressSpace, v: usize) -> QueuePair {
    let base = vcore_queue_va(v);
    let pages = QueuePair::REGION_SIZE / FRAME_SIZE;
    let mut first_pa = 0usize;
    for i in 0..pages {
        let pa = frames::alloc().expect("queue-pair region (bounded, at load)"); // zeroed
        if i == 0 {
            first_pa = pa;
        }
        aspace.map_user_frame(base + i * FRAME_SIZE, pa, MapPerm::UserRw);
    }
    // The header lives in the first page; write it through the linear map.
    // SAFETY: `first_pa` is a freshly allocated frame reached through the
    // kernel linear map; the header fits well within one page.
    unsafe {
        QueuePair::init_header(arch::phys_to_virt(first_pa) as *mut u8);
        // The overlay used at doorbell time binds to the cell's VA.
        QueuePair::attach(base as *mut u8)
    }
}

/// Map a **user stack for vcore `v`** and return its top.
///
/// Vcore 0's is [`map_stack`]'s, at [`USER_STACK_TOP`]; later ones sit below it with a
/// one-page gap, so a vcore running off the bottom of its stack faults instead of
/// walking into a sibling's - a guard page from the layout rather than a dedicated
/// mapping, the same trick the Linux stack reservation uses.
///
/// No System V initial block: a secondary vcore is not a fresh process, it is another
/// context of one that already parsed its arguments. Its entry takes none.
pub fn map_vcore_stack(aspace: &mut AddressSpace, v: usize) -> usize {
    assert!(v > 0, "vcore 0's stack is `map_stack`'s");
    // (pages + 1) per vcore: the extra page is the guard gap.
    let top = USER_STACK_TOP - v * (USER_STACK_PAGES + 1) * FRAME_SIZE;
    let mut va = top - USER_STACK_PAGES * FRAME_SIZE;
    while va < top {
        let pa = frames::alloc().expect("vcore stack page (bounded, at load)");
        aspace.map_user_frame(va, pa, MapPerm::UserRw);
        va += FRAME_SIZE;
    }
    top
}

/// Base VA of a **device BAR window** mapped into a driver cell (docs/DRIVERS.md 4.1):
/// 28 GiB, in the gap between the channel region (24 GiB) and grants (32 GiB), free in
/// every cell root and below every ISA's user ceiling.
pub const USER_BAR_VA: usize = 0x7_0000_0000;

/// Largest BAR window a cell may be given here: 4 MiB.
///
/// Not a design ceiling - it is what the proof needs and what a bounded fixed window can
/// hold without moving its neighbours. NVMe's BAR0 register file is 16 KiB; a GPU
/// framebuffer aperture is orders larger and would want the region allocator
/// (docs/SUBSTRATE.md pillar 2) rather than a bigger constant.
pub const USER_BAR_MAX: usize = 4 * 1024 * 1024;
const _: () = assert!(USER_BAR_VA + USER_BAR_MAX < 0x8_0000_0000);

/// Map a device **BAR window** into `aspace` as uncached device memory, returning the
/// cell VA and byte length it was mapped at (docs/DRIVERS.md 4.1, the `BarWindow` grant
/// and the first leg of D2's device capability trio).
///
/// **The launcher does this, not the cell.** `SYS_GRANT(MemKind::DeviceBar)` stays
/// refused, and that refusal is the design rather than a gap: owning a device is
/// authority a cell cannot mint for itself, exactly as the W^X exception and the
/// cell-spawn capability are minted by whatever launches the cell
/// (docs/ARCHITECTURE.md 5.1). A cell that was not given a window has no mapping at
/// these addresses and cannot obtain one.
///
/// Bounded twice over: to the BAR's **enumerated extent**, so a cell can never reach a
/// neighbouring device's registers by asking for more than the BAR holds, and to
/// [`USER_BAR_MAX`]. `None` if the BAR is unassigned, an I/O-space BAR (not memory), or
/// larger than the window.
pub fn map_device_bar(
    aspace: &mut AddressSpace,
    bar: &crate::hw::PciBar,
) -> Option<(usize, usize)> {
    if bar.io || bar.base == 0 || bar.size == 0 {
        return None;
    }
    let len = bar.size as usize;
    if len > USER_BAR_MAX {
        return None;
    }
    // Whole pages: a BAR is page-aligned by the PCI spec, and mapping a partial page
    // would expose whatever shares it.
    let pages = len.div_ceil(FRAME_SIZE);
    for i in 0..pages {
        aspace.map_user_frame(
            USER_BAR_VA + i * FRAME_SIZE,
            bar.base as usize + i * FRAME_SIZE,
            MapPerm::UserDevice,
        );
    }
    Some((USER_BAR_VA, pages * FRAME_SIZE))
}

/// Base VA of a loaded cell's **cross-cell shared channel** region
/// (docs/LIBRHEO.md Phase E): 24 GiB, between the file-mmap (20 GiB) and grant
/// (32 GiB) regions, free in every cell root. `SYS_CONNECT` reports it. The two
/// cells of a connection map the *same* frames here at this VA, so the SPSC ring
/// they overlay drives one set of physical words (a typed queue pair whose two
/// ends live in two cells, IO.md 6).
pub const USER_CHANNEL_VA: usize = 0x6_0000_0000;

/// Pages a shared channel region occupies (a full queue-pair region).
pub const CHANNEL_PAGES: usize = QueuePair::REGION_SIZE / FRAME_SIZE;

/// Allocate the frames for a cross-cell shared channel and write a fresh ring
/// header into them (docs/LIBRHEO.md Phase E). Returns the frame list; the
/// caller maps it into *each* peer with [`map_channel_into`] (so the same frames
/// back both ends) and each cell overlays its own [`QueuePair`] at
/// [`USER_CHANNEL_VA`]. The header is written once through the kernel linear map
/// (no cell is active during setup). The kernel never drains this ring - the two
/// cells drive the SQ/CQ directly over the shared frames.
pub fn alloc_channel() -> [usize; CHANNEL_PAGES] {
    let mut framelist = [0usize; CHANNEL_PAGES];
    for (i, slot) in framelist.iter_mut().enumerate() {
        let pa = frames::alloc().expect("channel region (bounded, at load)"); // zeroed
        *slot = pa;
        if i == 0 {
            // SAFETY: `pa` is a freshly allocated frame reached through the
            // kernel linear map; the header fits well within one page.
            unsafe { QueuePair::init_header(arch::phys_to_virt(pa) as *mut u8) };
        }
    }
    framelist
}

/// Map the frames of a shared channel into `aspace` at [`USER_CHANNEL_VA`], RW so
/// the cell can drive its side of the SPSC rings. Called once per peer with the
/// **same** `framelist` from [`alloc_channel`], so both cells share the frames.
pub fn map_channel_into(aspace: &mut AddressSpace, framelist: &[usize; CHANNEL_PAGES]) {
    map_channel_into_slot(aspace, framelist, 0);
}

/// The VA of channel **slot** `slot` in a cell (docs/NETSTACK.md the service-cell
/// section, rheo-net N4a): slot 0 is [`USER_CHANNEL_VA`] (the Phase E/J channel),
/// and each further slot is one queue-pair region above it. A **service cell**
/// holds one slot per client, so it has N distinct rings; every client sees its own
/// end at slot 0. `MAX_CELL_CHANNELS` slots span far less than the 8 GiB to the
/// next region (32 GiB grants), so nothing collides.
pub const fn channel_slot_va(slot: usize) -> usize {
    USER_CHANNEL_VA + slot * QueuePair::REGION_SIZE
}

/// Map the frames of a shared channel into `aspace` at channel slot `slot`, RW so
/// the cell can drive its side of the SPSC rings (docs/NETSTACK.md rheo-net N4a).
/// Called once per peer with the **same** `framelist` from [`alloc_channel`].
pub fn map_channel_into_slot(
    aspace: &mut AddressSpace,
    framelist: &[usize; CHANNEL_PAGES],
    slot: usize,
) {
    let base = channel_slot_va(slot);
    for (i, &pa) in framelist.iter().enumerate() {
        aspace.map_user_frame(base + i * FRAME_SIZE, pa, MapPerm::UserRw);
    }
}

/// Load `image` into `aspace`; returns the entry-point VA. The caller then
/// builds a trap frame at that entry with a stack from `map_stack`.
pub fn load_elf(image: &[u8], aspace: &mut AddressSpace) -> Option<usize> {
    let elf = Elf::parse(image)?;
    elf.for_each_load(|seg| map_segment(aspace, image, seg, 0))?;
    Some(elf.entry() as usize)
}

/// Load bias for an `ET_DYN` (PIE / static-PIE) Linux image (docs/
/// LINUX-COMPAT.md 4): 4 GiB, free in every cell root. `ET_EXEC` images load
/// at their linked address (bias 0).
pub const LINUX_DYN_BASE: usize = 0x1_0000_0000;

/// Load bias for the ELF **interpreter** (`ld-linux-*.so`) of a dynamically-
/// linked binary (docs/LINUX-COMPAT.md L7): 64 GiB, well clear of the main
/// image (4 GiB), the stack (8 GiB), and the anonymous mmap region (12 GiB and
/// up, where ld.so maps the shared libraries), so the interpreter never
/// collides with them. `AT_BASE` carries this to ld.so's self-relocation.
pub const LINUX_INTERP_BASE: usize = 0x10_0000_0000;

/// How many `PT_LOAD`s of one image may be left for the page-fault handler. A
/// glibc binary has three or four; 12 leaves room and keeps the struct small.
pub const MAX_IMAGE_SEGS: usize = 12;

/// One `PT_LOAD` the loader **recorded instead of copying**, for the personality to
/// turn into a file-backed mapping (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2).
///
/// The loader cannot insert the record itself: the VMA list is per-cell Linux state
/// and the cell does not exist yet at load time. So it reports what it chose not to
/// copy, and `linux::install_cell` maps it.
#[derive(Copy, Clone)]
pub struct ImageSeg {
    /// Page-aligned start VA, already biased.
    pub base: usize,
    /// Page-rounded length from `base`.
    pub len: usize,
    /// mmap-style prot bits (the personality's vocabulary, not `MapPerm`).
    pub prot: u64,
    /// The [`crate::linux::filemap`] entry this segment's bytes come from. Per
    /// segment, not per image, because a dynamically linked program is **two** files:
    /// the program and its interpreter.
    ///
    /// This record owns one reference to it; `linux::install_cell` hands that
    /// reference to the VMA record it creates.
    pub file: crate::linux::filemap::Handle,
    /// File offset corresponding to `base`.
    pub off: u64,
    /// Bytes of file content from `base`; past it the pages are zero.
    pub file_len: usize,
}

impl ImageSeg {
    const EMPTY: ImageSeg = ImageSeg {
        base: 0,
        len: 0,
        prot: 0,
        file: 0,
        off: 0,
        file_len: 0,
    };
}

/// mmap-style prot bits for an ELF segment's `p_flags`. The personality stores prot
/// rather than a `MapPerm` (see `Vma::prot`), so a recorded segment must arrive in
/// the same vocabulary a `mmap` would.
///
/// `PF_X | PF_W` becomes read-execute, not read-write-execute: W^X is structural
/// (there is no RWX `MapPerm`) and an ELF `PT_LOAD` is never both in practice.
fn prot_from_pf(flags: u32) -> u64 {
    // 1 = PROT_READ, 2 = PROT_WRITE, 4 = PROT_EXEC (the mmap values).
    if flags & PF_X != 0 {
        1 | 4
    } else if flags & PF_W != 0 {
        1 | 2
    } else {
        1
    }
}

/// What a loaded Linux image needs for its auxv (docs/LINUX-COMPAT.md L1).
pub struct LinuxImage {
    /// Entry point to start execution at (already biased). For a dynamically-
    /// linked binary this is the **interpreter's** entry (ld.so runs first);
    /// otherwise it is the program's own entry.
    pub entry: usize,
    /// `AT_BASE`: the interpreter's load bias for a dynamic binary
    /// (`LINUX_INTERP_BASE`), else the program's own bias (0 for `ET_EXEC`).
    pub bias: usize,
    /// Virtual address of the **main program's** program-header table
    /// (`AT_PHDR`), or 0 if the headers were not covered by a `PT_LOAD`. ld.so
    /// walks these to relocate the program.
    pub phdr: usize,
    /// `AT_PHENT` / `AT_PHNUM` (the main program's).
    pub phent: usize,
    pub phnum: usize,
    /// `AT_ENTRY`: the **main program's** entry point (biased), even when
    /// execution starts in the interpreter. ld.so jumps here after relocation.
    pub at_entry: usize,
    /// Highest mapped VA rounded up to a page: where the `brk` heap starts
    /// (docs/LINUX-COMPAT.md L2).
    pub image_end: usize,
    /// Stack bytes the image asked for via `PT_GNU_STACK` `p_memsz`, or 0 if it
    /// asked for nothing (docs/ARCHITECTURE-DEBT.md 4.0). `stack::setup_stack`
    /// sizes the initial stack from this, clamped to
    /// [`crate::linux::stack::LINUX_STACK_MAX_PAGES`].
    ///
    /// Read from the **main program**, not the interpreter: `ld.so` carries its
    /// own `PT_GNU_STACK` and it is the program's requirement that matters.
    pub stack_want: usize,
    /// Segments left for demand paging. Empty means the whole image was copied
    /// eagerly, which is still the answer for every non-Linux loader.
    ///
    /// Each record owns one `filemap` reference; `linux::install_cell` passes it
    /// straight to the VMA record, so it releases nothing and leaks nothing. A caller
    /// that loads an image and never installs it must release them itself.
    pub segs: [ImageSeg; MAX_IMAGE_SEGS],
    pub nsegs: usize,
}

impl LinuxImage {
    /// The recorded segments (`segs[..nsegs]`).
    pub fn recorded(&self) -> &[ImageSeg] {
        &self.segs[..self.nsegs]
    }
}

/// Load a Linux ELF (`ET_EXEC` or `ET_DYN`) into `aspace`, applying the
/// standard bias for a position-independent image, and return the facts its
/// auxv needs. Segments are mapped at `vaddr + bias`; no relocation
/// processing (a static-PIE's `rcrt1` self-relocates, docs/LINUX-COMPAT.md).
pub fn load_elf_linux(image: &[u8], aspace: &mut AddressSpace) -> Option<LinuxImage> {
    let elf = Elf::parse(image)?;
    let bias = match elf.etype() {
        elf::ET_DYN => LINUX_DYN_BASE,
        elf::ET_EXEC => 0,
        _ => return None,
    };
    // The image is already resident in kernel memory, so copying every page into a
    // frame allocates a **second** copy of the whole program - which for a large
    // binary is where its memory cost actually lands (docs/ARCHITECTURE-DEBT.md 4.0,
    // blocker 2). Register it as a backing store and record the segments that can be
    // filled from it on first touch; anything that cannot is still copied here.
    //
    // SAFETY: `image` must outlive the cell. Every caller passes a `'static` blob
    // (a test kernel's `include_bytes!`); this is the one constraint `open_mem`
    // documents and the reason it is unsafe.
    let mut rec = SegRecorder::new();
    rec.begin(unsafe { crate::linux::filemap::open_mem(image.as_ptr() as usize, image.len()) });
    let mut image_end = 0usize;
    elf.for_each_load(|seg| {
        let end = seg.vaddr as usize + bias + seg.memsz;
        if end > image_end {
            image_end = end;
        }
        if rec.record(seg, bias) {
            Some(())
        } else {
            map_segment(aspace, image, seg, bias)
        }
    })?;
    let image_end = (image_end + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
    let phdr = elf.phdr_vaddr().map(|v| v as usize + bias).unwrap_or(0);
    let main_entry = elf.entry() as usize + bias;

    // Dynamically linked? A `PT_INTERP` names the ELF interpreter
    // (`ld-linux-*.so`, docs/LINUX-COMPAT.md L7). Load it as a second `ET_DYN`
    // at `LINUX_INTERP_BASE`, resolving its path through the VFS, and start
    // execution there - ld.so then maps and relocates the program + libc at
    // runtime. `AT_BASE` = the interpreter's bias; `AT_ENTRY` stays the main
    // program's entry. No kernel relocation processing (ld.so self-relocates).
    let (entry, at_base) = match elf.interp() {
        Some((off, filesz)) => {
            let interp_entry = load_interp(image, off, filesz, aspace, &mut rec)?;
            (interp_entry, LINUX_INTERP_BASE)
        }
        None => (main_entry, bias),
    };
    rec.finish();

    Some(LinuxImage {
        entry,
        bias: at_base,
        phdr,
        phent: elf.phentsize(),
        phnum: elf.phnum(),
        at_entry: main_entry,
        image_end,
        stack_want: elf.stack_size().unwrap_or(0),
        segs: rec.segs,
        nsegs: rec.nsegs,
    })
}

/// Image pages left to demand paging since boot, and image pages copied into frames at
/// load. Together they are the witness that a load is lazy - and the *ratio* is what
/// makes it a witness rather than a claim, because `execve` and the ELF interpreter are
/// loaded deep inside a syscall where a test can measure nothing directly
/// (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2).
static mut RECORDED_PAGES: u64 = 0;
static mut EAGER_PAGES: u64 = 0;

fn bump(counter: *mut u64, bytes: usize) {
    // SAFETY: single CPU, synchronous; loads never run concurrently.
    unsafe { *counter = (*counter).wrapping_add((bytes / FRAME_SIZE) as u64) };
}

/// Pages of ELF image left for the fault handler to fill, since boot.
pub fn recorded_pages() -> u64 {
    // SAFETY: single CPU.
    unsafe { *core::ptr::addr_of!(RECORDED_PAGES) }
}

/// Pages of ELF image copied into frames at load time, since boot.
pub fn eager_pages() -> u64 {
    // SAFETY: single CPU.
    unsafe { *core::ptr::addr_of!(EAGER_PAGES) }
}

/// A loader records against one file at a time, and a dynamically linked program is
/// two: the program and its interpreter. Two is the ceiling because a *third* file is
/// something `ld.so` maps itself, through `mmap`, which is already demand-paged.
const MAX_STORES: usize = 2;

/// Collects the `PT_LOAD`s a loader chose not to copy.
///
/// Two conditions have to hold for a segment to be recorded, and both were found by
/// a segment that broke without them:
///
/// 1. **`filesz == memsz`.** A segment with a `.bss` tail is partly file and partly
///    zero *within one record*, and getting that boundary wrong produced a null
///    dereference in a static Rust binary. The measured target has no `.bss` in any
///    `PT_LOAD`, and a `.bss` tail is tens of KiB, so the honest scope is: whole-file
///    segments are demand-paged, the rest are copied.
/// 2. **`p_offset` and `p_vaddr` congruent mod the page size.** Demand paging fills
///    whole pages, so the page containing VA `v` must correspond to a page-aligned
///    file offset. Every real toolchain emits segments this way; a hand-built ELF
///    that does not is copied instead of mapped wrong.
struct SegRecorder {
    /// The stores this recorder opened, each holding **one** reference of its own that
    /// [`Self::finish`] gives back. Records take their own on top, so a store with no
    /// records is released and one with records survives - without either path having
    /// to count.
    stores: [Option<crate::linux::filemap::Handle>; MAX_STORES],
    nstores: usize,
    segs: [ImageSeg; MAX_IMAGE_SEGS],
    nsegs: usize,
}

impl SegRecorder {
    fn new() -> SegRecorder {
        SegRecorder {
            stores: [None; MAX_STORES],
            nstores: 0,
            segs: [ImageSeg::EMPTY; MAX_IMAGE_SEGS],
            nsegs: 0,
        }
    }

    /// Start recording against `store` (from `filemap::open`/`open_mem`), taking over
    /// its reference. `None` - the registry was full, or there is no file server - makes
    /// every following `record` decline, so the caller loads eagerly.
    fn begin(&mut self, store: Option<crate::linux::filemap::Handle>) {
        if self.nstores == MAX_STORES {
            // Cannot happen with two callers, but releasing beats leaking silently.
            if let Some(h) = store {
                crate::linux::filemap::close(h);
            }
            return;
        }
        self.stores[self.nstores] = store;
        self.nstores += 1;
    }

    /// The store `record` is currently filling for.
    fn current(&self) -> Option<crate::linux::filemap::Handle> {
        self.stores[self.nstores.checked_sub(1)?]
    }

    /// Record `seg`, or return false to tell the caller to copy it eagerly.
    fn record(&mut self, seg: &Segment, bias: usize) -> bool {
        let Some(store) = self.current() else {
            return false;
        };
        if self.nsegs == MAX_IMAGE_SEGS {
            crate::println!(
                "linux: {MAX_IMAGE_SEGS} image segments already recorded - the rest \
                 are loaded eagerly"
            );
            return false;
        }
        if seg.filesz != seg.memsz {
            // Not a defect, and not silent: say which segment and why.
            crate::println!(
                "linux: segment at {:#x} has a {:#x}-byte zero tail - loaded eagerly",
                seg.vaddr as usize + bias,
                seg.memsz - seg.filesz
            );
            return false;
        }
        let vaddr = seg.vaddr as usize + bias;
        let page_off = vaddr & (FRAME_SIZE - 1);
        if seg.offset & (FRAME_SIZE - 1) != page_off {
            crate::println!(
                "linux: segment at {vaddr:#x} has file offset {:#x}, not congruent to \
                 its VA mod the page size - loaded eagerly",
                seg.offset
            );
            return false;
        }
        let base = vaddr - page_off;
        self.segs[self.nsegs] = ImageSeg {
            base,
            len: (page_off + seg.memsz + FRAME_SIZE - 1) & !(FRAME_SIZE - 1),
            prot: prot_from_pf(seg.flags),
            file: store,
            off: (seg.offset - page_off) as u64,
            file_len: page_off + seg.filesz,
        };
        self.nsegs += 1;
        bump(addr_of_mut!(RECORDED_PAGES), self.segs[self.nsegs - 1].len);
        // This record's own reference, on top of the one `begin` took over.
        crate::linux::filemap::addref(store);
        true
    }

    /// Give back the reference each `begin` took over. A store with records survives on
    /// theirs; a store with none is released here, because a registry slot held for zero
    /// mappings is a leak and the registry is small enough to notice.
    fn finish(&self) {
        for h in self.stores.iter().flatten() {
            crate::linux::filemap::close(*h);
        }
    }
}

/// Load the ELF interpreter named by a `PT_INTERP` segment at
/// `LINUX_INTERP_BASE`, streaming it from the VFS (docs/LINUX-COMPAT.md L7). The
/// path bytes lie at `image[off..off+filesz]` (NUL-terminated); they are opened
/// through the registered `svc::FileOps` handler (the same VFS the program's
/// own file I/O uses), so the interpreter is found on the cell's `/lib` exactly
/// as on Linux. Returns the interpreter's (biased) entry point.
fn load_interp(
    image: &[u8],
    off: usize,
    filesz: usize,
    aspace: &mut AddressSpace,
    rec: &mut SegRecorder,
) -> Option<usize> {
    let ops = crate::svc::file_ops()?;
    // The path is NUL-terminated inside the segment; trim at the NUL.
    let bytes = image.get(off..off + filesz)?;
    let path_len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let path_va = image.as_ptr() as u64 + off as u64;
    let fd = (ops.open)(path_va, path_len as u64, 0);
    if fd < 0 {
        return None;
    }
    let fd = fd as u64;
    // The interpreter is the recorder's **second** file, with its own handle - the
    // mapping's, not this `fd`, which is closed below. `path_va` points into `image`,
    // i.e. kernel memory, which is what `filemap::open` requires.
    rec.begin(crate::linux::filemap::open(path_va, path_len as u64));
    let r = stream_elf_at(ops, fd, LINUX_INTERP_BASE, aspace, rec);
    (ops.close)(fd);
    r
}

/// Stream every `PT_LOAD` of the ELF open on `fd` into `aspace` at load bias
/// `bias`, reading each segment page-by-page from the VFS (docs/LINUX-COMPAT.md
/// L7). Returns the biased entry point. Shared by the interpreter loader and
/// the `execve` streaming path.
///
/// `rec` must already be `begin`-ed on a store for **this** file; segments it declines
/// are streamed into frames as before.
fn stream_elf_at(
    ops: &crate::svc::FileOps,
    fd: u64,
    bias: usize,
    aspace: &mut AddressSpace,
    rec: &mut SegRecorder,
) -> Option<usize> {
    let mut hdr = [0u8; HDR_BUF];
    (ops.lseek)(fd, 0, 0);
    let n = (ops.read)(fd, hdr.as_mut_ptr() as u64, HDR_BUF as u64);
    if n < 64 {
        return None;
    }
    let elf = Elf::parse(&hdr[..n as usize])?;
    elf.for_each_load_streamed(|seg| {
        if rec.record(seg, bias) {
            Some(())
        } else {
            stream_segment(ops, fd, seg, bias, aspace)
        }
    })?;
    Some(elf.entry() as usize + bias)
}

/// Load a Linux ELF for `execve` by **streaming** it from the VFS into a fresh
/// address space (docs/LINUX-COMPAT.md L6): the kernel never holds the whole
/// image in a contiguous buffer. Only the ELF header + program-header table are
/// read into a small kernel buffer; each `PT_LOAD` segment's bytes are read
/// page-by-page directly into its destination frame (through the kernel linear
/// map). `open`/`read`/`lseek`/`close` are the registered `svc::FileOps` VFS
/// handlers. Returns the `LinuxImage` (auxv facts) or None on any error.
/// Every page is committed here, so the returned image has **no** recorded segments
/// and the caller needs no page-fault handler. This is the variant a **native** cell's
/// `SYS_SPAWN` uses: native cells have no VMA list and nothing to map records with, so
/// a demand-paged image would leave the child with an address space full of holes.
///
/// [`exec_elf_from_vfs_demand`] is the lazy twin, for the Linux personality only.
pub fn exec_elf_from_vfs(
    ops: &crate::svc::FileOps,
    path_va: u64,
    path_len: u64,
    aspace: &mut AddressSpace,
) -> Option<LinuxImage> {
    // An empty recorder declines every segment, so `exec_elf_inner` streams them all.
    exec_from_vfs(ops, path_va, path_len, aspace, false)
}

/// [`exec_elf_from_vfs`] with the image **demand-paged** (docs/ARCHITECTURE-DEBT.md
/// 4.0, blocker 2): the segments it can leave to the page-fault handler come back in
/// `LinuxImage::recorded()`.
///
/// **The caller must map them.** `linux::exec_reinit` does, via
/// `record_image_segments`. A caller that ignores them gets a child with no pages for
/// its own code and no diagnostic - which is exactly what happened when this and the
/// eager path were one function and the native `SYS_SPAWN` inherited the laziness.
///
/// `path_va`/`path_len` must name the path in **kernel** memory (both callers copy it
/// out of the cell first), because the mapping opens its own long-lived handle over it
/// - the `fd` here is closed on return.
pub fn exec_elf_from_vfs_demand(
    ops: &crate::svc::FileOps,
    path_va: u64,
    path_len: u64,
    aspace: &mut AddressSpace,
) -> Option<LinuxImage> {
    exec_from_vfs(ops, path_va, path_len, aspace, true)
}

fn exec_from_vfs(
    ops: &crate::svc::FileOps,
    path_va: u64,
    path_len: u64,
    aspace: &mut AddressSpace,
    demand: bool,
) -> Option<LinuxImage> {
    let fd = (ops.open)(path_va, path_len, 0);
    if fd < 0 {
        return None;
    }
    let fd = fd as u64;
    let mut rec = SegRecorder::new();
    if demand {
        rec.begin(crate::linux::filemap::open(path_va, path_len));
    }
    let r = exec_elf_inner(ops, fd, aspace, &mut rec, demand);
    rec.finish();
    (ops.close)(fd);
    r
}

/// The ELF header + program-header table fit here (glibc static binaries have
/// `e_phoff == 64` and a handful of program headers).
const HDR_BUF: usize = 4096;

fn exec_elf_inner(
    ops: &crate::svc::FileOps,
    fd: u64,
    aspace: &mut AddressSpace,
    rec: &mut SegRecorder,
    demand: bool,
) -> Option<LinuxImage> {
    // Read the header region (ELF header + phdr table) into a kernel buffer.
    let mut hdr = [0u8; HDR_BUF];
    (ops.lseek)(fd, 0, 0); // SEEK_SET
    let n = (ops.read)(fd, hdr.as_mut_ptr() as u64, HDR_BUF as u64);
    if n < 64 {
        return None;
    }
    let hdr = &hdr[..n as usize];
    let elf = Elf::parse(hdr)?;
    let bias = match elf.etype() {
        elf::ET_DYN => LINUX_DYN_BASE,
        elf::ET_EXEC => 0,
        _ => return None,
    };
    let mut image_end = 0usize;
    elf.for_each_load_streamed(|seg| {
        let end = seg.vaddr as usize + bias + seg.memsz;
        if end > image_end {
            image_end = end;
        }
        if rec.record(seg, bias) {
            Some(())
        } else {
            stream_segment(ops, fd, seg, bias, aspace)
        }
    })?;
    let image_end = (image_end + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
    let phdr = elf.phdr_vaddr().map(|v| v as usize + bias).unwrap_or(0);
    let main_entry = elf.entry() as usize + bias;

    // Dynamically linked? Mirror `load_elf_linux`: a `PT_INTERP` names the ELF
    // interpreter (`ld-linux-*.so`). Stream it in as a second `ET_DYN` at
    // `LINUX_INTERP_BASE` and start execution there; ld.so then maps + relocates
    // the program and libc at runtime over fd-backed `mmap` (docs/LINUX-COMPAT.md
    // L7). `AT_BASE` = the interpreter's bias, `AT_ENTRY` stays the main program's
    // entry, `AT_PHDR` the program's phdr - all biased by the program's own bias
    // above. No kernel relocation (ld.so self-relocates). This is the streaming
    // `execve` twin of the `load_elf_linux` initial-load path, so an `execve`d
    // dynamic binary (a shell launching a dynamic program) now works, not only a
    // dynamic binary loaded as a cell's initial image.
    let (entry, at_base) = match elf.interp() {
        Some((off, filesz)) => {
            let interp_entry = load_interp_streamed(ops, fd, off, filesz, aspace, rec, demand)?;
            (interp_entry, LINUX_INTERP_BASE)
        }
        None => (main_entry, bias),
    };

    Some(LinuxImage {
        entry,
        bias: at_base,
        phdr,
        phent: elf.phentsize(),
        phnum: elf.phnum(),
        at_entry: main_entry,
        image_end,
        stack_want: elf.stack_size().unwrap_or(0),
        segs: rec.segs,
        nsegs: rec.nsegs,
    })
}

/// Streaming twin of [`load_interp`]: load the ELF interpreter for an `execve`d
/// dynamic binary. The streaming path holds only the header buffer, not the whole
/// image, so the `PT_INTERP` path string is read from the main program's `fd` at the
/// segment's file offset (rather than from an in-memory image). Streams the
/// interpreter at [`LINUX_INTERP_BASE`] and returns its (biased) entry point.
///
/// `demand` matches the main program's choice: a Linux `execve` demand-pages the
/// interpreter too (it becomes the recorder's second store); a native `SYS_SPAWN`
/// - which has no `PT_INTERP` in practice - would stream it eagerly.
fn load_interp_streamed(
    ops: &crate::svc::FileOps,
    main_fd: u64,
    off: usize,
    filesz: usize,
    aspace: &mut AddressSpace,
    rec: &mut SegRecorder,
    demand: bool,
) -> Option<usize> {
    // The interp path is a short NUL-terminated string; a real toolchain emits
    // ~30 bytes ("/lib64/ld-linux-x86-64.so.2"). A pathological longer path fails
    // cleanly here rather than being truncated.
    let mut pathbuf = [0u8; 256];
    if filesz == 0 || filesz > pathbuf.len() {
        return None;
    }
    (ops.lseek)(main_fd, off as i64, 0); // SEEK_SET
    let got = (ops.read)(main_fd, pathbuf.as_mut_ptr() as u64, filesz as u64);
    if got <= 0 {
        return None;
    }
    let got = got as usize;
    let path_len = pathbuf[..got].iter().position(|&b| b == 0).unwrap_or(got);
    if path_len == 0 {
        return None;
    }
    let path_va = pathbuf.as_ptr() as u64;
    let fd = (ops.open)(path_va, path_len as u64, 0);
    if fd < 0 {
        return None;
    }
    let fd = fd as u64;
    // The interpreter is the recorder's second store, with its own handle - the
    // mapping's, not this `fd`, which is closed below. `pathbuf` is a kernel stack
    // buffer, which is what `filemap::open` requires (it opens immediately and keeps
    // an fd, not the path bytes).
    if demand {
        rec.begin(crate::linux::filemap::open(path_va, path_len as u64));
    }
    let r = stream_elf_at(ops, fd, LINUX_INTERP_BASE, aspace, rec);
    (ops.close)(fd);
    r
}

/// Map one `PT_LOAD` segment, reading its file bytes page-by-page from `fd`
/// straight into each destination frame (through the kernel linear map, so no
/// giant contiguous kernel buffer is needed - docs/LINUX-COMPAT.md L6).
fn stream_segment(
    ops: &crate::svc::FileOps,
    fd: u64,
    seg: &Segment,
    bias: usize,
    aspace: &mut AddressSpace,
) -> Option<()> {
    let vaddr = seg.vaddr as usize + bias;
    let va0 = vaddr & !(FRAME_SIZE - 1);
    let mem_end = vaddr.checked_add(seg.memsz)?;
    let perm = seg_perm(seg.flags);
    bump(addr_of_mut!(EAGER_PAGES), mem_end - va0);
    let mut va = va0;
    while va < mem_end {
        let pa = frames::alloc().expect("ELF segment page (bounded by the image, at load)"); // zeroed (bss/zero-fill already done)
        let copy_lo = va.max(vaddr);
        let copy_hi = (va + FRAME_SIZE).min(vaddr + seg.filesz);
        if copy_lo < copy_hi {
            let n = (copy_hi - copy_lo) as u64;
            let src_off = (seg.offset + (copy_lo - vaddr)) as i64;
            let dst = arch::phys_to_virt(pa) + (copy_lo - va);
            (ops.lseek)(fd, src_off, 0); // SEEK_SET
            // FileOps reads into a VA in the *current* map; a kernel VA
            // (phys_to_virt of the destination frame) is valid here because the
            // VFS handler runs in kernel context.
            let got = (ops.read)(fd, dst as u64, n);
            if got != n as i64 {
                return None;
            }
        }
        aspace.map_user_frame(va, pa, perm);
        va += FRAME_SIZE;
    }
    Some(())
}

/// Map the initial user stack with no arguments and return the initial SP.
/// A thin wrapper over [`setup_stack`] with empty argv/envp, so a program that
/// does not use its arguments (e.g. the std proof program) still sees a valid
/// `argc == 0` block at SP.
pub fn map_stack(aspace: &mut AddressSpace) -> usize {
    setup_stack(aspace, &[], &[])
}

/// Map the initial user stack and lay out the System V initial process stack
/// (docs/USERLAND.md M5): `argc`, the `argv` pointer array (NULL-terminated),
/// then the `envp` pointer array (NULL-terminated), with the argument and
/// environment strings living above. Returns the initial SP, which points at
/// `argc`. A crt0's `_start` reads `argc`/`argv` from there; `std::env::args`
/// then works over the real arguments.
///
/// The cell's address space is not active during load, so the kernel writes
/// the block into the top stack frame through its identity mapping (PA = VA
/// in kernel space) and stores *user* VAs in the pointer arrays. The block
/// must fit in the top page; the caller keeps argv/envp small (asserted).
pub fn setup_stack(aspace: &mut AddressSpace, args: &[&[u8]], envs: &[&[u8]]) -> usize {
    // Allocate and map every stack page; remember the top page's physical
    // frame so we can write the initial block into it below.
    let mut top_pa = 0usize;
    let mut va = USER_STACK_TOP - USER_STACK_PAGES * FRAME_SIZE;
    while va < USER_STACK_TOP {
        let pa = frames::alloc().expect("initial stack page (bounded, at load)");
        aspace.map_user_frame(va, pa, MapPerm::UserRw);
        if va == USER_STACK_TOP - FRAME_SIZE {
            top_pa = pa;
        }
        va += FRAME_SIZE;
    }

    let base_va = USER_STACK_TOP - FRAME_SIZE;
    // SAFETY: `top_pa` is a freshly allocated, zeroed frame we just mapped at
    // `base_va`; the kernel writes it through its linear map (identity on
    // x86/riscv; the high map on aarch64), only within its FRAME_SIZE bytes.
    let page = arch::phys_to_virt(top_pa) as *mut u8;

    // Copy the argument then environment strings near the top of the page,
    // growing downward, recording each string's user VA. Capped so a caller
    // cannot overflow the fixed pointer-VA table.
    const MAX_PTRS: usize = 64;
    assert!(
        args.len() + envs.len() <= MAX_PTRS,
        "too many argv/envp entries"
    );
    let mut str_vas = [0usize; MAX_PTRS];
    let mut off = FRAME_SIZE;
    let write_str = |page: *mut u8, off: &mut usize, s: &[u8]| -> usize {
        *off -= s.len() + 1; // room for the string and its NUL
        // SAFETY: bounds ensured by the fit assertion below.
        unsafe {
            core::ptr::copy_nonoverlapping(s.as_ptr(), page.add(*off), s.len());
            *page.add(*off + s.len()) = 0;
        }
        base_va + *off
    };
    for (i, s) in args.iter().chain(envs.iter()).enumerate() {
        str_vas[i] = write_str(page, &mut off, s);
    }

    // The pointer block: argc, argv[..], NULL, envp[..], NULL. It sits below
    // the strings, 16-byte aligned (the x86-64 SysV entry requires SP % 16 == 0
    // at `argc`; base_va is already 16-aligned).
    let words = 1 + args.len() + 1 + envs.len() + 1;
    let block_bytes = words * 8;
    let sp_off = (off - block_bytes) & !0xF;
    assert!(
        sp_off < off,
        "argv/envp block does not fit the initial stack page"
    );

    // SAFETY: `sp_off .. off` lies within the page and below the strings.
    unsafe {
        let mut w = (page.add(sp_off)) as *mut u64;
        w.write(args.len() as u64); // argc
        // argv pointers, NULL, envp pointers, NULL - the string VAs are already
        // in `str_vas` (args first, then envs).
        for &va in &str_vas[..args.len()] {
            w = w.add(1);
            w.write(va as u64);
        }
        w = w.add(1);
        w.write(0); // argv NULL terminator
        for &va in &str_vas[args.len()..args.len() + envs.len()] {
            w = w.add(1);
            w.write(va as u64);
        }
        w = w.add(1);
        w.write(0); // envp NULL terminator
    }

    base_va + sp_off
}

fn seg_perm(flags: u32) -> MapPerm {
    // W^X: executable segments are mapped read+execute, writable ones
    // read+write, the rest read-only. (An ELF PT_LOAD is never both.)
    if flags & PF_X != 0 {
        MapPerm::UserRx
    } else if flags & PF_W != 0 {
        MapPerm::UserRw
    } else {
        MapPerm::UserRo
    }
}

fn map_segment(aspace: &mut AddressSpace, image: &[u8], seg: &Segment, bias: usize) -> Option<()> {
    let vaddr = seg.vaddr as usize + bias;
    let va0 = vaddr & !(FRAME_SIZE - 1);
    let mem_end = vaddr.checked_add(seg.memsz)?;
    let perm = seg_perm(seg.flags);

    bump(addr_of_mut!(EAGER_PAGES), mem_end - va0);
    let mut va = va0;
    while va < mem_end {
        let pa = frames::alloc().expect("ELF segment page (bounded by the image, at load)"); // zeroed, so bss/zero-fill is already done
        // Copy the file bytes of this segment that fall in [va, va+FRAME_SIZE).
        let copy_lo = va.max(vaddr);
        let copy_hi = (va + FRAME_SIZE).min(vaddr + seg.filesz);
        if copy_lo < copy_hi {
            let n = copy_hi - copy_lo;
            let src_off = seg.offset + (copy_lo - vaddr);
            let dst_off = copy_lo - va;
            // SAFETY: `pa` is a freshly allocated frame written through the
            // kernel's linear map (identity on x86/riscv; the high map on
            // aarch64); the source range was bounds-checked in
            // `Elf::for_each_load`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    image.as_ptr().add(src_off),
                    (arch::phys_to_virt(pa) as *mut u8).add(dst_off),
                    n,
                );
            }
        }
        aspace.map_user_frame(va, pa, perm);
        va += FRAME_SIZE;
    }
    Some(())
}
