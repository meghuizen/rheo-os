// Model checking over the **shipped** resource graph (kernel/src/hw/graph.rs), included
// verbatim (docs/RESOURCE-GRAPH.md).
//
// WHY HERE AND NOT IN A BOOT TEST. The graph is index arithmetic - contiguous per-node
// capability ranges, a free-slot search, an edge table that replaces rather than duplicates
// - and every interesting case is a boundary: a node added after another was freed, a
// capability appended out of order, a request that filters everything out, a metric that
// must be refused. A boot test builds one topology and asks a handful of questions; it
// would pass on an implementation that mis-attributes capabilities as soon as two nodes are
// created in the wrong order.
//
// The oracles are independent: a `Vec`-of-`Vec` shadow model for capability attribution, and
// arithmetic for the rest. Never the graph's own accessors - the `entity` fuzzer's first I5
// check asked the code under test whether work existed and passed while stranding work,
// because both sides agreed on a wrong answer (verify/README.md).

use std::collections::HashMap;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Owner(u16);
impl Owner {
    pub const fn cell(index: usize) -> Owner {
        Owner(index as u16)
    }
}

pub struct Funded<T: Copy> {
    slots: Vec<T>,
}

impl<T: Copy> Funded<T> {
    pub const fn new() -> Funded<T> {
        Funded { slots: Vec::new() }
    }
    pub fn set_owner(&mut self, _owner: Owner) {}
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }
    pub fn get(&self, index: usize) -> Option<T> {
        self.slots.get(index).copied()
    }
    pub fn set(&mut self, index: usize, value: T) -> bool {
        match self.slots.get_mut(index) {
            Some(s) => {
                *s = value;
                true
            }
            None => false,
        }
    }
    pub fn set_growing(&mut self, index: usize, value: T) -> bool {
        // A real growth fails when the owner's budget is exhausted; modelled with a cap so
        // the refusal path is exercised rather than assumed unreachable.
        if index >= CAP_LIMIT {
            return false;
        }
        while index >= self.slots.len() {
            self.slots.push(unsafe { std::mem::zeroed() });
        }
        self.set(index, value)
    }
}

const CAP_LIMIT: usize = 512;

mod mm {
    pub mod kmeta {
        pub use crate::{Funded, Owner};
    }
}

#[allow(dead_code)]
#[path = "../../kernel/src/hw/graph.rs"]
mod graph;

use graph::{
    Arch, CapClass, Capability, Cost, Graph, IsaSet, Metric, NodeId, NodeKind, QueryError, Reach,
    Request, Resolution, Trust,
};

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 33
}

fn perf(class: CapClass, rate: u32, detail: u64) -> Capability {
    Capability {
        class,
        reach: Reach::Inline,
        rate,
        trust: Trust::Performance,
        detail,
    }
}

fn offload(class: CapClass, rate: u32, min_bytes: u32) -> Capability {
    Capability {
        class,
        reach: Reach::Offload {
            queue_depth: 64,
            doorbell_ns: 500,
            min_bytes,
        },
        rate,
        trust: Trust::Performance,
        detail: 0,
    }
}

fn fresh() -> Graph {
    let mut g = Graph::new();
    g.init(Owner::cell(0));
    g
}

// ---------------------------------------------------------------- deterministic properties

/// Capabilities must belong to the node they were added to. The shadow model is a plain
/// map, computed here.
fn attribution() -> Result<(), String> {
    let mut g = fresh();
    let mut want: HashMap<u16, Vec<CapClass>> = HashMap::new();
    for i in 0..24u64 {
        let id = g.add_node(NodeKind::Cpu, i).ok_or("add_node")?;
        let classes = match i % 3 {
            0 => vec![CapClass::Matmul],
            1 => vec![CapClass::Crypto, CapClass::Entropy],
            _ => vec![CapClass::FloatSimd, CapClass::Dma, CapClass::Codec],
        };
        for c in &classes {
            if !g.add_capability(id, perf(*c, 100, 0)) {
                return Err(format!("add_capability refused on node {}", id.0));
            }
        }
        want.insert(id.0, classes);
    }
    if g.misordered_caps() != 0 {
        return Err(format!("{} capabilities misordered", g.misordered_caps()));
    }
    for (id, classes) in &want {
        let got: Vec<CapClass> = g.capabilities(NodeId(*id)).map(|c| c.class).collect();
        if &got != classes {
            return Err(format!(
                "node {id} offers {got:?}, expected {classes:?} - capabilities are \
                 mis-attributed across nodes"
            ));
        }
    }
    Ok(())
}

