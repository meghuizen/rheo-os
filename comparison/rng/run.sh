#!/bin/sh
# Build and run the host RNG comparison (rheo-os ChaCha20 DRBG vs Linux
# getrandom/getentropy/urandom). Plain rustc, no crates. Real hardware,
# real Linux - this is a wall-clock/cycle comparison, unlike the in-QEMU
# icount benches which measure instruction path length only.
set -e
dir=$(dirname "$0")
tmp=$(mktemp -d)
rustc -O -C target-cpu=native -o "$tmp/getrandom_bench" "$dir/getrandom_bench.rs"
"$tmp/getrandom_bench"
echo
# The entropy-source companion: jitter quality/rate + the AVX2 headroom
# measurement (runtime-dispatched, verified against the scalar core).
rustc -O -o "$tmp/entropy_bench" "$dir/entropy_bench.rs"
exec "$tmp/entropy_bench"
