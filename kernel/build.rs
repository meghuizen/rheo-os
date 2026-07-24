// Selects the linker script for the target ISA (kernel/link/<arch>.ld).
// The assembly sources in kernel/arch/ are pulled in with include_str! from
// the matching src/arch/<isa> module, so Cargo tracks them automatically.

fn main() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let script = format!("{manifest_dir}/link/{arch}.ld");
    println!("cargo:rustc-link-arg=-T{script}");
    println!("cargo:rerun-if-changed={script}");
}
