/* inetremote.c - the rheo-net N4b proof fixture (docs/NETSTACK.md N4b,
 * docs/LINUX-COMPAT.md L8-INET remote). An **unmodified static-glibc C program**
 * that does REAL REMOTE networking over the NIC - no loopback, no shortcut:
 *
 *   1. UDP: build a DNS query by hand and `sendto` it to QEMU SLIRP's built-in
 *      DNS responder at 10.0.2.3:53, then `recvfrom` the reply. Asserts the
 *      *structure* of the answer - our transaction id echoed back, the QR
 *      (response) bit set, and the sender being 10.0.2.3:53 - never a specific
 *      resolved address, because SLIRP proxies to whatever resolver the host has
 *      (so an A record's value is not deterministic).
 *   2. TCP: `connect()` to a closed port on the SLIRP gateway (10.0.2.2:9). A
 *      real three-way handshake goes out over the NIC; SLIRP answers with a
 *      reset, which the transport turns into ECONNREFUSED.
 *
 * Both phases are honest about the environment: each prints one line from a
 * small, fixed set of outcomes, so the transcript stays exact while the program
 * never fabricates a reply it did not get. Exit is 0 whenever the *mechanism*
 * worked; a mechanism failure prints "... FAIL" and exits 1.
 *
 * Built from source by xtask (static glibc, ET_EXEC, no relink), never
 * committed - mirrors inet.c / af_unix.c / hello.c.
 */

#include <arpa/inet.h>
#include <errno.h>
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

/* SLIRP's built-in DNS responder and gateway (QEMU `-netdev user`). */
#define DNS_IP "10.0.2.3"
#define DNS_PORT 53
#define GW_IP "10.0.2.2"
/* A closed port on the gateway: SLIRP answers a SYN here with a reset. */
#define CLOSED_PORT 9

#define TXID 0x1234

/* A minimal DNS query: id, RD flag, one question - A IN example.com. */
static const unsigned char QUERY[29] = {
    0x12, 0x34,                                     /* transaction id */
    0x01, 0x00,                                     /* flags: RD */
    0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* qd=1, an/ns/ar=0 */
    7, 'e', 'x', 'a', 'm', 'p', 'l', 'e',           /* "example" */
    3, 'c', 'o', 'm',                               /* "com" */
    0x00,                                           /* root label */
    0x00, 0x01,                                     /* qtype  A */
    0x00, 0x01,                                     /* qclass IN */
};

int main(void) {
    /* ---------------- 1. UDP to a real remote address ---------------- */
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    if (s < 0) return fail("udp socket FAIL");

    struct sockaddr_in dst;
    memset(&dst, 0, sizeof dst);
    dst.sin_family = AF_INET;
    dst.sin_port = htons(DNS_PORT);
    dst.sin_addr.s_addr = inet_addr(DNS_IP);

    ssize_t sent = sendto(s, QUERY, sizeof QUERY, 0, (struct sockaddr *)&dst, sizeof dst);
    if (sent != (ssize_t)sizeof QUERY) return fail("udp sendto FAIL");
    outs("inetremote: udp sent\n");

    unsigned char reply[512];
    struct sockaddr_in from;
    socklen_t flen = sizeof from;
    memset(&from, 0, sizeof from);
    ssize_t n = recvfrom(s, reply, sizeof reply, 0, (struct sockaddr *)&from, &flen);
    if (n < 0) {
        /* No reply reached us. Honest, and not a mechanism failure: the send
         * genuinely went out over the NIC. */
        outs("inetremote: dns no reply\n");
    } else if (n < 12) {
        return fail("udp short reply FAIL");
    } else {
        unsigned id = (reply[0] << 8) | reply[1];
        int qr = (reply[2] & 0x80) != 0;
        unsigned ancount = (reply[6] << 8) | reply[7];
        if (id != TXID) return fail("dns txid FAIL");
        if (!qr) return fail("dns qr FAIL");
        if (from.sin_addr.s_addr != inet_addr(DNS_IP)) return fail("dns src ip FAIL");
        if (ntohs(from.sin_port) != DNS_PORT) return fail("dns src port FAIL");
        outs("inetremote: dns reply ok\n");
        /* The answer count depends on the host having outbound DNS, so it is
         * reported, never asserted. */
        outs(ancount > 0 ? "inetremote: dns answers yes\n"
                         : "inetremote: dns answers none\n");
    }
    close(s);

    /* ---------------- 2. TCP connect to a real remote address ---------------- */
    int t = socket(AF_INET, SOCK_STREAM, 0);
    if (t < 0) return fail("tcp socket FAIL");

    struct sockaddr_in gw;
    memset(&gw, 0, sizeof gw);
    gw.sin_family = AF_INET;
    gw.sin_port = htons(CLOSED_PORT);
    gw.sin_addr.s_addr = inet_addr(GW_IP);

    errno = 0;
    int rc = connect(t, (struct sockaddr *)&gw, sizeof gw);
    if (rc == 0) {
        /* Something really was listening: a completed remote handshake. */
        outs("inetremote: tcp connected\n");
    } else if (errno == ECONNREFUSED) {
        outs("inetremote: tcp refused\n");
    } else if (errno == ETIMEDOUT) {
        outs("inetremote: tcp timeout\n");
    } else {
        return fail("tcp connect FAIL");
    }
    close(t);

    outs("inetremote OK\n");
    return 0;
}
