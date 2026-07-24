//! rheo-os environment variables (installed into std as `sys/env/rheo.rs` by
//! targets/patch-std.py; docs/USERLAND.md M5). rheo has no kernel environment
//! block yet, so the environment is an in-process table: empty at start,
//! writable via `set_var`, readable via `var`/`vars`. This gives coreutils a
//! real, working `std::env` surface (e.g. `env`, `printenv`) without inventing
//! a kernel env ABI - a genuine limitation documented here, not a stub that
//! lies about success.
use crate::collections::BTreeMap;
use crate::ffi::{OsStr, OsString};
use crate::io;
use crate::sync::{Mutex, OnceLock};

pub use super::common::Env;

fn table() -> &'static Mutex<BTreeMap<OsString, OsString>> {
    static T: OnceLock<Mutex<BTreeMap<OsString, OsString>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn lock() -> crate::sync::MutexGuard<'static, BTreeMap<OsString, OsString>> {
    table().lock().unwrap_or_else(|e| e.into_inner())
}

pub fn env() -> Env {
    let entries: Vec<(OsString, OsString)> =
        lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    Env::new(entries)
}

pub fn getenv(k: &OsStr) -> Option<OsString> {
    lock().get(k).cloned()
}

pub unsafe fn setenv(k: &OsStr, v: &OsStr) -> io::Result<()> {
    lock().insert(k.to_os_string(), v.to_os_string());
    Ok(())
}

pub unsafe fn unsetenv(k: &OsStr) -> io::Result<()> {
    lock().remove(k);
    Ok(())
}
