//! `jsondemo` - parses an embedded JSON document with rheo-json, on the OS
//! (docs/JSON.md). Exercises the whole native stack under a real workload: the
//! libc heap/allocator (rheo-json builds `Vec`s for arrays), `println!`, and
//! the parser's zero-copy string borrows. Exits with a value derived from the
//! parse so the `jsonrun` test can check it deterministically.

#![no_std]
#![no_main]

extern crate alloc;

use rheo_json::parse;
use rheo_libc as libc;

const DOC: &str = r#"{
  "name": "rheo-os",
  "version": 4,
  "ok": true,
  "ratio": 0.5,
  "features": ["capabilities", "zero-copy", "runtime-dispatch"],
  "note": "café ☕"
}"#;

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    let v = match parse(DOC) {
        Ok(v) => v,
        Err(e) => {
            libc::eprintln!("jsondemo: parse error {:?} at byte {}", e.kind, e.offset);
            return 1;
        }
    };

    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("?");
    let version = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0);
    let features = v.get("features").and_then(|x| x.as_array()).unwrap_or(&[]);
    let note = v.get("note").and_then(|x| x.as_str()).unwrap_or("?");

    libc::println!(
        "jsondemo: name={name} version={version} ok={} ratio={}",
        v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false),
        v.get("ratio").and_then(|x| x.as_f64()).unwrap_or(0.0),
    );
    for (i, f) in features.iter().enumerate() {
        libc::println!("  feature[{i}] = {}", f.as_str().unwrap_or("?"));
    }
    libc::println!("  note = {note}");

    // Deterministic exit: version + number of features (4 + 3 = 7).
    version as i32 + features.len() as i32
}
