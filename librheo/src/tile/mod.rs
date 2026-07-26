//! Tile-centric compute - one tile program, every engine (docs/TILES.md).
//!
//! A tile is shape x dtype x memory space; a [`TileBuf`] is a dtype-tagged
//! buffer over a memory grant (object 5); a [`TileProgram`] is built once and
//! lowered per engine: the [`CpuExecutor`] runs it strand-parallel in the cell
//! (the library-call lowering, scalar inner kernels today), and the
//! engine-graph lowering submits the SAME program as dependency-graph nodes
//! (object 6) for the kernel's CPU engine now and device engines when their
//! driver cells exist. [`TileSim`] walks a program against a [`TileContract`]
//! and counts work and traffic deterministically - counts, never timing.
//!
//! Honest scope (docs/TILES.md 12): integer paths are exact everywhere; F32
//! runs soft-float in a cell; in-cell SIMD waits on U-mode vector-state
//! save/restore; on the single-CPU cooperative runtime strand pipelining is
//! interleaving, not overlap (SMP is task #27).

pub mod kernels;

use alloc::vec::Vec;
use core::marker::PhantomData;

use crate::compute::{self, EngineKind, Preemption};
use crate::mem::{Grant, MemKind};
use crate::rt;

// ============================================================================
// Dtypes: the element format rides the buffer type (docs/TILES.md 2).
// ============================================================================

/// Element formats - the full quantization matrix (docs/TILES.md 2). Integer
/// and F32 are native (a whole-byte CPU `Repr` the executor slices and
/// computes on today). The narrow formats are **storage dtypes**: the tile
/// layer converts to/from them deterministically (bit-exact
/// [`kernels`](super::kernels)), and MMA *over* them is a device-engine
/// lowering (declared - a GEMM over a non-native input is a compile error,
/// no [`MmaInput`] impl, so "no engine here runs this" is a build error).
///
/// Widths: int4/fp8 formats are 8-bit slots except the block-packed int4
/// (2 codes/byte). TF32 ("bfloat32") is carried in an f32 slot with a
/// reduced mantissa.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Dtype {
    // Native integer + f32.
    I8 = 0,
    U8 = 1,
    I32 = 2,
    F32 = 3,
    // 16-bit floats.
    F16 = 4,
    Bf16 = 5,
    // 8-bit floats: E4M3 (fp8) and E5M2 (the "bfloat8" truncation format).
    F8E4M3 = 6,
    F8E5M2 = 7,
    // TF32 / "bfloat32": f32 range, 10-bit mantissa, stored in an f32 slot.
    Tf32 = 8,
    // 4-bit integer, block-quantized, two codes per byte.
    I4Block32 = 9,
}

impl Dtype {
    /// Element width in bits (I4Block32 is sub-byte: 4; Tf32 occupies an f32
    /// slot: 32).
    pub fn bits(self) -> usize {
        match self {
            Dtype::I8 | Dtype::U8 | Dtype::F8E4M3 | Dtype::F8E5M2 => 8,
            Dtype::F16 | Dtype::Bf16 => 16,
            Dtype::I32 | Dtype::F32 | Dtype::Tf32 => 32,
            Dtype::I4Block32 => 4,
        }
    }
    /// Bytes for `n` elements (rounded up for sub-byte formats).
    pub fn bytes(self, n: usize) -> usize {
        (n * self.bits()).div_ceil(8)
    }
    /// Whether the CPU executor computes on this dtype today (vs. only
    /// converts to/from it as storage).
    pub fn is_native(self) -> bool {
        matches!(self, Dtype::I8 | Dtype::U8 | Dtype::I32 | Dtype::F32)
    }
}

/// Marker types carrying a [`Dtype`] at the type level.
pub struct I8;
pub struct U8;
pub struct I32;
pub struct F32;
pub struct F16;
pub struct Bf16;
pub struct F8E4M3;
pub struct F8E5M2;
pub struct Tf32;
pub struct I4Block32;

/// A type-level dtype tag.
pub trait Dtyped: 'static {
    const DTYPE: Dtype;
}
macro_rules! dtyped {
    ($($t:ident => $d:expr),+ $(,)?) => {
        $(impl Dtyped for $t { const DTYPE: Dtype = $d; })+
    };
}
dtyped! {
    I8 => Dtype::I8, U8 => Dtype::U8, I32 => Dtype::I32, F32 => Dtype::F32,
    F16 => Dtype::F16, Bf16 => Dtype::Bf16, F8E4M3 => Dtype::F8E4M3,
    F8E5M2 => Dtype::F8E5M2, Tf32 => Dtype::Tf32, I4Block32 => Dtype::I4Block32,
}

/// Natively representable dtypes (a whole-byte in-memory `Repr` the CPU can
/// slice). Declared/storage dtypes have no `Native` impl - no CPU view.
pub trait Native: Dtyped {
    type Repr: Copy + Default + 'static;
}
impl Native for I8 {
    type Repr = i8;
}
impl Native for U8 {
    type Repr = u8;
}
impl Native for I32 {
    type Repr = i32;
}
impl Native for F32 {
    type Repr = f32;
}

/// Dtype pairs a GEMM accepts: input -> accumulator. `I8 -> I32` everywhere
/// (the kernel engine's only pair); `F32 -> F32` on the [`CpuExecutor`] only
/// (soft-float in a cell; the kernel is integer-only). Declared dtypes have
/// no impl, so `gemm` over them is a compile error - the type system saying
/// "no engine here can run this" (docs/TILES.md 2).
pub trait MmaInput: Dtyped {
    type Acc: Dtyped;
}
impl MmaInput for I8 {
    type Acc = I32;
}
impl MmaInput for F32 {
    type Acc = F32;
}

// ============================================================================
// TileBuf: a dtype-tagged 2D buffer over a memory grant.
// ============================================================================

