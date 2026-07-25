/* Dynamically-linked glibc C fixture for the Linux personality
   (docs/LINUX-COMPAT.md L7). Built UNMODIFIED with the ISA's gcc and NO
   -static / -no-pie, so it is a stock ET_DYN/PIE binary whose PT_INTERP names
   ld-linux; the kernel loads ld.so, which maps + relocates this program and
   libc at runtime. The `linuxdyn` test seeds /lib with the real toolchain
   glibc and asserts this program's exact stdout + exit code. */
#include <stdio.h>
#include <string.h>

int main(void) {
    char buf[32];
    strcpy(buf, "hello from dynamic glibc");
    printf("%s\n", buf);
    return 12;
}
