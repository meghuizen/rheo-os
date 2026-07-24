// A read-only ext4 driver (docs/FILESYSTEMS.md 1, Tier-1 legacy on-disk).
// Parses a real ext4 image: superblock, block-group descriptors, inodes, the
// extent tree (ext4's default block map), and linear directory entries. Enough
// to mount a disk image and read files/dirs - "disk interchange", not the
// system's own storage. Write support (journaling, allocation) is out of
// scope; the FileSystem write methods default to EROFS.
//
// Scope honestly bounded: extent depth 0 (inline extents - covers small
// files/dirs), 32-bit block group descriptors, directory blocks iterated
// linearly (valid whether or not dir_index/htree is set). Deeper extent trees
// return EIO rather than silently misreading.
//
// Regular // comments keep this file includable by the host validation harness.

use crate::vfs::{DirEntry, Errno, FileSystem, FileType, Metadata, NodeId};
use alloc::string::String;
use alloc::vec::Vec;

const SB_OFFSET: usize = 1024;
const EXT4_MAGIC: u16 = 0xEF53;
const EXTENTS_FL: u32 = 0x0008_0000;
const EH_MAGIC: u16 = 0xF30A;

/// A mounted ext4 image (borrowed for the whole run; `include_bytes!` in the
/// OS, a leaked buffer in the host test).
pub struct Ext4 {
    d: &'static [u8],
    block_size: u64,
    inode_size: u32,
    inodes_per_group: u32,
    first_data_block: u32,
    desc_size: usize,
}

// --- little-endian readers (bounds-checked; corruption -> None -> EIO) ---

fn rd16(d: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*d.get(off)?, *d.get(off + 1)?]))
}
fn rd32(d: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *d.get(off)?,
        *d.get(off + 1)?,
        *d.get(off + 2)?,
        *d.get(off + 3)?,
    ]))
}

impl Ext4 {
    pub fn new(image: &'static [u8]) -> Result<Ext4, Errno> {
        let sb = SB_OFFSET;
        if rd16(image, sb + 56) != Some(EXT4_MAGIC) {
            return Err(Errno::Inval);
        }
        let log_bs = rd32(image, sb + 24).ok_or(Errno::Io)?;
        let block_size = 1024u64 << log_bs;
        let inodes_per_group = rd32(image, sb + 40).ok_or(Errno::Io)?;
        let first_data_block = rd32(image, sb + 20).ok_or(Errno::Io)?;
        let mut inode_size = rd16(image, sb + 88).ok_or(Errno::Io)? as u32;
        if inode_size == 0 {
            inode_size = 128;
        }
        // 64bit feature (incompat 0x80) widens the group descriptor to 64 B.
        let incompat = rd32(image, sb + 96).ok_or(Errno::Io)?;
        let desc_size = if incompat & 0x80 != 0 {
            rd16(image, sb + 254).unwrap_or(64).max(32) as usize
        } else {
            32
        };
        Ok(Ext4 {
            d: image,
            block_size,
            inode_size,
            inodes_per_group,
            first_data_block,
            desc_size,
        })
    }

    fn block(&self, n: u64) -> Result<&[u8], Errno> {
        let start = (n * self.block_size) as usize;
        let end = start + self.block_size as usize;
        self.d.get(start..end).ok_or(Errno::Io)
    }

    /// Byte offset of inode `ino` (1-based) via its group descriptor.
    fn inode_offset(&self, ino: u64) -> Result<usize, Errno> {
        if ino == 0 {
            return Err(Errno::NoEnt);
        }
        let group = (ino - 1) / self.inodes_per_group as u64;
        let index = (ino - 1) % self.inodes_per_group as u64;
        // Group descriptor table starts at the block after the superblock's.
        let gdt_block = self.first_data_block as u64 + 1;
        let gd = (gdt_block * self.block_size) as usize + group as usize * self.desc_size;
        // bg_inode_table_lo at +8.
        let itable = rd32(self.d, gd + 8).ok_or(Errno::Io)? as u64;
        Ok((itable * self.block_size) as usize + (index * self.inode_size as u64) as usize)
    }

    fn inode(&self, ino: u64) -> Result<Inode, Errno> {
        let off = self.inode_offset(ino)?;
        let mode = rd16(self.d, off).ok_or(Errno::Io)?;
        let size_lo = rd32(self.d, off + 4).ok_or(Errno::Io)? as u64;
        let size_hi = rd32(self.d, off + 108).unwrap_or(0) as u64;
        let flags = rd32(self.d, off + 32).ok_or(Errno::Io)?;
        Ok(Inode {
            mode,
            size: size_lo | (size_hi << 32),
            flags,
            i_block: off + 40,
        })
    }

