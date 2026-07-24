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

/// Top of the initial user stack (docs/USERLAND.md): 8 GiB, free in every
/// cell root. The stack grows down from here.
pub const USER_STACK_TOP: usize = 0x2_0000_0000;
/// Initial stack size: 32 KiB.
pub const USER_STACK_PAGES: usize = 8;

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

/// What a loaded Linux image needs for its auxv (docs/LINUX-COMPAT.md L1).
pub struct LinuxImage {
    /// Entry point (already biased for `ET_DYN`).
    pub entry: usize,
    /// Load bias applied (`AT_BASE`-style; 0 for `ET_EXEC`).
    pub bias: usize,
    /// Virtual address of the program-header table (`AT_PHDR`), or 0 if the
    /// headers were not covered by a `PT_LOAD` (rare; auxv then omits it).
    pub phdr: usize,
    /// `AT_PHENT` / `AT_PHNUM`.
    pub phent: usize,
    pub phnum: usize,
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
    Some(LinuxImage {
        entry: elf.entry() as usize + bias,
        bias,
        phdr,
        phent: elf.phentsize(),
        phnum: elf.phnum(),
        image_end,
    })
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
        let pa = frames::alloc();
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
        let pa = frames::alloc(); // zeroed, so bss/zero-fill is already done
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
