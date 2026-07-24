//! The mount table and path resolution. A path is resolved by picking the
//! mount whose prefix matches the most leading components (so `/mnt/x` uses
//! the `/mnt` mount, everything else the `/` mount), then walking the
//! remaining components through that filesystem with `lookup`. This is the
//! per-session `/` of POSIX-PERSONALITY.md 3, in miniature.

use crate::vfs::{Errno, FileSystem, NodeId};
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

struct Mount {
    /// Path components of the mount point, e.g. `/mnt` -> `["mnt"]`, `/` -> `[]`.
    prefix: Vec<String>,
    fs: Rc<dyn FileSystem>,
}

static mut MOUNTS: Option<Vec<Mount>> = None;

fn mounts() -> &'static mut Vec<Mount> {
    // SAFETY: single-vcore cooperative; the table is built at setup and read
    // during resolution, never concurrently.
    unsafe { (*core::ptr::addr_of_mut!(MOUNTS)).get_or_insert_with(Vec::new) }
}

/// Drop all mounts (tests set up fresh mounts per run).
pub fn reset() {
    *mounts() = Vec::new();
}

/// Mount `fs` at absolute path `at` (`"/"`, `"/mnt"`, ...).
pub fn mount(at: &str, fs: Rc<dyn FileSystem>) {
    mounts().push(Mount {
        prefix: components(at),
        fs,
    });
}

/// Split an absolute path into net components, resolving `.` (dropped) and
/// `..` (pops the previous component).
pub fn components(path: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(String::from(other)),
        }
    }
    out
}

/// Pick the mount with the longest component-prefix of `comps`, returning it
/// and the remaining components to walk within it.
fn pick(comps: &[String]) -> Result<(Rc<dyn FileSystem>, Vec<String>), Errno> {
    let ms = mounts();
    let mut best: Option<usize> = None;
    let mut best_len = 0usize;
    for (i, m) in ms.iter().enumerate() {
        let pl = m.prefix.len();
        if pl <= comps.len() && comps[..pl] == m.prefix[..] && (best.is_none() || pl >= best_len) {
            best = Some(i);
            best_len = pl;
        }
    }
    let idx = best.ok_or(Errno::NoEnt)?;
    let rest = comps[best_len..].to_vec();
    Ok((ms[idx].fs.clone(), rest))
}

fn walk(fs: &Rc<dyn FileSystem>, rest: &[String]) -> Result<NodeId, Errno> {
    let mut node = fs.root();
    for comp in rest {
        node = fs.lookup(node, comp)?;
    }
    Ok(node)
}

/// Resolve an absolute path to (filesystem, node).
pub fn resolve(path: &str) -> Result<(Rc<dyn FileSystem>, NodeId), Errno> {
    let comps = components(path);
    let (fs, rest) = pick(&comps)?;
    let node = walk(&fs, &rest)?;
    Ok((fs, node))
}

/// Resolve the parent directory of `path`, returning (filesystem, parent
/// node, final component). Errors if the path is a bare mount root.
pub fn resolve_parent(path: &str) -> Result<(Rc<dyn FileSystem>, NodeId, String), Errno> {
    let mut comps = components(path);
    let last = comps.pop().ok_or(Errno::Inval)?;
    let (fs, rest) = pick(&comps)?;
    let parent = walk(&fs, &rest)?;
    Ok((fs, parent, last))
}
