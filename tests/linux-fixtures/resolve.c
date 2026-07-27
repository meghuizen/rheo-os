/* resolve.c - name resolution through glibc's own resolver, on this OS
 * (docs/LINUX-COMPAT.md L8-INET remote, docs/NETSTACK.md 18). An **unmodified
 * static-glibc C program** that calls `getaddrinfo` - the call every real
 * networked program makes - rather than hand-building a DNS packet the way
 * `inetremote.c` does.
 *
 * It only works if the OS provides what glibc's resolver reads:
 *   - /etc/nsswitch.conf   ("hosts: files dns")
 *   - /etc/hosts           (the `files` backend)
 *   - /etc/resolv.conf     (the `dns` backend's nameserver)
 * and if a datagram sent to that nameserver actually reaches the wire. Before
 * those files existed glibc fell back to the built-in nameserver 127.0.0.1:53,
 * which this kernel classifies as **loopback** - so the query went to an
 * in-kernel datagram queue with nothing listening, and the send reported
 * success anyway. `getaddrinfo` then failed for a reason nothing pointed at.
 *
 * Three phases, in the docs/ENGINEERING.md 4 shape:
 *
 *   0. **Deterministic, network-free**: a loopback UDP `sendto` to a port with
 *      nothing bound must be **refused** (ECONNREFUSED), not reported sent. This
 *      is the rejection that used to be a silent success (ENGINEERING.md 6/7).
 *   1. **Deterministic, network-free**: resolve `rheo.test`, which the kernel
 *      seeds into /etc/hosts as 10.9.8.7. The answer is a closed form, so the
 *      exact address is printed and asserted. This proves the resolver read the
 *      seeded configuration - no wire involved.
 *   2. **Live, bonus**: resolve a real public name. The address depends on the
 *      host's resolver (QEMU SLIRP proxies to it), so the address is NEVER
 *      printed or asserted - only one line from a fixed set saying whether a
 *      structurally valid answer came back.
 *
 * Built from source by xtask (static glibc, ET_EXEC, no relink), never
 * committed - mirrors inetremote.c / inet.c / hello.c.
 *
 * Expected stdout:
 *   resolve: loopback refused
 *   resolve: hosts 10.9.8.7
 *   resolve: dns ok          (or "resolve: dns none")
 *   resolve OK
 */

#include <arpa/inet.h>
#include <errno.h>
#include <netdb.h>
#include <netinet/in.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static void outs(const char *s) { write(1, s, strlen(s)); }
static int fail(const char *s) {
    outs(s);
    outs("\n");
    return 1;
}

/* The name + address the kernel seeds into /etc/hosts. Chosen so it is in no
 * real DNS zone: if this resolves, it resolved from the file. */
#define HOSTS_NAME "rheo.test"
#define HOSTS_ADDR "10.9.8.7"

/* A real public name for the live phase. Its address is never asserted. */
#define LIVE_NAME "api.anthropic.com"

/* Resolve `name` to a single IPv4 address, or 0 on any failure. */
static unsigned long resolve4(const char *name) {
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_INET; /* one query, not A + AAAA */
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo(name, NULL, &hints, &res) != 0 || res == NULL) return 0;
    unsigned long a = 0;
    if (res->ai_family == AF_INET && res->ai_addrlen >= sizeof(struct sockaddr_in)) {
        struct sockaddr_in *sin = (struct sockaddr_in *)res->ai_addr;
        a = (unsigned long)sin->sin_addr.s_addr;
    }
    freeaddrinfo(res);
    return a;
}

int main(void) {
    /* ---- 0. a loopback datagram nobody can receive must be refused ---- */
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    if (u < 0) return fail("resolve: socket FAIL");
    struct sockaddr_in nx;
    memset(&nx, 0, sizeof nx);
    nx.sin_family = AF_INET;
    nx.sin_port = htons(9); /* discard: nothing in this cell binds it */
    nx.sin_addr.s_addr = inet_addr("127.0.0.1");
    errno = 0;
    ssize_t r = sendto(u, "x", 1, 0, (struct sockaddr *)&nx, sizeof nx);
    if (r >= 0) return fail("resolve: nolistener send reported success FAIL");
    if (errno != ECONNREFUSED) return fail("resolve: nolistener errno FAIL");
    outs("resolve: loopback refused\n");
    close(u);

    /* ---- 1. /etc/hosts (deterministic, no wire) ---- */
    unsigned long h = resolve4(HOSTS_NAME);
    if (h == 0) return fail("resolve: hosts FAIL");
    struct in_addr ha;
    ha.s_addr = (in_addr_t)h;
    char text[INET_ADDRSTRLEN];
    if (inet_ntop(AF_INET, &ha, text, sizeof text) == NULL) return fail("resolve: ntop FAIL");
    if (strcmp(text, HOSTS_ADDR) != 0) return fail("resolve: hosts addr FAIL");
    outs("resolve: hosts " HOSTS_ADDR "\n");

    /* ---- 2. real DNS (live, reported not asserted) ---- */
    unsigned long d = resolve4(LIVE_NAME);
    /* Structure only: a non-zero IPv4 that is not the /etc/hosts answer means
     * the `dns` backend answered over the wire. The value itself is a property
     * of the host's resolver, so it is never printed. */
    if (d != 0 && d != h)
        outs("resolve: dns ok\n");
    else
        outs("resolve: dns none\n");

    outs("resolve OK\n");
    return 0;
}
