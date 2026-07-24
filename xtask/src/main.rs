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
const TEST_KERNELS: [&str; 18] = [
    "kernel",
    "cap-invariants",
    "queue-pipeline",
    "isolation-hw",
    "resources",
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
    "coreutils",
    "linuxrun",
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
        _ => &[],
    }
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

    /// ET_EXEC link base for the fixtures (docs/LINUX-COMPAT.md L2): a VA that
    /// is free in the cell's page-table root AND reachable by the ISA's default
    /// code model. x86/riscv use 1 GiB (riscv medlow reaches < 2 GiB; x86 small
    /// reaches < 2 GiB); ARM64's cell root maps kernel RAM at 1-2 GiB, so it
    /// uses 2 GiB (aarch64 small reaches < 4 GiB).
    fn linux_link_base(self) -> &'static str {
        match self {
            Arch::X86_64 | Arch::Riscv64 => "0x40000000",
            Arch::Aarch64 => "0x80000000",
        }
    }
}

/// Build the Linux-personality test fixtures from source (docs/LINUX-COMPAT.md
/// L2, fixture matrix in section 6): a static-glibc Rust `std` hello and a
/// static-glibc C hello, both ET_EXEC relinked to a per-arch free base so the
/// simple (no-relocation) loader path applies. No binaries live in git; these
/// are `include_bytes!`d by the `linuxrun` test kernel. Must run before the
/// kernels.
fn build_linux_fixtures(arch: Arch) -> bool {
    println!("[xtask] building Linux fixtures for {}", arch.name());
    let base = arch.linux_link_base();
    let cc = arch.linux_cc();

    // Rust std hello: static glibc, ET_EXEC (-no-pie + static relocation
    // model), relinked to the per-arch base. The cross gcc is the linker so
    // the right sysroot/crt objects are used; x86 forces bfd ld because
    // rust-lld rejects -Ttext-segment.
    let mut rustflags = format!(
        "-C target-feature=+crt-static -C relocation-model=static \
         -C linker={cc} -C link-arg=-no-pie -C link-arg=-Wl,-Ttext-segment={base}"
    );
    if arch == Arch::X86_64 {
        rustflags.push_str(" -C link-arg=-fuse-ld=bfd");
    }
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

    // C hello: gcc -static -no-pie, relinked to the same base.
    let out_dir = format!("tests/linux-fixtures/build/{}", arch.name());
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("[xtask] mkdir {out_dir}: {e}");
        return false;
    }
    let mut c = Command::new(cc);
    c.args([
        "-static",
        "-no-pie",
        &format!("-Wl,-Ttext-segment={base}"),
        "tests/linux-fixtures/hello.c",
        "-o",
        &format!("{out_dir}/chello"),
    ]);
    if !matches!(c.status().map(|s| s.success()), Ok(true)) {
        eprintln!("[xtask] C glibc fixture build failed for {}", arch.name());
        return false;
    }
    true
}

fn build(arch: Arch, release: bool) -> bool {
    if !build_userland(arch) {
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
