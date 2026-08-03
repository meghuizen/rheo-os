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
const TEST_KERNELS: [&str; 71] = [
    "kernel",
    "substrate",
    "observe",
    "cap-invariants",
    "queue-pipeline",
    "isolation-hw",
    "security",
    "resources",
    "pmem",
    "numa",
    "smp",
    "linuxsmp",
    "linuxbunsmp",
    "linuxnodesmp",
    "linuxclaudesmp",
    "shell-smoke",
    "hwinfo",
    "rng",
    "runtime",
    "posix",
    "blockfs",
    "nvmefs",
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
    "linuxnode",
    "linuxbun",
    "linuxclaude",
    "librheoproc",
    "librheonet",
    "netwait",
    "schedidle",
    "preempt",
    "linuxpoll",
    "librheogpu",
    "librheoipc",
    "librheopipe",
    "netcore",
    "netl4",
    "netdns",
    "nettrace",
    "netlocal",
    "nettcp",
    "nettcpcc",
    "netsmoltcp",
    "netcrypto",
    "nettls",
    "linuxunix",
    "linuxinet",
    "linuxnet",
    "netservice",
    "nethttp",
    "nethostcfg",
    "gpuhw",
    "iommu",
    "librheotile",
    "librheotilebattle",
];

/// Extra QEMU args for a given test kernel. `blockfs` needs a virtio-blk disk
/// (the ext4 fixture). arm/riscv `virt` present it over virtio-mmio; x86 q35
/// has no virtio-mmio, so there it is a virtio-*pci* device (disable-legacy=on
/// pins the modern-only layout the PCI-config-tunnel driver expects).
///
/// The `gpuhw` arms additionally **drop any GPU device model this QEMU build does
/// not have** - see [`gpu_device_args`].
fn extra_qemu_args(arch: Arch, kernel: &str) -> Vec<String> {
    if kernel == "gpuhw" {
        return gpu_device_args(arch);
    }
    let mut args: Vec<String> = fixed_qemu_args(arch, kernel)
        .iter()
        .map(|s| s.to_string())
        .collect();
    if kernel == "rng" {
        args.extend(tpm_device_args(arch));
        args.extend(hid_device_args(arch));
    }
    args
}

/// A **HID keyboard** for the `rng` kernel, plus the QMP socket used to press
/// keys on it (docs/TIME-IDENTITY.md 4a).
///
/// A keyboard nobody types on produces no events, and an entropy source that is
/// never exercised is untested code. QEMU can be told to deliver a keystroke
/// over its monitor protocol, so the test really does receive HID events - the
/// driver's drain path runs against a device that is actually sending.
///
/// The socket path is per ISA so three parallel boots cannot collide.
fn hid_device_args(arch: Arch) -> Vec<String> {
    let dev = match arch {
        Arch::X86_64 => "virtio-keyboard-pci,disable-legacy=on",
        _ => "virtio-keyboard-device",
    };
    let mut args: Vec<String> = Vec::new();
    if arch != Arch::X86_64 {
        // The modern virtio-mmio layout, as every other mmio device here needs.
        args.push("-global".into());
        args.push("virtio-mmio.force-legacy=false".into());
    }
    args.push("-device".into());
    args.push(dev.into());
    args.push("-qmp".into());
    args.push(format!("unix:{},server=on,wait=off", qmp_path(arch)));
    args
}

/// Where the QMP socket for this ISA's `rng` boot lives.
fn qmp_path(arch: Arch) -> String {
    format!("target/qmp-{}.sock", arch.name())
}

