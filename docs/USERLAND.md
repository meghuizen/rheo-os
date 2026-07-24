# Userland: building and running native apps

**Status:** Draft v0.1. New. Sits under POSIX-PERSONALITY.md and
FILESYSTEMS.md; the goal is that Rust (and later C/C++) programs built for
rheo-os run as real U-mode cells.

## Goal

Build and run native Rust/C/C++ apps on rheo-os, and eventually the Rust
rewrite of the GNU coreutils (uutils). Two decisions were made up front:

1. **Native target, recompile.** Apps are *recompiled* for a rheo-os target,
   not run as unmodified Linux binaries. This matches the OS's "translation
   at the edge" philosophy and is far more tractable than emulating the Linux
   syscall/ELF ABI. (A Linux-syscall shim could be layered on later.)
2. **A clean Rust libc as the translation layer.** A relibc-style libc
   (implemented in Rust) provides the C/POSIX ABI on top of rheo-os's native
   syscalls. It is C-ABI / source compatible, so C/C++/Rust source builds
   against it - not bit-for-bit glibc. "glibc ABI compatibility" (versioned
   symbols, glibc struct/TLS layout) is only needed to run unmodified
   glibc-linked binaries, which the recompile approach avoids.
3. **Rust first.** The first language to reach "runs a real program" is Rust
   (custom target + std port) - the direct path to uutils, and the most
   self-contained (no external C toolchain/sysroot needed).

## The gap this closes

Until now every U-mode program was hand-written in `kernel/src/user_progs.rs`
and *baked into the kernel image*, loaded into one fixed 2 MiB `.user`
window, and had to avoid any out-of-line call into kernel `.text`. That works
for a shell but cannot host a separately-compiled binary. A loaded ELF is
*self-contained* - it carries its own text/rodata/data and never references
kernel `.text` - so mapping its segments into a cell removes the `.user`
hazard by construction.

## Milestones

- **M1 - loader + address space. [done]** An ELF64 loader and a general
  per-cell address space (map an arbitrary user VA to an allocated frame, not
  just the fixed window). Loads a separately-compiled freestanding Rust ELF
  into a cell and runs it (`elfrun` test).
- **M2 - syscall surface. [done]** A multi-argument syscall ABI
  (`decode_syscall -> (nr, [u64;6])` on all three ISAs), kernel-native memory
  and process calls (`mmap`-anon bump allocator, `exit_group`), and fd-based
  file calls (`open/close/read/write/lseek`) forwarded to a **personality
  handler** (`svc::FileOps`) - function pointers a service/test registers,
  keeping the kernel free of a filesystem dependency. The handler runs in
  kernel context during the trap (user memory accessible) and is backed by
  the `posix/` VFS. The `posixrun` test loads a native program that opens a
  file, `mmap`s a buffer, reads the file, echoes it to stdout, and exits with
  the byte count. (`stat`/`getdents` and a real `brk` are folded into M3 as
  the libc needs them.)
- **M3 - libc.** A Rust libc (relibc-style): `crt0`/`_start`, `malloc`
  (over `brk`/`mmap`), `string.h`, `stdio`, `errno`, and the syscall stubs.
- **M4 - Rust target + std.** A `rheo-os` custom target JSON and a std `sys`
  backend over the libc, so `cargo build --target rheo-os` yields runnable
  ELFs.
- **M5 - coreutils.** Cross-compile a uutils/coreutils subset and run it.

Each milestone builds and boots on all three ISAs before it lands.

## Address-space layout (M1)

The kernel and MMIO occupy the low VAs (identity/supervisor) and differ per
ISA (x86 identity 0-1 GiB; riscv MMIO 0-1 GiB + kernel RAM 2-3 GiB; arm MMIO
0-1 GiB + kernel RAM 1-2 GiB). **VA `[4 GiB, ...)` is free in all three cell
roots**, so a loaded image lives high:

- `USER_IMAGE_BASE = 0x1_0000_0000` (4 GiB) - the ELF is linked here.
- `USER_STACK_TOP  = 0x2_0000_0000` (8 GiB) - stack grows down from here.

The loader allocates frames for each `PT_LOAD` segment, copies the file bytes
in (kernel RAM is identity-mapped, so a frame's PA is directly writable during
load), zeroes the rest (bss), and maps each segment's VA to its frame with the
segment's W^X permission. This is `arch::paging_map_frame` - a general
`va -> pa` user mapping that creates intermediate tables on demand, added
alongside the window-restricted `paging_map` the existing cells still use.

## Toolchain (M1)

The `userland/` crate is a `no_std` freestanding program built for each ISA's
bare target with a user linker script (base 4 GiB, `ENTRY(_start)`). `xtask`
builds it before the test kernels; the `elfrun` test kernel embeds the built
ELF with `include_bytes!` and loads it. Later milestones replace the embedded
image with one read off the live disk / ramfs (the block stack already does
this for data).