/// Adding a capability to a node that is not the most recent must be **refused and
/// counted**, never stored where it will be read as another node's.
fn misordered_is_refused() -> Result<(), String> {
    let mut g = fresh();
    let a = g.add_node(NodeKind::Cpu, 1).ok_or("a")?;
    g.add_capability(a, perf(CapClass::Matmul, 1, 0));
    let b = g.add_node(NodeKind::Cpu, 2).ok_or("b")?;
    g.add_capability(b, perf(CapClass::Crypto, 1, 0));
    // `a` is no longer the most recent; this would land in `b`'s range.
    if g.add_capability(a, perf(CapClass::Dma, 1, 0)) {
        return Err("an out-of-order capability was accepted".into());
    }
    if g.misordered_caps() != 1 {
        return Err(format!("{} counted, expected 1", g.misordered_caps()));
    }
    let a_caps: Vec<CapClass> = g.capabilities(a).map(|c| c.class).collect();
    let b_caps: Vec<CapClass> = g.capabilities(b).map(|c| c.class).collect();
    if a_caps != vec![CapClass::Matmul] || b_caps != vec![CapClass::Crypto] {
        return Err(format!(
            "the refusal still altered the graph: a={a_caps:?} b={b_caps:?}"
        ));
    }
    Ok(())
}

/// An offload below its own `min_bytes` is not a candidate - using it would be slower than
/// not having it.
fn offload_threshold() -> Result<(), String> {
    let mut g = fresh();
    let cpu = g.add_node(NodeKind::Cpu, 0).ok_or("cpu")?;
    g.add_capability(cpu, perf(CapClass::Crypto, 1_000, 0));
    let nic = g.add_node(NodeKind::Engine, 1).ok_or("nic")?;
    g.add_capability(nic, offload(CapClass::Crypto, 100_000, 4096));
    g.add_edge_both(
        cpu,
        nic,
        Cost {
            latency_ns: 900,
            bandwidth_mbs: 50_000,
            hops: 1,
            energy: 4,
        },
    );

    let small = Request {
        bytes: 64,
        ..Request::any(CapClass::Crypto)
    };
    let (who, _) = g
        .nearest(cpu, &small, Metric::Latency)
        .map_err(|e| format!("small request refused: {e:?}"))?;
    if who != cpu {
        return Err("a 64-byte request was sent to an offload with a 4096-byte floor".into());
    }
    let big = Request {
        bytes: 1 << 20,
        ..Request::any(CapClass::Crypto)
    };
    // Bandwidth is the metric that should pick the engine; latency would keep it local.
    let (who, _) = g
        .nearest(cpu, &big, Metric::Bandwidth)
        .map_err(|e| format!("large request refused: {e:?}"))?;
    if who != nic {
        return Err("a 1 MiB bandwidth-ranked request stayed on the CPU".into());
    }
    Ok(())
}

/// A class whose trust model is not a cost must **refuse** a cost metric rather than return
/// a plausible wrong answer. `nearest(Attest, Latency)` is the canonical case.
fn meaningless_metric_refused() -> Result<(), String> {
    let mut g = fresh();
    let cpu = g.add_node(NodeKind::Cpu, 0).ok_or("cpu")?;
    let tpm = g.add_node(NodeKind::TrustRoot, 1).ok_or("tpm")?;
    g.add_capability(
        tpm,
        Capability {
            class: CapClass::Attest,
            reach: Reach::Offload {
                queue_depth: 1,
                doorbell_ns: 1_000_000,
                min_bytes: 0,
            },
            rate: 0,
            trust: Trust::Attestation { chain: 7 },
            detail: 0,
        },
    );
    g.add_edge_both(cpu, tpm, Cost::LOCAL);
    match g.nearest(cpu, &Request::any(CapClass::Attest), Metric::Latency) {
        Err(QueryError::MetricMeaningless) => {}
        other => return Err(format!("ranking a TPM by latency returned {other:?}")),
    }
    // The same for entropy, which is ranked by evidence.
    let rng = g.add_node(NodeKind::Device, 2).ok_or("rng")?;
    g.add_capability(
        rng,
        Capability {
            class: CapClass::Entropy,
            reach: Reach::Inline,
            rate: 1_000_000,
            trust: Trust::Entropy {
                health_tested: true,
                assessed_bits: 7,
            },
            detail: 0,
        },
    );
    match g.nearest(cpu, &Request::any(CapClass::Entropy), Metric::Bandwidth) {
        Err(QueryError::MetricMeaningless) => {}
        other => return Err(format!("ranking entropy by bandwidth returned {other:?}")),
    }
    Ok(())
}

