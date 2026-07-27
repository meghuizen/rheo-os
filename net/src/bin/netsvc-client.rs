//! `netsvc-client` - the client half of the rheo-net Phase N4a service-cell proof
//! (docs/NETSTACK.md the service-cell section). Spawned by `netsvc-demo` (the
//! service), it talks over the channel it **inherited at spawn** - its own slot 0,
//! a private ring shared with exactly that service end.
//!
//! Its identity comes from `argv[1]`, which is also the service's channel slot for
//! it, so every client's requests and expected answers are **distinct**:
//!
//! - `ECHO(0xA5A50000 | id)` -> `echo_transform(val, id)`, which only this client's
//!   serving strand computes (a per-client keyed rotate+mix, predicted exactly here);
//! - `RESOLVE(name id)` -> that client's own catalogue name, answered from the
//!   service's network-free `net::dns` tiers (`10.1.1.1` / `10.2.2.2` / `10.3.3.3`);
//! - client 0 only: `RESOLVE(gateway)` -> the service's **bonus live** network op,
//!   accepted either way (an honest degradation, never asserted);
//! - `BYE` -> the count of requests the service served this client.
//!
//! Exits `id + 1` on full success (so the service proves *which* child it reaped),
//! or `0x70 + step` on the first mismatch.

#![no_std]
#![no_main]

extern crate alloc;

use librheo::sys;
use rheo_net::ip::Ipv4Addr;
use rheo_net::service::{Client, NAME_ALPHA, NAME_BETA, NAME_GAMMA, NAME_GATEWAY, echo_transform};

fn fail(id: u8, step: u64, msg: &str) -> ! {
    librheo::println!("netsvc-client {id}: FAIL at step {step}: {msg}");
    sys::exit(0x70 + step)
}

/// This client's catalogue name id + the IPv4 the service must answer with.
fn expect_name(id: u8) -> (u32, Ipv4Addr) {
    match id {
        0 => (NAME_ALPHA, Ipv4Addr::new(10, 1, 1, 1)),
        1 => (NAME_BETA, Ipv4Addr::new(10, 2, 2, 2)),
        _ => (NAME_GAMMA, Ipv4Addr::new(10, 3, 3, 3)),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let args = librheo::proc::args();
    let id: u8 = args
        .get(1)
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(u8::MAX);
    if id == u8::MAX {
        librheo::println!("netsvc-client: no client id in argv");
        sys::exit(0x6f);
    }

    librheo::rt::block_on(async move {
        let Some(mut client) = Client::open(id) else {
            fail(
                id,
                0,
                "no inherited channel - the service did not hand one over",
            )
        };

        // 1. Per-client echo: only this client's serving strand produces this value.
        let seed = 0xA5A5_0000u32 | id as u32;
        let want = echo_transform(seed, id);
        match client.echo(seed).await {
            Some(v) if v == want => {}
            Some(v) => {
                librheo::println!("netsvc-client {id}: echo {v:#010x} want {want:#010x}");
                fail(id, 1, "echo response wrong");
            }
            None => fail(id, 1, "echo reply tag mismatch"),
        }

        // 2. This client's own name: a distinct request, a distinct correct answer.
        let (name_id, want_ip) = expect_name(id);
        let want_word = u32::from_be_bytes(want_ip.octets());
        match client.resolve(name_id).await {
            Some(v) if v == want_word => {}
            Some(v) => {
                librheo::println!("netsvc-client {id}: resolve {v:#010x} want {want_word:#010x}");
                fail(id, 2, "resolve response wrong");
            }
            None => fail(id, 2, "resolve reply tag mismatch"),
        }

        // 3. Client 0 only: ask the service for the live gateway resolve. Accepted
        // either way - a headless/NIC-less run reports 0 and stays a pass.
        let mut expect_reqs = 3u32;
        if id == 0 {
            expect_reqs = 4;
            match client.resolve(NAME_GATEWAY).await {
                Some(0) => librheo::println!(
                    "netsvc-client 0: live gateway resolve unavailable (reported skipped)"
                ),
                Some(v) => {
                    let ip = v.to_be_bytes();
                    librheo::println!(
                        "netsvc-client 0: live gateway resolve -> {}.{}.{}.{}",
                        ip[0],
                        ip[1],
                        ip[2],
                        ip[3]
                    );
                }
                None => fail(id, 3, "live resolve reply tag mismatch"),
            }
        }

        // 4. Say goodbye; the reply is how many requests the service served us.
        match client.bye().await {
            Some(v) if v == expect_reqs => {}
            Some(v) => {
                librheo::println!("netsvc-client {id}: bye count {v} want {expect_reqs}");
                fail(id, 4, "bye count wrong");
            }
            None => fail(id, 4, "bye reply tag mismatch"),
        }

        librheo::println!("netsvc-client {id}: all responses correct and distinct");
        sys::exit(id as u64 + 1);
    });

    sys::exit(0x6e)
}
