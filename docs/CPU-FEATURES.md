# CPU features: event delivery, and what to do when the hardware says no

Two foundations, and they are the same idea applied at two levels: **ask the hardware
what it can do, never the vendor's name, and when the answer is no, resolve the request
to something honest rather than refusing or pretending.**

This document owns the x86-64 event-delivery foundation (FRED) and the general
feature-resolution layer. It pairs with docs/EXECUTION-MODEL.md, which is what runs on
top, and with docs/TILES.md, whose SIMD dispatch is the working precedent for everything
in section 2.

---

## 1. Event delivery: FRED as the foundation, IDT as the fallback

### 1.1 Why this is foundational rather than an optimisation

FRED (Flexible Return and Event Delivery) deletes a defect class this tree has hit **four
separate times**, and that is the argument for it - not novelty and not speed.

The scar: `SYSRET` **consumes RCX and R11**, so it is correct only when returning from the
syscall that `SYSCALL` entered. Four instances, each found the hard way:

1. the first ring-3 fault resume,
2. `rt_sigreturn` rewriting its frame in place,
3. `enter_user_first` re-entering a timer-captured frame on a secondary,
4. the fault resume that genuinely re-executes an instruction.

None of them faulted. Each was two corrupted registers and a program that stopped making
sense - in one case four cores whose bounded loops stopped terminating. The rule was
eventually written down ("SYSRET is only for returning from the syscall it was entered
by") and the resume paths moved to `IRET`.

Under FRED there is one return pair, `ERETS` (to supervisor) and `ERETU` (to user), and
**neither consumes a general-purpose register**. The defect class cannot be expressed.

What else it buys, mapped to this tree's own problems:

| FRED property | What it fixes here |
|---|---|
| Event delivery always to a defined **stack level** (four levels, chosen per event type) | per-entity kernel stacks (docs/EXECUTION-MODEL.md stage E4) plus nested-event safety become structural, instead of an IST table maintained by hand per vector |
| Kernel `GS` loaded by the CPU on entry | `swapgs` disappears. This tree deliberately never gives a cell a GS base *because* `swapgs` is error-prone; FRED removes the reason for the restriction |
| `CS`/`SS` always valid on entry | the ring-3 frame capture in `vectors.S` stops being special-cased |
| Nested-event state carried in the event frame | a preemption slice firing while the kernel holds a reference into a funded table is a defined state, not a hazard to split "note it" from "act on it" around |
| One entry point for all events | the trap path stops being "a syscall stub plus N vector stubs", which is where three of the four SYSRET instances lived |

### 1.2 Vendor neutrality is a code property, not a claim about silicon

**The implementation must not contain the word Intel in a condition.** FRED is discovered
through `CPUID.(EAX=07H,ECX=1):EAX` bit 17 (FRED) and bit 18 (LKGS), enabled through
`CR4.FRED`, and configured through the `IA32_FRED_*` MSRs. Every one of those is an
architectural interface. Any x86-64 CPU that reports the bit gets the FRED path, whoever
made it; any CPU that does not gets the IDT path. There is no vendor check anywhere,
because a vendor check is a guess about silicon and the bit is a fact about it.

That distinction is load-bearing rather than stylistic, and this tree has the scar to prove
it. The x86-64 LAPIC was driven only through the x2APIC MSR block on the reasoning that
modern x86 has x2APIC. QEMU 8.2 TCG reports none, the MSR block was inert, and **x86 SMP
and the x86 timer were the same defect** for months - fixed by asking the hardware
(request x2APIC, read `IA32_APIC_BASE` back, keep it only if `EXTD` latched; otherwise map
the xAPIC MMIO page and require a write/read-back). A vendor-named condition would have
made that unaskable.

**Honest about AMD specifically, because the question deserves a fact and not a
reassurance:** FRED originates as an Intel specification, and this document does not claim
which AMD parts implement it - that is a property of silicon, discoverable at run time and
not knowable from here. What this design guarantees is the thing that is in our control:
if an AMD CPU reports the FRED bits, it takes the FRED path with no code change and no
vendor exception, and if it does not, it takes the IDT path which is the one the entire
test suite runs on today. The failure mode a vendor check creates - correct silicon taking
the slow path forever because nobody updated a list - is designed out rather than
documented.

Known vendor asymmetries the design must therefore *not* assume away, each probed
individually rather than inferred from any other: `LKGS` (bit 18) is a separate bit from
FRED and must be probed separately; `CR4.FRED` may be present without the CPU supporting
every event type this kernel wants at a given stack level; and the `WRMSRNS` /
`IA32_FRED_*` MSR set is checked by writing and reading back, not by assuming that a set
CPUID bit implies a writable MSR. That last one is exactly the x2APIC lesson restated.

### 1.3 The bring-up rule: verify, report, keep both paths proven

```
   probe CPUID.(EAX=7,ECX=1):EAX bit 17 (FRED) and bit 18 (LKGS)
      |
      +-- both reported -> publish the per-core RSP/SSP stack-level MSRs, read them
      |                    back, set CR4.FRED, read IT back, install the single event
      |                    entry point, then TAKE ONE SYNTHETIC EVENT and check where
      |                    it landed before anything depends on the path
      |                       |
      |                       +-- landed correctly -> arch::event_mode() == Fred
      |                       +-- did not         -> unwind to Idt, print the reason
      |
      +-- absent -> the IDT path exactly as today, byte for byte
                    -> arch::event_mode() == Idt
```

Three requirements, each forced by a real defect in this tree:

- **Verify, do not claim.** A claimed event path that never delivers is a *hang*, not a
  slow path - the NVMe interrupt lesson, where masking a table entry turned a passing test
  into a 120-second timeout with no output. So bring-up takes one event through the new
  path and checks where it landed.
- **The verification must not be able to hang either.** The NVMe path needed
  `arch::irq_window` - a one-instruction window rather than an idle wait - for exactly this
  reason. FRED bring-up gets the same treatment.
- **Report the mode.** `arch::event_mode()` is printed at boot and readable by a test, so
  no proof can say "FRED" about an IDT run. The three wake modes
  (`net_rx::IdleMode`) and `input::interrupt_driven()` are the same pattern.

**The IDT path is not legacy, and this was checked rather than assumed.** The obvious
reading - "8.2 is old, a newer QEMU will model FRED" - is wrong, and the difference matters
because it decides whether the FRED path can ever be proven in this repository.
**QEMU 11.0.3 was built from source here and inspected**: `fred` and `lkgs` appear in
`target/i386/cpu.c`'s feature names and in `target/i386/kvm/kvm.c`, and **nothing in
`target/i386/tcg/` mentions FRED at all**. So FRED in QEMU is a bit that can be *exposed to
a KVM guest whose host CPU has it*, not an event-delivery path TCG *emulates*. This
container has no KVM.

The consequence, stated so nobody re-runs the experiment: the IDT path is the one all 210
boot tests run on under any QEMU available here, it stays first-class and proven, and the
FRED path is lab-gated on real silicon - exactly as AVX-512 is (also confirmed absent from
QEMU 11's TCG: zero mentions in `target/i386/tcg/`). A design where the fallback rots is a
design with one untested path and one unreachable one, so the ordering is deliberate: the
IDT path stays the tested default and FRED is added beside it.

**What a newer QEMU *does* unlock, measured the same way**, because the same inspection
found four things 8.2 lacked - `CPUID_EXT_X2APIC` and `CPUID_EXT_PCID` in
`TCG_EXT_FEATURES`, `INVPCID` in the TCG leaf-7 word, and a real `riscv-iommu-pci` device
(`hw/riscv/riscv-iommu-pci.c`). The first three are the interesting ones for this section's
thesis: this kernel *observes* all three rather than assuming them, so on QEMU 11 the same
binary should take the x2APIC path instead of the xAPIC-MMIO fallback and report a usable
TLB tag - and if it fails there, the fallback had been masking a defect in the primary path.
That is a genuine test of observe-never-infer rather than a version bump.

### 1.4 It reduces per-ISA divergence

ARM64's `eret` and RISC-V's `sret` consume no registers, and both ISAs already carry the
interrupted state in a frame. x86-64 was the outlier, and every one of the four SYSRET
instances is a cost of that. FRED makes x86-64 look like the other two, which is
docs/TARGET-ARCHITECTURES.md 4's goal rather than an exception to it.

---

## 2. Feature resolution: translate, do not refuse - and never silently change the answer

### 2.1 The rule

A cell asks for an **operation**, not an instruction. The framework resolves it to one of
four outcomes, in this order, and **reports which**:

```
  request: "int8 matmul, 64x64x64"
      |
      +-- 1. NATIVE       the CPU has the instruction (AMX, VNNI, SVE, SME)
      +-- 2. TRANSLATED   a different instruction sequence computing the SAME BITS
      +-- 3. EMULATED     a portable scalar sequence computing the same bits, slower
      +-- 4. UNAVAILABLE  no honest answer exists -> refuse, with the reason
```

Never a fifth outcome. In particular never "something close enough", which is the failure
this section exists to prevent.

The precedent is already in the tree and working: `librheo::tile::simd` probes each SIMD
tier at boot, **functionality-checks it bit-exact against the scalar oracle**, benchmarks
it, picks the fastest that passed, and falls back to scalar - and `librheotile` asserts
on-OS which tier ran. Section 2 is that discipline generalised and given a vocabulary.

### 2.2 The trap: translation is not always bit-exact, and the difference is invisible

This is the part a naive "just translate it" gets wrong, so it is stated first and as a
hard rule.

**`FMA` is not `mul` followed by `add`.** `fma(a,b,c)` computes `a*b+c` with **one**
rounding; `a*b` then `+c` rounds **twice**. The results differ in the last bits, and the
difference is not a fault, not a log line, and not reproducible by inspection - it is a
slightly different number. A tile kernel whose contract is "bit-identical to the scalar
reference" (which is exactly the contract `librheotile` and `librheotilebattle` assert, and
what makes the FlashAttention proofs equalities rather than tolerances) is **broken** by
that translation while appearing to work.

So every translation carries a classification, and it is checked rather than asserted:

| Class | Meaning | Allowed where |
|---|---|---|
| `BitExact` | provably the same bits, verified at boot against the scalar oracle over a fixed vector set | anywhere, including a bit-exactness contract |
| `Numeric` | mathematically the same operation, different rounding or different accumulation order | only where the caller declared a tolerance |
| `Refused` | neither can be offered | the request fails with the reason named |

A `Numeric` translation offered to a `BitExact` contract is **`Refused`**, not silently
substituted. That single rule is the whole value of this section: without it, "translate
when the hardware lacks it" is a mechanism for producing wrong answers quietly, which
docs/ENGINEERING.md 7 forbids more strongly than it forbids missing features.

### 2.3 What translates, and how

| Missing | Resolution | Class | Note |
|---|---|---|---|
| AMX tile matmul | blocked GEMM over AVX-512 / AVX2 / NEON, then scalar | **BitExact** for integer (int8 -> i32 accumulate is exact) | this is what `tile::kernels::gemm_i8_i32` already is; AMX becomes a fourth tier of an existing dispatch, not a new path |
| AVX-512 | two AVX2 halves per vector | **BitExact** for integer and for f32 add/mul (per-element ops, same rounding) | not for reductions, where the accumulation order changes - those are `Numeric` |
| VNNI (`vpdpbusd`) | widen + multiply + horizontal add | **BitExact** (integer) | |
| FMA | `mul` then `add` | **Numeric** - one rounding becomes two | refused under a bit-exact contract; the tile kernels' contract is bit-exact, so they must not take it |
| f16 / bf16 / FP8 arithmetic | convert to f32, compute, convert back | **BitExact** *for storage dtypes* - which is already this tree's rule: MMA over a storage dtype is a compile error until a device lowers it (docs/TILES.md) | |
| SVE / SME | fixed-width NEON tiles | BitExact (integer) | |
| `POPCNT`, `LZCNT`, `BMI` | portable bit tricks | BitExact | |
| hardware AES / SHA | the software backends already in `net::crypto` | BitExact - the software path *is* the oracle the hardware path was checked against | already the default, for the documented reason that baseline `+aes` has no graceful fallback (docs/NETSTACK.md 22) |
| hardware RNG (RDSEED / RNDR) | the ChaCha20 DRBG over a documented seed floor | not applicable - different contract | already built, already honest (docs/TIME-IDENTITY.md) |

Two entries in that table are not hypothetical: the crypto row and the RNG row are the
policy this tree already follows, and they are listed to show that section 2 is a name for
existing practice rather than a new subsystem.

### 2.4 Where it must not live

The resolution table is **userspace**, in `librheo::tile` and beside it, not in the kernel.
The kernel's job is to say what the hardware has (`hw::Inventory` already does, from CPUID
/ `ID_AA64*` / the device-tree ISA string) and to enable the state a feature needs across a
switch (the XSAVE work, already done). Choosing a lowering is a library decision, and
putting it in the kernel would make the kernel contain math - which is precisely what
keeps every syscall, trap and interrupt free of FP save/restore (docs/SUBSTRATE.md pillar
4). One exception exists and is already the shape: the kernel's compute engine
`#[path]`-includes librheo's dependency-free tile kernels rather than reimplementing them,
so there is one implementation and one oracle.

### 2.5 Honest scope

Under QEMU TCG: AVX2 is exposed and AVX-512 is not, AMX is not modelled, and TCG models no
SIMD *speedup* at all - so the probe's benchmark can legitimately keep the scalar tier
here, and the selection adapts to the real host. That is already recorded in
docs/TILES.md and it applies to everything in this section: the **functionality** check
(bit-exact against scalar) runs and is asserted everywhere; the **performance** choice is
only meaningful on hardware.