/// **Several RNGs feed one DRBG**: every source is returned, including an unassessed one,
/// labelled rather than dropped. A design that returned only "the best" would make the seed
/// depend on a single source, which is what SP 800-90C combining exists to prevent.
fn entropy_aggregates() -> Result<(), String> {
    let mut g = fresh();
    let cpu = g.add_node(NodeKind::Cpu, 0).ok_or("cpu")?;
    g.add_capability(
        cpu,
        Capability {
            class: CapClass::Entropy,
            reach: Reach::Inline,
            rate: 500,
            trust: Trust::Entropy {
                health_tested: true,
                assessed_bits: 6,
            },
            detail: 0,
        },
    );
    let vrng = g.add_node(NodeKind::Device, 1).ok_or("vrng")?;
    g.add_capability(
        vrng,
        Capability {
            class: CapClass::Entropy,
            reach: Reach::Offload {
                queue_depth: 8,
                doorbell_ns: 2_000,
                min_bytes: 0,
            },
            rate: 100_000,
            trust: Trust::Entropy {
                health_tested: false,
                assessed_bits: 0,
            },
            detail: 0,
        },
    );
    let jitter = g.add_node(NodeKind::Cpu, 2).ok_or("jitter")?;
    g.add_capability(
        jitter,
        Capability {
            class: CapClass::Entropy,
            reach: Reach::Inline,
            rate: 10,
            trust: Trust::Entropy {
                health_tested: true,
                assessed_bits: 2,
            },
            detail: 0,
        },
    );

    let sources: Vec<(NodeId, Capability)> = g.entropy_sources().collect();
    if sources.len() != 3 {
        return Err(format!(
            "{} entropy sources reported, expected 3 - aggregation dropped one",
            sources.len()
        ));
    }
    // The unassessed, fastest source must be present AND labelled, so a conditioner can
    // include it without counting its bits.
    let un = sources
        .iter()
        .find(|(id, _)| *id == vrng)
        .ok_or("the unassessed source was dropped")?;
    match un.1.trust {
        Trust::Entropy {
            health_tested: false,
            assessed_bits: 0,
        } => {}
        t => return Err(format!("the unassessed source reports {t:?}")),
    }
    // And the slowest health-tested source must not have been discarded for being slow.
    if !sources.iter().any(|(id, _)| *id == jitter) {
        return Err("the slow but health-tested source was dropped".into());
    }
    Ok(())
}

/// A bit-exact contract refuses a numerically-different provider rather than ranking it
/// down, even when it is nearer and faster.
fn bit_exact_refuses_numeric() -> Result<(), String> {
    let mut g = fresh();
    let cpu = g.add_node(NodeKind::Cpu, 0).ok_or("cpu")?;
    // detail bit 63 = "numerically different from the reference" (an FMA contraction).
    g.add_capability(cpu, perf(CapClass::Matmul, 100_000, 1 << 63));
    let slow = g.add_node(NodeKind::Engine, 1).ok_or("slow")?;
    g.add_capability(slow, perf(CapClass::Matmul, 10, 0));
    g.add_edge_both(
        cpu,
        slow,
        Cost {
            latency_ns: 10_000,
            bandwidth_mbs: 10,
            hops: 3,
            energy: 9,
        },
    );

    let loose = Request::any(CapClass::Matmul);
    let (who, _) = g
        .nearest(cpu, &loose, Metric::Latency)
        .map_err(|e| format!("loose request refused: {e:?}"))?;
    if who != cpu {
        return Err(
            "without a bit-exact contract the far faster provider was not chosen - a \
             resolution ranking dominated cost, preferring exactness the caller did not ask for"
                .into(),
        );
    }
    let strict = Request {
        bit_exact: true,
        ..Request::any(CapClass::Matmul)
    };
    let (who, r) = g
        .nearest(cpu, &strict, Metric::Latency)
        .map_err(|e| format!("bit-exact request refused entirely: {e:?}"))?;
    if who != slow {
        return Err(
            "a bit-exact contract still chose the numerically-different provider - a fast \
             different answer was preferred to a slow exact one"
                .into(),
        );
    }
    if r != Resolution::Native {
        return Err(format!("the exact provider resolved as {r:?}"));
    }
    Ok(())
}

