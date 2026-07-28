# Drivers - reusing the Linux driver ecosystem

**Status:** Draft v0.1. Design only - nothing in this document is built.
Expands ARCHITECTURE.md 5 (drivers live outside the kernel) into a concrete
plan for *where driver code comes from*. Composes GPU-HARDWARE.md 5 (the
driver cell), FILESYSTEMS.md (the FUSE-over-queues end state), NETSTACK.md
17-18 (service cells + the bridge pattern), and LINUX-COMPAT.md (the Linux
personality, L0-L8 complete).

---

## 1. The problem and the position

An operating system lives or dies on driver coverage, and the Linux driver
ecosystem took thousands of engineer-years to build. rheo-os will not rebuild
it. At the same time, ARCHITECTURE.md 5 is not negotiable: device drivers
beyond queue/IOMMU/reset plumbing are permanently outside the kernel, and a
driver's worst day must be bounded by the capability model, not by trust in
the driver.

Those two constraints are compatible, because Linux driver reuse does not
mean putting Linux code in the kernel. It means running driver code in
**contained user cells** - the GPU-HARDWARE.md 5 driver cell, generalized to
every device class - and getting the code itself from the places Linux
already keeps it:

- Linux's own **userspace-driver protocols** (FUSE, ublk, TUN/TAP, UHID,
  vhost-user) - stable wire ABIs with existing driver ecosystems behind
  them, which the completed Linux personality can already execute.
- The Linux kernel **compiled as a library** (LKL) inside a cell, for
  driver code that only exists in-kernel.
- A small set of **native drivers** for spec-uniform, hot-path hardware,
  where a single driver covers a whole class.

The position in one sentence: **rheo-os builds driver *containment* and
driver *protocols*; it imports driver *logic*.**

---

## 2. What already exists (the fragments this composes)

| Fragment | Where | What it gives this design |
|---|---|---|
| The driver cell | GPU-HARDWARE.md 5 | `BarWindow` grants over `MemKind::DeviceBar`, doorbell pages as single-page grants, the kill/FLR/re-attach recovery ladder |
| IOMMU domains + revocation | `kernel/src/hw/iommu.rs` (VT-d), `hw/smmuv3.rs` (SMMUv3); the `iommu` test | DMA containment proven: a device reads inside its domain, faults once revoked. riscv64 skips-with-reason (no QEMU model) |
| The bridge framework | `kernel/src/svc.rs`: `Bridge<T>` + `FileOps`/`SocketOps`/`NicOps`/`DisplayOps` | The north-bound seams. Kernel-resident fn tables today, documented as "a bridge to a fully message-driven service later" - this document is that later |
| Service cells | NETSTACK.md 17 (N4a): per-cell channel table, one strand per client, `SYS_YIELD` | How one driver cell serves many client cells concurrently |
| The queue ABI | `abi/` + `kernel/src/queue/`: stable on-wire rings, per-entry grant checks, inline vs by-reference payloads (IO.md) | The transport every driver protocol rides |
| The Linux personality | LINUX-COMPAT.md L0-L7 complete, L8 begun | Runs unmodified static and dynamic glibc binaries - the execution vehicle for existing Linux userspace driver programs |
| Interrupt plumbing | UART RX + NIC RX + one-shot timer interrupts on all three ISAs; `SYS_WAIT_INPUT`/`SYS_WAIT_NET` park-until-event | The shape a per-device IRQ wait generalizes |
| PCIe discovery | `kernel/src/hw/`: bridge recursion, BAR sizing/assignment, capability walk (MSI/MSI-X/FLR) | Enumerated devices + assigned BAR ranges to mint `BarWindow` grants from |

Nothing below invents a mechanism these fragments do not already sketch.

