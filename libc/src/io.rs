//! fd-based I/O wrappers and console writers (docs/USERLAND.md M3). The Rust
//! wrappers take slices/&str; `Stdout`/`Stderr` implement `core::fmt::Write`
//! so the `print!`/`println!`/`eprintln!` macros format straight to fds 1/2.

pub const O_RDONLY: u64 = 0;
pub const O_WRONLY: u64 = 1;
pub const O_RDWR: u64 = 2;
pub const O_CREAT: u64 = 0o100;
pub const O_TRUNC: u64 = 0o1000;
pub const O_APPEND: u64 = 0o2000;

/// Open `path`; returns a file descriptor or a negative errno.
pub fn open(path: &str, flags: u64) -> i64 {
    crate::sys::open(path.as_ptr() as u64, path.len() as u64, flags)
}

/// Read into `buf`; returns bytes read or a negative errno.
pub fn read(fd: i64, buf: &mut [u8]) -> i64 {
    crate::sys::read(fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64)
}

/// Write `buf`; returns bytes written or a negative errno.
pub fn write(fd: i64, buf: &[u8]) -> i64 {
    crate::sys::write(fd as u64, buf.as_ptr() as u64, buf.len() as u64)
}

pub fn close(fd: i64) -> i64 {
    crate::sys::close(fd as u64)
}

pub fn lseek(fd: i64, offset: i64, whence: u64) -> i64 {
    crate::sys::lseek(fd as u64, offset, whence)
}

/// A `core::fmt::Write` sink over a fixed fd (1 = stdout, 2 = stderr).
pub struct FdWriter(pub i64);

impl core::fmt::Write for FdWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let mut off = 0;
        let bytes = s.as_bytes();
        while off < bytes.len() {
            let n = write(self.0, &bytes[off..]);
            if n <= 0 {
                return Err(core::fmt::Error);
            }
            off += n as usize;
        }
        Ok(())
    }
}

pub fn stdout() -> FdWriter {
    FdWriter(1)
}
pub fn stderr() -> FdWriter {
    FdWriter(2)
}
