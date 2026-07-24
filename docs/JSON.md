# JSON parsing (rheo-json)

**Status:** Draft v0.1. New. A dependency-free, zero-copy JSON parser
(`json/`, package `rheo-json`), built as prep for the std port (M4,
docs/USERLAND.md): a real `no_std + alloc` Rust workload that runs on the OS
today and will recompile unchanged under the future std target.

## Why it fits the architecture

Three ARCHITECTURE.md principles shape the design:

- **"Exploit wide SIMD via measured runtime dispatch, never a
  lowest-common-denominator baseline"** (1.4). The parser has a scalar core
  that runs everywhere and a SIMD fast path selected by build/feature today
  (and by CPU-feature dispatch later, using the detection already in `hw/`).
- **Seal / zero-copy** ("a filled buffer read by many without copies", 0).
  String values borrow directly from the input (`Cow::Borrowed`); only strings
  containing escapes allocate.
- **Production-grade, not MVP** (quality bar). It is a full RFC 8259 parser
  (objects, arrays, numbers, `\u` escapes with surrogate pairs, UTF-8
  validation), bounded against stack exhaustion (`MAX_DEPTH`), with precise
  errors - correctness-tested on the host and run on all three ISAs.

## Design

- **`no_std + alloc`.** `#![cfg_attr(not(test), no_std)]`: the same crate runs
  in a cell over rheo-libc and is tested with `cargo test -p rheo-json` on the
  host (std).
- **Zero-copy DOM.** `Value<'a>` borrows from the input for `'a`; `Number`
  keeps the narrowest exact form (`u64`/`i64`/`f64`) so integers round-trip.
- **Single-pass recursive descent** with a depth limit. The string inner loop
  calls `scan::string_event`, which finds the next `"`, `\`, or control byte;
  a run with no escapes borrows the slice directly.
- **SIMD (host).** `scan::string_event` has an SSE2 implementation (16
  bytes/step) behind `feature = "simd"`, off by default so the on-OS build
  stays scalar. Executing SIMD in a cell awaits U-mode vector-state
  save/restore - see below. A fuzz test proves SIMD ≡ scalar over random
  inputs.

## Performance

Measured on the host (comparison/json/): ~155 MB/s scalar, ~160 MB/s with the
SSE2 string-scan on a record-shaped document. simdjson is ~15-25x faster; the
gap is structural (a tape/on-demand representation vs our heap DOM, and
whole-document two-stage SIMD indexing vs a string-body scan) and is analysed
honestly in comparison/json/README.md. rheo-json is serde_json-class (DOM);
closing the distance to simdjson is a deliberate follow-on, not a claim made
here.

## Roadmap

- A **tape / on-demand** value API to lift the allocation bound.
- **Two-stage SIMD structural indexing** (SSE2/AVX2, NEON/SVE2) via runtime
  CPU-feature dispatch.
- **On-OS SIMD**: save/restore vector state for cells (CPACR / `sstatus.FS` /
  XCR0, context-switch changes, dropping the arm soft-float target) so the
  SIMD path runs in a cell, not just on the host.
