//! Processes: spawn / wait, this cell's arguments, and an in-process environment
//! (docs/LIBRHEO.md Phase F, docs/ARCHITECTURE.md 3 object 1 Cell). Spawning a
//! native ELF creates a fresh cell with its own address space + queue pair; it
//! is gated by the **cell-spawn capability** the kernel checks (no ambient
//! authority). `Child::wait` is async - the parent's other strands run while it
//! blocks (the reactor blocks the parent in `SYS_WAIT` only once every strand
//! has parked).
//!
//! Arguments come from the initial stack the kernel built (`argv`/`envp`), read
//! via the pointer `_start` captured. The environment is an in-process table (a
//! program mutates its own view; it is not inherited across `spawn` yet -
//! documented). `identity` is this cell's synthesized id.

use alloc::string::String;
use alloc::vec::Vec;

use crate::ipc::{AsyncReceiver, AsyncSender, Channel};
use crate::rt;
use crate::sys;

/// A spawned child cell (docs/LIBRHEO.md Phase F). Its handle is the kernel's
/// child id; [`wait`](Child::wait) reaps it and yields its exit code.
pub struct Child {
    handle: u64,
}

impl Child {
    /// The kernel handle for this child.
    pub fn handle(&self) -> u64 {
        self.handle
    }

    /// Await the child's exit and return its exit code (0..=255; `FAULT_EXIT` =
    /// 139 if it faulted - native cells have no signals). Async: the parent's
    /// other strands run while this blocks.
    pub async fn wait(self) -> u64 {
        rt::wait_child(self.handle).await
    }
}

/// The error a failed [`spawn`] returns.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SpawnError;

/// Spawn `path` as a new native cell with `argv` and `env` (docs/LIBRHEO.md
/// Phase F). `argv[0]` is conventionally the program name. Returns a [`Child`]
/// or [`SpawnError`] (no spawn capability, ELF not found, or the cell table is
/// full). The child does not run until it is `wait`ed (or the parent exits).
pub fn spawn(path: &str, argv: &[&str], env: &[&str]) -> Result<Child, SpawnError> {
    spawn_inner(path, argv, env, 0)
}

/// Spawn `path` as a new native cell that **inherits this cell's channel end
/// `slot`** as its own slot 0, with the opposite role (docs/NETSTACK.md the
/// service-cell section, rheo-net N4a). This is how a **service cell** gives every
/// client its own private ring: the service holds N ends (slots `0..N`, wired at
/// connect time) and spawns client k on slot k. The child is slot-agnostic - it
/// always finds its end with `ipc::Channel::open()`.
///
/// [`spawn`] is this with the Phase J default (slot 0 if wired). Fails with
/// [`SpawnError`] if `slot` holds no channel, or for the usual spawn reasons.
pub fn spawn_on_channel(
    path: &str,
    argv: &[&str],
    env: &[&str],
    slot: usize,
) -> Result<Child, SpawnError> {
    spawn_inner(path, argv, env, sys::spawn_chan_spec(slot))
}

fn spawn_inner(
    path: &str,
    argv: &[&str],
    env: &[&str],
    chan_spec: u64,
) -> Result<Child, SpawnError> {
    // Build NUL-terminated C strings + NULL-terminated pointer arrays in this
    // cell's memory; the kernel reads them out before building the child stack.
    let mut argv_c: Vec<Vec<u8>> = Vec::with_capacity(argv.len());
    for a in argv {
        let mut s = Vec::with_capacity(a.len() + 1);
        s.extend_from_slice(a.as_bytes());
        s.push(0);
        argv_c.push(s);
    }
    let mut env_c: Vec<Vec<u8>> = Vec::with_capacity(env.len());
    for e in env {
        let mut s = Vec::with_capacity(e.len() + 1);
        s.extend_from_slice(e.as_bytes());
        s.push(0);
        env_c.push(s);
    }
    let mut argv_ptrs: Vec<u64> = argv_c.iter().map(|s| s.as_ptr() as u64).collect();
    argv_ptrs.push(0);
    let mut env_ptrs: Vec<u64> = env_c.iter().map(|s| s.as_ptr() as u64).collect();
    env_ptrs.push(0);

    let handle = sys::spawn_chan(
        path.as_ptr() as u64,
        path.len() as u64,
        argv_ptrs.as_ptr() as u64,
        env_ptrs.as_ptr() as u64,
        chan_spec,
    );
    if handle == u64::MAX {
        Err(SpawnError)
    } else {
        Ok(Child { handle })
    }
}

/// A spawned child piped to this cell (docs/LIBRHEO.md Phase J). The child
/// **inherits this cell's cross-cell channel** at spawn (the kernel maps the same
/// frames into it, opposite role), so the child's streamed output flows to this
/// cell over the Phase E channel - **not through the kernel** - and this cell
/// reads it with [`rx`](Pipe::rx). [`tx`](Pipe::tx) sends acks / back-pressure to
/// the child. Reap the child with `child.wait().await` once its stream is drained.
pub struct Pipe {
    /// The spawned producer child.
    pub child: Child,
    /// This (consumer) end's sender - acks / back-pressure to the child.
    pub tx: AsyncSender,
    /// This (consumer) end's receiver - the child's streamed output.
    pub rx: AsyncReceiver,
}

