# lsh — The Lattice Shell

**Status:** Draft v0.1. The first native Lattice application. Three components:
`latte-term` (terminal emulator cell), `lsh` (the shell), and `lattice-pty`
(the PTY-equivalent queue bridge). Written entirely in Rust. No POSIX
personality required for any of the three.

The central premise: a pipeline in bash is `fork + exec + pipe FDs`. A
pipeline in lsh is a **dependency graph submitted to the kernel**. This is
not a port of bash; it is a shell designed from first principles for what
this OS actually is.

---

## 1. Architecture — three cells

```
┌─────────────────────────────────────────────────────┐
│  latte-term (compositor client cell)                │
│  holds: display-engine grant, HID queue grants,    │
│         compositor surface capability               │
│  renders sealed UTF-8 surface → compositor          │
└──────────────┬──────────────────────────────────────┘
               │ PTY queue pair (bidirectional byte queue
               │ + control queue for resize/signals)
┌──────────────▼──────────────────────────────────────┐
│  lsh (shell runtime cell)                           │
│  holds: namespace view capability, object-store     │
│         read/write grants, cell-spawn grant,        │
│         cluster state-store read capability         │
└──────────────┬──────────────────────────────────────┘
               │ per-command queue pairs (stdin/stdout/stderr
               │ + capability handoff for typed pipelines)
┌──────────────▼──────────────────────────────────────┐
│  command cells  (one per foreground/background cmd) │
│  hold: exactly what lsh grants them — no more       │
└─────────────────────────────────────────────────────┘
```

### The PTY bridge (lattice-pty)

There is no kernel TTY subsystem. The PTY is a pair of native queue pairs:

```rust
pub struct Pty {
    /// Input: keystrokes from the terminal → shell
    input:   QueuePair<KeyEvent, u8>,
    /// Output: bytes from the shell → terminal for rendering
    output:  QueuePair<u8, ()>,
    /// Control: resize events, signal delivery, focus events
    control: QueuePair<PtyControlEvent, ()>,
}

pub enum PtyControlEvent {
    Resize    { cols: u16, rows: u16, pixel_w: u16, pixel_h: u16 },
    Signal    { sig: Signal },
    Focus     { gained: bool },
    Paste     { bracketed: bool },  // bracketed paste mode
}
```

TIOCGWINSZ → a typed `Resize` event. SIGWINCH → the same. SIGINT → a typed
`Signal` event on the control queue, not an OS signal. The shell maps these
to cell lifecycle operations or graph cancellations.

### latte-term — the terminal emulator cell

The emulator holds a compositor surface capability (GRAPHICS.md). Each frame:

1. Reads key events from HID queue.
2. Forwards bytes/events to lsh via the PTY queue pair.
3. Reads bytes from lsh's output queue.
4. Rasterises the VT sequence stream into a sealed pixel buffer using a
   custom GPU-accelerated text renderer (one tile per glyph, cached in HBM).
5. Hands the sealed buffer to the compositor cell as a read capability.
   Zero-copy: the compositor scans out what the terminal just rendered.

