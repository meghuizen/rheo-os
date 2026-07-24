//! A `std::fs`-shaped facade over the POSIX layer, so code written against
//! Rust's standard filesystem API runs on the OS's native VFS. This is the
//! "Rust standard library translation" for files: `File`, `OpenOptions`,
//! `read`/`read_to_string`/`write`, `create_dir`, `remove_file`, `metadata`,
//! `read_dir` - same shapes as `std::fs`, backed by capability-checked VFS
//! ops instead of Linux syscalls.

use crate::sys::{
    self, O_APPEND, O_CREAT, O_DIRECTORY, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY, Whence,
};
use crate::vfs::{DirEntry, Errno, Metadata};
use alloc::string::String;
use alloc::vec::Vec;

/// An open file. Closes on drop, like `std::fs::File`.
pub struct File {
    fd: usize,
}

impl File {
    pub fn open(path: &str) -> Result<File, Errno> {
        Ok(File {
            fd: sys::open(path, O_RDONLY)?,
        })
    }

    pub fn create(path: &str) -> Result<File, Errno> {
        Ok(File {
            fd: sys::open(path, O_WRONLY | O_CREAT | O_TRUNC)?,
        })
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        sys::read(self.fd, buf)
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize, Errno> {
        sys::write(self.fd, buf)
    }

    pub fn write_all(&self, mut buf: &[u8]) -> Result<(), Errno> {
        while !buf.is_empty() {
            let n = self.write(buf)?;
            if n == 0 {
                return Err(Errno::Io);
            }
            buf = &buf[n..];
        }
        Ok(())
    }

    pub fn read_to_end(&self, out: &mut Vec<u8>) -> Result<usize, Errno> {
        let mut tmp = [0u8; 512];
        let mut total = 0;
        loop {
            let n = self.read(&mut tmp)?;
            if n == 0 {
                return Ok(total);
            }
            out.extend_from_slice(&tmp[..n]);
            total += n;
        }
    }

    pub fn seek(&self, off: i64, whence: Whence) -> Result<u64, Errno> {
        sys::lseek(self.fd, off, whence)
    }

    pub fn metadata(&self) -> Result<Metadata, Errno> {
        sys::fstat(self.fd)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let _ = sys::close(self.fd);
    }
}

/// Builder mirroring `std::fs::OpenOptions`.
#[derive(Default)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    create: bool,
    truncate: bool,
    append: bool,
    directory: bool,
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions::default()
    }
    pub fn read(mut self, v: bool) -> Self {
        self.read = v;
        self
    }
    pub fn write(mut self, v: bool) -> Self {
        self.write = v;
        self
    }
    pub fn create(mut self, v: bool) -> Self {
        self.create = v;
        self
    }
    pub fn truncate(mut self, v: bool) -> Self {
        self.truncate = v;
        self
    }
    pub fn append(mut self, v: bool) -> Self {
        self.append = v;
        self
    }
    pub fn directory(mut self, v: bool) -> Self {
        self.directory = v;
        self
    }
    pub fn open(&self, path: &str) -> Result<File, Errno> {
        let mut flags = match (self.read, self.write) {
            (true, true) => O_RDWR,
            (false, true) => O_WRONLY,
            _ => O_RDONLY,
        };
        if self.create {
            flags |= O_CREAT;
        }
        if self.truncate {
            flags |= O_TRUNC;
        }
        if self.append {
            flags |= O_APPEND;
        }
        if self.directory {
            flags |= O_DIRECTORY;
        }
        Ok(File {
            fd: sys::open(path, flags)?,
        })
    }
}

// ---- free functions, like std::fs::{read, write, ...} ----

pub fn read(path: &str) -> Result<Vec<u8>, Errno> {
    let f = File::open(path)?;
    let mut out = Vec::new();
    f.read_to_end(&mut out)?;
    Ok(out)
}

pub fn read_to_string(path: &str) -> Result<String, Errno> {
    let bytes = read(path)?;
    String::from_utf8(bytes).map_err(|_| Errno::Inval)
}

pub fn write(path: &str, data: &[u8]) -> Result<(), Errno> {
    let f = File::create(path)?;
    f.write_all(data)
}

pub fn create_dir(path: &str) -> Result<(), Errno> {
    sys::mkdir(path)
}

pub fn remove_file(path: &str) -> Result<(), Errno> {
    sys::unlink(path)
}

pub fn metadata(path: &str) -> Result<Metadata, Errno> {
    sys::stat(path)
}

pub fn read_dir(path: &str) -> Result<Vec<DirEntry>, Errno> {
    sys::getdents(path)
}
