//! Dependency graphs (docs/ARCHITECTURE.md 3 object 6, docs/IO.md): work
//! nodes across engines, with edges expressing data dependencies and
//! conditional edges for speculative/routing work. This is the object a
//! pipeline compiles to - "a pipeline in lsh is a dependency graph
//! submitted to the kernel" (docs/SHELL.md 1).
//!
//! Single-host scope: nodes carry an integer op (engine::Op) and reference
//! earlier nodes as inputs (so the build order is a topological order).
//! The kernel executes the graph on an engine and returns per-node
//! results. Timeline semaphores, cross-engine transfer nodes, and
//! yield/budget contracts on unbounded nodes are later work.

use crate::engine::{Engine, Op};

pub const MAX_NODES: usize = 32;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum GraphError {
    /// A node referenced an input that is not an earlier node.
    BadEdge,
    /// Too many nodes for the fixed-capacity graph.
    Full,
}

/// One work node: an op and up to two inputs. An input is either an
/// immediate value or the result of an earlier node (by index).
#[derive(Copy, Clone)]
pub struct Node {
    pub op: Op,
    pub a: Input,
    pub b: Input,
}

#[derive(Copy, Clone)]
pub enum Input {
    Imm(u64),
    Node(usize),
}

/// A fixed-capacity dependency graph.
pub struct Graph {
    nodes: [Node; MAX_NODES],
    len: usize,
}

impl Graph {
    pub const fn new() -> Graph {
        Graph {
            nodes: [Node {
                op: Op::Const(0),
                a: Input::Imm(0),
                b: Input::Imm(0),
            }; MAX_NODES],
            len: 0,
        }
    }

    /// Append a node; returns its index. Inputs referencing later nodes
    /// are rejected (the build order must be topological).
    pub fn push(&mut self, op: Op, a: Input, b: Input) -> Result<usize, GraphError> {
        if self.len >= MAX_NODES {
            return Err(GraphError::Full);
        }
        for input in [a, b] {
            if let Input::Node(idx) = input
                && idx >= self.len
            {
                return Err(GraphError::BadEdge);
            }
        }
        let idx = self.len;
        self.nodes[idx] = Node { op, a, b };
        self.len += 1;
        Ok(idx)
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Execute the whole graph on `engine`, writing each node's result
    /// into `results` (length must be >= len). Returns the last node's
    /// result (the graph's output).
    pub fn run(&self, engine: &Engine, results: &mut [u64]) -> u64 {
        for i in 0..self.len {
            let node = &self.nodes[i];
            let a = self.resolve(node.a, results);
            let b = self.resolve(node.b, results);
            results[i] = engine.exec(node.op, a, b);
        }
        if self.len == 0 {
            0
        } else {
            results[self.len - 1]
        }
    }

    fn resolve(&self, input: Input, results: &[u64]) -> u64 {
        match input {
            Input::Imm(v) => v,
            Input::Node(idx) => results[idx],
        }
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}