Supported: Unicode grapheme clusters (full UAX#29), true colour, sixel
graphics, kitty image protocol, OSC hyperlinks, dim/bold/italic/strikethrough,
all standard escape sequences. The rendering backend uses Vulkan compute
to rasterise the glyph atlas (the GPU is an engine; text rendering is just
another compute workload on the engine the terminal holds a partition grant for).

---

## 2. The shell language — lsh

The grammar is familiar enough that bash muscle memory carries over, but the
semantics are Lattice-native. The key difference: **every expression is an
expression in a dependency graph**, not a sequence of blocking syscalls.

### Pipelines are dependency graphs

```lsh
# Familiar syntax, native semantics:
# Each command becomes a cell; | builds a dependency graph edge;
# the whole pipeline is submitted as one graph.
ls /data/models | grep llama | sort -k2 | head -20

# The kernel sees:
#   Node 0: cell[ls]      → sealed UTF-8 buffer A
#   Node 1: cell[grep]    ← buffer A, → sealed buffer B
#   Node 2: cell[sort]    ← buffer B, → sealed buffer C
#   Node 3: cell[head]    ← buffer C, → stdout queue
# One graph submission. Kernel schedules nodes optimally.
```

```lsh
# Typed pipeline: |> hands a sealed typed buffer (not raw bytes)
# The producer declares its output type; the consumer declares its input type;
# the shell verifies compatibility at parse time.
load-model llama-70b.safetensors |> infer --batch 32 |> decode |> jsonl > results.jsonl
#                                 ^^                  ^^
#           Buffer<ModelOutput>        Buffer<TokenStream>  Buffer<JsonLines>
```

The shell's parser resolves command output/input types from the command's
declared type signature (stored as metadata in the object store alongside
the command binary). Type mismatches in a typed pipeline are a parse-time
error with a helpful message, not a runtime garbling of bytes.

### Capability grants from the prompt

```lsh
# Give this command explicit capability grants:
cargo build @[nvme:rw, cpu:8cores, ddr:4GB]

# Reservation: the kernel admits or rejects before the command starts
inference-server --model llama-70b @reserve[gpu:2slices, hbm:40GB, ttl:24h]

# Read-only access to an object by content hash
infer --weights sha256:abc123de @[obj:sha256:abc123de:read]

# Without @[...] the command gets the shell's default grant:
# namespace read/write, stdout, stderr — nothing else.
```

The `@[...]` syntax is how the POSIX `sudo` model is replaced: instead of
escalating privilege, you declare exactly what the command needs, the kernel
checks the shell holds those grants to delegate, and mints exactly that
capability set for the command cell. The command cannot exceed its declaration.

### Background cells and structured concurrency

```lsh
# Spawn a background cell; get a handle
build_job = cargo build &

# A handle is a capability to the cell's lifecycle and output queue
await $build_job                       # block until done
await $build_job within 5m             # with timeout → error on expiry
await $build_job, $test_job            # both complete
race $server_job, $timeout_job         # first to complete wins

# Cancel a running cell (sends a cancellation event; poison propagates)
cancel $build_job
```

### Graph blocks — explicit dependency graphs

```lsh
# For complex multi-stage pipelines with fan-out/fan-in:
result = graph {
    raw     = load /data/raw/*.parquet
    cleaned = clean-data $raw
    encoded = encode --model bert $cleaned
    indexed = build-index $encoded

    # Fan-in: both branches must complete before save
    save /data/output/index.bin $indexed
}

# Submit the graph and get a handle
await $result within 30m | trace --full  # pipe the completion into a tracer
```

Inside a `graph { }` block, assignments are graph nodes, not sequential
statements. The shell's compiler analyses data dependencies between variables
and generates the dependency-graph submission automatically. No explicit
`depends_on`; the data flow is the dependency.

### Resource declarations in scripts

```lsh
#!/usr/bin/lsh

# Script-level resource declaration — kernel admits or rejects at startup
needs {
    capability: [nvme:read, gpu:execute, obj-store:rw]
    memory:     { hbm: 16GB, ddr: 4GB }
    engines:    [gpu:2slices, nvme:1ns, nic:1queue]
    ttl:        4h
}

# If the kernel cannot satisfy `needs`, the script exits with a clear
# error before doing any work — no partial execution, no hidden OOM.
```

### First-class object store interaction

```lsh
# The object store is the filesystem, from the shell's perspective
obj ls --type ModelObject               # list objects of a type
obj ls /data/models/**/*.safetensors   # glob over namespace view
obj put results.json --content-type application/json
obj get sha256:abc123 > local.json     # by content hash
obj stat model.safetensors             # metadata: hash, size, kind, version

# Objects as typed arguments (shell resolves the capability automatically)
infer --model @obj:llama-70b           # shell grants the command a read cap
                                       # to the named object; no path string
```

### Built-in cell management

```lsh
cells list                             # all cells in the current session
cells list --all                       # all cells the shell has view caps for
cells inspect $build_job               # live GRAPH_INSPECT output
cells trace --flow a3f2b901            # fetch the OTel span tree
cells kill $build_job                  # cancel + wait for teardown
caps list                              # capabilities the shell currently holds
caps inspect cap:4a2b                  # what this capability grants
```

---

## 3. Tab completion — type-system-aware

Tab completion is not glob expansion over strings. It is **typed completion**:
the shell knows the declared type of each argument position and queries the
appropriate source.

```
$ infer --model <Tab>
  llama-3-70b.safetensors     [ModelObject  sha256:3a4b]
  mistral-7b.safetensors      [ModelObject  sha256:9c1d]
  bert-base.safetensors       [ModelObject  sha256:77fe]
  (3 objects of type ModelObject in namespace /data/models)

$ caps grant <Tab>
  nvme:read    nvme:write    gpu:execute    obj-store:read    nic:queue
  (capabilities the shell holds and can delegate)

$ cells inspect <Tab>
  build_job    inference-server    compaction-bg
  (live cells in the current session)

$ @reserve[gpu:<Tab>
  1slice       2slices      4slices      whole
  (available GPU partition sizes on the current host)
```

The completion engine queries three sources:
1. **Command metadata** — each command declares argument types in its object-
   store metadata; the shell reads this to know what type `--model` expects.
2. **Object store typed query** — for object-type arguments, the shell queries
   the object store with a type filter, returning names/hashes.
3. **Capability registry** — for `@[...]` blocks, the shell queries which
   capability classes it currently holds and can delegate.

All three queries are async (queue submissions); the completion UI shows
results as they arrive with a spinner for slow object-store queries.

---

## 4. TUI support — Lattice-native

The shell's TUI model is different from ncurses or Ratatui over a PTY. Since
the terminal emulator is a Lattice cell with a compositor surface, the shell
can render **structured surfaces** rather than escape sequences.

A TUI application:
1. Requests a sub-surface capability from the terminal emulator.
2. Renders into its own sealed pixel buffer independently.
3. Hands the buffer to the terminal cell via capability handoff.
4. The terminal composes it into its surface; the compositor scans out.

This means a TUI app running inside the shell gets **hardware-accelerated
rendering** (its render target goes through Vulkan) and zero-copy composition
(no escape-sequence encoding and decoding; the pixels go straight to the
compositing stage).

For the shell's own interactive elements (the readline, completion overlay,
graph progress bars):

