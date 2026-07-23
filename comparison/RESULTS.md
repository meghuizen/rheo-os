# Concept validation results - rheo-os vs seL4

**Date:** 2026-07-23.
**Environment:** QEMU 8.2.2 (Ubuntu), `-icount shift=0,align=off,sleep=off`
(deterministic: every number below reproduces exactly).
**rheo-os:** release build, pinned nightly (rust-toolchain.toml), this
commit. **seL4:** sel4bench master (kernel `5126e718b`, sel4bench
`a0f099b`), PLATFORM=qemu-arm-virt, AARCH64, RELEASE, FASTPATH - built and
run with `comparison/sel4/run-sel4bench.sh`.

All numbers are **instruction path lengths** (instructions executed per
operation), not wall-clock time. Read comparison/README.md for why that is
the only honest QEMU metric and how it maps to the hardware targets.

## 1. Correctness: the concepts hold

`cargo xtask test --arch all` - green on x86-64, ARM64, RISC-V 64:

| Concept | Test kernel | Result |
|---|---|---|
| Unforgeability (ARCHITECTURE.md 8.2 #1) | cap-invariants | pass, all 3 ISAs |
| Monotonic attenuation (#2), runtime + compile-time | cap-invariants | pass, all 3 ISAs |
| Revocation soundness (#3), O(1) epoch bump | cap-invariants | pass, all 3 ISAs |
| Isolation via per-cell tables (#4), delegation as move | cap-invariants | pass, all 3 ISAs |
| Budget-metered capabilities (object 2) | cap-invariants | pass, all 3 ISAs |
| Queue ABI: batched submit, one doorbell (IO.md 1) | queue-pipeline | pass, all 3 ISAs |
| Flow context preserved through completions (obj. 10) | queue-pipeline | pass, all 3 ISAs |
| Per-entry grant check on the data path | queue-pipeline | pass, all 3 ISAs |
| Mid-stream revocation goes dark immediately | queue-pipeline | pass, all 3 ISAs |

## 2. The three numbers (docs/IO.md 7), instructions per operation

`cargo xtask bench --arch all`, best batch of 32x1024 ops (variance under
icount is zero; ARM64 ticks converted at the measured 16 insn/tick):

| Benchmark | x86-64 | ARM64 | RISC-V 64 |
|---|---|---|---|
| P1 grant check (typed handle) | 27 | 26 | 24 |
| P1 grant check (32-bit ABI form) | 26 | 25 | 24 |
| P1 deny path (revoked cap) | 19 | 22 | 21 |
| Ring push + pop (transport only, no kernel) | 75 | 71 | 79 |
| Doorbell trap (privilege round trip floor) | 39 | 48 | 63 |
| P2 queue round trip, single message | 211 | 200 | 255 |
| P2 queue round trip, batched 64/doorbell | 154 | 140 | 160 |
| P3 context switch (same-cell, cooperative) | 21 | 25 | 41 |

## 3. Head-to-head with seL4 (same QEMU, same machine, same icount)

seL4 fastpath IPC, aarch64 qemu-arm-virt, same-vspace, IPC length 0,
prio 254 (sel4bench "One way IPC microbenchmarks", stddev 0 under icount):

| Operation | seL4 | rheo-os (ARM64) |
|---|---|---|
| seL4_Call (one way) | 190 | - |
| seL4_ReplyRecv (one way) | 216 | - |
| **Synchronous round trip** | **406** | **200** (single-message queue RT) |
| **Amortized message cost, batched** | n/a (PPC does not batch) | **140** (64 per doorbell) |

Under plain TCG (no icount) seL4's same numbers are medians ~2506/~2829
"cycles" with ~50% stddev - which is why RESULTS.md quotes only the
deterministic icount runs for both systems.

### What this does and does not show

- **The queue-ABI bet survives its first falsification attempt.** The
  design concedes (docs/IO.md 6.1) that seL4's synchronous PPC is
  near-optimal for single cross-domain calls and expects to pay a penalty
  there, betting on batching. Measured: the rheo-os single-message round
  trip (200 insns) is currently *shorter* than seL4's PPC round trip
  (406), and batching brings it to 140/message - so the mechanism's path
  lengths are in the right region, not just defensibly behind.
- **The comparison is not yet fully apples-to-apples, in seL4's favor.**
  seL4's 406 includes two full protection-domain crossings with thread
  switches; the rheo-os 200 includes two real exception-level round trips
  (svc + eret) and all per-entry capability work, but *no address-space
  switch and no user mode* - cells run kernel-side until BUILD-ORDER.md
  steps 3/5 land. The gap that will close: an ASID/PCID-tagged address
  space switch is tens of instructions of path length, which still leaves
  the batched number (140) well under seL4's single-shot 406 - but that
  claim must be re-measured, not assumed, once cells are user-mode.
- **seL4's numbers are its own strength.** seL4's IPC carries a proved
  kernel behind it; rheo-os's capability core is runtime-tested (section
  1) but not yet proved (Verus, BUILD-ORDER.md step 4 gate).

## 4. Mapping to the hardware targets (ARCHITECTURE.md 8.4)

Path length is the leading indicator, not the verdict - cache/TLB
behaviour on real hardware decides finally (docs/TOOLING.md 4):

| Gate | Hardware target | Kill | Path length measured | Reading |
|---|---|---|---|---|
| P1 grant check | < 50 ns p99 | > 150 ns | 24-27 insns | ~8-13 cycles of work at IPC 2-3; passes with big margin unless cache-hostile |
| P2 single | < 1 us | > 3 us | 200-255 insns | orders of magnitude of headroom |
| P2 batched | < 100 ns amortized | > 300 ns | 140-160 insns | plausible at ~2 GHz; tight; hardware decides |
| P3 same-cell switch | < 200 ns | > 500 ns | 21-41 insns | floor only - the real number needs vcore state + runtime switch |

No gate can be *passed* in QEMU; none is *killed* by these numbers, and
P2-batched is the one to watch on hardware.

## 5. Verdict on viability, as of this commit

- The capability model (single mechanism: typed, delegatable,
  epoch-revocable, budget-metered) **works as specified** across three
  ISAs and its hot path is ~25 instructions - the concept is viable and
  cheap enough to sit under every kernel interaction, as the design
  requires.
- The queue-pair-as-only-syscall concept **holds its cost story** against
  the strongest incumbent comparison the design names: batched cost is
  ~1/3 of seL4's single-shot IPC path even before amortizing further, and
  the single-shot penalty the design accepted did not materialize at
  current scope.
- Mid-stream revocation and flow-context tracing - the two properties
  Linux-shaped systems retrofit painfully - **fall out of the ABI**, as
  claimed, and are enforced/preserved on the measured hot path (the
  per-entry grant check is included in every P2 number above).
- **Open before "viable" becomes "validated":** user-mode cells behind
  real address spaces (steps 3/5), the Verus proofs (step 4 gate), and
  hardware-lab numbers for P1-P3 (milestone M1). The next comparison to
  run after step 5: this same table, cross-address-space on both sides.