/// A **real TPM 2.0** for the `rng` kernel, so the TPM driver is executed rather
/// than only written (docs/TIME-IDENTITY.md 4a).
///
/// QEMU models the chip but not its behaviour: a `tpm-tis` device needs a
/// *backend*, and the one that works headlessly is `swtpm`, a software TPM
/// speaking the same protocol over a socket. So this starts one per ISA (its own
/// state directory and socket, so three parallel boots cannot collide) and hands
/// QEMU the chardev.
///
/// If `swtpm` is not installed the reason is printed and no TPM is attached: the
/// kernel then reports firmware describing no TPM, which is true of that machine,
/// and the phase says so rather than failing. That is the same
/// observe-what-is-there rule `gpu_device_args` follows for QXL.
fn tpm_device_args(arch: Arch) -> Vec<String> {
    if which("swtpm").is_none() {
        println!(
            "[xtask] swtpm not installed - no TPM attached, the rng kernel will report one absent"
        );
        return Vec::new();
    }
    let dir = format!("target/swtpm-{}", arch.name());
    let sock = format!("{dir}/sock");
    let _ = std::fs::create_dir_all(&dir);
    // A stale socket from a previous run points at a dead process, and QEMU's
    // connect then fails at launch with a zero-byte log - the failure shape the
    // GPU catalogue comment above warns about.
    let _ = std::fs::remove_file(&sock);
    // `--terminate` makes the daemon exit when QEMU disconnects, so a test run
    // leaves nothing behind.
    let started = std::process::Command::new("swtpm")
        .args([
            "socket",
            "--tpmstate",
            &format!("dir={dir}"),
            "--ctrl",
            &format!("type=unixio,path={sock}"),
            "--tpm2",
            "--daemon",
            "--terminate",
        ])
        .status();
    // Wait for the socket to appear rather than sleeping a guessed amount: a
    // deadline, not an iteration count.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !std::path::Path::new(&sock).exists() {
        if std::time::Instant::now() > deadline {
            println!("[xtask] swtpm did not create {sock} ({started:?}) - no TPM attached");
            return Vec::new();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // x86-64 q35 puts the TIS chip on the ISA bus at the architectural
    // 0xFED40000; arm/riscv `virt` take the MMIO variant, which the machine
    // places and describes in the device tree.
    let dev = match arch {
        Arch::X86_64 => "tpm-tis,tpmdev=tpm0",
        _ => "tpm-tis-device,tpmdev=tpm0",
    };
    [
        "-chardev",
        &format!("socket,id=chrtpm,path={sock}"),
        "-tpmdev",
        "emulator,id=tpm0,chardev=chrtpm",
        "-device",
        dev,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Where a program is on `PATH`, or `None`.
fn which(prog: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(prog))
        .find(|p| p.is_file())
}

/// The GPU device models `gpuhw` attaches, minus the ones this QEMU cannot make.
///
/// QXL needs SPICE compiled in and some distributions ship a QEMU without it, so
/// `-device qxl` is rejected *at launch* - QEMU exits before the kernel runs, which
/// is a zero-byte serial log and a failure that names no cause. That is the same
/// defect the kernel side already avoids for NVIDIA and Intel (no QEMU model at
/// all: classified by ID, absence reported), just moved up a layer - a hardcoded
/// catalogue of what QEMU has, asserted instead of observed.
///
/// So each model is checked against `-device help` and a missing one is dropped
/// with its reason printed. The kernel then reports that vendor absent exactly as
/// it reports NVIDIA absent, and drives every model that *is* there.
fn gpu_device_args(arch: Arch) -> Vec<String> {
    // A `pcie-root-port` with virtio-gpu behind it (reachable only if enumeration
    // programs the bridge's secondary bus - PVH boots have no firmware to do it),
    // then one function per GPU vendor QEMU models for the ISA.
    let mut args: Vec<String> = ["-device", "pcie-root-port,id=rp1,chassis=1,slot=1"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // (model name passed to -device, the base name `-device help` lists it under)
    let mut models: Vec<(&str, &str)> = vec![
        ("virtio-gpu-pci,bus=rp1,disable-legacy=on", "virtio-gpu-pci"),
        ("ati-vga", "ati-vga"),
        ("bochs-display", "bochs-display"),
        ("cirrus-vga", "cirrus-vga"),
    ];
    if arch == Arch::X86_64 {
        // VMware SVGA and Red Hat/QXL are x86-only in QEMU.
        models.push(("vmware-svga", "vmware-svga"));
        models.push(("qxl", "qxl"));
    }
    let listing = qemu_device_listing(arch);
    for (spec, base) in models {
        // `-device help` prints `name "ati-vga", bus PCI` - match the quoted name so
        // one model is never mistaken for another whose name contains it.
        if listing.contains(&format!("name \"{base}\"")) {
            args.push("-device".to_string());
            args.push(spec.to_string());
        } else {
            println!(
                "[xtask] {} has no '{base}' device model - not attached",
                arch.qemu()
            );
        }
    }
    args
}

/// What `qemu-system-<arch> -device help` prints, stdout and stderr together
/// (QEMU has used both over the years). Empty if QEMU cannot be run, in which case
/// every model reads as absent and the boot itself reports the missing QEMU.
fn qemu_device_listing(arch: Arch) -> String {
    Command::new(arch.qemu())
        .arg("-device")
        .arg("help")
        .output()
        .map(|o| {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            s
        })
        .unwrap_or_default()
}

fn fixed_qemu_args(arch: Arch, kernel: &str) -> &'static [&'static str] {
    match (kernel, arch) {
        // rng (docs/TIME-IDENTITY.md 4a): a **randomness device**, so the entropy
        // pool has a credited source that is not the CPU. It is what makes RISC-V
        // seedable at all - its `seed` CSR needs an M-mode grant this firmware does
        // not give - and it is attached on all three ISAs so the driver is proven
        // everywhere rather than only where the CPU instruction is missing.
        // Same two transports as every other virtio device here.
        ("rng", Arch::Riscv64 | Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-device",
            "virtio-rng-device",
        ],
        ("rng", Arch::X86_64) => &["-device", "virtio-rng-pci,disable-legacy=on"],
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
        // nvmefs (docs/SUBSTRATE.md S5): the same ext4 fixture behind a real NVMe
        // controller instead of virtio-blk. NVMe is a PCIe endpoint on every
        // machine here - the riscv/arm `virt` machines have a PCIe host bridge
        // (the same one `gpuhw` puts a root port on), and q35 obviously does - so
        // unlike virtio there is one device line for all three ISAs. `logical_block
        // _size` is left at QEMU's 512 default, which is what the driver requires
        // and refuses to guess around.
        // The `smp` kernel's per-core queue phase (docs/SUBSTRATE.md S5) needs a
        // real controller to create one queue pair per CPU on. Same fixture, same
        // read-only use - it never writes, so no snapshot is needed.
        // smp also gets **two memory nodes with the CPUs split across them**, so the
        // node-preferring claim (docs/SUBSTRATE.md pillar 6) has something to prefer:
        // CPUs 0-1 on node 0, 2-3 on node 1. Every pre-existing phase was verified to
        // pass unchanged under this launch before it was added - none of them asserts a
        // physical address - and on ARM64 no firmware describes memory at all, so there
        // the machine reports one node and the phase skips with a reason.
        ("smp", _) => &[
            "-drive",
            "file=tests/fixtures/ext4.img,if=none,id=nvm0,format=raw",
            "-device",
            "nvme,drive=nvm0,serial=rheonvme1",
            "-object",
            "memory-backend-ram,id=m0,size=512M",
            "-numa",
            "node,nodeid=0,memdev=m0,cpus=0-1",
            "-object",
            "memory-backend-ram,id=m1,size=512M",
            "-numa",
            "node,nodeid=1,memdev=m1,cpus=2-3",
        ],
        ("nvmefs", _) => &[
            "-drive",
            // `snapshot=on`: the write round-trip below is a real device write, and
            // the committed fixture must not be a casualty of running the test.
            // QEMU keeps the writes in a throwaway overlay, so reads see them
            // within the run and the file on disk is untouched.
            "file=tests/fixtures/ext4.img,if=none,id=nvm0,format=raw,snapshot=on",
            "-device",
            "nvme,drive=nvm0,serial=rheonvme0",
        ],
        // linuxdyn phase 3 (GOAL-DISK-2b): a per-ISA ext4 image built by
        // `build_dyn_disk_fixture` (gitignored), holding a dynamic glibc binary +
        // ld.so + libc, mounted off a live virtio-blk disk through ext4fs/ext4plus
        // and `execve`d. Same two transports as blockfs. If the image is a
        // placeholder (no e2fsprogs/toolchain), the test detects a non-ext4 disk
        // and skips phase 3.
        // linuxsmp's disk phase (docs/SMP.md 10.0e): the same `dyn-disk.img` linuxdyn
        // uses, so a *dynamically linked* Linux cell can be loaded off a live ext4 disk
        // and placed on a **secondary** core. That exercises the whole load path Node,
        // Bun and Claude Code depend on - block device, ext4, ld.so, file-backed mmap,
        // demand paging - from a core that is not the boot CPU, at a fraction of their
        // size.
        ("linuxsmp", Arch::Riscv64) => &[
            // Two cores of two threads, so the sysfs topology phase has something
            // to read. Inert for scheduling - same four hardware ids either way.
            "-smp",
            "4,sockets=1,cores=2,threads=2",
            // And two memory localities with the CPUs split across them, so the
            // `/sys/devices/system/node` phase has a `cpulist` and a `distance` row
            // that are not the degenerate one. **These lines are that phase's oracle.**
            "-object",
            "memory-backend-ram,id=m0,size=512M",
            "-numa",
            "node,nodeid=0,memdev=m0,cpus=0-1",
            "-object",
            "memory-backend-ram,id=m1,size=512M",
            "-numa",
            "node,nodeid=1,memdev=m1,cpus=2-3",
            "-numa",
            "dist,src=0,dst=1,val=20",
            "-numa",
            "dist,src=1,dst=0,val=20",
            "-global",
            "virtio-mmio.force-legacy=false",
            "-drive",
            "file=tests/linux-fixtures/build/riscv64/dyn-disk.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-device,drive=blk0",
        ],
        ("linuxsmp", Arch::Aarch64) => &[
            // Two cores of two threads, so the sysfs topology phase has something
            // to read. Inert for scheduling - same four hardware ids either way.
            "-smp",
            "4,sockets=1,cores=2,threads=2",
            // And two memory localities with the CPUs split across them, so the
            // `/sys/devices/system/node` phase has a `cpulist` and a `distance` row
            // that are not the degenerate one. **These lines are that phase's oracle.**
            "-object",
            "memory-backend-ram,id=m0,size=512M",
            "-numa",
            "node,nodeid=0,memdev=m0,cpus=0-1",
            "-object",
            "memory-backend-ram,id=m1,size=512M",
            "-numa",
            "node,nodeid=1,memdev=m1,cpus=2-3",
            "-numa",
            "dist,src=0,dst=1,val=20",
            "-numa",
            "dist,src=1,dst=0,val=20",
            "-global",
            "virtio-mmio.force-legacy=false",
            "-drive",
            "file=tests/linux-fixtures/build/aarch64/dyn-disk.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-device,drive=blk0",
        ],
        ("linuxsmp", Arch::X86_64) => &[
            // Two cores of two threads, so the sysfs topology phase has something
            // to read. Inert for scheduling - same four hardware ids either way.
            "-smp",
            "4,sockets=1,cores=2,threads=2",
            // And two memory localities with the CPUs split across them, so the
            // `/sys/devices/system/node` phase has a `cpulist` and a `distance` row
            // that are not the degenerate one. **These lines are that phase's oracle.**
            "-object",
            "memory-backend-ram,id=m0,size=512M",
            "-numa",
            "node,nodeid=0,memdev=m0,cpus=0-1",
            "-object",
            "memory-backend-ram,id=m1,size=512M",
            "-numa",
            "node,nodeid=1,memdev=m1,cpus=2-3",
            "-numa",
            "dist,src=0,dst=1,val=20",
            "-numa",
            "dist,src=1,dst=0,val=20",
            "-drive",
            "file=tests/linux-fixtures/build/x86_64/dyn-disk.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-pci,drive=blk0,disable-legacy=on",
        ],
        ("linuxdyn", Arch::Riscv64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-drive",
            "file=tests/linux-fixtures/build/riscv64/dyn-disk.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-device,drive=blk0",
        ],
        ("linuxdyn", Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-drive",
            "file=tests/linux-fixtures/build/aarch64/dyn-disk.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-device,drive=blk0",
        ],
        ("linuxdyn", Arch::X86_64) => &[
            "-drive",
            "file=tests/linux-fixtures/build/x86_64/dyn-disk.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-pci,drive=blk0,disable-legacy=on",
        ],
        // linuxnode (GOAL-NODE): the real x86-64 `node` binary + its glibc/libstdc++
        // shared-library set on a live virtio-blk disk, streamed + `execve`d off ext4
        // (the linuxdyn disk path at production scale, ~124 MB). x86-64 only - there
        // is no arm64/riscv64 node build here, so those ISAs get no drive, and the
        // test skips-with-reason (`virtio_blk::probe()` returns None). The image is
        // built by `build_node_disk_fixture` (gitignored); a placeholder when
        // /opt/node22 or mkfs.ext4 is absent makes the test skip on x86-64 too, so CI
        // (no node binary) stays green.
        // The same images as their primary-CPU counterparts, for the secondary-core runs
        // (docs/SMP.md 10.0e).
        ("linuxnodesmp", Arch::X86_64) => &[
            "-drive",
            "file=tests/linux-fixtures/build/x86_64/node-disk.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-pci,drive=blk0,disable-legacy=on",
        ],
        ("linuxclaudesmp", Arch::X86_64) => &[
            "-drive",
            "file=tests/linux-fixtures/build/x86_64/claude-disk.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-pci,drive=blk0,disable-legacy=on",
        ],
        ("linuxnode", Arch::X86_64) => &[
            "-drive",
            "file=tests/linux-fixtures/build/x86_64/node-disk.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-pci,drive=blk0,disable-legacy=on",
        ],
        // linuxbun (GOAL-BUN): the real x86-64 `bun` binary + its glibc set on a
        // live virtio-blk disk, streamed + `execve`d off ext4 (the linuxnode shape,
        // JavaScriptCore instead of V8). x86-64 only; arm/riscv get no drive and the
        // test skips-with-reason. Image built by `build_bun_disk_fixture`
        // (gitignored); a placeholder when /root/.bun is absent (CI) makes the test
        // skip, so CI stays green.
        // The same bun image as `linuxbun`, for the secondary-core run (docs/SMP.md 10.0e).
        ("linuxbunsmp", Arch::X86_64) => &[
            "-drive",
            "file=tests/linux-fixtures/build/x86_64/bun-disk.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-pci,drive=blk0,disable-legacy=on",
        ],
        ("linuxbun", Arch::X86_64) => &[
            "-drive",
            "file=tests/linux-fixtures/build/x86_64/bun-disk.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-pci,drive=blk0,disable-legacy=on",
        ],
        // linuxclaude (GOAL-CLAUDE): the real Claude Code binary (~275 MB), the
        // workload docs/ARCHITECTURE-DEBT.md 4.0 measured this tree against. Same
        // shape as linuxbun - it *is* a Bun-compiled executable - so the same
        // transport and the same skip-when-absent behaviour.
        ("linuxclaude", Arch::X86_64) => &[
            "-drive",
            "file=tests/linux-fixtures/build/x86_64/claude-disk.img,if=none,id=blk0,format=raw",
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
        // netwait (docs/NETSTACK.md, the async-receive path / rheo-net N2d): the
        // same SLIRP + virtio-net setup as librheonet - the ARP reply is now the
        // wake event for a *parked* receive (the cell blocks in SYS_WAIT_NET and,
        // on riscv/arm, the kernel idles at WFI until the NIC's RX interrupt
        // fires). Deterministic + network-free. Same two transports.
        ("netwait", Arch::Riscv64 | Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-device,netdev=n0",
        ],
        ("netwait", Arch::X86_64) => &[
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-pci,netdev=n0,disable-legacy=on",
        ],
        // netservice (docs/NETSTACK.md the service-cell section, rheo-net Phase N4a):
        // the service cell's proof is deterministic and network-free, but it also
        // performs ONE bonus live ARP for a client, so it gets the same SLIRP +
        // virtio-net setup as librheonet. With no netdev the service reports the live
        // path skipped and still passes - the netdev only unlocks the bonus.
        ("netservice", Arch::Riscv64 | Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-device,netdev=n0",
        ],
        ("netservice", Arch::X86_64) => &[
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-pci,netdev=n0,disable-legacy=on",
        ],
        // linuxnet (docs/NETSTACK.md N4b, docs/LINUX-COMPAT.md L8-INET remote): the
        // same SLIRP + virtio-net setup as netcore, now driven by an *unmodified
        // static-glibc Linux binary* through the `svc::SocketOps` bridge - a DNS
        // query to SLIRP's built-in responder (10.0.2.3:53) and a TCP connect to a
        // closed gateway port (10.0.2.2:9, answered with a reset). Deterministic +
        // network-free. Same two transports: virtio-mmio on arm/riscv, virtio-pci
        // on x86 (disable-legacy=on pins the modern layout the driver expects).
        ("linuxnet", Arch::Riscv64 | Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-device,netdev=n0",
        ],
        ("linuxnet", Arch::X86_64) => &[
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
        // nethostcfg (docs/NETSTACK.md rheo-net Phase N4c): host configuration -
        // DHCP + zeroconf/mDNS + NTP + the hostcfg store, over the same SLIRP +
        // virtio-net setup as netdns. The deterministic core (codecs, the DHCP state
        // machine + its timers, the link-local ARP claim, the mDNS codec, the NTP
        // offset/delay KAT) is entirely network-free; the netdev is here only so the
        // four *bonus* live attempts genuinely put frames on the wire - SLIRP runs no
        // guest-visible DHCP/NTP/mDNS service, so each skips with a printed reason.
        // Same two transports: virtio-mmio on arm/riscv, virtio-pci on x86.
        ("nethostcfg", Arch::Riscv64 | Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-device,netdev=n0",
        ],
        ("nethostcfg", Arch::X86_64) => &[
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
        // netsmoltcp (docs/NETSTACK.md §13, rheo-net Phase N2c): the smoltcp
        // blessed transport cell drives the NIC over the same SLIRP + virtio-net
        // setup as netl4. The smoltcp UDP socket sends a DNS query to SLIRP's
        // built-in responder (10.0.2.3:53) and receives the reply. Same two
        // transports: virtio-mmio on arm/riscv, virtio-pci on x86-64.
        ("netsmoltcp", Arch::Riscv64 | Arch::Aarch64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-device,netdev=n0",
        ],
        ("netsmoltcp", Arch::X86_64) => &[
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
        // gpuhw's device set is built by `gpu_device_args`, which drops a model
        // this QEMU build does not have rather than letting QEMU refuse to launch.
        // iommu (docs/GPU-HARDWARE.md 4, BUILD-ORDER.md step 12): VT-d DMA
        // remapping. x86-64 q35 gets `-device intel-iommu` (caching-mode=on
        // so the vIOMMU faults + requires invalidation, the mode that
        // reports out-of-grant DMA) plus a virtio-blk-pci disk as the DMA
        // source. intel-iommu must precede the devices it covers. arm/riscv
        // `virt` surface no DMAR base, so the kernel skips-with-reason and
        // needs no IOMMU device (a plain virtio-blk keeps the probe honest).
        ("iommu", Arch::X86_64) => &[
            "-device",
            "intel-iommu,caching-mode=on",
            "-drive",
            "file=tests/fixtures/ext4.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-pci,drive=blk0,disable-legacy=on,iommu_platform=on,share-rw=on",
            "-drive",
            "file=tests/fixtures/ext4.img,if=none,id=nvm0,format=raw,readonly=on",
            "-device",
            "nvme,drive=nvm0,serial=rheonvme2,share-rw=on",
        ],
        // ARM64: the SMMUv3 covers PCI, so use virtio-blk-*pci* (behind the
        // SMMU) with iommu_platform=on, mirroring the x86 VT-d proof. The
        // machine gains iommu=smmuv3 via `machine_override`.
        ("iommu", Arch::Aarch64) => &[
            "-drive",
            "file=tests/fixtures/ext4.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-pci,drive=blk0,disable-legacy=on,iommu_platform=on",
        ],
        // RISC-V has no QEMU IOMMU model, so the kernel skips-with-reason;
        // a plain virtio-blk keeps the (unreached) probe honest.
        ("iommu", Arch::Riscv64) => &[
            "-global",
            "virtio-mmio.force-legacy=false",
            "-drive",
            "file=tests/fixtures/ext4.img,if=none,id=blk0,format=raw",
            "-device",
            "virtio-blk-device,drive=blk0",
        ],
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
        // numa (docs/SUBSTRATE.md pillar 6): two 512 MiB memory nodes, so the frame
        // pool - which sits 64 MiB into RAM and is 512 MiB long - genuinely straddles
        // the boundary and each node owns a real share of it. The **same args on
        // every ISA**, deliberately: what differs is what each ISA's firmware path
        // then reports (SRAT on x86-64, the device tree on riscv64, nothing at all on
        // a bare-ELF arm boot), and holding the launch identical is what makes that
        // difference the ISA's rather than the test's. CPU affinity is left
        // unassigned - the claim is about memory placement.
        ("numa", Arch::X86_64) => &[
            "-object",
            "memory-backend-ram,id=m0,size=512M",
            "-numa",
            // `cpus=` is required once `hmat=on` is set: HMAT latency and bandwidth are
            // stated per (initiator, target), and an initiator proximity domain is one with
            // CPUs. Without it QEMU refuses the `hmat-lb` lines outright.
            "node,nodeid=0,memdev=m0,cpus=0-1",
            "-object",
            "memory-backend-ram,id=m1,size=512M",
            "-numa",
            "node,nodeid=1,memdev=m1,cpus=2-3",
            // The SLIT distances, and **these arguments are the test's oracle**: the kernel
            // asserts that the graph reports exactly what is declared here, never that its
            // own parser is self-consistent (docs/RESOURCE-GRAPH.md 5). ACPI's local value
            // is 10; 20 is the conventional one-hop remote.
            "-numa",
            "dist,src=0,dst=1,val=20",
            "-numa",
            "dist,src=1,dst=0,val=20",
            // HMAT: the magnitudes SLIT cannot give. These values are the test's oracle for
            // `Cost::latency_ns` and `Cost::bandwidth_mbs`, exactly as the distances above are
            // for `hops`. ACPI-only, so riscv64 and ARM64 ignore them and the kernel asserts
            // those read 0 = unknown there rather than a number derived from the distance.
            "-numa",
            "hmat-lb,initiator=0,target=1,hierarchy=memory,data-type=access-latency,latency=100",
            "-numa",
            "hmat-lb,initiator=0,target=1,hierarchy=memory,data-type=access-bandwidth,bandwidth=10240M",
            "-numa",
            "hmat-lb,initiator=1,target=0,hierarchy=memory,data-type=access-latency,latency=100",
            "-numa",
            "hmat-lb,initiator=1,target=0,hierarchy=memory,data-type=access-bandwidth,bandwidth=10240M",
        ],

        // hwinfo (docs/RESOURCE-GRAPH.md 2.5): a CPU topology with something in it to
        // discover. The base launch is a flat `-smp 4`, where every CPU is its own core in
        // its own package - a shape in which a correct discovery and a broken one both say
        // "four cores", so it can prove nothing. One socket, two cores, two threads each is
        // the smallest launch where the two groupings differ: 4 CPUs, 2 SMT pairs, 1 cache
        // domain. **These numbers are the test's oracle**, exactly as the `-numa dist` values
        // are for distances - the kernel asserts the topology it discovered matches what is
        // declared here, never that its own decode is self-consistent.
        //
        // The same line on every ISA, so what differs is what each ISA can *see*: CPUID on
        // x86-64, MPIDR's MT bit on ARM64, the device tree's `cpu-map` on riscv64 - and QEMU
        // flattens threads out of the riscv `cpu-map`, which the test reports rather than
        // works around.
        // hwinfo (docs/RESOURCE-GRAPH.md 2.4a): a CPU topology with something in it to
        // discover. `linuxsmp` gets the same line in its own arms above - it already has one
        // for its disk, and a later arm would never be reached.
        ("hwinfo", _) => &["-smp", "4,sockets=1,cores=2,threads=2"],

        ("numa", _) => &[
            "-object",
            "memory-backend-ram,id=m0,size=512M",
            "-numa",
            "node,nodeid=0,memdev=m0",
            "-object",
            "memory-backend-ram,id=m1,size=512M",
            "-numa",
            "node,nodeid=1,memdev=m1",
            // The SLIT distances, and **these arguments are the test's oracle**: the kernel
            // asserts that the graph reports exactly what is declared here, never that its
            // own parser is self-consistent (docs/RESOURCE-GRAPH.md 5). ACPI's local value
            // is 10; 20 is the conventional one-hop remote.
            "-numa",
            "dist,src=0,dst=1,val=20",
            "-numa",
            "dist,src=1,dst=0,val=20",
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

    /// Target for **hard-float cells** (librheo): the kernel stays soft-float
    /// (`target()`), but a cell that wants the vector units compiles hard-float
    /// so it can emit SSE/NEON (and, via runtime `#[target_feature]` dispatch,
    /// AVX/AVX-512/VNNI). x86 needs a custom JSON (the bare target forces
    /// soft-float); ARM64 uses the builtin hard-float (`+neon`) triple; RISC-V's
    /// bare target is already `lp64d` hard-float, so it is unchanged. See
    /// docs/LIBRHEO.md / docs/TILES.md 4.
    fn cell_target(self) -> &'static str {
        match self {
            Arch::X86_64 => "targets/rheo_cell-x86_64.json",
            Arch::Aarch64 => "aarch64-unknown-none",
            Arch::Riscv64 => "riscv64gc-unknown-none-elf",
        }
    }

    /// Output directory stem for `cell_target()` (the `target/<stem>/` cargo
    /// writes to). For x86 this is the JSON file stem, not the triple.
    fn cell_target_dir(self) -> &'static str {
        match self {
            Arch::X86_64 => "rheo_cell-x86_64",
            Arch::Aarch64 => "aarch64-unknown-none",
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
                // `hmat=on` publishes the ACPI HMAT table. Inert for every kernel that does
                // not read it - one extra table in the RSDT - and it is on the machine line
                // because QEMU takes a single `-machine`, so it cannot be added per kernel
                // the way `-numa` can (docs/RESOURCE-GRAPH.md 2.4).
                "q35,kernel-irqchip=split,hmat=on",
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
    let mut trace_window = String::new();
    let mut trace_ledger = false;
    let mut bin = String::from("kernel");
    // Set only when `--bin` is passed explicitly, so `test` can tell "run just
    // this kernel" apart from the `run` default (which is also a valid kernel
    // name). Without this, `test --bin <name>` silently booted all of them.
    let mut bin_filter: Option<String> = None;
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
                bin_filter = Some(value.clone());
            }
            // `trace` only: show one subsystem's window rather than the summary.
            "--window" => {
                let Some(value) = iter.next() else {
                    eprintln!("error: --window needs a subsystem name");
                    return ExitCode::FAILURE;
                };
                trace_window = value.clone();
            }
            // `trace` only: balance acquires against releases per owner.
            "--ledger" => trace_ledger = true,
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
        // `--bin <kernel>[,<kernel>...]` boots only those (fast iteration on the
        // kernels a change can actually affect); without it the whole matrix
        // runs. A userspace-only change cannot affect an unrelated kernel, but a
        // *kernel* change can - `.bss` motion once broke an unrelated kernel
        // (docs/ENGINEERING.md 11), so kernel changes still owe the full matrix.
        "test" => {
            let kernels: Vec<&str> = match &bin_filter {
                None => TEST_KERNELS.to_vec(),
                Some(list) => {
                    let selected: Vec<&str> = list
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .collect();
                    if selected.is_empty() {
                        eprintln!("error: --bin needs at least one kernel name");
                        return ExitCode::FAILURE;
                    }
                    if let Some(bad) = selected.iter().find(|n| !TEST_KERNELS.contains(n)) {
                        eprintln!("error: unknown test kernel '{bad}'");
                        eprintln!("known: {}", TEST_KERNELS.join(", "));
                        return ExitCode::FAILURE;
                    }
                    selected
                }
            };
            arches.iter().all(|&a| {
                build(a, true)
                    && kernels.iter().all(|kernel| {
                        let args = extra_qemu_args(a, kernel);
                        let args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                        boot_expect_pass(a, true, kernel, &args)
                    })
            })
        }
        // Benchmarks always run the release build: instruction path
        // lengths of an unoptimized kernel are not the system's numbers.
        "bench" => arches.iter().all(|&a| build(a, true) && bench(a, true)),
        // Type-check only: the fast inner development loop (see `check`).
        "check" => arches.iter().all(|&a| check(a)),
        // Host-side model checking of kernel state machines (see `verify`).
        "verify" => verify(),
        // Report the kernel's largest static allocations (see `sizes`).
        "sizes" => arches.iter().all(|&a| sizes(a, &bin)),
        // Window and query a boot's structured trace (see `trace`).
        "trace" => arches
            .iter()
            .all(|&a| trace(a, &bin, &trace_window, trace_ledger)),
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
        "usage: cargo xtask <build|check|run|test|bench|verify|sizes|trace|std-patch> \
         [--arch x86_64|aarch64|riscv64|all] [--bin <kernel>[,<kernel>...]] [--release]\n\
         \x20 trace: [--window <subsys>] [--ledger]"
    );
}

/// One parsed `@E` line from a boot's structured trace ([`kernel::trace`]).
struct Ev {
    seq: u64,
    ts: u64,
    cpu: u64,
    subsys: String,
    kind: u8,
    owner: u64,
    a: u64,
    b: u64,
}

/// Window and query the structured trace a boot left in its serial log.
///
/// The host half of [`kernel::trace`], and the reason it is a tool rather than an eyeball
/// exercise: a boot's log is one interleaved scrollback in which the three lines that
/// matter sit thousands apart from anything related to them. This groups the stream by
/// **subsystem** and by **owner** - a navigable buffer per source, the treatment cat9
/// gives a command's output - so the question "what did cell 3 do to its frames" is a
/// query rather than a grep.
///
/// Three views, in the order they are usually wanted:
///
/// - **summary** (default): one line per subsystem window - how many events, over what
///   span, and how the acquires and releases balance. A nonzero balance is a leak and
///   says which window to open.
/// - `--window <subsys>`: that window's events, in order.
/// - `--ledger`: per **owner**, acquires against releases, with the surviving balance and
///   the sequence number of the first unmatched acquire. That last number is the point: a
///   leak stops being "the total did not return to zero" and becomes "sequence 412 took a
///   frame nobody gave back", which is a place to look rather than a fact to explain.
///
/// **Loss is located, not counted.** Every event carries a sequence number, so a gap is
/// reported with the range it spans; a reader is told where the record is incomplete
/// rather than being handed a total and left to assume the rest is sound.
fn trace(arch: Arch, bin: &str, window: &str, ledger: bool) -> bool {
    let bin = if bin.is_empty() { "smp" } else { bin };
    let path = format!("target/qemu-{}-{bin}.log", arch.name());
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!(
            "[xtask] no log at {path} - run `cargo xtask test --arch {} --bin {bin}` first",
            arch.name()
        );
        return false;
    };
    let mut evs: Vec<Ev> = Vec::new();
    let mut header = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("@E# ") {
            header = rest.to_string();
            continue;
        }
        let Some(rest) = line.strip_prefix("@E ") else {
            continue;
        };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() != 8 {
            continue;
        }
        let num = |i: usize| f[i].parse::<u64>().ok();
        let (Some(seq), Some(ts), Some(cpu), Some(kind), Some(owner), Some(a), Some(b)) =
            (num(0), num(1), num(2), num(4), num(5), num(6), num(7))
        else {
            continue;
        };
        evs.push(Ev {
            seq,
            ts,
            cpu,
            subsys: f[3].to_string(),
            kind: kind as u8,
            owner,
            a,
            b,
        });
    }
    if evs.is_empty() {
        println!(
            "[xtask] {} {bin}: no trace in the log - the boot did not call `trace::enable()`",
            arch.name()
        );
        return true;
    }
    println!(
        "[xtask] {} {bin}: {} event(s) {header}",
        arch.name(),
        evs.len()
    );

    // Loss, located: a gap in the sequence is where the record is incomplete.
    let mut gaps = 0usize;
    for w in evs.windows(2) {
        if w[1].seq != w[0].seq + 1 {
            println!(
                "  LOST  seq {}..{} ({} event(s) overwritten)",
                w[0].seq,
                w[1].seq,
                w[1].seq - w[0].seq - 1
            );
            gaps += 1;
        }
    }
    if gaps > 0 {
        println!(
            "  ({gaps} gap(s) - the ring wrapped; raise `trace::CAPACITY` or narrow what is traced)"
        );
    }

    if ledger {
        return trace_ledger_view(&evs);
    }
    if !window.is_empty() {
        let base = evs.iter().map(|e| e.ts).min().unwrap_or(0);
        let mut n = 0usize;
        for e in evs.iter().filter(|e| e.subsys == window) {
            println!(
                "  {:>6} +{:>10}ns cpu{} {:<8} owner {:<5} a={} b={:#x}",
                e.seq,
                e.ts.saturating_sub(base),
                e.cpu,
                kind_name(e.kind),
                owner_name(e.owner),
                e.a,
                e.b
            );
            n += 1;
        }
        if n == 0 {
            println!("  window `{window}` is empty - nothing in this boot traced it");
        }
        return true;
    }

    // Summary: one line per window. Sorted by name so two runs are diffable.
    let mut names: Vec<&str> = evs.iter().map(|e| e.subsys.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let w: Vec<&Ev> = evs.iter().filter(|e| e.subsys == name).collect();
        let acq: u64 = w.iter().filter(|e| e.kind == 0).map(|e| e.a).sum();
        let rel: u64 = w.iter().filter(|e| e.kind == 1).map(|e| e.a).sum();
        let span = w.last().map(|e| e.ts).unwrap_or(0) - w.first().map(|e| e.ts).unwrap_or(0);
        let bal = acq as i64 - rel as i64;
        // A **positive** balance is a leak: taken inside the window, never returned. A
        // negative one is not, and saying so matters - it means frames acquired *before*
        // tracing started were released inside it, the ordinary consequence of enabling
        // the trace part-way through a boot. The first version called both a leak, and
        // the tool's own first run duly reported four cells leaking when nothing was
        // wrong. A diagnostic that cries wolf is worse than no diagnostic.
        let note = match bal {
            0 => "",
            b if b > 0 => "   <-- LEAK: taken inside the window, never returned",
            _ => "   (negative: released frames taken before tracing began)",
        };
        println!(
            "  {name:<8} {:>5} event(s)  acquired {acq:<6} released {rel:<6} balance {bal:<6} over {span}ns{note}",
            w.len(),
        );
    }
    println!("  (`--window <name>` for one window's events, `--ledger` for per-owner balances)");
    true
}

fn kind_name(k: u8) -> &'static str {
    match k {
        0 => "acquire",
        1 => "release",
        2 => "transfer",
        3 => "refuse",
        _ => "note",
    }
}

fn owner_name(o: u64) -> String {
    if o == u16::MAX as u64 {
        "kernel".to_string()
    } else {
        format!("cell{o}")
    }
}

/// Per-owner acquire/release balance, with the first unmatched acquire named.
fn trace_ledger_view(evs: &[Ev]) -> bool {
    let mut owners: Vec<u64> = evs.iter().map(|e| e.owner).collect();
    owners.sort_unstable();
    owners.dedup();
    let mut leaked = 0i64;
    for o in owners {
        let mut bal = 0i64;
        // The sequence at which the balance last rose from zero: where an unmatched
        // acquire began, which is the line a leak hunt should start from.
        let mut first_unmatched = None;
        let mut peak = 0i64;
        for e in evs.iter().filter(|e| e.owner == o) {
            match e.kind {
                0 => {
                    if bal == 0 {
                        first_unmatched = Some(e.seq);
                    }
                    bal += e.a as i64;
                }
                1 => bal -= e.a as i64,
                _ => {}
            }
            peak = peak.max(bal);
        }
        // Only a **positive** balance is unreturned; see the note in the summary view. The
        // sequence number is reported only for that case, because "first unmatched" has no
        // meaning for an owner that released more than it took inside the window.
        let tail = match (bal, first_unmatched) {
            (b, Some(s)) if b > 0 => format!("   <-- {b} unreturned, first unmatched at seq {s}"),
            (b, _) if b > 0 => format!("   <-- {b} unreturned"),
            (b, _) if b < 0 => format!("   ({} released from before the window)", -b),
            _ => String::new(),
        };
        leaked += bal.max(0);
        println!(
            "  {:<8} peak {peak:<6} balance {bal:<6}{tail}",
            owner_name(o)
        );
    }
    if leaked == 0 {
        println!("  no owner has an unreturned acquire in this window");
    } else {
        println!("  {leaked} frame(s) taken inside this window were never returned");
    }
    true
}

/// Report the largest **static allocations** in a built kernel, biggest last.
///
/// Why this is a command rather than a thing you work out each time: a recurring question
/// in this tree is "what is this fixed table actually costing", because the answer decides
/// whether a ceiling is worth removing and what shape the replacement should be
/// (docs/EXECUTION-MODEL.md 9.3 refused the obvious design on exactly such a number - one
/// funded table per cell would have spent 256 KiB of frames to save 21 KiB of `.bss`).
///
/// The way that question kept getting answered was by adding a throwaway
/// `const _: [(); 0] = [(); size_of::<T>()];`, reading the size out of the compile error,
/// and deleting it again - which is slow, tells you about one type at a time rather than
/// about the binary, and has a genuine failure mode: undoing it with `git checkout <file>`
/// discards every other edit in the file, which happened once in this tree and cost a
/// completed refactor.
///
/// `nm` already knows. This reads the symbol table of a kernel that is already built and
/// prints the `.bss`/`.data` symbols by size, so the question is one command and touches
/// no source at all.
fn sizes(arch: Arch, bin: &str) -> bool {
    // `smp` by default: it links the widest set of kernel subsystems, so its statics are
    // the closest thing to "the kernel's".
    let bin = if bin.is_empty() { "smp" } else { bin };
    let path = format!("target/{}/release/{bin}", arch.target());
    if !std::path::Path::new(&path).exists() {
        eprintln!(
            "[xtask] {} {bin}: not built - run `cargo xtask build --arch {}` first",
            arch.name(),
            arch.name()
        );
        return false;
    }
    // `-C` demangles, `-S` prints sizes, `--size-sort` orders by them. Symbols with no
    // size (most code labels) are omitted by `-S`, which is what leaves the tables.
    let out = match Command::new("nm")
        .args(["-C", "-S", "--size-sort", &path])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => {
            eprintln!("[xtask] {}: `nm` failed on {path}", arch.name());
            return false;
        }
    };
    let text = String::from_utf8_lossy(&out);
    let mut rows: Vec<(u64, &str, char)> = Vec::new();
    for line in text.lines() {
        // "<addr> <size> <kind> <name>"; kind b/B is .bss, d/D/g/G is .data.
        let mut f = line.splitn(4, ' ');
        let (_, size, kind, name) = match (f.next(), f.next(), f.next(), f.next()) {
            (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
            _ => continue,
        };
        let kind = kind.chars().next().unwrap_or('?');
        if !matches!(kind, 'b' | 'B' | 'd' | 'D' | 'g' | 'G') {
            continue;
        }
        let Ok(size) = u64::from_str_radix(size.trim(), 16) else {
            continue;
        };
        rows.push((size, name.trim(), kind));
    }
    rows.sort_by_key(|r| r.0);
    let total: u64 = rows.iter().map(|r| r.0).sum();
    // Biggest last, so the interesting end is next to the prompt.
    let shown = rows.len().min(25);
    println!(
        "[xtask] {} {bin}: {} static symbol(s), {total} bytes",
        arch.name(),
        rows.len()
    );
    for (size, name, kind) in rows.iter().skip(rows.len() - shown) {
        println!("  {size:>10}  {kind}  {name}");
    }
    true
}

/// Host-side model checking of the kernel state machines that are integer-only and
/// dependency-free (docs/EXECUTION-MODEL.md 8, `verify/`).
///
/// Each driver `#[path]`-includes the shipped kernel source and shims only the storage
/// the kernel funds from frames - the same rule `comparison/` follows. It is a separate
/// command from `test` on purpose: `test` boots QEMU and takes minutes, this takes
/// seconds and catches a class of defect that otherwise needs four cores and a
/// 120-second boot to surface. Neither replaces the other, and CI runs both.
fn verify() -> bool {
    // No target flag: this is a host program, and that is the point.
    let drivers = [
        ("entity", "verify/entity/fuzz.rs"),
        ("telemetry", "verify/telemetry/fuzz.rs"),
        ("graph", "verify/graph/fuzz.rs"),
        ("hetero", "verify/hetero/fuzz.rs"),
    ];
    let out = std::path::Path::new("target/verify");
    if let Err(e) = std::fs::create_dir_all(out) {
        eprintln!("error: cannot create {}: {e}", out.display());
        return false;
    }
    drivers.iter().all(|&(name, src)| {
        let bin = out.join(name);
        println!("[xtask] verify: building {src}");
        // **The kernel's edition**, because these drivers `#[path]`-include kernel source
        // verbatim. Built as 2021 they diverge silently until a file uses a 2024 construct and
        // then fail to *compile* - which is how a let-chain in `hw/graph.rs` broke this driver
        // and went unnoticed for a commit, because the check that was run counted passing
        // properties instead of reading the verdict.
        let built = Command::new("rustc")
            .args(["-O", "--edition", "2024", "-A", "dead_code", "-o"])
            .arg(&bin)
            .arg(src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !built {
            eprintln!("[xtask] verify {name}: FAIL (did not compile)");
            return false;
        }
        println!("[xtask] verify: running {name}");
        let ran = Command::new(&bin)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        println!(
            "[xtask] verify {name}: {}",
            if ran { "PASS" } else { "FAIL" }
        );
        ran
    })
}

/// **Type-check the kernel library, both with and without the `smp` feature** -
/// the fast inner loop for kernel work.
///
/// `build` is the honest gate, but it is not a development loop: before it reaches
/// a single line of kernel code it cross-builds the `userland` programs, every
/// librheo bin (twice, for the embedded spine), four separately-featured `net`
/// cells, the std programs, the coreutils multicall, the columnar dataset, the
/// pmem backing file, and the glibc Linux fixtures - none of which a change to
/// `kernel/src/` can affect. `check` skips all of it, so a compile error in kernel
/// code surfaces in seconds rather than minutes.
///
/// **Scope, deliberately: the `kernel` package only.** The `qemu-tests` package
/// `include_bytes!`s cell ELFs that do not exist until `build` has produced them,
/// so checking it without a prior build reports missing fixtures rather than
/// anything about the code. Test kernels are covered by `build`/`test`.
///
/// It checks the **`smp` feature in its own invocation**, because that is a
/// separate compilation of the same library: per-CPU code paths that exist only
/// under `kernel/smp` are the ones a portable change is most likely to break, and
/// the ordinary build hides them until the very end (`build` compiles the feature
/// only for the single `smp` bin). Both configurations must be clean.
///
/// Same target and RUSTFLAGS as `build` per ISA, so what it checks is what will be
/// built. It does **not** replace `build`/`test`: it cannot catch a link error, a
/// missing fixture, or anything about running.
fn check(arch: Arch) -> bool {
    for (label, features) in [("kernel", None), ("kernel + smp feature", Some("smp"))] {
        println!("[xtask] checking {label} for {}", arch.name());
        let mut cmd = Command::new("cargo");
        cmd.args([
            "check",
            "-p",
            "kernel",
            "--target",
            arch.target(),
            "-Zbuild-std=core,alloc,compiler_builtins",
            "-Zbuild-std-features=compiler-builtins-mem",
        ]);
        if let Some(feature) = features {
            cmd.args(["--features", feature]);
        }
        if let Some(flags) = kernel_rustflags(arch) {
            cmd.env("RUSTFLAGS", flags);
        }
        if !matches!(cmd.status().map(|s| s.success()), Ok(true)) {
            return false;
        }
    }
    true
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
    // The soft-float cells (userland/libc/net) build for the kernel's bare
    // target: they do no vector work, so hard-float buys them nothing.
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "-p",
        "userland",
        "-p",
        "rheo-libc",
        "-p",
        "rheo-net",
        // The deterministic-proof support types (`tcp::VirtualLink`) live behind
        // the non-default `proof` feature so a fault injector cannot reach a
        // production posture, and above all cannot reach the librheo-free codec
        // posture that links beside a *kernel* binary
        // (docs/ARCHITECTURE-DEBT.md 3.5). The demo **cells** are exactly the
        // proofs that drive it, so they are built with it on.
        "--features",
        "proof",
        "--release",
        "--target",
        arch.target(),
        "-Zbuild-std=core,alloc,compiler_builtins",
        "-Zbuild-std-features=compiler-builtins-mem",
    ]);
    if !matches!(cmd.status().map(|s| s.success()), Ok(true)) {
        return false;
    }
    // librheo builds **hard-float** (docs/LIBRHEO.md / docs/TILES.md 4): its
    // tile executor and any SIMD path need real vector registers. Built for the
    // cell target, then staged into the kernel target's release dir the test
    // kernels `include_bytes!` from (a build-orchestration copy - the loader is
    // unchanged; the kernel remains soft-float and just loads the hard-float
    // ELF, with FP state saved/restored across cell switches).
    if !build_librheo(arch, false) {
        return false;
    }
    stage_cell_bins(arch)
}

/// Build the librheo cell binaries for `arch`'s hard-float cell target. When
/// `embedded`, rebuild only `librheo-embed` with `--no-default-features` (the
/// minimal spine, docs/LIBRHEO.md Phase F).
fn build_librheo(arch: Arch, embedded: bool) -> bool {
    let mut cmd = Command::new("cargo");
    cmd.args(["build", "-p", "librheo"]);
    if embedded {
        cmd.args(["--bin", "librheo-embed", "--no-default-features"]);
    }
    cmd.args([
        "--release",
        "--target",
        arch.cell_target(),
        "-Zbuild-std=core,alloc,compiler_builtins",
        "-Zbuild-std-features=compiler-builtins-mem",
        "-Zjson-target-spec",
    ]);
    matches!(cmd.status().map(|s| s.success()), Ok(true))
}

/// Stage the hard-float librheo cell binaries into the kernel target's release
/// dir, where the test kernels `include_bytes!` them. A no-op on RISC-V, whose
/// cell target IS the kernel target (already `lp64d` hard-float). Copies the
/// top-level executables (extensionless files) only, not `deps/` or depfiles.
fn stage_cell_bins(arch: Arch) -> bool {
    if arch.cell_target_dir() == arch.target() {
        return true; // same dir - nothing to stage (RISC-V)
    }
    let src = PathBuf::from(format!("target/{}/release", arch.cell_target_dir()));
    let dst = PathBuf::from(format!("target/{}/release", arch.target()));
    let Ok(entries) = std::fs::read_dir(&src) else {
        eprintln!("error: cannot read {}", src.display());
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Executables have no extension; skip .d depfiles, dirs, and the
        // build/deps/incremental subdirs.
        if path.is_file() && path.extension().is_none() {
            let name = entry.file_name();
            if let Err(e) = std::fs::copy(&path, dst.join(&name)) {
                eprintln!("error: staging {}: {e}", name.to_string_lossy());
                return false;
            }
        }
    }
    true
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
    // Built for the hard-float cell target and staged like the full build, so
    // the librheo-embed the `librheoproc` kernel embeds is the minimal spine.
    build_librheo(arch, true) && stage_cell_bins(arch)
}

/// Build the `netsmoltcp-demo` bin with the `smoltcp` feature (docs/NETSTACK.md
/// §13, Phase N2c). `build_userland` builds `rheo-net` with default features, so
/// the smoltcp cell - gated behind `required-features = ["smoltcp"]` - is skipped
/// there (the from-scratch stack + every other net demo stay smoltcp-free). This
/// dedicated step builds only that one bin with the feature on, into the same
/// release path the `netsmoltcp` test kernel `include_bytes!`s. Must run after
/// `build_userland` so it does not disturb the other demo bins.
fn build_smoltcp_demo(arch: Arch) -> bool {
    println!(
        "[xtask] building netsmoltcp-demo (--features smoltcp) for {}",
        arch.name()
    );
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "-p",
        "rheo-net",
        "--bin",
        "netsmoltcp-demo",
        "--features",
        "smoltcp,proof",
        "--release",
        "--target",
        arch.target(),
        "-Zbuild-std=core,alloc,compiler_builtins",
        "-Zbuild-std-features=compiler-builtins-mem",
    ]);
    matches!(cmd.status().map(|s| s.success()), Ok(true))
}

