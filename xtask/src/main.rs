//! Build/run/test orchestration (`cargo xtask ...`), per DEVELOPMENT.md 2.
//! Wraps the awkward parts - build-std flags, linker script selection, QEMU
//! invocation - so day-to-day commands stay short:
//!
//!   cargo xtask build --arch riscv64
//!   cargo xtask run   --arch aarch64 [--bin bench-core]
//!   cargo xtask test  --arch all
//!   cargo xtask bench --arch all
//!
//! `test` boots every in-QEMU test kernel headless with a timeout and maps
//! the QEMU exit code back to pass/fail (DEVELOPMENT.md 6, 9). `bench`
//! boots the benchmark kernel under `-icount shift=0` so counters advance
//! deterministically with executed instructions - QEMU results are
//! instruction path lengths, never wall-clock claims (docs/TOOLING.md 4).
//! Serial output lands in target/qemu-<arch>-<bin>.log for CI artifacts.

use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

const TEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Every kernel binary booted by `cargo xtask test`, in order.
const TEST_KERNELS: [&str; 41] = [
    "kernel",
    "cap-invariants",
    "queue-pipeline",
    "isolation-hw",
    "resources",
    "pmem",
    "smp",
    "shell-smoke",
    "hwinfo",
    "rng",
    "runtime",
    "posix",
    "blockfs",
    "elfrun",
    "posixrun",
    "libcrun",
    "jsonrun",
    "stdrun",
    "librhearun",
    "librheodata",
    "librheocompute",
    "librheoterm",
    "librheowl",
    "coreutils",
    "linuxrun",
    "linuxtools",
    "linuxthreads",
    "linuxsig",
    "linuxproc",
    "linuxdyn",
    "librheoproc",
    "librheonet",
    "librheogpu",
    "librheoipc",
    "librheopipe",
    "netcore",
    "netl4",
    "netdns",
    "nettrace",
    "netlocal",
    "linuxunix",
];

