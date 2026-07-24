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
  `unsupported` random) and installs a real allocator. Idempotent; the anchors
  are checked so a toolchain bump fails loudly instead of misapplying.
- `std-rheo/` - the rheo `sys` module sources this repo owns (currently
  `alloc.rs`, a hole-list heap over `SYS_MMAP`) that `patch-std.py` copies into
  the std tree, plus `hello/`, a real-`std` proof program.

## Building a std program for rheo-os

```sh
cargo xtask std-patch          # apply the rust-src patch (once per toolchain)
cargo build --manifest-path targets/std-rheo/hello/Cargo.toml --release \
  --target targets/rheo_os-riscv64.json \
  -Zbuild-std=std,panic_abort -Zjson-target-spec
```

std compiles and links today. Running a std binary on the OS additionally
needs a crt0 `_start`, the rheo `stdio`/`process`/`fs` `sys` arms over the M2
syscalls, and U-mode FP/SIMD enablement - see docs/USERLAND.md M4.
