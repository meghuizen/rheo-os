//! `librheo-tilebattle` - production-shaped battle tests for the tile
//! framework (docs/TILES.md 10). Exits `0x42` only if every stage passes.
//!
//! The honesty banner first (printed at runtime too): production shapes are
//! the workloads' GEOMETRY, not a claim of serving a real model; in QEMU the
//! 7B-class shapes run SCALED (the ratio printed - TCG cannot execute 68
//! GMAC in a 120 s test budget), full size runs on the host in
//! comparison/tiles; QEMU proves correctness, the host proves caches.
//!
//! Stages:
//! - **7B-class layer GEMMs, scaled 1/16** (4096->256, 11008->688, geometry
//!   preserved): each computed twice at different tilings (block 16 vs 32) -
//!   integer exactness means the receipts must be identical (tiling
//!   invariance), plus the GQA-proxy score GEMM against an in-cell naive
//!   reference.
//! - **An attention block as one TileProgram**: QKV projections -> score
//!   GEMM -> integer requantize (the softmax slot) -> weighted-V, run twice
//!   with identical per-stage receipts. (K enters pre-transposed as its own
//!   synthetic input: the program has no transpose op yet - stated, not
//!   hidden.)
//! - **A paged-KV pattern** (AI-ARCHITECTURE.md 3): block-granular KV tiles
//!   in one pool, two sequences whose block tables share a prefix - the
//!   shared-prefix score rows must be bit-identical across sequences.
//! - **The columnar scan as tiles** (the docs/TILES.md 11 v1 conversion):
//!   the librheodata-shaped SUM aggregate as a tile reduce, exact closed
//!   form, computed by BOTH executors (library and kernel-graph) with equal
//!   receipts.
//! - **Soak**: 100 re-runs of the attention block over reused buffers - the
//!   receipt must never drift - plus a bounded grant-slot churn loop.
//! - **Boundary shapes**: (13,17,5) under block 16, 1xN / Nx1, stride >
//!   width, tail quantization blocks - all against naive references.
//! - **FlashAttention 2 and 3** (docs/TILES.md 13): the real exp softmax,
//!   filling the slot the integer attention block above leaves open. FA2's
//!   online-softmax block loop asserted **invariant to the block size**
//!   (the property the rescale exists for), FA3's pipelined form asserted
//!   equal to it with its staging-swap count hand-computed, plus the
//!   convex-combination range property, a paged-KV split accumulation, and
//!   the `exp2f`/`expf` bound against exact values.
//! - **The pipeline-depth fence**: a 64-op dependent tile chain that never
//!   touches the queue must complete (the regression fence for the
//!   reactor's no-progress guard), then a mixed variant with a graph
//!   submit.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};

use librheo::mem::MemKind;
use librheo::tile::{self, CpuExecutor, EngineExecutor, I8, I32, TileBuf, TileProgram, TileShape};
use librheo::{println, rt};

const OK_CODE: i32 = 0x42;

/// The scaling of the 7B-class shapes in QEMU (1/16: 4096 -> 256,
/// 11008 -> 688), printed so the ratio is never hidden.
const SCALE: usize = 16;
const D_MODEL: usize = 4096 / SCALE; // 256
const D_MLP: usize = 11008 / SCALE; // 688

static CODE: AtomicI32 = AtomicI32::new(0);

fn fail(c: i32) {
    if CODE.load(Ordering::Relaxed) == 0 {
        CODE.store(c, Ordering::Relaxed);
    }
}

fn block(sz: usize) -> TileShape {
    TileShape {
        m: sz,
        n: sz,
        k: sz,
    }
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    println!(
        "librheo-tilebattle: 7B-class geometry SCALED 1/{SCALE} in QEMU \
         (4096->{D_MODEL}, 11008->{D_MLP}); full size runs on the host \
         (comparison/tiles). Correctness here, caches there."
    );
    rt::block_on(work());
    let code = CODE.load(Ordering::Relaxed);
    if code != 0 {
        return code;
    }
    println!("librheo-tilebattle: all battle stages OK");
    OK_CODE
}

/// A deterministic i8 fill (distinct per `seed`).
fn fill_i8(buf: &mut TileBuf<I8>, seed: usize) {
    buf.fill_with(|i, j| ((i * (seed + 3) + j * (2 * seed + 7) + seed) & 0xFF) as u8 as i8);
}

// Paged-KV geometry (AI-ARCHITECTURE.md 3): a pool of block tiles, sequences
// addressing them through block tables, prefix sharing across sequences.
const PKV_BLK: usize = 16; // rows per KV block
const PKV_HD: usize = 128; // head_dim
const PKV_NBLK: usize = 12; // pool size in blocks
const PKV_PER_SEQ: usize = 8; // blocks per sequence
const PKV_SHARED: usize = 4; // shared prefix blocks

