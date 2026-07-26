# Memory Management

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.1; the memory-grant object
(section 3, object 5).

Position: the kernel manages **address space and physical pages** and never
runs a general-purpose heap for anyone. Cells get raw, typed memory grants
and build their own allocators. Stacks are virtual-memory tricks; heaps are
per-vcore arenas over grants. Reclaim is an explicit event, never an OOM
killer's surprise.

## 1. Split of responsibility

- **Kernel:** owns page frames per memory kind, hands out **grants**
  (contiguous-in-virtual, possibly-sparse-in-physical, with commit policy
  and a memory kind attached). The kernel's own allocations - capability
  tables, queue descriptors, page tables - come from **typed slab caches**
  (fixed-size objects, per-core magazines). There is no general `kmalloc`
  with its fragmentation and unpredictability; the kernel holds metadata, and
  metadata is slab-shaped.
- **Cell:** the runtime's allocator (jemalloc/mimalloc-class, or a GC - the
  OS does not care) manages the granted region. No `sbrk`, no global `mmap`
  contention; the allocator commits/releases pages via a queue operation.

## 2. Typed memory (recap)

Every allocation names a kind: DDR, HBM, CXL, PMEM, device-BAR, remote. Each
carries declared bandwidth/latency/coherence/DMA-reachability. `Buffer<Hbm>`
and `Buffer<Ddr>` are distinct types (ARCHITECTURE.md 4.1, ACCELERATORS.md
5). Placement and migration are explicit; migration is a scheduled DMA graph
node, never a transparent page fault.

### Userspace grant syscalls (librheo Phase B)

The memory-grant object is exposed to a native cell as mechanism (no new
object; ARCHITECTURE.md 6): `SYS_GRANT(len, kind, flags) -> (base VA, cap_id)`
reserves typed address space and mints a MemoryGrant capability;
`SYS_COMMIT`/`SYS_DECOMMIT` back/unback a sub-range with frames (demand paging
via **explicit commit**, no fault handler); `SYS_SEAL` makes a grant immutable
(read-only, shareable - the zero-copy-buffer precursor); `SYS_MMAP_FILE` maps a
VFS file range into the cell; `SYS_MUNMAP` frees a range's frames. Grants are
per-cell state (a fixed static table), and every commit/decommit/seal is
grant-checked (MAP right). **DDR is always backed for real**, and **PMEM is now
real where the platform exposes an nvdimm** (see section 2.1); HBM/CXL/remote
have no QEMU device model and stay honestly DDR-emulated; device-BAR has no
backing and is refused. NUMA placement is single-node here (the hint is
recorded, not acted on). See docs/LIBRHEO.md Phase B and librheo's `mem`
module (`Grant`/`Arena`/`Mapping`).

## 2.1 Real persistent memory (PMEM), where tractable

A `MemKind::Pmem` grant is backed by frames from a **real QEMU nvdimm's
physical region** - distinct from the DDR frame pool - on the ISA where the
emulator and firmware map actually expose one. This replaces the old
"PMEM emulated-as-DDR" caveat with real nvdimm backing where it is tractable,
and an honest skip-with-reason (DDR-backed, unchanged) where it is not.

**Discovery.** A real nvdimm's persistent span is reported by firmware, not the
ordinary RAM map. On x86-64 QEMU surfaces it **only** through the ACPI **NFIT**
(the SPA Range Structure, GUID `66F0D379-...-8CDB`) - it is absent from the PVH
E820 memmap - so `kernel/src/hw/acpi.rs` parses the NFIT and adds the region as
`MemKind::Pmem` to the machine `Inventory`. (No DT `pmem` node parse was needed
on arm/riscv - see the per-ISA status below - so `hw/fdt.rs` is unchanged.)

