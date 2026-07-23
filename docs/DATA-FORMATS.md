# Data Models, Formats, and IPC Protocols

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.4 (IPC) and section 7 (IDL);
relates to IO.md (queues, sealing) and NETWORKING.md (edge protocols).

Position: pick format by layer, and let one principle decide everything -
**data should be usable where it lands, without a parse-and-copy step** -
because the whole OS is built on queues and DMA. Three native tiers; text
formats exist only at the compatibility edge.

## 1. Tier 1 - kernel ABI: fixed-layout structs, no serialization

Queue submission/completion entries, capability handles, and DMA descriptors
are `repr(C)` structs with explicit size, alignment, and version fields
(io_uring SQE style). No varints, no tags: "decode" is a pointer cast plus
validation. This surface is generated from the IDL, frozen, and continuously
fuzzed - it is the nanosecond hot path and a parser here would be both a
performance bug and an attack surface.

## 2. The signed-data exception: canonical CBOR

Anything cryptographically signed - above all the cross-host capability tokens
(SECURITY-IDENTITY.md 2) - needs a **deterministic** encoding: same logical
content, same bytes, always, or signature verification becomes ambiguous.
Protobuf explicitly does *not* guarantee this (map ordering, unknown fields).
So signed tokens use **canonical CBOR (RFC 8949)** or a Biscuit-style fixed
layout. Small, heavily audited, never casually extended.

## 3. Tier 2 - control plane: one IDL, tagged binary, evolution built in

State-store objects, controller messages, and service RPC speak types defined
in the system **IDL**, encoded in a compact tagged binary format with field
numbers so old readers skip new fields. Closest prior art is Fuchsia's
**FIDL**; Lattice builds in its image rather than adopting protobuf wholesale,
for two reasons:

- Protobuf requires a full parse (no lazy/zero-copy access); Cap'n Proto and
  FlatBuffers fixed this by making the wire format *be* the in-memory format.
  Lattice uses **Cap'n-Proto-style arena layout** so a message maps directly.
- Lattice needs IDL-level awareness of **capabilities and handles as field
  types**, which no off-the-shelf format has.

So: Cap'n-Proto-style arena layout + protobuf-style evolution rules +
handle-typed fields. The state store persists these same encoded objects - no
translate-on-write.

## 4. Tier 3 - bulk data: Arrow in memory, Parquet at rest

The strongest alignment in the whole design:

- **Arrow** is specified at the byte level for shared memory and zero-copy, so
  an Arrow buffer in a shared-memory region is directly a queue payload,
  directly a DMA source, directly consumable by GPU kernels (cudf/RAPIDS
  already speak it). "Send a table between two cells" = pass a capability to
  the buffer. No copy, no encode. Arrow's own IPC framing rides the native
  queues as-is.
- **Parquet** is the at-rest dual - columnar, compressed, predicate-pushdown-
  friendly. The native object store has a Parquet-aware object class so
  storage servers or the DPU execute projection and filter **pushdown at the
  storage node**, shipping only needed columns (FILESYSTEMS.md 3,
  NETWORKING.md).
- **Model weights**: safetensors-style flat layouts stored as content-
  addressed objects, loadable as one DMA graph straight into HBM
  (AI-ARCHITECTURE.md 2).

**Avro** earns interop support (Kafka ecosystem, schema-registry
compatibility) but is not native - its row-oriented, schema-required-to-parse
design is the opposite of the zero-copy bias.

## 5. IPC protocol proper

- Connect = capability exchange yielding a typed queue pair; the IDL declares
  the protocol (request/response, stream, one-way), FIDL-protocol style.
- Small messages travel **inline** in the Tier-2 encoding (copy wins below the
  ~1-4 KB platform-measured threshold, IO.md 3).
- Large payloads travel as **capability references to shared buffers** - the
  message says "table at grant X," never the bytes.
- **Backpressure is structural:** queues have bounded depth, full is an
  explicit condition, no unbounded buffering anywhere.
- Cross-host, the identical typed protocol rides RDMA or mTLS/QUIC transport,
  chosen at connect (doctrine 9).

## 6. Network edge protocols

- **RDMA verbs** native inside the fabric (semantics already match: post work,
  poll completion).
- **QUIC** as the standard fallback/WAN transport (streams map onto queue
  semantics, connection migration suits leases, TLS built in) - NETWORKING.md
  3.
- **gRPC and HTTP/JSON** spoken only by edge gateway cells; **Arrow Flight**
  for bulk data interchange with the outside world.

## 7. Text formats, ruled explicitly

- **JSON** - compat edge and human debugging output. Every Tier-2 message has
  a canonical JSON rendering (`kubectl get -o json`, logs). Never stored,
  never internal transport.
- **YAML** - accepted at the GitOps/manifest edge, converted immediately,
  through a **tightened parser** (no anchor/alias bombs, no implicit typing -
  the Norway problem stays outside).
- **CSV** - import/export utility in the storage layer; converts to Arrow on
  ingest.
- **XML** - no native support; a userland library concern for cells needing
  SOAP-era interop, same status as any legacy format.

## 8. Buffer sealing and validation

Shared-buffer IPC means the receiver must treat mapped memory as untrusted-
mutable - the classic TOCTOU trap. The answer is the **seal** primitive
(IO.md 3): the producer fills, the kernel flips the buffer to immutable, N
receivers validate once and trust forever, and device mappings are read-only.
Validate-in-place discipline is only safe *because* of the seal.

## 9. The through-line

The kernel casts (Tier 1), the control plane parses once with evolution
(Tier 2), the data plane never parses at all (Tier 3).

## 10. Honest costs

- A custom FIDL-like IDL is real engineering with ecosystem cost (bindings,
  docs, tooling); protobuf usually wins on gravity, not quality.
- Zero-copy formats trade CPU for memory discipline: arena layouts are bigger
  on the wire than packed protobuf - right on RDMA fabrics, wrong on slow
  WANs, hence QUIC-side optional compression.
- Shared-buffer IPC requires the seal or a read-only remap before handoff; the
  discipline is a kernel primitive, not a convention.
