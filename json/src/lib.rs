//! rheo-json: a dependency-free, zero-copy JSON parser (docs/JSON.md).
//!
//! `no_std + alloc`, so the same crate runs in a cell over rheo-libc and on
//! the host (`cargo test`). It follows the OS's "measured runtime dispatch
//! over wide SIMD" stance (ARCHITECTURE.md 1.4): the scalar parser here works
//! everywhere; a SIMD structural pre-scan chosen at runtime is added on top
//! for the host benchmark. Strings borrow from the input unless they contain
//! escapes - the zero-copy path the seal/grant model favours.
//!
//! ```ignore
//! let v = rheo_json::parse(r#"{"ok":true,"xs":[1,2,3]}"#).unwrap();
//! assert_eq!(v.get("ok").and_then(|b| b.as_bool()), Some(true));
//! ```

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod error;
mod parse;
mod scan;
mod value;

pub use error::{Error, ErrorKind};
pub use parse::{MAX_DEPTH, parse, parse_bytes};
pub use value::{Number, Value};
