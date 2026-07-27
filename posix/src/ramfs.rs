//! An in-memory read-write filesystem (a tmpfs/ramfs) behind the VFS. This is
//! the system's working filesystem for now: files and directories in a tree,
//! backed by `alloc`, with interior mutability so it presents the `&self`
//! `FileSystem` interface a mount table needs. Nodes live in a slab addressed
//! by `NodeId`; the root is node 0.

use crate::vfs::{DirEntry, Errno, FileSystem, FileType, Metadata, NodeId};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

struct Node {
    kind: FileType,
    data: Vec<u8>,
    children: BTreeMap<String, NodeId>,
}

struct Inner {
    nodes: Vec<Option<Node>>,
    free: Vec<usize>,
}

impl Inner {
    fn alloc(&mut self, node: Node) -> NodeId {
        let id = match self.free.pop() {
            Some(i) => {
                self.nodes[i] = Some(node);
                i
            }
            None => {
                self.nodes.push(Some(node));
                self.nodes.len() - 1
            }
        };
        id as NodeId
    }
    fn get(&self, id: NodeId) -> Result<&Node, Errno> {
        self.nodes
            .get(id as usize)
            .and_then(|n| n.as_ref())
            .ok_or(Errno::NoEnt)
    }
    fn get_mut(&mut self, id: NodeId) -> Result<&mut Node, Errno> {
        self.nodes
            .get_mut(id as usize)
            .and_then(|n| n.as_mut())
            .ok_or(Errno::NoEnt)
    }
}

pub struct RamFs {
    inner: RefCell<Inner>,
}

impl Default for RamFs {
    fn default() -> Self {
        Self::new()
    }
}

impl RamFs {
    pub fn new() -> RamFs {
        let root = Node {
            kind: FileType::Dir,
            data: Vec::new(),
            children: BTreeMap::new(),
        };
        RamFs {
            inner: RefCell::new(Inner {
                nodes: alloc::vec![Some(root)],
                free: Vec::new(),
            }),
        }
    }
}

impl FileSystem for RamFs {
    fn root(&self) -> NodeId {
        0
    }

    fn lookup(&self, dir: NodeId, name: &str) -> Result<NodeId, Errno> {
        let inner = self.inner.borrow();
        let node = inner.get(dir)?;
        if node.kind != FileType::Dir {
            return Err(Errno::NotDir);
        }
        node.children.get(name).copied().ok_or(Errno::NoEnt)
    }

    fn metadata(&self, node: NodeId) -> Result<Metadata, Errno> {
        let inner = self.inner.borrow();
        let n = inner.get(node)?;
        Ok(Metadata {
            kind: n.kind,
            len: n.data.len() as u64,
            mode: if n.kind == FileType::Dir {
                0o755
            } else {
                0o644
            },
            node,
        })
    }

    fn read_at(&self, node: NodeId, off: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let inner = self.inner.borrow();
        let n = inner.get(node)?;
        if n.kind == FileType::Dir {
            return Err(Errno::IsDir);
        }
        let off = off as usize;
        if off >= n.data.len() {
            return Ok(0);
        }
        let count = core::cmp::min(buf.len(), n.data.len() - off);
        buf[..count].copy_from_slice(&n.data[off..off + count]);
        Ok(count)
    }

    fn readdir(&self, node: NodeId) -> Result<Vec<DirEntry>, Errno> {
        let inner = self.inner.borrow();
        let n = inner.get(node)?;
        if n.kind != FileType::Dir {
            return Err(Errno::NotDir);
        }
        let mut out = Vec::new();
        for (name, &child) in &n.children {
            let kind = inner.get(child).map(|c| c.kind).unwrap_or(FileType::Other);
            out.push(DirEntry {
                name: name.clone(),
                node: child,
                kind,
            });
        }
        Ok(out)
    }

    fn create(&self, dir: NodeId, name: &str, kind: FileType) -> Result<NodeId, Errno> {
        let mut inner = self.inner.borrow_mut();
        {
            let d = inner.get(dir)?;
            if d.kind != FileType::Dir {
                return Err(Errno::NotDir);
            }
            if d.children.contains_key(name) {
                return Err(Errno::Exists);
            }
        }
        let id = inner.alloc(Node {
            kind,
            data: Vec::new(),
            children: BTreeMap::new(),
        });
        inner.get_mut(dir)?.children.insert(String::from(name), id);
        Ok(id)
    }

    fn write_at(&self, node: NodeId, off: u64, buf: &[u8]) -> Result<usize, Errno> {
        let mut inner = self.inner.borrow_mut();
        let n = inner.get_mut(node)?;
        if n.kind == FileType::Dir {
            return Err(Errno::IsDir);
        }
        let end = off as usize + buf.len();
        if n.data.len() < end {
            n.data.resize(end, 0);
        }
        n.data[off as usize..end].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn truncate(&self, node: NodeId, len: u64) -> Result<(), Errno> {
        let mut inner = self.inner.borrow_mut();
        let n = inner.get_mut(node)?;
        if n.kind == FileType::Dir {
            return Err(Errno::IsDir);
        }
        n.data.resize(len as usize, 0);
        Ok(())
    }

    fn unlink(&self, dir: NodeId, name: &str) -> Result<(), Errno> {
        let mut inner = self.inner.borrow_mut();
        let child = {
            let d = inner.get(dir)?;
            match d.children.get(name) {
                Some(&c) => c,
                None => return Err(Errno::NoEnt),
            }
        };
        // A directory must be empty to be removed.
        {
            let c = inner.get(child)?;
            if c.kind == FileType::Dir && !c.children.is_empty() {
                return Err(Errno::NotEmpty);
            }
        }
        inner.get_mut(dir)?.children.remove(name);
        if let Some(slot) = inner.nodes.get_mut(child as usize) {
            *slot = None;
        }
        inner.free.push(child as usize);
        Ok(())
    }
}
