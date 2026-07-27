# Filesystems and Storage

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.6.

**Implemented (`posix/` crate + `ext4fs/`):** a VFS translation layer
(`FileSystem` trait), a read-write **ramfs**, the `BlockSource` seam
(`posix::block` - byte-addressed random access), and a read-only **ext4**
driver (Tier 1). A mount table gives the per-session `/` (section 3), and the
POSIX fd surface + a `std::fs` facade sit on top (POSIX-PERSONALITY.md). Proven
on all three ISAs by the `posix` test kernel.

The ext4 driver is the **`ext4plus`** crate (`ext4fs/` adapts it to
`posix::FileSystem` over a `BlockSource`), **not** a hand-rolled parser. This is
the "drop an existing Rust FS driver in behind the `BlockDevice`/`BlockSource`
seam" route below, taken for real: `ext4plus` is Google's read-only
`ext4-view`, so it handles the full on-disk format (arbitrary extent-tree depth,
htree dirs, checksums, ext2/3/4) that the original bounded hand-rolled parser
did not - a ~1.7 MB glibc `libc.so.6`, a depth-1 extent tree, reads correctly,
which the depth-0-only hand-rolled driver rejected. `ext4fs` lives in its own
crate so its dependencies stay **out of the deliberately dependency-free
`posix`** crate. Per the no-dependencies rule, the crate and its transitive
dependencies are named here: **`ext4plus` 0.1.0-rc.2**, pulling `async-trait`,
`async-lock` (+ `event-listener`), `maybe-async`, `crc`, `bitflags`, and `spin`
(the last two via the `sync` feature that drives `ext4plus` synchronously). It
is driven in **`sync`** mode because the driver is kernel-resident behind
`svc::FileOps`, whose call site is a synchronous syscall trap over a blocking
virtio-blk read - `maybe-async` strips the futures, so async here would be
poll-to-completion over blocking I/O with no overlap. The **async** posture is
correct when the filesystem moves into a **service cell** over the queue ABI
(the FUSE-over-queues end state, section 3), where a read parks a strand on an
`OP_READ` completion - and where **NVMe's** submission/completion queues realize
real queue-depth parallelism; the same crate flips to its default async mode
there, and the `BlockSource`/`BlockDevice` seam plus `maybe-async`'s design are
exactly what keep that reversible. `ext4plus` parses untrusted on-disk data, so
it is a supply-chain surface (mitigated by its read-only scope and Google
provenance); vendoring/audit is the standing follow-on.

**Live disk (implemented, all three ISAs):** a **virtio-blk driver**
(`kernel/src/hw/virtio_blk.rs`) exposes a **`BlockDevice`** trait
(`kernel/src/hw/block.rs`) - 512-byte sectors, transport-agnostic - behind
the same VFS, over two transports: **virtio-mmio** on the arm/riscv `virt`
machines and **virtio-pci** on x86-64 q35. The x86 path drives the device
*entirely through PCI configuration space* via the `VIRTIO_PCI_CAP_PCI_CFG`
capability (virtio spec 4.1.4.8), so no BAR is assigned or mapped - which
matters because PVH boot ships no firmware to program BARs and the kernel
only identity-maps the low 1 GiB; DMA still reaches the identity-mapped
virtqueue (PA=VA). The `blockfs` test kernel discovers the device, reads a
real ext4 image off the *live disk* (attached by QEMU with `-drive`), mounts
it, and reads files through `std::fs` - on all three ISAs.

**Route to more formats:** at the `BlockDevice`/`BlockSource` seam, existing
Rust filesystem drivers are *dropped in* rather than hand-written, so we do not
reimplement every on-disk format. ext4 already takes this route (`ext4plus`,
above); the same pattern serves `redoxfs` (Redox OS), `fatfs`, or a read/write
ext4 crate when a format is needed. The constraint is this repo's
no-dependencies rule: any such crate must be named in a doc and ideally
vendored/audited, since a filesystem driver parses untrusted on-disk data.
**Write** support for ext4 (journaling, allocation) is deferred - `ext4plus` is
read-only; a read/write crate slots in behind the same seam when writes to a
legacy format are actually needed.

Position: there is no global filesystem tree in the native model - storage is
a **capability-scoped object store**, and "filesystems" are either views over
it or translation layers around legacy formats. Three tiers, each with an
honest role.

## 1. Tier 1 - legacy on-disk (ext4, btrfs, ZFS, xfs)

- Served by **userspace filesystem server cells**: ordinary cells holding a
  grant to an NVMe namespace's queue pairs, speaking a filesystem protocol
  over queues to clients.
