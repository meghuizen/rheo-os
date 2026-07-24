//! The Linux personality (docs/LINUX-COMPAT.md). In the full design this is
//! a personality *cell* reached over queue pairs (POSIX-PERSONALITY.md 1);
//! it is kernel-resident here, like `svc.rs` - kernel-side handlers before
//! the service framework exists, running in trap context where the calling
//! cell's user memory is accessible. It adds no kernel object: PIDs, fds,
//! and signal state are per-cell synthesized state in this module, and every
//! underlying operation goes through the cell's existing grants (the
//! `svc::FileOps` VFS personality, the cell's address space, its DRBG).
//!
//! Dispatch reaches this module only for cells tagged `Personality::Linux`
//! (kernel/src/user.rs) - the branch happens *before* the syscall number is
//! interpreted, because native numbers collide with Linux numbers. Syscall
//! numbers are per-ISA constants from `arch::linux_abi` (two tables: x86-64
//! legacy, asm-generic shared by ARM64/RISC-V); this module is portable.
//!
//! The honesty policy (docs/LINUX-COMPAT.md 3) is enforced here: a syscall
//! returns success only when its observable semantics are provided; every
//! number without a handler logs `linux: ENOSYS nr=<n>` to the serial
//! console and returns -ENOSYS, so glibc/tool drift is visible, never a
//! silent hang.

pub mod errno;
pub mod stack;

use crate::arch::linux_abi::nr;

/// What the dispatcher should do after a Linux syscall: resume the cell with
/// a return value, or end its run with an exit code.
pub enum Ctl {
    /// Write the value to the syscall return register and resume.
    Ret(u64),
    /// End the cell's run (`exit` / `exit_group`) with this code.
    Exit(u64),
}

/// Encode a negative errno as the u64 return-register value (Linux userspace
/// treats -1..-4095 as error).
fn err(e: i64) -> Ctl {
    Ctl::Ret((-e) as u64)
}

/// Handle one Linux syscall for the current cell. `args` are the six raw
/// argument registers (already Linux-ordered by `arch::decode_syscall`).
pub fn handle(nr_val: u64, args: &[u64; 6]) -> Ctl {
    match nr_val {
        nr::WRITE => sys_write(args[0], args[1], args[2]),
        nr::EXIT | nr::EXIT_GROUP => Ctl::Exit(args[0]),
        nr::ARCH_PRCTL => sys_arch_prctl(args[0], args[1]),
        other => {
            crate::println!("linux: ENOSYS nr={other}");
            err(errno::ENOSYS)
        }
    }
}

/// write(fd, buf, count). L0 surface: fds 1/2 go to the console; anything
/// else is -EBADF until the L2 fd table (docs/LINUX-COMPAT.md 3).
fn sys_write(fd: u64, buf_va: u64, count: u64) -> Ctl {
    if fd != 1 && fd != 2 {
        return err(errno::EBADF);
    }
    // SAFETY: runs in trap context with the calling cell's user memory
    // accessible; the cell passes a VA range in its own mapped pages, the
    // same trust model as the svc.rs personality handlers.
    let bytes = unsafe { core::slice::from_raw_parts(buf_va as *const u8, count as usize) };
    for &b in bytes {
        crate::arch::serial_write_byte(b);
    }
    Ctl::Ret(count)
}

// arch_prctl codes (x86-64 asm/prctl.h).
const ARCH_SET_FS: u64 = 0x1002;
const ARCH_GET_FS: u64 = 0x1003;

/// arch_prctl(code, addr) - x86-64 only (docs/LINUX-COMPAT.md L1). glibc's
/// x86-64 startup sets the thread pointer with `ARCH_SET_FS` before any TLS
/// access. ARM64/RISC-V never reach here (no such number in their table; they
/// set the thread pointer in userspace). SET_FS programs the FS_BASE MSR;
/// GET_FS writes the current base to `*addr`.
fn sys_arch_prctl(code: u64, addr: u64) -> Ctl {
    match code {
        ARCH_SET_FS => {
            crate::arch::set_user_fs_base(addr);
            Ctl::Ret(0)
        }
        ARCH_GET_FS => {
            // SAFETY: trap context; `addr` is a writable VA in the cell.
            unsafe { (addr as *mut u64).write(crate::arch::user_fs_base()) };
            Ctl::Ret(0)
        }
        _ => err(errno::EINVAL),
    }
}