/// A rows x cols buffer of `D` elements over a [`Grant`], with an element
/// stride (row pitch) that may exceed `cols`. The grant's capability is the
/// buffer's graph-visible name; fresh grant frames arrive zeroed.
pub struct TileBuf<D: Dtyped> {
    grant: Grant,
    rows: usize,
    cols: usize,
    stride: usize,
    _d: PhantomData<D>,
}

impl<D: Dtyped> TileBuf<D> {
    /// Allocate rows x cols with stride == cols on memory of `kind`.
    pub fn alloc(kind: MemKind, rows: usize, cols: usize) -> Option<TileBuf<D>> {
        TileBuf::with_stride(kind, rows, cols, cols)
    }

    /// Allocate with an explicit element stride (>= cols).
    pub fn with_stride(
        kind: MemKind,
        rows: usize,
        cols: usize,
        stride: usize,
    ) -> Option<TileBuf<D>> {
        if rows == 0 || cols == 0 || stride < cols {
            return None;
        }
        let bytes = D::DTYPE.bytes(rows * stride);
        let grant = Grant::alloc(kind, bytes)?;
        Some(TileBuf {
            grant,
            rows,
            cols,
            stride,
            _d: PhantomData,
        })
    }

    /// A bounds-checked tile view: `None` if the h x w window at (r0, c0)
    /// leaves the buffer (docs/TILES.md 1 - an out-of-range tile is a `None`,
    /// not a fault).
    pub fn tile(&self, r0: usize, c0: usize, h: usize, w: usize) -> Option<Tile<'_, D>> {
        if h == 0 || w == 0 || r0 + h > self.rows || c0 + w > self.cols {
            return None;
        }
        Some(Tile {
            buf: self,
            r0,
            c0,
            h,
            w,
        })
    }

    pub fn rows(&self) -> usize {
        self.rows
    }
    pub fn cols(&self) -> usize {
        self.cols
    }
    pub fn stride(&self) -> usize {
        self.stride
    }
    /// The buffer's base VA (the graph descriptor operand).
    pub fn base_va(&self) -> u64 {
        self.grant.base() as u64
    }
    /// The backing grant's capability id.
    pub fn cap_id(&self) -> u32 {
        self.grant.cap_id()
    }
}

impl<D: Native> TileBuf<D> {
    /// The whole buffer (rows * stride elements) as a shared slice.
    pub fn as_slice(&self) -> &[D::Repr] {
        // SAFETY: the grant committed rows*stride elements of D at alloc.
        unsafe {
            core::slice::from_raw_parts(
                self.grant.base() as *const D::Repr,
                self.rows * self.stride,
            )
        }
    }
    /// The whole buffer as a mutable slice.
    pub fn as_mut_slice(&mut self) -> &mut [D::Repr] {
        // SAFETY: as above; `&mut self` guarantees uniqueness.
        unsafe {
            core::slice::from_raw_parts_mut(
                self.grant.base() as *mut D::Repr,
                self.rows * self.stride,
            )
        }
    }
    /// Fill element (r, c) = f(r, c) over the logical rows x cols window.
    pub fn fill_with(&mut self, mut f: impl FnMut(usize, usize) -> D::Repr) {
        let stride = self.stride;
        let (rows, cols) = (self.rows, self.cols);
        let s = self.as_mut_slice();
        for r in 0..rows {
            for c in 0..cols {
                s[r * stride + c] = f(r, c);
            }
        }
    }
}

/// A bounds-checked view into a [`TileBuf`].
pub struct Tile<'a, D: Dtyped> {
    buf: &'a TileBuf<D>,
    r0: usize,
    c0: usize,
    h: usize,
    w: usize,
}

impl<D: Dtyped> Tile<'_, D> {
    pub fn shape(&self) -> (usize, usize) {
        (self.h, self.w)
    }
    pub fn origin(&self) -> (usize, usize) {
        (self.r0, self.c0)
    }
    /// The view's first element VA (strided access continues at the parent's
    /// stride).
    pub fn base_va(&self) -> u64 {
        self.buf.base_va() + D::DTYPE.bytes(self.r0 * self.buf.stride + self.c0) as u64
    }
}

// ============================================================================
// TileProgram: built once, lowered per engine.
// ============================================================================

/// The block shape of a GEMM tiling (docs/TILES.md 1).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct TileShape {
    pub m: usize,
    pub n: usize,
    pub k: usize,
}

/// The abstract memory space a bound buffer plays in the program. The CPU
/// executor reaches all three through the cell's own mappings (locality is
/// the cache hierarchy's business); a device lowering assigns them to real
/// spaces per its [`TileContract`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Space {
    Host = 0,
    Scratch = 1,
    Device = 2,
}

/// A dtype-tagged reference to a buffer bound into a [`TileProgram`].
pub struct BufId<D: Dtyped>(u32, PhantomData<D>);
impl<D: Dtyped> Copy for BufId<D> {}
impl<D: Dtyped> Clone for BufId<D> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Program-build errors (shape disagreements and unsupported combinations are
/// caught at build, not at run - docs/TILES.md 2).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TileError {
    /// Buffer shapes disagree with the op.
    Shape,
    /// The op is not supported by the target executor/engine.
    UnsupportedOp,
    /// The dtype is not executable on the target.
    UnsupportedDtype,
    /// The named engine exists but has no executor (a recognised device
    /// engine without a driver cell) or does not exist.
    EngineUnavailable(u32),
    /// The kernel rejected the lowered graph (completion status).
    Kernel(u32),
}

/// Metadata for one bound buffer (dtype erased - the typed layer is
/// [`BufId`]).
#[derive(Copy, Clone)]
struct BufMeta {
    base_va: u64,
    rows: usize,
    cols: usize,
    stride: usize,
    dtype: Dtype,
    #[allow(dead_code)] // device lowerings will consume the space
    space: Space,
}