**Allocation.** A **separate** frame allocator (`kernel/src/mm/frames_pmem.rs`,
a bitmap over the discovered region) backs pmem grants, distinct from the DDR
`frames` pool. `Grant::commit` for `MemKind::Pmem` draws from it; if no nvdimm
was discovered it falls back to a DDR frame with a one-time logged note, so
machines without an nvdimm are unchanged. Two differences from the DDR pool:
(1) a pmem frame is **not zeroed** on allocation - persistent memory retains its
contents, which is the point of it; (2) QEMU places the nvdimm's physical span
at 4 GiB, **above the kernel's top-2 GiB linear map** on x86-64 (the "kernel"
code-model constraint), so the kernel reaches pmem frames through a dedicated
supervisor **mapping window** (`arch::pmem_map_window`, a fresh x86-64 PML4 slot)
rather than `phys_to_virt`. Only firmware parsing and this window are per-ISA
(both under the arch/hw layers); `frames_pmem` and the grant path are portable.

**Per-ISA status (honest):**

- **x86-64 (q35):** genuinely nvdimm-backed. `-machine nvdimm=on` + a
  `memory-backend-file` + an `nvdimm` device; discovered via NFIT; a `Pmem`
  grant's committed frames fall inside the real nvdimm region (proven by the
  `pmem` test kernel) with a write/read round-trip.
