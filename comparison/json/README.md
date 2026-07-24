# rheo-json vs simdjson

Honest throughput comparison for the JSON parser (docs/JSON.md), in the same
spirit as `comparison/rng` and `comparison/threads`: measure what we can on
this host, and label anything we cannot run here as a *published reference*,
never a fabricated local number (CLAUDE.md, "How benchmarks stay honest").

## What is measured

`run.sh` builds a representative document (an array of ~20k uniform records:
strings, numbers, bools, small nested arrays, unicode) and parses it in a
loop, reporting MB/s. Two configurations of the same parser:

- **scalar** - the path that runs on rheo-os today (in a cell over rheo-libc).
- **SSE2 string-scan** (`--features simd`) - the SIMD fast path for scanning
  string bodies, used on the host. It cannot run in a cell yet (U-mode vector
  state is not saved/restored), which is why it is a host-only feature.

## Results (this host, release)

| Parser | Throughput | Notes |
|---|---|---|
| rheo-json (scalar) | ~155 MB/s | runs on the OS; borrowed (zero-copy) strings |
| rheo-json (SSE2 string-scan) | ~160 MB/s | small win here - records have short strings |
| **simdjson (C++)** | **~2500-4000 MB/s** | *published by the simdjson project on server CPUs; not run on this host* |
| simd-json (Rust port) | ~1000-2000 MB/s | *published; not run here* |
| serde_json (DOM) | ~200-400 MB/s | *published; the closest architectural peer* |

Numbers vary with the document and CPU; re-run `./run.sh` for this machine.

## Why the gap, honestly

simdjson is ~15-25x faster, and the reasons are structural, not incidental:

1. **DOM vs tape.** rheo-json builds a heap value tree (a `Vec`/`Cow` per
   value), so parsing is dominated by allocation. simdjson writes a flat
   *tape* and resolves values on demand - almost no per-value allocation.
   This is the single biggest factor and puts us in serde_json's DOM class,
   not simdjson's.
2. **Where the SIMD is.** Our SIMD accelerates only the string-body scan.
   simdjson runs a two-stage design where **stage 1 indexes every structural
   byte of the whole document with SIMD** (quote/backslash bitmaps, carry-less
   prefix-xor for the in-string mask). That is the real win, and it needs
   wider, document-global vector work than a string-scan.
3. **Short strings.** In this record-shaped document the strings are ~10-20
   bytes, so the 16-byte SSE2 stride rarely runs a full step - hence the
   small scalar->SIMD delta. On documents with long string values the gap
   between the two widens.

## The path to close it (aligned with the architecture)

The OS explicitly targets "wide SIMD via measured runtime dispatch"
(ARCHITECTURE.md 1.4), so the design direction is set:

- A **tape / on-demand** value representation to remove the allocation bound.
- **Two-stage SIMD structural indexing** (SSE2/AVX2 on x86, NEON/SVE2 on arm),
  chosen by the CPU features we already detect in `hw/`.
- **On-OS SIMD** once U-mode vector state is saved/restored across context
  switches (the arch milestone noted in docs/JSON.md).

Until then, rheo-json is a correct, portable, zero-dependency, zero-copy DOM
parser that runs on the OS - and simdjson is the honest bar it is measured
against.
