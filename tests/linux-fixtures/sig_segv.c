/* Fault-to-handler fixture for the Linux personality (docs/LINUX-COMPAT.md L5):
   install a SIGSEGV handler, then deliberately write through a null pointer.
   The kernel must deliver SIGSEGV to the handler (by trap-frame rewrite) rather
   than kill the cell; the handler writes a marker and _exit(0). The `linuxsig`
   test asserts exact stdout ("caught segv\n") and exit 0 - proving fault-to-
   handler instead of a killed cell. */
#include <signal.h>
#include <string.h>
#include <unistd.h>

static void handler(int sig) {
    (void)sig;
    static const char msg[] = "caught segv\n";
    write(1, msg, sizeof msg - 1);
    _exit(0);
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = handler;
    sigaction(SIGSEGV, &sa, NULL);
    volatile int *p = (volatile int *)0;
    *p = 42; /* null write -> SIGSEGV -> handler */
    write(1, "not reached\n", 12);
    return 1;
}
