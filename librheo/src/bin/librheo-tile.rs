//! `librheo-tile` - the tile-framework proof program (docs/TILES.md). Runs as
//! a loaded cell with a real mapped queue pair and exits `0x42` only if every
//! stage passes:
//!
//! - **TileBuf + views**: dtype-tagged buffers over grants; an out-of-bounds
//!   tile view is a `None`, never a fault.
//! - **Tiled int8 GEMM** (128x128x128, block 16, 8 strands) on the
//!   [`CpuExecutor`]: asserted **elementwise** against an independent naive
//!   scalar reference computed in-cell, and the executor's FNV receipt must
//!   equal the FNV of that reference - two independent paths, one bit-exact
//!   answer.
//! - **TileSim determinism + exactness**: the simulator's counts equal the
//!   closed-form formulas, twice (identical runs).
//! - **Quantization round-trip**: f32 -> i8 (block 32) -> f32 within the
//!   per-block scale/2 error bound.
//! - **Reduce + copy + requantize**: exact receipts over known data.
//! - **Contracts + autotune key**: engine 0 is the measured CPU; two tilings
//!   of the same shapes produce different program hashes but the same shape
//!   class.
//!
//! The graph-path stage (EngineExecutor over `OP_GRAPH_SUBMIT` ops 4/5) is
//! added by the kernel slice (docs/TILES.md 6).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use core::sync::atomic::{AtomicI32, Ordering};

use librheo::mem::MemKind;
use librheo::tile::{
    self, CpuExecutor, Dtype, F32, I8, I32, TileBuf, TileProgram, TileShape, TileSim,
};
use librheo::{println, rt};

const OK_CODE: i32 = 0x42;
const N: usize = 128;
const BLOCK: TileShape = TileShape {
    m: 16,
    n: 16,
    k: 16,
};
const STRANDS: usize = 8;

static CODE: AtomicI32 = AtomicI32::new(0);

fn fail(c: i32) {
    if CODE.load(Ordering::Relaxed) == 0 {
        CODE.store(c, Ordering::Relaxed);
    }
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(work());
    let code = CODE.load(Ordering::Relaxed);
    if code != 0 {
        return code;
    }
    println!(
        "librheo-tile: tile framework OK ({N}x{N}x{N} block {}x8 strands)",
        BLOCK.m
    );
    OK_CODE
}

