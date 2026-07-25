//! `lrsh` - the librheo-native shell (docs/LIBRHEO.md Phase F), the integration
//! capstone. It is built **entirely** on librheo - no Linux personality: it
//! reads command lines over the Phase D console-input path (parking on input,
//! never spinning), parses them, runs builtins (`echo`/`cd`/`exit`), and for any
//! other command **spawns** a native ELF from `/bin` with its argv (Phase F
//! `proc::spawn`) and **awaits** its exit (`proc::Child::wait`), printing the
//! child's exit code. Everything runs on the async reactor: while the shell
//! blocks for input or for a child, the vcore is free for other strands.
//!
//! Scope (honest, docs/LIBRHEO.md Phase F): the line reader is a minimal
//! raw-byte reader (newline-terminated, backspace-aware); the full `term`
//! line-editor + history + escape decoding (Phase D) is available and is the
//! documented next integration, as are pipelines/redirection over cross-cell
//! channels (Phase E `ipc`). This capstone proves the spawn/wait spine a
//! bash-quality shell is built on.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use librheo::{println, proc, rt, sys};

/// Exit code at end-of-input if no `exit` builtin was run (the test's success
/// sentinel for a clean scripted session).
const EOF_CODE: u64 = 0x42;

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(async {
        let mut line: Vec<u8> = Vec::new();
        loop {
            let mut buf = [0u8; 64];
            // Park on console input (0% CPU where the UART RX interrupt is wired,
            // poll otherwise) - the Phase D block-and-wake.
            let n = rt::read_console(buf.as_mut_ptr(), buf.len()).await;
            if n == 0 {
                // End of input: run any trailing line, then exit cleanly.
                if !line.is_empty() {
                    run_line(&line).await;
                }
                sys::exit(EOF_CODE);
            }
            for &c in &buf[..n] {
                match c {
                    b'\n' | b'\r' => {
                        run_line(&line).await; // may `exit` and never return
                        line.clear();
                    }
                    0x7f | 0x08 => {
                        line.pop();
                    }
                    _ => line.push(c),
                }
            }
        }
    });
    // block_on returns only if the loop broke without exiting (it does not).
    EOF_CODE as i32
}

/// Parse and run one command line.
async fn run_line(line: &[u8]) {
    let Ok(s) = core::str::from_utf8(line) else {
        return;
    };
    let mut words = s.split_whitespace();
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
                    let mut msg = String::new();
                    let _ = core::fmt::write(&mut msg, format_args!("[exit {code}]"));
                    println!("{msg}");
                }
                Err(_) => println!("lrsh: {cmd}: not found"),
            }
        }
    }
}