/// Spawn `path` as a child whose output is **piped to this cell over the Phase E
/// channel** (docs/LIBRHEO.md Phase J): a cross-cell stdout pipeline between a
/// spawned cell and this one, built on the item-1 async `Sender`/`Receiver`. This
/// cell must already hold a cross-cell channel (`SYS_CONNECT` - wired at connect
/// time); the child inherits it at spawn with the opposite role. Returns a
/// [`Pipe`] (the child handle + this end's async sender/receiver), or
/// [`SpawnError`] (no channel wired, no spawn capability, ELF not found, cell
/// table full).
///
/// **Honest scope** (single-CPU, docs/LIBRHEO.md Phase J): the pipe connects a
/// spawned child to its **parent** cell (a valid `SYS_SWITCH` `cur^1` pair). Two
/// *sibling* spawned stages (`a | b`, both children) as a directly-switched pair
/// await a directed cross-cell switch / SMP (task #27) - the mechanism (channel
/// inheritance + async `Sender`/`Receiver`) is the deliverable.
pub fn spawn_piped(path: &str, argv: &[&str], env: &[&str]) -> Result<Pipe, SpawnError> {
    // Bind this cell's channel end to the reactor (the async Sender/Receiver).
    let ch = Channel::open().ok_or(SpawnError)?;
    let (tx, rx) = ch.split();
    // Spawn the child; the kernel maps this cell's channel into it (opposite role).
    let child = spawn(path, argv, env)?;
    Ok(Pipe { child, tx, rx })
}

// ------------------------------------------------------------------- args

/// This cell's command-line arguments (`argv`), parsed from the initial stack
/// the kernel built (docs/LIBRHEO.md Phase F). Empty for a top-level cell
/// installed without arguments.
pub fn args() -> Vec<String> {
    parse_cstr_array(argc_argv())
}

/// This cell's environment strings (`envp`, `KEY=VALUE`), as passed at spawn.
pub fn env_args() -> Vec<String> {
    parse_cstr_array(envp())
}

/// `(ptr-to-argv[0], argc)` from the SysV block, or `(null, 0)`.
fn argc_argv() -> (*const u64, usize) {
    let base = rt::args_ptr();
    if base == 0 {
        return (core::ptr::null(), 0);
    }
    // SAFETY: `base` is the kernel-built SysV block: [argc][argv..][NULL][envp..].
    unsafe {
        let argc = *(base as *const u64) as usize;
        ((base as *const u64).add(1), argc)
    }
}

/// `(ptr-to-envp[0], count)` from the SysV block (envp follows argv's NULL).
fn envp() -> (*const u64, usize) {
    let base = rt::args_ptr();
    if base == 0 {
        return (core::ptr::null(), 0);
    }
    // SAFETY: as above; walk past argc + argv[..] + the argv NULL terminator.
    unsafe {
        let argc = *(base as *const u64) as usize;
        let envp0 = (base as *const u64).add(1 + argc + 1);
        // Count entries up to the NULL terminator (bounded for safety).
        let mut n = 0usize;
        while n < 256 && *envp0.add(n) != 0 {
            n += 1;
        }
        (envp0, n)
    }
}

/// Turn a `(ptr, count)` array of C-string pointers into owned `String`s
/// (lossily decoding non-UTF-8 bytes).
fn parse_cstr_array((ptr, count): (*const u64, usize)) -> Vec<String> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        // SAFETY: `ptr[i]` is a NUL-terminated C string in this cell's memory.
        let s = unsafe {
            let p = *ptr.add(i) as *const u8;
            let mut len = 0usize;
            while *p.add(len) != 0 {
                len += 1;
            }
            core::slice::from_raw_parts(p, len)
        };
        out.push(String::from_utf8_lossy(s).into_owned());
    }
    out
}

// ------------------------------------------------------------- environment

/// This cell's synthesized identity: the kernel's live-cell count is not exposed
/// per-cell yet, so `identity` returns a stable in-process token (the reactor's
/// args pointer distinguishes a spawned cell from a top cell). A first-class
/// per-cell id capability is documented future work.
pub fn identity() -> u64 {
    rt::args_ptr()
}

/// An in-process environment table (docs/LIBRHEO.md Phase F). Seeded once from
/// the `envp` the cell was spawned with; a program mutates its own view. Not yet
/// inherited across `spawn` (a spawned child gets only the `env` explicitly
/// passed to [`spawn`]) - documented. Single-vcore cell, so no locking.
static mut ENV: Option<Vec<(String, String)>> = None;

fn env_table() -> &'static mut Vec<(String, String)> {
    // SAFETY: single-vcore cooperative cell; lazily initialised, then owned here.
    unsafe {
        let slot = &mut *core::ptr::addr_of_mut!(ENV);
        if slot.is_none() {
            let mut v = Vec::new();
            for kv in env_args() {
                if let Some((k, val)) = kv.split_once('=') {
                    v.push((String::from(k), String::from(val)));
                }
            }
            *slot = Some(v);
        }
        slot.as_mut().unwrap()
    }
}

/// The value of environment variable `key`, if set.
pub fn env_get(key: &str) -> Option<String> {
    env_table()
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

/// Set (or replace) environment variable `key` in this cell's view.
pub fn env_set(key: &str, value: &str) {
    let t = env_table();
    if let Some(e) = t.iter_mut().find(|(k, _)| k == key) {
        e.1 = String::from(value);
    } else {
        t.push((String::from(key), String::from(value)));
    }
}

/// All environment variables as `(key, value)` pairs.
pub fn env_vars() -> Vec<(String, String)> {
    env_table().clone()
}