/// One encoded tile op (buffer indices into `bufs`).
#[derive(Copy, Clone)]
enum OpEnc {
    Copy {
        src: u32,
        dst: u32,
    },
    Gemm {
        a: u32,
        b: u32,
        c: u32,
        block: TileShape,
    },
    MapShiftClamp {
        src: u32,
        dst: u32,
        shift: u32,
    },
    Reduce {
        src: u32,
    },
    Quant {
        src: u32,
        dst: u32,
        scales: u32,
        block: usize,
    },
    Dequant {
        src: u32,
        scales: u32,
        dst: u32,
        block: usize,
    },
    /// Element-format conversion (F32 <-> a narrow float storage dtype),
    /// bit-exact via the kernels. The dtype pair is read from the buffers.
    Cast {
        src: u32,
        dst: u32,
    },
}

/// A tile program: buffers bound by reference (the `'p` lifetime ties the
/// program to them), ops appended in execution order. Dtype agreement is
/// enforced by the [`BufId`] types at build; shapes are validated eagerly.
pub struct TileProgram<'p> {
    bufs: Vec<BufMeta>,
    ops: Vec<OpEnc>,
    _p: PhantomData<&'p ()>,
}

impl<'p> TileProgram<'p> {
    pub fn new() -> TileProgram<'p> {
        TileProgram {
            bufs: Vec::new(),
            ops: Vec::new(),
            _p: PhantomData,
        }
    }

    /// Bind a buffer into the program, declaring the space it plays in.
    pub fn bind<D: Dtyped>(&mut self, buf: &'p TileBuf<D>, space: Space) -> BufId<D> {
        let id = self.bufs.len() as u32;
        self.bufs.push(BufMeta {
            base_va: buf.base_va(),
            rows: buf.rows(),
            cols: buf.cols(),
            stride: buf.stride(),
            dtype: D::DTYPE,
            space,
        });
        BufId(id, PhantomData)
    }

    fn meta(&self, id: u32) -> BufMeta {
        self.bufs[id as usize]
    }

    /// Copy src -> dst (same dtype by type, same logical shape by check).
    pub fn copy<D: Dtyped>(&mut self, src: BufId<D>, dst: BufId<D>) -> Result<(), TileError> {
        let (s, d) = (self.meta(src.0), self.meta(dst.0));
        if s.rows != d.rows || s.cols != d.cols {
            return Err(TileError::Shape);
        }
        self.ops.push(OpEnc::Copy {
            src: src.0,
            dst: dst.0,
        });
        Ok(())
    }

    /// C[m,n] = A[m,k] x B[k,n] under `block` tiling. Input/accumulator dtype
    /// agreement is type-level (`A: MmaInput`); shapes are checked here.
    pub fn gemm<A: MmaInput>(
        &mut self,
        a: BufId<A>,
        b: BufId<A>,
        c: BufId<A::Acc>,
        block: TileShape,
    ) -> Result<(), TileError> {
        let (ma, mb, mc) = (self.meta(a.0), self.meta(b.0), self.meta(c.0));
        if ma.cols != mb.rows || mc.rows != ma.rows || mc.cols != mb.cols {
            return Err(TileError::Shape);
        }
        if block.m == 0 || block.n == 0 || block.k == 0 {
            return Err(TileError::Shape);
        }
        self.ops.push(OpEnc::Gemm {
            a: a.0,
            b: b.0,
            c: c.0,
            block,
        });
        Ok(())
    }

    /// Integer requantize i32 -> i8 (`clamp(src >> shift)`, the softmax slot).
    pub fn map_shift_clamp(
        &mut self,
        src: BufId<I32>,
        dst: BufId<I8>,
        shift: u32,
    ) -> Result<(), TileError> {
        let (s, d) = (self.meta(src.0), self.meta(dst.0));
        if s.rows != d.rows || s.cols != d.cols {
            return Err(TileError::Shape);
        }
        self.ops.push(OpEnc::MapShiftClamp {
            src: src.0,
            dst: dst.0,
            shift,
        });
        Ok(())
    }

    /// Wrapping u64 reduction of a native integer buffer.
    pub fn reduce<D: Dtyped>(&mut self, src: BufId<D>) -> Result<(), TileError> {
        let dt = self.meta(src.0).dtype;
        if !matches!(dt, Dtype::I8 | Dtype::U8 | Dtype::I32) {
            return Err(TileError::UnsupportedDtype);
        }
        self.ops.push(OpEnc::Reduce { src: src.0 });
        Ok(())
    }

    /// Per-block symmetric quantization f32 -> i8 with a scale plane
    /// (allocation-visible: `scales` is a real bound buffer, never rounded
    /// away - docs/TILES.md 2).
    pub fn quant(
        &mut self,
        src: BufId<F32>,
        dst: BufId<I8>,
        scales: BufId<F32>,
        block: usize,
    ) -> Result<(), TileError> {
        let (s, d, sc) = (self.meta(src.0), self.meta(dst.0), self.meta(scales.0));
        let elems = s.rows * s.cols;
        if block == 0 || s.rows != d.rows || s.cols != d.cols {
            return Err(TileError::Shape);
        }
        if sc.rows * sc.cols < elems.div_ceil(block) {
            return Err(TileError::Shape);
        }
        self.ops.push(OpEnc::Quant {
            src: src.0,
            dst: dst.0,
            scales: scales.0,
            block,
        });
        Ok(())
    }

    /// Dequantize i8 -> f32 with the scale plane from [`quant`](Self::quant).
    pub fn dequant(
        &mut self,
        src: BufId<I8>,
        scales: BufId<F32>,
        dst: BufId<F32>,
        block: usize,
    ) -> Result<(), TileError> {
        let (s, d, sc) = (self.meta(src.0), self.meta(dst.0), self.meta(scales.0));
        let elems = s.rows * s.cols;
        if block == 0 || s.rows != d.rows || s.cols != d.cols {
            return Err(TileError::Shape);
        }
        if sc.rows * sc.cols < elems.div_ceil(block) {
            return Err(TileError::Shape);
        }
        self.ops.push(OpEnc::Dequant {
            src: src.0,
            scales: scales.0,
            dst: dst.0,
            block,
        });
        Ok(())
    }

    /// Convert element format between F32 and a narrow float storage dtype
    /// (F16, Bf16, F8E4M3, F8E5M2, Tf32) - the storage half of the format
    /// (docs/TILES.md 2). Exactly one side must be F32; the other is the
    /// storage format. Same logical shape. Bit-exact and deterministic.
    pub fn cast<S: Dtyped, D: Dtyped>(
        &mut self,
        src: BufId<S>,
        dst: BufId<D>,
    ) -> Result<(), TileError> {
        let (s, d) = (self.meta(src.0), self.meta(dst.0));
        if s.rows != d.rows || s.cols != d.cols {
            return Err(TileError::Shape);
        }
        let ok = matches!(
            (s.dtype, d.dtype),
            (Dtype::F32, Dtype::F16)
                | (Dtype::F16, Dtype::F32)
                | (Dtype::F32, Dtype::Bf16)
                | (Dtype::Bf16, Dtype::F32)
                | (Dtype::F32, Dtype::F8E4M3)
                | (Dtype::F8E4M3, Dtype::F32)
                | (Dtype::F32, Dtype::F8E5M2)
                | (Dtype::F8E5M2, Dtype::F32)
                | (Dtype::F32, Dtype::Tf32)
                | (Dtype::Tf32, Dtype::F32)
        );
        if !ok {
            return Err(TileError::UnsupportedDtype);
        }
        self.ops.push(OpEnc::Cast {
            src: src.0,
            dst: dst.0,
        });
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// FNV-1a over the program's canonical encoding (ops + shapes + dtypes,
    /// NOT buffer addresses - the same program over different buffers hashes
    /// identically, which is what the autotune key wants).
    pub fn hash(&self) -> u64 {
        let mut bytes = Vec::new();
        for op in &self.ops {
            let (tag, ids, extra): (u8, [u32; 3], [usize; 3]) = match *op {
                OpEnc::Copy { src, dst } => (0, [src, dst, 0], [0, 0, 0]),
                OpEnc::Gemm { a, b, c, block } => (1, [a, b, c], [block.m, block.n, block.k]),
                OpEnc::MapShiftClamp { src, dst, shift } => (2, [src, dst, shift], [0, 0, 0]),
                OpEnc::Reduce { src } => (3, [src, 0, 0], [0, 0, 0]),
                OpEnc::Quant {
                    src,
                    dst,
                    scales,
                    block,
                } => (4, [src, dst, scales], [block, 0, 0]),
                OpEnc::Dequant {
                    src,
                    scales,
                    dst,
                    block,
                } => (5, [src, scales, dst], [block, 0, 0]),
                OpEnc::Cast { src, dst } => (6, [src, dst, 0], [0, 0, 0]),
            };
            bytes.push(tag);
            for id in ids {
                let m = self.bufs.get(id as usize);
                bytes.extend_from_slice(&(id).to_le_bytes());
                if let Some(m) = m {
                    bytes.extend_from_slice(&(m.rows as u64).to_le_bytes());
                    bytes.extend_from_slice(&(m.cols as u64).to_le_bytes());
                    bytes.push(m.dtype as u8);
                }
            }
            for e in extra {
                bytes.extend_from_slice(&(e as u64).to_le_bytes());
            }
        }
        kernels::fnv1a(&bytes)
    }
}

