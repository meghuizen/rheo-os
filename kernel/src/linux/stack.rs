//! The Linux initial process stack (docs/LINUX-COMPAT.md 4, milestone L1).
//! Beyond the System V `argc`/`argv`/`envp` block the native loader builds
//! (kernel/src/load.rs), a Linux program's crt0 (glibc `_start` /
//! `__libc_start_main`, or a static-PIE `rcrt1`) requires the **ELF
//! auxiliary vector** - AT_PHDR/AT_RANDOM/AT_PAGESZ and friends - immediately
//! after the envp NULL terminator. Without it glibc dereferences a missing
//! AT_RANDOM and crashes before `main`.
//!
//! The cell's address space is not active during load, so the kernel writes
//! the block into the top stack frame through its identity mapping (PA = VA
//! in kernel space) and stores *user* VAs in the pointer arrays, exactly like
//! `load::setup_stack`. The whole block must fit in the top page; argv/envp
//! for our fixtures easily do (asserted).

use crate::arch::MapPerm;
use crate::load::LinuxImage;
use crate::mm::AddressSpace;
use crate::mm::frames::{self, FRAME_SIZE};

/// Top of the initial user stack (matches load::USER_STACK_TOP): 8 GiB.
const USER_STACK_TOP: usize = 0x2_0000_0000;
/// Linux stacks are larger than the native 32 KiB - glibc probes and uses a
/// meaningful stack early. 1 MiB (matches the RLIMIT_STACK we report).
const LINUX_STACK_PAGES: usize = 256;

// ELF auxiliary-vector types (Linux uapi/linux/auxvec.h).
const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_FLAGS: u64 = 8;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_HWCAP: u64 = 16;
const AT_CLKTCK: u64 = 17;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;

/// Map the Linux initial stack for `img` and lay out argc / argv / envp /
/// auxv / strings. Returns the initial SP (points at argc, 16-aligned).
///
/// `args` and `envs` are byte slices (NUL-terminated on the stack). The auxv
/// is synthesized from `img` plus fixed identity values (docs/LINUX-COMPAT.md
/// 3): uid/gid 1000, not AT_SECURE, no vDSO. AT_RANDOM gets 16 bytes from a
/// freshly derived per-cell DRBG.
pub fn setup_stack(
    aspace: &mut AddressSpace,
    img: &LinuxImage,
    args: &[&[u8]],
    envs: &[&[u8]],
) -> usize {
    // Map every stack page; remember the top page's frame for the write-in.
    let mut top_pa = 0usize;
    let mut va = USER_STACK_TOP - LINUX_STACK_PAGES * FRAME_SIZE;
    while va < USER_STACK_TOP {
        let pa = frames::alloc();
        aspace.map_user_frame(va, pa, MapPerm::UserRw);
        if va == USER_STACK_TOP - FRAME_SIZE {
            top_pa = pa;
        }
        va += FRAME_SIZE;
    }

    let base_va = USER_STACK_TOP - FRAME_SIZE;
    // SAFETY: freshly allocated, zeroed, identity-mapped top frame; we write
    // only within its FRAME_SIZE bytes (fit asserted below).
    let page = top_pa as *mut u8;

    const MAX_PTRS: usize = 64;
    assert!(
        args.len() + envs.len() <= MAX_PTRS,
        "too many argv/envp entries"
    );

    // Strings (args, then envs) grow down from the top; record their VAs.
    let mut str_vas = [0usize; MAX_PTRS];
    let mut off = FRAME_SIZE;
    let write_bytes = |page: *mut u8, off: &mut usize, s: &[u8], nul: bool| -> usize {
        let extra = if nul { 1 } else { 0 };
        *off -= s.len() + extra;
        // SAFETY: bounds ensured by the fit assertion below.
        unsafe {
            core::ptr::copy_nonoverlapping(s.as_ptr(), page.add(*off), s.len());
            if nul {
                *page.add(*off + s.len()) = 0;
            }
        }
        base_va + *off
    };
    for (i, s) in args.iter().chain(envs.iter()).enumerate() {
        str_vas[i] = write_bytes(page, &mut off, s, true);
    }

    // 16 random bytes for AT_RANDOM, just above the pointer block. The hard
    // seeding gate applies: glibc uses AT_RANDOM for stack-protector canaries
    // and pointer guards, so an unseeded host must not exec, not exec weakly.
    let mut rnd = [0u8; 16];
    match crate::rng::derive_cell_drbg() {
        Some(mut d) => d.fill_bytes(&mut rnd),
        None => panic!("linux: exec refused - rng unseeded (no credited entropy source)"),
    }
    let random_va = write_bytes(page, &mut off, &rnd, false);

    // The auxv pairs (type, value), AT_NULL last. AT_PHDR is emitted only if
    // the loader found the headers in a PT_LOAD (docs/LINUX-COMPAT.md 4).
    let mut auxv: [(u64, u64); 20] = [(AT_NULL, 0); 20];
    let mut n = 0;
    let mut push = |t: u64, v: u64| {
        auxv[n] = (t, v);
        n += 1;
    };
    if img.phdr != 0 {
        push(AT_PHDR, img.phdr as u64);
        push(AT_PHENT, img.phent as u64);
        push(AT_PHNUM, img.phnum as u64);
    }
    push(AT_PAGESZ, FRAME_SIZE as u64);
    push(AT_BASE, img.bias as u64);
    push(AT_FLAGS, 0);
    push(AT_ENTRY, img.entry as u64);
    push(AT_UID, 1000);
    push(AT_EUID, 1000);
    push(AT_GID, 1000);
    push(AT_EGID, 1000);
    push(AT_SECURE, 0);
    push(AT_CLKTCK, 100);
    // AT_HWCAP: 0 = advertise no optional CPU features. glibc's ifunc
    // resolvers then choose the ISA-baseline implementations, matching the
    // state the kernel actually enabled (docs/LINUX-COMPAT.md 3).
    push(AT_HWCAP, 0);
    push(AT_RANDOM, random_va as u64);
    push(AT_EXECFN, str_vas[0] as u64);
    push(AT_NULL, 0);

    // Pointer block: argc, argv[..], NULL, envp[..], NULL, auxv pairs. Placed
    // below the strings, 16-aligned (base_va is 16-aligned).
    let words = 1 + args.len() + 1 + envs.len() + 1 + n * 2;
    let sp_off = (off - words * 8) & !0xF;
    assert!(
        sp_off < off,
        "argv/envp/auxv block does not fit the top page"
    );

    // SAFETY: sp_off..off lies within the page, below the strings.
    unsafe {
        let mut w = page.add(sp_off) as *mut u64;
        let mut put = |v: u64| {
            w.write(v);
            w = w.add(1);
        };
        put(args.len() as u64); // argc
        for &v in &str_vas[..args.len()] {
            put(v as u64);
        }
        put(0); // argv NULL
        for &v in &str_vas[args.len()..args.len() + envs.len()] {
            put(v as u64);
        }
        put(0); // envp NULL
        for &(t, v) in &auxv[..n] {
            put(t);
            put(v);
        }
    }

    base_va + sp_off
}
