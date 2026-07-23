# Tooling

**Status:** Draft v0.1. Expands ARCHITECTURE.md section 7 and the
verification plan (section 8).

Principle: a novel OS cannot also afford a novel build system. The language
choices are opinionated (Rust, one IDL, formal proofs on the core); the
tooling around them is deliberately boring and off-the-shelf wherever
possible.

## 1. Languages and why

- **Rust** for the kernel and all privileged components - because ownership
  *is* the capability model (non-Clone moves = unforgeable transfer),
  generics encode typed memory, typestate encodes queue disciplines, and
  `no_std` + async fit a control-plane kernel. Unsafe is concentrated in
  small audited crates (~2-5%: page tables, MMIO, DMA descriptors) behind
  safe wrappers.
- **Assembly** only for boot, exception vectors, and the context-switch inner
  loop - a few files per ISA, everything else behind the Arch trait
  (TARGET-ARCHITECTURES.md 4).
- **Not C++** (safety arrives too late), **not Zig yet** (pre-1.0, wrong risk
  for a decade-long kernel; revisit for tooling), **no managed language in
  the kernel** (GC in the grant-check path is a non-starter).
- **Userspace is polyglot**; system services in Rust for the same invariant-
  encoding reasons; controllers in Rust or Go (Go's Kubernetes-ecosystem
  gravity is worth exploiting at the compat edge); **WASM** is a first-class
  cell type for controllers, policy, probes, and dataplane programs.

## 2. The IDL and ABI

- One system **IDL** (FIDL-inspired: Cap'n-Proto-style arena layout,
  protobuf-style evolution rules, handle-typed fields) defines the frozen
  C-ABI kernel surface and all typed control-plane messages, generating
  Rust / C / Go bindings from a single source.
- The kernel's Tier-1 ABI (submission entries, capability handles) is
  fixed-layout `repr(C)`, versioned, and never parsed - decode is a cast plus
  validation. Signed cross-host tokens use deterministic CBOR (canonical
  encoding mandatory).

## 3. Build

- **Cargo workspaces** with `build-std` for the bare-metal kernel targets;
  cross-compilation is a `--target` invocation (`aarch64-unknown-none`,
  `riscv64gc-unknown-none`, x86-64 variant) - any port needing more than an
  Arch trait implementation is treated as an architecture bug.
- The **system image** is a content-addressed signed manifest (BOOT.md 2) -
  not a package tree. Reproducible builds are a hard requirement: the same
  source produces the same object hashes, which is what makes attestation and
  A/B rollback meaningful.

## 4. CI

- **QEMU-first on all three ISAs every commit** (EMULATION.md 2): boot,
  capability-core test suite, loom permutations, fuzz corpora, P1-P5
  microbenchmark trend tracking (absolute perf numbers gate on hardware).
- **Hardware lab from M1**: Xeon/EPYC node, Graviton/Ampere node, NVIDIA GPU
  node (AMD added at M5), an RDMA pair, a PTP switch. P6-P12 kill thresholds
  gate here (ARCHITECTURE.md 8.4).
- Gates are pass/fail against thresholds, not "tune until green" - a red
  performance cell re-examines the mechanism or withdraws the claim.

## 5. Correctness tooling

- **Verus** for the capability core proofs (Kani as fallback), scoped to
  ~3-5k lines (ARCHITECTURE.md 8.2); seL4-as-bottom-layer is the documented
  escape hatch if the proof effort overruns ~2 person-years.
- **loom** for every lock-free structure (rings, Chase-Lev deques, epoch
  reclamation).
- **Miri** + MTE/ASAN-class runs on all unsafe crates, CI-gated.
- **Structure-aware fuzzing**, continuous, on: submission-entry parsers, the
  cryptographic capability codec (the single most attacked surface), IDL
  decoders, the WASM verifier. Any kernel panic/invariant-break from fuzz
  input is a release blocker.
- **TLA+** models for revocation, leases-under-partition, and watch ordering,
  checked before implementation (8.1).
- **Deterministic simulation** harness (EMULATION.md 5) for reproducible
  distributed-protocol testing; **Jepsen-style** suite for the state store on
  real hosts (8.3).

## 6. Debugging and observability tooling

This is where the observability foundation (ARCHITECTURE.md 4.10) pays off as
developer experience:

- **Flow-ID tracing** end to end: a request's journey across cells, engines,
  DMA, and hosts is one causally-ordered trace, exported to standard OTLP
  backends (Grafana/Tempo/Jaeger) via an exporter cell. `kubectl`-style
  tooling and OTel dashboards work day one.
- **WASM dynamic probes** (the DTrace/eBPF role): verified, budgeted,
  attached under audited debug grants, type-checked against event schemas.
  Same machinery as the network dataplane programs (NETWORKING.md 4).
- **Runtime introspection**: because the kernel sees vcores, not the 100k
  strands, each runtime exports a standard introspection capability (strand
  dumps, wait-for graphs, per-strand accounting) - without it, 100k strands
  are unobservable, so it is mandatory, not optional (ARCHITECTURE.md 4.3).
- **Interactive debugging** (stop a strand, read memory, single-step) is a
  debug grant on a specific cell - powerful and gated, versus Linux's global
  ptrace-scope sysctl fight.
- Perf-regression gates on the three numbers a control-plane kernel lives on:
  grant check, queue round trip, context switch.

## 7. CLI and operator surface

- A native CLI speaks the typed state store directly (typed objects, not YAML
  string-slinging), with the Kubernetes-API compat edge providing `kubectl`
  for existing muscle memory and GitOps flows.
- Manifests: native uses the IDL's typed objects; the edge accepts YAML
  through a tightened parser (no anchor/alias bombs, no implicit typing - the
  Norway problem stays outside, ARCHITECTURE.md 4.9 / formats).
- Fleet operations are desired-state pushes reconciled per host; reimaging a
  node is manifest-download + reboot (BOOT.md 7), so there is no per-node
  mutable config for tooling to drift.

## 8. What is deliberately not built

- No custom editor, no bespoke package manager, no novel VCS - Git, standard
  editors, and existing GitOps tools are the workflow.
- No custom container registry format beyond content-addressed objects - OCI
  imports through a converter (CONTAINERS-KUBERNETES.md 1).
- No home-grown metrics/tracing protocol - OpenTelemetry/OTLP at the edge is
  the interoperability contract, generated from the event streams the system
  already produces.
