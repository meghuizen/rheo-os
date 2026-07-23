# Emulation

**Status:** Draft v0.1. Relates to POSIX-PERSONALITY.md (same-ISA legacy),
TARGET-ARCHITECTURES.md (ISA matrix), TOOLING.md (QEMU in CI).

"Emulation" covers four distinct things people conflate. Each has a different
answer and a different cost. The doctrine throughout: emulation is a cell
capability like any other - contained, metered, honest about its overhead.

## 1. The four kinds

| Kind | Question it answers | Answer |
|---|---|---|
| **Development emulation** | Can we build/test Lattice without a hardware lab? | QEMU full-system, the primary dev + CI platform |
| **Guest mode** | Can Lattice run as a VM on a cloud/hypervisor? | Yes, supported, with stated degradations |
| **Cross-ISA user binaries** | Can an ARM64 Linux binary run on an x86-64 Lattice host? | Yes, user-mode dynamic translation inside a POSIX cell |
| **Deterministic simulation** | Can we replay a distributed bug exactly? | A simulation harness for the control-plane protocols |

## 2. Development emulation (QEMU full-system)

- The CI platform for all three ISAs (TARGET-ARCHITECTURES.md 7): every
  commit boots x86-64-v3, ARM64 (with SVE/MTE), and RVA23 under QEMU and
  runs the capability-core tests, loom/fuzz corpora, and P1-P5
  microbenchmark trends.
- QEMU models the hardware floor pieces that matter: virtual IOMMU
  (viommu), TPM (swtpm), NVMe, virtio-net with queues. This lets almost the
  entire OS - capability core, cells, scheduling, I/O, cluster protocols -
  be developed and tested before hardware exists.
- What QEMU cannot faithfully model: real accelerator engines, PTP-grade
  timing, and true DMA performance. Those gate on the hardware lab (M1+).

## 3. Guest mode (Lattice as a VM)

- A supported production and development mode (TARGET-ARCHITECTURES.md 6)
  with honest degradations recorded in the host's attestation report:
  - Virtual IOMMU quality varies by hypervisor; isolation guarantees are only
    as strong as the vIOMMU.
  - Clock error bound e is worse without hardware PTP; lease windows widen
    accordingly (BOOT.md, SECURITY-IDENTITY.md 3).
  - Accelerators require passthrough (VFIO-style) to be real engines;
    para-virtual GPUs are treated as degraded-trust engines.
  - Entropy is hypervisor-fed (virtio-rng) plus local jitter, marked as such
    in attestation so cluster policy decides what such hosts may run.
- Lattice **hosting** VMs is out of scope initially - cells are the isolation
  unit, and adding a hypervisor role is a large surface with no fleet demand
  yet. Revisit only if a hard sub-cell tenant boundary is required (then it
  arrives as an engine-like "vCPU engine" + guest memory kind, in doctrine).

## 4. Cross-ISA user binaries

- A foreign-ISA Linux binary (e.g. ARM64 on x86-64) runs via a **user-mode
  dynamic binary translator** (QEMU-user / Rosetta-class) hosted *inside* a
  POSIX personality cell (POSIX-PERSONALITY.md 7). The translator is just
  code in a cell; its syscalls hit the personality's POSIX translation, which
  hits native primitives.
- Cost is honest and large (translation overhead, no acceleration for the
  translated ISA's vector units unless mapped); this is a compatibility
  convenience for occasional foreign binaries, not a performance path.
- Native multi-ISA is the real answer: build for the target ISA (a Cargo
  `--target` away, ARCHITECTURE.md section 7), and cross-ISA emulation is the
  fallback for binaries you cannot rebuild.

## 5. Deterministic simulation (the valuable one)

- The control-plane protocols - epoch revocation, leases under partition,
  state-store watch ordering, placement - are exactly the code that fails in
  rare interleavings. A **deterministic simulation harness** runs many cells'
  logic on a single thread against a virtual clock and a scriptable network
  (drop, delay, partition, reorder), FoundationDB-style.
- This complements, does not replace, the TLA+ models (ARCHITECTURE.md 8.1)
  and the Jepsen-style hardware tests (8.3): TLA+ proves the protocol,
  simulation finds implementation divergence from it cheaply and
  reproducibly, Jepsen validates on real hosts.
- Because the whole syscall model is async queues over shared memory,
  swapping the queue transport for a simulated one is a clean seam - the same
  cell code runs in production, in simulation, and cross-host without change.
  This is a direct dividend of doctrine 9 (local and distributed are one
  mechanism).

## 6. What emulation is not used for

- Not a substitute for the hardware lab on anything timing-, DMA-, or
  accelerator-sensitive (kill thresholds P6-P10, P12 gate on real silicon).
- Not a security boundary weaker than the cell: an emulator/translator is a
  contained cell, so a malicious foreign binary is bounded by that cell's
  grants exactly like native code.
