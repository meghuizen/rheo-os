# Tile GEMM comparison — tiled vs naive, and the cost model vs the host

What this measures, on the **host** (real wall-clock, real caches), using
the **exact tile kernels the OS ships** (`librheo/src/tile/kernels.rs`,
included verbatim — the same include-the-shipped-code rule as
`comparison/threads`):

1. **Tiled vs naive int8 GEMM** at a 7B-class projection shape — the
   wall-clock effect of cache-friendly blocking.
2. **The SIM-vs-HOST tiling-order table** — `TileSim`'s `bytes_staged`
   prediction (the same formula the in-tree `librheo::tile::TileSim` uses)
   must rank the block sizes the same way host wall-clock does. This is the
   traffic leg of the co-design loop (`docs/TILES.md 7`): the model is
   validated if the orderings match, and a divergence is **printed, not
   hidden**.
3. **A differential fuzz**: tiled == naive over 10,000 random
   shapes/tilings — the `json/src/scan.rs` discipline (a scalar reference
   plus a randomized equivalence check).
4. **(`--features simd`) an AVX2 inner kernel** proven **bit-for-bit
   identical** to the scalar kernel over 2,000 random shapes.

Run it:

```sh
sh comparison/tiles/run.sh
```

## Why the host, not QEMU

The in-tree proofs (`librheotile`, `bench-core p6_*`) run under QEMU, which
is honest for **correctness** and **instruction path length** but models
**no cache hierarchy** (`docs/TOOLING.md 4`). Tiling's entire benefit is
cache locality, so it is only measurable on real hardware — here, the host.
QEMU proves the tiled and naive results are bit-identical; the host proves
the tiling is *worth it*.

## Measured (this host)

The host varies run to run (~20%); the **ratios and orderings** are stable.
On the development host (x86-64):

| block | host ms (1024³) | sim bytes_staged |
|------:|----------------:|-----------------:|
| 16 | ~221 | 138,412,032 |
| 32 | ~150 | 71,303,168 |
| 64 | ~123 | 37,748,736 |
| 128 | ~104 | 20,971,520 |
| 256 | ~96 | 12,582,912 |

**Host fastest→slowest and sim least→most bytes both rank
`[256, 128, 64, 32, 16]`** — the cost model ranks the tilings exactly as the
host measures. Bigger blocks stage fewer bytes (each tile is reused more
before eviction), and the host confirms that ordering is what wall-clock
sees. Differential fuzz: 10,000 shapes, tiled == naive. AVX2 == scalar:
2,000 shapes, bit-for-bit.

## Honest caveats

- **This is a scalar `no_std` inner kernel, not a tuned BLAS.** The tiled
  vs naive *ratio* at 1024³ is near 1.0x because rustc auto-vectorizes the
  simple naive triple loop well at that size on this host; the blocking win
  shows up clearly in the **SIM-vs-HOST ordering table** (finer tilings are
  ~2.3x slower than coarse ones) and grows with problem size past cache. A
  production kernel would add register blocking, packing, and SIMD (the
  AVX2 path here is a correctness demo, not a tuned microkernel).
- **In-cell SIMD is host-only.** The AVX2 kernel runs in this comparison,
  not in a rheo-os cell: U-mode vector-state save/restore is not yet
  implemented on any ISA (`json/src/scan.rs` states the same for its SSE2
  path), so the on-OS build stays scalar. The differential fuzz is exactly
  why that is safe to defer — the SIMD path is proven equivalent.
- **No vendor-BLAS number is run here.** cuBLAS/oneDNN/OpenBLAS figures, if
  cited anywhere, are *published references*, never a fabricated local
  number — the `comparison/json` rule.
- **The sim is a first-order traffic model.** It counts bytes staged per
  space class; it does not model prefetch, associativity, or the
  block-vs-working-set interaction. It ranks tilings correctly here; where
  it would not, the table prints the divergence rather than hiding it. That
  falsifiability is the point (`docs/TILES.md 7`).

## Why the gap, honestly

The naive loop is not slow here because the compiler already vectorizes it;
the interesting signal is **traffic**, which is what the cost model
predicts and what the ordering table measures. On a problem that overflows
the last-level cache, or with a naive loop the compiler cannot vectorize
(strided, mixed-dtype), the blocking advantage is large — but stating that
without a machine that shows it would be the fabrication this repo forbids,
so what is reported is exactly what this host measures: the **ordering**,
which the model gets right, and the **equivalence**, which the fuzz proves.
