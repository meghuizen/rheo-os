//! ELF indirect functions (`STT_GNU_IFUNC`) for librheo cells - the glibc
//! mechanism, on this kernel's terms (docs/LIBRHEO.md "ifunc").
//!
//! # Why this exists
//!
//! Runtime CPU dispatch used to be a branch on a global: read a tier byte, test
//! it, switch to an implementation. That is correct and it is what
//! [`crate::tile::simd`] does, but it puts a load and two branches *inside every
//! call*. For a tile GEMM, amortised over `m*n*k` work, that is noise. For a
//! leaf called once per byte - `memcpy`, `exp2f` - it is a measurable fraction
//! of the function.
//!
//! An **ifunc** moves the choice to process start. The symbol is declared
//! `STT_GNU_IFUNC` and aliased to a *resolver*; the linker turns every call into
//! a PLT stub that jumps through one GOT slot and emits an `R_*_IRELATIVE`
//! relocation naming the resolver. Something must then run the resolvers and
//! fill the slots. On a Linux system that is `ld.so`, or - for a static binary -
//! glibc's own `ARCH_SETUP_IREL` walking `__rela_iplt_start..__rela_iplt_end`
//! before `main`. A librheo cell is a statically linked `ET_EXEC`, so it is the
//! second case, and [`apply_irel`] is that walk.
//!
//! # What this does NOT need
//!
//! **No kernel change, no kernel object, no new verb** (docs/ARCHITECTURE.md 6).
//! The kernel loader deliberately performs no relocation processing - that
//! property is what keeps `ld.so` in userspace where it belongs
//! (docs/LINUX-COMPAT.md L7) - and this does not ask it to. The relocations live
//! in an ordinary `PT_LOAD` the loader already maps, and the cell applies them to
//! itself with its own instructions. A cell that declares no ifunc symbol has an
//! empty array here and pays one compare.
//!
//! # Verified properties of the toolchain (not assumed)
//!
//! Measured against the pinned nightly, for all three cell targets:
//!
//! - `%gnu_indirect_function` is accepted by the integrated assembler on
//!   **x86-64, ARM64 and RISC-V alike**, so the declaration needs no `cfg`.
//!   (`@gnu_indirect_function` is *not* portable - `@` begins a comment in the
//!   ARM assembler.)
//! - `rust-lld` synthesises `__rela_iplt_start` / `__rela_iplt_end` and emits
//!   `R_X86_64_IRELATIVE` (37) / `R_AARCH64_IRELATIVE` (1032) /
//!   `R_RISCV_IRELATIVE` (58).
//! - The GOT slot lands in a **writable** `PT_LOAD`. It is also covered by
//!   `PT_GNU_RELRO`, which this loader does not honour - so it stays writable at
//!   the moment [`apply_irel`] runs. **If the loader ever enforces RELRO, this
//!   walk must run before that enforcement**, which is the same ordering
//!   constraint `ld.so` is under.
//! - librheo cells link as `ET_EXEC` at a fixed address, so the load bias is
//!   **zero** and `r_offset` / `r_addend` are absolute. This is the one
//!   assumption here that a change elsewhere could invalidate, and it is
//!   **unchecked**: a cell relinked as `ET_DYN` would need the bias added to
//!   both fields, and would fault here rather than fail cleanly. Stated rather
//!   than guarded, because the loader would have to start supplying a bias
//!   before there is anything to guard against.
//!
//! # The resolver environment
//!
//! A resolver runs *before the heap exists*. It may not allocate, may not use
//! the DRBG, and must not call any ifunc-dispatched function (including
//! `memcpy` - so no large struct returns or slice copies). It may issue a
//! syscall: [`crate::sys::cpu_features`] is a plain trap needing no cell state,
//! which is what lets a resolver ask the kernel's *validated* feature report
//! rather than executing `CPUID` itself. This mirrors glibc, whose resolvers run
//! before TLS and malloc are up.
//!
//! Resolvers here go one step further than glibc's, in keeping with
//! docs/ENGINEERING.md 1: a tier is selected only after being *observed* to
//! produce the reference answer, using fixed stack buffers so the check needs no
//! allocation. A feature bit says the instruction exists; it does not say this
//! implementation of it is right.

