#!/bin/sh
# Build and run both halves of the Linux comparison (comparison/linux/README.md):
# the host Linux scheduler's wake-to-run distribution, and rheo-os's own shipped
# EEVDF+BORE run queue over the equivalent scenario.
#
# The two report DIFFERENT UNITS on purpose - nanoseconds on one side, intervening
# slices on the other. Read the README before putting them side by side.
set -e
dir=$(dirname "$0")
tmp=$(mktemp -d)

echo "== host environment =="
uname -sr
nproc
if [ -r /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor ]; then
  echo "governor: $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)"
fi
if command -v systemd-detect-virt >/dev/null 2>&1; then
  echo "virt: $(systemd-detect-virt || echo none)"
fi
echo

echo "== Linux: scheduler wake-to-run under load =="
rustc -O -C target-cpu=native --edition 2021 -o "$tmp/schedlat" "$dir/sched_latency.rs"
"$tmp/schedlat"
echo

echo "== rheo-os: the shipped EEVDF+BORE run queue, same scenario =="
rustc -O -C target-cpu=native --edition 2021 -o "$tmp/rheosched" "$dir/rheo_sched.rs"
"$tmp/rheosched"