/// Extra QEMU args for a given test kernel. `blockfs` needs a virtio-blk disk
/// (the ext4 fixture). arm/riscv `virt` present it over virtio-mmio; x86 q35
/// has no virtio-mmio, so there it is a virtio-*pci* device (disable-legacy=on
/// pins the modern-only layout the PCI-config-tunnel driver expects).
fn extra_qemu_args(arch: Arch, kernel: &str) -> &'static [&'static str] {
    match (kernel, arch) {
        ("blockfs", Arch::Riscv64 | Arch::Aarch64) => &[
            // Present the modern (version 2) virtio-mmio transport, which the
            // driver implements; QEMU defaults to the legacy version-1 layout.
            "-global",
            "virtio-mmio.force-legacy=false",
            "-drive",
            "file=tests/fixtures/ext4.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-device,drive=blk0",
        ],
        ("blockfs", Arch::X86_64) => &[
            "-drive",
            "file=tests/fixtures/ext4.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-pci,drive=blk0,disable-legacy=on",
        ],
        // librheodata (docs/LIBRHEO.md Phase B): the columnar dataset on a live
        // virtio-blk disk, generated fresh by `gen_columnar_dataset` into
        // target/ (gitignored, never committed). Same transports as blockfs.
        ("librheodata", Arch::Riscv64 | Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-drive",
            "file=target/librheodata.bin,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-device,drive=blk0",
        ],
        ("librheodata", Arch::X86_64) => &[
            "-drive",
            "file=target/librheodata.bin,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-pci,drive=blk0,disable-legacy=on",
        ],
        // librheonet (docs/LIBRHEO.md Phase G, docs/NETWORKING.md): a virtio-net
        // NIC on a SLIRP user netdev - deterministic + network-free. The guest
        // sends a broadcast ARP for the gateway 10.0.2.2; SLIRP answers with an
        // ARP reply the driver receives (a real, reproducible headless RX proof).
        // Same two transports as blockfs: virtio-mmio on arm/riscv, virtio-pci
        // on x86 (disable-legacy=on pins the modern layout the driver expects).
        ("librheonet", Arch::Riscv64 | Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-device,netdev=n0",
        ],
        ("librheonet", Arch::X86_64) => &[
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-pci,netdev=n0,disable-legacy=on",
        ],
        // netcore (docs/NETSTACK.md rheo-net Phase N1a): same SLIRP + virtio-net
        // setup as librheonet - the ARP round trip now runs through the `net`
        // crate's eth/arp layers. Deterministic + network-free (SLIRP answers the
        // broadcast ARP for the gateway 10.0.2.2). Same two transports.
        ("netcore", Arch::Riscv64 | Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-device,netdev=n0",
        ],
        ("netcore", Arch::X86_64) => &[
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-pci,netdev=n0,disable-legacy=on",
        ],
        // netl4 (docs/NETSTACK.md rheo-net Phase N1b): UDP + ICMP over the same
        // SLIRP + virtio-net setup as netcore. The guest sends a DNS query over
        // UDP to SLIRP's built-in responder (10.0.2.3:53) and an ICMP echo to the
        // gateway (10.0.2.2), both of which SLIRP answers deterministically and
        // network-free. Same two transports: virtio-mmio on arm/riscv, virtio-pci
        // on x86 (disable-legacy=on pins the modern layout the driver expects).
        ("netl4", Arch::Riscv64 | Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-device,netdev=n0",
        ],
        ("netl4", Arch::X86_64) => &[
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-pci,netdev=n0,disable-legacy=on",
        ],
        // netdns (docs/NETSTACK.md rheo-net Phase N1c): the caching DNS client
        // over the same SLIRP + virtio-net setup as netl4. The deterministic
        // codec/hosts/blocklist/cache checks are network-free; the bonus live
        // resolve queries SLIRP's built-in DNS responder (10.0.2.3:53). Same two
        // transports: virtio-mmio on arm/riscv, virtio-pci on x86.
        ("netdns", Arch::Riscv64 | Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-device,netdev=n0",
        ],
        ("netdns", Arch::X86_64) => &[
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-pci,netdev=n0,disable-legacy=on",
        ],
        // nettrace (docs/NETSTACK.md rheo-net Phase N1e): TTL/hop-limit +
        // traceroute over the same SLIRP + virtio-net setup as netdns. The core
        // proof (TTL/decrement/Time-Exceeded oracles + the traceroute state
        // machine fed synthetic responses) is network-free; the bonus live 1-hop
        // trace probes the gateway 10.0.2.2 (SLIRP is the destination at hop 1, no
        // intermediate hops). Same two transports: virtio-mmio on arm/riscv,
        // virtio-pci on x86.
        ("nettrace", Arch::Riscv64 | Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-device,netdev=n0",
        ],
        ("nettrace", Arch::X86_64) => &[
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-pci,netdev=n0,disable-legacy=on",
        ],
        // librheogpu (docs/LIBRHEO.md Phase H, docs/DISPLAY.md): a virtio-gpu 2D
        // device the driver brings up and presents to. QEMU runs headless
        // (`-display none` is added for every test kernel in `boot_expect_pass`),
        // so the proof is the genuine 2D command round-trip, not a visible pixel.
        // Same two transports as blockfs: virtio-mmio on arm/riscv, virtio-pci on
        // x86 (disable-legacy=on pins the modern layout the driver expects).
        ("librheogpu", Arch::Riscv64 | Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-device",
            "virtio-gpu-device",
        ],
        ("librheogpu", Arch::X86_64) => &["-device", "virtio-gpu-pci,disable-legacy=on"],
        // pmem (docs/MEMORY.md real-PMEM path): a real QEMU nvdimm whose
        // persistent span the kernel discovers via the ACPI NFIT and backs a
        // `MemKind::Pmem` grant with. Only x86-64 q35 exposes one here: the
        // appended `-machine nvdimm=on` + `-m ...,slots,maxmem` enable memory
        // devices, the memory-backend-file is the backing store (a zeroed 16 MiB
        // file generated into target/ by `gen_pmem_backing`, never committed),
        // and the nvdimm device attaches it. arm/riscv `virt` do not accept an
        // nvdimm (arm needs an ACPI GED device; riscv has no nvdimm support), so
        // the pmem kernel runs there with no extra args and skips-with-reason.
        ("pmem", Arch::X86_64) => &[
            "-machine",
            "nvdimm=on",
            "-m",
            "1G,slots=2,maxmem=4G",
            "-object",
            "memory-backend-file,id=pm0,share=on,mem-path=target/pmem.img,size=16M,pmem=on",
            "-device",
            "nvdimm,memdev=pm0,id=nv0",
        ],
        _ => &[],
    }
}

/// Path of the generated columnar dataset (gitignored).
const COLUMNAR_DATASET: &str = "target/librheodata.bin";

