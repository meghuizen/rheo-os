//! `tilelinux` - the **tile framework's own kernels, run by an unmodified Linux
//! binary** on rheo-os (docs/TILES.md 13.4b, docs/LINUX-COMPAT.md).
//!
//! ## What this settles
//!
//! The tile framework lives in `librheo`, the OS's native userspace library, and every
//! proof of it so far ran in a librheo cell. Node, Bun and Claude Code are not librheo
//! cells - they are `Personality::Linux` cells speaking the Linux syscall ABI - so
//! "the tile structure works" and "real Linux binaries run" were two claims about two
//! substrates with nothing joining them.
//!
//! This binary joins them at the only place they can honestly be joined: the tile
//! **kernels** are dependency-free Rust, so the same source that the librheo executor
//! and the kernel's own compute engine compile is `#[path]`-included here and compiled
//! into a static-glibc Linux program instead. Same source, different substrate. The
//! test kernel runs it as a `Personality::Linux` cell and compares its output, byte for
//! byte, against the librheo cell's - so if the two substrates ever disagree about the
//! arithmetic, that is a defect rather than a footnote.
//!
//! That is a claim about the *kernels* and the ABI beneath them, not about Node: Node
//! does not call these functions, and this does not pretend it does. What it shows is
//! that the tile programs need nothing librheo provides that the Linux personality
//! cannot - no queue pair, no typed grant, no native verb - which is the property that
//! makes the tile framework usable from the compatibility substrate at all.
//!
//! ## Output
//!
//! Two lines from a fixed set, so the transcript stays exact:
//!   `tilelinux: gemm <hex>` - an FNV-1a hash of the whole int8 -> i32 GEMM output.
//!   `tilelinux: attn <hex>` - the same over the FlashAttention 2 output's raw bits.
//! Hashes rather than the arrays themselves: 2 KiB of f32 on a serial console is not a
//! transcript, and the hash is exact - a single wrong bit changes it.

// The shared tile sources, `#[path]`-included from librheo exactly as `kernel/engine.rs`
// and `bench-core` include them. At the **crate root**, not nested, so `attn`'s
// `super::fmath` resolves here - and so each `#[path]` is relative to `src/`.
#[path = "../../../../librheo/src/tile/fmath.rs"]
#[allow(dead_code)]
mod fmath;
#[path = "../../../../librheo/src/tile/kernels.rs"]
#[allow(dead_code)]
mod kernels;
#[path = "../../../../librheo/src/tile/attn.rs"]
#[allow(dead_code)]
mod attn;

use attn::{AttnShape, flash_attention_2};

// Shapes and input formulas, identical to `librheo/src/bin/librheo-fa.rs`. Two programs
// deriving the same inputs from the same index arithmetic is what makes comparing their
// outputs meaningful.
const TQ: usize = 32;
const TK: usize = 128;
const D: usize = 32;
const BLOCK_K: usize = 32;
const GM: usize = 32;
const GN: usize = 32;
const GK: usize = 32;

fn fill(buf: &mut [f32], salt: u32) {
    for (i, x) in buf.iter_mut().enumerate() {
        let h = (i as u32)
            .wrapping_mul(2_654_435_761)
            .wrapping_add(salt.wrapping_mul(97))
            ^ 0x9E37_79B9;
        *x = ((h >> 20) as i32 - 2048) as f32 * (1.0 / 512.0);
    }
}

fn fill_i8(buf: &mut [i8], salt: u32) {
    for (i, x) in buf.iter_mut().enumerate() {
        let h = (i as u32)
            .wrapping_mul(2_246_822_519)
            .wrapping_add(salt.wrapping_mul(31));
        *x = ((h >> 17) & 0x7F) as i8 - 64;
    }
}

/// FNV-1a over raw bytes. Chosen because the *kernel side* computes the same hash over
/// the librheo cell's output with the same constants, so the comparison is one `u64`
/// rather than a shared page.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn main() {
    // --- the tiled int8 -> i32 GEMM ---
    let mut a = vec![0i8; GM * GK];
    let mut b = vec![0i8; GK * GN];
    fill_i8(&mut a, 5);
    fill_i8(&mut b, 7);
    let mut c = vec![0i32; GM * GN];
    // SAFETY: `a`/`b` cover the full operands and `c` covers exactly `GM * GN`.
    unsafe {
        kernels::gemm_i8_i32(
            a.as_ptr(),
            GK,
            b.as_ptr(),
            GN,
            c.as_mut_ptr(),
            GN,
            GM,
            GN,
            GK,
        );
    }
    let gbytes: Vec<u8> = c.iter().flat_map(|v| v.to_le_bytes()).collect();
    println!("tilelinux: gemm {:016x}", fnv(&gbytes));

    // --- FlashAttention 2 over the whole head ---
    let mut q = vec![0.0f32; TQ * D];
    let mut k = vec![0.0f32; TK * D];
    let mut v = vec![0.0f32; TK * D];
    fill(&mut q, 1);
    fill(&mut k, 2);
    fill(&mut v, 3);
    let shape = AttnShape { tq: TQ, tk: TK, d: D };
    let scale = shape.scale();
    let mut s = vec![0.0f32; BLOCK_K];
    let mut acc = vec![0.0f32; D];
    let mut o = vec![0.0f32; TQ * D];
    if flash_attention_2(
        &q, &k, &v, &mut o, shape, scale, BLOCK_K, &mut s, &mut acc,
    )
    .is_err()
    {
        println!("tilelinux: attn FAILED");
        std::process::exit(1);
    }
    let obytes: Vec<u8> = o.iter().flat_map(|x| x.to_bits().to_le_bytes()).collect();
    println!("tilelinux: attn {:016x}", fnv(&obytes));
}