use core::sync::atomic::{AtomicUsize, Ordering};

/// `Elf64_Rela` is three 64-bit words - `r_offset`, `r_info`, `r_addend` -
/// identically on all three targets. Held as a word count rather than a `struct`
/// because every read here is deliberately word-at-a-time (see [`apply_irel`]).
const RELA_WORDS: usize = 3;
/// `size_of::<Elf64_Rela>()`.
const RELA_SIZE: usize = RELA_WORDS * core::mem::size_of::<usize>();

/// `R_*_IRELATIVE` for this ISA. The relocation that says "call the function at
/// `r_addend` and store what it returns at `r_offset`". The three values are
/// unrelated numbers in three separate ABIs, which is why this is the one `cfg`
/// in the module (docs/TARGET-ARCHITECTURES.md 4.1).
#[cfg(target_arch = "x86_64")]
const R_IRELATIVE: usize = 37;
#[cfg(target_arch = "aarch64")]
const R_IRELATIVE: usize = 1032;
#[cfg(target_arch = "riscv64")]
const R_IRELATIVE: usize = 58;

// The bounds of the IRELATIVE array, reached through a table of two **absolute**
// 64-bit words rather than by referencing the linker's symbols directly.
//
// This indirection is not stylistic. When an image declares no ifunc symbol at
// all - which is every librheo cell today, and `librheo-embed` forever - LLD
// still defines `__rela_iplt_start` / `__rela_iplt_end`, as **absolute zero**.
// Rust then addresses them PC-relative, and a PC-relative reference from `.text`
// to address 0 does not reach:
//
//   relocation R_RISCV_PCREL_HI20 out of range: -1048576 is not in
//   [-524288, 524287]; references '__rela_iplt_end'
//
// A `.quad` of the symbol emits an absolute 64-bit relocation instead, which has
// no range to exceed and represents zero perfectly well. The `.weak`
// declarations are glibc's own belt-and-braces for the same case (a linker that
// leaves them undefined rather than zero resolves them to 0 too).
core::arch::global_asm!(
    ".weak __rela_iplt_start",
    ".weak __rela_iplt_end",
    ".pushsection .rodata.rheo_irel, \"a\"",
    ".p2align 3",
    ".globl __rheo_irel_bounds",
    ".hidden __rheo_irel_bounds",
    "__rheo_irel_bounds:",
    ".quad __rela_iplt_start",
    ".quad __rela_iplt_end",
    ".popsection",
);

unsafe extern "C" {
    /// `[start, end)` of the IRELATIVE relocation array as absolute addresses.
    /// Both zero in a cell that declares no ifunc.
    static __rheo_irel_bounds: [usize; 2];
}

/// Read the relocation array bounds. `(0, 0)` when the image has no ifunc.
fn bounds() -> (usize, usize) {
    // SAFETY: a 16-byte aligned pair of words the linker filled in this image's
    // own `.rodata`; read-only and always present.
    let b = unsafe { core::ptr::read(&raw const __rheo_irel_bounds) };
    (b[0], b[1])
}

/// Number of relocations [`apply_irel`] has applied. The observable a test
/// asserts on: a cell whose ifunc symbols resolved reports a nonzero count, and
/// it is a count of *work done*, not of entries seen - a skipped entry is not
/// counted.
static APPLIED: AtomicUsize = AtomicUsize::new(0);

/// How many IRELATIVE relocations have been applied in this cell.
pub fn applied() -> usize {
    APPLIED.load(Ordering::Relaxed)
}

/// How many IRELATIVE relocations this image carries - read from the linker's
/// own array bounds, so it is the *expected* count against which [`applied`] is
/// checked. Zero for a cell with no ifunc symbols.
pub fn pending() -> usize {
    let (start, end) = bounds();
    if end <= start {
        return 0;
    }
    (end - start) / RELA_SIZE
}

