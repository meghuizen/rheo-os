#!/bin/sh
# Build and run the tile-GEMM comparison on the host: tiled vs naive int8
# GEMM at a true 7B-class shape, the SIM-vs-HOST tiling-order table, and a
# differential fuzz (tiled == naive). Optionally an AVX2 inner kernel proven
# bit-identical to the scalar one (--features simd).
#
# Real wall-clock (unlike the in-QEMU icount benches) - this is where
# tiling's cache-locality win is measurable, because the host has a cache
# hierarchy that QEMU does not model. It uses the EXACT tile kernels the OS
# ships (librheo/src/tile/kernels.rs, included verbatim). See README.md for
# the honest caveats.
set -e
dir=$(dirname "$0")
tmp=$(mktemp -d)

echo "== scalar (portable) build =="
rustc -O -C target-cpu=native --edition 2024 -o "$tmp/tiles" "$dir/gemm_bench.rs"
"$tmp/tiles"

echo
echo "== SIMD build (AVX2 inner kernel + differential check) =="
rustc -O -C target-cpu=native --cfg 'feature="simd"' --edition 2024 \
  -o "$tmp/tiles_simd" "$dir/gemm_bench.rs"
"$tmp/tiles_simd"
