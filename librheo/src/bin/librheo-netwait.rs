//! `librheo-netwait` - the rheo-net **N2d** proof program (docs/NETSTACK.md, the
//! async-receive path): **true async receive**. Where `librheo-net` (Phase G)
//! re-polled `OP_NET_RX` until a frame turned up, this program *parks*: the
//! receiving strand suspends on the reactor's network slot, the vcore runs the
//! cell's other strands, and only when they have all parked does the reactor block
//! in the kernel (`SYS_WAIT_NET`), which idles the CPU at WFI until the NIC's RX
//! interrupt fires (where that interrupt is wired - docs/NETSTACK.md per-ISA table).
//!
//! The sequence is deterministic and network-free (QEMU SLIRP answers all of it):
//!
//! 1. read the NIC MAC (`OP_NET_MAC`);
//! 2. **drain** the receive queue with the non-blocking `net::try_recv`, so the
//!    waits below genuinely start from an empty queue;
//! 3. spawn a **witness** strand that counts how many times it is resumed;
//! 4. send a broadcast **ARP request** for the SLIRP gateway `10.0.2.2` and
//!    `net::recv(...).await` - **parks**; the ARP reply is the wake;
//! 5. send a **TCP SYN** to a closed port on the gateway and park again - a second
//!    real frame (the reset) through the same blocking path;
//! 6. park once more with a **deadline** (`net::recv_timeout`) on an empty queue
//!    with nothing in flight: no frame can arrive, so the kernel arms the deadline
//!    and *halts the CPU* until it fires - the 0%-CPU park, deterministically,
//!    and the "packet or RTO, whichever comes first" primitive a transport needs.
//!
//! It exits `0x42` only if: both frames are the expected replies, the witness ran
//! while the receiver was parked (so the receive really suspended rather than
//! spinning the vcore), the bounded wait returned empty at its deadline, and the
//! reactor recorded **exactly one wakeup per receive** (`rt::net_wakeups()`) -
//! one park + one wake each, never N re-polls. The `netwait` test kernel asserts
//! that code plus the kernel-side evidence (genuine NIC interrupts taken, and the
//! halt, where the interrupt is wired).

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use librheo::{net, println, rt};

/// Failure code (0 = success), set inside the `'static` async root.
static CODE: AtomicI32 = AtomicI32::new(0);
/// How many times the witness strand was resumed while the receiver was parked.
static WITNESS: AtomicU32 = AtomicU32::new(0);
/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;
/// Resumptions the witness strand performs (it yields between each).
const WITNESS_STEPS: u32 = 4;
/// Frames the receiver is willing to park for before giving up. Under SLIRP the
/// ARP reply is the only inbound frame, so one park is the expected case; the
/// budget only tolerates unrelated broadcast traffic.
const MAX_PARKS: u64 = 8;

/// SLIRP's default guest address and gateway (QEMU `-netdev user`).
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
const ETHERTYPE_ARP: u16 = 0x0806;
/// A closed port on the SLIRP "host" alias, and the source port we use. SLIRP
/// answers a SYN here by attempting a real host connection, which fails only after
/// the guest yields - the deterministic *later* frame the idle-park proof needs.
const SYN_PORT: u16 = 9;
const SYN_SPORT: u16 = 40000;
/// Deadline for the idle-park phase: long enough that the halt is unambiguous,
/// short enough not to slow the test.
const IDLE_TIMEOUT_NS: u64 = 20_000_000; // 20 ms

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(run());
    let code = CODE.load(Ordering::Relaxed);
    if code != 0 {
        return code;
    }
    println!("librheo-netwait: parked receive woke on a real frame");
    OK_CODE
}

fn fail(code: i32) {
    CODE.store(code, Ordering::Relaxed);
}

