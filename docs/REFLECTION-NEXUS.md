# Reflection — The Nexus Proposal, Scrutinised

**Status:** Draft v0.1. A critical comparison against an alternative green-field
design ("Nexus") proposed independently, and the refinements it justifies.
Relates to SCHEDULING.md, ACCELERATORS.md, REALTIME.md, and the doctrines in
ARCHITECTURE.md 2.

## 0. Summary judgment

Nexus and Lattice converge on the same spine — capabilities, dataflow graphs,
queue pairs, typed memory, poison propagation, live inspection, Rust,
compatibility-as-personalities. Nexus explicitly borrows several concrete
Lattice decisions. Where they *diverge*, Nexus is a **superset that re-adds
things Lattice deliberately rejected**, and on scrutiny the rejections hold.

This document does not rewrite Lattice. It records why the divergences fail,
and folds in the three places where Nexus exposed a genuine gap in Lattice.
The bar for "rewrite everything" was: does any Nexus divergence beat a Lattice
doctrine on Lattice's own goals? The answer, after simulation, is no — but it
sharpened three under-specified areas.

## 1. Point-by-point scrutiny

| Nexus divergence | Lattice position | Verdict |
|---|---|---|
| ML-driven predictive scheduling in the scheduler | Deterministic EDF admission; ML advises placement only, never enforces | **Reject Nexus.** Simulated below (§2). A prediction is not a proof; RT guarantees require proof. |
| "No priorities, pure reservation" (absolute) | Hard/soft/elastic reservations | **Refine Lattice.** Nexus's absolutism exposed a missing tier: best-effort residual work (§3). |
| Quantum / neuromorphic / molecular engines as headline | Engine model is open but unstated for probabilistic engines | **Refine Lattice.** One paragraph, not a redesign (§4). Nexus over-invests in speculation. |
| Cyclic graphs with feedback loops | DAG only | **Refine Lattice.** Feedback lives in time (periodic re-submission), not graph structure (§5). |
| Kernel on a dedicated safety island / co-processor | Verified capability core on the main cores | **Reject Nexus.** Adds a trust boundary and blows the grant-check latency budget (§6). |
| Hot paths on user-accessible engines "when safe" | Two-level scheduling + userspace data plane | **No change.** This is already Lattice, stated more vaguely. |
| ZK proofs for identity "where useful" | Attestation + short-lived signed capabilities | **No change / minor.** ZK is a tool for specific privacy cases, not a base primitive; noted in §7. |
| "AI-native: OS uses models for security anomaly detection, energy opt" | Observability + controllers as cells | **Partial adopt.** As advisory controller cells (§7), never in enforcement. |

## 2. Simulation — the ML scheduler failure

The highest-stakes divergence. Nexus places ML-driven predictive modelling in
the scheduler. Simulate it against Lattice's own hardest requirement: a
Priority::Critical real-time control loop (REALTIME.md §7).

### Scenario

An industrial motor controller: 1 ms period, 400 µs budget, deadline 800 µs,
`Priority::Critical`. Co-located on a host with bursty ML inference and batch
analytics. The scheduler must guarantee the control loop's deadline.

### Lattice (deterministic EDF admission)

```
[admission] control-loop task: C=400µs, P=1ms → U=0.40
[admission] EDF check on latency pool: sum(Ci/Pi)=0.40 ≤ 0.90 (10% reserved)
[admission] ACCEPTED — mathematically guaranteed to meet every deadline
[runtime]   every period: dedicated tickless core, strand runs immediately
[runtime]   worst-case jitter: 2-10µs (SMI is the only deviation, surfaced)
[proof]     the guarantee is a theorem: U ≤ 1 ⟹ no missed deadline (EDF)
[t+3h]      analytics workload spikes to 100% of its elastic budget
[result]    control loop UNAFFECTED — its reservation is hard, not elastic
```

The guarantee survives the spike because it is a proof about the reserved
core, not a prediction about behaviour.

### Nexus (ML predictive placement)

```
[model]   trained on observed workload patterns; places tasks to
          "predicted optimal" engines/cores
[t=0]     control loop placed well; model has seen this pattern
[t+3h]    a workload mix the model has NOT seen: analytics + inference +
          a GC pause pattern that correlates with the control loop's period
[model]   mispredicts available headroom; co-locates control loop with a
          bursty neighbour on a shared core "because historically that was fine"
[result]  control loop misses its 800µs deadline by 240µs during the burst
[physical] the motor overshoots; mechanical damage or safety-stop
[debug]   "why did the model place it there?" → no causal answer.
          The decision is distributed across weights. There is no audit
          trail, only a model version and an input vector.
```

The failure mode is precisely the one the model cannot prevent: the novel
input. And by construction, novel inputs cluster around incidents — the
exact moments you most need the guarantee. The ML scheduler is optimising
for the *average* case at the cost of the *worst* case, and real-time is a
worst-case discipline.

### Conclusion