/// Resolve every `STT_GNU_IFUNC` symbol in this image: for each IRELATIVE
/// relocation, call the resolver named by `r_addend` and store the address it
/// returns into the GOT slot at `r_offset`.
///
/// Called once, first, by the crt0 (`start.rs`) - **before the heap, the DRBG,
/// the capability set or the reactor**, because any of those may call a function
/// that is itself dispatched here. Until this runs, calling an ifunc symbol
/// jumps through an unfilled slot.
///
/// Idempotent: resolvers are pure, so a second call recomputes the same
/// addresses. It is nonetheless driven from vcore 0 only, so the stores race
/// nothing (docs/SMP.md 10.0a).
///
/// # Safety
///
/// Must be called exactly once per process, from the crt0, before any
/// ifunc-dispatched function is called and before any other cell state is
/// brought up. The caller guarantees no sibling vcore is inside `main` yet.
pub unsafe fn apply_irel() {
    let (start, end) = bounds();
    if end <= start {
        return; // no ifunc symbols in this cell
    }
    let n = (end - start) / RELA_SIZE;
    let mut applied = 0usize;
    for i in 0..n {
        // Read the three fields as individual words rather than copying the
        // `Rela` struct. A 24-byte struct load is free to lower to a `memcpy`
        // call - and `memcpy` is one of the symbols this loop exists to resolve,
        // so that lowering would jump through the very slot that is still empty.
        // Three explicit loads cannot.
        //
        // SAFETY: `[start, end)` is the linker-synthesised relocation array,
        // laid out as `Elf64_Rela` and inside a mapped read-only PT_LOAD.
        let e = unsafe { (start as *const usize).add(i * RELA_WORDS) };
        let (r_offset, r_info, r_addend) = unsafe { (e.read(), e.add(1).read(), e.add(2).read()) };
        // The low 32 bits of `r_info` are the type. Anything that is not
        // IRELATIVE does not belong to us: skip rather than guess, so a future
        // linker that widens this array cannot make us call an arbitrary word.
        if r_info & 0xFFFF_FFFF != R_IRELATIVE {
            continue;
        }
        // SAFETY: `r_addend` is a resolver address the linker wrote from a
        // symbol in this image's `.text`; `r_offset` is the GOT slot it wrote
        // for the same symbol, in this image's writable PT_LOAD. Both are
        // absolute because the cell is a fixed-address `ET_EXEC` (bias 0).
        unsafe {
            let resolver: extern "C" fn() -> usize = core::mem::transmute(r_addend);
            let target = resolver();
            core::ptr::write(r_offset as *mut usize, target);
        }
        applied += 1;
    }
    APPLIED.store(applied, Ordering::Relaxed);
}

/// Declare an ELF indirect function.
///
/// Emits a `STT_GNU_IFUNC` symbol `$name` aliased to `$resolver`, so the linker
/// routes every call to `$name` through a PLT stub and one GOT slot, and emits
/// the `R_*_IRELATIVE` relocation [`apply_irel`] consumes. The Rust-visible
/// declaration is a normal `extern "C"` function - call sites are unaware.
///
/// `$resolver` must be an `#[unsafe(no_mangle)] extern "C" fn() -> usize`
/// returning the address of the chosen implementation, and must obey the
/// resolver environment documented at the module level (no allocation, no
/// ifunc-dispatched calls).
///
/// `%gnu_indirect_function` rather than `@...`: `@` opens a comment in the ARM
/// assembler, and `%` is accepted by all three (verified, module docs).
#[macro_export]
macro_rules! ifunc {
    ($name:ident => $resolver:ident) => {
        core::arch::global_asm!(
            concat!(".globl ", stringify!($name)),
            concat!(".type ", stringify!($name), ", %gnu_indirect_function"),
            concat!(".set ", stringify!($name), ", ", stringify!($resolver)),
        );
    };
}
