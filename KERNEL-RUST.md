# Kernel Rust - Implementation Guide

**Status:** Draft v0.1. The how-to for writing Lattice kernel code in Rust.
Pairs with TOOLING.md (toolchain philosophy) and ARCHITECTURE.md section 7
(language rationale). This document is concrete and code-heavy; the why is
in TOOLING.md, the how is here.

---

## 1. Custom kernel targets

Three targets, one per ISA. Place under `targets/` in the workspace root.
These are JSON target specs consumed by `cargo build --target`.

### `x86_64-lattice-kernel.json`

```json
{
  "arch": "x86_64",
  "cpu": "x86-64-v3",
  "data-layout": "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-f80:128-n8:16:32:64-S128",
  "features": "+pcid,+avx2,+bmi2,+fma,+popcnt,-3dnow,-mmx,-sse4a",
  "linker-flavor": "ld.lld",
  "linker": "rust-lld",
  "panic-strategy": "abort",
  "disable-redzone": true,
  "vendor": "lattice",
  "os": "none",
  "env": "",
  "executables": true,
  "relocation-model": "static",
  "pre-link-args": {
    "ld.lld": ["-T", "kernel/link/x86_64.ld", "--gc-sections"]
  }
}
```

### `aarch64-lattice-kernel.json`

```json
{
  "arch": "aarch64",
  "cpu": "neoverse-v1",
  "data-layout": "e-m:e-i8:8:32-i16:16:32-i64:64-i128:128-n32:64-S128",
  "features": "+lse,+pauth,+bti,+mte,+sve2",
  "linker-flavor": "ld.lld",
  "linker": "rust-lld",
  "panic-strategy": "abort",
  "disable-redzone": true,
  "vendor": "lattice",
  "os": "none",
  "relocation-model": "static",
  "pre-link-args": {
    "ld.lld": ["-T", "kernel/link/aarch64.ld", "--gc-sections"]
  }
}
```

Key properties that go beyond the `unknown-none` targets:

- **`disable-redzone: true`** — not optional. An interrupt that fires while
  kernel code is in the x86 128-byte redzone silently corrupts it. This
  forces the compiler to never use that space, at the cost of one extra
  instruction on some function entries. Worth it unconditionally.
- **`+pcid`** — mandatory on x86; the TLB-tag-per-cell design requires it.
  Making it a target feature means a build on a pre-Westmere CPU fails at
  load time with a clear error, not at runtime with a silent #GP.
- **`+mte`** on ARM — memory tagging. Flagged as available so the hardened
  allocator and the POSIX personality can use it without per-site
  `#[target_feature]` annotations.
- **`+lse`** on ARM — large system extension atomics. The kernel's lock-free
  structures require these. Without LSE, the Rust compiler falls back to
  LL/SC loops which are subtly wrong under high contention.
- **`relocation-model: static`** — kernel code runs at a fixed physical/virtual
  address; no position-independent overhead.

---

## 2. Capability types — rights encoded at the type level

The single most important use of Rust's type system in the codebase: making
wrong-capability accesses a compile error rather than a runtime grant-check
failure. The runtime grant check still runs for the unforgeable guarantee;
this adds an ergonomics layer that catches mistakes in kernel and system code
before they reach the check.

### Rights as const-generic bitmasks

```rust
// kernel/capability/mod.rs

const READ:     u32 = 1 << 0;
const WRITE:    u32 = 1 << 1;
const EXECUTE:  u32 = 1 << 2;
const DELEGATE: u32 = 1 << 3;
const MAP:      u32 = 1 << 4;

// Rights<MASK> is a zero-size type; no runtime cost whatsoever
#[derive(Copy, Clone, Debug)]
pub struct Rights<const MASK: u32>;

pub trait RightSet: Copy { const MASK: u32; }
impl<const M: u32> RightSet for Rights<M> { const MASK: u32 = M; }

// Compile-time subset check: A ⊆ B iff (A & B) == A
// This is a const-expression assertion that fails at monomorphization
pub struct Assert<const COND: bool>;
pub trait IsTrue {}
impl IsTrue for Assert<true> {}

pub trait SubsetOf<R: RightSet>: RightSet {}
impl<const A: u32, const B: u32> SubsetOf<Rights<B>> for Rights<A>
where
    Assert<{ A & B == A }>: IsTrue,
{}

// Convenience type aliases
pub type ReadOnly<T>       = Capability<T, Rights<{ READ }>>;
pub type ReadWrite<T>      = Capability<T, Rights<{ READ | WRITE }>>;
pub type Executable<T>     = Capability<T, Rights<{ READ | EXECUTE }>>;
pub type Delegatable<T>    = Capability<T, Rights<{ READ | DELEGATE }>>;
pub type Full<T>           = Capability<T, Rights<{ READ | WRITE | EXECUTE
                                                     | DELEGATE | MAP }>>;
```