/// A binary's ISA baseline is a filter, not a ranking: a v4 request must not be placed on a
/// v3 node however near it is. In a mixed-generation cluster that is a SIGILL, not a
/// slowdown.
fn isa_filter() -> Result<(), String> {
    let mut g = fresh();
    let v3 = g.add_node(NodeKind::Cpu, 0).ok_or("v3")?;
    g.set_isa(
        v3,
        IsaSet {
            arch: Arch::X86_64,
            baseline: 3,
            features: 0b1,
        },
    );
    g.add_capability(v3, perf(CapClass::Matmul, 1_000, 0));
    let v4 = g.add_node(NodeKind::Cpu, 1).ok_or("v4")?;
    g.set_isa(
        v4,
        IsaSet {
            arch: Arch::X86_64,
            baseline: 4,
            features: 0b11,
        },
    );
    g.add_capability(v4, perf(CapClass::Matmul, 10, 0));
    g.add_edge_both(
        v3,
        v4,
        Cost {
            latency_ns: 5_000,
            bandwidth_mbs: 100,
            hops: 2,
            energy: 5,
        },
    );

    let want_v4 = Request {
        isa: IsaSet {
            arch: Arch::X86_64,
            baseline: 4,
            features: 0b10,
        },
        ..Request::any(CapClass::Matmul)
    };
    let (who, _) = g
        .nearest(v3, &want_v4, Metric::Latency)
        .map_err(|e| format!("v4 request refused: {e:?}"))?;
    if who != v4 {
        return Err("a v4 binary was placed on a v3 node".into());
    }
    // Cross-architecture must be refused outright, not ranked.
    let want_arm = Request {
        isa: IsaSet {
            arch: Arch::Aarch64,
            baseline: 1,
            features: 0,
        },
        ..Request::any(CapClass::Matmul)
    };
    match g.nearest(v3, &want_arm, Metric::Latency) {
        Err(QueryError::AllFiltered) => {}
        other => return Err(format!("an aarch64 request on an x86 machine returned {other:?}")),
    }
    Ok(())
}

/// The three refusals must be distinguishable: nothing offers the class, everything was
/// filtered, and the metric is wrong are different answers a caller acts on differently.
fn refusals_distinct() -> Result<(), String> {
    let mut g = fresh();
    let cpu = g.add_node(NodeKind::Cpu, 0).ok_or("cpu")?;
    match g.nearest(cpu, &Request::any(CapClass::Codec), Metric::Latency) {
        Err(QueryError::NoProvider) => {}
        other => return Err(format!("no provider at all returned {other:?}")),
    }
    g.add_capability(cpu, offload(CapClass::Codec, 10, 1 << 20));
    match g.nearest(cpu, &Request::any(CapClass::Codec), Metric::Latency) {
        Err(QueryError::AllFiltered) => {}
        other => return Err(format!("a provider filtered by threshold returned {other:?}")),
    }
    match g.nearest(NodeId(9999), &Request::any(CapClass::Codec), Metric::Latency) {
        Err(QueryError::NoNode) => {}
        other => return Err(format!("an absent origin returned {other:?}")),
    }
    Ok(())
}

