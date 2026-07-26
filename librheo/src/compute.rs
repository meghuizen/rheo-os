//! Parallel & accelerated compute (docs/LIBRHEO.md Phase C, docs/ARCHITECTURE.md
//! 3 objects 4 & 6). Two surfaces, both async-first:
//!
//! - **strands as M:N parallel workers**: [`map_reduce`]/[`parallel_for`]/
//!   [`scan`] fan work across N strands over the Phase A executor. On the
//!   single-CPU cooperative runtime these strands interleave rather than run on
//!   separate cores (SMP work-stealing is task #27); the surface is the parallel
//!   *decomposition*, and the aggregate is exact. Buffers come from `mem`
//!   ([`Grant`](crate::mem::Grant)/[`Arena`](crate::mem::Arena)), which hand out
//!   aligned, SIMD-friendly slices.
//! - **engine/graph submission**: [`Engine::info`] reports which executor the
//!   cell runs on (attest-by-measurement, object 4), and [`GraphBuilder`] builds
//!   a dependency graph in userspace and submits it to the kernel's CPU engine
//!   over the async queue ([`sys::OP_GRAPH_SUBMIT`]), awaiting the result. The
//!   CPU engine is the only real engine here; GPU/NPU accelerators land behind
//!   the same API (attested firmware, documented future work).

use alloc::vec::Vec;

use crate::rt;
use crate::sys::{self, GraphNode};

// ============================================================================
// Strands as M:N parallel workers.
// ============================================================================

/// Map a range in `parts` partitions across strands, then reduce the partials.
/// `map(lo, hi)` computes a partition's partial over `[lo, hi)`; `reduce`
/// combines two partials; `id` is the reduction identity. The classic parallel
/// aggregation (a columnar `SUM`/`COUNT`/`MAX`, a warehouse rollup).
pub async fn map_reduce<R, M, F>(len: usize, parts: usize, map: M, reduce: F, id: R) -> R
where
    R: Copy + 'static,
    M: Fn(usize, usize) -> R + Copy + 'static,
    F: Fn(R, R) -> R,
{
    let parts = parts.max(1);
    let chunk = len.div_ceil(parts);
    let mut handles = Vec::new();
    for p in 0..parts {
        let lo = p * chunk;
        if lo >= len {
            break;
        }
        let hi = (lo + chunk).min(len);
        handles.push(rt::spawn(async move { map(lo, hi) }));
    }
    let mut acc = id;
    for h in handles {
        acc = reduce(acc, h.join().await);
    }
    acc
}

/// Run `body(lo, hi)` over `parts` partitions of `[0, len)` across strands, for
/// its side effects (a parallel loop with no reduction). `body` must operate on
/// disjoint ranges - the partitions never overlap.
pub async fn parallel_for<B>(len: usize, parts: usize, body: B)
where
    B: Fn(usize, usize) + Copy + 'static,
{
    map_reduce(len, parts, body, |_, _| (), ()).await;
}

/// In-place inclusive prefix sum (scan) of `data` across `parts` strands: a
/// three-phase blocked scan (parallel per-block sums, a sequential exclusive
/// prefix of the block totals, then parallel per-block offset add). Each strand
/// owns a disjoint block, so the raw-pointer writes never alias.
pub async fn scan(data: &mut [u64], parts: usize) {
    let len = data.len();
    if len == 0 {
        return;
    }
    let parts = parts.max(1).min(len);
    let chunk = len.div_ceil(parts);
    let base = data.as_mut_ptr() as usize;

    // Phase 1: local inclusive prefix per block; collect each block's total.
    let mut h1 = Vec::new();
    for p in 0..parts {
        let lo = p * chunk;
        if lo >= len {
            break;
        }
        let hi = (lo + chunk).min(len);
        h1.push(rt::spawn(async move {
            let mut acc = 0u64;
            for i in lo..hi {
                // SAFETY: `[lo, hi)` is this strand's disjoint block within `data`.
                unsafe {
                    let q = (base as *mut u64).add(i);
                    acc = acc.wrapping_add(*q);
                    *q = acc;
                }
            }
            acc
        }));
    }
    let mut totals = Vec::new();
    for h in h1 {
        totals.push(h.join().await);
    }

    // Phase 2: exclusive prefix of the block totals (sequential, tiny).
    let mut offsets = Vec::new();
    let mut off = 0u64;
    for t in &totals {
        offsets.push(off);
        off = off.wrapping_add(*t);
    }

    // Phase 3: add each block's offset to its elements, in parallel.
    let mut h3 = Vec::new();
    for (p, &offv) in offsets.iter().enumerate() {
        if offv == 0 {
            continue; // the first (and any all-zero) block needs no fixup
        }
        let lo = p * chunk;
        let hi = (lo + chunk).min(len);
        h3.push(rt::spawn(async move {
            for i in lo..hi {
                // SAFETY: disjoint block, as above.
                unsafe {
                    let q = (base as *mut u64).add(i);
                    *q = (*q).wrapping_add(offv);
                }
            }
        }));
    }
    for h in h3 {
        h.join().await;
    }
}

