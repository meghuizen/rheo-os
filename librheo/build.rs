// librheo programs are loaded ELF cells with the same per-arch memory layout
// as the raw userland programs, so they share the linker scripts
// (userland/link/<arch>.ld): ENTRY(_start) + the per-ISA link base
// (docs/USERLAND.md, docs/LIBRHEO.md).

fn main() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let script = format!("{dir}/../userland/link/{arch}.ld");
    println!("cargo:rustc-link-arg=-T{script}");
    println!("cargo:rerun-if-changed={script}");
}