- Role: data import/export and disk interchange - not the system's own
  storage. Modern userspace-FS performance (ZFS, FUSE-over-io_uring lineage)
  plus our batched-native queue ABI makes this respectable, unlike Hurd-era
  userspace servers.

## 2. Tier 2 - distributed POSIX-ish (CephFS, HDFS, NFS)

- First-class clients: these systems already *are* the architecture
  (identity-authenticated clients, object servers, metadata services). A
  CephFS client cell holds grants to reach OSD queue endpoints; RDMA
  transports map directly; cephx is replaced by native workload identity.
- Role: existing data lakes and shared POSIX namespaces during migration.
  The native store earns trust gradually while these carry real data -
  filesystems take a decade to trust, and that schedule is respected.

## 3. Tier 3 - the native object store

### Objects

- The unit is a **typed, versioned object**: bytes + schema-tagged metadata +
  optional content hash, named by UUIDv7 (or content hash for immutable
  objects). Copy-on-write versioning throughout; snapshots are version pins.
- Object classes with distinct contracts: mutable versioned objects,
  **append-log objects** (WAL/Kafka-shaped, with fencing tokens for writer
  handoff), and **content-addressed immutable objects** (images, model
  weights, Parquet files - dedup for free, hash = integrity = attestable
  provenance).

### Namespaces are indexes

- Directories do not exist at the storage layer. A namespace is a
  **materialized view** mapping paths to objects - cheap to create,
  per-tenant, per-session, per-identity. The POSIX personality's synthesized
  `/` is exactly one of these views (POSIX-PERSONALITY.md 3). Plan 9's
  per-process namespace idea, finally on a security model that enforces it.

### Access and I/O

- Access is **grants on object sets** - delegatable, expiring, auditable.
  No mode bits, no ACL subsystem, no LSM labels: the third parallel
  permission system Linux carries is deleted, and there is exactly one.
- I/O is the queue ABI end to end (IO.md): durability classes, completion
  windows, group commit by contract, DMA graphs for bulk paths, explicit
  cache tier, `streaming` bypass.

### Indexes are differential

- Every index-shaped structure (namespace views, secondary indexes, the
  state store's indexes) is a delta layer absorbing single events at memory
  speed, merged into the base on policy (size/age/idle). LSM economics,
  applied uniformly; per-object-class index policy (B-tree-ish vs LSM-ish)
  stays declarable because read-heavy point lookups price differently.
- Merge/compaction runs under the same reservation model as everything else
  - background merges cannot starve foreground I/O contracts, and merge debt
  signals clients through pressure events before it becomes a latency cliff.

### Distribution

- Placement, erasure coding, and replication are **object-class policies**
  across hosts and fabric islands, consuming the cluster fundamentals
  directly: HLC-versioned metadata, lease-fenced writers during partitions,
  membership from the consensus core. Compare Ceph, which had to rebuild
  clocks, auth, and membership on top of Linux; here the storage layer
  consumes them.
- **Pushdown:** Parquet-aware object classes execute projection and filter
  at the storage node or DPU, shipping only needed columns (NETWORKING.md,
  AI-ARCHITECTURE.md). Arrow is the in-memory dual; conversion at ingest for
  CSV, at the edge for everything textual.

## 4. Encryption and integrity

- Per-object (or per-object-class) encryption keys are capabilities;
  at-rest compromise of a disk yields sealed ciphertext plus hashes.
  Content addressing doubles as end-to-end integrity - a corrupted replica
  is detected by name.

## 5. POSIX semantics - the honest gap list

The personality synthesizes files over objects; the leaks are documented,
not hidden:

- `rename()` atomicity across what are really two index updates: provided
  within one namespace view via an atomic index operation; cross-view rename
  is copy+delete, stated.
- `mmap` of remote objects: forbidden transparently (no network paging,
  doctrine 4) - pin-local-first or fail loudly.
- `flock`/POSIX locks map to leases with expiry and fencing - semantics are
  *better* under failure but not bit-identical; long-held advisory locks
  behave differently and the personality documents it.
- `/proc`, `/sys`: synthesized, scoped to your own cells, format-compatible
  for the common tools, incomplete by design.
- Expected fidelity: ~99% interactive, ~80% arbitrary sysadmin scripts
  (ARCHITECTURE.md P11 gates this).

## 6. What to build first

The append-log object class and content-addressed immutable objects come
first - the state store, model registry, and image pipeline all need exactly
those two, and both have small, testable contracts. General mutable objects
and erasure-coded placement follow. Tier 1/2 support ships from day one so
no real data ever waits on the native store's maturity.
