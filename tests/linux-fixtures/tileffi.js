// tileffi.js - JavaScript calling a tile kernel, loaded as a real script FILE off the
// live ext4 disk (docs/TILES.md 13.4d).
//
// A file rather than `-e`, and that difference is the point: asked to run a file, Bun
// calls `createFakeTemporaryNodeExecutable`, which writes a stand-in `node` into a temp
// directory - so this path needs a writable filesystem, which `-e` does not. The
// read-only ext4 root is now composed with a read-write ramfs at /tmp by the mount
// table, and this is what proves that composition carries a real runtime's needs.
//
// `bun:ffi` is built into Bun, so there is no addon to compile: the runtime opens the
// library, generates a native trampoline for the declared signature, and calls through
// it. The value returned is the low 31 bits of an FNV-1a hash of the whole int8 -> i32
// GEMM output, so it proves the kernel ran rather than that a symbol resolved - the same
// number the librheo cells, the static `tilelinux` binary and the `dlopentile` C probe
// produce for a 32x32x32 GEMM. 31 bits because a JS number is exact only to 2^53.
const { dlopen, FFIType } = require("bun:ffi");
const lib = dlopen("/lib/libtileso.so", {
  tile_gemm_check: {
    args: [FFIType.u32, FFIType.u32, FFIType.u32],
    returns: FFIType.i32,
  },
});
console.log("tileffi: gemm " + lib.symbols.tile_gemm_check(32, 32, 32));
