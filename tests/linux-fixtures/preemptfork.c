// Two single-context Linux processes, both compute-bound at the same time.
//
// This exists to reach `linux::proc::preempt_cell` - the arm that moves the CPU to
// **another cell** when the interrupted one has no ready sibling context of its own
// (docs/ARCHITECTURE-DEBT.md 7.6, which recorded it as unexercised). Every existing
// preemption proof either runs native cells or a multi-threaded Linux cell, and a
// multi-threaded cell always has a ready sibling, so the first arm always answers
// and the second never runs.
//
// The shape that reaches it: `fork`, then **both** processes spin issuing no syscall
// at all. Each is a single-context cell, so the sibling arm has nothing to pick, and
// cooperatively the child cannot run until the parent reaches `waitpid` - which is
// exactly why the parent spins *before* waiting. Under preemption the parent's slice
// expires inside its own loop and the only place left to go is the child.
//
// The loop touches a `volatile` so the compiler cannot delete it, and the count is
// sized to outlast several 1 ms slices under TCG. The transcript is
// scheduling-independent: the parent prints once, after reaping.

#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile unsigned long sink;

static void spin(unsigned long n) {
    for (unsigned long i = 0; i < n; i++) {
        sink += i;
    }
}

#define SPIN 40000000UL

int main(void) {
    pid_t p = fork();
    if (p < 0) {
        printf("preemptfork fork failed\n");
        return 1;
    }
    if (p == 0) {
        spin(SPIN);
        _exit(7);
    }
    spin(SPIN);
    int st = 0;
    if (waitpid(p, &st, 0) != p) {
        printf("preemptfork wait failed\n");
        return 1;
    }
    printf("preemptfork parent done child %d\n", WEXITSTATUS(st));
    return 0;
}