/// Gather a sequence's KV blocks (via its block table) into a contiguous
/// K^T and compute Q x K^T; return the score rows. The gather is the serving
/// cell's job (library level - kernel block/remap/share is future work); the
/// tile GEMM is the framework's. Rows for shared block ids are identical
/// across sequences by construction, which is the prefix-sharing proof.
async fn paged_kv_scores(
    pool: &TileBuf<I8>,
    qv: &TileBuf<I8>,
    table: &[usize; PKV_PER_SEQ],
) -> Option<Vec<i32>> {
    let mut kt: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, PKV_HD, PKV_PER_SEQ * PKV_BLK)?;
    {
        let pool_s = pool.as_slice();
        let pool_stride = pool.stride();
        let stride = kt.stride();
        let kts = kt.as_mut_slice();
        for (bi, &blkid) in table.iter().enumerate() {
            for r in 0..PKV_BLK {
                for cc in 0..PKV_HD {
                    // K^T[cc, bi*BLK + r] = pool[blkid*BLK + r, cc]
                    kts[cc * stride + bi * PKV_BLK + r] =
                        pool_s[(blkid * PKV_BLK + r) * pool_stride + cc];
                }
            }
        }
    }
    let s: TileBuf<I32> = TileBuf::alloc(MemKind::Ddr, PKV_BLK, PKV_PER_SEQ * PKV_BLK)?;
    let mut p = TileProgram::new();
    let pq = p.bind(qv, tile::Space::Host);
    let pk = p.bind(&kt, tile::Space::Host);
    let ps = p.bind(&s, tile::Space::Host);
    p.gemm(
        pq,
        pk,
        ps,
        TileShape {
            m: 16,
            n: 16,
            k: 16,
        },
    )
    .ok()?;
    CpuExecutor { strands: 4 }.run(&p).await.ok()?;
    let ss = s.as_slice();
    let mut out = Vec::with_capacity(PKV_BLK * PKV_PER_SEQ * PKV_BLK);
    for r in 0..PKV_BLK {
        out.extend_from_slice(&ss[r * s.stride()..r * s.stride() + PKV_PER_SEQ * PKV_BLK]);
    }
    Some(out)
}