```
┌─────────────────────────────────────────────────────┐
│ /data/models > infer --model llama-70b              │  ← readline layer
├─────────────────────────────────────────────────────┤
│ 🔵 Graph: infer --batch 32                          │  ← progress overlay
│    ├─ [EXEC ] load-model       ██████░░░░  62%      │
│    ├─ [PEND ] prefill          waiting              │
│    └─ [PEND ] decode           waiting              │
├─────────────────────────────────────────────────────┤
│ > ls | grep llama | wc -l                           │  ← history
│   3                                                 │
│ > obj ls --type ModelObject                         │
│   llama-70b    mistral-7b    bert-base              │
└─────────────────────────────────────────────────────┘
```

The progress overlay is driven by live `GRAPH_INSPECT` queries on running
cells — the same API the CLI tooling uses. The shell subscribes to completion
events on running graphs and redraws the overlay as nodes change state.

---

## 5. Rust crate structure

```
lsh-workspace/
├── lattice-pty/          # PTY queue-pair abstraction
│   ├── src/
│   │   ├── lib.rs        # Pty, PtyControlEvent, KeyEvent types
│   │   └── bridge.rs     # PTY server (runs in latte-term) + client (runs in lsh)
│   └── Cargo.toml
│
├── latte-term/           # Terminal emulator cell
│   ├── src/
│   │   ├── main.rs       # Cell entry point; holds compositor + HID grants
│   │   ├── vt.rs         # VT sequence parser (VT100, xterm, kitty protocol)
│   │   ├── render/
│   │   │   ├── glyph.rs  # Glyph atlas, Unicode shaping (via rustybuzz)
│   │   │   ├── grid.rs   # Terminal grid (cell grid, damage tracking)
│   │   │   └── gpu.rs    # Vulkan compute rasteriser for the glyph atlas
│   │   └── input.rs      # HID queue → KeyEvent translation
│   └── Cargo.toml
│
├── lsh/                  # The shell
│   ├── src/
│   │   ├── main.rs       # Shell cell entry point
│   │   ├── parser/
│   │   │   ├── lexer.rs  # Tokeniser
│   │   │   ├── grammar.rs# PEG grammar (via peg or pest)
│   │   │   └── ast.rs    # AST: Pipeline, GraphBlock, CapGrant, TypedPipe...
│   │   ├── compiler/
│   │   │   ├── graph.rs  # AST → dependency graph (GraphSubmission)
│   │   │   ├── types.rs  # Type checker for typed pipelines (|>)
│   │   │   └── caps.rs   # Capability grant elaboration
│   │   ├── runtime/
│   │   │   ├── exec.rs   # Cell spawn, graph submit, await/cancel
│   │   │   ├── job.rs    # Job table: background cells + handles
│   │   │   └── env.rs    # Shell environment: namespace view, variables
│   │   ├── completion/
│   │   │   ├── engine.rs # Async completion query orchestrator
│   │   │   ├── obj.rs    # Object-store typed completion source
│   │   │   ├── cap.rs    # Capability completion source
│   │   │   └── cmd.rs    # Command argument type completion source
│   │   ├── readline/
│   │   │   ├── editor.rs # Line editor: grapheme-aware, syntax highlighting
│   │   │   ├── history.rs# History: persisted as append-log objects
│   │   │   └── hint.rs   # Inline hints (fish-style, from history/types)
│   │   └── tui/
│   │       ├── surface.rs# Sub-surface capability request + render
│   │       ├── widgets/
│   │       │   ├── progress.rs # Graph progress overlay
│   │       │   ├── completion.rs # Completion popup
│   │       │   └── inspect.rs  # Inline cell/graph inspector
│   │       └── layout.rs # Surface layout (readline + overlay + history)
│   └── Cargo.toml
│
└── lsh-sdk/              # Library for writing lsh-native commands
    ├── src/
    │   ├── command.rs    # CommandMeta: declare argument types, output types
    │   ├── typed_io.rs   # TypedInput<T>, TypedOutput<T> for |> pipelines
    │   └── progress.rs   # Progress reporting into the shell's overlay
    └── Cargo.toml
```

---

## 6. Key implementation details

### The line editor — grapheme-cluster-aware from day one