ML in the enforcement path is rejected. The refinement it justifies: ML/heuristic
models may run as **advisory placement-hint controller cells** (SCHEDULING.md
already allows this) — they suggest placements for *soft and elastic* work, the
suggestions pass through the same deterministic admission math, and a rejected
or mispredicted hint costs nothing because the math is the backstop. Hard and
Critical reservations never consult a model. This keeps the average-case win
(better packing of best-effort work) without risking the worst-case guarantee.

**This was already Lattice's position; the simulation confirms it against the
strongest version of the counter-argument.**

## 3. Refinement — the residual best-effort tier

Nexus's "pure reservation, no priorities" absolutism exposed a real gap.
Lattice has hard/soft/elastic reservations, but what runs work that is
*legitimately unschedulable as a reservation* yet *must eventually run* — a
background `updatedb`, log rotation, a nightly report, a spare-cycle batch job?

Pure reservation rejects it (it has no deadline to admit against). Pure
priority (Nexus rightly rejects priority) would let it starve or be starved.
Neither answer is right.

### The fix: a residual pool that consumes only slack

Add a fourth reservation class below elastic:

```rust
pub enum ReservationClass {
    Hard,        // guaranteed, admission-checked, never revoked
    Soft,        // guaranteed budget, degrades proportionally under pressure
    Elastic,     // floor + reclaimable ceiling (pressure events)
    Residual,    // NO guarantee; consumes only slack cycles the pools leave idle
}
```

`Residual` work:
- Is admitted always (it promises nothing, so there is nothing to check).
- Runs only when a pool would otherwise idle — it consumes the 10% latency-pool
  safety margin and the shared pool's unused capacity, but yields instantly
  (at the next yield point) when any reserved work becomes runnable.
- Is metered so a runaway residual job is visible, but cannot starve anything
  because it is definitionally lowest and preemptible-first.
