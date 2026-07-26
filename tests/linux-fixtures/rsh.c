/* rsh - a minimal POSIX-ish shell for the Linux-personality L6 P11 gate
 * (docs/LINUX-COMPAT.md L6, docs/POSIX-PERSONALITY.md 5). A full dash/busybox
 * cross-build was out of budget; this is a small from-scratch static-glibc
 * shell that exercises exactly the L6 process primitives the gate is about:
 * fork + execve + wait4 + pipe2 + dup2, with pipelines and && / || short-circuit.
 *
 * Usage: rsh -c "<command line>". Each command word list is run as
 * /bin/coreutils <words> (the suite is the uutils/coreutils multicall), so a
 * pipeline stage `seq 1 5` execs `coreutils seq 1 5`. Built static-glibc
 * ET_EXEC by xtask build_linux_fixtures; no binary lives in git.
 */

#include <unistd.h>
#include <sys/wait.h>
#include <string.h>

#define MAX_STAGES 8
#define MAX_WORDS 32

/* Split `s` in place on spaces into `out[0..]` (NULL-terminated). */
static int split_words(char *s, char **out, int max) {
    int c = 0;
    char *p = s;
    while (*p && c < max - 1) {
        while (*p == ' ') p++;
        if (!*p) break;
        out[c++] = p;
        while (*p && *p != ' ') p++;
        if (*p) *p++ = 0;
    }
    out[c] = 0;
    return c;
}

/* Run one pipeline (a string containing single '|' separators); return the
 * exit status of the last stage. */
static int run_pipeline(char *line) {
    char *stages[MAX_STAGES];
    int ns = 0;
    stages[ns++] = line;
    for (char *p = line; *p && ns < MAX_STAGES; p++) {
        if (*p == '|') {
            *p = 0;
            stages[ns++] = p + 1;
        }
    }

    int prev_read = -1;
    pid_t pids[MAX_STAGES];
    for (int i = 0; i < ns; i++) {
        int pfd[2] = {-1, -1};
        if (i < ns - 1) {
            if (pipe(pfd) != 0) return 127;
        }
        pid_t pid = fork();
        if (pid == 0) {
            if (prev_read >= 0) dup2(prev_read, 0);
            if (i < ns - 1) dup2(pfd[1], 1);
            if (prev_read >= 0) close(prev_read);
            if (pfd[0] >= 0) close(pfd[0]);
            if (pfd[1] >= 0) close(pfd[1]);
            char *w[MAX_WORDS];
            w[0] = "coreutils";
            split_words(stages[i], w + 1, MAX_WORDS - 1);
            char *envp[] = {0};
            execve("/bin/coreutils", w, envp);
            _exit(127);
        }
        pids[i] = pid;
        if (prev_read >= 0) close(prev_read);
        if (i < ns - 1) {
            close(pfd[1]);
            prev_read = pfd[0];
        }
    }

    int last = 0;
    for (int i = 0; i < ns; i++) {
        int st = 0;
        wait4(pids[i], &st, 0, 0);
        if (i == ns - 1) last = WIFEXITED(st) ? WEXITSTATUS(st) : 1;
    }
    return last;
}

int main(int argc, char **argv) {
    if (argc < 3 || strcmp(argv[1], "-c") != 0) return 2;
    char *cmd = argv[2];

    int status = 0;
    int first = 1;
    int op = 0; /* 0 = run, 1 = &&, 2 = || */
    char *seg = cmd;
    char *p = cmd;
    while (1) {
        int found = 0;
        char *q = p;
        while (*q) {
            if (q[0] == '&' && q[1] == '&') { found = 1; break; }
            if (q[0] == '|' && q[1] == '|') { found = 2; break; }
            q++;
        }
        int run = first || (op == 1 && status == 0) || (op == 2 && status != 0);
        if (run) {
            char buf[512];
            int len = (int)(q - seg);
            if (len > 511) len = 511;
            memcpy(buf, seg, len);
            buf[len] = 0;
            status = run_pipeline(buf);
        }
        first = 0;
        if (!found) break;
        op = found;
        p = q + 2;
        seg = p;
    }
    return status;
}
