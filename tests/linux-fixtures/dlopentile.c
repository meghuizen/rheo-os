/* dlopentile - can a program running on rheo-os `dlopen` a shared library and call
 * into it? (docs/TILES.md 13.4c)
 *
 * This is the probe that decides whether a JS runtime's FFI can reach a tile kernel.
 * Bun's `bun:ffi` and Node's N-API addons both come down to `dlopen` + `dlsym` +
 * an indirect call, so if that works for C it is reachable for them, and if it does
 * not, the reason is a fact about the personality rather than about JavaScript.
 *
 * The library it opens exports the tile framework's GEMM behind a C ABI and returns
 * an FNV-1a hash of the result, so the value proves the kernel actually ran - the
 * same number `tilelinux` and `librheo-fa` produce for the same shape.
 *
 * One line from a fixed set, so the transcript stays exact. A failure prints the
 * stage it failed at rather than a generic error: `dlopen` and `dlsym` fail for
 * different reasons and the distinction is the whole value of a probe.
 */

#include <dlfcn.h>
#include <stdio.h>

typedef unsigned long long (*gemm_hash_fn)(unsigned, unsigned, unsigned);

int main(void) {
  void *h = dlopen("/lib/libtileso.so", RTLD_NOW);
  if (!h) {
    printf("dlopentile: dlopen failed\n");
    return 1;
  }
  gemm_hash_fn f = (gemm_hash_fn)dlsym(h, "tile_gemm_hash");
  if (!f) {
    printf("dlopentile: dlsym failed\n");
    return 2;
  }
  unsigned long long got = f(32, 32, 32);
  printf("dlopentile: gemm %016llx\n", got);
  return 0;
}