- Makes progress guarantees only statistically ("will complete when the system
  has spare capacity"), which is the honest contract for best-effort work.

This is not priority in disguise: there is exactly one residual tier, it cannot
be tuned to compete with reserved work, and reserved work is never scheduled
against it — residual simply mops up slack. It closes the gap Nexus's absolutism
revealed without reintroducing the priority-inversion pathologies priorities
carry.

**Action: add `ReservationClass::Residual` to SCHEDULING.md 4 and the admission
model.**

## 4. Refinement — probabilistic and non-deterministic engines

Nexus makes quantum/neuromorphic/molecular computing a headline. Designing
quantum-specific kernel machinery in 2026 is speculative theatre — the hardware,
error models, and control interfaces are not settled. But Nexus is right that a
30-year OS must not *foreclose* them.

The honest position: **the engine contract (ACCELERATORS.md 1) already spans
them; make that explicit and stop.** An engine is queues + IOMMU + attestation
+ a declared execution contract. Extend the execution contract vocabulary:

```rust
pub enum ExecutionContract {
    Deterministic,                    // CPU, GPU, NPU, DMA — result is exact
    Probabilistic {                   // quantum, neuromorphic, analog
        result_carries_confidence: bool,   // completion includes error/confidence
        requires_classical_control: bool,  // e.g. quantum: classical setup + readout
        repeatable: bool,                   // same input → same output? (usually no)
    },
}
```

A probabilistic engine's completion entry carries an error rate / confidence
in its status field (the `NodeStatus` already has room, OPEN-QUESTIONS.md §2).
A quantum node in a graph is: classical-control-node → quantum-node
(probabilistic, non-preemptible, N shots) → classical-readout-node. The graph
model already expresses this — it is a DAG with a probabilistic node in the
middle whose output edge carries a confidence value. Neuromorphic inference is
an engine whose output is a probability distribution, consumed by a downstream
node that thresholds or samples it.

**No new kernel objects. No quantum-specific scheduler. One extended enum and
one paragraph.** This is the difference between designing for the future
(keeping the abstraction open) and speculating about it (building machinery for
hardware that does not exist). Lattice does the former; Nexus drifts toward the
latter.

**Action: add `ExecutionContract` to ACCELERATORS.md engine definition; add
one worked example of a quantum node in a graph.**

## 5. Refinement — feedback loops without cyclic graphs

Nexus proposes cyclic graphs for control systems. This is a real requirement
(control loops have feedback) but the wrong mechanism. A cyclic dependency
graph breaks three things Lattice relies on:

- **Poison propagation** (forward-only requires a DAG; a cycle has no "forward").
- **Deadlock/cycle detection** (a legitimate cycle is indistinguishable from a
  bug cycle).
- **Topological scheduling** (a cycle has no topological order).

The correct model: **the cycle lives in time, not in the graph.** A control
loop is a periodic re-submission of a DAG whose inputs include the previous
iteration's outputs (state carried across iterations). This is exactly the
`PeriodicTask` + carried state pattern (REALTIME.md §4, §7):

```
Iteration N:    [read sensor] → [compute control(state_N, sensor)] → [actuate]
                                        ↓ produces state_{N+1}
Iteration N+1:  [read sensor] → [compute control(state_{N+1}, sensor)] → [actuate]
```

The "feedback edge" (state_{N+1} feeding back into iteration N+1) is not a graph
edge — it is a value carried in the strand's local state from one periodic
activation to the next. Each iteration is an acyclic graph. The loop is the
periodic re-submission.

This preserves every DAG property while expressing arbitrary feedback control.
It also matches how real control software is actually written (a control loop
is a function called every period with retained state), so the model maps
directly onto practitioner intuition.

**Action: add a "feedback and control loops" note to OPEN-QUESTIONS.md or
REALTIME.md making explicit that cycles are temporal, not structural.**

## 6. Rejection — the safety-island kernel

Nexus proposes running the kernel control plane on "a small trusted computing
base, possibly on a dedicated safety island or co-processor."

This sounds safer and is worse:

- **It adds a cross-processor trust boundary to the grant-check hot path.**
  The grant check must be <50ns p99 (P1). A round trip to a co-processor is
  hundreds of ns to µs. The safety island would blow the single most important
  latency budget in the system.
- **It solves a problem verification already solves.** The capability core is
  small (~3-5k lines) specifically so it can be formally verified on the main
  cores (KERNEL-RUST.md §5, seL4 precedent). A verified core on the main cores
  is as trustworthy as one on an island, without the boundary cost.
- **It fragments the design.** Now there are two execution environments, two
  toolchains, two attestation roots. The minimal-composable-core thesis
  (one kernel, everything else cells) is broken.

A safety island is the right answer when you *cannot* verify the control plane
(so you isolate it in hardware instead). Lattice can verify it. The island is
solving Lattice's problem with hardware Lattice does not need.

**Verdict: reject. The verified core runs on the main cores.** (A confidential-
compute enclave for the *attestation root key* is a different, legitimate thing
— that is key custody, not the control plane, and VIRTUALIZATION.md §7 already
covers it.)

## 7. Partial adopt — AI for observability, ZK for specific privacy

Two Nexus ideas are good in a bounded form:

- **AI for security anomaly detection and energy optimisation:** yes, as
  **advisory controller cells** reading the event stream (OBSERVABILITY.md).
  An anomaly-detection cell watches capability-grant patterns and flags
  outliers to an operator; an energy-optimisation cell suggests DVFS/consolidation
  policies to the power subsystem (POWER.md). Both *advise*; neither *enforces*.
  A flagged anomaly does not auto-revoke (that would let a mispredicting model
  cause an outage); it raises an operator alert. This is already consistent with
  Lattice doctrine — worth stating explicitly as a supported pattern.

- **Zero-knowledge proofs for identity:** a tool for specific cases (proving a
  property of a workload without revealing it — e.g. "this cell runs an approved
  image" without revealing which), not a base primitive. The base identity is
  attestation + short-lived signed capabilities (SECURITY-IDENTITY.md). ZK is an
  optional attestation *mode* for privacy-sensitive federation, added when a
  concrete use case demands it. Noted as a future extension, not adopted into
  the core.

## 8. What Nexus got right that Lattice already had

Worth acknowledging: the Nexus document independently arrived at queue pairs,
poison tokens, typed `BufferRef`, live graph inspection, cells-replace-
everything, capabilities-not-ambient-authority, reservation-based scheduling,
Rust, and compatibility-as-personalities. This convergence is evidence the core
design is not idiosyncratic — two independent green-field efforts aimed at the
same goals land on the same spine. That is reassuring for the fundamentals.

## 9. Net changes to the Lattice plan

Three refinements adopted, three divergences rejected, everything else
unchanged. No rewrite.

**Adopted:**
1. `ReservationClass::Residual` — a slack-only best-effort tier (§3) →
   SCHEDULING.md.
2. `ExecutionContract` extended for probabilistic engines (§4) →
   ACCELERATORS.md.
3. Feedback-as-temporal (cycles live in re-submission, not graph structure)
   made explicit (§5) → REALTIME.md.
4. AI-as-advisory-controller and ZK-as-optional-attestation-mode noted as
   supported patterns (§7) → OBSERVABILITY.md / SECURITY-IDENTITY.md.

**Rejected (with reasons that strengthen the doctrines):**
1. ML in the scheduler enforcement path — a prediction is not a proof (§2).
2. Safety-island kernel — blows the grant-check budget; verification already
   solves it (§6).
3. Quantum-specific kernel machinery — speculative; the open engine abstraction
   is the correct future-proofing (§4).

The exercise did not find a better foundation. It found the strongest available
counter-argument, simulated it, and confirmed the foundation holds — while
sharpening four under-specified areas. That is the right outcome for a
"scrutinise even if we rewrite everything" pass: the willingness to rewrite was
real, the evidence did not demand it, and the design is better for having been
pushed.