/// Generate the librheo Phase B columnar dataset (docs/LIBRHEO.md): a 16-byte
/// header `[magic u32][nrows u32][ncols u32][reserved u32]` then column A
/// (`col_a[i] = i`) then column B (`col_b[i] = i & 1`), each `nrows` little-
/// endian u32, padded to a 512-byte sector. Deterministic, so the scan's exact
/// aggregate is a closed form. Written to `target/` (never committed - the "no
/// artifacts staged" rule) before the `librheodata` kernel boots.
fn gen_columnar_dataset() -> bool {
    const NROWS: u32 = 65536;
    const NCOLS: u32 = 2;
    let mut out: Vec<u8> = Vec::with_capacity(16 + (NROWS * NCOLS * 4) as usize + 512);
    out.extend_from_slice(&0x314C_4F43u32.to_le_bytes()); // magic "COL1"
    out.extend_from_slice(&NROWS.to_le_bytes());
    out.extend_from_slice(&NCOLS.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved
    for i in 0..NROWS {
        out.extend_from_slice(&i.to_le_bytes()); // col_a[i] = i
    }
    for i in 0..NROWS {
        out.extend_from_slice(&(i & 1).to_le_bytes()); // col_b[i] = i & 1
    }
    while !out.len().is_multiple_of(512) {
        out.push(0);
    }
    if let Err(e) = std::fs::write(COLUMNAR_DATASET, &out) {
        eprintln!("[xtask] writing {COLUMNAR_DATASET}: {e}");
        return false;
    }
    true
}
/// Path of the generated nvdimm backing file (gitignored).
const PMEM_BACKING: &str = "target/pmem.img";

/// Generate a zeroed 16 MiB backing file for the `pmem` test kernel's QEMU
/// nvdimm (docs/MEMORY.md real-PMEM path). Written to `target/` before boot
/// (never committed - the "no artifacts staged" rule). A fresh nvdimm starts
/// zeroed, which is what the write/read round-trip proof expects.
fn gen_pmem_backing() -> bool {
    const SIZE: usize = 16 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(PMEM_BACKING)
        && meta.len() as usize == SIZE
    {
        return true; // already staged at the right size
    }
    if let Err(e) = std::fs::write(PMEM_BACKING, vec![0u8; SIZE]) {
        eprintln!("[xtask] writing {PMEM_BACKING}: {e}");
        return false;
    }
    true
}

const BENCH_KERNEL: &str = "bench-core";

#[derive(Clone, Copy, PartialEq)]
enum Arch {
    X86_64,
    Aarch64,
    Riscv64,
}

const ALL_ARCHES: [Arch; 3] = [Arch::X86_64, Arch::Aarch64, Arch::Riscv64];

impl Arch {
    fn name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
            Arch::Riscv64 => "riscv64",
        }
    }

    fn target(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64-unknown-none",
            Arch::Aarch64 => "aarch64-unknown-none-softfloat",
            Arch::Riscv64 => "riscv64gc-unknown-none-elf",
        }
    }

    fn qemu(self) -> &'static str {
        match self {
            Arch::X86_64 => "qemu-system-x86_64",
            Arch::Aarch64 => "qemu-system-aarch64",
            Arch::Riscv64 => "qemu-system-riscv64",
        }
    }

    /// Machine flags per DEVELOPMENT.md 5 (devices are added as the
    /// subsystems that need them come online, per BUILD-ORDER.md).
    fn qemu_machine_args(self) -> &'static [&'static str] {
        match self {
            Arch::X86_64 => &[
                "-machine",
                "q35,kernel-irqchip=split",
                "-cpu",
                "max",
                "-m",
                "1G",
                "-smp",
                "4",
                "-device",
                "isa-debug-exit,iobase=0xf4,iosize=0x04",
            ],
            Arch::Aarch64 => &[
                "-machine",
                // highmem-ecam=off pins the PCIe ECAM to the low window
                // (0x3f00_0000) the kernel identity-maps and discovers.
                "virt,gic-version=3,highmem-ecam=off",
                "-cpu",
                "max",
                "-m",
                "1G",
                "-smp",
                "4",
                "-semihosting-config",
                "enable=on,target=native",
            ],
            Arch::Riscv64 => &[
                "-machine",
                "virt,aia=aplic-imsic",
                "-cpu",
                "max",
                "-m",
                "1G",
                "-smp",
                "4",
                "-bios",
                "default",
            ],
        }
    }

    /// The QEMU process exit code that means "kernel reported success".
    /// x86_64 isa-debug-exit turns our 0x10 into (0x10 << 1) | 1 = 33;
    /// semihosting (ARM64) and sifive_test (RISC-V) exit with 0.
    fn success_exit_code(self) -> i32 {
        match self {
            Arch::X86_64 => 33,
            Arch::Aarch64 | Arch::Riscv64 => 0,
        }
    }

    fn kernel_path(self, release: bool, bin: &str) -> PathBuf {
        let profile = if release { "release" } else { "debug" };
        PathBuf::from(format!("target/{}/{profile}/{bin}", self.target()))
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        print_usage();
        return ExitCode::FAILURE;
    };

    let mut arches = vec![Arch::X86_64];
    let mut release = false;
    let mut bin = String::from("kernel");
    let mut iter = args[1..].iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--arch" => {
                let Some(value) = iter.next() else {
                    eprintln!("error: --arch needs a value");
                    return ExitCode::FAILURE;
                };
                arches = match value.as_str() {
                    "x86_64" => vec![Arch::X86_64],
                    "aarch64" => vec![Arch::Aarch64],
                    "riscv64" => vec![Arch::Riscv64],
                    "all" => ALL_ARCHES.to_vec(),
                    other => {
                        eprintln!("error: unknown arch '{other}' (x86_64, aarch64, riscv64, all)");
                        return ExitCode::FAILURE;
                    }
                };
            }
            "--bin" => {
                let Some(value) = iter.next() else {
                    eprintln!("error: --bin needs a value");
                    return ExitCode::FAILURE;
                };
                bin = value.clone();
            }
            "--release" => release = true,
            other => {
                eprintln!("error: unknown flag '{other}'");
                print_usage();
                return ExitCode::FAILURE;
            }
        }
    }

    let ok = match command {
        "build" => arches.iter().all(|&a| build(a, release)),
        "run" => {
            if arches.len() != 1 {
                eprintln!("error: 'run' takes exactly one --arch");
                return ExitCode::FAILURE;
            }
            build(arches[0], release) && run_interactive(arches[0], release, &bin)
        }
        // Release always: U-mode programs must contain no out-of-line
        // calls (debug builds insert pointer-check panics that land in
        // unmapped kernel .text), and optimized path lengths are the
        // system's real numbers anyway.
        "test" => arches.iter().all(|&a| {
            build(a, true)
                && TEST_KERNELS
                    .iter()
                    .all(|kernel| boot_expect_pass(a, true, kernel, extra_qemu_args(a, kernel)))
        }),
        // Benchmarks always run the release build: instruction path
        // lengths of an unoptimized kernel are not the system's numbers.
        "bench" => arches.iter().all(|&a| build(a, true) && bench(a, true)),
        // Patch the toolchain's vendored rust-src to add `target_os = "rheo"`
        // so `std` can be built for the rheo-os target (docs/USERLAND.md M4).
        // Idempotent; run once per toolchain before building std programs.
        "std-patch" => std_patch(),
        _ => {
            print_usage();
            false
        }
    };

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn print_usage() {
    eprintln!(
        "usage: cargo xtask <build|run|test|bench|std-patch> \
         [--arch x86_64|aarch64|riscv64|all] [--bin <kernel>] [--release]"
    );
}

