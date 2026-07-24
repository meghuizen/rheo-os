# comparison/rng/ - cryptographic RNG vs Linux

The claim is that rheo-os gets random bytes faster than Linux. This
directory measures it honestly, on real hardware, against Linux's own
`getrandom`/`getentropy`/`/dev/urandom`.

## What is being compared

The rheo-os randomness design (docs/TIME-IDENTITY.md 4) is a **per-cell
ChaCha20 DRBG read as a library call over the cell's own state** - not a
syscall, not a shared pool. Linux's CRNG is *also* ChaCha20, so the
cryptographic primitive is identical; the only difference this benchmark
isolates is the **path to a byte**:

- rheo-os: a function call over the cell's DRBG (fast key erasure for
  forward secrecy). No privilege transition.
- Linux `getrandom(2)`: a syscall (kernel entry, the CRNG lock, a copy back
  to userspace).
- Linux `getentropy(3)` / `getrandom(3)`: the glibc wrappers.
- Linux `/dev/urandom`: a file read.

The exact ChaCha20 core the kernel ships is `include!`d verbatim from
`kernel/src/rng/chacha.rs`, so this is the same code, not a re-implementation.

## Run it

```sh
sh comparison/rng/run.sh
```

Plain `rustc -O` (no crates). Unlike the in-QEMU benches - which report
deterministic *instruction path length* under `-icount` and never wall-clock
time (docs/TOOLING.md 4) - this one runs on the host CPU and reports real
cycles (via `rdtsc`) and throughput, because "faster than Linux" is a
real-hardware question about the syscall boundary.

## Measured results

Host: Intel Xeon @ 2.10 GHz, Linux 6.18.5. Representative of three runs
(cycle counts vary a few percent; the ratios are stable).

**32-byte draw (a key- or nonce-sized request) - cycles per call:**

| path | cycles/call | vs rheo-os |
|------|------------:|-----------:|
| rheo-os DRBG (library call) | ~110 | 1.0x |
| Linux `getrandom(2)` syscall | ~525 | 4.8x slower |
| Linux `getentropy(3)` (glibc) | ~530 | 4.8x slower |

**Bulk throughput (MB/s, higher is better):**

| path | MB/s |
|------|-----:|
| rheo-os DRBG (library call) | ~610 |
| Linux `getrandom(3)` (glibc) | ~473 |
| Linux `/dev/urandom` | ~472 |

rheo-os is ~4.8x faster on the small draws that dominate real use (every
TLS nonce, ECDSA nonce, UUID, cookie) and ~1.3x faster on bulk.

## Honest caveats

- The small-draw win is the architectural point: it is the syscall boundary,
  not the cipher, that rheo-os removes. That boundary is fixed per call, so
  the smaller the request the larger the win.
- On bulk, the Linux **kernel** ChaCha uses SIMD (AVX2) while this is a
  scalar `no_std` ChaCha; even so the measured `getrandom` throughput is
  lower, because at these request sizes the per-call kernel-entry and copy
  cost still dominate the delivered rate. A SIMD backend for the kernel
  DRBG (a portable optimisation) would widen the bulk gap further; it is not
  needed for the claim.
- On this host `getentropy` measures at syscall cost, i.e. it is not taking
  a getrandom-vDSO fast path. Where the vDSO is active the small-draw gap
  narrows but does not close: the vDSO still manages per-thread state and
  reseed bookkeeping that a per-cell DRBG does not.
- These are host numbers for the comparison only. The in-kernel RNG path
  length on all three ISAs is measured separately and deterministically by
  `cargo xtask bench` (the `rng_*` lines): ~23-25 instructions per byte for
  the scalar ChaCha20 DRBG, consistent across x86-64/ARM64/RISC-V.
