//! `librheo-echo` - a tiny native coreutil built on librheo (docs/LIBRHEO.md
//! Phase F), for the shell to spawn. Prints its arguments (argv[1..]) joined by
//! spaces, then a newline, and exits 0. It reads `argv` via `proc::args` - the
//! Phase F process-arguments path - proving a spawned cell sees its arguments.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use librheo::{println, proc};

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    let args = proc::args();
    let mut out = String::new();
    for (i, a) in args.iter().skip(1).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(a);
    }
    println!("{out}");
    0
}
