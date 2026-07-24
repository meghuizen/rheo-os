/* A minimal static-glibc echo, the execve target for the L6 proof
 * (docs/LINUX-COMPAT.md L6): prints argv[1..] separated by spaces + a newline.
 * Loaded from the VFS by the parent's execve; built by xtask
 * build_linux_fixtures. */

#include <unistd.h>
#include <string.h>

int main(int argc, char **argv) {
    for (int i = 1; i < argc; i++) {
        if (i > 1) {
            write(1, " ", 1);
        }
        write(1, argv[i], strlen(argv[i]));
    }
    write(1, "\n", 1);
    return 0;
}
