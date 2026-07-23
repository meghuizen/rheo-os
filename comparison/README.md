# comparison/ - Concept validation against seL4

This directory holds the methodology, the reproduction scripts, and the
measured results for validating the core rheo-os concepts against seL4 -
the system the design itself names as the honest benchmark
(docs/IO.md 6.1, docs/ARCHITECTURE.md 8.2).

## What is being validated

Two different things, and the distinction matters:

1. **Correctness of the concepts.** The four capability-core proof
   properties (unforgeability, monotonic attenuation, revocation
   soundness, isolation) and the queue-pair ABI semantics (per-entry grant
   checks, flow-context propagation, mid-stream revocation) run as in-QEMU
   test kernels on all three ISAs on every commit: `cargo xtask test`.
   These pass/fail - no interpretation needed.

2. **Performance shape of the mechanisms.** "The three numbers" a
   control-plane kernel lives on (docs/IO.md 7): P1 grant check, P2 queue
   round trip, P3 context switch. `cargo xtask bench` measures them.

## Measurement methodology (read before quoting numbers)

Absolute wall-clock performance only gates on the hardware lab
(docs/TOOLING.md 4). QEMU TCG timing is not hardware timing: there are no
real caches, no TLB miss costs, no branch predictors. What QEMU *can*
measure honestly is **instruction path length**: with
`-icount shift=0,align=off,sleep=off` the guest counters advance
deterministically with executed instructions (our benchmark's stddev is
zero across runs, and its calibration loop verifies the tick:instruction
ratio per ISA at runtime).

Path length is the same metric the seL4 literature leans on for its
fastpath, and it is the leading indicator for the P1-P3 hardware targets:
a 25-instruction grant check *can* meet "< 50 ns p99" on a modern core; a
2000-instruction one cannot. It is a necessary-not-sufficient signal -
cache and TLB behaviour on real hardware can still kill a short path.

Both systems are measured in the **same QEMU build (8.2.2), same
machine model (aarch64 virt), same icount mode** - so the comparison is
between instruction path lengths of the two designs, not between two
simulators.

## Reproducing

rheo-os side (all three ISAs, a few minutes):

```sh
cargo xtask bench --arch all       # always builds --release
# results land in target/qemu-<arch>-bench-core.log
```

seL4 side (aarch64, ~30 min build):

```sh
./sel4/run-sel4bench.sh /path/to/workspace
```

The script pins the exact configuration used for RESULTS.md:
sel4bench master, PLATFORM=qemu-arm-virt, AARCH64, RELEASE, FASTPATH,
run twice (plain TCG and icount) with the output captured. See the
script for the two documented deviations (AllowUnstableOverhead,
IRQUSER=OFF) and why they are needed under TCG.

## The honest framing of the seL4 comparison

Per docs/IO.md 6.1: the queue pair is a composition of seL4's data channel
(shared-memory rings) and signalling channel (notification); seL4's third
channel - the synchronous Protected Procedure Call - is the one primitive
the rheo-os design deliberately does *not* privilege. seL4's PPC is
near-optimal for single synchronous cross-domain calls; the rheo-os bet is
that its target workloads are dominated by throughput patterns where
doorbell-coalesced batching wins. The P2 single-message number is where
that trade-off is visible, and RESULTS.md reports it side by side with the
batched number that carries the actual design claim.

Current scope caveat: rheo-os cells do not yet run in user mode behind
hardware address spaces (BUILD-ORDER.md steps 3/5 pending), so its round
trip includes a real exception-level round trip (svc/int3/ebreak) but no
address-space switch, while seL4's IPC includes a full thread switch.
RESULTS.md quantifies this gap instead of hiding it.