/// Apply the rheo-os std patch to the active toolchain's rust-src
/// (targets/patch-std.py). Idempotent.
fn std_patch() -> bool {
    println!("[xtask] patching rust-src std for target_os = \"rheo\"");
    matches!(
        Command::new("python3")
            .args(["targets/patch-std.py"])
            .status()
            .map(|s| s.success()),
        Ok(true)
    )
}

/// Bare-metal build with build-std (DEVELOPMENT.md 3): the kernel and
/// every in-QEMU test kernel.
/// Build the userspace programs (docs/USERLAND.md) that the `elfrun`,
/// `posixrun`, and `libcrun` tests embed: the raw `userland` programs and the
/// libc-linked programs in `rheo-libc`. Always release (a separate artifact
/// from the kernel profile). The link base is per-arch (userland/link/<arch>.ld)
/// so the default code model reaches it - no RUSTFLAGS override needed.
/// `alloc` is in build-std so libc-linked programs can use the heap. Must run
/// before the test kernels, which `include_bytes!` the built ELFs.
fn build_userland(arch: Arch) -> bool {
    println!("[xtask] building userspace for {}", arch.name());
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "-p",
        "userland",
        "-p",
        "rheo-libc",
        "-p",
        "librheo",
        "-p",
        "rheo-net",
        "--release",
        "--target",
        arch.target(),
        "-Zbuild-std=core,alloc,compiler_builtins",
        "-Zbuild-std-features=compiler-builtins-mem",
    ]);
    matches!(cmd.status().map(|s| s.success()), Ok(true))
}

/// Rebuild only the `librheo-embed` bin with `--no-default-features` (the
/// spine: cap/rt/mem/sys - no term/io/proc/rng/...) so it links the minimal
/// surface (docs/LIBRHEO.md Phase F embedded proof). Overwrites the full-feature
/// build `build_userland` produced at the same path; the `librheoproc` kernel
/// embeds this minimal artifact and asserts it is substantially smaller.
fn build_librheo_embedded(arch: Arch) -> bool {
    println!(
        "[xtask] building librheo-embed (no-default-features) for {}",
        arch.name()
    );
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "-p",
        "librheo",
        "--bin",
        "librheo-embed",
        "--no-default-features",
        "--release",
        "--target",
        arch.target(),
        "-Zbuild-std=core,alloc,compiler_builtins",
        "-Zbuild-std-features=compiler-builtins-mem",
    ]);
    matches!(cmd.status().map(|s| s.success()), Ok(true))
}

/// Build a real-std program (`manifest`) for the rheo-os target of `arch`, so
/// a test can embed the ELF (docs/USERLAND.md M4/M5). Applies the rust-src std
/// patch first (idempotent) and uses `-Zbuild-std=std` against the custom JSON
/// target. Used for the `stdrun` proof program and the `coreutils` cell.
fn build_std_program(arch: Arch, manifest: &str, label: &str) -> bool {
    if !std_patch() {
        return false;
    }
    println!("[xtask] building std program '{label}' for {}", arch.name());
    let target = format!("targets/rheo_os-{}.json", arch.name());
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "--manifest-path",
        manifest,
        "--release",
        "--target",
        &target,
        "-Zbuild-std=std,panic_abort",
        "-Zbuild-std-features=compiler-builtins-mem",
        "-Zjson-target-spec",
    ]);
    matches!(cmd.status().map(|s| s.success()), Ok(true))
}