/// `cost` is a node to itself, a direct edge, or nothing - **never a chained path**, because
/// chaining is the shortest-path search this module refuses to contain.
fn cost_is_not_transitive() -> Result<(), String> {
    let mut g = fresh();
    let a = g.add_node(NodeKind::MemoryNode, 0).ok_or("a")?;
    let b = g.add_node(NodeKind::MemoryNode, 1).ok_or("b")?;
    let c = g.add_node(NodeKind::MemoryNode, 2).ok_or("c")?;
    let one = Cost {
        latency_ns: 100,
        bandwidth_mbs: 1000,
        hops: 1,
        energy: 1,
    };
    g.add_edge_both(a, b, one);
    g.add_edge_both(b, c, one);
    if g.cost(a, a) != Some(Cost::LOCAL) {
        return Err("a node to itself is not LOCAL".into());
    }
    if g.cost(a, b) != Some(one) {
        return Err("a direct edge was not returned".into());
    }
    if g.cost(a, c).is_some() {
        return Err("cost(a,c) answered through b - the graph computed a path".into());
    }
    Ok(())
}

/// An edge recorded twice replaces rather than duplicates, so a firmware table read twice
/// does not double the graph.
fn edges_replace() -> Result<(), String> {
    let mut g = fresh();
    let a = g.add_node(NodeKind::MemoryNode, 0).ok_or("a")?;
    let b = g.add_node(NodeKind::MemoryNode, 1).ok_or("b")?;
    let first = Cost {
        latency_ns: 100,
        bandwidth_mbs: 1,
        hops: 1,
        energy: 1,
    };
    let second = Cost {
        latency_ns: 200,
        bandwidth_mbs: 2,
        hops: 2,
        energy: 2,
    };
    g.add_edge(a, b, first);
    g.add_edge(a, b, second);
    if g.edge_count() != 1 {
        return Err(format!("{} edges after two writes of one pair", g.edge_count()));
    }
    if g.cost(a, b) != Some(second) {
        return Err("the second write did not replace the first".into());
    }
    Ok(())
}

