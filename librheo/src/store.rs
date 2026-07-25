//! Async block/dataset access for the bulk warehouse path (docs/LIBRHEO.md
//! Phase B). A thin layer over [`io`](crate::io) + [`mem`](crate::mem): open a
//! dataset, learn its size, and either **map** it zero-copy for a scan or
//! **stream** ranges asynchronously. The block/object transport underneath is
//! the kernel's `BlockDevice`/virtio-blk seam (docs/FILESYSTEMS.md); a cell
//! reaches it through the VFS the same way a file is reached, so `store` and
//! `io` share one submit/complete machinery (folded together for now).

use crate::io::File;
use crate::mem::Mapping;

/// A dataset opened for the bulk path: the open file plus its byte length.
pub struct Dataset {
    file: File,
    len: u64,
}

impl Dataset {
    /// Open `path` and stat it. `Err` is the completion status.
    pub async fn open(path: &str) -> Result<Dataset, u32> {
        let file = File::open(path).await?;
        let len = file.size().await?;
        Ok(Dataset { file, len })
    }

    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Map `[offset, offset+len)` of the dataset zero-copy for a scan.
    pub fn map(&self, offset: u64, len: usize) -> Option<Mapping> {
        Mapping::file(self.file.fd() as u64, offset, len)
    }

    /// Map the whole dataset zero-copy.
    pub fn map_all(&self) -> Option<Mapping> {
        Mapping::file(self.file.fd() as u64, 0, self.len as usize)
    }
}