impl Arch {
    /// The `*-unknown-linux-gnu` rustup target for the Linux personality
    /// fixtures (docs/LINUX-COMPAT.md L2).
    fn linux_gnu_target(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64-unknown-linux-gnu",
            Arch::Aarch64 => "aarch64-unknown-linux-gnu",
            Arch::Riscv64 => "riscv64gc-unknown-linux-gnu",
        }
    }

    /// The gcc that links a static-glibc binary for this ISA.
    fn linux_cc(self) -> &'static str {
        match self {
            Arch::X86_64 => "gcc",
            Arch::Aarch64 => "aarch64-linux-gnu-gcc",
            Arch::Riscv64 => "riscv64-linux-gnu-gcc",
        }
    }

    /// The toolchain runtime dynamic-linker + libc for the L7 dynamic fixture
    /// (docs/LINUX-COMPAT.md L7): `(ld.so source path, libc.so.6 source path)`.
    /// These live in the cross toolchain sysroots (host multiarch for x86-64)
    /// and are copied into the gitignored fixture build dir at build time - no
    /// `.so` blob is committed. If a path is missing for an ISA, that ISA's
    /// dynamic fixture is skipped-with-reason (the static L2-L6 coverage stays).
    fn dyn_runtime_libs(self) -> (&'static str, &'static str) {
        match self {
            Arch::X86_64 => (
                "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
                "/lib/x86_64-linux-gnu/libc.so.6",
            ),
            Arch::Aarch64 => (
                "/usr/aarch64-linux-gnu/lib/ld-linux-aarch64.so.1",
                "/usr/aarch64-linux-gnu/lib/libc.so.6",
            ),
            Arch::Riscv64 => (
                "/usr/riscv64-linux-gnu/lib/ld-linux-riscv64-lp64d.so.1",
                "/usr/riscv64-linux-gnu/lib/libc.so.6",
            ),
        }
    }
}

/// Build the Linux-personality test fixtures from source (docs/LINUX-COMPAT.md
/// L2, fixture matrix in section 6): a static-glibc Rust `std` hello and a
/// static-glibc C hello. No binaries live in git; these are `include_bytes!`d
/// by the `linuxrun` test kernel. Must run before the kernels.
fn build_linux_fixtures(arch: Arch) -> bool {
    println!("[xtask] building Linux fixtures for {}", arch.name());
    let cc = arch.linux_cc();

    // Rust std hello: static glibc, ET_EXEC (-no-pie + static relocation
    // model). All three ISAs are now higher-half kernels (docs/MEMORY.md), so
    // the whole low half is free and every fixture keeps glibc's stock ET_EXEC
    // base (x86/arm 0x400000, riscv 0x10000) - no relink - which proves a stock
    // binary loads unmodified (docs/LINUX-COMPAT.md L2). The cross gcc is the
    // linker so the right sysroot/crt objects are used.
    let rustflags = format!(
        "-C target-feature=+crt-static -C relocation-model=static \
         -C linker={cc} -C link-arg=-no-pie"
    );
    let mut rust = Command::new("cargo");
    rust.args([
        "build",
        "--manifest-path",
        "tests/linux-fixtures/rusthello/Cargo.toml",
        "--release",
        "--target",
        arch.linux_gnu_target(),
    ]);
    rust.env("RUSTFLAGS", &rustflags);
    if !matches!(rust.status().map(|s| s.success()), Ok(true)) {
        eprintln!(
            "[xtask] Rust glibc fixture build failed for {}",
            arch.name()
        );
        return false;
    }

    // Multi-threaded Rust std fixture (L4, docs/LINUX-COMPAT.md): same
    // static-glibc ET_EXEC recipe, exercising clone/futex/TLS/join.
    let mut threads = Command::new("cargo");
    threads.args([
        "build",
        "--manifest-path",
        "tests/linux-fixtures/rustthreads/Cargo.toml",
        "--release",
        "--target",
        arch.linux_gnu_target(),
    ]);
    threads.env("RUSTFLAGS", &rustflags);
    if !matches!(threads.status().map(|s| s.success()), Ok(true)) {
        eprintln!(
            "[xtask] Rust threads fixture build failed for {}",
            arch.name()
        );
        return false;
    }

    // C hello: gcc -static -no-pie, stock ET_EXEC base (no relink).
    let out_dir = format!("tests/linux-fixtures/build/{}", arch.name());
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("[xtask] mkdir {out_dir}: {e}");
        return false;
    }
    let mut c = Command::new(cc);
    c.arg("-static").arg("-no-pie");
    c.args([
        "tests/linux-fixtures/hello.c",
        "-o",
        &format!("{out_dir}/chello"),
    ]);
    if !matches!(c.status().map(|s| s.success()), Ok(true)) {
        eprintln!("[xtask] C glibc fixture build failed for {}", arch.name());
        return false;
    }

    // Signal fixtures (L5) + process fixtures (L6): same static-glibc ET_EXEC
    // recipe as chello. `procdemo` (pipe2+fork+dup2+execve+wait4) and `cecho`
    // (its execve target) are the `linuxproc` proof (docs/LINUX-COMPAT.md L6).
    for (src, bin) in [
        ("sig_raise.c", "sig_raise"),
        ("sig_segv.c", "sig_segv"),
        ("sig_dfl.c", "sig_dfl"),
        ("procdemo.c", "procdemo"),
        ("cecho.c", "cecho"),
        ("rsh.c", "rsh"),
        // AF_UNIX (L8, docs/LINUX-COMPAT.md): socketpair+fork + bind/listen/
        // connect/accept, the `linuxunix` proof.
        ("af_unix.c", "af_unix"),
    ] {
        let mut sc = Command::new(cc);
        sc.arg("-static").arg("-no-pie");
        sc.args([
            &format!("tests/linux-fixtures/{src}"),
            "-o",
            &format!("{out_dir}/{bin}"),
        ]);
        if !matches!(sc.status().map(|s| s.success()), Ok(true)) {
            eprintln!(
                "[xtask] signal fixture {bin} build failed for {}",
                arch.name()
            );
            return false;
        }
    }

    if !build_dyn_fixture(arch, cc, &out_dir) {
        return false;
    }

    build_coreutils_fixture(arch)
}

