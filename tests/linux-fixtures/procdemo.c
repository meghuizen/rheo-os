/* Linux-personality L6 proof (docs/LINUX-COMPAT.md L6): one static-glibc
 * process that exercises pipe2 + fork + dup2 + execve + wait4 end-to-end.
 *
 * The parent creates a pipe, forks; the child redirects its stdout to the
 * pipe's write end and execve's /bin/cecho (a second static-glibc binary
 * served from the VFS), which prints its arguments. The parent reads the
 * pipe, reaps the child, and prints a deterministic transcript, exiting with
 * a code derived from the child's exit status. Built static-glibc ET_EXEC by
 * xtask build_linux_fixtures; no binary lives in git. */

#include <unistd.h>
#include <sys/wait.h>
#include <string.h>

int main(void) {
    int fd[2];
    if (pipe(fd) != 0) {
        _exit(10);
    }
    pid_t pid = fork();
    if (pid < 0) {
        _exit(11);
    }
    if (pid == 0) {
        /* child: stdout -> pipe write end, then exec the echo helper */
        dup2(fd[1], 1);
        close(fd[0]);
        close(fd[1]);
        char *argv[] = {"cecho", "hi", "there", 0};
        char *envp[] = {0};
        execve("/bin/cecho", argv, envp);
        _exit(127); /* exec failed */
    }

    /* parent: drain the pipe, reap, report */
    close(fd[1]);
    char buf[256];
    int n = 0, r;
    while (n < (int)sizeof(buf) - 1 && (r = read(fd[0], buf + n, sizeof(buf) - 1 - n)) > 0) {
        n += r;
    }
    close(fd[0]);

    int status = 0;
    wait4(pid, &status, 0, 0);
    int code = WIFEXITED(status) ? WEXITSTATUS(status) : -1;

    write(1, "child said: ", 12);
    write(1, buf, n);
    char c = (char)('0' + (code & 0xf));
    write(1, "child exit: ", 12);
    write(1, &c, 1);
    write(1, "\n", 1);
    return code + 7;
}
