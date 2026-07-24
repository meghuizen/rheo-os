# Development - Compile, Boot, Run, Debug

**Status:** Draft v0.1. The practical how-to. Pairs with TOOLING.md (toolchain
philosophy), BOOT.md (the conceptual measured-boot chain), TARGET-
ARCHITECTURES.md (ISA specifics), and EMULATION.md (QEMU in CI). Build
sequence is in BUILD-ORDER.md.

Short version: it is a `no_std` Rust kernel, cross-compiled with Cargo to bare
targets, booted in QEMU with a serial console and a GDB stub. Everything below
is emulation-first; real hardware comes at milestone M1 (ARCHITECTURE.md 8.6).

## 1. Prerequisites

Install once:

- **Rust nightly** (needed for `build-std` and some `no_std` features), via
  `rustup`.
- Bare targets: `rustup target add x86_64-unknown-none aarch64-unknown-none-softfloat
  riscv64gc-unknown-none-elf`.
- `rust-src` component (for `build-std`): `rustup component add rust-src`.
- **cargo-binutils** + `llvm-tools-preview` (gives `rust-objdump`,
  `rust-nm`, `rust-size`).
- **QEMU** 8.x+ system emulators: `qemu-system-x86_64`,
  `qemu-system-aarch64`, `qemu-system-riscv64`.
- **GDB** with multi-arch support (`gdb-multiarch`) or LLDB.
- **swtpm** (software TPM, for the measured-boot path).
- **OVMF/EDK2** firmware images (UEFI boot on x86-64/ARM64).

No custom toolchain, no forked compiler - the deliberate boredom from
TOOLING.md 3.

## 2. Repository shape

```
lattice/
  xtask/            # build/run/test orchestration (cargo xtask ...)
  kernel/           # no_std kernel crate
    src/
    arch/           # per-ISA modules behind the Arch trait
      x86_64/       # boot.S, vectors.S, context_switch.S, paging, apic...
      aarch64/
      riscv64/
    link/           # linker scripts per arch
  idl/              # the system IDL + codegen (TOOLING.md 2)
  services/         # system service cells (state store, reconciler, ...)
  runtime/          # the strand runtime library
  tests/            # in-QEMU test kernels
  targets/          # custom target JSON if a built-in target won't do
```

The **xtask pattern** (a small Rust binary invoked as `cargo xtask`) wraps the
awkward parts - `build-std` flags, linker script selection, QEMU invocation,
GDB attach - so day-to-day commands are short: `cargo xtask run --arch
aarch64`.

## 3. Compile

Cargo builds bare-metal with `build-std` for `core` and `alloc`:

```
cargo build \
  --target x86_64-unknown-none \
  -Z build-std=core,alloc,compiler_builtins \
  -Z build-std-features=compiler-builtins-mem \
  -p kernel
```

Cross-compiling to another ISA is only the `--target` flag
(`aarch64-unknown-none-softfloat`, `riscv64gc-unknown-none-elf`). Any port that needs
more than a new `arch/` module implementation is treated as an architecture
bug (TARGET-ARCHITECTURES.md 4). The assembly stubs (`boot.S`, vectors,
context switch) are the only non-Rust files, a handful per ISA.

Output is an ELF; some boot paths want a flat binary
(`rust-objcopy -O binary`).

## 4. Boot models

Two paths, chosen by how far along you are:

- **Fast dev path - direct kernel load.** QEMU loads the kernel ELF directly
  (`-kernel`), skipping firmware. x86-64 uses a PVH or multiboot2 entry;
  ARM64 and RISC-V load an ELF/flat image and jump to it with a device tree
  pointer in a register. Fastest iteration; no measured boot.
- **Real boot path - UEFI + measured boot.** OVMF firmware runs a UEFI
  bootloader stub that measures and loads the kernel, extending TPM PCRs
  (BOOT.md 1). Use this once the attestation chain is being built (M3), and
  in a subset of CI to keep the real path honest.

RISC-V always has **OpenSBI** as M-mode firmware underneath (QEMU's default
`-bios`); the kernel runs in S-mode and talks to firmware over SBI calls.

## 5. Run in QEMU

Concrete invocations. The xtask wraps these; they are spelled out so the
mechanics are visible.

**x86-64** (Q35 machine, split IRQ chip so the virtual IOMMU works):

```
qemu-system-x86_64 \
  -machine q35,kernel-irqchip=split \
  -cpu max,+pcid \
  -m 4G -smp 4 \
  -device intel-iommu,intremap=on \
  -kernel target/x86_64-unknown-none/debug/kernel \
  -serial mon:stdio \
  -no-reboot -no-shutdown \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
  -d int,guest_errors -D qemu.log
```

**ARM64** (virt machine with GICv3 and an SMMUv3 IOMMU):

```
qemu-system-aarch64 \
  -machine virt,gic-version=3,iommu=smmuv3,virtualization=on \
  -cpu max \
  -m 4G -smp 4 \
  -kernel target/aarch64-unknown-none-softfloat/debug/kernel \
  -serial mon:stdio -no-reboot
```

**RISC-V** (virt machine, OpenSBI provides firmware):

```
qemu-system-riscv64 \
  -machine virt,aia=aplic-imsic \
  -cpu max -m 4G -smp 4 \
  -bios default \
  -kernel target/riscv64gc-unknown-none-elf/debug/kernel \
  -serial mon:stdio -no-reboot
```

