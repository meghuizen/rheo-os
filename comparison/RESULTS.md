# Concept validation results - rheo-os vs seL4

**Date:** 2026-07 (re-measured after user-mode landed).
**Environment:** QEMU 8.2.2 (Ubuntu), `-icount shift=0,align=off,sleep=off`
(deterministic: every number reproduces exactly).
**rheo-os:** release build, pinned nightly (rust-toolchain.toml), this
commit. **seL4:** sel4bench master (kernel `5126e718b`, sel4bench
`a0f099b`), PLATFORM=qemu-arm-virt, AARCH64, RELEASE, FASTPATH - built and
run with `comparison/sel4/run-sel4bench.sh`.

All numbers are **instruction path lengths** (instructions executed per
operation), not wall-clock time. Read comparison/README.md for why that is
the only honest QEMU metric and how it maps to the hardware targets. On
x86-64 and RISC-V the guest counter advances one tick per instruction
under icount (calibration line `ticks_per_kilo_insn=1000`), so those
columns are exact; on ARM64 the virtual counter runs at 1/16 the
instruction rate (`ticks_per_kilo_insn=62`), so its column is rounded and
marked `~`.

## 1. What changed since the last milestone

The previous report flagged one caveat in bold: cells were *not yet* behind
hardware address spaces, so isolation was enforced by a table lookup and
the round-trip numbers crossed a privilege boundary but **no address-space
switch**. That caveat is now closed. Cells run in real U-mode (RISC-V
U-mode, ARM64 EL0, x86-64 ring 3) behind per-cell page tables (Sv39 /
4 KiB-granule AArch64 / 4-level x86-64), and the isolation tests are
enforced by the MMU faulting, not by kernel bookkeeping.

## 2. Correctness: the concepts hold (MMU-enforced)

`cargo xtask test --arch all` - green on x86-64, ARM64, RISC-V 64:

| Concept | Test kernel | How it's enforced | Result |
|---|---|---|---|
| Unforgeability, attenuation, revocation, isolation (ARCH 8.2) | cap-invariants | runtime checks on the capability core | pass, 3 ISAs |
| Queue ABI: batched submit + one doorbell, flow context, per-entry grant check, mid-stream revocation | queue-pipeline | in-kernel | pass, 3 ISAs |
| A cell cannot read kernel memory | isolation-hw | **page fault** (no U bit on kernel pages) | pass, 3 ISAs |
| A cell cannot read another cell's page | isolation-hw | **page fault** (not mapped in this root) | pass, 3 ISAs |
| W^X: a code page is not writable | isolation-hw | **page fault** (read-only mapping) | pass, 3 ISAs |
| NX: a data page is not executable | isolation-hw | **page fault** (NX / UXN / no-X) | pass, 3 ISAs |
| A cell reads its own mapped page | isolation-hw | succeeds (control) | pass, 3 ISAs |

The isolation lemma (ARCHITECTURE.md 8.2 property 4) is now demonstrated the
way it will hold in production: two cells with disjoint page tables fault
the moment either reaches for the other's memory.

## 3. The numbers (instructions per operation)

`cargo xtask bench --arch all`, best batch under icount. In-kernel figures
measure the mechanism in isolation; user-mode figures cross the real
privilege boundary (and, for P5, a real address-space switch each way).

| Benchmark | x86-64 | ARM64 | RISC-V 64 |
|---|---|---|---|
| P1 grant check (typed handle) | 27 | ~26 | 24 |
| P1 deny path (revoked) | 19 | ~22 | 21 |
| P3 context switch (same-cell, cooperative) | 20 | ~25 | 40 |
| Doorbell trap floor (int3 / svc / ebreak) | 40 | ~48 | 67 |
| P2 queue round trip, in-kernel single | 212 | ~200 | 259 |
| P2 queue round trip, in-kernel batched 64 | 154 | ~139 | 160 |
| **Syscall floor (U-mode, empty syscall)** | **76** | **~91** | **119** |
| **P2 queue round trip (U-mode, real syscall)** | **207** | **~210** | **277** |
| **P5 cross-cell round trip (2 addr-space switches)** | **155** | **~211** | **273** |

The U-mode P2 (submit -> `syscall` doorbell -> kernel grant-checks and
completes -> reap) is the honest single-message number: it includes the
privilege transition, the per-entry capability check, and the completion,
all across the ring-3/EL0/U-mode boundary. P5 is a directed cross-cell
switch (save frame, switch page table, restore peer) each way.

## 4. Head-to-head with seL4 (same QEMU, same machine, same icount)

seL4 fastpath IPC, aarch64 qemu-arm-virt, same configuration, deterministic
under icount (sel4bench "One way IPC microbenchmarks"):

| Operation | seL4 | rheo-os |
|---|---|---|
| One-way cross-domain (Call) | 190 | - |
| One-way cross-domain (ReplyRecv) | 216 | - |
| **Synchronous cross-domain round trip** | **406** | **155 (x86) / ~211 (arm64) / 273 (riscv), cross-cell** |
| Amortized message cost, batched | n/a (PPC does not batch) | 139-160 (in-kernel, 64/doorbell) |
| Queue round trip with grant check + completion, U-mode | - | 207 (x86) / ~210 (arm64) / 277 (riscv) |

