//! `librheo-pipesrc` - the **producer** stage of a cross-cell stdout pipeline
//! (docs/LIBRHEO.md Phase J). A spawned child that **inherits its parent's Phase
//! E channel** at spawn (the kernel maps the same frames into it, opposite role)
//! and streams a known byte sequence to the parent over the async `Sender` - its
//! "stdout", flowing cross-cell over the shared ring, not through the kernel.
//!
//! It sends `N` bytes (`'A'..`), each acknowledged by the consumer (a simple
//! back-pressure ping-pong so the parent reads every byte before the child
//! exits), then exits 0. `proc::spawn_piped` on the parent side wires this up.

#![no_std]
#![no_main]

extern crate alloc;

use librheo::ipc::{Channel, Message};
use librheo::rt;

/// Bytes streamed to the consumer (must match the parent's expectation).
const N: u32 = 12;

/// The known byte the producer emits at position `i` (`'A'` + i mod 26).
fn byte(i: u32) -> u32 {
    b'A' as u32 + (i % 26)
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    let ch = Channel::open().expect("librheo-pipesrc: no channel inherited from parent");
    let (tx, rx) = ch.split();
    rt::block_on(async move {
        for i in 0..N {
            tx.send(Message {
                tag: i as u64,
                val: byte(i),
            })
            .await;
            // Wait for the consumer's ack before the next byte (back-pressure),
            // so the parent has drained the stream before this child exits.
            let _ack = rx.recv().await;
        }
    });
    0
}
