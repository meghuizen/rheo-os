// A coreutils multicall program for rheo-os (docs/USERLAND.md M5). Faithful
// reimplementations of a real coreutils subset, dispatched by argv - the same
// multicall shape uutils/busybox use. Built against real `std`: arguments
// arrive through the crt0 (`std::env::args`), files are read through
// `std::fs` over the VFS, output goes to `std::io::stdout`. This is the M5
// deliverable: standard command-line tools running as a U-mode cell.
//
// It is honest about what it is - a from-scratch port, not the upstream uutils
// crate (whose clap/uucore dependency tree needs std surface rheo does not
// have yet: `IsTerminal`, locale, terminal width). See docs/USERLAND.md M5.
#![feature(restricted_std)]

use std::io::{self, Read, Write};
use std::process::ExitCode;

// Force-link the crt0 (ENTRY(_start) pulls in _start from rheo-rt).
extern crate rheo_rt as _;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Multicall dispatch: prefer argv[0]'s basename as the utility name (the
    // symlink convention). If that is not a known utility, fall back to the
    // busybox form `coreutils <util> [args...]`.
    let arg0 = args.first().map(|s| base(s)).unwrap_or("");
    let (util, rest): (&str, &[String]) = if is_util(arg0) {
        (arg0, &args[1..])
    } else if args.len() >= 2 {
        (args[1].as_str(), &args[2..])
    } else {
        (arg0, &args[1..])
    };

    let code = run(util, rest);
    ExitCode::from(code as u8)
}

fn is_util(name: &str) -> bool {
    matches!(
        name,
        "true"
            | "false"
            | "echo"
            | "cat"
            | "wc"
            | "head"
            | "ls"
            | "seq"
            | "basename"
            | "dirname"
            | "nl"
            | "pwd"
            | "env"
    )
}

/// Run `util` with `args`; return its exit code (0 = success).
fn run(util: &str, args: &[String]) -> i32 {
    match util {
        "true" => 0,
        "false" => 1,
        "echo" => echo(args),
        "cat" => cat(args),
        "wc" => wc(args),
        "head" => head(args),
        "ls" => ls(args),
        "seq" => seq(args),
        "basename" => basename(args),
        "dirname" => dirname(args),
        "nl" => nl(args),
        "pwd" => pwd(),
        "env" => env(),
        other => {
            let _ = writeln!(io::stderr(), "coreutils: unknown utility '{other}'");
            127
        }
    }
}

// -- helpers --

/// The final path component of `p` (like the `basename` of argv[0]).
fn base(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn out(bytes: &[u8]) -> i32 {
    match io::stdout().write_all(bytes) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// Read a named file, or stdin when the name is "-".
fn read_input(name: &str) -> io::Result<Vec<u8>> {
    if name == "-" {
        let mut v = Vec::new();
        io::stdin().read_to_end(&mut v)?;
        Ok(v)
    } else {
        std::fs::read(name)
    }
}

// -- utilities --

fn echo(args: &[String]) -> i32 {
    let mut newline = true;
    let mut start = 0;
    if let Some(first) = args.first() {
        if first == "-n" {
            newline = false;
            start = 1;
        }
    }
    let line = args[start..].join(" ");
    let mut s = line.into_bytes();
    if newline {
        s.push(b'\n');
    }
    out(&s)
}

fn cat(args: &[String]) -> i32 {
    if args.is_empty() {
        return match read_input("-") {
            Ok(v) => out(&v),
            Err(_) => 1,
        };
    }
    let mut rc = 0;
    for f in args {
        match read_input(f) {
            Ok(v) => {
                if out(&v) != 0 {
                    rc = 1;
                }
            }
            Err(e) => {
                let _ = writeln!(io::stderr(), "cat: {f}: {e}");
                rc = 1;
            }
        }
    }
    rc
}

fn counts(bytes: &[u8]) -> (usize, usize, usize) {
    let nbytes = bytes.len();
    let nlines = bytes.iter().filter(|&&b| b == b'\n').count();
    // Words: maximal runs of non-whitespace (ASCII whitespace, like coreutils
    // in the C locale).
    let mut nwords = 0;
    let mut in_word = false;
    for &b in bytes {
        let ws = matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
        if ws {
            in_word = false;
        } else if !in_word {
            in_word = true;
            nwords += 1;
        }
    }
    (nlines, nwords, nbytes)
}

fn wc(args: &[String]) -> i32 {
    // Flags: -l lines, -w words, -c bytes. With none, show all three.
    let mut want_l = false;
    let mut want_w = false;
    let mut want_c = false;
    let mut files: Vec<&String> = Vec::new();
    for a in args {
        match a.as_str() {
            "-l" => want_l = true,
            "-w" => want_w = true,
            "-c" => want_c = true,
            _ => files.push(a),
        }
    }
    if !want_l && !want_w && !want_c {
        want_l = true;
        want_w = true;
        want_c = true;
    }

    let render = |name: Option<&str>, l: usize, w: usize, c: usize| -> Vec<u8> {
        let mut parts: Vec<String> = Vec::new();
        if want_l {
            parts.push(l.to_string());
        }
        if want_w {
            parts.push(w.to_string());
        }
        if want_c {
            parts.push(c.to_string());
        }
        let mut s = parts.join(" ");
        if let Some(n) = name {
            s.push(' ');
            s.push_str(n);
        }
        s.push('\n');
        s.into_bytes()
    };

    if files.is_empty() {
        return match read_input("-") {
            Ok(v) => {
                let (l, w, c) = counts(&v);
                out(&render(None, l, w, c))
            }
            Err(_) => 1,
        };
    }
    let mut rc = 0;
    for f in files {
        match read_input(f) {
            Ok(v) => {
                let (l, w, c) = counts(&v);
                if out(&render(Some(f), l, w, c)) != 0 {
                    rc = 1;
                }
            }
            Err(e) => {
                let _ = writeln!(io::stderr(), "wc: {f}: {e}");
                rc = 1;
            }
        }
    }
    rc
}

fn head(args: &[String]) -> i32 {
    let mut n = 10usize;
    let mut files: Vec<&String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-n" {
            i += 1;
            if let Some(v) = args.get(i) {
                n = v.parse().unwrap_or(10);
            }
        } else {
            files.push(&args[i]);
        }
        i += 1;
    }

    let emit = |bytes: &[u8]| -> i32 {
        let mut count = 0;
        let mut end = 0;
        for (idx, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                count += 1;
                if count == n {
                    end = idx + 1;
                    break;
                }
            }
            end = idx + 1;
        }
        out(&bytes[..end])
    };

    if files.is_empty() {
        return match read_input("-") {
            Ok(v) => emit(&v),
            Err(_) => 1,
        };
    }
    let mut rc = 0;
    for f in &files {
        match read_input(f) {
            Ok(v) => {
                if emit(&v) != 0 {
                    rc = 1;
                }
            }
            Err(e) => {
                let _ = writeln!(io::stderr(), "head: {f}: {e}");
                rc = 1;
            }
        }
    }
    rc
}