// ============================================================================
// Engine introspection (attest-by-measurement, object 4).
// ============================================================================

/// The kind of executor a cell's compute runs on. Only [`Cpu`](EngineKind::Cpu)
/// is real in QEMU; the accelerator kinds are the same-API future (attested
/// firmware, docs/LIBRHEO.md Phase C / docs/ACCELERATORS.md).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EngineKind {
    Cpu,
    Gpu,
    Npu,
    Other,
}

/// Whether the engine can be preempted per-instruction (the CPU) or only at op
/// boundaries (a typical accelerator).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Preemption {
    Instruction,
    OpBoundary,
}

/// What the kernel MEASURED for the engine at attach: its kind, its per-op cost
/// in kernel ticks (a measurement, never a vendor claim), its preemption
/// contract, and - for a device engine - its PCI vendor ID (0x10DE NVIDIA,
/// 0x1002 AMD, 0x8086 Intel, 0x1AF4 virtio; 0 for the CPU). A GPU engine's
/// measured cost is 0 until a driver cell can execute on it - enumerated
/// and registered, honestly not benchmarked (docs/GPU-HARDWARE.md 9).
#[derive(Copy, Clone, Debug)]
pub struct EngineInfo {
    pub kind: EngineKind,
    pub measured_cost_ticks: u64,
    pub preemption: Preemption,
    pub vendor: u16,
}

fn decode(raw: sys::EngineInfo) -> EngineInfo {
    EngineInfo {
        kind: match raw.kind {
            0 => EngineKind::Cpu,
            1 => EngineKind::Gpu,
            2 => EngineKind::Npu,
            _ => EngineKind::Other,
        },
        measured_cost_ticks: raw.measured_cost_ticks,
        preemption: if raw.preemption == 0 {
            Preemption::Instruction
        } else {
            Preemption::OpBoundary
        },
        vendor: raw.vendor as u16,
    }
}

/// A handle to the compute engine a cell offloads graphs to. Engine 0 is
/// the kernel's CPU engine; the rest are the PCIe-enumerated GPUs.
pub struct Engine;

impl Engine {
    /// Report the engine's measured throughput + kind + preemption contract
    /// (`SYS_ENGINE_INFO`). "What executor am I on?" answered by measurement.
    pub fn info() -> EngineInfo {
        decode(sys::engine_info())
    }

    /// How many engines the kernel registered (CPU + recognised GPUs).
    pub fn count() -> u64 {
        sys::engine_count()
    }

    /// Report engine `index` from the kernel's table, or `None` when the
    /// index is out of range.
    pub fn info_at(index: u64) -> Option<EngineInfo> {
        let (n, raw) = sys::engine_info_at(index);
        if index < n { Some(decode(raw)) } else { None }
    }
}

// ============================================================================
// Userspace dependency-graph submission (object 6).
// ============================================================================

/// A reference to a node in a [`GraphBuilder`] (the index of an appended node).
#[derive(Copy, Clone)]
pub struct NodeRef(u64);

/// An input to a graph node: an immediate value or an earlier node's result.
#[derive(Copy, Clone)]
pub enum In {
    Imm(u64),
    Node(NodeRef),
}

