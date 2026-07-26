//! Engines (docs/ARCHITECTURE.md 3 object 4, docs/ACCELERATORS.md 1): any
//! executor. The doctrine is *measured, not claimed* - throughput is
//! benchmarked at attach and recorded, and a preemption contract is
//! declared up front.
//!
//! Single-host scope: a CPU compute engine that executes the integer ops a
//! dependency graph is built from. Real GPU/NPU/DPU engines as contained
//! driver cells, attested firmware, and spatial partitioning are Phase 4+
//! work (BUILD-ORDER.md steps 11, 18); the *contract* - attach-time
//! measurement + declared preemption - is exercised here so the object is
//! real, not a placeholder.

use crate::abi::{BufReduceDesc, TileGemmDesc};
use crate::time;

/// The canonical scalar tile kernels (docs/TILES.md) - a SOURCE include of
/// the file librheo ships, not a cargo dependency (the kernel's zero-deps
/// rule holds; the file is dependency-free by contract). The library
/// executor, this engine, bench-core, and the host comparison all run the
/// same bytes.
#[path = "../../librheo/src/tile/kernels.rs"]
mod tile_kernels;

/// The integer operations an engine can execute. A dependency-graph node
/// carries one of these (docs/IO.md, docs/ARCHITECTURE.md 3 object 6).
/// `BufReduce`/`TileGemm` are the buffer-carrying tile ops (docs/TILES.md
/// 6): their descriptors are validated by `svc::graph_submit` before an
/// `Op` is ever built, so `exec` runs them without re-checking.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Op {
    /// Produce a constant.
    Const(u64),
    /// a + b.
    Add,
    /// a * b.
    Mul,
    /// Select a if cond != 0 else b (a conditional edge - speculative
    /// decoding / MoE routing, docs/ARCHITECTURE.md 3 object 6).
    Select,
    /// Wrapping u64 sum over a cell buffer (op 4). Returns the sum.
    BufReduce(BufReduceDesc),
    /// Tiled int8 -> i32 GEMM over cell buffers (op 5). Zeroes C, runs the
    /// whole (bounded) product, returns the FNV-1a receipt of C.
    TileGemm(TileGemmDesc),
}

/// Whether an engine can be preempted mid-op (declared, not discovered).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Preemption {
    /// Preemptible at any instruction (the CPU compute engine).
    Instruction,
    /// Preemptible only at op boundaries (typical accelerator).
    OpBoundary,
}

/// A CPU compute engine. `measured_cost` is filled by `attach` - it is a
/// measurement, never a vendor claim.
pub struct Engine {
    pub preemption: Preemption,
    measured_cost_ticks: u64,
    attached: bool,
}

impl Engine {
    pub const fn cpu() -> Engine {
        Engine {
            preemption: Preemption::Instruction,
            measured_cost_ticks: 0,
            attached: false,
        }
    }

    /// Attach the engine: benchmark a known op stream and record the
    /// measured per-op cost. This is the attest-by-measurement contract.
    pub fn attach(&mut self) {
        const CAL: u64 = 4096;
        let start = time::monotonic();
        let mut acc = 0u64;
        let mut i = 0u64;
        while i < CAL {
            acc = self.exec(Op::Add, acc, 1);
            i += 1;
        }
        let elapsed = time::monotonic().wrapping_sub(start);
        core::hint::black_box(acc);
        self.measured_cost_ticks = elapsed / CAL;
        self.attached = true;
    }

    pub fn measured_cost_ticks(&self) -> u64 {
        self.measured_cost_ticks
    }
    pub fn is_attached(&self) -> bool {
        self.attached
    }

    /// Execute one op with two inputs (unused inputs pass 0).
    ///
    /// The tile ops run synchronously and bounded (docs/TILES.md 6): their
    /// descriptors passed `svc::graph_submit` validation (VAs non-zero, dims
    /// capped, dtypes exact), so the worst node is 16.7M MACs. The userspace
    /// executor owns the yield rule; this engine is the graph-lowered path.
    pub fn exec(&self, op: Op, a: u64, b: u64) -> u64 {
        match op {
            Op::Const(v) => v,
            Op::Add => a.wrapping_add(b),
            Op::Mul => a.wrapping_mul(b),
            Op::Select => {
                if a != 0 {
                    b
                } else {
                    0
                }
            }
            Op::BufReduce(d) => {
                // SAFETY: the descriptor passed validation; `va` is the
                // submitting cell's mapped buffer, live during the drain
                // (the same trust contract as `nodes_va` itself).
                unsafe {
                    tile_kernels::reduce_wrapping(d.va as *const u8, d.elems as usize, d.dtype)
                }
            }
            Op::TileGemm(d) => {
                let (m, n, k) = (d.m as usize, d.n as usize, d.k as usize);
                let cs = d.c_stride as usize;
                // SAFETY: validated descriptor over the cell's live mappings,
                // as above; C rows are within the cell's buffer.
                unsafe {
                    for r in 0..m {
                        let row = (d.c_va as *mut i32).add(r * cs);
                        for j in 0..n {
                            *row.add(j) = 0;
                        }
                    }
                    tile_kernels::gemm_i8_i32(
                        d.a_va as *const i8,
                        d.a_stride as usize,
                        d.b_va as *const i8,
                        d.b_stride as usize,
                        d.c_va as *mut i32,
                        cs,
                        m,
                        n,
                        k,
                    );
                    // The FNV-1a receipt over C's logical m x n window
                    // (row-wise: a stride > n never hashes pad bytes).
                    let mut h = 0xcbf2_9ce4_8422_2325u64;
                    for r in 0..m {
                        let row = core::slice::from_raw_parts(
                            (d.c_va as *const i32).add(r * cs) as *const u8,
                            n * 4,
                        );
                        for &byte in row {
                            h ^= byte as u64;
                            h = h.wrapping_mul(0x0000_0100_0000_01b3);
                        }
                    }
                    h
                }
            }
        }
    }
}