/// Build the L7 **dynamically-linked** glibc fixture (docs/LINUX-COMPAT.md L7):
/// a stock ET_DYN/PIE C hello (no `-static`/`-no-pie`) plus the toolchain's real
/// `ld-linux` + `libc.so.6`, copied into the gitignored build dir so the
/// `linuxdyn` test can `include_bytes!` them and seed the cell's `/lib`. The
/// runtime `.so`s are never committed. If they cannot be located for this ISA,
/// the fixture is **skipped-with-reason**: a 1-byte placeholder `ld.so` is
/// written so the test still compiles, and it detects the placeholder and skips
/// (the static L2-L6 coverage remains). Returns false only on a hard build
/// error (the C compile itself failing), never on a missing runtime lib.
fn build_dyn_fixture(arch: Arch, cc: &str, out_dir: &str) -> bool {
    // Stock dynamic PIE: no -static, no -no-pie (ET_DYN/PIE is gcc's default).
    let mut c = Command::new(cc);
    c.args([
        "tests/linux-fixtures/dhello.c",
        "-o",
        &format!("{out_dir}/dhello"),
    ]);
    if !matches!(c.status().map(|s| s.success()), Ok(true)) {
        eprintln!("[xtask] dynamic C fixture build failed for {}", arch.name());
        return false;
    }

    // Copy the real ld.so + libc.so.6 out of the toolchain, or skip-with-reason.
    let (ld_src, libc_src) = arch.dyn_runtime_libs();
    let ld_dst = format!("{out_dir}/ld.so");
    let libc_dst = format!("{out_dir}/libc.so.6");
    let copied =
        std::fs::copy(ld_src, &ld_dst).is_ok() && std::fs::copy(libc_src, &libc_dst).is_ok();
    if copied {
        println!(
            "[xtask] copied dynamic runtime ({ld_src}, {libc_src}) for {}",
            arch.name()
        );
    } else {
        eprintln!(
            "[xtask] SKIP dynamic fixture for {}: runtime ld.so/libc not found \
             ({ld_src}); linuxdyn will skip this ISA (static coverage stays)",
            arch.name()
        );
        // 1-byte placeholders so the test still compiles + detects the skip.
        let _ = std::fs::write(&ld_dst, [0u8]);
        let _ = std::fs::write(&libc_dst, [0u8]);
    }
    true
}

/// Pinned upstream uutils/coreutils crate for the L3 Linux-personality fixture
/// (docs/LINUX-COMPAT.md 6). Bump deliberately, in the doc's fixture matrix too.
const COREUTILS_VERSION: &str = "0.0.29";
/// The subset of utilities the `linuxtools` test exercises (each is a crate
/// feature that pulls in the matching `uu_*` dependency). Kept small so the
/// static-glibc multicall binary and its build stay lean.
const COREUTILS_FEATURES: &str = "true,false,echo,cat,wc,head,seq,ls,sort,basename,dirname,pwd";

