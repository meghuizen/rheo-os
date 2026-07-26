#!/usr/bin/env bash
# Measure rheo-json parse throughput on this host, and simdjson too if it is
# installed. Honest measurement discipline (comparison/README.md): same host,
# same document; anything not runnable here is reported as a labelled
# published reference, never a fabricated local number.
set -euo pipefail
cd "$(dirname "$0")/../.."

echo "== rheo-json (scalar) =="
cargo run -q -p rheo-json --example bench --release

echo "== rheo-json (SSE2 string-scan) =="
cargo run -q -p rheo-json --example bench --release --features simd

echo "== simdjson =="
if command -v simdjson >/dev/null 2>&1; then
    echo "(found simdjson CLI - run it over the same document to compare)"
    simdjson --version || true
else
    echo "simdjson is not installed on this host - not measured."
    echo "See README.md for its published reference throughput and why the gap exists."
fi