### The Capability type

```rust
use core::marker::PhantomData;

/// An unforgeable handle to a kernel resource.
/// Non-Clone by design: the move is the transfer.
pub struct Capability<T, R: RightSet> {
    handle:   CapabilityHandle,          // opaque u64, kernel-managed
    _phantom: PhantomData<fn() -> (T, R)>,
}

impl<T, R: RightSet> Capability<T, R> {
    /// Attenuation: narrow the rights. Compile error if R2 has bits R lacks.
    #[inline]
    pub fn attenuate<R2>(self) -> Capability<T, R2>
    where
        R2: RightSet + SubsetOf<R>
    {
        // No syscall needed: the handle is the same; the narrower type
        // prevents misuse at compile time. The kernel validates at use time.
        Capability { handle: self.handle, _phantom: PhantomData }
    }

    /// Delegate to another cell. Consumes the capability; the kernel
    /// transfers the grant to the target cell.
    pub fn delegate(self, target: CellId) -> Result<(), CapError> {
        // SAFETY: handle is valid (non-null, non-expired) by construction;
        // the capability is consumed, so we cannot use it after this call.
        let result = unsafe {
            syscall::cap_delegate(self.handle.as_raw(), target.as_raw())
        };
        core::mem::forget(self); // kernel now owns the lifecycle
        result.map_err(CapError::from)
    }

    /// Derive a child capability with a subset of rights (both narrowed).
    pub fn derive<R2>(&self) -> Result<Capability<T, R2>, CapError>
    where
        R2: RightSet + SubsetOf<R>
    {
        let handle = unsafe {
            syscall::cap_derive(self.handle.as_raw(), R2::MASK)
        }?;
        Ok(Capability { handle: CapabilityHandle::from_raw(handle),
                        _phantom: PhantomData })
    }
}

impl<T, R: RightSet> Drop for Capability<T, R> {
    fn drop(&mut self) {
        // Release the kernel grant on drop — RAII for capabilities
        unsafe { syscall::cap_release(self.handle.as_raw()) };
    }
}
```

### Usage example — compile-time enforcement

```rust
fn write_buffer(buf: ReadOnly<MemoryBuffer>, data: &[u8]) {
    // Trying to attenuate to a wider right is a compile error:
    // let rw = buf.attenuate::<ReadWrite<MemoryBuffer>>();
    // error: the trait bound `Rights<{READ|WRITE}>: SubsetOf<Rights<{READ}>>` 
    //        is not satisfied

    // Correct: attenuate to the same or narrower rights
    let _narrow = buf.attenuate::<ReadOnly<MemoryBuffer>>(); // fine
}
```

---

## 3. Ring buffer and descriptor abstractions

### The DmaSafe sealed trait

```rust
mod sealed {
    /// Sealed: only types explicitly marked can be used in DMA buffers.
    /// This prevents accidental DMA of types containing pointers, which
    /// would expose host virtual addresses to devices.
    pub unsafe trait DmaSafe: Copy + Sized + 'static {}
}
pub use sealed::DmaSafe;
```

### Submission queue entry — exactly 64 bytes

```rust
/// A submission queue entry. Must match the kernel ABI exactly.
/// The `align(64)` ensures each entry occupies its own cache line,
/// preventing false sharing between producer and consumer.
#[repr(C, align(64))]
pub struct SqEntry {
    pub opcode:    u8,
    pub flags:     u8,
    pub engine_id: u16,
    pub cap_id:    u32,
    pub flow_id:   u128,         // 16 bytes — the distributed trace handle
    pub user_data: u64,          // returned in CqEntry unchanged
    pub payload:   [u8; 32],     // opcode-specific
}
const _: () = assert!(core::mem::size_of::<SqEntry>() == 64);
unsafe impl DmaSafe for SqEntry {}

/// A completion queue entry.
#[repr(C, align(32))]
pub struct CqEntry {
    pub flow_id:   u128,
    pub user_data: u64,
    pub status:    u32,
    pub result:    u32,
}
const _: () = assert!(core::mem::size_of::<CqEntry>() == 32);
unsafe impl DmaSafe for CqEntry {}
```

