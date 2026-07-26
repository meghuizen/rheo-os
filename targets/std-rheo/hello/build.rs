// Link the std program at the rheo per-arch base with ENTRY(_start), reusing
// the userland linker scripts (docs/USERLAND.md M4). `_start` comes from the
// rheo-rt crt0 crate, pulled in by ENTRY(_start).
fn main() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg=-T{dir}/../../../userland/link/{arch}.ld");
}