```rust
// lsh/src/readline/editor.rs

use unicode_segmentation::UnicodeSegmentation;

pub struct LineEditor {
    /// The buffer is a Vec of grapheme clusters, not bytes or chars.
    /// This handles emoji, combining marks, CJK, etc. correctly.
    graphemes: Vec<String>,
    cursor:    usize,          // cursor position in grapheme clusters
    viewport:  usize,          // left edge of visible window (for long lines)
    syntax:    SyntaxHighlighter,
    hints:     HintProvider,
}

impl LineEditor {
    pub fn insert(&mut self, text: &str) {
        for g in text.graphemes(true) {
            self.graphemes.insert(self.cursor, g.to_owned());
            self.cursor += 1;
        }
        self.invalidate(); // trigger redraw
    }

    pub fn move_word_right(&mut self) {
        // Skip whitespace, then skip word characters — operates on graphemes,
        // not bytes. Handles "café" (5 graphemes) correctly.
        while self.cursor < self.graphemes.len()
            && self.graphemes[self.cursor].chars().all(|c| c.is_whitespace())
        {
            self.cursor += 1;
        }
        while self.cursor < self.graphemes.len()
            && !self.graphemes[self.cursor].chars().all(|c| c.is_whitespace())
        {
            self.cursor += 1;
        }
    }

    /// Render the current line into the terminal surface.
    /// Returns a damage region so the terminal only redraws what changed.
    pub fn render(&self, surface: &mut TermSurface) -> DamageRegion {
        let highlighted = self.syntax.highlight(&self.graphemes);
        surface.write_line(highlighted, self.cursor, self.viewport)
    }
}
```

### The AST and pipeline compilation

```rust
// lsh/src/parser/ast.rs

pub enum Expr {
    /// Simple command with arguments and capability grants
    Command {
        name:      String,
        args:      Vec<Arg>,
        grants:    Vec<CapGrant>,       // from @[...]
        reserves:  Vec<Reservation>,   // from @reserve[...]
    },
    /// Raw-byte pipeline — nodes connected by byte queues
    Pipeline {
        stages: Vec<Expr>,
    },
    /// Typed pipeline — nodes connected by sealed typed buffers
    TypedPipeline {
        stages: Vec<Expr>,
    },
    /// Explicit dependency graph block
    GraphBlock {
        nodes: Vec<(String, Expr)>,     // name = expr
    },
    /// Background execution — returns a JobHandle
    Background(Box<Expr>),
    /// Await a job handle
    Await {
        handle:  Expr,
        timeout: Option<Duration>,
    },
    Redirect { expr: Box<Expr>, target: RedirectTarget },
    VarRef(String),
    Literal(Value),
}

// lsh/src/compiler/graph.rs

impl GraphCompiler {
    /// Compile a Pipeline into a GraphSubmission.
    /// Each stage becomes a graph node; edges are the queue pairs between them.
    pub fn compile_pipeline(
        &mut self,
        stages: &[Expr],
    ) -> Result<GraphSubmission, CompileError> {
        let mut nodes   = Vec::new();
        let mut edges   = Vec::new();
        let mut prev_output: Option<BufferSlot> = None;

        for (i, stage) in stages.iter().enumerate() {
            let node = self.compile_command_node(stage, i)?;

            if let Some(prev) = prev_output.take() {
                // Wire the previous stage's stdout buffer to this stage's stdin
                edges.push(GraphEdge {
                    from:        prev,
                    to:          node.stdin_slot,
                    buffer_kind: MemoryKind::Ddr,  // raw bytes: DDR is fine
                });
            }

            prev_output = Some(node.stdout_slot);
            nodes.push(node);
        }

        // The last stage's stdout → the shell's terminal output queue
        if let Some(last_out) = prev_output {
            nodes.last_mut().unwrap().output_target =
                OutputTarget::TerminalQueue(self.stdout_queue.clone());
        }

        Ok(GraphSubmission { nodes, edges, flow_id: self.session_flow_id() })
    }

    /// Compile a GraphBlock: analyse data-flow dependencies between
    /// named variables and build the dependency edges automatically.
    pub fn compile_graph_block(
        &mut self,
        bindings: &[(String, Expr)],
    ) -> Result<GraphSubmission, CompileError> {
        // Build a map: variable name → graph node index
        let mut var_map: HashMap<String, NodeId> = HashMap::new();
        let mut nodes = Vec::new();

        for (name, expr) in bindings {
            // Find which variables this expression reads
            let deps: Vec<NodeId> = expr.var_refs()
                .filter_map(|v| var_map.get(v).copied())
                .collect();

            let node = self.compile_command_node(expr, nodes.len())?;
            let id   = node.id;
            nodes.push(node);
            var_map.insert(name.clone(), id);

            // Register dependency edges from the vars this node reads
            for dep in deps {
                // The compiler inserts a sealed-buffer edge automatically
                // based on the producer's declared output type
            }
        }

        Ok(GraphSubmission { nodes, edges: self.inferred_edges, .. })
    }
}
```

### Typed pipeline type checking