### The ring buffer itself

```rust
use core::{
    marker::PhantomPinned,
    ptr,
    sync::atomic::{AtomicU32, Ordering},
};

/// A single-producer, single-consumer ring buffer over DMA-safe memory.
/// N must be a power of two for the index masking to work.
pub struct Ring<T: DmaSafe, const N: usize> {
    /// IOMMU-mapped memory. *Not* a Rust reference — the hardware also
    /// reads/writes this memory, so we use volatile primitives only.
    entries: *mut T,
    head:    AtomicU32,
    tail:    AtomicU32,
    _pin:    PhantomPinned, // ring must not move after IOMMU mapping
}

// SAFETY: the ring's memory is IOMMU-mapped and owned uniquely.
unsafe impl<T: DmaSafe, const N: usize> Send for Ring<T, N> {}

impl<T: DmaSafe, const N: usize> Ring<T, N> {
    const MASK: usize = N - 1;
    const _POWER_OF_TWO: () = assert!(N.is_power_of_two());

    /// Push one entry. Returns false if the ring is full.
    /// Hot path: one volatile write + one atomic store.
    #[inline(always)]
    pub fn push(&self, entry: T) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) as usize >= N {
            return false; // full
        }
        let idx = (head as usize) & Self::MASK;
        // SAFETY: idx is in-bounds by the capacity check above.
        // Volatile because the kernel / hardware reads this memory.
        unsafe { ptr::write_volatile(self.entries.add(idx), entry) };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Pop one entry. Returns None if empty.
    #[inline(always)]
    pub fn pop(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None; // empty
        }
        let idx = (tail as usize) & Self::MASK;
        // SAFETY: idx is in-bounds; volatile read matches volatile write.
        let entry = unsafe { ptr::read_volatile(self.entries.add(idx)) };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(entry)
    }
}
```

The `write_volatile` / `read_volatile` pair is load-bearing here. Without it
the compiler is allowed to reorder, combine, or eliminate these memory
operations because it cannot see the hardware reading the "dead" memory.
`Ordering::Release` on the head/tail updates pairs with the `Ordering::Acquire`
on the other side: the entry is always fully written before the index update
is visible to the consumer.

---

## 4. The `#[capability]` proc macro

A proc macro attribute that injects a compile-time capability check into a
function's signature. The caller must have the required capability type in
scope; missing it is a compile error.

```rust
// Usage in kernel/driver/nvme.rs:
#[capability(require = "ReadWrite<NvmeEngine>")]
pub fn submit_io(ring: &mut Ring<NvmeEntry, 256>, entry: NvmeEntry) {
    ring.push(entry);
}

// The macro expands this to approximately:
pub fn submit_io<__CapProof: __HasCapability<ReadWrite<NvmeEngine>>>(
    _cap_proof: &__CapProof,
    ring: &mut Ring<NvmeEntry, 256>,
    entry: NvmeEntry,
) {
    ring.push(entry);
}

// Call sites must supply a capability reference:
fn caller(cap: &ReadWrite<NvmeEngine>, ring: &mut Ring<NvmeEntry, 256>) {
    submit_io(cap, ring, entry);   // fine — cap satisfies the bound
}

fn bad_caller(ring: &mut Ring<NvmeEntry, 256>) {
    submit_io(ring, entry);
    // error[E0277]: the trait `__HasCapability<ReadWrite<NvmeEngine>>`
    //               is not implemented for `Ring<NvmeEntry, 256>`
}
```

The macro is syntactic sugar over a trait bound. The trait
`__HasCapability<C>` is implemented for `&Capability<T, R>` where `R`
satisfies the declared right set — the same `SubsetOf` machinery from
section 2. Nothing magical; a newtype wrapper around what the type system
already gives us.

---

## 5. Formal verification integration

### Scope

Verify exactly: the capability core (mint, delegate, derive, revoke,
grant-check, ~3-5k lines). No more. Verification effort scales super-linearly
with scope; a verified core with an unverified periphery is strictly better
than an unverified attempt at verifying everything.

### Verus for the capability core

Verus is the right tool: it handles Rust's unsafe subset, bitvector
arithmetic (which the rights-mask proofs need), and integrates with the
Z3 SMT solver. Syntax example:

```rust
// kernel/capability/verified.rs
use verus::prelude::*;

verus! {
    // Specification: pure, not compiled into the binary
    pub spec fn cap_valid(
        cap:     CapabilityToken,
        epoch:   u64,
        granted: u32,
    ) -> bool {
        cap.epoch == epoch
        && cap.rights & !granted == 0u32
    }

    // Monotonic attenuation: narrowing rights preserves validity
    pub proof fn attenuation_preserves_validity(
        cap:     CapabilityToken,
        mask:    u32,
        epoch:   u64,
        granted: u32,
    )
        requires
            cap_valid(cap, epoch, granted),
            mask & !cap.rights == 0u32,   // mask ⊆ cap.rights
        ensures
            cap_valid(
                CapabilityToken { rights: cap.rights & mask, ..cap },
                epoch,
                granted,
            )
    {
        // Z3 handles the bitvector arithmetic; no manual proof steps needed
        assert(cap.rights & mask & !granted == 0u32) by (bit_vector);
    }

    // The grant check implements the spec exactly
    #[verifier::when_used_as_spec(cap_valid)]
    pub fn grant_check(
        cap:     &CapabilityToken,
        epoch:   u64,
        granted: u32,
    ) -> (result: bool)
        ensures result == cap_valid(*cap, epoch, granted)
    {
        cap.epoch == epoch && cap.rights & !granted == 0
    }
}
```

### Prusti for simpler invariants

Prusti (ETH Zurich, Viper backend) handles simpler postcondition proofs —
queue depth bounds, admission control arithmetic — with less setup overhead
than Verus:

```rust
#[requires(self.depth < N)]
#[ensures(self.depth == old(self.depth) + 1)]
fn ring_push(&mut self, entry: SqEntry) { ... }
```

### CI integration

Both tools run as cargo subcommands (`cargo verus`, `cargo prusti`). The
verification suite runs in a separate CI stage after unit tests, not as part
of the main build, because verification is slow (~minutes for the capability
core). A verification failure is a release blocker; it never blocks a
development build.

---

## 6. Code size — avoiding the monomorphization trap

The kernel's binary size matters. Unconstrained generics produce a new
function copy per concrete type (monomorphization). In a library this is
fine; in a kernel that instantiates many engine types, allocator sizes, and
descriptor types, it produces a bloated binary and hurts I-cache behaviour.

### Static dispatch tables over generic functions

```rust
// BAD: N monomorphized copies of submit_graph, one per EngineKind
fn submit_graph<E: EngineOps>(graph: &Graph, engine: &E) {
    engine.submit(graph);
}

// GOOD: one function; static function-pointer table; indexed by kind
type SubmitFn = unsafe fn(graph: *const Graph, handle: u64);

static ENGINE_DISPATCH: [SubmitFn; EngineKind::COUNT] = [
    nvme_submit,   // EngineKind::Nvme
    gpu_submit,    // EngineKind::Gpu
    nic_submit,    // EngineKind::Nic
    dma_submit,    // EngineKind::Dma
];

#[inline(always)]
fn submit_graph(graph: &Graph, kind: EngineKind, handle: u64) {
    // One indexed load + indirect call; branch predictor handles the
    // common case (one or two dominant engine types per workload) well.
    unsafe { ENGINE_DISPATCH[kind as usize](graph as *const _, handle) }
}
```

Each per-engine function is monomorphic (one copy). The dispatch is a single
array index — no trait object fat pointer, no vtable lookup indirection.

### Build flags for the kernel target

```toml
# kernel/Cargo.toml
[profile.release]
opt-level     = "z"      # optimise for size over speed in non-hot paths
panic         = "abort"  # no unwinding machinery
lto           = true     # whole-crate dead-code elimination
codegen-units = 1        # single CGU for maximum LTO effectiveness
strip         = "symbols"

# Hot paths override per-function: #[inline(always)] on the ring push/pop,
# #[cold] on error paths, #[optimize(size)] on large cold functions.
```

`opt-level = "z"` applies to the whole crate; hot functions annotated with
`#[inline(always)]` are still inlined regardless (the annotation overrides
the global setting). `codegen-units = 1` is expensive in compile time but
mandatory for LTO to see the whole codebase and eliminate dead paths.

### Const generics for zero-cost fixed sizes

```rust
// Ring<T, N> — N is a const generic; the power-of-two check fires at
// compile time, not at runtime. No branch in push/pop.
const _: () = assert!(N.is_power_of_two());
let idx = (head as usize) & (N - 1);  // compiles to AND; no division
```

Use const generics for all fixed-size structures (ring sizes, descriptor
counts, slab sizes). Use `typenum` only when the const generic feature is
insufficient (e.g., type-level arithmetic across trait bounds); prefer const
generics when possible because they produce better error messages and simpler
code.

