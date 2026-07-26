//! librheo Phase J proof: **symmetric async IPC** (docs/LIBRHEO.md). ONE binary,
//! run as TWO cells that share a typed cross-cell queue pair and ping-pong N
//! typed messages over the async [`AsyncSender`]/[`AsyncReceiver`] - neither cell
//! busy-switching.
//!
//! - **producer** (client, role 0): for each round, `send`s a `Message` then
//!   `recv`s the consumer's ack. Its `recv` (of the ack) parks on the reactor.
//! - **consumer** (server, role 1): for each round, `recv`s a `Message`, checks
//!   it against the expected sequence, and `send`s an ack. Its `recv` parks on
//!   the reactor and is woken by the reactor's channel service - never a spin.
//!
//! Because each round's `recv` finds an empty ring (the peer is parked awaiting
//! this side), every receive genuinely parks and is resumed by the reactor's
//! cross-cell hand-off. The consumer asserts (a) it received the exact expected
//! sequence and (b) `rt::chan_wakeups() == N` - i.e. all N messages arrived via a
//! reactor park+wake, not a busy switch. On success it exits `0x42`. The test
//! kernel wires cell 0 = consumer (role 1), cell 1 = producer (role 0), and
//! starts the consumer; its exit is the asserted outcome.

#![no_std]
#![no_main]

extern crate alloc;

use librheo::ipc::{Channel, Message};
use librheo::{println, rt, sys};

/// Rounds of ping-pong (typed messages exchanged).
const N: u32 = 8;
/// The consumer's success sentinel.
const OK: u64 = 0x42;

/// The deterministic payload the producer sends for message `i` (non-trivial so
/// a dropped/reordered message would fail the exact-sequence check).
fn payload(i: u32) -> u32 {
    i.wrapping_mul(0x9E37_79B1) ^ 0x5A5A_1234
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    let ch = Channel::open().expect("librheo-ipc: no cross-cell channel wired");
    let is_producer = ch.is_client();
    let (tx, rx) = ch.split();

    if is_producer {
        rt::block_on(async move {
            for i in 0..N {
                tx.send(Message {
                    tag: i as u64,
                    val: payload(i),
                })
                .await;
                // Await the consumer's ack (this parks on the reactor).
                let _ack = rx.recv().await;
            }
        });
        // Unreached: the consumer exits first (it is the started/top cell).
        sys::exit(0)
    } else {
        rt::block_on(async move {
            let mut ok = true;
            for i in 0..N {
                let m = rx.recv().await;
                if m.tag != i as u64 || m.val != payload(i) {
                    ok = false;
                }
                // Ack the message so the producer advances to the next round.
                tx.send(Message {
                    tag: i as u64,
                    val: 1,
                })
                .await;
            }
            let wakeups = rt::chan_wakeups();
            if ok && wakeups == N as u64 {
                println!(
                    "librheo-ipc: consumer received {N} typed msgs, all reactor-parked \
                     ({wakeups} wakeups) - symmetric async IPC OK"
                );
                sys::exit(OK);
            }
            println!("librheo-ipc: FAIL (ok={ok} wakeups={wakeups}, expected {N})");
            sys::exit(1);
        });
        sys::exit(1)
    }
}