```rust
// lsh/src/compiler/types.rs

/// Each command declares its input/output types in its object-store metadata.
/// The type checker validates |> pipelines at parse time.
pub fn check_typed_pipeline(stages: &[CommandMeta]) -> Result<(), TypeError> {
    for window in stages.windows(2) {
        let producer = &window[0];
        let consumer = &window[1];

        match (&producer.output_type, &consumer.input_type) {
            (Some(out), Some(inp)) if out != inp => {
                return Err(TypeError::Mismatch {
                    stage:    producer.name.clone(),
                    produced: out.clone(),
                    expected: inp.clone(),
                    hint:     suggest_adapter(out, inp),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

// Error message quality — this is user-facing:
// error: type mismatch in typed pipeline
//   infer --batch 32 |> jsonl
//                    ^^ infer produces Buffer<TokenStream>
//                       but jsonl expects Buffer<JsonLines>
//   hint: try `infer --batch 32 |> decode |> jsonl`
//              ~~~~~~~~~~~~~~~~~~~~~~~~ inserts the missing decode stage
```

### The completion engine — async, multi-source

```rust
// lsh/src/completion/engine.rs

pub struct CompletionEngine {
    obj_source: ObjStoreSource,
    cap_source: CapabilitySource,
    cmd_source: CommandMetaSource,
    hist_source: HistorySource,
}

impl CompletionEngine {
    /// Submit all relevant completion queries in parallel (as queue submissions)
    /// and stream results back to the UI as they arrive.
    pub async fn complete(
        &self,
        ctx: &CompletionContext,
    ) -> impl Stream<Item = CompletionItem> {
        let (tx, rx) = channel();

        // Determine which sources apply based on parse context:
        // - After --model: query obj store for ModelObject type
        // - After @[: query capabilities the shell holds
        // - After a command name: query PATH + obj store for executables
        // - Anywhere: history completions
        let sources: Vec<BoxedSource> = match ctx.position {
            ArgPosition::Named { arg_name } => {
                let arg_type = ctx.command_meta
                    .and_then(|m| m.arg_type(arg_name));
                match arg_type {
                    Some(ArgType::Object(obj_type)) =>
                        vec![self.obj_source.for_type(obj_type), self.hist_source.clone()],
                    Some(ArgType::Path) =>
                        vec![self.obj_source.for_namespace(), self.hist_source.clone()],
                    Some(ArgType::Capability) =>
                        vec![self.cap_source.clone()],
                    None =>
                        vec![self.hist_source.clone()],
                }
            }
            ArgPosition::CapBlock =>
                vec![self.cap_source.for_delegation()],
            ArgPosition::CommandName =>
                vec![self.cmd_source.clone(), self.hist_source.clone()],
            _ =>
                vec![self.hist_source.clone()],
        };

        // Spawn all sources concurrently — each is a queue submission
        for source in sources {
            let tx = tx.clone();
            spawn_strand(async move {
                let items = source.query(ctx).await;
                for item in items { tx.send(item).ok(); }
            });
        }

        rx
    }
}
```

### History — persisted as append-log objects

```rust
// lsh/src/readline/history.rs

/// Shell history is an append-log object in the object store.
/// Each entry carries: timestamp (HLC), command text, session cell ID,
/// exit status, flow ID (so you can re-trace any historical command).
#[derive(Serialize)]
pub struct HistoryEntry {
    pub ts:       HlcTimestamp,     // HLC — causally ordered across sessions
    pub command:  String,
    pub session:  CellId,
    pub status:   ExitStatus,
    pub flow_id:  FlowId,           // link to the OTel span tree
    pub duration: Duration,
}

pub struct History {
    log: AppendLogObject,           // native object store primitive
}

impl History {
    pub fn push(&self, entry: HistoryEntry) -> Result<(), ObjError> {
        // durability class: ordered (persist before next prompt; no flush wait)
        self.log.append(entry, DurabilityClass::Ordered)
    }

    /// Retrieve history entries matching a query.
    /// Since history is a typed append-log, this is a range scan —
    /// not grep over a text file.
    pub fn search(&self, query: HistoryQuery) -> impl Iterator<Item = HistoryEntry> {
        self.log.scan(query.time_range)
            .filter(move |e| query.matches(e))
    }
}
```

History is stored as a typed append-log object (FILESYSTEMS.md 3), which means:
- Persists across reboots with the same durability as any other object.
- Searchable by time range, session, command text, or exit status as a typed query.
- Any historical command's `flow_id` links directly to its OTel span tree —
  `ctrl-R` in the shell can show you not just the command but how long it
  took per stage, which is a qualitatively different debugging experience.

### The lsh-sdk — writing lsh-native commands

Any Rust program that links against `lsh-sdk` is a first-class lsh citizen:
it declares its types, gets typed I/O, and can report progress into the
shell's live graph overlay.

