//! `netsvc-demo` - the rheo-net Phase N4a proof cell: a **network service cell
//! serving three client cells concurrently** (docs/NETSTACK.md the service-cell
//! section).
//!
//! This cell is the service. The test kernel wires it **three** cross-cell channel
//! ends (slots 0-2, one ring region per client) and a cell-spawn capability. It:
//!
//! 1. binds all three ends to the reactor ([`rheo_net::service::Service::bind`]),
//!    seeding the network-free resolution tiers - two names in a `dns::HostsTable`,
//!    one in a `dns::Cache`;
//! 2. reads the NIC MAC (if a NIC exists) so the **bonus live** op can run;
//! 3. spawns `/bin/netsvc-client` **three times**, handing client k the service's
//!    channel slot k as that child's own slot 0;
//! 4. runs `serve()`: **one strand per client**, each parked on its own channel,
//!    answering requests and replying on that client's channel;
//! 5. reaps all three children and asserts the whole ledger.
//!
//! Asserted before exiting `0x42`: every client's per-client request count; the
//! **interleave witness** - the exact round-robin processing order `0,1,2,0,1,2,...`
//! (strand k reaches round r only after strands `0..k` did, so no strand monopolised
//! the vcore); the **in-flight witness** - all three clients' requests were queued at
//! the same instant; per-client reactor **park+wake** counts (a spin would leave them
//! 0); and each child's distinct exit code (so each was really reaped).
//!
//! Concurrent, not parallel: one CPU, cooperative (SMP is task #27).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;

use librheo::sys;
use rheo_net::ip::Ipv4Addr;
use rheo_net::service::{NAME_ALPHA, NAME_BETA, NAME_GAMMA, Service};

/// The client program the service spawns.
const CLIENT_PATH: &str = "/bin/netsvc-client";

/// Requests each client sends: client 0 does one extra (the bonus live op).
const REQS_C0: u32 = 4;
const REQS_OTHER: u32 = 3;

fn fail(step: u32, msg: &str) -> ! {
    librheo::println!("netsvc: FAIL at step {step}: {msg}");
    sys::exit(0x40 + step as u64)
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    // The two network-free resolution tiers: alpha/beta in the hosts table, gamma
    // in the TTL cache - so each client's distinct request is answered from real
    // `net::dns` machinery, deterministically and without a packet.
    let hosts = [
        ("alpha.rheo.test", Ipv4Addr::new(10, 1, 1, 1)),
        ("beta.rheo.test", Ipv4Addr::new(10, 2, 2, 2)),
    ];
    let cached = [("gamma.rheo.test", Ipv4Addr::new(10, 3, 3, 3))];

    let Some(mut service) = Service::bind(&hosts, &cached) else {
        fail(
            0,
            "no cross-cell channels wired - this cell is not a service",
        )
    };
    let nclients = service.clients();
    librheo::println!("netsvc: service bound {nclients} client channel end(s)");
    if nclients < 3 {
        fail(
            1,
            "need at least 3 client channel ends for the fan-out proof",
        );
    }

    // The bonus live path: a real NIC identity, if this kernel gave us one. Without
    // it the live op degrades to REPLY_NONE and the deterministic core is untouched.
    let gateway = Ipv4Addr::new(10, 0, 2, 2);
    librheo::rt::block_on(async move {
        match librheo::net::mac().await {
            Ok(mac) => {
                librheo::println!("netsvc: NIC MAC {mac:?} - bonus live op enabled");
                service.set_identity(mac, Ipv4Addr::new(10, 0, 2, 15), gateway);
            }
            Err(_) => librheo::println!("netsvc: no NIC - bonus live op will report skipped"),
        }

        // Spawn one client cell per channel slot. Each child inherits THAT slot as
        // its own slot 0 (docs/NETSTACK.md rheo-net N4a) - a private ring per client.
        let mut children = Vec::new();
        for k in 0..nclients {
            let id = format!("{k}");
            match service.spawn_client(k, CLIENT_PATH, &["netsvc-client", &id]) {
                Ok(c) => children.push(c),
                Err(_) => fail(2, "spawn_client failed"),
            }
        }
        librheo::println!("netsvc: spawned {} client cell(s)", children.len());

        // Serve them all concurrently: one strand per client.
        let report = service.serve().await;
        librheo::println!(
            "netsvc: served={:?} order={:?} max_in_flight={} wakeups={:?} live_ops={} live={:#010x}",
            report.served,
            report.order,
            report.max_in_flight,
            report.wakeups,
            report.live_ops,
            report.live_result
        );

        // ---- the ledger ----

        // (1) per-client request counts: client 0 does the extra live op.
        let want_served: Vec<u32> = (0..nclients)
            .map(|k| if k == 0 { REQS_C0 } else { REQS_OTHER })
            .collect();
        if report.served != want_served {
            fail(3, "per-client served counts wrong");
        }

        // (2) the interleave witness: the exact round-robin processing order.
        // Rounds A (echo), B (resolve), C (client 0's live resolve / the others'
        // bye) each touch all N clients in slot order; round D is client 0's bye.
        let mut want_order: Vec<u8> = Vec::new();
        for _ in 0..3 {
            for k in 0..nclients {
                want_order.push(k as u8);
            }
        }
        want_order.push(0);
        if report.order != want_order {
            fail(4, "processing order is not the round-robin interleave");
        }

        // (3) the in-flight witness: all N clients' requests queued at once.
        if report.max_in_flight < nclients {
            fail(5, "clients were never concurrently in flight");
        }

        // (4) every message arrived by a genuine reactor park + wake.
        if report.wakeups != want_served.iter().map(|&v| v as u64).collect::<Vec<u64>>() {
            fail(
                6,
                "per-client reactor wakeups do not match the messages served",
            );
        }

        // (5) reap every child; client k exits with k+1.
        for (k, child) in children.into_iter().enumerate() {
            let code = child.wait().await;
            librheo::println!("netsvc: client {k} exited {code}");
            if code != k as u64 + 1 {
                fail(7, "a client exited with the wrong code");
            }
        }

        // The bonus live op is reported, never asserted (headless-honest).
        if report.live_ops > 0 && report.live_result != 0 {
            let ip = report.live_result.to_be_bytes();
            librheo::println!(
                "netsvc: bonus live op OK - ARP resolved the gateway {}.{}.{}.{} on the real NIC",
                ip[0],
                ip[1],
                ip[2],
                ip[3]
            );
        } else {
            librheo::println!(
                "netsvc: bonus live op did not complete (no NIC, or no reply) - \
                 deterministic core unaffected"
            );
        }

        librheo::println!(
            "netsvc: {nclients} clients served concurrently over {nclients} channels, \
             distinct correct responses, round-robin interleave, all reaped"
        );
        sys::exit(0x42);
    });

    // block_on never returns here (every path above exits).
    let _ = (NAME_ALPHA, NAME_BETA, NAME_GAMMA);
    sys::exit(0x4f)
}