/// Build a 42-byte broadcast ARP request "who has `GATEWAY_IP`" from `mac`.
fn build_arp_request(mac: [u8; 6]) -> [u8; 42] {
    let mut f = [0u8; 42];
    f[0..6].copy_from_slice(&[0xff; 6]); // dst: broadcast
    f[6..12].copy_from_slice(&mac); // src: our MAC
    f[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    f[14..16].copy_from_slice(&1u16.to_be_bytes()); // htype: Ethernet
    f[16..18].copy_from_slice(&0x0800u16.to_be_bytes()); // ptype: IPv4
    f[18] = 6; // hlen
    f[19] = 4; // plen
    f[20..22].copy_from_slice(&1u16.to_be_bytes()); // oper: request
    f[22..28].copy_from_slice(&mac); // sha: our MAC
    f[28..32].copy_from_slice(&GUEST_IP); // spa: our IP
    f[38..42].copy_from_slice(&GATEWAY_IP); // tpa: gateway
    f
}

/// Build a 54-byte IPv4 **TCP SYN** to `GATEWAY_IP:SYN_PORT` from `mac`. SLIRP
/// answers a SYN to a closed port by opening a real host socket, which fails
/// asynchronously - so the reset comes back only after the guest yields the CPU,
/// which is exactly the wait we want to prove halts.
fn build_tcp_syn(mac: [u8; 6], gw_mac: [u8; 6]) -> [u8; 54] {
    let mut f = [0u8; 54];
    f[0..6].copy_from_slice(&gw_mac);
    f[6..12].copy_from_slice(&mac);
    f[12..14].copy_from_slice(&0x0800u16.to_be_bytes()); // IPv4

    // IPv4 header (20 bytes, no options).
    let ip = &mut f[14..34];
    ip[0] = 0x45; // version 4, IHL 5
    ip[2..4].copy_from_slice(&40u16.to_be_bytes()); // total length: 20 + 20
    ip[4..6].copy_from_slice(&0x1234u16.to_be_bytes()); // id
    ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes()); // don't fragment
    ip[8] = 64; // TTL
    ip[9] = 6; // protocol: TCP
    ip[12..16].copy_from_slice(&GUEST_IP);
    ip[16..20].copy_from_slice(&GATEWAY_IP);
    let ck = checksum(ip, 0);
    f[24..26].copy_from_slice(&ck.to_be_bytes());

    // TCP header (20 bytes, no options).
    let tcp = &mut f[34..54];
    tcp[0..2].copy_from_slice(&SYN_SPORT.to_be_bytes());
    tcp[2..4].copy_from_slice(&SYN_PORT.to_be_bytes());
    tcp[4..8].copy_from_slice(&1u32.to_be_bytes()); // seq
    tcp[12] = 5 << 4; // data offset 5 words
    tcp[13] = 0x02; // SYN
    tcp[14..16].copy_from_slice(&1024u16.to_be_bytes()); // window
    // TCP checksum over the pseudo-header (src, dst, proto, length) + header.
    let mut pseudo = [0u8; 12];
    pseudo[0..4].copy_from_slice(&GUEST_IP);
    pseudo[4..8].copy_from_slice(&GATEWAY_IP);
    pseudo[9] = 6;
    pseudo[10..12].copy_from_slice(&20u16.to_be_bytes());
    let partial = sum16(&pseudo, 0);
    let ck = checksum(&f[34..54], partial);
    f[50..52].copy_from_slice(&ck.to_be_bytes());
    f
}

/// Accumulate `data` into a 32-bit ones-complement sum (big-endian 16-bit words).
fn sum16(data: &[u8], mut acc: u32) -> u32 {
    let mut i = 0;
    while i + 1 < data.len() {
        acc += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        acc += (data[i] as u32) << 8;
    }
    acc
}

/// The Internet checksum of `data` with a partial sum folded in.
fn checksum(data: &[u8], partial: u32) -> u16 {
    let mut acc = sum16(data, partial);
    while acc >> 16 != 0 {
        acc = (acc & 0xFFFF) + (acc >> 16);
    }
    !(acc as u16)
}

/// True if `f` is an IPv4 TCP segment from the gateway to our SYN's source port
/// (SLIRP's answer to a SYN for a closed port: a reset, or a SYN-ACK if something
/// on the host happens to accept).
fn is_gateway_tcp_answer(f: &[u8]) -> bool {
    f.len() >= 54
        && f[12..14] == 0x0800u16.to_be_bytes()
        && f[14] >> 4 == 4
        && f[23] == 6 // protocol TCP
        && f[26..30] == GATEWAY_IP
        && f[36..38] == SYN_SPORT.to_be_bytes()
}

/// True if `f` is an ARP **reply** whose sender IP is the gateway.
fn is_gateway_arp_reply(f: &[u8]) -> bool {
    f.len() >= 42
        && f[12..14] == ETHERTYPE_ARP.to_be_bytes()
        && f[20..22] == 2u16.to_be_bytes() // oper: reply
        && f[28..32] == GATEWAY_IP // sender protocol address
}

/// The witness strand: yields `WITNESS_STEPS` times, counting each resumption.
/// It can only make progress while the receiver is parked, so a non-zero count
/// proves the receive suspended instead of holding the vcore.
async fn witness() {
    for _ in 0..WITNESS_STEPS {
        rt::yield_now().await;
        WITNESS.fetch_add(1, Ordering::Relaxed);
    }
}