```rust
// An example lsh-native command: 'infer'
// It declares its I/O types so lsh can type-check |> pipelines
// and provide typed tab completion.

use lsh_sdk::{command, TypedInput, TypedOutput, Progress};
use lattice_types::{ModelObject, TokenStream};

#[command(
    name = "infer",
    input  = "ModelObject",       // enables |> from load-model
    output = "TokenStream",       // enables |> to decode
    args   = [
        ("--batch", ArgType::USize, "Batch size (default: 1)"),
        ("--max-tokens", ArgType::USize, "Max output tokens"),
    ]
)]
async fn main(
    input:    TypedInput<ModelObject>,
    output:   TypedOutput<TokenStream>,
    progress: Progress,
    args:     Args,
) -> Result<(), Error> {
    let model   = input.recv().await?;
    let batch   = args.get::<usize>("--batch").unwrap_or(1);

    progress.set_total(args.get::<usize>("--max-tokens").unwrap_or(512));

    for token in run_inference(&model, batch) {
        progress.increment(1);
        output.send(token).await?;
    }
    Ok(())
}
```

The `#[command]` macro:
1. Registers the command's metadata in the object store at install time
   (name, argument types, input/output types).
2. Generates the cell entry point (reads grants from the capability set,
   sets up the typed I/O queues, calls `main`).
3. Makes the command discoverable by the completion engine.

---

## 7. What this means for scripting

A Lattice shell script is different from bash in one key way: it can express
*resource requirements and data types* that bash cannot:

```lsh
#!/usr/bin/lsh

# Dataset pre-processing pipeline
# Declare what we need — kernel admits before we start
needs {
    engines:    [nvme:1ns, gpu:2slices, nic:1queue]
    memory:     { hbm: 8GB, ddr: 32GB }
    capability: [obj-store:rw, nvme:rw, gpu:execute]
}

model = obj get sha256:$MODEL_HASH          # typed: ModelObject

# Build a graph — not a sequence, a dependency graph
# The compiler figures out the parallelism automatically
results = graph {
    raw      = obj ls /data/raw --type ParquetObject
    cleaned  = clean-data $raw --output-type ParquetObject
    encoded  = encode-batch --model $model $cleaned
    indexed  = build-index $encoded
    _        = obj put /data/output/index.bin $indexed
}

await $results within 2h

# The flow_id of $results links to the full OTel span tree for the run
echo "Done. Trace: $(trace url $results)"
```

### Comparison: the same work in bash

```bash
#!/bin/bash
# No type checking, no resource declaration, no graph — just hope
set -euo pipefail

MODEL_PATH="/data/models/llama-70b.safetensors"
RAW_DIR="/data/raw"
OUT_DIR="/data/output"

# Sequential — no parallelism expressed
for f in "$RAW_DIR"/*.parquet; do
    python3 clean.py "$f" >> "$OUT_DIR/cleaned.parquet"
done
python3 encode.py --model "$MODEL_PATH" "$OUT_DIR/cleaned.parquet" > "$OUT_DIR/encoded.bin"
python3 build_index.py "$OUT_DIR/encoded.bin" > "$OUT_DIR/index.bin"
```

The lsh version expresses the parallelism (the graph compiler extracts it),
declares the resource requirements (the kernel enforces them), uses typed
objects (the object store tracks them), and produces a traceable span tree
(the OTel system records it). The bash version does none of those things.

---

## 9. Channels — stdin, stdout, stderr, and beyond

### The POSIX problem

POSIX gives every process three anonymous, untyped byte streams: fd 0
(stdin), fd 1 (stdout), fd 2 (stderr). Everything else is convention:
progress bars abuse stderr, log output mixes with data output, JSON and
human-readable text share the same fd 1, and distinguishing an error from
a warning requires parsing bytes the program chose to emit.

In Lattice, **channels are named, typed, capability-gated queue pairs**.
A command cell's I/O is not "file descriptors the kernel opened"; it is a
set of explicitly granted channel capabilities the shell mints at spawn time.
The command can only use what it was granted.

### The channel set

```rust
/// The full set of channels a command cell may hold.
/// The shell grants a subset based on the command's declared needs
/// and the current context (pipeline, interactive, script).
pub struct CellChannels {
    /// Typed input from the previous pipeline stage (or the terminal for
    /// interactive programs). For batch/pipeline: Buffer<InputType>.
    /// For interactive: raw KeyEvent queue from the PTY.
    pub stdin:    StdinChannel,

    /// Typed output to the next pipeline stage or terminal.
    /// Raw bytes for POSIX-compatible commands; typed buffers for |> pipelines.
    pub stdout:   StdoutChannel,

    /// Structured diagnostics — NOT raw bytes. Always present.
    pub stderr:   Sender<Diagnostic>,

    /// Progress events — separate from diagnostics, optional.
    pub progress: Option<Sender<ProgressEvent>>,

    /// Structured log events — flows into the OTel event stream.
    pub log:      Option<Sender<LogEvent>>,

    /// Exit completion — the command's final status, typed.
    pub exit:     ExitSender,
}
```