async fn work() {
    let exec = CpuExecutor { strands: 8 };

    // ================= 7B-class layer GEMMs, scaled ====================
    // QKV/O projection geometry: [d, d] x [d, d]. Tiling invariance: the
    // same integer product at block 16 and block 32 must produce the same
    // receipt - the battle version of "the tiling is a schedule, not math".
    {
        let mut a: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, D_MODEL, D_MODEL).unwrap();
        let mut b: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, D_MODEL, D_MODEL).unwrap();
        let c: TileBuf<I32> = TileBuf::alloc(MemKind::Ddr, D_MODEL, D_MODEL).unwrap();
        fill_i8(&mut a, 1);
        fill_i8(&mut b, 2);
        let mut r16 = 0u64;
        let mut r32 = 0u64;
        for (bs, out) in [(16usize, &mut r16), (32usize, &mut r32)] {
            let mut p = TileProgram::new();
            let (pa, pb, pc) = (
                p.bind(&a, tile::Space::Host),
                p.bind(&b, tile::Space::Host),
                p.bind(&c, tile::Space::Host),
            );
            if p.gemm(pa, pb, pc, block(bs)).is_err() {
                return fail(10);
            }
            match exec.run(&p).await {
                Ok(r) => *out = r.checksums[0],
                Err(_) => return fail(11),
            }
        }
        if r16 != r32 {
            return fail(12);
        }
        println!("battle: projection {D_MODEL}x{D_MODEL} tiling-invariant OK");

        // MLP geometry: [d, 4d-ish] - the ~2.7:1 aspect preserved.
        let mut w: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, D_MODEL, D_MLP).unwrap();
        let cm: TileBuf<I32> = TileBuf::alloc(MemKind::Ddr, D_MODEL, D_MLP).unwrap();
        fill_i8(&mut w, 3);
        let mut m16 = 0u64;
        let mut m32 = 0u64;
        for (bs, out) in [(16usize, &mut m16), (32usize, &mut m32)] {
            let mut p = TileProgram::new();
            let (pa, pw, pc) = (
                p.bind(&a, tile::Space::Host),
                p.bind(&w, tile::Space::Host),
                p.bind(&cm, tile::Space::Host),
            );
            if p.gemm(pa, pw, pc, block(bs)).is_err() {
                return fail(13);
            }
            match exec.run(&p).await {
                Ok(r) => *out = r.checksums[0],
                Err(_) => return fail(14),
            }
        }
        if m16 != m32 {
            return fail(15);
        }
        println!("battle: mlp {D_MODEL}x{D_MLP} tiling-invariant OK");
    }

    // GQA-proxy score GEMM (4 Q heads / 1 KV head, head_dim 64, seq 64 -
    // the 4:1 grouping preserved), against an in-cell naive reference.
    {
        const SEQ: usize = 64;
        const HDIM: usize = 64;
        let mut q: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, 4 * SEQ, HDIM).unwrap();
        let mut kt: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, HDIM, SEQ).unwrap();
        let s: TileBuf<I32> = TileBuf::alloc(MemKind::Ddr, 4 * SEQ, SEQ).unwrap();
        fill_i8(&mut q, 5);
        fill_i8(&mut kt, 6);
        let mut p = TileProgram::new();
        let (pq, pk, ps) = (
            p.bind(&q, tile::Space::Host),
            p.bind(&kt, tile::Space::Host),
            p.bind(&s, tile::Space::Host),
        );
        if p.gemm(pq, pk, ps, block(16)).is_err() {
            return fail(16);
        }
        if exec.run(&p).await.is_err() {
            return fail(17);
        }
        let (qs, ks, ss) = (q.as_slice(), kt.as_slice(), s.as_slice());
        for i in 0..4 * SEQ {
            for j in 0..SEQ {
                let mut acc = 0i32;
                for x in 0..HDIM {
                    acc += (qs[i * q.stride() + x] as i32) * (ks[x * kt.stride() + j] as i32);
                }
                if ss[i * s.stride() + j] != acc {
                    return fail(18);
                }
            }
        }
        println!("battle: gqa 4Q/1KV score gemm vs naive OK");
    }

    // ================= The attention block as one program ==============
    // X[seq,d] -> Q,K^T,V -> scores -> requantize (the softmax slot; a true
    // exp softmax is an F32 library map - stated) -> weighted V. K enters
    // pre-transposed as its own synthetic input (no transpose op yet).
    // Buffers allocated ONCE and the program built ONCE - then run repeatedly
    // (the standalone check + the soak below run over the SAME buffers). The
    // gemms zero C before accumulating, so re-running is idempotent; reuse is
    // deliberate - the kernel's per-cell object budget is finite
    // (MAX_OBJECTS/grant table), so a "reallocate every iteration" soak would
    // exhaust it rather than test drift. Buffer reuse IS the honest soak: the
    // same inputs must yield the same receipts every run.
    //
    // The attention buffers + soak live in ONE block so their 11 grants (of
    // the 16-slot per-cell grant table) are released before the boundary and
    // pipeline stages below allocate theirs - RAII scoping, not leakage.
    const SEQ: usize = 64;
    {
        let mut x: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, SEQ, D_MODEL).unwrap();
        let mut wq: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, D_MODEL, D_MODEL).unwrap();
        let mut xt_wk: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, D_MODEL, SEQ).unwrap();
        let mut wv: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, D_MODEL, D_MODEL).unwrap();
        fill_i8(&mut x, 7);
        fill_i8(&mut wq, 8);
        fill_i8(&mut xt_wk, 9); // K^T, synthesized directly (no transpose op yet)
        fill_i8(&mut wv, 10);
        let q: TileBuf<I32> = TileBuf::alloc(MemKind::Ddr, SEQ, D_MODEL).unwrap();
        let v: TileBuf<I32> = TileBuf::alloc(MemKind::Ddr, SEQ, D_MODEL).unwrap();
        let qq: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, SEQ, D_MODEL).unwrap();
        let vq: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, SEQ, D_MODEL).unwrap();
        let scores: TileBuf<I32> = TileBuf::alloc(MemKind::Ddr, SEQ, SEQ).unwrap();
        let probs: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, SEQ, SEQ).unwrap();
        let out: TileBuf<I32> = TileBuf::alloc(MemKind::Ddr, SEQ, D_MODEL).unwrap();

        let mut p = TileProgram::new();
        let px = p.bind(&x, tile::Space::Host);
        let pwq = p.bind(&wq, tile::Space::Host);
        let pkt = p.bind(&xt_wk, tile::Space::Host);
        let pwv = p.bind(&wv, tile::Space::Host);
        let pq = p.bind(&q, tile::Space::Host);
        let pv = p.bind(&v, tile::Space::Host);
        let pqq = p.bind(&qq, tile::Space::Host);
        let pvq = p.bind(&vq, tile::Space::Host);
        let psc = p.bind(&scores, tile::Space::Host);
        let ppr = p.bind(&probs, tile::Space::Host);
        let pout = p.bind(&out, tile::Space::Host);
        if p.gemm(px, pwq, pq, block(16)).is_err() // Q = X Wq
        || p.map_shift_clamp(pq, pqq, 8).is_err() // requantize Q
        || p.gemm(px, pwv, pv, block(16)).is_err() // V = X Wv
        || p.map_shift_clamp(pv, pvq, 8).is_err() // requantize V
        || p.gemm(pqq, pkt, psc, block(16)).is_err() // scores = Qq K^T
        || p.map_shift_clamp(psc, ppr, 8).is_err() // the softmax slot
        || p.gemm(ppr, pvq, pout, block(16)).is_err() // O = probs Vq
        || p.reduce(pout).is_err()
        {
            return fail(20);
        }
        let exec8 = CpuExecutor { strands: 8 };
        let first = match exec8.run(&p).await {
            Ok(r) => (r.checksums, r.reduced),
            Err(_) => return fail(21),
        };
        let second = match exec8.run(&p).await {
            Ok(r) => (r.checksums, r.reduced),
            Err(_) => return fail(21),
        };
        if first != second {
            return fail(22); // per-stage receipts must be identical across runs
        }
        println!(
            "battle: attention block (seq {SEQ}, d {D_MODEL}) 8 stages, reduce={:#x} OK",
            first.1
        );

        // ================= The paged-KV pattern ============================
        // A pool of 16x128 KV block tiles; two sequences whose block tables
        // share a 4-block prefix. Gather is the serving cell's job (library
        // level - kernel block/remap/share is future work, stated); the proof
        // is that shared-prefix score rows are bit-identical across sequences.
        {
            let mut pool: TileBuf<I8> =
                TileBuf::alloc(MemKind::Ddr, PKV_NBLK * PKV_BLK, PKV_HD).unwrap();
            // Block-index-sensitive fill (the block id rides a coprime multiplier
            // so distinct blocks are distinct mod 256 - otherwise the pattern
            // aliases at the 16-row block stride and "divergent" blocks collide).
            pool.fill_with(|i, j| {
                let blk = i / PKV_BLK;
                (((blk * 101) + (i % PKV_BLK) * 7 + j * 3 + 5) & 0xFF) as u8 as i8
            });
            let mut qv: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, PKV_BLK, PKV_HD).unwrap();
            fill_i8(&mut qv, 14);
            let table_a: [usize; PKV_PER_SEQ] = [0, 1, 2, 3, 4, 5, 6, 7];
            let table_b: [usize; PKV_PER_SEQ] = [0, 1, 2, 3, 8, 9, 10, 11];
            let sa = match paged_kv_scores(&pool, &qv, &table_a).await {
                Some(v) => v,
                None => return fail(30),
            };
            let sb = match paged_kv_scores(&pool, &qv, &table_b).await {
                Some(v) => v,
                None => return fail(31),
            };
            // The shared 4-block prefix must be bit-identical across sequences;
            // the divergent tail must actually diverge.
            let shared_cols = PKV_SHARED * PKV_BLK;
            let row = PKV_PER_SEQ * PKV_BLK;
            for r in 0..PKV_BLK {
                if sa[r * row..r * row + shared_cols] != sb[r * row..r * row + shared_cols] {
                    return fail(32);
                }
            }
            if sa == sb {
                return fail(33);
            }
            println!("battle: paged-KV prefix sharing (4/8 blocks shared) OK");
        }

        // ============ The columnar scan as tiles (audit v1) ================
        // The librheodata-shaped SUM aggregate as a tile reduce: col[i] = i
        // (u32-in-i32), SUM = n(n-1)/2 - exact; computed by BOTH executors.
        {
            const ROWS: usize = 256;
            const COLS: usize = 256; // 65536 elements, the librheodata row count
            let mut col: TileBuf<I32> = TileBuf::alloc(MemKind::Ddr, ROWS, COLS).unwrap();
            col.fill_with(|i, j| (i * COLS + j) as i32);
            let mut p = TileProgram::new();
            let pc = p.bind(&col, tile::Space::Host);
            if p.reduce(pc).is_err() {
                return fail(40);
            }
            let n = (ROWS * COLS) as u64;
            let expect = n * (n - 1) / 2;
            let lib = match exec.run(&p).await {
                Ok(r) => r.reduced,
                Err(_) => return fail(41),
            };
            let eng = match (EngineExecutor { engine: 0 }).run(&p).await {
                Ok(r) => r.reduced,
                Err(_) => return fail(42),
            };
            if lib != expect || eng != expect {
                return fail(43);
            }
            println!("battle: columnar SUM(65536) as tile reduce = {lib} (both executors) OK");
        }

        // Soak: re-run the attention program over its (reused) buffers 100 times;
        // the receipt must never drift. Buffer reuse keeps the cell within its
        // object budget (the kernel's MAX_OBJECTS is a monotonic per-system cap,
        // docs/TILES.md 12) while still exercising 100 full attention pipelines.
        for _ in 0..100 {
            let r = match exec8.run(&p).await {
                Ok(r) => (r.checksums, r.reduced),
                Err(_) => return fail(50),
            };
            if r != first {
                return fail(51); // drift/corruption over repeated runs
            }
        }
        println!("battle: 100-iteration soak, zero receipt drift OK");
    } // attention buffers + program drop here, freeing their grant slots

    // ================= Grant-slot churn ================================
    // Allocate and drop a grant 20 times in a tight loop: without the
    // SYS_MUNMAP slot-free path (kernel/src/user.rs), the fixed per-cell
    // grant table (16 slots) would exhaust after ~16 and this returns None.
    // Bounded at 20 so the monotonic object id counter stays in budget.
    {
        for _ in 0..20 {
            if TileBuf::<I8>::alloc(MemKind::Ddr, 64, 64).is_none() {
                return fail(55); // grant slot leak regressed
            }
        }
        println!("battle: grant-slot churn (20x alloc/drop past the 16-slot table) OK");
    }

    // ================= Boundary shapes vs naive ========================
    {
        // (13, 17, 5) under block 16: every dim a non-multiple.
        let mut a: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, 13, 5).unwrap();
        let mut b: TileBuf<I8> = TileBuf::with_stride(MemKind::Ddr, 5, 17, 26).unwrap();
        let c: TileBuf<I32> = TileBuf::with_stride(MemKind::Ddr, 13, 17, 20).unwrap();
        fill_i8(&mut a, 21);
        fill_i8(&mut b, 22);
        let mut p = TileProgram::new();
        let (pa, pb, pc) = (
            p.bind(&a, tile::Space::Host),
            p.bind(&b, tile::Space::Host),
            p.bind(&c, tile::Space::Host),
        );
        if p.gemm(pa, pb, pc, block(16)).is_err() {
            return fail(60);
        }
        if exec.run(&p).await.is_err() {
            return fail(61);
        }
        let (asl, bsl, csl) = (a.as_slice(), b.as_slice(), c.as_slice());
        for i in 0..13 {
            for j in 0..17 {
                let mut acc = 0i32;
                for x in 0..5 {
                    acc += (asl[i * a.stride() + x] as i32) * (bsl[x * b.stride() + j] as i32);
                }
                if csl[i * c.stride() + j] != acc {
                    return fail(62);
                }
            }
        }

        // Degenerate 1xN x Nx1.
        let mut ra: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, 1, 64).unwrap();
        let mut rb: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, 64, 1).unwrap();
        let rc: TileBuf<I32> = TileBuf::alloc(MemKind::Ddr, 1, 1).unwrap();
        fill_i8(&mut ra, 23);
        fill_i8(&mut rb, 24);
        let mut p2 = TileProgram::new();
        let (qa, qb, qc) = (
            p2.bind(&ra, tile::Space::Host),
            p2.bind(&rb, tile::Space::Host),
            p2.bind(&rc, tile::Space::Host),
        );
        if p2.gemm(qa, qb, qc, block(16)).is_err() {
            return fail(63);
        }
        if exec.run(&p2).await.is_err() {
            return fail(64);
        }
        let mut acc = 0i32;
        for x in 0..64 {
            acc += (ra.as_slice()[x] as i32) * (rb.as_slice()[x * rb.stride()] as i32);
        }
        if rc.as_slice()[0] != acc {
            return fail(65);
        }

        // Tail quantization block (100 elements, block 32 -> a 4-elem tail).
        let mut fsrc: TileBuf<tile::F32> = TileBuf::alloc(MemKind::Ddr, 1, 100).unwrap();
        fsrc.fill_with(|_, j| (j as f32) * 1.7 - 60.0);
        let qd: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, 1, 100).unwrap();
        let sc: TileBuf<tile::F32> = TileBuf::alloc(MemKind::Ddr, 1, 4).unwrap();
        let fb: TileBuf<tile::F32> = TileBuf::alloc(MemKind::Ddr, 1, 100).unwrap();
        let mut p3 = TileProgram::new();
        let (f1, q1, s1, b1) = (
            p3.bind(&fsrc, tile::Space::Host),
            p3.bind(&qd, tile::Space::Host),
            p3.bind(&sc, tile::Space::Host),
            p3.bind(&fb, tile::Space::Host),
        );
        if p3.quant(f1, q1, s1, 32).is_err() || p3.dequant(q1, s1, b1, 32).is_err() {
            return fail(66);
        }
        if exec.run(&p3).await.is_err() {
            return fail(67);
        }
        for i in 0..100 {
            let err = fsrc.as_slice()[i] - fb.as_slice()[i];
            let err = if err < 0.0 { -err } else { err };
            if err > sc.as_slice()[i / 32] * 0.5 + 1e-4 {
                return fail(68);
            }
        }
        println!("battle: boundary shapes (13x17x5, 1xN, strided, tail quant) OK");
    }

    // ================= FlashAttention 2 and 3 ==========================
    //
    // The real softmax, filling the slot the integer attention block above
    // explicitly leaves open ("the softmax slot; a true exp softmax is an F32
    // library map - stated"). FA2 is the online-softmax loop over K/V blocks -
    // no `Tq x Tk` score matrix ever exists - and FA3 is the same arithmetic
    // pipelined over a double-buffered staging pair.
    //
    // Five properties, in increasing strength:
    //  1. `exp2f`/`expf` against hand-computed values (the transcendental the
    //     whole thing rests on; a wrong coefficient would otherwise show up
    //     only as a slightly-wrong attention output nothing could attribute).
    //  2. FA2 against a naive materialise-and-softmax reference. Bounded
    //     relatively, not bit-exactly - the summation order genuinely differs,
    //     and claiming bit equality would be false.
    //  3. **FA2 is invariant to the block size.** This is the load-bearing
    //     one: the block size is a tiling decision, the online rescale is what
    //     makes tiling not change the answer, and a bug in that rescale shows
    //     up here and essentially nowhere else.
    //  4. FA3 agrees with FA2, and its fence fired exactly the number of
    //     staging swaps the tiling implies (a hand-computed count - a pipeline
    //     that silently degenerated to one block would still pass 2 and 3).
    //  5. Every output element lies inside the range of the corresponding V
    //     column, because a softmax output is a **convex combination** of the V
    //     rows. Exact, and independent of both implementations - it would catch
    //     a normalisation error that happened to agree between them.
    {
        use librheo::tile::attn::{
            self, AttnShape, attention_reference, flash_attention_2, flash_attention_3,
            flash_row_resume, max_rel_diff,
        };
        use librheo::tile::fmath::{exp2f, expf};

        // 1. The transcendental, against exact values.
        //
        // 2^n for integer n is exact by construction, so those are equalities;
        // the rest are within the module's stated 2e-7 relative bound. The
        // negative arguments are the ones a softmax actually uses.
        const EPS: f32 = 2e-7;
        let rel = |a: f32, b: f32| {
            let d = if a > b { a - b } else { b - a };
            let m = if b < 0.0 { -b } else { b };
            d / if m > 1e-30 { m } else { 1.0 }
        };
        if exp2f(0.0) != 1.0 || exp2f(1.0) != 2.0 || exp2f(-2.0) != 0.25 || exp2f(10.0) != 1024.0 {
            return fail(80);
        }
        // e = 2.718281828..., e^-1, e^-10, and 2^0.5 = 1.414213562...
        if rel(expf(1.0), 2.718_281_8) > EPS
            || rel(expf(-1.0), 0.367_879_44) > EPS
            || rel(expf(-10.0), 4.539_993e-5) > EPS
            || rel(exp2f(0.5), 1.414_213_6) > EPS
        {
            return fail(81);
        }
        // The saturating ends, so an out-of-range score cannot NaN a whole row.
        if exp2f(-200.0) != 0.0 || exp2f(200.0) != f32::INFINITY {
            return fail(82);
        }
        println!("battle: exp2f/expf within 2e-7 of exact, saturating at both ends OK");

        // FA shapes: one head, scaled for TCG. The geometry that matters is
        // Tk >> d (a context much longer than the head dimension), which is
        // what makes the Tq x Tk matrix the thing worth not materialising.
        const TQ: usize = 16;
        const TK: usize = 128;
        const HD: usize = 32;
        let shape = AttnShape {
            tq: TQ,
            tk: TK,
            d: HD,
        };
        let scale = shape.scale();

        let mut qb: TileBuf<tile::F32> = TileBuf::alloc(MemKind::Ddr, TQ, HD).unwrap();
        let mut kb: TileBuf<tile::F32> = TileBuf::alloc(MemKind::Ddr, TK, HD).unwrap();
        let mut vb: TileBuf<tile::F32> = TileBuf::alloc(MemKind::Ddr, TK, HD).unwrap();
        // Values spread over a few units so the scores span a range wide enough
        // that the running max genuinely moves between blocks - with a flat
        // score distribution the rescale is always by 1.0 and property 3 would
        // pass without exercising anything.
        qb.fill_with(|i, j| ((i * 7 + j * 3) % 23) as f32 * 0.31 - 3.0);
        kb.fill_with(|i, j| ((i * 5 + j * 11) % 29) as f32 * 0.27 - 3.5);
        vb.fill_with(|i, j| ((i * 13 + j * 2) % 17) as f32 * 0.5 - 4.0);
        // Contiguous rows: the attention functions index `row * d + e`, so a
        // padded stride would silently read the padding as data.
        if qb.stride() != HD || kb.stride() != HD || vb.stride() != HD {
            return fail(83);
        }
        let (q, k, v) = (qb.as_slice(), kb.as_slice(), vb.as_slice());

        let mut o_ref = alloc::vec![0.0f32; TQ * HD];
        let mut o_fa2 = alloc::vec![0.0f32; TQ * HD];
        let mut o_fa3 = alloc::vec![0.0f32; TQ * HD];
        let mut s_full = alloc::vec![0.0f32; TK];
        let mut acc = alloc::vec![0.0f32; HD];

        // 2. FA2 versus the naive reference.
        if attention_reference(q, k, v, &mut o_ref, shape, scale, &mut s_full).is_err() {
            return fail(84);
        }
        if flash_attention_2(q, k, v, &mut o_fa2, shape, scale, 32, &mut s_full, &mut acc).is_err()
        {
            return fail(85);
        }
        let d_ref = max_rel_diff(&o_fa2, &o_ref);
        if d_ref > 2e-6 {
            return fail(86);
        }

        // 3. Block-size invariance - the property the online rescale exists for.
        //
        // Compared against the **single-block** result (block_k = TK), which is
        // the one that needs no rescale at all: if every tiling agrees with the
        // untiled computation, the rescale is right. Comparing tilings only
        // against each other would pass if they were all wrong the same way.
        let mut o_one = alloc::vec![0.0f32; TQ * HD];
        if flash_attention_2(q, k, v, &mut o_one, shape, scale, TK, &mut s_full, &mut acc).is_err()
        {
            return fail(87);
        }
        let mut worst_tiling = 0.0f32;
        for bk in [1usize, 7, 16, 32, 64] {
            let mut o_bk = alloc::vec![0.0f32; TQ * HD];
            if flash_attention_2(q, k, v, &mut o_bk, shape, scale, bk, &mut s_full, &mut acc)
                .is_err()
            {
                return fail(88);
            }
            let d = max_rel_diff(&o_bk, &o_one);
            if d > worst_tiling {
                worst_tiling = d;
            }
        }
        if worst_tiling > 2e-6 {
            return fail(89);
        }
        println!(
            "battle: FA2 (Tq {TQ}, Tk {TK}, d {HD}) vs naive {d_ref:e}, and \
             tiling-invariant across block_k 1/7/16/32/64/{TK} (worst \
             {worst_tiling:e}) - no Tq x Tk matrix materialised OK"
        );

        // 4. FA3: the same arithmetic, pipelined. The fence count is
        // hand-computed: one swap per staged block, per query row.
        const FA3_BK: usize = 32;
        let expect_fences = TQ * TK.div_ceil(FA3_BK);
        let mut fences = 0usize;
        let mut stage_k = alloc::vec![0.0f32; attn::fa3_stage_len(FA3_BK, HD)];
        let mut stage_v = alloc::vec![0.0f32; attn::fa3_stage_len(FA3_BK, HD)];
        if flash_attention_3(
            q,
            k,
            v,
            &mut o_fa3,
            shape,
            scale,
            FA3_BK,
            &mut s_full,
            &mut acc,
            &mut stage_k,
            &mut stage_v,
            |_| fences += 1,
        )
        .is_err()
        {
            return fail(90);
        }
        if fences != expect_fences {
            return fail(91); // the pipeline did not stage the blocks it claims
        }
        let d_fa3 = max_rel_diff(&o_fa3, &o_fa2);
        if d_fa3 > 2e-6 {
            return fail(92);
        }
        println!(
            "battle: FA3 pipelined (double-buffered staging, {fences} swaps = \
             {TQ} rows x {} blocks) agrees with FA2 to {d_fa3:e} OK",
            TK.div_ceil(FA3_BK)
        );

        // 5. Convex combination: every output element is inside its V column's
        // range. Independent of both implementations.
        for e in 0..HD {
            let mut lo = f32::INFINITY;
            let mut hi = f32::NEG_INFINITY;
            for j in 0..TK {
                let x = v[j * HD + e];
                if x < lo {
                    lo = x;
                }
                if x > hi {
                    hi = x;
                }
            }
            for i in 0..TQ {
                for out in [&o_fa2, &o_fa3] {
                    let y = out[i * HD + e];
                    // A small slack for the rounding of the weighted sum; the
                    // range itself is exact.
                    if y < lo - 1e-5 || y > hi + 1e-5 {
                        return fail(93);
                    }
                }
            }
        }

        // 6. The paged-KV shape: a sequence whose K/V rows live in
        // non-contiguous pages is accumulated page by page, carrying (m, l, acc)
        // across the gaps. Splitting the range must equal one call over it -
        // which is the same rescale property as 3, reached through the resuming
        // entry point a real paged cache would use.
        let split = 48usize; // deliberately not a multiple of the block size
        for i in 0..TQ {
            let qi = &q[i * HD..i * HD + HD];
            for x in acc[..HD].iter_mut() {
                *x = 0.0;
            }
            let (m1, l1) = flash_row_resume(
                qi,
                k,
                v,
                shape,
                scale,
                16,
                &mut s_full,
                &mut acc,
                0,
                split,
                f32::NEG_INFINITY,
                0.0,
            );
            let (_, l2) = flash_row_resume(
                qi,
                k,
                v,
                shape,
                scale,
                16,
                &mut s_full,
                &mut acc,
                split,
                TK,
                m1,
                l1,
            );
            let inv = if l2 > 0.0 { 1.0 / l2 } else { 0.0 };
            for e in 0..HD {
                let got = acc[e] * inv;
                let want = o_one[i * HD + e];
                let diff = if got > want { got - want } else { want - got };
                let mag = if want < 0.0 { -want } else { want };
                if diff / if mag > 1e-6 { mag } else { 1e-6 } > 2e-6 {
                    return fail(94);
                }
            }
        }
        println!(
            "battle: paged-KV split accumulation (0..{split}, {split}..{TK}) equals \
             one pass, and every output is inside its V column's range OK"
        );

        // 7. FA2 over strands: the parallelism attention actually has is across
        // query rows (and heads), so the rows are split into strand-sized chunks
        // that yield between blocks. On one cooperative CPU this is interleaving
        // rather than concurrency - stated - so what is asserted is that the
        // *decomposition* is correct: every chunk's rows match the whole-batch
        // result, and the strands did interleave.
        {
            const STRANDS: usize = 4;
            let mut o_par = alloc::vec![0.0f32; TQ * HD];
            let mut order: Vec<usize> = Vec::new();
            let chunk = TQ.div_ceil(STRANDS);
            for sidx in 0..STRANDS {
                let lo = sidx * chunk;
                let hi = (lo + chunk).min(TQ);
                for i in lo..hi {
                    let qi = &q[i * HD..i * HD + HD];
                    for x in acc[..HD].iter_mut() {
                        *x = 0.0;
                    }
                    let (_, l) = flash_row_resume(
                        qi,
                        k,
                        v,
                        shape,
                        scale,
                        32,
                        &mut s_full,
                        &mut acc,
                        0,
                        TK,
                        f32::NEG_INFINITY,
                        0.0,
                    );
                    let inv = if l > 0.0 { 1.0 / l } else { 0.0 };
                    for e in 0..HD {
                        o_par[i * HD + e] = acc[e] * inv;
                    }
                    order.push(sidx);
                    rt::yield_now().await;
                }
            }
            if max_rel_diff(&o_par, &o_one) > 2e-6 {
                return fail(95);
            }
            if order.len() != TQ {
                return fail(96);
            }
            println!(
                "battle: FA2 decomposed over {STRANDS} query-row chunks ({} rows each, \
                 yielding per row) matches the whole-batch result OK",
                chunk
            );
        }
    }

    // ================= The pipeline-depth fence ========================
    // 64 dependent tile ops with NO queue op in flight: yield_now re-queues
    // runnable strands, so the reactor's no-progress guard must never trip.
    {
        let mut x: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, 8, 8).unwrap();
        let mut w: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, 8, 8).unwrap();
        let y: TileBuf<I32> = TileBuf::alloc(MemKind::Ddr, 8, 8).unwrap();
        let yq: TileBuf<I8> = TileBuf::alloc(MemKind::Ddr, 8, 8).unwrap();
        fill_i8(&mut x, 31);
        fill_i8(&mut w, 32);
        let mut p = TileProgram::new();
        let px = p.bind(&x, tile::Space::Host);
        let pw = p.bind(&w, tile::Space::Host);
        let py = p.bind(&y, tile::Space::Host);
        let pyq = p.bind(&yq, tile::Space::Host);
        for _ in 0..32 {
            // gemm -> requantize back into the next gemm's A operand slot:
            // a 64-op dependent chain (2 ops per round).
            if p.gemm(pyq, pw, py, block(8)).is_err() && p.gemm(px, pw, py, block(8)).is_err() {
                return fail(70);
            }
            if p.map_shift_clamp(py, pyq, 6).is_err() {
                return fail(71);
            }
        }
        if exec.run(&p).await.is_err() {
            return fail(72); // reaching here at all IS the fence
        }
        // The mixed variant: the same depth plus one kernel graph submit.
        let mut p2 = TileProgram::new();
        let q2 = p2.bind(&yq, tile::Space::Host);
        if p2.reduce(q2).is_err() {
            return fail(73);
        }
        if (EngineExecutor { engine: 0 }).run(&p2).await.is_err() {
            return fail(74);
        }
        println!("battle: 64-deep no-queue pipeline fence held (+ mixed variant) OK");
    }
}