Add as subsystems come online:

- **Devices (the first engines):** `-device virtio-blk-pci`,
  `-device virtio-net-pci`, `-device virtio-rng-pci` (BUILD-ORDER.md steps
  11-13).
- **TPM (measured boot / attestation):** run `swtpm` on a socket, then
  `-chardev socket,...` + `-tpmdev emulator,...` + `-device tpm-tis`
  (or `tpm-crb`).
- **UEFI:** `-drive if=pflash,format=raw,readonly=on,file=OVMF_CODE.fd`
  and a writable `OVMF_VARS.fd`.
- **Multi-host cluster testing:** multiple QEMU instances on a bridged or
  socket-networked backend, or the deterministic simulation harness
  (EMULATION.md 5) for reproducible protocol tests without real networking.

`-serial mon:stdio` multiplexes the guest serial console and the QEMU monitor
on your terminal (Ctrl-A C toggles to the monitor).

## 6. Early output and clean exit

Before any driver exists, two things make a kernel debuggable:

- **Serial println.** The very first arch code wires a UART (16550 on x86-64
  and RISC-V virt, PL011 on ARM64 virt) so `println!` works. This is step 1
  in BUILD-ORDER.md for a reason - you cannot debug what cannot talk.
- **A QEMU exit device** so a test kernel can signal pass/fail to CI:
  `isa-debug-exit` on x86-64 (write a code to port 0xf4), `sifive_test` on
  RISC-V virt, or ARM/RISC-V **semihosting** (`-semihosting`) for an exit
  syscall. The test harness reads QEMU's exit status: a chosen success code
  means the in-QEMU test suite passed.

## 7. Debugging

**GDB stub (the workhorse).** Add `-s -S` to any QEMU command: `-s` opens a
GDB server on TCP 1234, `-S` freezes the CPU at reset so you attach before the
first instruction.

```
gdb-multiarch target/aarch64-unknown-none-softfloat/debug/kernel
(gdb) set architecture aarch64        # if multiarch needs a hint
(gdb) target remote :1234
(gdb) break kernel_main
(gdb) continue
(gdb) info registers
(gdb) backtrace
```

`rust-gdb` / `rust-lldb` add pretty-printers for Rust types. For a stripped
flat binary, load symbols separately with `add-symbol-file`.

**QEMU monitor** (Ctrl-A C, or `-monitor telnet:...`):

- `info registers` - CPU state.
- `info mem` / `info tlb` - page-table and TLB inspection (invaluable for
  the paging bring-up and for cell address-space bugs).
- `info irq` / `info pic` - interrupt controller state.
- `x/16i $pc` - disassemble around the program counter.

**QEMU tracing** to catch faults before you even have a debugger attached:

- `-d int` logs every exception/interrupt - the fastest way to diagnose a
  triple fault or an unexpected trap during early boot.
- `-d int,cpu_reset,guest_errors,mmu -D qemu.log` for a fuller picture.
- `-d in_asm` logs the guest instructions QEMU executed (see section 8).

**Panic handling.** The `no_std` panic handler prints location + message to
serial and then triggers the QEMU exit device with a failure code, so a panic
in CI is an immediate red, not a hang. In interactive runs it halts so GDB can
inspect the frozen state.

**Turning an address into a line:** `rust-addr2line -e kernel 0xADDR`
(or `addr2line`) resolves a raw address from a panic or a fault log back to
source file and line.

## 8. Disassembly and inspection

- **`rust-objdump -d kernel`** (or `llvm-objdump`, `objdump`) - disassemble
  the kernel image; `-S` interleaves source when debug info is present.
- **`rust-nm` / `readelf -a` / `rust-size`** - symbols, ELF headers, section
  sizes (watch the kernel image stay small).
- **`rust-objcopy`** - ELF to flat binary, or strip.
- **QEMU `-d in_asm,out_asm`** - see the actual guest instruction blocks QEMU
  translated, useful when the ELF disassembly and runtime behaviour disagree
  (self-modifying trampolines, the context-switch stub).
- **radare2 / Cutter / Ghidra** - for deeper reverse-engineering of a blob or
  a firmware image, or to inspect a contained vendor driver cell's binary.
- **GPU code (later, in the contained vendor cell):** `nvdisasm` and
  `cuobjdump` for PTX/SASS, `roc-obj`/`llvm-objdump` for AMD GCN/CDNA. These
  live in the compilation service's vendor cell (AI-ARCHITECTURE.md 4), not
  the kernel, so they only matter once the AI layer is in flight.

## 9. The CI loop (what runs on every commit)

Per EMULATION.md 2 and TOOLING.md 4: for each of the three ISAs, boot a test
kernel headless in QEMU with a timeout, run the in-QEMU suite (capability-core
tests, unit tests, `loom` permutations, fuzz corpora), and read the QEMU exit
code via the debug-exit/semihosting device to gate pass/fail. Serial output is
captured to a log artifact. Absolute performance numbers (P1-P12) only gate on
the hardware lab; QEMU runs track correctness and trend microbenchmarks.

## 10. First-day smoke test

The minimal loop that proves the whole toolchain before any real subsystem
exists: `cargo xtask run --arch riscv64` builds the kernel, launches QEMU with
a serial console, and the kernel prints one line and exits clean through the
test device. If that line appears and CI goes green on all three ISAs, the
compile/boot/run/debug chain is working and BUILD-ORDER.md step 1 is done.
