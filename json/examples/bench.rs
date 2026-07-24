//! Host throughput benchmark for rheo-json (docs/JSON.md, comparison/json/).
//! Builds a representative document, parses it repeatedly, and reports MB/s.
//! Run scalar:  cargo run -p rheo-json --example bench --release
//! Run SIMD:    cargo run -p rheo-json --example bench --release --features simd

use std::time::Instant;

use rheo_json::parse;

/// An array of `n` uniform objects - the kind of record stream JSON is
/// usually used for (strings, numbers, bools, small nested arrays, unicode).
fn make_doc(n: usize) -> String {
    let mut s = String::from("[");
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            r#"{{"id":{i},"name":"item-{i}","active":{},"score":{}.25,"tags":["alpha","beta","gamma"],"note":"café ☕"}}"#,
            i % 2 == 0,
            i % 1000
        ));
    }
    s.push(']');
    s
}

fn main() {
    let doc = make_doc(20_000);
    let bytes = doc.len();

    for _ in 0..3 {
        assert!(parse(&doc).is_ok(), "warmup parse failed");
    }

    let iters = 50;
    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..iters {
        let v = parse(&doc).expect("parse");
        acc += v.as_array().map(|a| a.len()).unwrap_or(0);
    }
    let elapsed = start.elapsed();

    let total = (bytes * iters) as f64;
    let mbps = total / elapsed.as_secs_f64() / 1.0e6;
    let path = if cfg!(feature = "simd") {
        "simd (sse2 string-scan)"
    } else {
        "scalar"
    };
    println!(
        "rheo-json [{path}]: {bytes} bytes x {iters} iters in {elapsed:?} -> {mbps:.0} MB/s (checksum {acc})"
    );
}