/// Build the `netcrypto-demo` bin with the `crypto` feature (docs/NETSTACK.md §3,
/// Phase N3a). Like `build_smoltcp_demo`, `build_userland` (default features)
/// skips it via `required-features = ["crypto"]`, so this dedicated step builds
/// just that one bin with the RustCrypto tree, into the same release path the
/// `netcrypto` test kernel `include_bytes!`s.
///
/// The `--cfg *_force_soft` / `curve25519_dalek_backend="serial"` flags force the
/// **software** AES / GHASH / curve25519 backends: `x86_64-unknown-none`'s default
/// target features otherwise select intrinsics backends (AES-NI / CLMUL / AVX2)
/// that miscompile under LLVM ("Do not know how to split the result of this
/// operator"). The soft backends are the scalar portable path our doctrine wants
/// anyway (docs/NETSTACK.md §3), applied uniformly on all three ISAs so behaviour
/// is identical everywhere. Setting env RUSTFLAGS replaces `.cargo/config.toml`'s
/// `[target]` flags, so x86_64's relocation/code-model are re-supplied here.
fn build_crypto_demo(arch: Arch) -> bool {
    println!(
        "[xtask] building netcrypto-demo (--features crypto, soft backends) for {}",
        arch.name()
    );
    let mut rustflags = String::from(
        "--cfg aes_force_soft --cfg polyval_force_soft \
         --cfg curve25519_dalek_backend=\"serial\"",
    );
    // x86_64-unknown-none needs its config.toml model flags re-supplied (env
    // RUSTFLAGS overrides, does not merge with, the [target] config rustflags).
    if arch == Arch::X86_64 {
        rustflags.push_str(" -C relocation-model=static -C code-model=small");
    }
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "-p",
        "rheo-net",
        "--bin",
        "netcrypto-demo",
        "--features",
        "crypto",
        "--release",
        "--target",
        arch.target(),
        "-Zbuild-std=core,alloc,compiler_builtins",
        "-Zbuild-std-features=compiler-builtins-mem",
    ]);
    cmd.env("RUSTFLAGS", rustflags);
    matches!(cmd.status().map(|s| s.success()), Ok(true))
}

