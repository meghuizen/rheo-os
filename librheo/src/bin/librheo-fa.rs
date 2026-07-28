//! `librheo-fa` - **FlashAttention 2 over one slice of the query rows**, so a set of
//! these cells computes one attention head across several CPUs at once
//! (docs/TILES.md 13, docs/SUBSTRATE.md pillar 3).
//!
//! ## Why a cell and not a kernel work queue
//!
//! The tile GEMM is drained by every core *in kernel context*, because it is integer.
//! FlashAttention is not: its softmax needs a real `exp`, so it is f32 - and the kernel
//! is deliberately FP-free (docs/SUBSTRATE.md pillar 4: if the kernel never executes an
//! FP instruction, no syscall, trap or interrupt has to save the vector file). The
//! `.user`-window programs cannot host it either - they are built as part of the
//! soft-float kernel crate, and soft-float f32 means out-of-line calls into kernel
//! `.text`, which a cell has no mapping for.
//!
//! So the parallel unit here is a **loaded, hard-float ELF cell**, which is what
//! librheo cells already are. Several are installed and handed to the kernel's cell
//! placement, and whichever core is free claims one.
//!
//! ## Why the query-row split is exact
//!
//! Output row `i` of attention depends only on query row `i` (and on all of K and V).
//! Splitting by query rows therefore changes nothing about any row's arithmetic - not
//! even the summation order, unlike splitting the K/V loop - so N cells each doing a
//! slice must produce a result **bit-identical** to one cell doing all of them. That is
//! the property `librheo-tilebattle` already asserts single-threaded ("FA2 decomposed
//! over 4 query-row chunks matches the whole-batch result"); this cell is that
//! decomposition executed on separate CPUs.
//!
//! ## The contract with the test kernel
//!
//! Two pages are mapped in by the launcher at fixed addresses, because a placed cell is
//! given no argv:
//!
//!   - `PARAMS_VA`: a [`Params`] block - which rows this cell owns, and a status word it
//!     sets so the kernel can tell "ran and finished" from "ran and faulted".
//!   - `OUT_VA`: the shared `Tq x d` f32 output. Each cell writes **only its own rows**,
//!     so the cells need no lock and no coordination at all; that disjointness is what
//!     makes the parallelism sound rather than lucky.
//!
//! Q, K and V are **generated in the cell** from a fixed formula rather than mapped in,
//! so every cell derives byte-identical inputs and the launcher has one less shared
//! region to get right.

#![no_std]
#![no_main]

extern crate alloc;

use librheo::tile::attn::{AttnShape, flash_attention_2, flash_attention_3};

/// Shape of the head. Small enough that several cells fit the boot-test budget under
/// TCG, large enough that a slice is real work and `Tk` needs several K/V blocks.
const TQ: usize = 32;
const TK: usize = 128;
const D: usize = 32;
/// K/V block width for the online-softmax loop.
const BLOCK_K: usize = 32;

/// Where the launcher maps this cell's parameter page and the shared output.
/// Deliberately inside the anonymous-`mmap` window, which no librheo start-up path
/// touches, and page-aligned.
const PARAMS_VA: usize = 0x3_4000_0000;
const OUT_VA: usize = 0x3_4001_0000;

/// What the launcher writes at [`PARAMS_VA`] and reads back after the run.
#[repr(C)]
struct Params {
    /// First query row this cell owns.
    lo: u32,
    /// One past the last.
    hi: u32,
    /// Set to 1 by the cell once every one of its rows is written. The launcher
    /// asserts it, so "the cell exited 0" and "the cell did the work" stay separate
    /// claims.
    status: u32,
    /// Rows this cell actually wrote, so a slice that silently did nothing is visible.
    rows: u32,
}

