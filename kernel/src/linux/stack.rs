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

use crate::arch::{self, MapPerm};
use crate::load::LinuxImage;
use crate::mm::AddressSpace;
use crate::mm::frames::{self, FRAME_SIZE};

/// Top of the initial user stack (matches load::USER_STACK_TOP): 8 GiB.
const USER_STACK_TOP: usize = 0x2_0000_0000;
/// Linux stacks are larger than the native 32 KiB - glibc probes and uses a
/// meaningful stack early. **8 MiB**, the glibc/Linux default `RLIMIT_STACK`,
/// and it must match the `RLIMIT_STACK` the personality reports
/// (`linux::rlimit_for`) because glibc sizes *thread* stacks from that number.
///
/// It was 1 MiB, which real programs overrun (deep recursion, big stack frames
/// in a JIT). The whole stack is mapped **eagerly** at load, so this costs
/// 8 MiB of frames per Linux cell up front; a guard-page + demand-grow stack is
/// the proper fix and rides with demand paging (docs/LINUX-COMPAT.md).
///
/// This is the **default**, used when the image asks for nothing. An image that
/// records a larger `PT_GNU_STACK` `p_memsz` gets that instead, up to
/// [`LINUX_STACK_MAX_PAGES`] - see [`stack_pages_for`].
pub const LINUX_STACK_PAGES: usize = 2048;

/// Ceiling on an image's `PT_GNU_STACK` request: **64 MiB**.
///
/// A bound rather than blind obedience, because the stack is mapped eagerly and
/// charged to the cell's frame budget: an image asking for 2 GiB would exhaust
/// the pool at load with no diagnostic near the cause. 64 MiB is 8x the default
/// and 5x the largest real request measured (the Claude Code binary's 12.8 MiB,
/// docs/ARCHITECTURE-DEBT.md 4.0); a request above it is clamped **and logged**,
/// so a program that genuinely needs more fails with the reason on the console
/// rather than mysteriously.
pub const LINUX_STACK_MAX_PAGES: usize = 16384;

/// How many stack pages to map for an image that asked for `want` bytes via
/// `PT_GNU_STACK` (0 = asked for nothing).
///
/// The loader used to ignore `PT_GNU_STACK` entirely and hand every Linux cell
/// [`LINUX_STACK_PAGES`], so an image asking for more silently got less and
/// overran - the failure landing far from its cause, in whatever function
/// happened to be deep enough (docs/ARCHITECTURE-DEBT.md 4.0). Reading the
/// header is the difference between a number that happens to fit today's
/// binaries and a mechanism that fits tomorrow's.
pub fn stack_pages_for(want: usize) -> usize {
    if want == 0 {
        return LINUX_STACK_PAGES;
    }
    let pages = want.div_ceil(FRAME_SIZE).max(LINUX_STACK_PAGES);
    if pages > LINUX_STACK_MAX_PAGES {
        crate::println!(
            "linux: PT_GNU_STACK asks {} KiB, clamped to the {} KiB ceiling \
             (stack is mapped eagerly and charged to the cell)",
            want / 1024,
            LINUX_STACK_MAX_PAGES * FRAME_SIZE / 1024
        );
        return LINUX_STACK_MAX_PAGES;
    }
    pages
}

