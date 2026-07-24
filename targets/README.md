# targets/

Custom target specs and the `std` port for rheo-os (docs/USERLAND.md M4).

The kernel and the `no_std` userland programs (M1-M3) use the built-in bare
targets (`x86_64-unknown-none`, `aarch64-unknown-none-softfloat`,
`riscv64gc-unknown-none-elf`). For real `std`, rheo-os needs its own
`target_os = "rheo"`:

- `rheo_os-<arch>.json` - the custom target spec (`os = "rheo"`, static,
  panic=abort). `riscv64` is provided and proven; the other ISAs follow the
  same shape.
- `patch-std.py` - patches the toolchain's vendored `rust-src` `std` to add a
  `rheo` platform: it routes rheo to std's portable fallbacks (single-threaded
  `no_threads` sync/TLS - sound because SMP is deferred - `generic` io errors,
  `unsupported` random) and installs the repo-owned real arms (allocator,
  stdio, command-line `args`, environment, and filesystem). Idempotent; the
  anchors are checked so a toolchain bump fails loudly instead of misapplying.
- `std-rheo/` - the rheo `sys` module sources this repo owns (`alloc.rs`, a
  hole-list heap over `SYS_MMAP`; `stdio.rs`, non-blocking fd 0/1/2 I/O;
  `args.rs`, argv from the crt0; `env.rs`, an in-process env table; `fs.rs`,
  `File`/`metadata`/`read_dir` over the file syscalls) that `patch-std.py`
  copies into the std tree, plus `rheo-rt/` (the crt0 `_start`, which reads
  `argc`/`argv` off the initial stack), `hello/` (a real-`std` proof program),
  and `coreutils/` (the `rheo-coreutils` multicall cell, M5).

## Building a std program for rheo-os

```sh
cargo xtask std-patch          # apply the rust-src patch (once per toolchain)
cargo build --manifest-path targets/std-rheo/hello/Cargo.toml --release \
  --target targets/rheo_os-riscv64.json \
  -Zbuild-std=std,panic_abort -Zbuild-std-features=compiler-builtins-mem \
  -Zjson-target-spec
```

A std program compiles, links, and **runs on the OS on all three ISAs** - the
`stdrun` test kernel loads and runs it (`cargo xtask test`). The targets are
soft-float, so only float-heavy programs are gated, pending U-mode FP/SIMD
enablement (docs/USERLAND.md M4).