/// A degraded graph - one node, no distances - must answer "everything is equally near",
/// which is exactly correct for a machine whose firmware describes nothing, and must report
/// its source honestly.
fn degraded_is_usable() -> Result<(), String> {
    let mut g = fresh();
    let only = g.add_node(NodeKind::MemoryNode, 0).ok_or("only")?;
    if g.source() != graph::Source::None {
        return Err("a graph built from nothing claims a source".into());
    }
    if g.cost(only, only) != Some(Cost::LOCAL) {
        return Err("a single node is not local to itself".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------- randomised model

/// Build a random graph and check the invariants that must hold of any of them.
fn random_model(seed: u64) -> Result<(), String> {
    let mut st = seed;
    let mut g = fresh();
    let mut shadow: Vec<(u16, Vec<CapClass>)> = Vec::new();
    let classes = [
        CapClass::Matmul,
        CapClass::Crypto,
        CapClass::Entropy,
        CapClass::Dma,
        CapClass::Codec,
    ];
    let kinds = [
        NodeKind::Cpu,
        NodeKind::MemoryNode,
        NodeKind::Device,
        NodeKind::Engine,
        NodeKind::TrustRoot,
    ];
    let n = 4 + (lcg(&mut st) % 20) as usize;
    for i in 0..n {
        let kind = kinds[(lcg(&mut st) as usize) % kinds.len()];
        let Some(id) = g.add_node(kind, i as u64) else {
            continue;
        };
        let ncaps = (lcg(&mut st) % 4) as usize;
        let mut mine = Vec::new();
        for _ in 0..ncaps {
            let c = classes[(lcg(&mut st) as usize) % classes.len()];
            let cap = if lcg(&mut st) % 2 == 0 {
                perf(c, (lcg(&mut st) % 1000) as u32, 0)
            } else {
                offload(c, (lcg(&mut st) % 100000) as u32, (lcg(&mut st) % 8192) as u32)
            };
            if g.add_capability(id, cap) {
                mine.push(c);
            }
        }
        shadow.push((id.0, mine));
    }
    // Random edges.
    for _ in 0..(n * 2) {
        let a = NodeId((lcg(&mut st) % n as u64) as u16);
        let b = NodeId((lcg(&mut st) % n as u64) as u16);
        g.add_edge(
            a,
            b,
            Cost {
                latency_ns: (lcg(&mut st) % 100_000) as u32,
                bandwidth_mbs: (lcg(&mut st) % 100_000) as u32,
                hops: (lcg(&mut st) % 8) as u8,
                energy: (lcg(&mut st) % 16) as u8,
            },
        );
    }

    // Invariant 1: capability attribution survives everything above.
    for (id, mine) in &shadow {
        let got: Vec<CapClass> = g.capabilities(NodeId(*id)).map(|c| c.class).collect();
        if &got != mine {
            return Err(format!(
                "seed {seed}: node {id} offers {got:?}, shadow says {mine:?}"
            ));
        }
    }
    if g.misordered_caps() != 0 {
        return Err(format!("seed {seed}: capabilities were misordered"));
    }

    // Invariant 2: `nearest` never returns a node that `resolve` calls Unavailable, and
    // never returns one filtered by threshold.
    for c in classes {
        let req = Request {
            bytes: (lcg(&mut st) % 16384) as u32,
            ..Request::any(c)
        };
        if c == CapClass::Entropy {
            // Ranking entropy by a cost metric must be refused, not answered.
            if let Ok((who, _)) = g.nearest(NodeId(0), &req, Metric::Latency) {
                if g.capabilities(who).any(|k| k.class == CapClass::Entropy) {
                    return Err(format!("seed {seed}: entropy was ranked by latency"));
                }
            }
            continue;
        }
        if let Ok((who, _)) = g.nearest(NodeId(0), &req, Metric::Hops) {
            if g.resolve(who, &req) == Resolution::Unavailable {
                return Err(format!(
                    "seed {seed}: nearest returned {} which resolve calls Unavailable",
                    who.0
                ));
            }
            // And it must actually offer the class.
            if !g.capabilities(who).any(|k| k.class == c) {
                return Err(format!(
                    "seed {seed}: nearest returned {} which does not offer {c:?}",
                    who.0
                ));
            }
        }
    }

    // Invariant 3: providers is exactly the set resolve accepts - the two must not diverge.
    for c in classes {
        let req = Request::any(c);
        let from_providers: Vec<u16> = g.providers(&req).map(|i| i.0).collect();
        let by_hand: Vec<u16> = (0..g.node_count() as u16 + 8)
            .filter(|i| g.get(NodeId(*i)).is_some())
            .filter(|i| g.resolve(NodeId(*i), &req) != Resolution::Unavailable)
            .collect();
        if from_providers != by_hand {
            return Err(format!(
                "seed {seed}: providers {from_providers:?} != resolve-accepted {by_hand:?}"
            ));
        }
    }
    Ok(())
}

fn main() {
    let mut failures = 0usize;
    println!("== resource graph: deterministic properties ==");
    for (name, r) in [
        ("capabilities belong to the node they were added to", attribution()),
        ("an out-of-order capability is refused and counted", misordered_is_refused()),
        ("an offload below its own floor is not a candidate", offload_threshold()),
        ("a meaningless metric is refused, not answered", meaningless_metric_refused()),
        ("several RNGs are aggregated, none dropped", entropy_aggregates()),
        ("a bit-exact contract refuses a numeric provider", bit_exact_refuses_numeric()),
        ("an ISA baseline filters rather than ranks", isa_filter()),
        ("the three refusals are distinguishable", refusals_distinct()),
        ("cost is direct or nothing, never a path", cost_is_not_transitive()),
        ("an edge written twice replaces", edges_replace()),
        ("a degraded graph is usable and honest", degraded_is_usable()),
    ] {
        match r {
            Ok(()) => println!("  ok   {name}"),
            Err(e) => {
                println!("  FAIL {name}: {e}");
                failures += 1;
            }
        }
    }

    println!("== resource graph: randomised topologies ==");
    let mut bad = 0;
    for run in 0..5_000u64 {
        if let Err(e) = random_model(0xA5A5 ^ run.wrapping_mul(0x9E3779B97F4A7C15)) {
            if bad == 0 {
                println!("  FAIL {e}");
            }
            bad += 1;
        }
    }
    if bad == 0 {
        println!("  ok   5000 random topologies, 4..24 nodes");
    } else {
        failures += 1;
    }

    if failures > 0 {
        println!("graph fuzz: FAIL ({failures} properties)");
        std::process::exit(1);
    }
    println!("graph fuzz: PASS");
}