/// The `[base, len)` of a cell's stack reservation, sized from the image's
/// `PT_GNU_STACK` request. `install_cell` registers this as an anonymous read-write
/// VMA so the pages below the top one grow on fault (docs/ARCHITECTURE-DEBT.md 4.0),
/// and a touch below `base` hits no VMA and becomes a SIGSEGV - the guard page.
pub fn reservation(img: &LinuxImage) -> (usize, usize) {
    let bytes = stack_pages_for(img.stack_want) * FRAME_SIZE;
    (USER_STACK_TOP - bytes, bytes)
}

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
    // Map **only the top page** - the one the kernel writes the initial process block
    // (argv/envp/auxv) into, which is asserted to fit one page below. The rest of the
    // stack is left to grow on fault: `install_cell` registers the whole span as an
    // anonymous VMA, so a touch below the top page faults into a fresh zeroed frame
    // through `linux::mem::fault`, and a touch below the *reservation* hits no VMA and
    // becomes a SIGSEGV - a guard page for free (docs/ARCHITECTURE-DEBT.md 4.0). The
    // request used to be mapped whole, so an image asking for a 64 MiB stack paid 64
    // MiB before `main`; now it pays one page plus what it touches.
    let sizing_note = stack_pages_for(img.stack_want); // logs a clamp if the image over-asks
    let _ = sizing_note;
    let top_pa = frames::alloc().expect("initial process stack top page (bounded, at load)");
    aspace.map_user_frame(USER_STACK_TOP - FRAME_SIZE, top_pa, MapPerm::UserRw);

    // Inject the rt_sigreturn trampoline page for the signal machinery
    // (docs/LINUX-COMPAT.md L5). ARM64/RISC-V have no SA_RESTORER path, so the
    // handler returns through this 2-instruction page; x86-64 uses the caller's
    // sa_restorer and returns an empty code slice, so nothing is mapped.
    let tramp = arch::sig_tramp_code();
    if !tramp.is_empty() {
        let pa = frames::alloc().expect("initial process stack (bounded, at load)");
        aspace.map_user_frame(arch::SIGTRAMP_VA, pa, MapPerm::UserRx);
        // SAFETY: freshly allocated frame; written through the kernel linear map
        // within `tramp.len()` bytes (« FRAME_SIZE).
        unsafe {
            core::ptr::copy_nonoverlapping(
                tramp.as_ptr(),
                arch::phys_to_virt(pa) as *mut u8,
                tramp.len(),
            );
        }
    }

    let base_va = USER_STACK_TOP - FRAME_SIZE;
    // SAFETY: freshly allocated, zeroed top frame; the kernel writes it through
    // its linear map (identity on x86/riscv; the high map on aarch64), only
    // within its FRAME_SIZE bytes (fit asserted below).
    let page = arch::phys_to_virt(top_pa) as *mut u8;

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

    // 16 random bytes for AT_RANDOM, just above the pointer block.
    let mut rnd = [0u8; 16];
    crate::rng::derive_cell_drbg().fill_bytes(&mut rnd);
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
    // AT_ENTRY is the main program's entry even for a dynamically-linked binary
    // (execution starts in ld.so, which jumps here after relocation, L7).
    push(AT_ENTRY, img.at_entry as u64);
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

    // Snapshot the finished auxv as the raw `/proc/self/auxv` byte stream (each
    // entry a pair of native-endian u64, AT_NULL last). glibc/rustix read
    // AT_EXECFN etc. from `/proc/self/auxv` when the kernel provides no
    // PR_GET_AUXV (docs/LINUX-COMPAT.md L3); the pointer values (AT_EXECFN,
    // AT_RANDOM) are user VAs into this same stack, valid while the cell runs.
    record_auxv(&auxv[..n]);

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

// ------------------------------------------------------- /proc/self/auxv

/// Serialized `/proc/self/auxv` bytes for the most recently built stack. Room
/// for the auxv this module emits (< 20 pairs * 16 B). Single cell installs at
/// a time (single CPU, synchronous), so `install_cell` copies this into the
/// cell's fd table immediately after `setup_stack`.
const AUXV_BYTES_MAX: usize = 20 * 16;
static mut LAST_AUXV: [u8; AUXV_BYTES_MAX] = [0; AUXV_BYTES_MAX];
static mut LAST_AUXV_LEN: usize = 0;

/// Serialize the `(type, value)` pairs into `LAST_AUXV`.
fn record_auxv(pairs: &[(u64, u64)]) {
    // SAFETY: single-threaded setup; `pairs.len() <= 20` (the push cap above).
    unsafe {
        let buf = &mut *core::ptr::addr_of_mut!(LAST_AUXV);
        let mut off = 0;
        for &(t, v) in pairs {
            buf[off..off + 8].copy_from_slice(&t.to_ne_bytes());
            buf[off + 8..off + 16].copy_from_slice(&v.to_ne_bytes());
            off += 16;
        }
        LAST_AUXV_LEN = off;
    }
}

/// The serialized auxv from the last `setup_stack` (for `/proc/self/auxv`).
pub fn last_auxv() -> &'static [u8] {
    // SAFETY: read of a buffer filled by the preceding `setup_stack`.
    unsafe {
        core::slice::from_raw_parts(core::ptr::addr_of!(LAST_AUXV) as *const u8, LAST_AUXV_LEN)
    }
}
