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
    let pages = QueuePair::REGION_SIZE / FRAME_SIZE;
    let mut first_pa = 0usize;
    for i in 0..pages {
        let pa = frames::alloc().expect("queue-pair region (bounded, at load)"); // zeroed
        if i == 0 {
            first_pa = pa;
        }
        aspace.map_user_frame(USER_QUEUE_VA + i * FRAME_SIZE, pa, MapPerm::UserRw);
    }
    // The header lives in the first page; write it through the linear map.
    // SAFETY: `first_pa` is a freshly allocated frame reached through the
    // kernel linear map; the header fits well within one page.
    unsafe {
        QueuePair::init_header(arch::phys_to_virt(first_pa) as *mut u8);
        // The overlay used at doorbell time binds to the cell's VA.
        QueuePair::attach(USER_QUEUE_VA as *mut u8)
    }
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
    let mut image_end = 0usize;
    elf.for_each_load(|seg| {
        let end = seg.vaddr as usize + bias + seg.memsz;
        if end > image_end {
            image_end = end;
        }
        map_segment(aspace, image, seg, bias)
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
            let interp_entry = load_interp(image, off, filesz, aspace)?;
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
    })
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
    let r = stream_elf_at(ops, fd, LINUX_INTERP_BASE, aspace);
    (ops.close)(fd);
    r
}

/// Stream every `PT_LOAD` of the ELF open on `fd` into `aspace` at load bias
/// `bias`, reading each segment page-by-page from the VFS (docs/LINUX-COMPAT.md
/// L7). Returns the biased entry point. Shared by the interpreter loader and
/// the `execve` streaming path.
fn stream_elf_at(
    ops: &crate::svc::FileOps,
    fd: u64,
    bias: usize,
    aspace: &mut AddressSpace,
) -> Option<usize> {
    let mut hdr = [0u8; HDR_BUF];
    (ops.lseek)(fd, 0, 0);
    let n = (ops.read)(fd, hdr.as_mut_ptr() as u64, HDR_BUF as u64);
    if n < 64 {
        return None;
    }
    let elf = Elf::parse(&hdr[..n as usize])?;
    elf.for_each_load_streamed(|seg| stream_segment(ops, fd, seg, bias, aspace))?;
    Some(elf.entry() as usize + bias)
}

/// Load a Linux ELF for `execve` by **streaming** it from the VFS into a fresh
/// address space (docs/LINUX-COMPAT.md L6): the kernel never holds the whole
/// image in a contiguous buffer. Only the ELF header + program-header table are
/// read into a small kernel buffer; each `PT_LOAD` segment's bytes are read
/// page-by-page directly into its destination frame (through the kernel linear
/// map). `open`/`read`/`lseek`/`close` are the registered `svc::FileOps` VFS
/// handlers. Returns the `LinuxImage` (auxv facts) or None on any error.
pub fn exec_elf_from_vfs(
    ops: &crate::svc::FileOps,
    path_va: u64,
    path_len: u64,
    aspace: &mut AddressSpace,
) -> Option<LinuxImage> {
    let fd = (ops.open)(path_va, path_len, 0);
    if fd < 0 {
        return None;
    }
    let fd = fd as u64;
    let r = exec_elf_inner(ops, fd, aspace);
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
        stream_segment(ops, fd, seg, bias, aspace)
    })?;
    let image_end = (image_end + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
    let phdr = elf.phdr_vaddr().map(|v| v as usize + bias).unwrap_or(0);
    let entry = elf.entry() as usize + bias;
    // `execve` of a dynamically-linked binary (PT_INTERP) is not handled on the
    // streaming path yet - the L7 `linuxdyn` proof loads the dynamic binary
    // directly (`load_elf_linux`), not via `execve` (docs/LINUX-COMPAT.md L7).
    Some(LinuxImage {
        entry,
        bias,
        phdr,
        phent: elf.phentsize(),
        phnum: elf.phnum(),
        at_entry: entry,
        image_end,
    })
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
