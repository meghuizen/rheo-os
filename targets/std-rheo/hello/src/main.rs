// A real-`std` program for the rheo-os target (docs/USERLAND.md M4). It uses
// the standard library - `String`, `Vec`, `format!`, iterators, `println!` -
// not `core` + a shim. `restricted_std` is std's opt-in for an out-of-tree
// target_os. It links the rheo-rt crt0 (via ENTRY(_start)) and runs on the OS
// (the `stdrun` test loads it). Returns ExitCode 7 so the test can check it.
#![feature(restricted_std)]

use std::process::ExitCode;

// Force-link the crt0 (nothing here references it; ENTRY(_start) pulls _start).
extern crate rheo_rt as _;

fn main() -> ExitCode {
    let mut parts: Vec<String> = Vec::new();
    for i in 0..4 {
        parts.push(format!("part{i}"));
    }
    let joined = parts.join("-");
    println!(
        "hello from std on rheo-os: {joined} ({} parts)",
        parts.len()
    );
    ExitCode::from(7)
}