async fn work() {
    // ---- TileBuf + checked views -------------------------------------
    let mut a: TileBuf<I8> = match TileBuf::alloc(MemKind::Ddr, N, N) {
        Some(b) => b,
        None => return fail(10),
    };
    let mut b: TileBuf<I8> = match TileBuf::alloc(MemKind::Ddr, N, N) {
        Some(b) => b,
        None => return fail(11),
    };
    let c: TileBuf<I32> = match TileBuf::alloc(MemKind::Ddr, N, N) {
        Some(b) => b,
        None => return fail(12),
    };
    // Deterministic inputs (the same formulas every ISA).
    a.fill_with(|i, j| ((i * 31 + j * 7) & 0xFF) as u8 as i8);
    b.fill_with(|i, j| ((i * 17 + j * 13) & 0xFF) as u8 as i8);

    // Views: in-bounds works, out-of-bounds is None (not a fault).
    if a.tile(0, 0, 16, 16).is_none() {
        return fail(13);
    }
    if a.tile(N - 8, N - 8, 16, 16).is_some() {
        return fail(14);
    }

    // ---- Tiled GEMM on the CpuExecutor vs the naive reference ---------
    let mut prog = TileProgram::new();
    let pa = prog.bind(&a, tile::Space::Host);
    let pb = prog.bind(&b, tile::Space::Host);
    let pc = prog.bind(&c, tile::Space::Host);
    if prog.gemm(pa, pb, pc, BLOCK).is_err() {
        return fail(20);
    }
    let exec = CpuExecutor { strands: STRANDS };
    let report = match exec.run(&prog).await {
        Ok(r) => r,
        Err(_) => return fail(21),
    };

    // Independent naive reference (plain triple loop, no tiling, no strands).
    let asl = a.as_slice();
    let bsl = b.as_slice();
    let mut reference = vec![0i32; N * N];
    for i in 0..N {
        for p in 0..N {
            let av = asl[i * a.stride() + p] as i32;
            for j in 0..N {
                reference[i * N + j] += av * (bsl[p * b.stride() + j] as i32);
            }
        }
    }
    let csl = c.as_slice();
    for i in 0..N {
        for j in 0..N {
            if csl[i * c.stride() + j] != reference[i * N + j] {
                return fail(22);
            }
        }
    }
    // The executor's receipt equals the FNV of the reference bytes.
    let ref_bytes =
        unsafe { core::slice::from_raw_parts(reference.as_ptr() as *const u8, N * N * 4) };
    if report.checksums.first().copied() != Some(tile::kernels::fnv1a(ref_bytes)) {
        return fail(23);
    }

    // ---- TileSim: exact closed forms, deterministic -------------------
    let contract = match tile::contract_for(0) {
        Some(ct) => ct,
        None => return fail(30),
    };
    let sim1 = TileSim::simulate(&prog, &contract);
    let sim2 = TileSim::simulate(&prog, &contract);
    if sim1 != sim2 {
        return fail(31);
    }
    let t = (N / 16) as u64; // 8 tiles per axis at block 16
    if sim1.mac_ops != (N * N * N) as u64
        || sim1.mma_tiles != t * t * t
        || sim1.tile_trips != t * t * t
        || sim1.yields != t * t
        || sim1.bytes_in != (2 * N * N) as u64
        || sim1.bytes_out != (N * N * 4) as u64
        || sim1.bytes_staged != t * t * t * (16 * 16 * 2) + t * t * (16 * 16 * 4)
    {
        return fail(32);
    }

    // ---- Contracts + the autotune key ---------------------------------
    if contract.kind != librheo::compute::EngineKind::Cpu || !contract.measured {
        return fail(33);
    }
    if !contract
        .mma
        .iter()
        .any(|(d, s)| *d == Dtype::I8 && s.m == 16)
    {
        return fail(34);
    }
    let mut prog2 = TileProgram::new();
    let qa = prog2.bind(&a, tile::Space::Host);
    let qb = prog2.bind(&b, tile::Space::Host);
    let qc = prog2.bind(&c, tile::Space::Host);
    if prog2
        .gemm(
            qa,
            qb,
            qc,
            TileShape {
                m: 32,
                n: 32,
                k: 32,
            },
        )
        .is_err()
    {
        return fail(35);
    }
    let k1 = tile::autotune_key(&prog, &contract);
    let k2 = tile::autotune_key(&prog2, &contract);
    if k1.program == k2.program {
        return fail(36); // different tilings must differ in program identity
    }
    if k1.shape_class != k2.shape_class {
        return fail(37); // ... but the same shapes share a shape class
    }

    // ---- Reduce, copy, requantize (exact receipts) ---------------------
    let mut src: TileBuf<I32> = match TileBuf::alloc(MemKind::Ddr, 64, 64) {
        Some(b) => b,
        None => return fail(40),
    };
    src.fill_with(|i, j| (i * 64 + j) as i32);
    let dst: TileBuf<I32> = match TileBuf::alloc(MemKind::Ddr, 64, 64) {
        Some(b) => b,
        None => return fail(41),
    };
    let qdst: TileBuf<I8> = match TileBuf::alloc(MemKind::Ddr, 64, 64) {
        Some(b) => b,
        None => return fail(42),
    };
    let mut p3 = TileProgram::new();
    let s3 = p3.bind(&src, tile::Space::Host);
    let d3 = p3.bind(&dst, tile::Space::Host);
    let q3 = p3.bind(&qdst, tile::Space::Host);
    if p3.copy(s3, d3).is_err() || p3.map_shift_clamp(d3, q3, 4).is_err() || p3.reduce(d3).is_err()
    {
        return fail(43);
    }
    let r3 = match exec.run(&p3).await {
        Ok(r) => r,
        Err(_) => return fail(44),
    };
    // SUM 0..4095 = 4095*4096/2.
    if r3.reduced != (4095u64 * 4096) / 2 {
        return fail(45);
    }
    // The requantize clamps: element (63,63) = 4095 >> 4 = 255 -> clamped 127.
    if qdst.as_slice()[63 * qdst.stride() + 63] != 127 {
        return fail(46);
    }

    // ---- Quantization round-trip ---------------------------------------
    let mut fsrc: TileBuf<F32> = match TileBuf::alloc(MemKind::Ddr, 16, 32) {
        Some(b) => b,
        None => return fail(50),
    };
    fsrc.fill_with(|i, j| ((i * 32 + j) as f32) * 0.37 - 80.0);
    let qbuf: TileBuf<I8> = match TileBuf::alloc(MemKind::Ddr, 16, 32) {
        Some(b) => b,
        None => return fail(51),
    };
    let scales: TileBuf<F32> = match TileBuf::alloc(MemKind::Ddr, 1, 16) {
        Some(b) => b,
        None => return fail(52),
    };
    let fdst: TileBuf<F32> = match TileBuf::alloc(MemKind::Ddr, 16, 32) {
        Some(b) => b,
        None => return fail(53),
    };
    let mut p4 = TileProgram::new();
    let fs = p4.bind(&fsrc, tile::Space::Host);
    let qb4 = p4.bind(&qbuf, tile::Space::Host);
    let sc4 = p4.bind(&scales, tile::Space::Host);
    let fd4 = p4.bind(&fdst, tile::Space::Host);
    if p4.quant(fs, qb4, sc4, 32).is_err() || p4.dequant(qb4, sc4, fd4, 32).is_err() {
        return fail(54);
    }
    if exec.run(&p4).await.is_err() {
        return fail(55);
    }
    let orig = fsrc.as_slice();
    let back = fdst.as_slice();
    let scl = scales.as_slice();
    for i in 0..16 * 32 {
        let err = orig[i] - back[i];
        let err = if err < 0.0 { -err } else { err };
        let bound = scl[i / 32] * 0.5 + 1e-4;
        if err > bound {
            return fail(56);
        }
    }
}
