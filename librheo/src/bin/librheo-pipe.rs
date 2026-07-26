//! `librheo-pipe` - the **consumer** stage of a cross-cell stdout pipeline
//! (docs/LIBRHEO.md Phase J). The orchestrator: it `spawn_piped`s
//! `/bin/pipesrc`, which **inherits this cell's Phase E channel** at spawn, then
//! reads the child's streamed byte output over the async `Receiver` (acking each
//! byte), reaps the child, and verifies it received exactly the child's output -
//! a genuine `a | b` where a spawned cell's "stdout" flows to another cell over
//! an `ipc` channel, not through the kernel.
//!
//! On success it prints the reconstructed stream and exits `0x42`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use librheo::ipc::Message;
use librheo::proc::Pipe;
use librheo::{println, proc, rt, sys};

/// Bytes expected from the producer (must match `librheo-pipesrc`).
const N: u32 = 12;
/// Success sentinel the test asserts.
const OK: u64 = 0x42;

fn byte(i: u32) -> u32 {
    b'A' as u32 + (i % 26)
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(async {
        let Pipe { child, tx, rx } = proc::spawn_piped("/bin/pipesrc", &["pipesrc"], &[])
            .expect("librheo-pipe: spawn_piped");

        let mut got: Vec<u8> = Vec::new();
        let mut ok = true;
        for i in 0..N {
            let m = rx.recv().await;
            if m.tag != i as u64 || m.val != byte(i) {
                ok = false;
            }
            got.push(m.val as u8);
            // Ack the byte (back-pressure) so the producer streams the next one.
            tx.send(Message {
                tag: i as u64,
                val: 1,
            })
            .await;
        }
        // Reap the child (it exits once its whole stream is acked).
        let code = child.wait().await;

        let stream = String::from_utf8_lossy(&got);
        let expect: String = (0..N).map(|i| byte(i) as u8 as char).collect();
        if ok && code == 0 && stream == expect {
            println!(
                "librheo-pipe: piped {N} bytes from spawned child \"{stream}\" \
                 - cross-cell stdout pipeline OK"
            );
            sys::exit(OK);
        }
        println!("librheo-pipe: FAIL (ok={ok} child_code={code} got=\"{stream}\")");
        sys::exit(1);
    });
    OK as i32
}