/// Deterministic Q/K/V, identical in every cell. Integer-derived and then scaled, so
/// there is no accumulation of rounding differences between cells - each derives the
/// exact same bits.
fn fill(buf: &mut [f32], salt: u32) {
    for (i, x) in buf.iter_mut().enumerate() {
        let h = (i as u32)
            .wrapping_mul(2_654_435_761)
            .wrapping_add(salt.wrapping_mul(97))
            ^ 0x9E37_79B9;
        // A small signed range: attention inputs near zero keep the softmax in the
        // part of its domain where the block decomposition is interesting (no single
        // key dominating every row).
        *x = ((h >> 20) as i32 - 2048) as f32 * (1.0 / 512.0);
    }
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    // SAFETY: the launcher mapped a writable, `Params`-aligned page at `PARAMS_VA` and
    // `TQ * D` f32s at `OUT_VA` before this cell was entered.
    let p = PARAMS_VA as *mut Params;
    let (lo, hi) = unsafe { ((*p).lo as usize, (*p).hi as usize) };
    if hi > TQ || lo > hi {
        return 2;
    }

    // Inputs, generated rather than shared. All of K and V, because every query row
    // attends to the whole context; only this cell's slice of Q is needed, but
    // generating all of it keeps the formula index-based and therefore identical
    // across cells.
    let mut q = alloc::vec![0.0f32; TQ * D];
    let mut k = alloc::vec![0.0f32; TK * D];
    let mut v = alloc::vec![0.0f32; TK * D];
    fill(&mut q, 1);
    fill(&mut k, 2);
    fill(&mut v, 3);

    let shape = AttnShape {
        tq: hi - lo,
        tk: TK,
        d: D,
    };
    let scale = AttnShape {
        tq: TQ,
        tk: TK,
        d: D,
    }
    .scale();

    // Scratch for the online softmax: one K block of scores and one accumulator row.
    let mut s = alloc::vec![0.0f32; BLOCK_K];
    let mut acc = alloc::vec![0.0f32; D];
    let mut out = alloc::vec![0.0f32; (hi - lo) * D];

    if flash_attention_2(
        &q[lo * D..hi * D],
        &k,
        &v,
        &mut out,
        shape,
        scale,
        BLOCK_K,
        &mut s,
        &mut acc,
    )
    .is_err()
    {
        return 3;
    }

    // **FA3 on the same slice, and it must agree bit-for-bit.** FA3 is the same
    // arithmetic pipelined over a double-buffered staging pair, so agreement is an
    // equality rather than a tolerance - and checking it *here*, inside the cell that
    // is running on some core the launcher did not choose, is what makes the parallel
    // proof cover FA3 as well as FA2. The staging-swap count is counted rather than
    // assumed, so a pipeline that degenerated to a single block would be visible.
    let mut out3 = alloc::vec![0.0f32; (hi - lo) * D];
    // **Two** blocks each: the pipeline consumes one buffer while staging into the
    // other, which is what makes it a pipeline rather than the same loop with a copy in
    // it. Sizing these for one block is refused outright (`ScratchTooSmall`) rather than
    // silently degenerating.
    let mut stage_k = alloc::vec![0.0f32; 2 * BLOCK_K * D];
    let mut stage_v = alloc::vec![0.0f32; 2 * BLOCK_K * D];
    let mut swaps = 0usize;
    if flash_attention_3(
        &q[lo * D..hi * D],
        &k,
        &v,
        &mut out3,
        shape,
        scale,
        BLOCK_K,
        &mut s,
        &mut acc,
        &mut stage_k,
        &mut stage_v,
        |_| swaps += 1,
    )
    .is_err()
    {
        return 4;
    }
    // One swap per (row, K block): a slice of `hi - lo` rows over `TK / BLOCK_K` blocks.
    if swaps != (hi - lo) * (TK / BLOCK_K) {
        return 5;
    }
    for (a, b) in out.iter().zip(out3.iter()) {
        if a.to_bits() != b.to_bits() {
            return 6;
        }
    }

    // Publish **only this cell's rows**. Disjoint by construction, so no lock.
    // SAFETY: `OUT_VA` is a writable mapping of at least `TQ * D` f32s, and this cell
    // writes strictly inside `[lo, hi)`.
    unsafe {
        let dst = OUT_VA as *mut f32;
        for (n, val) in out.iter().enumerate() {
            dst.add(lo * D + n).write(*val);
        }
        (*p).rows = (hi - lo) as u32;
        (*p).status = 1;
    }
    // Exit code carries the slice, so a finished run ties back to the cell that
    // produced it even though placement, not the launcher, chose the core.
    (lo + 1) as i32
}
