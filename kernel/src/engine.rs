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

use crate::time;

/// The integer operations an engine can execute. A dependency-graph node
/// carries one of these (docs/IO.md, docs/ARCHITECTURE.md 3 object 6).
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
        }
    }
}
