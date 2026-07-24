// A real-`std` program for the rheo-os target (docs/USERLAND.md M4). It uses
// the standard library - `String`, `Vec`, `format!`, iterators - not `core` +
// a shim. `restricted_std` is std's opt-in for an out-of-tree target_os; it
// compiles and links against the patched rust-src (targets/patch-std.py).
//
// Running it on the OS additionally needs a crt0 `_start` and the rheo stdio/
// process `sys` arms (the documented next step); this crate proves the target
// and std build.
#![feature(restricted_std)]

fn main() {
    let mut parts: Vec<String> = Vec::new();
    for i in 0..4 {
        parts.push(format!("part{i}"));
    }
    let joined = parts.join("-");
    println!("hello from std on rheo-os: {joined} ({} parts)", parts.len());
}
