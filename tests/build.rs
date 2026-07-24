// Test kernels link exactly like the kernel binary: same per-ISA linker
// script (kernel/link/<arch>.ld). Kept in sync with kernel/build.rs.

fn main() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let script = format!("{manifest_dir}/../kernel/link/{arch}.ld");
    println!("cargo:rustc-link-arg=-T{script}");
    println!("cargo:rerun-if-changed={script}");
}
