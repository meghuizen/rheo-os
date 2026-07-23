#!/usr/bin/env bash
# Build and run seL4's benchmark suite (sel4bench) for qemu-arm-virt
# (aarch64), twice: plain TCG and deterministic icount. This is the exact
# configuration behind comparison/RESULTS.md.
#
# Usage: ./run-sel4bench.sh <workspace-dir>
#
# Prerequisites (Debian/Ubuntu):
#   apt install cmake ninja-build gcc-aarch64-linux-gnu device-tree-compiler \
#               libxml2-utils python3-pip qemu-system-arm
#   pip3 install setuptools sel4-deps   # or: pyelftools jinja2 pyyaml ply \
#                                       #     six future protobuf lxml pyfdt
#
# Notes on the two non-default flags:
#  - AllowUnstableOverhead=ON: under plain TCG the IPC benchmark's
#    measurement-overhead samples jitter (host wall clock drives the
#    counter), and the suite refuses to run without this. Under icount the
#    overhead is stable anyway (stddev 0).
#  - IRQUSER=OFF: qemu-arm-virt has no ltimer driver in util_libs; the
#    irquser benchmark crashes the whole driver without it. No other
#    benchmark needs the ltimer.

set -euo pipefail

WORKSPACE="${1:?usage: $0 <workspace-dir>}"
mkdir -p "$WORKSPACE"
cd "$WORKSPACE"

# Fetch. gerrit.googlesource.com may be unreachable behind proxies; the
# GitHub mirror of the repo launcher works everywhere.
if [ ! -d .repo ]; then
    pip3 show git-repo >/dev/null 2>&1 || pip3 install --user git-repo || true
    repo init -u https://github.com/seL4/sel4bench-manifest -b master \
        --repo-url=https://github.com/GerritCodeReview/git-repo --no-clone-bundle
    repo sync
fi

mkdir -p build
cd build
[ -f build.ninja ] || ../init-build.sh \
    -DPLATFORM=qemu-arm-virt -DAARCH64=TRUE -DFASTPATH=TRUE \
    -DRELEASE=TRUE -DSIMULATION=TRUE
cmake -DAllowUnstableOverhead=ON -DIRQUSER=OFF .
ninja

IMAGE=images/sel4benchapp-image-arm-qemu-arm-virt

run_qemu() {
    # sel4bench prints its JSON and then idles; cap each run.
    timeout 1200 qemu-system-aarch64 \
        -machine virt,gic-version=2 -cpu cortex-a53 -nographic -m size=1024 \
        -kernel "$IMAGE" "$@" | tee "$OUT" || true
    grep -q "All is well in the universe" "$OUT" \
        && echo "== run completed: $OUT" \
        || echo "== WARNING: run did not complete cleanly: $OUT"
}

OUT=sel4bench-output.txt          run_qemu
OUT=sel4bench-output-icount.txt   run_qemu -icount shift=0,align=off,sleep=off

echo "Parse the 'One way IPC microbenchmarks' JSON blocks in the two"
echo "output files; RESULTS.md quotes seL4_Call and seL4_ReplyRecv"
echo "(same vspace, IPC length 0, prio 254)."
