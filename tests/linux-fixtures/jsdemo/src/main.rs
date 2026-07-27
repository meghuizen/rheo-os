// A real JavaScript engine (pure-Rust `boa`) running unmodified on rheo-os under
// the Linux personality - the on-goal proxy for Node/Claude Code. Evaluates real
// JS (arithmetic, strings, an array reduce, a closure) and prints a deterministic
// result, exercising the engine's parser, bytecode VM, heap and GC.
use boa_engine::{Context, Source};

fn main() {
    let code = r#"
        function add(a, b) { return a + b; }
        let xs = [1, 2, 3, 4, 5, 6];
        let sum = xs.reduce((a, x) => add(a, x), 0);
        let s = "rheo";
        s + ":" + (sum * 2)
    "#;
    let mut ctx = Context::default();
    match ctx.eval(Source::from_bytes(code)) {
        Ok(v) => {
            let s = v.to_string(&mut ctx).unwrap();
            println!("js: {}", s.to_std_string_escaped());
        }
        Err(e) => {
            println!("js error: {e}");
            std::process::exit(1);
        }
    }
}