Driver cells are ordinary cells and therefore run on the substrate
docs/SUBSTRATE.md re-founds: funded per-cell metadata (an LKL cell's fd/
thread/timer counts would blow today's fixed caps), vcores for concurrency,
per-vcore queues for the data plane, and the "interrupts are optional,
report the mode truthfully" law for the D2 IRQ wait. D2's device capability
trio and SUBSTRATE.md pillar 5's NVMe pass-through land as one capability.

---

## 3. The three lanes

### 3.1 Lane A - Linux's own userspace-driver protocols (highest leverage)

Linux spent two decades moving driver classes *out* of its own kernel behind
wire protocols that are **stable ABI** (unlike its in-kernel API, which
breaks every release). Each protocol has a real driver ecosystem behind it
today, and every driver in those ecosystems is an ordinary userspace program
the completed Linux personality can run **unmodified**. rheo-os implements
the *kernel side* of each protocol - a translation onto a native seam - and
inherits the ecosystem.

| Protocol | Ecosystem it unlocks | Native seam it translates onto |
|---|---|---|
| **FUSE** (`/dev/fuse`) | fuser (Rust) and libfuse filesystems: sshfs, s3fs, NTFS-3G, rclone mounts, every hobby and vendor FS | `posix::FileSystem` + the VFS mount table |
| **ublk** (`/dev/ublk-control`, io_uring-based) | userspace block drivers: qcow2/vhd backends, network block devices, dedup/compression layers | `hw::block::BlockDevice` |
| **TUN/TAP** | userspace network dataplanes: VPNs (WireGuard-go, tailscaled), tunnels, user-mode switches | the raw-frame path behind `svc::NicOps` |
| **UHID** | userspace HID device drivers (Bluetooth HID, exotic input hardware) | the kernel input ring (`kernel/src/input.rs`) as typed events |
| **vhost-user** | userspace virtio device backends (DPDK/SPDK-adjacent) | the virtio drivers' queue seam |

FUSE lands first, for three reasons: FILESYSTEMS.md already names
"FUSE-over-queues" as the end state, so this is the doc'd direction rather
than a new one; it is fully QEMU-provable with no device model at all; and
it retires the largest single class the tree would otherwise hand-write
(every filesystem beyond ramfs/ext4-ro).

**Shape of the FUSE translation.** A FUSE server binary runs as a
`Personality::Linux` cell. It opens `/dev/fuse` (a new `FdKind` in
`kernel/src/linux/fd.rs`, the `pipe`/`eventfd` precedent - per-cell
synthesized state, **no kernel object**) and mounts itself at a path. The
personality side of the mount is a `posix::FileSystem` implementation whose
operations *are* FUSE requests: a client cell's `open`/`read`/`getdents`
becomes a `FUSE_OPEN`/`FUSE_READ`/`FUSE_READDIR` message on the server's
`/dev/fuse` fd; the server's write-back completes the client's blocked call
through the scheduler-idle machinery (the same registration-not-spin wait
every other blocking path now uses). The FUSE wire format is versioned and
documented (`include/uapi/linux/fuse.h`); rheo-os speaks a pinned protocol
version and negotiates down, exactly as the real kernel does.

### 3.2 Lane B - the LKL driver cell (the general Linux-code vehicle)

Some driver logic exists only inside the Linux kernel: read-write
ext4/xfs/btrfs, the USB and HID class stacks, the long tail of NIC and HBA
silicon, most TPU/NPU stub drivers. For these, the Linux kernel itself is
compiled as a library - **LKL** (upstream `arch/lkl`, the Linux Kernel
Library) - and hosted *inside a librheo driver cell*. LKL asks its host for
~30 operations (threads, memory, time, semaphores, IRQ delivery); the shim
maps them onto librheo: threads become strands, memory comes from grants,
time from `time::`, device access from the section 4 capability bundle.

What this buys, concretely:

- **Filesystem drivers wholesale**: LKL mounts a real block device through
  Linux's own ext4/xfs/btrfs code, read-write, journaling included, and the
  cell exports the result over the FsOps protocol. The FILESYSTEMS.md
  deferral ("write support for ext4 is deferred") is answered without
  hand-writing a journaling implementation.
- **Hardware drivers for the long tail**: with BAR windows, IRQ forwarding
  and IOMMU-domain DMA (section 4), an LKL cell's PCI host shim presents
  the real device to the unmodified Linux driver. One device per cell, one
  IOMMU domain per device.
- **Class stacks**: USB (xHCI can be native, but the *class* zoo above it -
  HID, storage, CDC - is decades of quirk tables) and the crypto/dm layers
  (LUKS/dm-crypt *format* compatibility) come as code, not as specs to
  re-implement.

Rules that keep this honest:

- **Source-level reuse, pinned.** LKL is built from a pinned kernel tree by
  xtask at build time - the uutils/coreutils precedent (built from source,
  nothing binary in git). Binary `.ko` modules are refused forever: Linux
  has no stable in-kernel ABI, so there is nothing honest to be compatible
  *with*.
- **Contained like any blob.** An LKL cell is exactly the GPU-HARDWARE.md 5
  vendor blob, and gets the same worst-day bound: kill the cell, FLR the
  device, tear down the IOMMU domain, re-attach, restart. Nothing it did
  survives except what it wrote into its own objects.
- **Compatibility, not performance.** The LKL lane is the coverage path.
  Anything hot graduates to Lane C (or a purpose-built native cell) when
  measurement says so - and the claim stays "measured", never assumed.

### 3.3 Lane C - native where spec-uniform and hot

A handful of drivers cover most real server hardware because the *spec* is
the driver contract, not the vendor: the virtio family (already built:
blk/net/gpu-2D across mmio + pci transports), **NVMe**, **xHCI**, **AHCI**,
and CPU-instruction crypto (AES/SHA - the software backends in
`net/src/crypto` are already the proven default; Linux crypto-*offload*
drivers are explicitly not worth hosting). These stay hand-written native:
they are the hot path, they are few, and each one covers a class.

Lane C is deliberately short. Every time a native driver is proposed beyond
this list, the question is "why is the LKL or protocol lane not enough?",
and the answer lands in this document or the driver is not written.

---

## 4. The common driver framework (kernel side)

Everything a driver cell - Lane B hosting Linux code, or a future native
driver cell - needs to touch real hardware. Four pieces, every one a
composition over existing objects. The admission-rule audit is section 7.

**4.1 The device capability bundle.** Owning a device means holding:

- **`BarWindow` grants** (`MemKind::DeviceBar`) over the BAR ranges
  enumeration assigned - the GPU-HARDWARE.md 5 design, verbatim, applied to
  every device class. This lifts the current blanket `kind == 4` refusal at
  the syscall boundary; device-BAR grants become real, but only over
  enumerated `BarAssignment` ranges, only to the cell holding the device,
  grant-checked at map time. Doorbell pages stay separate single-page
  grants.

  **Built (leg 1 of D2's three, and only that).** A cell now reads a real
  device register at the unprivileged level through a window the *launcher*
  mapped: `load::map_device_bar` maps a whole-page span of an enumerated,
  assigned, memory-space BAR into a cell's address space as
  `MapPerm::UserDevice` - uncached device memory per ISA (x86-64 `PCD|PWT`,
  PAT entry 3 = UC; ARM64 MAIR attr 0, Device-nGnRnE, unshared; RISC-V base
  Sv39 has no cacheability field at all, so it is a plain mapping and the
  doc says so rather than implying an attribute the hardware format cannot
  express). It refuses I/O-space BARs, unassigned BARs, and anything larger
  than `USER_BAR_MAX` (4 MiB), and it places the window in its own fixed
  region (`USER_BAR_VA`, 28 GiB) below the ISA user floor.

  What has **not** changed is the authority question: `SYS_GRANT` with
  `MemKind::DeviceBar` is still refused, so a cell cannot give itself a
  device - the window is minted by whatever launches the cell, exactly as
  the W^X exception, the cell-spawn capability and the queue pair are. That
  is the whole point of calling this leg 1: the *mapping* mechanism exists,
  the *capability bundle* (a config-space capability, a cell-lifetime IOMMU
  domain, and a cell-reachable grant verb for both) does not.

  Proven by `nvmefs`'s closing phase on all three ISAs against a value the
  cell cannot fabricate: the NVMe controller's **VS** register at BAR0+0x08,
  read by the cell through its granted window and compared against the same
  register read by the kernel through its own MMIO mapping (`0x10400` under
  this QEMU - a live reading, not a constant; aiming the cell 4 bytes off
  makes the comparison fail, observed). The window's **bound** is asserted in
  the same phase, because a device mapping that overran its BAR would hand
  the cell the next device's registers: the same cell aimed one page past the
  mapping **faults**. Removing the grant entirely also faults, observed - so
  the read is a genuine access through the page tables and not a value the
  cell could have produced without a mapping.

  Honest: **cacheability is unobservable here.** QEMU's TCG models no cache,
  so nothing in the tree can distinguish a UC mapping from a writeback one -
  the attributes above are asserted by construction (the per-ISA paging code)
  and not by measurement, and a device whose registers a stale cache line
  would corrupt is a hardware-lab gate.
- A **config-space capability**: read/write of the device's own PCI config
  function only (the accessor exists in `hw/`; the capability scopes it to
  one BDF).
- A **per-device IOMMU domain bound to the owning cell**. The mechanism is
  proven (`iommu` test: VT-d and SMMUv3, read-inside-domain succeeds,
  faults after revoke); what is new is only the binding of a domain's
  lifetime to a cell's lifetime. No IOMMU on an ISA (riscv64 under QEMU
  8.2) means **no hardware pass-through on that ISA** - refused with a
  reason, never waved through uncontained.

**4.2 DMA mapping as a grant verb.** A driver cell maps one of its own
committed grants into its device's IOMMU domain - the `dma_map` equivalent.
It is grant-checked (the cell can only expose memory its grants already
name), epoch-revocable like every mapping (revoking the grant revokes the
device's view - the `iommu` test's fault-after-revoke, now user-reachable),
and bounded by the same per-cell frame budget as everything else. The device
can never be aimed at another cell's memory, whatever the driver does.

**4.3 IRQ forwarding to cells.** The kernel owns interrupt controllers
permanently; a driver cell gets a **wait verb, not a handler**. The
`SYS_WAIT_INPUT` / `SYS_WAIT_NET` park-until-event shape generalizes to a
per-device wait: the kernel's handler ACKs/EOIs at the controller, counts
the arrival, and wakes the parked cell (or completes through the scheduler
idle state if the whole machine was halted - the registration-not-spin
discipline of ARCHITECTURE-DEBT.md 2.4). MSI-X programming comes from the
already-walked capability list; where a transport has no usable vector
(x86-64 virtio-pci via the config tunnel today), the wait degrades to the
timer-backed idle **and says so** (`interrupt_driven() == false`), the
net_rx precedent.

**4.4 North-bound class protocols.** The `svc.rs` bridge tables are the
seams; this framework finishes their documented sentence ("kernel-resident
today, a bridge to a fully message-driven service later"): each table gains
a **message-driven registration**, where the implementation lives in a
driver cell reached over a cross-cell channel (N4a service-cell framework,
one strand per client) instead of a kernel-resident fn table. The set
grows to cover the classes: `FileOps` (exists), `NicOps` (exists),
`DisplayOps` (exists), `BlockOps` and `HidOps` (new tables, same
`Bridge<T>` shape). A client cell's `read` does not know or care whether
the filesystem behind the mount is kernel-resident ext4-ro, a FUSE cell, or
an LKL cell - the seam is the same.

---

## 5. Per-subsystem mapping

The user-facing question - "which lane serves which hardware class" -
answered per subsystem:

| Subsystem | Built today | Reuse path |
|---|---|---|
| **Filesystems** | ramfs rw, ext4 ro (`ext4plus`), VFS + mounts | **Lane A FUSE** for the userspace-FS ecosystem, unmodified; **Lane B LKL** for in-kernel formats read-write (ext4/xfs/btrfs) |
| **Block** | virtio-blk (mmio + pci), `BlockDevice` trait, block cache | **Lane C NVMe** native (spec-uniform, hot); **Lane A ublk** for userspace block drivers; **Lane B** for RAID/HBA long tail |
| **NICs** | virtio-net, raw-frame path, NIC RX interrupt (riscv/arm) | **Lane C** native for the few big datacenter NICs when the lab demands them (the DPDK model proves userspace NIC drivers work); **Lane A TUN/vhost-user** for dataplanes; **Lane B** long tail |
| **Encryption** | ChaCha20/AES-GCM software backends proven on-OS; hw path verified working, software kept as default | **Lane C** CPU instructions; LUKS/dm-crypt *format* compat via **Lane B**'s dm layer; crypto-offload PCI devices not worth hosting |
| **TPU / NPU** | enumerated (PCI class 0x12) into the engine table | **Lane B** for the small kernel stub drivers; the heavy lifting in vendor stacks is *already userspace* and runs under the Linux personality; graph/engine API per ACCELERATORS.md |
| **GPU** | virtio-gpu 2D, vendor/family recognition, per-vendor front-ends, IOMMU | **Honestly excluded from Linux reuse.** DRM + Mesa is a kernel/userspace co-design that LKL cannot host in practice (display, GPU-MMU and command submission cross the boundary constantly). The GPU path stays GPU-HARDWARE.md's own stages: virtio-gpu, then vendor driver cells |
| **HID / USB** | nothing (serial console only) | **Lane C xHCI** native once (one spec, one driver); the USB class zoo above it via **Lane B**; **Lane A UHID** for userspace HID drivers |

---

## 6. What is refused, and honest limits

- **Binary kernel modules (`.ko`)**: refused forever. No stable ABI exists
  to honour; pretending otherwise would be compatibility theater.
- **GPUs via Linux reuse**: refused as stated above. Saying "Linux drivers
  for all subsystems" and quietly including Mesa would be the kind of claim
  ENGINEERING.md exists to prevent.
- **Uncontained pass-through**: a device is handed to a cell only behind an
  IOMMU domain. No QEMU IOMMU model on riscv64 means no pass-through proof
  on riscv64 - skip-with-reason, the standing pattern.
- **In-kernel driver logic**: the Lane C native drivers live behind the
  bridge seams and (like virtio-gpu today, per GPU-HARDWARE.md 12) any
  kernel residency is a named bring-up seam with its retirement stage
  scheduled, not a quiet permanent resident.
- **Performance parity claims**: none are made here. The LKL lane is
  coverage; QEMU proves protocol and containment correctness only; numbers
  come from the lab or stay unclaimed (TOOLING.md 4).
- **LKL supply chain**: hosting the Linux kernel as a library imports a
  large body of C into a cell. The containment story is the mitigation
  (a cell's worst day is bounded), but the import is named here as a
  supply-chain surface, like `ext4plus` in FILESYSTEMS.md - pinned tree,
  built from source by xtask, nothing binary in git.

---

## 7. Admission-rule audit (ARCHITECTURE.md 6)

Every kernel-side piece, against the three tests (unforgeable enforcement /
arbitrates shared hardware / mechanism with policy outside):

| Piece | New object? | Verdict |
|---|---|---|
| `BarWindow` device-BAR grants | **No** - MemoryGrant (object 5) with `MemKind::DeviceBar` made real; already designed in GPU-HARDWARE.md 5 | Passes: MMIO windows must be unforgeable and hardware-arbitrated; which registers a driver pokes is policy, outside |
| DMA map into an IOMMU domain | **No** - a grant verb composing object 5 with the existing IOMMU mechanism | Passes: DMA containment cannot be a library; what to map is policy, outside |
| Per-device IRQ wait | **No** - the `SYS_WAIT_INPUT`/`SYS_WAIT_NET` shape with a device selector (the N4a slot-argument precedent: not a new verb family) | Passes: the interrupt controller is shared hardware only the kernel can own; what to do on wake is outside |
| `BlockOps`/`HidOps` bridge tables + message-driven registration | **No** - `Bridge<T>` repeated (the `FileOps` -> `SocketOps` -> `NicOps` lineage), delivery over existing channels/queues | Passes: composition, not extension |
| FUSE `/dev/fuse` fd | **No** - per-cell synthesized state in the Linux personality (`FdKind`), the pipe/eventfd/epoll precedent | Not a kernel object at all |
| LKL, fuser, ublk servers | **No** - ordinary cell userspace | Kernel never sees them except as cells |

Zero new kernel objects, zero new verb families. The one genuinely new
kernel-side *mechanism* is binding an IOMMU domain's lifetime to a cell's -
an association between two things that already exist.

---

## 8. Build order

Phases D1-D5, each with its proof named up front (ENGINEERING.md: a slice
lands with its done-when):

- **D1 - FUSE.** The `/dev/fuse` FdKind + the FUSE-backed
  `posix::FileSystem`. **Done when:** an unmodified fuser-style hello-fs
  binary (static-glibc, built from source like every linux-fixture) serves
  a mount, and a *second* cell reads a file from it through `std::fs`,
  exact content asserted, all three ISAs, QEMU-only. No hardware touched.
- **D2 - the device capability trio.** BAR-window grants + the per-device
  IRQ wait + DMA-map-into-domain. **Leg 1 (the BAR window) is built** - see
  4.1; the IRQ wait and the cell-reachable DMA-map verb are not, so D2 is
  open. **Done when:** virtio-blk (or virtio-net)
  is **re-homed** from the kernel into a native driver cell that drives it
  through granted BARs and a real IRQ wait, serving the existing
  `BlockOps`/`NicOps` seam, with the old in-kernel driver retired from that
  boot - the sibling of GPU-HARDWARE.md stage 3, proven where the IOMMU
  exists (x86-64 + ARM64; riscv64 skip-with-reason).
- **D3 - the LKL cell.** LKL built from a pinned tree by xtask, the librheo
  host-ops shim. **Done when:** an LKL cell mounts a real ext4 image
  **read-write** off a live virtio-blk disk, a client cell writes a file
  through the mount and reads it back byte-exact after a remount, all three
  ISAs.
- **D4 - the protocol set.** ublk, TUN, UHID translations onto their seams.
  **Done when:** one unmodified Linux binary per protocol serves its class
  end-to-end (a ublk-served block device mounts; a TUN dataplane passes a
  frame; a UHID device's event reaches the input ring).
- **D5 - the long tail.** Real hardware in the lab: an LKL cell drives one
  physical NIC and one HBA/NVMe-behind-quirks device through the D2 trio.
  Lab-gated; QEMU proves nothing further here.

D1 needs nothing from D2/D3 and retires the biggest coverage gap first.
D2 is the keystone for everything hardware-shaped and is exactly the
already-scheduled GPU stage 3, generalized - the two should land as one
piece of work.

---

## 9. Dependencies named (the no-deps rule)

Per the standing rule (a doc must name any external code), the imports this
design will bring when its phases land, none of them today:

- **LKL** (`github.com/lkl/linux`, `arch/lkl`): the Linux kernel tree,
  pinned to one release, built from source by xtask into a static library a
  driver cell links. Gitignored artifacts, nothing binary committed - the
  uutils/coreutils precedent (D3).
- **FUSE protocol definition** (`include/uapi/linux/fuse.h`): a wire format
  re-declared in `abi/`-style Rust, not linked code; fuser/libfuse binaries
  are test *fixtures* built from source like the rest of
  `tests/linux-fixtures/` (D1).
- **ublk/UHID/TUN**: wire formats only, same treatment (D4).

No crate is added to the kernel, `posix`, or `abi` by any phase.