impl Default for TileProgram<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// What a run produced: one deterministic receipt per op (GEMM: FNV-1a of C;
/// reduce: the wrapping sum; copy/map/quant/dequant: FNV-1a of dst), plus the
/// last reduction value for convenience. Buffers carry the real output; the
/// receipts are what tests assert across executors (docs/TILES.md 6).
pub struct RunReport {
    pub reduced: u64,
    pub checksums: Vec<u64>,
}

// ============================================================================
// CpuExecutor: the library-call lowering (strand-parallel, scalar kernels).
// ============================================================================

/// Elementwise ops yield every this many elements (the tile-loop back-edge
/// rule, ACCELERATORS.md 3); [`TileSim`] counts yields with the same constant.
pub const YIELD_ELEMS: usize = 4096;

/// The CPU lowering: ops run in program order; within a GEMM, output row-tile
/// bands fan across strands (the disjoint-range pattern), and every strand
/// yields at each (m-tile, n-tile) back-edge, so a compute-bound program
/// never starves the cell's other strands and the reactor's idle ladder
/// always sees progress. Scalar inner kernels today (docs/TILES.md 4).
pub struct CpuExecutor {
    pub strands: usize,
}

impl CpuExecutor {
    pub async fn run(&self, prog: &TileProgram<'_>) -> Result<RunReport, TileError> {
        let mut checksums = Vec::with_capacity(prog.ops.len());
        let mut reduced = 0u64;
        for op in &prog.ops {
            match *op {
                OpEnc::Gemm { a, b, c, block } => {
                    let (ma, mb, mc) = (prog.meta(a), prog.meta(b), prog.meta(c));
                    match ma.dtype {
                        Dtype::I8 => gemm_banded_i8(ma, mb, mc, block, self.strands).await,
                        Dtype::F32 => gemm_banded_f32(ma, mb, mc, block, self.strands).await,
                        _ => return Err(TileError::UnsupportedDtype),
                    }
                    checksums.push(fnv_buf(mc));
                }
                OpEnc::Copy { src, dst } => {
                    let (s, d) = (prog.meta(src), prog.meta(dst));
                    copy_rows(s, d).await;
                    checksums.push(fnv_buf(d));
                }
                OpEnc::MapShiftClamp { src, dst, shift } => {
                    let (s, d) = (prog.meta(src), prog.meta(dst));
                    map_shift_clamp(s, d, shift).await;
                    checksums.push(fnv_buf(d));
                }
                OpEnc::Reduce { src } => {
                    let s = prog.meta(src);
                    reduced = reduce_buf(s).await;
                    checksums.push(reduced);
                }
                OpEnc::Quant {
                    src,
                    dst,
                    scales,
                    block,
                } => {
                    let (s, d, sc) = (prog.meta(src), prog.meta(dst), prog.meta(scales));
                    // Quantization runs whole (blocks are small); yield after.
                    // SAFETY: metas describe live bound buffers (lifetime 'p).
                    unsafe {
                        kernels::quant_f32_i8(
                            s.base_va as *const f32,
                            d.base_va as *mut i8,
                            sc.base_va as *mut f32,
                            s.rows * s.cols,
                            block,
                        );
                    }
                    rt::yield_now().await;
                    checksums.push(fnv_buf(d));
                }
                OpEnc::Dequant {
                    src,
                    scales,
                    dst,
                    block,
                } => {
                    let (s, sc, d) = (prog.meta(src), prog.meta(scales), prog.meta(dst));
                    // SAFETY: as above.
                    unsafe {
                        kernels::dequant_i8_f32(
                            s.base_va as *const i8,
                            sc.base_va as *const f32,
                            d.base_va as *mut f32,
                            s.rows * s.cols,
                            block,
                        );
                    }
                    rt::yield_now().await;
                    checksums.push(fnv_buf(d));
                }
                OpEnc::Cast { src, dst } => {
                    let (s, d) = (prog.meta(src), prog.meta(dst));
                    cast_buf(s, d);
                    rt::yield_now().await;
                    checksums.push(fnv_buf(d));
                }
            }
        }
        Ok(RunReport { reduced, checksums })
    }
}