impl In {
    fn encode(self) -> (u32, u64) {
        match self {
            In::Imm(v) => (0, v),
            In::Node(n) => (1, n.0),
        }
    }
}

/// Builds a dependency graph in userspace and submits it to the CPU engine
/// (docs/ARCHITECTURE.md 3 object 6). Nodes are appended in topological order
/// (an input may only reference an earlier node); [`submit`](GraphBuilder::submit)
/// hands the whole node list to the kernel over the async queue and awaits the
/// computed result. Arithmetic nodes today (Const/Add/Mul/Select); a
/// buffer-reduce/map node kind is documented future work.
pub struct GraphBuilder {
    nodes: Vec<GraphNode>,
}

impl GraphBuilder {
    pub fn new() -> GraphBuilder {
        GraphBuilder { nodes: Vec::new() }
    }

    fn push(&mut self, op: u32, a: In, b: In) -> NodeRef {
        let (a_is_node, a_val) = a.encode();
        let (b_is_node, b_val) = b.encode();
        let idx = self.nodes.len() as u64;
        self.nodes.push(GraphNode {
            op,
            a_is_node,
            b_is_node,
            _pad: 0,
            a: a_val,
            b: b_val,
        });
        NodeRef(idx)
    }

    /// A constant-producing node.
    pub fn constant(&mut self, v: u64) -> NodeRef {
        self.push(0, In::Imm(v), In::Imm(0))
    }
    /// `a + b`.
    pub fn add(&mut self, a: In, b: In) -> NodeRef {
        self.push(1, a, b)
    }
    /// `a * b`.
    pub fn mul(&mut self, a: In, b: In) -> NodeRef {
        self.push(2, a, b)
    }
    /// Select: `cond` if `cond != 0` else 0 (a conditional edge - MoE routing /
    /// speculative decoding, object 6). The kernel engine's `Select` returns the
    /// second input when the first is non-zero.
    pub fn select(&mut self, cond: In, val: In) -> NodeRef {
        self.push(3, cond, val)
    }
    /// Buffer-carrying reduce node (op 4, docs/TILES.md 6): `desc_va` is the
    /// cell VA of a [`sys::BufReduceDesc`]; the node's result is the wrapping
    /// sum. The descriptor must stay alive until [`submit`](Self::submit)
    /// completes.
    pub fn buf_reduce(&mut self, desc_va: u64) -> NodeRef {
        self.push(4, In::Imm(desc_va), In::Imm(0))
    }
    /// Tiled int8 GEMM node (op 5, docs/TILES.md 6): `desc_va` is the cell VA
    /// of a [`sys::TileGemmDesc`]; the node's result is the FNV-1a receipt of
    /// C. The descriptor and buffers must stay alive until submit completes.
    pub fn tile_gemm(&mut self, desc_va: u64) -> NodeRef {
        self.push(5, In::Imm(desc_va), In::Imm(0))
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Submit the graph to the CPU engine and await its result: the value of the
    /// last node (the graph output). `Err(status)` if the kernel rejected the
    /// graph (a malformed edge, an empty/oversized graph) - the completion
    /// status. Zero-copy: the node list and result buffer are the cell's own
    /// heap memory, read/written in place by the kernel during the doorbell drain.
    pub async fn submit(&self) -> Result<u64, u32> {
        let count = self.nodes.len();
        if count == 0 {
            return Err(sys::STATUS_BAD_OPCODE);
        }
        let mut results: Vec<u64> = alloc::vec![0u64; count];
        let mut args = [0u8; 24];
        args[0..8].copy_from_slice(&(self.nodes.as_ptr() as u64).to_le_bytes());
        args[8..12].copy_from_slice(&(count as u32).to_le_bytes());
        args[12..20].copy_from_slice(&(results.as_mut_ptr() as u64).to_le_bytes());
        let cqe = rt::submit_and_await(sys::OP_GRAPH_SUBMIT, args).await;
        if cqe.status == sys::STATUS_OK {
            Ok(results[count - 1])
        } else {
            Err(cqe.status)
        }
    }
}

impl Default for GraphBuilder {
    fn default() -> Self {
        Self::new()
    }
}