The shell subscribes to all channels at the time it spawns the command cell.
A channel the shell did not subscribe to is a no-op for the command: the
`progress` sender is cheaply dropped if the shell didn't request it. No cost
for channels not in use.

### stderr — structured diagnostics, not raw bytes

```rust
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub level:     DiagLevel,           // error | warning | info | hint | debug
    pub code:      Option<u32>,         // machine-readable code, e.g. E0001
    pub message:   String,              // the primary message
    pub span:      Option<DiagSpan>,    // file:line:col if applicable
    pub labels:    Vec<DiagLabel>,      // secondary spans with messages
    pub notes:     Vec<String>,         // `note:` annotations
    pub helps:     Vec<String>,         // `help:` suggestions
    pub flow_id:   FlowId,              // which request caused this
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagLevel { Debug, Info, Hint, Warning, Error, Fatal }

pub struct DiagSpan {
    pub path:   ObjectPath,             // file or object path
    pub line:   u32,
    pub col:    u32,
    pub len:    u32,
}
```

The terminal emulator renders `Diagnostic` events with proper colour and
structure — the same format rustc pioneered, promoted to a channel:

```
error[E0001]: model not found
  --> /data/models/llama-70b.safetensors
   |
   = note: object sha256:3a4b... does not exist in namespace /data/models
   = help: available models:
           · llama-3-70b.safetensors   (sha256:9c1d...)
           · mistral-7b.safetensors    (sha256:77fe...)
```

The command never embeds ANSI escape codes. It emits a `Diagnostic` with
`code=1`, `message="model not found"`, `notes=[...]`, `helps=[...]`. The
terminal renders it with colour; a log sink stores it as structured JSON;
a CI runner formats it as GitHub annotations. Same event, multiple renderers,
no parsing.

### progress — a first-class channel

```rust
#[derive(Debug, Clone)]
pub struct ProgressEvent {
    pub id:      u64,                   // stable ID for this progress bar
    pub label:   String,                // "Loading weights" / "Compiling" etc.
    pub current: u64,
    pub total:   Option<u64>,           // None = indeterminate
    pub unit:    ProgressUnit,          // Bytes | Items | Percent | Custom(str)
    pub detail:  Option<String>,        // current item name, ETA, etc.
}
```

Any command that emits `ProgressEvent`s on its `progress` channel gets a
live progress bar in the shell's graph overlay automatically. No special
integration code. The shell subscribes to the channel; the overlay renders
updates as they arrive. Progress from every stage of a pipeline appears in
the same overlay, labelled by stage.

```
█ Running: cargo build | wasm-pack | copy-assets
  ├─ cargo build      ████████████░░░░  73%  (compiling lattice-pty)
  ├─ wasm-pack        waiting
  └─ copy-assets      waiting
```

### log — structured, correlated with flow IDs

```rust
#[derive(Debug, Clone)]
pub struct LogEvent {
    pub level:   LogLevel,
    pub target:  String,               // "inference::sampler", "storage::gc"
    pub message: String,
    pub fields:  Vec<(String, Value)>, // structured key-value pairs
    pub flow_id: FlowId,               // links to the OTel span tree
    pub ts:      HlcTimestamp,
}
```

Log events flow directly into the OTel event stream with `flow_id` attached.
Every log line a command emits is automatically correlated with the request
or pipeline invocation that caused it — without the command knowing anything
about how lsh is routing its logs.

The shell's default behaviour: log events go to the OTel exporter silently.
An operator can redirect them: `cmd log> debug.log` or `cmd log.level>=warn`
to suppress below a threshold.

### Exit — typed, not an integer

```rust
pub enum CellExit {
    Success,
    Failure {
        code:        u32,
        summary:     String,
        /// Key diagnostics that caused the failure — the shell renders these
        /// inline rather than making the user scroll back through stderr
        diagnostics: Vec<Diagnostic>,
    },
    Cancelled { by: CancellationSource },
    Timeout   { after: Duration },
    OomKilled,                          // exhausted memory grant
    BudgetKilled { resource: ResourceKind },
}
```

The shell renders each variant differently. A `Failure` with structured
`diagnostics` shows the key errors inline at the prompt — no scrolling back.
A `Timeout` shows how long it ran. A `BudgetKilled` tells the user exactly
which resource ran out.

### Interactive vs. pipeline stdin

The shell determines stdin mode at spawn time:

```rust
pub enum StdinChannel {
    /// For pipeline commands: a sealed buffer from the previous stage,
    /// or a typed object for the first stage of a typed (|>) pipeline.
    Buffer(SealedBuffer),

    /// For interactive commands (vim, htop, python REPL):
    /// the raw key-event queue from the PTY bridge.
    /// The command controls cursor movement, raw mode, etc. via control events.
    Interactive(PtyInputQueue),

    /// Null — no input (equivalent to /dev/null)
    Null,
}
```

A command in a pipeline gets `Buffer`; a command run directly at the prompt
with no pipeline context gets `Interactive`. The shell decides based on
whether the command is the first node in a pipeline without a producer.