/// Build the **unpatched upstream uutils/coreutils** multicall binary from
/// crates.io (pinned `COREUTILS_VERSION`), static-glibc ET_EXEC for `arch`, for
/// the `linuxtools` test (docs/LINUX-COMPAT.md L3). Built with `cargo install`
/// (which compiles the crate's own multicall `coreutils` bin from registry
/// source) into the gitignored `build/<arch>/cu` root; no binary lives in git.
/// Existence-cached: a rebuild is skipped if the binary is already present, so
/// repeated `cargo xtask test` runs stay fast (delete `build/<arch>/cu` to force
/// a rebuild after bumping the version or feature set).
fn build_coreutils_fixture(arch: Arch) -> bool {
    let cc = arch.linux_cc();
    let root = format!("tests/linux-fixtures/build/{}/cu", arch.name());
    let bin = format!("{root}/bin/coreutils");
    if std::path::Path::new(&bin).exists() {
        println!("[xtask] coreutils fixture cached for {}", arch.name());
        return true;
    }
    println!(
        "[xtask] building upstream uutils/coreutils {COREUTILS_VERSION} for {}",
        arch.name()
    );
    let rustflags = format!(
        "-C target-feature=+crt-static -C relocation-model=static \
         -C linker={cc} -C link-arg=-no-pie"
    );
    let mut cmd = Command::new("cargo");
    cmd.args([
        "install",
        "coreutils",
        "--version",
        &format!("={COREUTILS_VERSION}"),
        "--force",
        // Not `--locked`: 0.0.29's bundled Cargo.lock pins an ancient rustix
        // that no longer builds on current nightly; fresh resolution of the
        // (semver-compatible) transitive deps is required. The fixture *crate*
        // is still version-pinned, which is what the reproducibility contract
        // (docs/LINUX-COMPAT.md 6) asks for.
        "--no-default-features",
        "--features",
        COREUTILS_FEATURES,
        "--target",
        arch.linux_gnu_target(),
        "--target-dir",
        &format!("{root}-target"),
        "--root",
        &root,
    ]);
    cmd.env("RUSTFLAGS", &rustflags);
    if !matches!(cmd.status().map(|s| s.success()), Ok(true)) {
        eprintln!(
            "[xtask] coreutils fixture build failed for {} (crates.io fetch + \
             cross static-glibc link required)",
            arch.name()
        );
        return false;
    }
    true
}

fn build(arch: Arch, release: bool) -> bool {
    if !build_userland(arch) {
        return false;
    }
    // The embedded proof (docs/LIBRHEO.md Phase F): rebuild `librheo-embed` with
    // NO default features (the spine only) so it links the minimal surface and is
    // substantially smaller. Must run AFTER `build_userland` (which builds every
    // librheo bin with the full feature set) so this minimal build is the one the
    // `librheoproc` kernel embeds.
    if !build_librheo_embedded(arch) {
        return false;
    }
    // The librheodata (Phase B) dataset the test kernel reads off the live disk.
    if !gen_columnar_dataset() {
        return false;
    }
    // The pmem (real-PMEM path) nvdimm backing file.
    if !gen_pmem_backing() {
        return false;
    }
    if !build_linux_fixtures(arch) {
        return false;
    }
    if !build_std_program(arch, "targets/std-rheo/hello/Cargo.toml", "hello") {
        return false;
    }
    if !build_std_program(arch, "targets/std-rheo/coreutils/Cargo.toml", "coreutils") {
        return false;
    }
    println!("[xtask] building kernels for {}", arch.name());
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "-p",
        "kernel",
        "-p",
        "qemu-tests",
        "--target",
        arch.target(),
        "-Zbuild-std=core,alloc,compiler_builtins",
        "-Zbuild-std-features=compiler-builtins-mem",
    ]);
    if release {
        cmd.arg("--release");
    }
    if arch == Arch::Aarch64 {
        // Higher-half kernel (docs/MEMORY.md): the kernel .text/.data run at
        // high (TTBR1) VAs while the low `.user` window is per-cell in TTBR0.
        // Kernel code materializes the addresses of low `.user` symbols, which
        // is beyond the small code model's +-4 GiB adrp reach, so the aarch64
        // kernel is built with the large code model (absolute movz/movk). Only
        // this package needs it; the userland/std fixtures link low and small.
        cmd.env("RUSTFLAGS", "-C code-model=large");
    }
    if arch == Arch::X86_64 {
        // Higher-half kernel (docs/MEMORY.md): the kernel + `.user` window run
        // at top-2 GiB VAs. Kernel code references those symbols with signed
        // 32-bit relocations, which reach the top 2 GiB only under the "kernel"
        // code model - the small model (unsigned 32-bit, .cargo/config.toml)
        // cannot address them. Static relocation is kept (nothing relocates the
        // image). The env RUSTFLAGS overrides the config's [target] rustflags
        // for this package only; the low-linked userland/std fixtures keep the
        // config's small model.
        cmd.env(
            "RUSTFLAGS",
            "-C relocation-model=static -C code-model=kernel",
        );
    }
    if !matches!(cmd.status().map(|s| s.success()), Ok(true)) {
        return false;
    }
    build_smp_kernel(arch, release)
}

