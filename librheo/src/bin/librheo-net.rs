//! `librheo-net` - the Phase G proof program (docs/LIBRHEO.md, docs/NETWORKING.md):
//! **raw-frame networking over a real virtio-net NIC**. It asks the driver for
//! the NIC's MAC, builds and sends a broadcast **ARP request** for the SLIRP
//! gateway `10.0.2.2`, then polls `net::recv` for the **ARP reply** SLIRP sends
//! back - proving a genuine round trip through the virtqueues (TX out, RX in).
//!
//! It exits `0x42` only if it receives a well-formed ARP reply whose sender IP is
//! `10.0.2.2`; the `librheonet` test kernel asserts that code. A full IP/TCP
//! stack is a **service** (docs/NETWORKING.md 2), out of scope here - this proves
//! the NIC data path (the queue plumbing the kernel owns), not a transport.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicI32, Ordering};

use librheo::{net, println, rt};

/// Failure code (0 = success), set inside the `'static` async root.
static CODE: AtomicI32 = AtomicI32::new(0);
/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;

/// SLIRP's default guest address and gateway (QEMU `-netdev user`).
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
const ETHERTYPE_ARP: u16 = 0x0806;
/// Retry budget for the RX poll. Each `recv` is a doorbell (a VM exit that lets
/// QEMU's SLIRP backend run), so the reply lands within a handful of iterations;
/// the cap only guards against a wholly non-delivering backend.
const RX_RETRIES: u32 = 200_000;

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(run());
    let code = CODE.load(Ordering::Relaxed);
    if code != 0 {
        return code;
    }
    println!("librheo-net: ARP round trip over virtio-net OK");
    OK_CODE
}

fn fail(code: i32) {
    CODE.store(code, Ordering::Relaxed);
}

/// Build a 42-byte broadcast ARP request "who has `tpa`" from `mac`/`GUEST_IP`.
fn build_arp_request(mac: [u8; 6]) -> [u8; 42] {
    let mut f = [0u8; 42];
    // Ethernet header.
    f[0..6].copy_from_slice(&[0xff; 6]); // dst: broadcast
    f[6..12].copy_from_slice(&mac); // src: our MAC
    f[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    // ARP payload.
    f[14..16].copy_from_slice(&1u16.to_be_bytes()); // htype: Ethernet
    f[16..18].copy_from_slice(&0x0800u16.to_be_bytes()); // ptype: IPv4
    f[18] = 6; // hlen
    f[19] = 4; // plen
    f[20..22].copy_from_slice(&1u16.to_be_bytes()); // oper: request
    f[22..28].copy_from_slice(&mac); // sha: our MAC
    f[28..32].copy_from_slice(&GUEST_IP); // spa: our IP
    // f[32..38] tha: zero (unknown)
    f[38..42].copy_from_slice(&GATEWAY_IP); // tpa: gateway
    f
}

/// True if `f` is an ARP **reply** whose sender IP is the gateway.
fn is_gateway_arp_reply(f: &[u8]) -> bool {
    f.len() >= 42
        && f[12..14] == ETHERTYPE_ARP.to_be_bytes()
        && f[20..22] == 2u16.to_be_bytes() // oper: reply
        && f[28..32] == GATEWAY_IP // sender protocol address
}

async fn run() {
    // 1. Ask the driver for the NIC MAC (the ARP source; also what QEMU's rx
    //    filter accepts, so SLIRP's unicast reply reaches our RX queue).
    let mac = match net::mac().await {
        Ok(m) => m.0,
        Err(_) => {
            fail(10);
            return;
        }
    };

    // 2. Send the broadcast ARP request.
    let req = build_arp_request(mac);
    if net::send(&req).await.is_err() {
        fail(11);
        return;
    }

    // 3. Poll for the ARP reply (skipping any unrelated broadcast traffic).
    let mut buf = [0u8; 1600];
    for _ in 0..RX_RETRIES {
        match net::recv(&mut buf).await {
            Ok(0) => continue, // nothing yet - poll again
            Ok(n) if is_gateway_arp_reply(&buf[..n]) => {
                println!(
                    "librheo-net: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, ARP reply for 10.0.2.2 ({n} B)",
                    mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
                );
                return; // success - CODE stays 0
            }
            Ok(_) => continue, // some other frame - keep waiting
            Err(_) => {
                fail(12);
                return;
            }
        }
    }
    fail(13); // no ARP reply within the retry budget
}
