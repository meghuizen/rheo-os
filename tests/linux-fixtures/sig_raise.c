/* Async signal fixture for the Linux personality (docs/LINUX-COMPAT.md L5):
   install a SIGUSR1 handler, raise it, and confirm the handler ran and the
   program resumed (rt_sigreturn) to exit 0. Built static-glibc by
   xtask::build_linux_fixtures; run in the `linuxsig` test kernel, which asserts
   its exact stdout ("handled 10\n") and exit code (0). */
#include <stdio.h>
#include <signal.h>
#include <string.h>

static volatile sig_atomic_t got = 0;

static void handler(int sig) {
    got = sig;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = handler;
    sigaction(SIGUSR1, &sa, NULL);
    raise(SIGUSR1);
    printf("handled %d\n", (int)got);
    return 0;
}