/// Per-arch RUSTFLAGS for the higher-half kernel build (see `build`). Applied to
/// both the main kernel build and the separate `smp` kernel build so they use
/// the same code/relocation model.
fn kernel_rustflags(arch: Arch) -> Option<&'static str> {
    match arch {
        Arch::Aarch64 => Some("-C code-model=large"),
        Arch::X86_64 => Some("-C relocation-model=static -C code-model=kernel"),
        Arch::Riscv64 => None,
    }
}

/// Build the `smp` test kernel in its own cargo invocation with `kernel/smp`
/// enabled (docs/SMP.md, task #27). SMP is feature-gated OFF by default so the
/// other 31 test kernels link a byte-identical `kernel` lib (adding the module
/// perturbs LLVM codegen-unit hashing, which must not reach non-SMP kernels).
/// The `smp` bin has `required-features = ["smp"]`, so the main `-p qemu-tests`
/// build skips it; this builds just that bin with the feature on. Same target +
/// RUSTFLAGS as the main kernel build.
fn build_smp_kernel(arch: Arch, release: bool) -> bool {
    println!(
        "[xtask] building the smp test kernel (kernel/smp feature) for {}",
        arch.name()
    );
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "-p",
        "qemu-tests",
        "--bin",
        "smp",
        "--features",
        "smp",
        "--target",
        arch.target(),
        "-Zbuild-std=core,alloc,compiler_builtins",
        "-Zbuild-std-features=compiler-builtins-mem",
    ]);
    if release {
        cmd.arg("--release");
    }
    if let Some(flags) = kernel_rustflags(arch) {
        cmd.env("RUSTFLAGS", flags);
    }
    matches!(cmd.status().map(|s| s.success()), Ok(true))
}

fn qemu_command(arch: Arch, release: bool, bin: &str) -> Command {
    let mut cmd = Command::new(arch.qemu());
    cmd.args(arch.qemu_machine_args());
    cmd.arg("-kernel").arg(arch.kernel_path(release, bin));
    cmd.args(["-no-reboot", "-nodefaults"]);
    cmd
}

/// Interactive run: serial console + QEMU monitor multiplexed on the
/// terminal (Ctrl-A C toggles, Ctrl-A X quits).
fn run_interactive(arch: Arch, release: bool, bin: &str) -> bool {
    let mut cmd = qemu_command(arch, release, bin);
    cmd.args(["-serial", "mon:stdio", "-display", "none"]);
    println!(
        "[xtask] running {bin} on {} in QEMU (Ctrl-A X to quit)",
        arch.name()
    );
    matches!(cmd.status().map(|s| s.success()), Ok(true))
}

/// The benchmark run: deterministic instruction counting via icount.
/// Results are instruction path lengths - comparable across runs and
/// against other systems measured the same way, but not wall-clock.
fn bench(arch: Arch, release: bool) -> bool {
    boot_expect_pass(
        arch,
        release,
        BENCH_KERNEL,
        &["-icount", "shift=0,align=off,sleep=off"],
    )
}

/// Headless boot of one kernel binary: capture serial output, enforce a
/// timeout, and map the QEMU exit code back to pass/fail.
fn boot_expect_pass(arch: Arch, release: bool, bin: &str, extra_args: &[&str]) -> bool {
    let log_path = PathBuf::from(format!("target/qemu-{}-{bin}.log", arch.name()));
    let mut cmd = qemu_command(arch, release, bin);
    cmd.args(["-display", "none", "-monitor", "none"]);
    cmd.args(extra_args);
    cmd.arg("-serial")
        .arg(format!("file:{}", log_path.display()));
    cmd.stdin(Stdio::null());

    println!("[xtask] booting {bin} on {} in QEMU", arch.name());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("[xtask] failed to start {}: {err}", arch.qemu());
            return false;
        }
    };

    // Poll for exit with a deadline; kill on timeout so CI never hangs.
    let deadline = Instant::now() + TEST_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(err) => {
                eprintln!("[xtask] wait failed: {err}");
                let _ = child.kill();
                break None;
            }
        }
    };

    if let Ok(serial) = std::fs::read_to_string(&log_path) {
        print!("{serial}");
    }

    match status {
        None => {
            eprintln!(
                "[xtask] {} {bin}: TIMEOUT after {}s",
                arch.name(),
                TEST_TIMEOUT.as_secs()
            );
            false
        }
        Some(status) => {
            let code = status.code().unwrap_or(-1);
            if code == arch.success_exit_code() {
                println!("[xtask] {} {bin}: PASS", arch.name());
                true
            } else {
                eprintln!(
                    "[xtask] {} {bin}: FAIL (qemu exit code {code}, expected {})",
                    arch.name(),
                    arch.success_exit_code()
                );
                false
            }
        }
    }
}