- **ARM64 (virt):** skip-with-reason, DDR-backed, unchanged. QEMU's arm `virt`
  refuses an nvdimm without an ACPI **GED** device ("memory hotplug is not
  enabled: missing acpi-ged device"), and this kernel uses a built-in DT-less
  machine profile with no ACPI/NFIT parser, so no pmem region is surfaced.
- **RISC-V (virt):** skip-with-reason, DDR-backed, unchanged. QEMU 8.2's riscv
  `virt` machine has **no** nvdimm support (`Property 'virt-machine.nvdimm' not
  found`), so there is no nvdimm to discover.

**Persistence caveat.** Cross-reboot persistence (bytes surviving a power cycle)
is a property of the real backing DIMM and is **not** headlessly assertable in a
single QEMU boot; it is not claimed. The proof here is that the grant is backed
by the real persistent-memory **physical range** plus a write/read round-trip
(the `pmem` test kernel, all three ISAs - real on x86-64, skip-with-reason on
arm/riscv). This adds no kernel object or verb: `MemKind::Pmem` already flows
through the existing grant syscalls, so this is backing an existing typed object
with real frames (mechanism), passing ARCHITECTURE.md 6 with nothing new.

## 3. Stacks

- Stackful strand: reserve ~1 MB virtual, commit nothing, first-touch commits
  4 KB pages downward, an unmapped **guard region** below catches overflow as
  a clean fault (kill the strand, not the cell). 100k stackful strands
  reserve address space, not RAM.
- Stack commit is **charged to the cell's grant** on touch, so deep recursion
  eats the cell's own budget visibly.
- Long-parked strands can `park + decommit-below-watermark` (MADV_FREE-style,
  re-zeroed on next touch).
- **No stack swapping, ever** (doctrine 4). Exceeding the grant fails loudly,
  inside the cell.
- **Kernel stacks are per-vcore, not per-strand** - one of the biggest
  structural wins. Linux pays 8-16 KB kernel stack per thread; here the
  kernel context count equals the vcore count, so 100k strands cost the
  kernel a few kilobytes total.

## 4. Heaps

- **Per-vcore arenas** are the default: each vcore allocates from its own
  arena - no allocator lock on the fast path, no false sharing between
  vcores, NUMA-correct because arena pages come from the vcore's home domain.
  Cross-vcore frees go to a per-arena remote-free queue drained by the owner
  (mimalloc's trick, matching the queue-everything instinct).
- **Size-class slabs** inside arenas for small objects; large allocations go
  straight to grant pages.
- **Huge pages are the default commit quantum**, not a tuning flag: grants
  commit in 2 MB units when the pattern sustains it, and arena geometry is
  built around 2 MB alignment so transparent-huge-page-style split/collapse
  churn (Linux's khugepaged CPU burn and latency spikes) never happens. 1 GB
  pages are an explicit grant attribute for HBM and buffer pools. TLB reach
  is a top silent tax on big-memory workloads; this buys it back
  structurally.
- **Typed heaps follow typed memory:** a `Buffer<Hbm>` allocation comes from
  an HBM-backed arena; the allocator API takes memory kind as a parameter, so
  "my tensor allocator silently fell back to DDR" cannot happen - fallback is
  explicit policy, and metering shows which kind is consumed.
- **Decommit without unreserve:** a heap returns physical pages from its slack
  without surrendering address layout - the key tool for fighting
  allocator-internal fragmentation.

## 5. Zeroing

Pages returned to the kernel are zeroed by a **background low-priority engine
job** (or the DMA engine, which many platforms zero at memory-controller
speed), so allocation fast paths hand out pre-zeroed pages without paying the
memset at the worst moment. Absolute security rule: no page crosses a cell
boundary un-zeroed.

## 6. Fragmentation, three levels

- **Virtual:** nearly free (huge address space, grants are sparse).
- **Physical:** contained by per-kind, per-size-class page pools plus the
  2 MB commit quantum, so mixed 4 KB confetti never forms.
- **Allocator-internal:** the runtime's problem, given the decommit-without-
  unreserve tool to solve it.

## 7. Reclaim - no OOM killer

- Grants are **hard by default** (reserved - the database contract). A cell
  may also hold **elastic** grants: a guaranteed floor plus a reclaimable
  ceiling.
- Under host pressure the kernel does not scan or swap; it sends a **pressure
  event** on the cell's control queue ("return 512 MB within 100 ms from the
  elastic range"). The runtime drops caches, decommits arenas, shrinks pools -
  it knows what is cheap to lose; the kernel never did.
- Miss the deadline and the elastic portion is force-decommitted; the cell
  then faults on its own missing pages, **failing alone**. There is no global
  victim selection, ever.
- This is what Linux gropes toward with PSI + memory.high + userspace OOM
  daemons; here it is the native contract.

## 8. Safety

- Use-after-free and double-free **within** a cell are the language/runtime's
  job: Rust cells get it at compile time; C-legacy cells can opt into
  hardened allocator modes (quarantine, **MTE memory tagging** on ARM64 -
  cheap enough to leave on in production; exposed via the Arch trait,
  TARGET-ARCHITECTURES.md 4).
- **Across** cells it is the OS's job and absolute: buffer capabilities are
  epoch-versioned, so a revoked or freed shared buffer turns stale handles
  into clean faults, never someone else's recycled data.
- Leaks are contained: a cell can only leak its own grant, metering makes the
  slope visible early, and cell death returns everything (no kernel-side
  orphan state).

## 9. Kernel address-space layout (higher-half)

A trap enters the kernel with the faulting cell's page-table root still active
(no root switch on trap entry), so the kernel must be mapped in every cell's
root. Mapping it supervisor-only in the **low** half wastes the low addresses a
user program wants: a stock Linux `ET_EXEC` links at 0x400000 and collides with
the kernel's low identity map. The fix is to run the kernel in the **high
canonical half** so the whole low half belongs to user programs.

- **aarch64 (done):** the kernel + all MMIO live in **TTBR1_EL1** (VAs with the
  top bits set); a cell's **TTBR0_EL1** root maps *only* that cell's user pages.
  The kernel is linked at `KERNEL_VA_BASE | load_address`
  (`KERNEL_VA_BASE = 0xFFFF_0000_0000_0000`, link/aarch64.ld) and loaded at its
  physical address via `AT()`; the boot trampoline (`arch/aarch64/boot.S`)
  builds an initial TTBR1 linear map plus a temporary TTBR0 identity map,
  enables the MMU, and branches to the high-half continuation before any Rust
  runs. TTBR1 is set once at boot and never switched; `paging_activate` only
  rewrites TTBR0 (per cell), and `paging_activate_kernel` restores the boot low
  identity map between cell runs so setup code can still reach the low `.user`
  window. The `.user` window (hand-written U-mode code + per-cell data) stays
  low, identity-mapped, per-cell in TTBR0 - so isolation is unchanged. Because
  the kernel now spans high (`.text`/`.data`) and low (`.user`) symbols beyond
  the small code model's +-4 GiB `adrp` reach, the aarch64 kernel is built with
  the **large code model** (xtask).
- **riscv64 (done):** a single `satp`, so - unlike aarch64's TTBR split - every
  cell root still carries the kernel + MMIO (mapped **supervisor**, high), since
  a trap enters with the cell root active and must reach the handler. The whole
  low half is left free, so a stock Linux `ET_EXEC` at 0x10000 loads unmodified.
  `KERNEL_VA_BASE = 0xFFFF_FFC0_0000_0000` is the base of the Sv39 39-bit
  sign-extended high half (top-half level-2 indices 256..511); MMIO maps at
  index 256, kernel RAM at 258. The boot trampoline (`arch/riscv64/boot.S`)
  builds an initial root (high MMIO + high kernel RAM gigapages, plus a
  transient low identity so the turn-on instruction stays mapped), writes
  `satp`, and jumps to the high-half continuation before any Rust runs; that
  boot root doubles as the kernel working root (`paging_activate_kernel`).
  `paging_new_root` builds a cell root that maps the kernel + MMIO high
  (supervisor) and leaves the low half free for the loader; the kernel-RAM
  gigaregion is a level-1 table of supervisor superpages with the one `.user`
  slot delegated to U pages. **Unlike aarch64 the `.user` window is linked
  high**, adjacent to the kernel: RISC-V has no "large" code model (medany
  reaches only +-2 GiB PC-relative), so keeping `.user` next to the kernel
  keeps every kernel->`.user` reference in range. Isolation is unchanged - a
  cell is gated by the U bit on the leaf PTE, not by the address.
- **x86_64 (done):** a single CR3, so - like riscv64, unlike aarch64's TTBR
  split - every cell root still carries the kernel (mapped **supervisor**,
  high), since a trap enters with the cell root active and must reach the
  handler. The whole low half is left free, so a stock Linux `ET_EXEC` at
  0x400000 loads unmodified. `KERNEL_VA_BASE = 0xFFFF_FFFF_8000_0000` is the base
  of the top-2 GiB high half - the natural x86-64 higher-half region, addressed
  by the **kernel code model** (signed 32-bit relocations reaching the top
  2 GiB); the kernel is built with that model (xtask), overriding the small
  model the low-linked userland keeps. The boot trampoline (`arch/x86_64/boot.S`)
  runs under PVH: entered in 32-bit protected mode with paging off, it builds
  initial 4-level tables (PML4[511] = the kernel high linear map of phys
  0-2 GiB as 2 MiB supervisor pages; PML4[0] = a transient low identity so the
  instruction right after paging turns on stays mapped), enables PAE + long
  mode + paging, far-jumps to 64-bit, then absolute-jumps to the high-half
  continuation before any Rust runs; that boot root doubles as the kernel
  working root (`paging_activate_kernel`). `paging_new_root` builds a cell root
  that maps the kernel high (supervisor) - two PDs of 2 MiB supervisor pages
  with the one `.user` slot delegated to a 4 KiB page table whose leaves carry
  the US bit - and leaves the low half free for the loader (GDT/TSS/IDT and the
  LSTAR syscall entry all live at high VAs). **Like riscv64 the `.user` window
  is linked high**, adjacent to the kernel, so every kernel->`.user` reference
  stays within the kernel code model's reach. x86 MMIO is port I/O (COM1
  serial) or PCI config ports (CF8/CFC), so no device needs a high VA; ACPI
  tables and the PVH start-info are read through the high linear map.
- **The `phys_to_virt` seam.** The kernel touches physical frames (page tables,
  freshly allocated frames, ELF-load target pages, DMA rings) at
  `arch::phys_to_virt(pa)` - `pa | KERNEL_VA_BASE` on all three ISAs - never at
  the raw physical address, since the kernel no longer
  identity-maps RAM low. `arch::virt_to_phys` is the inverse, used to hand
  physical addresses to devices (virtio DMA) and to bounds-check the frame pool
  against the kernel image. Both are per-ISA functions behind the portable
  `crate::arch` surface, so `mm`, `load`, and the hw drivers stay ISA-neutral.

## 10. Honest costs

- Explicit commit policy and pressure-event handling push real work onto
  runtimes: a naive port that ignores pressure events simply loses its
  elastic memory (correct, but a sharper cliff than Linux's gradual swap-
  thrash).
- Pre-zeroing pools trade idle memory bandwidth for allocation latency (right
  on servers, worth a knob).
- Per-vcore arenas fragment memory across vcores for wildly asymmetric
  allocation patterns; the remote-free drain and periodic arena rebalance
  handle most of it, but a 1-vcore-allocates / 63-vcores-free workload tests
  that machinery.
