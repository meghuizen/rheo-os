/* Default-disposition fixture for the Linux personality (docs/LINUX-COMPAT.md
   L5): raise SIGABRT with no handler installed. The kernel must apply the
   default disposition (terminate) and report the cell's exit as 128+signo =
   134. The `linuxsig` test asserts exit 134 and empty stdout - proving SIG_DFL
   semantics. */
#include <signal.h>

int main(void) {
    raise(SIGABRT); /* SIG_DFL for SIGABRT: terminate (128 + 6 = 134) */
    return 3;       /* not reached */
}