/// Build the `nettls-demo` bin with the `tls` feature (docs/NETSTACK.md §15,
/// Phase N3b). The `tls` feature implies `crypto`, so the same force-soft AES /
/// GHASH / curve25519 backend cfgs as `build_crypto_demo` are needed (the
/// intrinsics backends miscompile under LLVM on `x86_64-unknown-none`). Like the
/// crypto demo, `build_userland` (default features) skips it via
/// `required-features = ["tls"]`, so this dedicated step builds just that bin into
/// the release path the `nettls` test kernel `include_bytes!`s. Must run after
/// `build_userland`.
fn build_tls_demo(arch: Arch) -> bool {
    println!(
        "[xtask] building nettls-demo (--features tls, soft backends) for {}",
        arch.name()
    );
    let mut rustflags = String::from(
        "--cfg aes_force_soft --cfg polyval_force_soft \
         --cfg curve25519_dalek_backend=\"serial\"",
    );
    if arch == Arch::X86_64 {
        rustflags.push_str(" -C relocation-model=static -C code-model=small");
    }
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "-p",
        "rheo-net",
        "--bin",
        "nettls-demo",
        "--features",
        "tls,proof",
        "--release",
        "--target",
        arch.target(),
        "-Zbuild-std=core,alloc,compiler_builtins",
        "-Zbuild-std-features=compiler-builtins-mem",
    ]);
    cmd.env("RUSTFLAGS", rustflags);
    matches!(cmd.status().map(|s| s.success()), Ok(true))
}

