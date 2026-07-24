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

use crate::arch::MapPerm;
use crate::elf::{Elf, PF_W, PF_X, Segment};
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
    elf.for_each_load(|seg| map_segment(aspace, image, seg))?;
    Some(elf.entry() as usize)
}

/// Map the initial user stack and return the top (the initial SP).
pub fn map_stack(aspace: &mut AddressSpace) -> usize {
    let mut va = USER_STACK_TOP - USER_STACK_PAGES * FRAME_SIZE;
    while va < USER_STACK_TOP {
        let pa = frames::alloc();
        aspace.map_user_frame(va, pa, MapPerm::UserRw);
        va += FRAME_SIZE;
    }
    USER_STACK_TOP
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

fn map_segment(aspace: &mut AddressSpace, image: &[u8], seg: &Segment) -> Option<()> {
    let vaddr = seg.vaddr as usize;
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
            // SAFETY: `pa` is a freshly allocated, identity-mapped frame; the
            // source range was bounds-checked in `Elf::for_each_load`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    image.as_ptr().add(src_off),
                    (pa as *mut u8).add(dst_off),
                    n,
                );
            }
        }
        aspace.map_user_frame(va, pa, perm);
        va += FRAME_SIZE;
    }
    Some(())
}
