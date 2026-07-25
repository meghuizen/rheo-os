//! `librheo-term` - the Phase D proof program (docs/LIBRHEO.md). An interactive
//! read-eval loop over the `term` byte-stream discipline: it reads keys (parking
//! on input via the reactor - the kernel idles until a byte arrives), edits a
//! line with history, renders each change, and collects committed lines. Driven
//! by a scripted keystroke sequence that exercises typing, backspace,
//! cursor-left + insert, an escape sequence (arrow keys), and history recall
//! (Up), it verifies the committed lines exactly and exits with a distinctive
//! code. The `librheoterm` test asserts that code (and, where the UART RX
//! interrupt is wired, that the kernel actually idled).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};

use librheo::rt;
use librheo::term::{Edit, KeyReader, LineEditor, Renderer};

/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;

static RESULT: AtomicI32 = AtomicI32::new(0);

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    // The read-eval loop is one strand; it parks on console input, so the vcore
    // (and, underneath, the kernel) idles between keystrokes.
    rt::block_on(async {
        RESULT.store(run().await, Ordering::Relaxed);
    });
    RESULT.load(Ordering::Relaxed)
}

async fn run() -> i32 {
    // The scripted keystrokes drive: "worlq"<BS>"d"<CR> -> commit "world";
    // "helo"<Left>"l"<CR> -> commit "hello"; <Up><Up><CR> -> commit "world"
    // (recall the older history entry). See `librheoterm.rs` for the exact bytes.
    let expected = ["world", "hello", "world"];
    let mut committed: Vec<String> = Vec::new();

    let mut reader = KeyReader::new();
    let mut editor = LineEditor::new();
    let mut render = Renderer::new("$ ");
    render.paint("", 0);

    while let Some(key) = reader.next_key().await {
        match editor.apply(key) {
            Edit::Commit(line) => {
                render.newline();
                committed.push(line);
                render.paint("", 0);
            }
            Edit::Redraw => render.paint(&editor.line(), editor.cursor()),
            Edit::Eof => break,
            Edit::Noop => {}
        }
    }
    render.newline();

    // Verify the committed lines match the scripted expectation exactly.
    if committed.len() != expected.len() {
        return 20;
    }
    for (got, want) in committed.iter().zip(expected.iter()) {
        if got.as_str() != *want {
            return 21;
        }
    }
    librheo::println!(
        "librheo-term: editing+history+escape OK, committed {}",
        committed.len()
    );
    OK_CODE
}