/// Build the `nethttp-demo` bin with the `tls` feature (docs/NETSTACK.md §19,
/// Phase N5a). The HTTP codec itself needs no feature at all - it is in the
/// always-compiled half of rheo-net - but the proof cell also runs one HTTP/1.1
/// exchange **through the TLS 1.3 record layer**, so it is gated on `tls` and needs
/// the same force-soft AES / GHASH / curve25519 backend cfgs as `build_tls_demo`
/// (the intrinsics backends miscompile under LLVM on `x86_64-unknown-none`). Like
/// the crypto/TLS demos, `build_userland` (default features) skips it via
/// `required-features = ["tls"]`, so this dedicated step builds just that bin into
/// the release path the `nethttp` test kernel `include_bytes!`s. Must run after
/// `build_userland`.
fn build_http_demo(arch: Arch) -> bool {
    println!(
        "[xtask] building nethttp-demo (--features tls, soft backends) for {}",
        arch.name()
    );
    let mut rustflags = String::from(
        "--cfg aes_force_soft --cfg polyval_force_soft \
         --cfg curve25519_dalek_backend=\"serial\"",
    );
    if arch == Arch::X86_64 {
        rustflags.push_str(" -C relocation-model=static -C code-model=small");
    }
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "-p",
        "rheo-net",
        "--bin",
        "nethttp-demo",
        "--features",
        "tls,proof",
        "--release",
        "--target",
        arch.target(),
        "-Zbuild-std=core,alloc,compiler_builtins",
        "-Zbuild-std-features=compiler-builtins-mem",
    ]);
    cmd.env("RUSTFLAGS", rustflags);
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

    /// The g++ that links a dynamic C++ binary for the four-library `dcpp` fixture.
    /// If it is absent (no cross-g++ for an ISA), the C++ compile fails and the
    /// fixture becomes a placeholder, so `linuxdyn`'s C++ phase skips-with-reason.
    fn cxx(self) -> &'static str {
        match self {
            Arch::X86_64 => "g++",
            Arch::Aarch64 => "aarch64-linux-gnu-g++",
            Arch::Riscv64 => "riscv64-linux-gnu-g++",
        }
    }

    /// The toolchain C++ runtime libs `dcpp` links: `(libstdc++.so.6, libgcc_s.so.1)`.
    /// Copied beside libc for the four-library dynamic fixture; a missing one makes
    /// the C++ phase skip-with-reason for that ISA.
    fn cpp_runtime_libs(self) -> (&'static str, &'static str) {
        match self {
            Arch::X86_64 => (
                "/usr/lib/x86_64-linux-gnu/libstdc++.so.6",
                "/lib/x86_64-linux-gnu/libgcc_s.so.1",
            ),
            Arch::Aarch64 => (
                "/usr/aarch64-linux-gnu/lib/libstdc++.so.6",
                "/usr/aarch64-linux-gnu/lib/libgcc_s.so.1",
            ),
            Arch::Riscv64 => (
                "/usr/riscv64-linux-gnu/lib/libstdc++.so.6",
                "/usr/riscv64-linux-gnu/lib/libgcc_s.so.1",
            ),
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

    /// The absolute path a dynamic binary's `PT_INTERP` names for this ISA (the
    /// `ld-linux-*.so` path, verified with `readelf -p .interp`). Used to place
    /// the interpreter at the right path inside the `linuxdisk` ext4 image so
    /// ld.so is found on the mounted disk exactly as on Linux.
    fn interp_path(self) -> &'static str {
        match self {
            Arch::X86_64 => "/lib64/ld-linux-x86-64.so.2",
            Arch::Aarch64 => "/lib/ld-linux-aarch64.so.1",
            Arch::Riscv64 => "/lib/ld-linux-riscv64-lp64d.so.1",
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

    // More contexts than the old fixed ceiling (docs/SUBSTRATE.md pillar 1): 12
    // simultaneously-live threads, where the pre-migration `MAX_THREADS = 8` array
    // allowed 7. Same static-glibc recipe; the 4-thread L4 fixture above is left
    // untouched so its proof still holds unedited.
    let mut many = Command::new("cargo");
    many.args([
        "build",
        "--manifest-path",
        "tests/linux-fixtures/manythreads/Cargo.toml",
        "--release",
        "--target",
        arch.linux_gnu_target(),
    ]);
    many.env("RUSTFLAGS", &rustflags);
    if !matches!(many.status().map(|s| s.success()), Ok(true)) {
        eprintln!(
            "[xtask] many-context fixture build failed for {}",
            arch.name()
        );
        return false;
    }

    // A real JavaScript engine (pure-Rust `boa`), same static-glibc recipe - the
    // on-goal proxy for Node/Claude Code: a language runtime (parser + bytecode VM
    // + heap + GC) run unmodified under the Linux personality (docs/LINUX-COMPAT.md,
    // the `linuxjs` fixture in `linuxrun`). ~10 MB, so it also exercises the
    // demand-paged loader at scale.
    let mut js = Command::new("cargo");
    js.args([
        "build",
        "--manifest-path",
        "tests/linux-fixtures/jsdemo/Cargo.toml",
        "--release",
        "--target",
        arch.linux_gnu_target(),
    ]);
    js.env("RUSTFLAGS", &rustflags);
    if !matches!(js.status().map(|s| s.success()), Ok(true)) {
        eprintln!(
            "[xtask] boa JS-engine fixture build failed for {}",
            arch.name()
        );
        return false;
    }

    // The **tile framework's own kernels in a Linux binary** (docs/TILES.md 13.4b):
    // `fmath`/`kernels`/`attn` are `#[path]`-included from librheo, so the same source
    // the librheo executor and the kernel's compute engine compile is built here as a
    // static-glibc Linux program instead. The `linuxtile` phase compares its output
    // hashes against the librheo cell's, byte for byte.
    let mut tl = Command::new("cargo");
    tl.args([
        "build",
        "--manifest-path",
        "tests/linux-fixtures/tilelinux/Cargo.toml",
        "--release",
        "--target",
        arch.linux_gnu_target(),
    ]);
    tl.env("RUSTFLAGS", &rustflags);
    if !matches!(tl.status().map(|s| s.success()), Ok(true)) {
        eprintln!(
            "[xtask] tile-in-Linux fixture build failed for {}",
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
    /// Fixtures needing no extra link arguments (most of them).
    const NO_EXTRA: &[&str] = &[];
    for (src, bin, extra) in [
        ("sig_raise.c", "sig_raise", NO_EXTRA),
        ("sig_segv.c", "sig_segv", NO_EXTRA),
        ("sig_dfl.c", "sig_dfl", NO_EXTRA),
        // FP/SIMD across a handler (docs/SUBSTRATE.md S4): the interrupted
        // vector registers must survive a handler that clobbers them.
        ("sig_fp.c", "sig_fp", NO_EXTRA),
        ("procdemo.c", "procdemo", NO_EXTRA),
        ("cecho.c", "cecho", NO_EXTRA),
        ("rsh.c", "rsh", NO_EXTRA),
        // AF_UNIX (L8, docs/LINUX-COMPAT.md): socketpair+fork + bind/listen/
        // connect/accept, the `linuxunix` proof.
        ("af_unix.c", "af_unix", NO_EXTRA),
        // The personality's GLOBAL registries under concurrent load (docs/SMP.md 10.2):
        // pipes and eventfds allocated/used/freed in a tight loop, every value keyed on
        // the caller's own pid so another process's bytes are detected rather than
        // tolerated. The `smp` kernel runs two of these on two cores, which is what makes
        // `linux::plock` testable rather than merely present.
        ("regstress.c", "regstress", NO_EXTRA),
        // AF_INET/AF_INET6 loopback (L8-INET, docs/LINUX-COMPAT.md): TCP+UDP+epoll
        // over 127.0.0.1 and TCP over ::1, the `linuxinet` proof.
        ("inet.c", "inet", NO_EXTRA),
        // Remote INET over the NIC (rheo-net N4b, docs/NETSTACK.md N4b): a real
        // DNS round trip to SLIRP's resolver + a real remote TCP connect, the
        // `linuxnet` proof.
        ("inetremote.c", "inetremote", NO_EXTRA),
        // Name resolution through glibc's own resolver (docs/NETSTACK.md 18):
        // `getaddrinfo` over the seeded /etc/{nsswitch.conf,hosts,resolv.conf},
        // the second `linuxnet` phase.
        ("resolve.c", "resolve", NO_EXTRA),
        // `fcntl`'s honesty (docs/LINUX-COMPAT.md, the `fcntl` row): unimplemented
        // commands refused, O_NONBLOCK honoured, F_GETFL real, FD_CLOEXEC closed
        // across execve. Part of the `linuxproc` proof (it execve's itself).
        ("fcntlx.c", "fcntlx", NO_EXTRA),
        ("lstatx.c", "lstatx", NO_EXTRA),
        // Cross-process signalling + `/proc/self/exe` (docs/ARCHITECTURE-DEBT.md 4):
        // `kill` used to refuse any pid but our own and silently self-target on
        // pid 0/-1, and `readlinkat` was a hardcoded -ENOENT. Part of the
        // `linuxproc` proof (it execve's itself for the exe-path phase).
        ("killx.c", "killx", NO_EXTRA),
        // The mmap region is bounded and MAP_FIXED cannot replace the kernel's
        // rings (docs/ARCHITECTURE-DEBT.md 4, blocker 2): the cursor used to run
        // out of its region, through the queue and into ld.so, silently. Part of
        // the `linuxproc` proof.
        ("mmapx.c", "mmapx", NO_EXTRA),
        // `sched_yield` must cross processes, not only a cell's own contexts
        // (docs/ARCHITECTURE-DEBT.md 4): a single-threaded yielder had no ready
        // sibling context, so the call returned immediately and a forked child
        // could starve its parent. Part of the `linuxproc` proof.
        ("yieldx.c", "yieldx", NO_EXTRA),
        // A futex wait that must END BY ITSELF (docs/LINUX-COMPAT.md L4, the
        // `futex` row): `pthread_cond_timedwait` on a never-signalled condvar.
        // Part of the `linuxthreads` proof.
        ("condwait.c", "condwait", NO_EXTRA),
        // `poll`/`epoll_wait`/`nanosleep` truth + creation-time O_NONBLOCK
        // (docs/ARCHITECTURE-DEBT.md 2.4): the `linuxpoll` proof.
        ("pollx.c", "pollx", NO_EXTRA),
        // A wait that can never end: the scheduler's deadlock diagnostic, the
        // second `linuxpoll` phase.
        ("polldead.c", "polldead", NO_EXTRA),
        // timerfd - the libuv event-loop timer source (docs/LINUX-COMPAT.md
        // L8-TIMERFD): a blocking read parks on the deadline, and epoll_wait wakes
        // on expiry. The third `linuxpoll` phase.
        ("timerx.c", "timerx", NO_EXTRA),
        // The libuv event-loop core: one epoll set multiplexing a periodic timerfd,
        // an eventfd, and a pipe at once - proving the three wake sources compose
        // (docs/LINUX-COMPAT.md L8-TIMERFD). The fourth `linuxpoll` phase.
        ("uvloop.c", "uvloop", NO_EXTRA),
        // The loader must size the stack from **PT_GNU_STACK**, not a fixed
        // constant (docs/ARCHITECTURE-DEBT.md 4.0, blocker 1). Linked with
        // `-z stacksize` so its own header asks for more than the old 8 MiB
        // default, then it touches that much stack. Part of `linuxproc`.
        // 12 MiB of PT_GNU_STACK, above the loader's old fixed 8 MiB default.
        // The spelling matters: GNU ld wants `stack-size`, lld wants `stacksize`,
        // and ld *ignores* the one it does not know with a warning, not an error -
        // so the wrong spelling links fine and produces a p_memsz of 0.
        ("stackx.c", "stackx", &["-Wl,-z,stack-size=12582912"]),
        ("sysx.c", "sysx", NO_EXTRA),
        ("cpulist.c", "cpulist", NO_EXTRA),
        ("procstat.c", "procstat", NO_EXTRA),
        ("cputopo.c", "cputopo", NO_EXTRA),
        ("numatopo.c", "numatopo", NO_EXTRA),
        ("mmapdp.c", "mmapdp", NO_EXTRA),
        ("cowfork.c", "cowfork", NO_EXTRA),
        // Two single-context processes spinning at once, which is the only shape
        // that reaches `linux::proc::preempt_cell` - the move-to-another-cell arm.
        // The last `linuxproc` phase.
        ("preemptfork.c", "preemptfork", NO_EXTRA),
        // A directory fd used by-fd in a forked child - the only shape that notices
        // whether `fork` deep-copies the funded fd-path table.
        ("forkdir.c", "forkdir", NO_EXTRA),
    ] {
        let mut sc = Command::new(cc);
        sc.arg("-static").arg("-no-pie");
        sc.args([
            &format!("tests/linux-fixtures/{src}"),
            "-o",
            &format!("{out_dir}/{bin}"),
        ]);
        // Per-fixture link arguments. Most need none; a fixture that has to
        // *record something in its own ELF headers* (a PT_GNU_STACK size) can
        // only do it at link time, and a one-off special case here would be the
        // start of a second fixture-building path.
        sc.args(extra.iter());
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
    build_dyn_disk_fixture(arch, &out_dir);
    build_node_disk_fixture(arch, &out_dir);
    build_bun_disk_fixture(arch, &out_dir);
    build_claude_disk_fixture(arch, &out_dir);

    build_coreutils_fixture(arch)
}

/// Does an external tool exist on PATH? (xtask has zero deps, so probe by
/// spawning it - `.output()` errors only when the binary is not found.)
fn have_tool(name: &str) -> bool {
    Command::new(name).arg("-V").output().is_ok()
}

/// Build the `linuxdisk` fixture: a real ext4 image (`mkfs.ext4` + `debugfs`)
/// holding the dynamic hello at `/bin/dhello`, its interpreter at the ISA's
/// `PT_INTERP` path, and `/lib/libc.so.6` - so the `linuxdisk` test mounts it off
/// a live virtio-blk disk (through `ext4fs`/`ext4plus` + the block cache) and
/// `execve`s a dynamically-linked binary straight from ext4 (GOAL-DISK-2b). The
/// image is gitignored, like the `.so` fixtures it embeds.
///
/// Skipped-with-reason - a small zeroed placeholder image, so QEMU's `-drive`
/// still has a file and the test detects a non-ext4 disk and skips - when the
/// runtime libs are placeholders (the toolchain `.so`s were absent) or
/// `mkfs.ext4`/`debugfs` are not installed. Never fails the build.
fn build_dyn_disk_fixture(arch: Arch, out_dir: &str) {
    let img = format!("{out_dir}/dyn-disk.img");
    let ld = format!("{out_dir}/ld.so");
    let libc = format!("{out_dir}/libc.so.6");
    let dhello = format!("{out_dir}/dhello");
    let placeholder = |img: &str| {
        let _ = std::fs::write(img, vec![0u8; 64 * 1024]);
    };
    let big = |p: &str| {
        std::fs::metadata(p)
            .map(|m| m.len() > 4096)
            .unwrap_or(false)
    };

    if !(big(&libc) && big(&ld)) {
        eprintln!(
            "[xtask] SKIP linuxdisk image for {}: runtime ld.so/libc not available \
             (linuxdisk will skip this ISA)",
            arch.name()
        );
        placeholder(&img);
        return;
    }
    if !(have_tool("mkfs.ext4") && have_tool("debugfs")) {
        eprintln!(
            "[xtask] SKIP linuxdisk image for {}: mkfs.ext4/debugfs not installed \
             (linuxdisk will skip this ISA)",
            arch.name()
        );
        placeholder(&img);
        return;
    }

    // A driver-parseable ext4 (the gen-ext4.sh flags): 1 KiB blocks, no
    // journal/csum/64bit/htree/resize. 8 MiB holds libc (~2 MB) with slack.
    let _ = std::fs::remove_file(&img);
    let ok = matches!(
        Command::new("dd")
            .args([
                "if=/dev/zero",
                &format!("of={img}"),
                "bs=1024",
                "count=8192"
            ])
            .output()
            .map(|o| o.status.success()),
        Ok(true)
    ) && matches!(
        Command::new("mkfs.ext4")
            .args([
                "-q",
                "-b",
                "1024",
                "-O",
                "^has_journal,^metadata_csum,^64bit,^resize_inode,^dir_index,extent",
                "-F",
                &img,
            ])
            .output()
            .map(|o| o.status.success()),
        Ok(true)
    );
    if !ok {
        placeholder(&img);
        return;
    }

    // debugfs dest paths are relative to root (no leading slash). The
    // interpreter goes at its PT_INTERP path (/lib64/... on x86, /lib/... else).
    let interp_rel = arch.interp_path().trim_start_matches('/');
    let debugfs = |cmd: &str| {
        let _ = Command::new("debugfs")
            .args(["-w", "-R", cmd, &img])
            .output();
    };
    debugfs("mkdir /bin");
    debugfs("mkdir /lib");
    if arch.interp_path().starts_with("/lib64/") {
        debugfs("mkdir /lib64");
    }
    debugfs(&format!("write {dhello} bin/dhello"));
    debugfs(&format!("write {libc} lib/libc.so.6"));
    debugfs(&format!("write {ld} {interp_rel}"));
    println!(
        "[xtask] built linuxdisk ext4 image for {} ({img}; interp {})",
        arch.name(),
        arch.interp_path()
    );
}

/// Build the `linuxnode` fixture (GOAL-NODE): the real x86-64 `node` binary
/// (~124 MB) + its glibc + libstdc++ shared-library set on an ext4 image, streamed
/// off a live virtio-blk disk by the `linuxnode` test. Thin caller of
/// [`build_runtime_disk_fixture`].
fn build_node_disk_fixture(arch: Arch, out_dir: &str) {
    // A real multi-file program resolving an **npm-style package**, staged on the host
    // and written into the image below. `/app/main.js` does `require('greeter')` - a
    // *bare specifier*, which drives Node's full resolution: walk `node_modules`, read
    // the package's `package.json`, follow its `main` field to `index.js`. That is how
    // an npm-installed dependency loads, so it exercises the resolver npm and Claude
    // Code stand on, reading a package's metadata and entry file off the live disk
    // rather than evaluating an inline `-e` string.
    //
    // Ported from the `claude-md-test-pipelines` branch. A cherry-pick was not possible:
    // that branch's `linuxnode` predates this one's JIT-enabled run, so its version of
    // the test would have reverted `node` to `--jitless`.
    if arch == Arch::X86_64 {
        let main_js = "const greeter = require('greeter');\n\
             const path = require('path');\n\
             console.log(path.basename('/bin/rheo') + ':' + greeter.answer([10, 20, 12]));\n";
        let greeter_index =
            "module.exports = { answer: (arr) => arr.reduce((a, b) => a + b, 0) };\n";
        let greeter_pkg =
            "{ \"name\": \"greeter\", \"version\": \"1.0.0\", \"main\": \"index.js\" }\n";
        let _ = std::fs::write(format!("{out_dir}/node-main.js"), main_js);
        let _ = std::fs::write(format!("{out_dir}/node-greeter-index.js"), greeter_index);
        let _ = std::fs::write(format!("{out_dir}/node-greeter-pkg.json"), greeter_pkg);
    }
    let main_src = format!("{out_dir}/node-main.js");
    let gi_src = format!("{out_dir}/node-greeter-index.js");
    let gp_src = format!("{out_dir}/node-greeter-pkg.json");
    build_runtime_disk_fixture(
        arch,
        out_dir,
        "linuxnode",
        "node-disk.img",
        "/opt/node22/bin/node",
        "node",
        &[
            "libc.so.6",
            "libm.so.6",
            "libdl.so.2",
            "libpthread.so.0",
            "libstdc++.so.6",
            "libgcc_s.so.1",
        ],
        &[
            (main_src.as_str(), "app/main.js"),
            (gi_src.as_str(), "app/node_modules/greeter/index.js"),
            (gp_src.as_str(), "app/node_modules/greeter/package.json"),
        ],
    );
}

/// Build the `linuxbun` fixture (GOAL-BUN): the real x86-64 `bun` binary (~99 MB,
/// JavaScriptCore) + its glibc set on an ext4 image, streamed off a live
/// virtio-blk disk by the `linuxbun` test. Thin caller of
/// [`build_runtime_disk_fixture`] (bun needs no libstdc++/libgcc_s).
fn build_bun_disk_fixture(arch: Arch, out_dir: &str) {
    build_runtime_disk_fixture(
        arch,
        out_dir,
        "linuxbun",
        "bun-disk.img",
        "/root/.bun/bin/bun",
        "bun",
        &["libc.so.6", "libm.so.6", "libdl.so.2", "libpthread.so.0"],
        // The tile shared library and the JS that calls it through `bun:ffi`
        // (docs/TILES.md 13.4d): the last step of "a JS runtime calls a tile kernel".
        // `libgcc_s.so.1` because a Rust `cdylib` needs the unwinder even at
        // `panic = "abort"` - the fact the `dlopen` probe turned up.
        &[
            (
                "tests/linux-fixtures/build/x86_64/libtileso.so",
                "lib/libtileso.so",
            ),
            ("/lib/x86_64-linux-gnu/libgcc_s.so.1", "lib/libgcc_s.so.1"),
            ("tests/linux-fixtures/tileffi.js", "bin/tileffi.js"),
        ],
    );
}

/// Build the `linuxclaude` fixture (GOAL-CLAUDE): the real **Claude Code** binary
/// (~275 MB) + its glibc set on an ext4 image, streamed off a live virtio-blk disk by
/// the `linuxclaude` test.
///
/// This is the workload docs/ARCHITECTURE-DEBT.md 4.0 measured the tree against and
/// named as the target: a Bun-compiled single-file executable, so it is the same
/// JavaScriptCore runtime `linuxbun` proves, at four times the size and with its
/// whole application bundled in. It needs `librt` on top of bun's set. Thin caller of
/// [`build_runtime_disk_fixture`].
fn build_claude_disk_fixture(arch: Arch, out_dir: &str) {
    build_runtime_disk_fixture(
        arch,
        out_dir,
        "linuxclaude",
        "claude-disk.img",
        "/opt/claude-code/bin/claude",
        "claude",
        &[
            "libc.so.6",
            "libm.so.6",
            "libdl.so.2",
            "libpthread.so.0",
            "librt.so.1",
        ],
        &[],
    );
}

/// Build a **disk-streamed language-runtime** ext4 fixture (GOAL-NODE / GOAL-BUN,
/// docs/LINUX-COMPAT.md): a real ext4 image holding a real x86-64 dynamic binary at
/// `/bin/<dst>`, its `ld-linux-x86-64.so.2` at the PT_INTERP path, and its host
/// glibc `.so` set under `/lib` - so the matching test mounts it off a live
/// virtio-blk disk (`ext4fs`/`ext4plus` + the block cache) and `execve`s the binary
/// straight from ext4, streamed demand-paged, none resident whole. The image is
/// gitignored (a ~100 MB binary is never committed).
///
/// **x86-64 only.** The binaries are x86-64 ELFs; for arm64/riscv64 no image is
/// written and no `-drive` is attached, so the test skips-with-reason. On x86-64,
/// if the binary is absent (e.g. CI), any host glibc `.so` cannot be found, or
/// `mkfs.ext4`/`debugfs` are missing, a small placeholder image is written so
/// QEMU's `-drive` still has a file and the test detects a non-ext4 disk and skips
/// - CI stays green. Never fails the build.
// Eight arguments because an image is eight independent facts about itself; bundling
// them in a struct would move the same list one line up with no caller made simpler.
#[allow(clippy::too_many_arguments)]
fn build_runtime_disk_fixture(
    arch: Arch,
    out_dir: &str,
    test: &str,
    img_name: &str,
    binary: &str,
    dst: &str,
    libs: &[&str],
    // Extra `(host source, image destination)` files. The Bun image uses this to carry
    // the tile shared library and the JS that calls it through `bun:ffi`
    // (docs/TILES.md 13.4d); every other image passes none.
    extras: &[(&str, &str)],
) {
    if arch != Arch::X86_64 {
        return; // x86-64 binaries only; no drive attached elsewhere, test skips
    }
    let img = format!("{out_dir}/{img_name}");
    let placeholder = |img: &str| {
        let _ = std::fs::write(img, vec![0u8; 64 * 1024]);
    };

    // The interpreter goes at its PT_INTERP path; the DT_NEEDED libraries under
    // /lib, found via LD_LIBRARY_PATH=/lib:/lib64.
    const INTERP_SRC: &str = "/lib64/ld-linux-x86-64.so.2";
    const LIBDIR: &str = "/lib/x86_64-linux-gnu";

    let exists = |p: &str| std::path::Path::new(p).exists();
    if !exists(binary)
        || !exists(INTERP_SRC)
        || libs.iter().any(|l| !exists(&format!("{LIBDIR}/{l}")))
    {
        eprintln!(
            "[xtask] SKIP {test} image for x86_64: {binary} or host glibc not present \
             ({test} will skip - CI/other hosts unaffected)"
        );
        placeholder(&img);
        return;
    }
    if !(have_tool("mkfs.ext4") && have_tool("debugfs")) {
        eprintln!("[xtask] SKIP {test} image for x86_64: mkfs.ext4/debugfs not installed");
        placeholder(&img);
        return;
    }

    // Size the ext4 from the payload rather than to a constant (same
    // driver-parseable flags as the linuxdisk image, 1 KiB blocks).
    //
    // It used to be a flat 200 MiB, chosen for a ~124 MB `node` plus ~10 MB of
    // libraries. The Claude Code binary is ~275 MB, so it did not fit - and the
    // failure was not a build error but `execve` refusing at boot with
    // "streaming execve of the runtime binary", which says nothing about the
    // image being too small. A size derived from the file cannot get this wrong
    // for the next, larger binary either.
    let payload_kib = std::fs::metadata(binary)
        .map(|m| m.len() / 1024)
        .unwrap_or(0);
    // Libraries plus ext4 metadata plus slack; 1.25x the binary with a 64 MiB floor
    // is generous next to the cost of an unexplained boot failure.
    let count_kib = (payload_kib + payload_kib / 4 + 65_536).max(204_800);
    let _ = std::fs::remove_file(&img);
    let ok = matches!(
        Command::new("dd")
            .args([
                "if=/dev/zero",
                &format!("of={img}"),
                "bs=1024",
                &format!("count={count_kib}"),
            ])
            .output()
            .map(|o| o.status.success()),
        Ok(true)
    ) && matches!(
        Command::new("mkfs.ext4")
            .args([
                "-q",
                "-b",
                "1024",
                "-O",
                "^has_journal,^metadata_csum,^64bit,^resize_inode,^dir_index,extent",
                "-F",
                &img,
            ])
            .output()
            .map(|o| o.status.success()),
        Ok(true)
    );
    if !ok {
        placeholder(&img);
        return;
    }

    let debugfs = |cmd: &str| {
        let _ = Command::new("debugfs")
            .args(["-w", "-R", cmd, &img])
            .output();
    };
    debugfs("mkdir /bin");
    debugfs("mkdir /lib");
    debugfs("mkdir /lib64");
    debugfs(&format!("write {binary} bin/{dst}"));
    debugfs(&format!("write {INTERP_SRC} lib64/ld-linux-x86-64.so.2"));
    for l in libs {
        debugfs(&format!("write {LIBDIR}/{l} lib/{l}"));
    }
    // Create every parent directory of every destination, **shallowest first**:
    // `debugfs mkdir` does not create parents, so a nested payload like
    // `app/node_modules/greeter/index.js` needs each level made in order. Ported from
    // the `claude-md-test-pipelines` branch, which needed it for the same reason.
    let mut dirs: Vec<String> = Vec::new();
    for (_, dest) in extras {
        let parts: Vec<&str> = dest.split('/').collect();
        for i in 1..parts.len() {
            let d = parts[..i].join("/");
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    dirs.sort_by_key(|d| d.matches('/').count());
    for d in &dirs {
        debugfs(&format!("mkdir /{d}"));
    }
    for (src, dstpath) in extras {
        if exists(src) {
            debugfs(&format!("write {src} {dstpath}"));
        } else {
            eprintln!("[xtask] {test} image: extra {src} absent - the phase using it will skip");
        }
    }
    seed_runtime_procfs(out_dir, &debugfs);
    println!("[xtask] built {test} ext4 image for x86_64 ({img}; real {dst} + glibc set)");
}

/// Seed the small `/proc` and `/sys` files a language runtime reads at startup.
///
/// These are **not** a procfs. They are a handful of static text files with the values
/// this kernel genuinely has, placed on the image so a runtime's startup probes get an
/// answer instead of `ENOENT`. Which ones matter was measured, not guessed: the Linux
/// personality now prints the path of every refused `open`, and the real Bun binary was
/// observed probing exactly these before it aborted (docs/LINUX-COMPAT.md, GOAL-BUN).
///
/// Each value is true of this kernel:
///
/// - `/proc/sys/vm/overcommit_memory` = `0` (heuristic). Accurate: `mmap` reserves
///   without committing and frames arrive on fault (demand paging), which is what
///   heuristic overcommit describes.
/// - `/proc/sys/vm/mmap_min_addr` = `65536`, the conventional floor, and true here -
///   nothing is mapped below it.
/// - `/proc/self/cgroup` = `0::/`. There are no cgroups, and that is the cgroup-v2
///   spelling of "the root, unconstrained" - the answer an unconstrained Linux process
///   gets, not a placeholder.
///
/// Deliberately **not** provided: `/proc/self/maps`, which Bun also probes. A static
/// file there would be a fabricated memory map, and the honest version is generated
/// from the cell's own VMA list by the personality - real work, named in
/// docs/LINUX-COMPAT.md rather than faked here. Same for
/// `/sys/devices/system/cpu/{online,present,possible}`: those were seeded as the
/// constant `0-0`, and are now **synthesized by the personality** from
/// `smp::online_count()`, for exactly the reason `maps` is - a static topology file is
/// a fabricated machine, and libuv sizes its thread pool from it. Same for
/// **`/proc/stat`**, which was seeded here with a single `cpu0` line whatever the boot's
/// CPU count was: counting `cpuN` lines is how the portable readers count CPUs, so the
/// count now comes from `smp::online_count()` too, and the seeded file is gone so a
/// constant cannot answer first. Its jiffy fields stay 0 in the synthesized version,
/// which is the honest answer and not a placeholder - this kernel keeps no per-CPU
/// user/system/idle accounting, and a fabricated breakdown is what a reader would
/// compute a CPU percentage from. Same for
/// `/etc/localtime`: glibc falls back to UTC on `ENOENT`, which is correct, since this
/// kernel has no timezone database and inventing one would be worse than the fallback.
fn seed_runtime_procfs(out_dir: &str, debugfs: &dyn Fn(&str)) {
    debugfs("mkdir /proc");
    debugfs("mkdir /proc/self");
    debugfs("mkdir /proc/sys");
    debugfs("mkdir /proc/sys/vm");
    debugfs("mkdir /sys");
    debugfs("mkdir /sys/devices");
    debugfs("mkdir /sys/devices/system");
    debugfs("mkdir /sys/devices/system/cpu");
    debugfs("mkdir /sys/fs");
    debugfs("mkdir /sys/fs/cgroup");
    let files: &[(&str, &str)] = &[
        ("overcommit_memory", "0\n"),
        ("mmap_min_addr", "65536\n"),
        ("cgroup", "0::/\n"),
        // cgroup-v2 memory limits. `max` is the v2 spelling of "no limit", which is
        // true: nothing constrains a cell's memory but its own frame budget, and a
        // runtime that reads these sizes its heap from them (Bun does, having been
        // told `0::/` above). A number would be a fabricated limit.
        ("memory_max", "max\n"),
        ("memory_high", "max\n"),
    ];
    for (name, body) in files {
        let path = format!("{out_dir}/procseed-{name}");
        if std::fs::write(&path, body).is_err() {
            continue;
        }
        let dest = match *name {
            "overcommit_memory" => "proc/sys/vm/overcommit_memory",
            "mmap_min_addr" => "proc/sys/vm/mmap_min_addr",
            "cgroup" => "proc/self/cgroup",
            "memory_max" => "sys/fs/cgroup/memory.max",
            "memory_high" => "sys/fs/cgroup/memory.high",
            _ => "proc/stat",
        };
        debugfs(&format!("write {path} {dest}"));
    }
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

    // The **dlopen probe** (docs/TILES.md 13.4c): a dynamic PIE that `dlopen`s a
    // shared library exporting the tile framework's GEMM and calls it. This is the
    // question a JS runtime's FFI reduces to - `bun:ffi` and Node's N-API addons are
    // both `dlopen` + `dlsym` + an indirect call - so answering it for C answers
    // whether a tile kernel is reachable from JavaScript at all, and a failure is a
    // fact about the personality rather than about JavaScript. Not fatal: a
    // placeholder makes the phase skip.
    let so_src = "tests/linux-fixtures/tileso";
    let mut so = Command::new("cargo");
    so.args([
        "build",
        "--manifest-path",
        &format!("{so_src}/Cargo.toml"),
        "--release",
        "--target",
        arch.linux_gnu_target(),
    ]);
    // **Not** the static fixtures' flags. `+crt-static -no-pie` cannot produce a shared
    // object at all - a `.so` is position-independent and dynamically linked by
    // definition - so this one gets only the cross linker. Discovered by the cdylib
    // failing to link for aarch64 while succeeding on the host, where no cross flags
    // apply.
    so.env("RUSTFLAGS", format!("-C linker={cc}"));
    let so_ok = matches!(so.status().map(|s| s.success()), Ok(true));
    let so_built = format!(
        "{so_src}/target/{}/release/libtileso.so",
        arch.linux_gnu_target()
    );
    let so_dst = format!("{out_dir}/libtileso.so");
    if so_ok && std::fs::copy(&so_built, &so_dst).is_ok() {
        let mut dl = Command::new(cc);
        dl.args([
            "tests/linux-fixtures/dlopentile.c",
            "-o",
            &format!("{out_dir}/dlopentile"),
        ]);
        if !matches!(dl.status().map(|s| s.success()), Ok(true)) {
            eprintln!("[xtask] dlopen probe build failed for {}", arch.name());
            let _ = std::fs::write(format!("{out_dir}/dlopentile"), [0u8]);
        }
    } else {
        eprintln!(
            "[xtask] tile shared library unavailable for {} - dlopen probe skipped",
            arch.name()
        );
        let _ = std::fs::write(format!("{out_dir}/dlopentile"), [0u8]);
        let _ = std::fs::write(&so_dst, [0u8]);
    }

    // A second dynamic PIE that links a SECOND shared library (libm) besides
    // libc, so `ld.so` must load two libraries and resolve versions across them
    // (the multi-library case, GOAL-DYN-MULTILIB). `-fno-builtin` forces a real
    // libm call rather than a compile-time constant fold. A build failure here is
    // not fatal: a placeholder makes `linuxdyn` skip only the multi-library phase.
    let dmath_dst = format!("{out_dir}/dmath");
    let mut cm = Command::new(cc);
    cm.args([
        "tests/linux-fixtures/dmath.c",
        "-fno-builtin",
        "-o",
        &dmath_dst,
        "-lm",
    ]);
    if !matches!(cm.status().map(|s| s.success()), Ok(true)) {
        eprintln!(
            "[xtask] dmath (multi-library) fixture build failed for {}; \
             linuxdyn skips the multi-library phase",
            arch.name()
        );
        let _ = std::fs::write(&dmath_dst, [0u8]);
    }

    // A third dynamic PIE - a C++ hello linking libstdc++ + libgcc_s + libc
    // (+ libm) - the four-library production shape. Built with g++; a failure
    // (e.g. no cross-g++ for this ISA) writes a placeholder so `linuxdyn` skips
    // only the C++ phase.
    let dcpp_dst = format!("{out_dir}/dcpp");
    let mut cpp = Command::new(arch.cxx());
    cpp.args(["tests/linux-fixtures/dcpp.cpp", "-O2", "-o", &dcpp_dst]);
    let dcpp_ok = matches!(cpp.status().map(|s| s.success()), Ok(true));
    if !dcpp_ok {
        eprintln!(
            "[xtask] dcpp (C++ four-library) fixture build failed for {} \
             (no {}?); linuxdyn skips the C++ phase",
            arch.name(),
            arch.cxx()
        );
        let _ = std::fs::write(&dcpp_dst, [0u8]);
    }
    // Copy libstdc++ + libgcc_s beside libc, or placeholder → C++ phase skips.
    let (libstdcpp_src, libgcc_src) = arch.cpp_runtime_libs();
    let libstdcpp_dst = format!("{out_dir}/libstdc++.so.6");
    let libgcc_dst = format!("{out_dir}/libgcc_s.so.1");
    if dcpp_ok
        && std::fs::copy(libstdcpp_src, &libstdcpp_dst).is_ok()
        && std::fs::copy(libgcc_src, &libgcc_dst).is_ok()
    {
        println!(
            "[xtask] copied C++ runtime ({libstdcpp_src}, {libgcc_src}) for {}",
            arch.name()
        );
    } else {
        if dcpp_ok {
            eprintln!(
                "[xtask] C++ runtime libs not found ({libstdcpp_src}); linuxdyn \
                 skips the C++ phase for {}",
                arch.name()
            );
        }
        let _ = std::fs::write(&libstdcpp_dst, [0u8]);
        let _ = std::fs::write(&libgcc_dst, [0u8]);
    }

    // Copy the real ld.so + libc.so.6 + libm.so.6 out of the toolchain, or
    // skip-with-reason. libm lives beside libc in the same sysroot lib dir.
    let (ld_src, libc_src) = arch.dyn_runtime_libs();
    let libm_src = libc_src.replace("libc.so.6", "libm.so.6");
    let ld_dst = format!("{out_dir}/ld.so");
    let libc_dst = format!("{out_dir}/libc.so.6");
    let libm_dst = format!("{out_dir}/libm.so.6");
    let copied =
        std::fs::copy(ld_src, &ld_dst).is_ok() && std::fs::copy(libc_src, &libc_dst).is_ok();
    if copied {
        println!(
            "[xtask] copied dynamic runtime ({ld_src}, {libc_src}) for {}",
            arch.name()
        );
        if std::fs::copy(&libm_src, &libm_dst).is_ok() {
            println!("[xtask] copied {libm_src} for {}", arch.name());
        } else {
            eprintln!(
                "[xtask] libm.so.6 not found ({libm_src}); linuxdyn skips the \
                 multi-library phase for {}",
                arch.name()
            );
            let _ = std::fs::write(&libm_dst, [0u8]);
        }
    } else {
        eprintln!(
            "[xtask] SKIP dynamic fixture for {}: runtime ld.so/libc not found \
             ({ld_src}); linuxdyn will skip this ISA (static coverage stays)",
            arch.name()
        );
        // 1-byte placeholders so the test still compiles + detects the skip.
        let _ = std::fs::write(&ld_dst, [0u8]);
        let _ = std::fs::write(&libc_dst, [0u8]);
        let _ = std::fs::write(&libm_dst, [0u8]);
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
    // The N2c smoltcp cell (docs/NETSTACK.md §13): built separately with the
    // `smoltcp` feature (the default `build_userland` skips it via required-features).
    if !build_smoltcp_demo(arch) {
        return false;
    }
    // The N3a crypto cell (docs/NETSTACK.md §3): built separately with the
    // `crypto` feature + the force-soft backend cfgs (build_userland skips it via
    // required-features).
    if !build_crypto_demo(arch) {
        return false;
    }
    // The N3b TLS 1.3 cell (docs/NETSTACK.md §15): built separately with the `tls`
    // feature (implies `crypto`) + the same force-soft backend cfgs (build_userland
    // skips it via required-features).
    if !build_tls_demo(arch) {
        return false;
    }
    // The N5a HTTP cell (docs/NETSTACK.md §19): built separately with the `tls`
    // feature (its HTTPS composition needs the record layer) + the same
    // force-soft backend cfgs (build_userland skips it via required-features).
    if !build_http_demo(arch) {
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
        "[xtask] building the smp test kernels (kernel/smp feature) for {}",
        arch.name()
    );
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "-p",
        "qemu-tests",
        "--bin",
        "smp",
        "--bin",
        "linuxsmp",
        "--bin",
        "linuxbunsmp",
        "--bin",
        "linuxnodesmp",
        "--bin",
        "linuxclaudesmp",
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

/// A per-test replacement for the `-machine` string. Most tests share the
/// per-arch default; the `iommu` test on ARM needs `iommu=smmuv3` added
/// (an SMMUv3 covers PCI devices, so the test also uses virtio-blk-pci).
fn machine_override(arch: Arch, kernel: &str) -> Option<&'static str> {
    match (kernel, arch) {
        ("iommu", Arch::Aarch64) => Some("virt,gic-version=3,highmem-ecam=off,iommu=smmuv3"),
        _ => None,
    }
}

fn qemu_command(arch: Arch, release: bool, bin: &str) -> Command {
    let mut cmd = Command::new(arch.qemu());
    let margs = arch.qemu_machine_args();
    if let Some(machine) = machine_override(arch, bin) {
        // Emit the machine args, substituting the token after `-machine`.
        let mut i = 0;
        while i < margs.len() {
            if margs[i] == "-machine" && i + 1 < margs.len() {
                cmd.arg("-machine").arg(machine);
                i += 2;
            } else {
                cmd.arg(margs[i]);
                i += 1;
            }
        }
    } else {
        cmd.args(margs);
    }
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
/// Press a handful of keys on the guest over QEMU's monitor protocol.
///
/// Hand-written JSON over a Unix socket: the four messages are fixed strings and
/// nothing is parsed back beyond waiting for the greeting, so this needs no JSON
/// library and xtask stays dependency-free (docs/SUBSTRATE.md 11, Tier K).
///
/// Keys are sent **repeatedly over a window** rather than once, because there is
/// no signal saying when the guest's driver has posted its buffers - an event
/// delivered before that is dropped by the device. Repeating is free and is
/// what a person typing looks like anyway.
fn send_keystrokes(path: &str) {
    use std::io::{Read, Write};
    // Wait for QEMU to create the socket. A deadline, not a fixed sleep.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut sock = loop {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return,
        }
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
    // QMP sends a greeting, then wants capabilities negotiated before commands.
    let mut buf = [0u8; 4096];
    let _ = sock.read(&mut buf);
    if sock
        .write_all(b"{\"execute\":\"qmp_capabilities\"}\n")
        .is_err()
    {
        return;
    }
    let _ = sock.read(&mut buf);

    // A press and a release of a few different keys, spread over a few seconds.
    const KEYS: [&str; 4] = ["a", "b", "spc", "ret"];
    for round in 0..12 {
        let key = KEYS[round % KEYS.len()];
        for down in [true, false] {
            let msg = format!(
                "{{\"execute\":\"input-send-event\",\"arguments\":{{\"events\":\
                 [{{\"type\":\"key\",\"data\":{{\"down\":{down},\"key\":\
                 {{\"type\":\"qcode\",\"data\":\"{key}\"}}}}}}]}}}}\n"
            );
            if sock.write_all(msg.as_bytes()).is_err() {
                return;
            }
            let _ = sock.read(&mut buf);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn boot_expect_pass(arch: Arch, release: bool, bin: &str, extra_args: &[&str]) -> bool {
    let log_path = PathBuf::from(format!("target/qemu-{}-{bin}.log", arch.name()));
    let mut cmd = qemu_command(arch, release, bin);
    cmd.args(["-display", "none", "-monitor", "none"]);
    cmd.args(extra_args);
    cmd.arg("-serial")
        .arg(format!("file:{}", log_path.display()));
    cmd.stdin(Stdio::null());

    // A stale QMP socket from a previous run makes QEMU's bind fail at launch.
    if bin == "rng" {
        let _ = std::fs::remove_file(qmp_path(arch));
    }

    println!("[xtask] booting {bin} on {} in QEMU", arch.name());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("[xtask] failed to start {}: {err}", arch.qemu());
            return false;
        }
    };

    // Press keys on the guest's HID keyboard, so the virtio-input driver is
    // exercised by a device that is actually sending (docs/TIME-IDENTITY.md 4a).
    // In a thread, because the boot must not wait on it: a run where QMP never
    // answers reports no HID events rather than hanging.
    if bin == "rng" {
        let path = qmp_path(arch);
        std::thread::spawn(move || send_keystrokes(&path));
    }

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
                // **Keep the failing run's log.** `target/qemu-<arch>-<bin>.log` is
                // overwritten by the next boot, and in a full-matrix run the next boot is
                // seconds away - so the evidence for a failure is routinely gone before
                // anyone reads it. That is not hypothetical: an intermittent `netdns`
                // failure in this tree was diagnosed by *reading the source* rather than
                // the log, because the log had already been replaced by a passing run
                // (docs/ARCHITECTURE-DEBT.md 7.6).
                let keep = format!("target/qemu-{}-{bin}.fail.log", arch.name());
                match std::fs::copy(&log_path, &keep) {
                    Ok(_) => eprintln!(
                        "[xtask] {} {bin}: FAIL (qemu exit code {code}, expected {}) - log kept at {keep}",
                        arch.name(),
                        arch.success_exit_code()
                    ),
                    Err(_) => eprintln!(
                        "[xtask] {} {bin}: FAIL (qemu exit code {code}, expected {})",
                        arch.name(),
                        arch.success_exit_code()
                    ),
                }
                false
            }
        }
    }
}