async fn run() {
    // 1. The NIC MAC (the ARP source, and what QEMU's RX filter accepts, so
    //    SLIRP's unicast reply reaches our receive queue).
    let mac = match net::mac().await {
        Ok(m) => m.0,
        Err(_) => {
            fail(10);
            return;
        }
    };

    // 2. Drain anything already queued, so the park below starts from empty.
    let mut buf = [0u8; 1600];
    loop {
        match net::try_recv(&mut buf).await {
            Ok(0) => break,
            Ok(_) => continue,
            Err(_) => {
                fail(11);
                return;
            }
        }
    }

    // 3. A sibling strand that can only run while the receiver is parked.
    rt::spawn(witness());

    // 4. Send the broadcast ARP request.
    let req = build_arp_request(mac);
    if net::send(&req).await.is_err() {
        fail(12);
        return;
    }

    // 5. Park until a frame arrives. Every iteration is one park + one wake.
    let mut parks = 0u64;
    while parks < MAX_PARKS {
        let n = match net::recv(&mut buf).await {
            Ok(n) => n,
            Err(_) => {
                fail(13);
                return;
            }
        };
        parks += 1;
        // The reactor must have recorded exactly one wakeup per parked receive -
        // no re-poll storm hiding behind the await.
        if rt::net_wakeups() != parks {
            fail(14);
            return;
        }
        if n == 0 {
            fail(15); // the kernel's wait gave up (no NIC / poll budget expired)
            return;
        }
        if !is_gateway_arp_reply(&buf[..n]) {
            continue; // unrelated frame: park again
        }
        // The witness must have advanced while we were parked.
        let seen = WITNESS.load(Ordering::Relaxed);
        if seen != WITNESS_STEPS {
            fail(16);
            return;
        }
        println!(
            "librheo-netwait: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, ARP reply for 10.0.2.2 ({n} B) after {parks} park(s), witness {seen}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );
        // 6. The gateway also answers a TCP SYN to a closed port - a second real
        //    frame, taken through the same parked receive.
        let mut gw_mac = [0u8; 6];
        gw_mac.copy_from_slice(&buf[6..12]);
        let parks = match syn_phase(mac, gw_mac, &mut buf, parks).await {
            Some(p) => p,
            None => return, // CODE already set
        };
        // 7. The idle-park phase: park on a frame that will never come, with a
        //    deadline. Nothing is in flight, so the kernel *must* halt the CPU and
        //    be woken by the armed deadline - the 0%-CPU park, deterministically.
        idle_phase(&mut buf, parks).await;
        return; // CODE is whatever idle_phase left
    }
    fail(17); // no ARP reply within the park budget
}

/// A second parked receive over a different protocol: SLIRP answers a TCP SYN to a
/// closed port on its host alias with a reset. Returns the running park count, or
/// `None` if it failed (CODE set).
async fn syn_phase(mac: [u8; 6], gw_mac: [u8; 6], buf: &mut [u8], mut parks: u64) -> Option<u64> {
    let syn = build_tcp_syn(mac, gw_mac);
    if net::send(&syn).await.is_err() {
        fail(18);
        return None;
    }
    let budget = parks + MAX_PARKS;
    while parks < budget {
        let n = match net::recv(buf).await {
            Ok(n) => n,
            Err(_) => {
                fail(19);
                return None;
            }
        };
        parks += 1;
        if rt::net_wakeups() != parks {
            fail(20);
            return None;
        }
        if n == 0 {
            fail(21); // the kernel's wait gave up
            return None;
        }
        if is_gateway_tcp_answer(&buf[..n]) {
            println!(
                "librheo-netwait: gateway answered the SYN ({n} B, TCP flags {:#04x}) over the same parked receive",
                buf[47]
            );
            return Some(parks);
        }
    }
    fail(22); // no answer to the SYN within the park budget
    None
}

/// The idle-park phase: park on a frame that will not come, with a deadline.
/// Nothing is in flight (both answers above are consumed and the queue is empty),
/// so the kernel has no frame to find: it arms the deadline, **halts the CPU**, and
/// returns 0 when the timer wakes it. That is the 0%-CPU park, proven
/// deterministically - and it is the same halt a received frame wakes (the receive
/// interrupts taken above are counted by the kernel). It is also the mechanism a
/// transport needs: wait for a packet **or** the retransmission timeout.
async fn idle_phase(buf: &mut [u8], parks: u64) {
    let n = match net::recv_timeout(buf, IDLE_TIMEOUT_NS).await {
        Ok(n) => n,
        Err(_) => {
            fail(23);
            return;
        }
    };
    if rt::net_wakeups() != parks + 1 {
        fail(24);
        return;
    }
    if n != 0 {
        fail(25); // an unexpected frame arrived - the deadline was not what woke us
        return;
    }
    println!(
        "librheo-netwait: bounded receive on an empty queue hit its {IDLE_TIMEOUT_NS} ns deadline \
         with one park (the kernel waited; on an interrupt-driven ISA it halted the CPU)"
    );
}
