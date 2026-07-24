#!/bin/sh
# Build and run the host RNG comparison (rheo-os ChaCha20 DRBG vs Linux
# getrandom/getentropy/urandom). Plain rustc, no crates. Real hardware,
# real Linux - this is a wall-clock/cycle comparison, unlike the in-QEMU
# icount benches which measure instruction path length only.
set -e
dir=$(dirname "$0")
out=$(mktemp -d)/getrandom_bench
rustc -O -C target-cpu=native -o "$out" "$dir/getrandom_bench.rs"
exec "$out"
