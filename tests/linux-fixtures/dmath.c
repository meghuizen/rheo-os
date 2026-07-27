// A stock dynamically-linked (PIE) glibc C program that links a SECOND shared
// library besides libc - libm - so `ld.so` must load two libraries and resolve
// one's symbols/versions against the other. This is the multi-library case the
// single-library `dhello` does not exercise (docs/LINUX-COMPAT.md L7,
// GOAL-DYN-MULTILIB).
//
// `-fno-builtin` stops gcc constant-folding `sqrt(16.0)` at compile time, which
// is what forces a genuine `libm.so.6` dependency (DT_NEEDED libm) and a runtime
// call resolved across objects. Built with `-lm`.
#include <stdio.h>
#include <math.h>

int main(void) {
    double r = sqrt(16.0);
    printf("dmath: sqrt16=%d\n", (int)r);
    return (int)r; // exit 4
}
