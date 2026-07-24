// Select the per-arch userland linker script (userland/link/<arch>.ld),
// mirroring the kernel's build.rs. Each sets the link base to a VA that is
// free in that ISA's cell root (docs/USERLAND.md): x86 at 1 GiB (below 2 GiB
// so the default small code model reaches it), arm/riscv at 4 GiB. The
// kernel's ELF loader reads e_entry, so the base is not hard-coded anywhere
// but here.

fn main() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let script = format!("{dir}/link/{arch}.ld");
    println!("cargo:rustc-link-arg=-T{script}");
    println!("cargo:rerun-if-changed={script}");
}
