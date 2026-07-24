#!/bin/sh
# Build and run the thread-model comparison: rheo-os strands (the exact
# executor the OS ships) vs Rust std::thread, Go goroutines, and Python
# threading/asyncio, on the host. Real wall-clock (unlike the in-QEMU icount
# benches). .NET is not installed in this environment - see README.md.
set -e
dir=$(dirname "$0")
tmp=$(mktemp -d)

echo "== rheo-os strands (this OS) =="
rustc -O -C target-cpu=native --edition 2024 -o "$tmp/strands" "$dir/strands_bench.rs"
"$tmp/strands"

echo "== Rust std::thread (default Linux/Rust threading) =="
rustc -O -C target-cpu=native --edition 2021 -o "$tmp/rthreads" "$dir/rust_threads.rs"
"$tmp/rthreads"

if command -v go >/dev/null 2>&1; then
  echo "== Go goroutines =="
  (cd "$tmp" && cp "$OLDPWD/$dir/goroutines.go" . 2>/dev/null || cp "$dir/goroutines.go" .; \
   go run goroutines.go 2>/dev/null) || go run "$dir/goroutines.go"
fi

if command -v python3 >/dev/null 2>&1; then
  echo "== Python threading / asyncio =="
  python3 "$dir/py_threads.py"
fi

echo "== .NET 10 =="
echo "not installed in this environment (dotnet absent); see README.md for the"
echo "architectural expectation (System.Threading.Thread is OS-backed)."