### What this shows, stated honestly

- **The design's central bet now holds across a real address-space
  boundary.** seL4's synchronous IPC round trip is 406 instructions; the
  rheo-os cross-cell directed round trip - two genuine page-table switches -
  is 155-273 depending on ISA. The queue round trip that also does useful
  work (a grant-checked completion) is 207-277. Both sit at or below seL4's
  number, on the same emulator and clock.
- **The comparison is now apples-to-apples on isolation, but the two
  measure different amounts of work.** seL4's 406 is a full IPC: endpoint
  lookup, badge, message registers, and a scheduling decision. The rheo-os
  cross-cell path is a *directed switch* with no message payload or
  capability lookup on the switch itself - lighter by design, because the
  design pushes data transfer onto the shared ring (the P2 number) rather
  than into the control transfer. So read P5 as "the cost of the address-
  space crossing" and P2-user as "the cost of getting work done across it",
  and compare each to the part of seL4's IPC it corresponds to.
- **ARM64's column is coarse.** Its virtual counter ticks once per ~16
  instructions, so `~211` is `13.1 ticks x 16`. x86-64 and RISC-V count one
  tick per instruction and are exact. The ordering and order-of-magnitude
  conclusions hold on all three; only ARM64's precise value is rounded.
- **seL4's proof is still its advantage.** seL4's IPC carries a verified
  kernel; the rheo-os capability core is runtime-tested (section 2), not yet
  proved (Verus, BUILD-ORDER.md step 4 gate).

## 5. Mapping to the hardware targets (ARCHITECTURE.md 8.4)

Path length is the leading indicator, not the verdict - cache/TLB behaviour
on real hardware decides finally (docs/TOOLING.md 4):

| Gate | Hardware target | Kill | Measured (insns) | Reading |
|---|---|---|---|---|
| P1 grant check | < 50 ns p99 | > 150 ns | 24-27 | ~10 cycles of work; ample margin |
| P2 single (U-mode) | < 1 us | > 3 us | 207-277 | orders of magnitude of headroom |
| P2 batched | < 100 ns amortized | > 300 ns | 139-160 | tight at ~2 GHz; hardware decides |
| P3 same-cell switch | < 200 ns | > 500 ns | 20-40 | floor; real number needs runtime state |
| P5 cross-cell | < 500 ns same host | > 1.5 us | 155-273 | plausible; TLB refill on real HW is the risk |

No gate can be *passed* in QEMU; none is *killed* by these numbers. P2-
batched and P5 are the two to watch on real hardware, where a TLB miss on
the address-space switch is the cost QEMU cannot show.

### Tiles (host, docs/TILES.md)

The tile framework's cost model is validated where its benefit is real -
on the host, because QEMU models no caches (`comparison/tiles`). The
in-QEMU `p6_*` benches report per-tile-op instruction path lengths and the
tiling-cost ordering (finer tiling = more tile trips = more instructions);
the host confirms that ordering in wall-clock:

| block (1024³) | host ms | TileSim bytes_staged |
|---:|---:|---:|
| 16 | ~221 | 138,412,032 |
| 64 | ~123 | 37,748,736 |
| 256 | ~96 | 12,582,912 |

Host fastest→slowest and sim least→most-bytes **both rank
`[256,128,64,32,16]`** - the traffic model ranks tilings as the host
measures (the co-design loop's traffic leg). Differential fuzz: 10,000
random shapes, tiled == naive. AVX2 inner kernel == scalar, bit-for-bit,
2,000 shapes. Honest: the scalar `no_std` kernel is not a tuned BLAS.
**In-cell SIMD now runs on-OS** - librheo cells are hard-float and the
kernel saves vector state across cell switches, so `tile::simd` dispatches
AVX2 in a cell (the `librheotile` test asserts it bit-exact); AVX-512/VNNI
light up on real hardware (QEMU TCG has AVX2 only), proven here on the host.
`comparison/tiles/README.md` has the full caveats.

## 6. Verdict on viability, as of this commit

- The capability model, the queue-pair ABI, and cells-as-protection-domains
  all **work as specified, now with hardware enforcement**, on three ISAs.
  Isolation is the MMU's job and the MMU does it; the four proof properties
  hold at runtime; the queue ABI's batching, tracing, and mid-stream
  revocation fall out of the ABI as claimed.
- The **cost story survives contact with real user mode**: crossing a real
  privilege-and-address-space boundary costs 155-277 instructions, at or
  under seL4's synchronous IPC on the same emulator - and the design's
  batching amortizes the data path below that.
- **Open before "viable" becomes "validated":** the Verus proofs (step 4
  gate), hardware-lab numbers for P1-P5 including TLB effects (milestone
  M1), ASID/PCID-tagged switches measured under churn, and a cross-cell
  path that carries a real typed message (not just a directed switch) for a
  fully like-for-like seL4 IPC comparison.
