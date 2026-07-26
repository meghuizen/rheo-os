//! `lrsh` - the librheo-native shell (docs/LIBRHEO.md Phase F/J), the integration
//! capstone. Built **entirely** on librheo - no Linux personality. As of Phase J
//! it drives the full `term` line editor: it decodes keystrokes with
//! [`KeyReader`](librheo::term::KeyReader) (parking on input via the reactor,
//! never spinning), edits the line with [`LineEditor`](librheo::term::LineEditor)
//! (in-line cursor moves, backspace, word/line kill, **history recall** with
//! Up/Down, and a **command-name completion hook** on Tab), repaints with the
//! buffered [`Renderer`](librheo::term::Renderer), and on Enter runs builtins
//! (`echo`/`cd`/`exit`) or **spawns** `/bin/<cmd>` (Phase F `proc::spawn`) and
//! awaits its exit. Everything runs on the async reactor: while the shell blocks
//! for input or a child, the vcore is free for other strands.
//!
//! Scope (honest, docs/LIBRHEO.md Phase J): pipelines/redirection between spawned
//! cells use `proc::spawn_piped` (the Phase J cross-cell channel); wiring a full
//! `a | b` chain into the shell's parser is the documented next step.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use librheo::term::{Edit, KeyReader, LineEditor, Renderer};
use librheo::{println, proc, rt, sys};

/// Exit code at end-of-input if no `exit` builtin was run (the test's success
/// sentinel for a clean scripted session).
const EOF_CODE: u64 = 0x42;

/// Command names the Tab completer knows (builtins + spawnable coreutils). A real
/// shell would also scan `/bin`; this fixed set is the completion-hook proof.
const COMMANDS: &[&str] = &["echo", "child", "cd", "exit"];

/// The completion hook: complete a bare leading word that is a **unique** prefix
/// of a known command name (e.g. `ec` -> `echo`). Ambiguous or already-spaced
/// lines are left unchanged.
fn complete(line: &str) -> Option<String> {
    if line.is_empty() || line.contains(' ') {
        return None;
    }
    let mut hit: Option<&str> = None;
    for &c in COMMANDS {
        if c.starts_with(line) {
            if hit.is_some() {
                return None; // ambiguous - do not guess
            }
            hit = Some(c);
        }
    }
    hit.map(String::from)
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(async {
        let mut reader = KeyReader::new();
        let mut editor = LineEditor::new().with_completer(complete);
        let mut render = Renderer::new("$ ");
        render.paint("", 0);

        // One strand runs the read-edit-eval loop; it parks on console input
        // (0% CPU where the UART RX interrupt is wired) between keystrokes.
        while let Some(key) = reader.next_key().await {
            match editor.apply(key) {
                Edit::Commit(line) => {
                    render.newline();
                    run_line(&line).await; // may `exit` and never return
                    render.paint("", 0);
                }
                Edit::Redraw => render.paint(&editor.line(), editor.cursor()),
                Edit::Eof => break,
                Edit::Noop => {}
            }
        }
        // End of input without an `exit`: leave cleanly.
        sys::exit(EOF_CODE);
    });
    EOF_CODE as i32
}

/// Parse and run one committed command line.
async fn run_line(line: &str) {
    let mut words = line.split_whitespace();
    let Some(cmd) = words.next() else {
        return; // blank line
    };
    let rest: Vec<&str> = words.collect();

    match cmd {
        // `exit [code]` - leave the shell (default the success sentinel).
        "exit" => {
            let code = rest
                .first()
                .and_then(|x| x.parse::<u64>().ok())
                .unwrap_or(EOF_CODE);
            sys::exit(code);
        }
        // `echo ...` - a builtin (no spawn).
        "echo" => println!("{}", rest.join(" ")),
        // `cd` - native cells have no per-cell cwd object yet; acknowledge and
        // move on (a documented gap - the VFS cwd is a Linux-personality feature).
        "cd" => {}
        // Anything else: spawn `/bin/<cmd>` with argv and await it.
        _ => {
            let path = format!("/bin/{cmd}");
            let mut argv: Vec<&str> = Vec::with_capacity(rest.len() + 1);
            argv.push(cmd);
            argv.extend_from_slice(&rest);
            match proc::spawn(&path, &argv, &[]) {
                Ok(child) => {
                    let code = child.wait().await;
                    println!("[exit {code}]");
                }
                Err(_) => println!("lrsh: {cmd}: not found"),
            }
        }
    }
}
