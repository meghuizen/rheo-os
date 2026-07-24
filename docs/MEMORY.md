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
  the **large code model** (xtask). x86_64/riscv64 remain low-half for now.
- **The `phys_to_virt` seam.** The kernel touches physical frames (page tables,
  freshly allocated frames, ELF-load target pages, DMA rings) at
  `arch::phys_to_virt(pa)` - `pa | KERNEL_VA_BASE` on aarch64, the identity on
  x86_64/riscv64 - never at the raw physical address, since the kernel no longer
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
