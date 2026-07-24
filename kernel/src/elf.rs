//! A minimal ELF64 parser for loading userland programs (docs/USERLAND.md
//! M1). Reads the entry point and iterates `PT_LOAD` segments. It is
//! bounds-checked and allocation-free; the actual mapping/copying lives in
//! `load`. Only what the loader needs - no sections, symbols, or relocations
//! (the userland programs are statically linked `EXEC` files).

/// Program-header flags.
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
#[allow(dead_code)]
pub const PF_R: u32 = 4;

const PT_LOAD: u32 = 1;

/// ELF `e_type` values the loader distinguishes.
pub const ET_EXEC: u16 = 2;
pub const ET_DYN: u16 = 3;

/// One loadable segment, in terms the loader maps directly.
pub struct Segment {
    /// Virtual address the segment is linked at.
    pub vaddr: u64,
    /// Byte offset of the segment's file image within the ELF.
    pub offset: usize,
    /// Bytes present in the file (the rest of `memsz` is zero-fill / bss).
    pub filesz: usize,
    /// Bytes the segment occupies in memory (>= filesz).
    pub memsz: usize,
    /// PF_* permission flags.
    pub flags: u32,
}

/// A parsed ELF64 image (little-endian).
pub struct Elf<'a> {
    image: &'a [u8],
    etype: u16,
    entry: u64,
    phoff: usize,
    phnum: usize,
    phentsize: usize,
}

fn rd_u16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}
fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}
fn rd_u64(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

impl<'a> Elf<'a> {
    /// Parse the ELF header. Returns None if it is not a little-endian
    /// 64-bit ELF or the header is truncated.
    pub fn parse(image: &'a [u8]) -> Option<Elf<'a>> {
        if image.get(0..4)? != b"\x7fELF" {
            return None;
        }
        if *image.get(4)? != 2 {
            return None; // EI_CLASS: 64-bit
        }
        if *image.get(5)? != 1 {
            return None; // EI_DATA: little-endian
        }
        let etype = rd_u16(image, 16)?;
        let entry = rd_u64(image, 24)?;
        let phoff = rd_u64(image, 32)? as usize;
        let phentsize = rd_u16(image, 54)? as usize;
        let phnum = rd_u16(image, 56)? as usize;
        if phentsize < 56 {
            return None; // an ELF64 program header is 56 bytes
        }
        Some(Elf {
            image,
            etype,
            entry,
            phoff,
            phnum,
            phentsize,
        })
    }

    /// The linked entry point (add the load bias for an `ET_DYN` image).
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// `e_type` (`ET_EXEC` or `ET_DYN`).
    pub fn etype(&self) -> u16 {
        self.etype
    }

    /// File offset of the program-header table.
    pub fn phoff(&self) -> usize {
        self.phoff
    }

    /// Program-header entry size and count (for the auxv AT_PHENT/AT_PHNUM).
    pub fn phentsize(&self) -> usize {
        self.phentsize
    }
    pub fn phnum(&self) -> usize {
        self.phnum
    }

    /// The virtual address of the program-header table as the loaded image
    /// will see it (auxv `AT_PHDR`), if a `PT_LOAD` segment covers the file
    /// range `[phoff, phoff + phnum*phentsize)`. The load bias is added by
    /// the caller for an `ET_DYN` image. Returns None if no segment maps the
    /// headers (the caller then copies them to a page - docs/LINUX-COMPAT.md).
    pub fn phdr_vaddr(&self) -> Option<u64> {
        let lo = self.phoff;
        let hi = lo + self.phnum * self.phentsize;
        let mut found = None;
        // Manual scan (for_each_load takes a closure that cannot early-return
        // a value): a PT_LOAD whose file range contains the header table.
        for i in 0..self.phnum {
            let base = self.phoff + i * self.phentsize;
            if rd_u32(self.image, base) != Some(PT_LOAD) {
                continue;
            }
            let offset = rd_u64(self.image, base + 8)? as usize;
            let vaddr = rd_u64(self.image, base + 16)?;
            let filesz = rd_u64(self.image, base + 32)? as usize;
            if offset <= lo && hi <= offset + filesz {
                found = Some(vaddr + (lo - offset) as u64);
                break;
            }
        }
        found
    }

    /// Like [`for_each_load`], but WITHOUT bounds-checking each segment's file
    /// range against the image slice - for the streaming `execve` loader, where
    /// only the ELF header + program-header table live in the passed buffer and
    /// each segment's bytes are read separately from the file
    /// (docs/LINUX-COMPAT.md L6). The program-header table itself must still lie
    /// within the buffer.
    pub fn for_each_load_streamed(&self, mut f: impl FnMut(&Segment) -> Option<()>) -> Option<()> {
        for i in 0..self.phnum {
            let base = self.phoff + i * self.phentsize;
            if rd_u32(self.image, base)? != PT_LOAD {
                continue;
            }
            let flags = rd_u32(self.image, base + 4)?;
            let offset = rd_u64(self.image, base + 8)? as usize;
            let vaddr = rd_u64(self.image, base + 16)?;
            let filesz = rd_u64(self.image, base + 32)? as usize;
            let memsz = rd_u64(self.image, base + 40)? as usize;
            if filesz > memsz {
                return None;
            }
            f(&Segment {
                vaddr,
                offset,
                filesz,
                memsz,
                flags,
            })?;
        }
        Some(())
    }

    /// Invoke `f` for each `PT_LOAD` segment in order. Returns None if the
    /// headers are malformed, a segment's file range is out of bounds, or
    /// `f` returns None.
    pub fn for_each_load(&self, mut f: impl FnMut(&Segment) -> Option<()>) -> Option<()> {
        for i in 0..self.phnum {
            let base = self.phoff + i * self.phentsize;
            if rd_u32(self.image, base)? != PT_LOAD {
                continue;
            }
            let flags = rd_u32(self.image, base + 4)?;
            let offset = rd_u64(self.image, base + 8)? as usize;
            let vaddr = rd_u64(self.image, base + 16)?;
            let filesz = rd_u64(self.image, base + 32)? as usize;
            let memsz = rd_u64(self.image, base + 40)? as usize;
            // The file bytes must lie within the image; memsz must cover them.
            if offset.checked_add(filesz)? > self.image.len() || filesz > memsz {
                return None;
            }
            f(&Segment {
                vaddr,
                offset,
                filesz,
                memsz,
                flags,
            })?;
        }
        Some(())
    }
}
