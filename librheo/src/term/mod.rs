//! `term`: the byte-stream terminal discipline (docs/LIBRHEO.md Phase D,
//! docs/SHELL.md 1). Raw, non-blocking, high-performance - the
//! reedline/crossterm-class core a bash-quality shell is built on, in **userland**
//! (the kernel owns no TTY; it hands up a raw byte stream). Three layers over the
//! raw console:
//!
//! - [`input`]: a decoder turning raw bytes into typed [`Key`]s - CSI/SS3 escape
//!   sequences (arrows, Home/End/Delete/PageUp-Down, function keys), UTF-8
//!   codepoints, and control chars - with an async `next_key().await` that parks
//!   on the input completion (0%-CPU idle where the UART RX interrupt is wired,
//!   poll otherwise). Built on `rt::read_console`.
//! - [`edit`]: a line editor - insertion, cursor movement, word/line kill,
//!   history recall (up/down), and a completion hook.
//! - [`render`]: a buffered, minimal-diff renderer - batched writes (submit N,
//!   one flush), line repaint with erase-to-EOL, and absolute cursor positioning.
//!
//! Raw mode is the default: no kernel echo or line discipline, librheo owns it.
//! The typed `Key` layer sits on top of the byte stream, which stays primary.

pub mod edit;
pub mod input;
pub mod render;

pub use edit::{Edit, LineEditor};
pub use input::{Key, KeyReader};
pub use render::Renderer;