---

## 7. Cross-language interop — IDL binding targets

The system IDL generates bindings for all client languages from one source.
Current targets and their interop mechanism:

| Language | Mechanism | Status |
|---|---|---|
| Rust | Native structs + proc macros | Primary |
| C | `#[repr(C)]` header generation | Frozen ABI surface |
| Go | `cgo` bindings from the C header | For compat-edge controllers |
| **C#** | P/Invoke-compatible structs via C header | See below |
| Python | `ctypes`/`cffi` from C header | Tooling and scripting |
| Zig | Direct C header import | Tooling alternative |

### C# Native AOT interop

C# Native AOT produces self-contained binaries with a C ABI, making it a
viable language for system-service cells and compatibility personalities.
The Win32 personality in particular benefits from C# because the ecosystem
has the best Win32 API coverage (CsWin32 generates complete P/Invoke bindings
from the Windows metadata).

```csharp
// The Lattice queue ABI from C# — P/Invoke into the C-ABI kernel surface
[StructLayout(LayoutKind.Sequential, Pack = 1, Size = 64)]
public unsafe struct SqEntry {
    public byte    Opcode;
    public byte    Flags;
    public ushort  EngineId;
    public uint    CapId;
    public fixed byte FlowId[16];
    public ulong   UserData;
    public fixed byte Payload[32];
}

// NativeAOT function export — callable from Rust via the C ABI
[UnmanagedCallersOnly(EntryPoint = "win32_cell_init")]
public static int Init(ulong controlQueueCap) {
    // ... Win32 personality initialization
    return 0;
}
```

A C# Native AOT cell runs identically to any other cell: it holds only the
capabilities it was granted, its GC runs inside the cell address space, and
its crashes are bounded to the cell boundary. The GC causes no problems for
the rest of the system because the cell is the isolation unit.

The IDL code generator adds a C# output mode: for each IDL struct it emits
a `[StructLayout(Sequential)]` C# struct that is bit-for-bit identical to the
Rust `#[repr(C)]` struct, so P/Invoke across the cell boundary is a
zero-copy cast.

---

## 8. Interrupt handling — the assembly boundary

Interrupt handlers are the one place where Rust's type system cannot fully
protect you, because the CPU delivers interrupts at arbitrary points and
the calling convention is entirely hardware-defined.

```rust
// x86_64: extern "x86-interrupt" is Rust's built-in interrupt calling
// convention — saves/restores all caller-saved registers automatically.
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

extern "x86-interrupt" fn page_fault_handler(
    frame: InterruptStackFrame,
    error_code: u64,
) {
    // This function is entered with a 16-byte-aligned stack (hardware
    // guarantee) and no redzone (target ensures this).
    // Minimum work here: record the fault, schedule the handling cell.
    crate::fault::record_page_fault(frame.instruction_pointer, error_code);
}
```

For ARM64, the exception vectors are written in assembly and call into Rust:

```asm
// kernel/arch/aarch64/vectors.S
.balign 2048
.global exception_vector_table
exception_vector_table:
    // EL1 synchronous (current level, SP_EL1)
    .balign 128
    stp x29, x30, [sp, #-16]!
    mov x0, sp
    bl  el1_sync_handler        // Rust function: fn el1_sync_handler(sp: u64)
    ldp x29, x30, [sp], #16
    eret
```

The Rust handler receives the stack pointer and reconstructs the exception
context from there. The assembly stub is as small as possible; all logic is
in Rust.

---

## 9. Panic handler — production kernel behaviour

```rust
// kernel/panic.rs
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // 1. Disable interrupts on this core immediately.
    arch::interrupts::disable();

    // 2. Emit a structured event on the per-core event ring.
    //    This may succeed or may not (if the event system is itself broken);
    //    try but do not depend on it.
    let _ = events::emit_kernel_panic(
        info.location().map(|l| (l.file(), l.line())),
        info.message(),
    );

    // 3. Attempt a controlled per-core halt. Other cores continue.
    //    The watchdog will detect this core is gone and fence it.
    arch::halt_current_core();
}
```

The design principle: `panic!` in kernel code means "an invariant that
should be provably impossible was violated." It is not an error handler; it
is an assertion. The vast majority of error paths use `Result<T, E>` and
propagate errors as events. `unwrap()` appears in kernel code only on
operations whose failure is provable-impossible by the surrounding invariants
— and those should be marked `// SAFETY: <reason>` like any unsafe block.
