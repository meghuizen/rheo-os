/* Static-glibc C fixture for the Linux personality (docs/LINUX-COMPAT.md L2).
   Built -static-pie per arch by xtask::build_linux_fixtures; run in the
   `linuxrun` test kernel, which asserts its exact stdout and exit code. */
#include <stdio.h>
#include <string.h>

int main(void) {
    char buf[32];
    strcpy(buf, "hello from glibc C");
    printf("%s\n", buf);
    return 9;
}