/// Element-format conversion between F32 and a narrow storage dtype, applied
/// over the logical rows x cols window (row-wise, honoring strides). Bit-exact
/// via the canonical kernels; the pair was validated at build.
fn cast_buf(s: BufMeta, d: BufMeta) {
    for r in 0..s.rows {
        let src_row = s.base_va as usize + s.dtype.bytes(r * s.stride);
        let dst_row = d.base_va as usize + d.dtype.bytes(r * d.stride);
        for c in 0..s.cols {
            // SAFETY: element (r, c) of both bound buffers, per-dtype width.
            unsafe {
                match (s.dtype, d.dtype) {
                    (Dtype::F32, Dtype::F16) => {
                        let v = *((src_row + 4 * c) as *const f32);
                        *((dst_row + 2 * c) as *mut u16) = kernels::f32_to_f16_bits(v);
                    }
                    (Dtype::F16, Dtype::F32) => {
                        let b = *((src_row + 2 * c) as *const u16);
                        *((dst_row + 4 * c) as *mut f32) = kernels::f16_bits_to_f32(b);
                    }
                    (Dtype::F32, Dtype::Bf16) => {
                        let v = *((src_row + 4 * c) as *const f32);
                        *((dst_row + 2 * c) as *mut u16) = kernels::f32_to_bf16_bits(v);
                    }
                    (Dtype::Bf16, Dtype::F32) => {
                        let b = *((src_row + 2 * c) as *const u16);
                        *((dst_row + 4 * c) as *mut f32) = kernels::bf16_bits_to_f32(b);
                    }
                    (Dtype::F32, Dtype::F8E4M3) => {
                        let v = *((src_row + 4 * c) as *const f32);
                        *((dst_row + c) as *mut u8) = kernels::f32_to_f8e4m3_bits(v);
                    }
                    (Dtype::F8E4M3, Dtype::F32) => {
                        let b = *((src_row + c) as *const u8);
                        *((dst_row + 4 * c) as *mut f32) = kernels::f8e4m3_bits_to_f32(b);
                    }
                    (Dtype::F32, Dtype::F8E5M2) => {
                        let v = *((src_row + 4 * c) as *const f32);
                        *((dst_row + c) as *mut u8) = kernels::f32_to_f8e5m2_bits(v);
                    }
                    (Dtype::F8E5M2, Dtype::F32) => {
                        let b = *((src_row + c) as *const u8);
                        *((dst_row + 4 * c) as *mut f32) = kernels::f8e5m2_bits_to_f32(b);
                    }
                    (Dtype::F32, Dtype::Tf32) => {
                        let v = *((src_row + 4 * c) as *const f32);
                        *((dst_row + 4 * c) as *mut f32) = kernels::f32_to_tf32(v);
                    }
                    (Dtype::Tf32, Dtype::F32) => {
                        let v = *((src_row + 4 * c) as *const f32);
                        *((dst_row + 4 * c) as *mut f32) = v;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// FNV receipt over a buffer's logical rows x cols window (row-wise, so a
/// stride > cols never hashes pad bytes).
fn fnv_buf(m: BufMeta) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for r in 0..m.rows {
        let row_va = m.base_va as usize + m.dtype.bytes(r * m.stride);
        // SAFETY: the row is inside the bound buffer.
        let row =
            unsafe { core::slice::from_raw_parts(row_va as *const u8, m.dtype.bytes(m.cols)) };
        for &b in row {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// Strand-banded, block-tiled int8 GEMM. C is zeroed first (fresh-product
/// semantics); bands split the m-tile axis across strands; every strand
/// yields at each (m-tile, n-tile) back-edge - `TileSim` counts exactly
/// `ceil(m/bm) * ceil(n/bn)` yields for the whole op.
async fn gemm_banded_i8(ma: BufMeta, mb: BufMeta, mc: BufMeta, block: TileShape, strands: usize) {
    let (m, n, k) = (ma.rows, mb.cols, ma.cols);
    zero_c::<i32>(mc);
    let mtiles = m.div_ceil(block.m);
    let strands = strands.max(1).min(mtiles);
    let per_band = mtiles.div_ceil(strands);
    let mut handles = Vec::new();
    for band in 0..strands {
        let t0 = band * per_band;
        if t0 >= mtiles {
            break;
        }
        let t1 = (t0 + per_band).min(mtiles);
        let (a_va, a_s) = (ma.base_va as usize, ma.stride);
        let (b_va, b_s) = (mb.base_va as usize, mb.stride);
        let (c_va, c_s) = (mc.base_va as usize, mc.stride);
        handles.push(rt::spawn(async move {
            for mt in t0..t1 {
                let i0 = mt * block.m;
                let bm = block.m.min(m - i0);
                let mut j0 = 0;
                while j0 < n {
                    let bn = block.n.min(n - j0);
                    let mut p0 = 0;
                    while p0 < k {
                        let bk = block.k.min(k - p0);
                        // SAFETY: this band owns C rows [i0, i0+bm) - bands
                        // are disjoint in m; A/B are read-only here.
                        unsafe {
                            kernels::gemm_i8_i32(
                                (a_va as *const i8).add(i0 * a_s + p0),
                                a_s,
                                (b_va as *const i8).add(p0 * b_s + j0),
                                b_s,
                                (c_va as *mut i32).add(i0 * c_s + j0),
                                c_s,
                                bm,
                                bn,
                                bk,
                            );
                        }
                        p0 += block.k;
                    }
                    j0 += block.n;
                    // The tile-loop back-edge yield (ACCELERATORS.md 3).
                    rt::yield_now().await;
                }
            }
        }));
    }
    for h in handles {
        h.join().await;
    }
}

/// F32 GEMM (CpuExecutor only - soft-float in a cell, never in the kernel).
async fn gemm_banded_f32(ma: BufMeta, mb: BufMeta, mc: BufMeta, block: TileShape, strands: usize) {
    let (m, n, k) = (ma.rows, mb.cols, ma.cols);
    zero_c::<f32>(mc);
    let mtiles = m.div_ceil(block.m);
    let strands = strands.max(1).min(mtiles);
    let per_band = mtiles.div_ceil(strands);
    let mut handles = Vec::new();
    for band in 0..strands {
        let t0 = band * per_band;
        if t0 >= mtiles {
            break;
        }
        let t1 = (t0 + per_band).min(mtiles);
        let (a_va, a_s) = (ma.base_va as usize, ma.stride);
        let (b_va, b_s) = (mb.base_va as usize, mb.stride);
        let (c_va, c_s) = (mc.base_va as usize, mc.stride);
        handles.push(rt::spawn(async move {
            for mt in t0..t1 {
                let i0 = mt * block.m;
                let bm = block.m.min(m - i0);
                let mut j0 = 0;
                while j0 < n {
                    let bn = block.n.min(n - j0);
                    let mut p0 = 0;
                    while p0 < k {
                        let bk = block.k.min(k - p0);
                        // SAFETY: disjoint C bands, as in the i8 path.
                        unsafe {
                            let a = (a_va as *const f32).add(i0 * a_s + p0);
                            let b = (b_va as *const f32).add(p0 * b_s + j0);
                            let c = (c_va as *mut f32).add(i0 * c_s + j0);
                            for i in 0..bm {
                                for p in 0..bk {
                                    let av = *a.add(i * a_s + p);
                                    for j in 0..bn {
                                        *c.add(i * c_s + j) += av * *b.add(p * b_s + j);
                                    }
                                }
                            }
                        }
                        p0 += block.k;
                    }
                    j0 += block.n;
                    rt::yield_now().await;
                }
            }
        }));
    }
    for h in handles {
        h.join().await;
    }
}

fn zero_c<T: Default + Copy>(mc: BufMeta) {
    for r in 0..mc.rows {
        // SAFETY: row r of the bound output buffer.
        unsafe {
            let row = (mc.base_va as usize + mc.dtype.bytes(r * mc.stride)) as *mut T;
            for j in 0..mc.cols {
                *row.add(j) = T::default();
            }
        }
    }
}

async fn copy_rows(s: BufMeta, d: BufMeta) {
    let row_bytes = s.dtype.bytes(s.cols);
    let mut since_yield = 0usize;
    for r in 0..s.rows {
        // SAFETY: both rows are inside their bound buffers; distinct buffers.
        unsafe {
            core::ptr::copy_nonoverlapping(
                (s.base_va as usize + s.dtype.bytes(r * s.stride)) as *const u8,
                (d.base_va as usize + d.dtype.bytes(r * d.stride)) as *mut u8,
                row_bytes,
            );
        }
        since_yield += s.cols;
        if since_yield >= YIELD_ELEMS {
            since_yield = 0;
            rt::yield_now().await;
        }
    }
    rt::yield_now().await;
}

async fn map_shift_clamp(s: BufMeta, d: BufMeta, shift: u32) {
    let mut since_yield = 0usize;
    for r in 0..s.rows {
        // SAFETY: rows inside their bound buffers; i32 src, i8 dst.
        unsafe {
            kernels::shift_clamp_i32_i8(
                (s.base_va as usize + 4 * r * s.stride) as *const i32,
                (d.base_va as usize + r * d.stride) as *mut i8,
                s.cols,
                shift,
            );
        }
        since_yield += s.cols;
        if since_yield >= YIELD_ELEMS {
            since_yield = 0;
            rt::yield_now().await;
        }
    }
    rt::yield_now().await;
}

async fn reduce_buf(s: BufMeta) -> u64 {
    let mut acc = 0u64;
    let mut since_yield = 0usize;
    for r in 0..s.rows {
        // SAFETY: row r of the bound source buffer.
        let sum = unsafe {
            kernels::reduce_wrapping(
                (s.base_va as usize + s.dtype.bytes(r * s.stride)) as *const u8,
                s.cols,
                s.dtype as u32,
            )
        };
        acc = acc.wrapping_add(sum);
        since_yield += s.cols;
        if since_yield >= YIELD_ELEMS {
            since_yield = 0;
            rt::yield_now().await;
        }
    }
    acc
}

// ============================================================================
// EngineExecutor: the graph lowering - the device-portable artifact.
// ============================================================================

/// Lower the SAME [`TileProgram`] to dependency-graph nodes (ops 4/5,
/// docs/TILES.md 6) and submit over the queue. Engine 0 - the kernel's CPU
/// engine - executes today; a recognised device engine (GPU/NPU/TPU/FPGA)
/// returns [`TileError::EngineUnavailable`] until its contained driver cell
/// exists (GPU-HARDWARE.md 5) - never a faked run. The kernel's v1 tile
/// contract is GEMM (int8->i32 only) and reduce; other ops on this path are
/// [`TileError::UnsupportedOp`] - the CpuExecutor runs them as library calls.
pub struct EngineExecutor {
    pub engine: u64,
}

impl EngineExecutor {
    pub async fn run(&self, prog: &TileProgram<'_>) -> Result<RunReport, TileError> {
        let n = compute::Engine::count();
        if self.engine >= n || self.engine != 0 {
            return Err(TileError::EngineUnavailable(self.engine as u32));
        }
        let mut checksums = Vec::with_capacity(prog.ops.len());
        let mut reduced = 0u64;
        for op in &prog.ops {
            match *op {
                OpEnc::Gemm { a, b, c, .. } => {
                    let (ma, mb, mc) = (prog.meta(a), prog.meta(b), prog.meta(c));
                    if ma.dtype != Dtype::I8 || mc.dtype != Dtype::I32 {
                        return Err(TileError::UnsupportedDtype);
                    }
                    let desc = crate::sys::TileGemmDesc {
                        a_va: ma.base_va,
                        b_va: mb.base_va,
                        c_va: mc.base_va,
                        m: ma.rows as u32,
                        n: mb.cols as u32,
                        k: ma.cols as u32,
                        a_stride: ma.stride as u32,
                        b_stride: mb.stride as u32,
                        c_stride: mc.stride as u32,
                        dtype_in: ma.dtype as u32,
                        dtype_acc: mc.dtype as u32,
                    };
                    let mut gb = compute::GraphBuilder::new();
                    gb.tile_gemm(&desc as *const crate::sys::TileGemmDesc as u64);
                    let receipt = gb.submit().await.map_err(TileError::Kernel)?;
                    checksums.push(receipt);
                }
                OpEnc::Reduce { src } => {
                    let s = prog.meta(src);
                    let desc = crate::sys::BufReduceDesc {
                        va: s.base_va,
                        elems: (s.rows * s.cols) as u64,
                        dtype: s.dtype as u32,
                        _pad: 0,
                    };
                    // The kernel reduce is contiguous; a strided buffer's
                    // logical window is not - the library path serves it.
                    if s.stride != s.cols {
                        return Err(TileError::UnsupportedOp);
                    }
                    let mut gb = compute::GraphBuilder::new();
                    gb.buf_reduce(&desc as *const crate::sys::BufReduceDesc as u64);
                    reduced = gb.submit().await.map_err(TileError::Kernel)?;
                    checksums.push(reduced);
                }
                _ => return Err(TileError::UnsupportedOp),
            }
        }
        Ok(RunReport { reduced, checksums })
    }
}

// ============================================================================
// TileContract + TileSim + the autotune key (docs/TILES.md 3, 7, 8).
// ============================================================================

/// One memory space an engine declares, with its capacity.
#[derive(Copy, Clone, Debug)]
pub struct SpaceDesc {
    pub space: Space,
    pub bytes: usize,
}

/// What an engine declares at attach (docs/TILES.md 3). `measured == false`
/// means exactly that placement has no measured basis yet - never fabricated.
#[derive(Clone, Debug)]
pub struct TileContract {
    pub kind: EngineKind,
    pub vendor: u16,
    pub measured: bool,
    pub mma: &'static [(Dtype, TileShape)],
    pub spaces: &'static [SpaceDesc],
    pub copy_engines: u8,
    pub preemption: Preemption,
}

/// Declared-by-kind contract tables. The CPU's MMA shapes are the scalar
/// executor's natural blocks (SIMD/AMX/SME sharpen them later); device rows
/// are the portable defaults their driver cells will replace with attested,
/// measured values.
static CPU_MMA: [(Dtype, TileShape); 2] = [
    (
        Dtype::I8,
        TileShape {
            m: 16,
            n: 16,
            k: 16,
        },
    ),
    (Dtype::F32, TileShape { m: 8, n: 8, k: 8 }),
];
static CPU_SPACES: [SpaceDesc; 2] = [
    SpaceDesc {
        space: Space::Host,
        bytes: usize::MAX,
    },
    SpaceDesc {
        space: Space::Scratch,
        bytes: 1 << 20,
    },
];
static DEV_MMA: [(Dtype, TileShape); 1] = [(
    Dtype::I8,
    TileShape {
        m: 16,
        n: 16,
        k: 16,
    },
)];
static DEV_SPACES: [SpaceDesc; 3] = [
    SpaceDesc {
        space: Space::Host,
        bytes: usize::MAX,
    },
    SpaceDesc {
        space: Space::Scratch,
        bytes: 64 << 10,
    },
    SpaceDesc {
        space: Space::Device,
        bytes: 1 << 30,
    },
];

/// Build the [`TileContract`] for engine `index` from the kernel's engine
/// table (`SYS_ENGINE_INFO`) + the per-kind declared table. `None` when the
/// index is out of range.
pub fn contract_for(index: u64) -> Option<TileContract> {
    let info = compute::Engine::info_at(index)?;
    let (mma, spaces): (&'static [(Dtype, TileShape)], &'static [SpaceDesc]) = match info.kind {
        EngineKind::Cpu => (&CPU_MMA, &CPU_SPACES),
        _ => (&DEV_MMA, &DEV_SPACES),
    };
    Some(TileContract {
        kind: info.kind,
        vendor: info.vendor,
        // The CPU engine is measured by construction (svc::init runs the
        // attach benchmark before any cell exists; on ARM64 the coarse
        // counter can round the per-op cost to 0 ticks - still a
        // measurement). A device engine's zero cost means "recognised, not
        // benchmarked" (docs/GPU-HARDWARE.md 9), so only a nonzero result
        // marks it measured.
        measured: info.kind == EngineKind::Cpu || info.measured_cost_ticks != 0,
        mma,
        spaces,
        copy_engines: if info.kind == EngineKind::Cpu { 0 } else { 1 },
        preemption: info.preemption,
    })
}

/// What [`TileSim`] counts: pure, deterministic work/traffic counters - never
/// timing (docs/TILES.md 7).
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct SimReport {
    pub mma_tiles: u64,
    pub mac_ops: u64,
    pub elem_ops: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub bytes_staged: u64,
    pub tile_trips: u64,
    pub yields: u64,
}

/// The deterministic tile simulator: walks a program against a contract and
/// counts. The formulas are the executor's own loop structure, so the
/// op-count leg is checkable under icount and the traffic leg on real caches
/// (docs/TILES.md 7).
pub struct TileSim;

impl TileSim {
    pub fn simulate(prog: &TileProgram<'_>, _contract: &TileContract) -> SimReport {
        let mut r = SimReport::default();
        for op in &prog.ops {
            match *op {
                OpEnc::Gemm { a, b, c, block } => {
                    let (ma, mb, mc) = (prog.meta(a), prog.meta(b), prog.meta(c));
                    let (m, n, k) = (ma.rows, mb.cols, ma.cols);
                    let (mt, nt, kt) = (
                        m.div_ceil(block.m) as u64,
                        n.div_ceil(block.n) as u64,
                        k.div_ceil(block.k) as u64,
                    );
                    let esz_in = ma.dtype.bytes(1) as u64;
                    let esz_acc = mc.dtype.bytes(1) as u64;
                    r.mma_tiles += mt * nt * kt;
                    r.mac_ops += (m * n * k) as u64;
                    r.bytes_in += ((m * k) + (k * n)) as u64 * esz_in;
                    r.bytes_out += (m * n) as u64 * esz_acc;
                    r.bytes_staged +=
                        mt * nt * kt * ((block.m * block.k + block.k * block.n) as u64) * esz_in
                            + mt * nt * ((block.m * block.n) as u64) * esz_acc;
                    r.tile_trips += mt * nt * kt;
                    r.yields += mt * nt;
                }
                OpEnc::Copy { src, dst } => {
                    let (s, d) = (prog.meta(src), prog.meta(dst));
                    let elems = (s.rows * s.cols) as u64;
                    r.elem_ops += elems;
                    r.bytes_in += s.dtype.bytes(s.rows * s.cols) as u64;
                    r.bytes_out += d.dtype.bytes(d.rows * d.cols) as u64;
                    r.yields += elems.div_ceil(YIELD_ELEMS as u64).max(1);
                }
                OpEnc::MapShiftClamp { src, dst, .. } => {
                    let (s, d) = (prog.meta(src), prog.meta(dst));
                    let elems = (s.rows * s.cols) as u64;
                    r.elem_ops += elems;
                    r.bytes_in += (s.rows * s.cols * 4) as u64;
                    r.bytes_out += (d.rows * d.cols) as u64;
                    r.yields += elems.div_ceil(YIELD_ELEMS as u64).max(1);
                }
                OpEnc::Reduce { src } => {
                    let s = prog.meta(src);
                    let elems = (s.rows * s.cols) as u64;
                    r.elem_ops += elems;
                    r.bytes_in += s.dtype.bytes(s.rows * s.cols) as u64;
                    r.yields += elems / (YIELD_ELEMS as u64);
                }
                OpEnc::Quant { src, block, .. } => {
                    let s = prog.meta(src);
                    let elems = s.rows * s.cols;
                    r.elem_ops += elems as u64;
                    r.bytes_in += (elems * 4) as u64;
                    r.bytes_out += (elems + 4 * elems.div_ceil(block)) as u64;
                    r.yields += 1;
                }
                OpEnc::Dequant { src, block, .. } => {
                    let s = prog.meta(src);
                    let elems = s.rows * s.cols;
                    r.elem_ops += elems as u64;
                    r.bytes_in += (elems + 4 * elems.div_ceil(block)) as u64;
                    r.bytes_out += (elems * 4) as u64;
                    r.yields += 1;
                }
                OpEnc::Cast { src, dst } => {
                    let (s, d) = (prog.meta(src), prog.meta(dst));
                    let elems = s.rows * s.cols;
                    r.elem_ops += elems as u64;
                    r.bytes_in += s.dtype.bytes(elems) as u64;
                    r.bytes_out += d.dtype.bytes(elems) as u64;
                    r.yields += 1;
                }
            }
        }
        r
    }
}

/// The autotune cache key (docs/TILES.md 8): program identity x engine
/// identity x shape class. Computed now; the content-addressed cluster cache
/// it indexes is the future system service (AI-ARCHITECTURE.md 4).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AutotuneKey {
    pub program: u64,
    pub kind: u32,
    pub vendor: u16,
    pub engine_identity: u64,
    pub shape_class: u64,
}

/// Bucket a dim to its power-of-two class.
fn pow2_class(v: usize) -> u64 {
    (v.max(1)).next_power_of_two() as u64
}

pub fn autotune_key(prog: &TileProgram<'_>, c: &TileContract) -> AutotuneKey {
    let mut shape_bytes = Vec::new();
    for op in &prog.ops {
        if let OpEnc::Gemm { a, b, .. } = *op {
            let (ma, mb) = (prog.meta(a), prog.meta(b));
            shape_bytes.extend_from_slice(&pow2_class(ma.rows).to_le_bytes());
            shape_bytes.extend_from_slice(&pow2_class(mb.cols).to_le_bytes());
            shape_bytes.extend_from_slice(&pow2_class(ma.cols).to_le_bytes());
        }
    }
    let mut ident = Vec::new();
    ident.extend_from_slice(&(c.kind as u32 as u64).to_le_bytes());
    ident.extend_from_slice(&(c.vendor as u64).to_le_bytes());
    ident.extend_from_slice(&(c.measured as u64).to_le_bytes());
    AutotuneKey {
        program: prog.hash(),
        kind: c.kind as u32,
        vendor: c.vendor,
        engine_identity: kernels::fnv1a(&ident),
        shape_class: kernels::fnv1a(&shape_bytes),
    }
}