fn ls(args: &[String]) -> i32 {
    let path = args.first().map(|s| s.as_str()).unwrap_or(".");
    // rheo has no cwd yet; treat "." as the root.
    let dir = if path == "." { "/" } else { path };
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            let _ = writeln!(io::stderr(), "ls: {dir}: {e}");
            return 1;
        }
    };
    let mut names: Vec<String> = Vec::new();
    for ent in rd {
        match ent {
            Ok(e) => names.push(e.file_name().to_string_lossy().into_owned()),
            Err(_) => return 1,
        }
    }
    names.sort();
    let mut s = String::new();
    for n in names {
        s.push_str(&n);
        s.push('\n');
    }
    out(s.as_bytes())
}

fn seq(args: &[String]) -> i32 {
    // seq LAST | seq FIRST LAST | seq FIRST STEP LAST (integers).
    let nums: Vec<i64> = args.iter().filter_map(|a| a.parse().ok()).collect();
    let (first, step, last) = match nums.len() {
        1 => (1, 1, nums[0]),
        2 => (nums[0], 1, nums[1]),
        3 => (nums[0], nums[1], nums[2]),
        _ => {
            let _ = writeln!(io::stderr(), "seq: usage: seq [FIRST [STEP]] LAST");
            return 1;
        }
    };
    if step == 0 {
        let _ = writeln!(io::stderr(), "seq: step must be nonzero");
        return 1;
    }
    let mut s = String::new();
    let mut v = first;
    while (step > 0 && v <= last) || (step < 0 && v >= last) {
        s.push_str(&v.to_string());
        s.push('\n');
        v += step;
    }
    out(s.as_bytes())
}

fn basename(args: &[String]) -> i32 {
    let Some(name) = args.first() else {
        let _ = writeln!(io::stderr(), "basename: missing operand");
        return 1;
    };
    let trimmed = name.trim_end_matches('/');
    let mut b = if trimmed.is_empty() {
        "/"
    } else {
        base(trimmed)
    };
    if let Some(suffix) = args.get(1) {
        if b != suffix.as_str() {
            if let Some(stripped) = b.strip_suffix(suffix.as_str()) {
                b = stripped;
            }
        }
    }
    out(format!("{b}\n").as_bytes())
}

fn dirname(args: &[String]) -> i32 {
    let Some(name) = args.first() else {
        let _ = writeln!(io::stderr(), "dirname: missing operand");
        return 1;
    };
    let trimmed = name.trim_end_matches('/');
    let d = match trimmed.rfind('/') {
        Some(0) => "/",
        Some(i) => &trimmed[..i],
        None => ".",
    };
    out(format!("{d}\n").as_bytes())
}

fn nl(args: &[String]) -> i32 {
    let name = args.first().map(|s| s.as_str()).unwrap_or("-");
    let bytes = match read_input(name) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(io::stderr(), "nl: {name}: {e}");
            return 1;
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut s = String::new();
    for (i, line) in text.lines().enumerate() {
        s.push_str(&format!("{:>6}\t{}\n", i + 1, line));
    }
    out(s.as_bytes())
}

fn pwd() -> i32 {
    out(b"/\n")
}

fn env() -> i32 {
    let mut s = String::new();
    for (k, v) in std::env::vars() {
        s.push_str(&format!("{k}={v}\n"));
    }
    out(s.as_bytes())
}
