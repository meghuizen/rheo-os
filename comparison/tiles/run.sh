#!/bin/sh
# Build and run the tile-GEMM comparison on the host: tiled vs naive int8
# GEMM at a true 7B-class shape, the SIM-vs-HOST tiling-order table, a
# differential fuzz (tiled == naive), and the runtime-dispatched SIMD tiers
# (scalar / AVX2=x86-64-v3 / AVX-512=v4 / VNNI=Zen4 int8 AI acceleration),
# each proven bit-identical to scalar.
#
# ALL SIMD tiers are compiled in unconditionally - the binary carries every
# path even on a CPU that lacks the feature - and the runtime
# `is_x86_feature_detected!` dispatch selects a tier only when the hardware
# is actually present. So this one build runs anywhere and lights up the
# widest instruction set the host supports.
#
# Real wall-clock (unlike the in-QEMU icount benches) - this is where
# tiling's cache-locality win and the SIMD tiers are measurable, because the
# host has real vector units and a cache hierarchy that QEMU does not model.
# It uses the EXACT tile kernels the OS ships (librheo/src/tile/kernels.rs,
# included verbatim). See README.md for the honest caveats.
set -e
dir=$(dirname "$0")
tmp=$(mktemp -d)

# `-C target-cpu=native` lets the compiler use the host's widest ISA for the
# scalar/auto-vectorized paths; the explicit SIMD kernels use runtime
# dispatch regardless, so the binary still runs on older CPUs if rebuilt
# without native (the tiers self-select).
rustc -O -C target-cpu=native --edition 2024 -o "$tmp/tiles" "$dir/gemm_bench.rs"
"$tmp/tiles"