### Channel redirection syntax

```lsh
# Standard — identical to bash semantics, different implementation
cmd > file.txt              # stdout → object store (append-log object)
cmd < input.txt             # stdin from object (typed if declared)
cmd >> log.txt              # stdout → append to existing log object

# The new channel operators
cmd 2> errors.log           # stderr (structured diagnostics) → log object
cmd 2.warn errors.log       # stderr, warnings only → log object
cmd 2.error > /dev/null     # suppress only errors (keep warnings, info)
cmd log> debug.log          # log channel → object
cmd progress> /dev/null     # suppress progress display

# Typed pipe for the stderr channel (pipe diagnostics into a viewer)
cargo build 2>|> annotate-diag | diag-viewer --group-by-file

# Merge stderr into stdout (POSIX compat — produces raw bytes on stdout)
cmd 2>&1

# Capture stderr as a typed variable
{out, diag} = cmd           # out: stdout bytes, diag: Vec<Diagnostic>

# Filter by diagnostic level in a pipeline
cmd 2>|> filter-diag --level error |> report-errors
```

### What the lsh-sdk emits — from the command's perspective

```rust
use lsh_sdk::{Diagnostic, DiagLevel, ProgressEvent, LogEvent, ExitStatus};

async fn run(ctx: CmdContext) -> Result<(), Error> {
    // Emit a warning — goes to the structured stderr channel
    ctx.warn(Diagnostic {
        level:   DiagLevel::Warning,
        code:    Some(42),
        message: "config file not found, using defaults".into(),
        helps:   vec!["create ~/.lshrc to customise".into()],
        ..Default::default()
    });

    // Emit progress — appears in the shell overlay automatically
    let pb = ctx.progress("Processing", total_items);
    for item in items {
        process(item)?;
        pb.increment(1);
    }

    // Emit a structured log event — goes to OTel, correlated with flow_id
    ctx.log(LogLevel::Debug, "storage::gc", "compaction complete",
        &[("freed_bytes", Value::U64(freed)), ("duration_ms", Value::U64(elapsed))]);

    // On error: emit a rich diagnostic and return Failure
    if something_wrong {
        ctx.error(Diagnostic {
            level:   DiagLevel::Error,
            code:    Some(1),
            message: "failed to process batch".into(),
            notes:   vec![format!("processed {} of {} items", done, total)],
            ..Default::default()
        });
        return Err(Error::Fatal);
    }

    Ok(())
}
```

The `lsh-sdk` handles routing each event to the right channel. The command
author never calls `eprintln!` or formats ANSI codes. They emit typed events.

### How the shell aggregates across a pipeline

When a multi-stage pipeline runs, the shell collects all channel events from
all stages and presents them coherently at completion:

```
$ cargo build | wasm-pack | deploy
  ✓  cargo build        18.3s
  ✓  wasm-pack           4.1s
  ✗  deploy              0.8s  failed

2 warnings, 1 error:

  warning[W0042]  cargo build
    config not found; using defaults

  error[E0503]  deploy
    remote host unreachable: cluster.example.com:8443
    = note: tried 3 times over 800ms
    = help: check cluster connectivity with `cells ping cluster.example.com`
```

Without structured channels this requires parsing stderr bytes from three
processes, correlating them with timing data, and formatting a summary. With
structured channels it is a fold over a `Vec<Diagnostic>` from each stage's
`stderr` channel — three lines of code.

### Backward compatibility — POSIX fd 1/2 translation

Commands running under the POSIX personality write to fd 1 and fd 2 as
raw bytes. The POSIX personality bridge translates:

- fd 1 writes → `StdoutChannel::Buffer` byte queue (same as before)
- fd 2 writes → `Diagnostic { level: Warning, message: <utf-8 bytes>, code: None }`
  events on the stderr channel — typed, but without structured fields

So even a bash script's `echo "something went wrong" >&2` shows up in the
structured stderr channel. It has no `code`, no `span`, no `help` — but it
is still a `Diagnostic`, and the shell renders it consistently with native
diagnostics. The structured richness degrades gracefully; the channel model
does not degrade at all.


For existing bash scripts, two paths:

1. **POSIX personality + bash binary** — run bash under the POSIX personality
   with a synthesised filesystem namespace (POSIX-PERSONALITY.md). This works
   for ~80% of scripts without changes. The shell that invoked it is lsh;
   bash runs as a POSIX child cell.

2. **`lsh --bash-compat script.sh`** — lsh's parser falls back to a bash-
   compatible mode that accepts bash syntax and maps it to lsh semantics
   where possible (pipelines → graphs, background jobs → background cells),
   and falls through to the POSIX personality for constructs that cannot be
   translated (process substitution with `<(...)`, specific built-in
   behaviours).

The goal is that someone can `chmod +x script.lsh` and have it work, and can
also `lsh --bash-compat old-script.sh` and have that work for the common 80%.
The 20% that doesn't work is documented, not hidden.
