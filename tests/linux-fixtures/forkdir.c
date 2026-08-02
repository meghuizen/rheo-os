/* A directory fd inherited across `fork`, used by-fd in the child.
 *
 * This exists for one reason: the kernel stores an open VFS descriptor's **path** in a
 * per-cell funded side table now, not inline in the descriptor
 * (docs/EXECUTION-MODEL.md 9.8), and `fork` raw-copies the fd table. So the child
 * inherits `path_len` for every fd while its own path table is empty unless the fork
 * explicitly deep-copies it.
 *
 * The failure that causes is **silent**: the child reads a zeroed path and acts on it.
 * Nothing in the suite noticed - removing the deep copy left `linuxproc`, `linuxtools`
 * and `linuxdyn` all passing - which is why this fixture exists rather than a comment
 * asserting the copy is needed.
 *
 * `getdents64` on a directory fd is the operation that reads the stored path (the VFS is
 * re-entered by name), so the child doing it on an *inherited* fd is the narrowest test
 * of the copy. The parent does the same first, so a failure in the child cannot be
 * blamed on the directory being unreadable. */

#include <fcntl.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int count_dents(int fd) {
    char buf[2048];
    long n = syscall(SYS_getdents64, fd, buf, sizeof buf);
    return n > 0 ? 1 : 0;
}

int main(void) {
    int fd = open("/bin", O_RDONLY | O_DIRECTORY);
    if (fd < 0) {
        printf("forkdir: open failed\n");
        return 1;
    }
    /* The CHILD reads first, and that ordering is not cosmetic: `getdents64` keeps a
     * per-fd cursor (`dir_off`) which the child inherits, so a parent that read first
     * would hand the child an fd already at end-of-directory and the child would
     * legitimately see nothing. `lseek` does not reset that cursor. The first version of
     * this fixture had the parent read first and failed for exactly that reason - the
     * fixture was wrong, not the kernel. */
    pid_t p = fork();
    if (p < 0) {
        printf("forkdir: fork failed\n");
        return 3;
    }
    if (p == 0) {
        _exit(count_dents(fd) ? 0 : 9);
    }
    int st = 0;
    waitpid(p, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) {
        printf("forkdir: child read no entries\n");
        return 4;
    }
    printf("forkdir: child ok\n");

    /* And the parent afterwards, on its own cursor, so a child failure above could not
     * have been the directory simply being unreadable. */
    if (!count_dents(fd)) {
        printf("forkdir: parent read no entries\n");
        return 2;
    }
    printf("forkdir: parent ok\n");
    return 0;
}