    /// Map every logical block of the inode to a physical block via the
    /// inline extent tree (depth 0).
    fn extents(&self, inode: &Inode) -> Result<Vec<Extent>, Errno> {
        if inode.flags & EXTENTS_FL == 0 {
            // Classic block maps not supported (mkfs.ext4 uses extents).
            return Err(Errno::Io);
        }
        let h = inode.i_block;
        if rd16(self.d, h).ok_or(Errno::Io)? != EH_MAGIC {
            return Err(Errno::Io);
        }
        let entries = rd16(self.d, h + 2).ok_or(Errno::Io)?;
        let depth = rd16(self.d, h + 6).ok_or(Errno::Io)?;
        if depth != 0 {
            return Err(Errno::Io); // deep extent tree unsupported
        }
        let mut out = Vec::new();
        for i in 0..entries as usize {
            let e = h + 12 + i * 12;
            let logical = rd32(self.d, e).ok_or(Errno::Io)?;
            let mut len = rd16(self.d, e + 4).ok_or(Errno::Io)?;
            let start_hi = rd16(self.d, e + 6).ok_or(Errno::Io)? as u64;
            let start_lo = rd32(self.d, e + 8).ok_or(Errno::Io)? as u64;
            // len > 32768 marks an uninitialized extent; its real length is
            // len - 32768 and it reads as zeros. We only read initialized data.
            if len > 32768 {
                len -= 32768;
            }
            out.push(Extent {
                logical,
                phys: (start_hi << 32) | start_lo,
                len,
            });
        }
        Ok(out)
    }

    fn read_inode_at(&self, inode: &Inode, off: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        if off >= inode.size {
            return Ok(0);
        }
        let extents = self.extents(inode)?;
        let want = core::cmp::min(buf.len() as u64, inode.size - off) as usize;
        let mut done = 0;
        while done < want {
            let pos = off + done as u64;
            let lblock = pos / self.block_size;
            let within = (pos % self.block_size) as usize;
            let phys = extents
                .iter()
                .find(|e| lblock >= e.logical as u64 && lblock < e.logical as u64 + e.len as u64)
                .map(|e| e.phys + (lblock - e.logical as u64));
            let n = core::cmp::min(want - done, self.block_size as usize - within);
            match phys {
                Some(pb) => {
                    let blk = self.block(pb)?;
                    buf[done..done + n].copy_from_slice(&blk[within..within + n]);
                }
                None => {
                    // Hole (sparse/uninitialized): reads as zeros.
                    for b in &mut buf[done..done + n] {
                        *b = 0;
                    }
                }
            }
            done += n;
        }
        Ok(done)
    }
}

struct Inode {
    mode: u16,
    size: u64,
    flags: u32,
    i_block: usize,
}

struct Extent {
    logical: u32,
    phys: u64,
    len: u16,
}

fn type_of(mode: u16) -> FileType {
    match mode & 0xF000 {
        0x8000 => FileType::Regular,
        0x4000 => FileType::Dir,
        0xA000 => FileType::Symlink,
        _ => FileType::Other,
    }
}

impl FileSystem for Ext4 {
    fn root(&self) -> NodeId {
        2 // ext4 root is always inode 2
    }

    fn lookup(&self, dir: NodeId, name: &str) -> Result<NodeId, Errno> {
        for e in self.readdir(dir)? {
            if e.name == name {
                return Ok(e.node);
            }
        }
        Err(Errno::NoEnt)
    }

    fn metadata(&self, node: NodeId) -> Result<Metadata, Errno> {
        let ino = self.inode(node)?;
        Ok(Metadata {
            kind: type_of(ino.mode),
            len: ino.size,
            mode: ino.mode & 0x0FFF,
        })
    }

    fn read_at(&self, node: NodeId, off: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let ino = self.inode(node)?;
        if type_of(ino.mode) == FileType::Dir {
            return Err(Errno::IsDir);
        }
        self.read_inode_at(&ino, off, buf)
    }

    fn readdir(&self, node: NodeId) -> Result<Vec<DirEntry>, Errno> {
        let ino = self.inode(node)?;
        if type_of(ino.mode) != FileType::Dir {
            return Err(Errno::NotDir);
        }
        // Read the whole directory into a buffer, then walk dir entries.
        let mut data = alloc::vec![0u8; ino.size as usize];
        self.read_inode_at(&ino, 0, &mut data)?;

        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= data.len() {
            let child = rd32(&data, pos).ok_or(Errno::Io)?;
            let rec_len = rd16(&data, pos + 4).ok_or(Errno::Io)? as usize;
            let name_len = *data.get(pos + 6).ok_or(Errno::Io)? as usize;
            let ftype = *data.get(pos + 7).ok_or(Errno::Io)?;
            if rec_len < 8 {
                break;
            }
            if child != 0 && name_len > 0 && pos + 8 + name_len <= data.len() {
                let name = String::from_utf8_lossy(&data[pos + 8..pos + 8 + name_len]).into_owned();
                let kind = match ftype {
                    1 => FileType::Regular,
                    2 => FileType::Dir,
                    7 => FileType::Symlink,
                    _ => FileType::Other,
                };
                out.push(DirEntry {
                    name,
                    node: child as NodeId,
                    kind,
                });
            }
            pos += rec_len;
        }
        Ok(out)
    }
}
