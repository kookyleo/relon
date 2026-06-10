# Relon

**Logic as data.** Relon is an executable data format: business
logic — validation rules, pricing formulas, workflow steps — is written
once and stored like JSON, then evaluated by an embeddable, sandboxed
runtime. Determinism is part of the design: same source + same input →
byte-identical output, no floating-point quirks, no iteration-order
leaks, no implicit ambient state.

## 🚀 Quick Start

### Build the CLI
```bash
cargo build --release
```

### Run an Example
Use the `relon-cli` to evaluate a file and output JSON:
```bash
cargo run -p relon-cli -- run examples/demo.relon
```

The CLI runs **sandboxed by default**: only `std/*` imports resolve and
capability-gated native fns are denied. If your script needs local
`#import "./lib.relon"` paths or registered host fns that touch FS /
network, pass `--trust`:
```bash
cargo run -p relon-cli -- run fixtures/modules/main.relon --trust
```

### Local Validation
Run the full test suite and strict lint gate before shipping changes:
```bash
cargo test
cargo clippy --workspace --all-targets -- -D warnings
cargo run -q -p relon-fmt -- --check fixtures/*.relon fixtures/modules/*.relon fixtures/errors/*.relon examples/*.relon
```

CI on GitHub Actions enforces the same four checks on every PR, plus
a separate `cargo build` job against the pinned MSRV (`1.92`) so
toolchain drift surfaces early.

See [`SECURITY.md`](./SECURITY.md) for the sandbox threat model and
how to report vulnerabilities.

After a fresh clone, install the repository's git hooks once:
```bash
./scripts/install-hooks.sh
```
The pre-commit hook lists every staged file before each commit so
authors can spot accidental cross-task scope creep (common in
parallel workflows). It's advisory — never blocks.

## 🛠 Features

- **Sandboxed by default**: a script declares the capabilities it needs
  (`reads_fs` / `writes_fs` / `network` / `reads_clock` / `reads_env` /
  `uses_rng`; native fns are gated by the same capability bits); the
  host grants them.
  Scripts can't elevate themselves; the host can choose to grant all
  caps explicitly via `--trust` / `Capabilities::all_granted()`, and
  that grant is auditable code at the call site rather than an implicit
  trusted mode.
- **Self-describing schemas**: `#schema` records and `#enum` tagged
  enums, recursive contracts, branded values — type information travels
  with the payload.
- **Context-aware references**: `&root`, `&sibling`, `&prev`, `&next`
  let logic reference its surrounding data without hard-coded paths.
- **Functional core**: arrow closures (`(x) => x + 1`) and method
  shorthands (`f(x): x + 1`), comprehensions, pipes, pattern match —
  pure expressions, no IO or side effects.
- **Canonical std**: `#import list from "std/list"` is part of the
  language, not a host extension — scripts can rely on it without the
  embedder wiring anything up.

## 📖 Example

```javascript
{
    currency(val, symbol): val + " " + symbol,

    price: 100,

    @currency("USD")
    display: &sibling.price
}
```

## 📚 Documentation

- **Language spec**:
  [`docs/zh/guide/spec.md`](docs/zh/guide/spec.md) ·
  [English](docs/en/guide/spec.md)
- **Use cases & positioning**:
  [`docs/zh/guide/use-cases.md`](docs/zh/guide/use-cases.md)
- **Architecture overview** (for contributors / deep host integrations):
  [`docs/zh/guide/architecture.md`](docs/zh/guide/architecture.md)
- **Local docs site**: `cd docs && npx vitepress dev`

## 🏗 Project Structure
- `crates/relon-parser`: Rowan-backed lexer/parser, CST, and legacy AST lowering.
- `crates/relon-analyzer`: Semantic-analysis layer (schema desugar, name resolution, type checking, diagnostics).
- `crates/relon-eval-api`: Shared types + the `Evaluator` trait every backend implements.
- `crates/relon-cap`: Zero-dependency capability data types (`Capabilities`, `NativeFnGate`).
- `crates/relon-evaluator`: Tree-walking interpreter (full-surface reference backend) + standard library.
- `crates/relon-ir`: Lowered IR consumed by the compiled backends.
- `crates/relon-codegen-cranelift`: Cranelift native AOT backend + sandbox glue.
- `crates/relon-codegen-llvm`: LLVM native AOT backend.
- `crates/relon-object-cache`, `crates/relon-object-link`: Native object cache, HMAC integrity, memfd/dlopen loader, and ET_REL linking support.
- `crates/relon`: Public API facade (`from_str` / `json_from_*` / `value_from_*` and their `*_trusted` variants, `EvaluatorBuilder` with `Backend::Auto` as the default dispatch, `Projector`, `new_evaluator`).
- `crates/relon-rs-build`, `crates/relon-rs-macro`, `crates/relon-rs-shims`, `crates/relon-rs-demo`: Build-time AOT — compile `.relon` from `build.rs`, bind via `include_relon!`, runtime ABI shims, and a working demo.
- `crates/relon-cli`: Command-line tool.
- `crates/relon-fmt`: Formatter / syntax checker.
- `crates/relon-lsp`: Language Server (parse + analyze + diagnostics).
- `crates/relon-test-harness`: Cross-backend differential integration tests.
- `crates/relon-bench`: Internal micro-benchmark harness (not published).
- `crates/relon-unicode`: Unicode tables, algorithms, and the glob matcher shared by the evaluator and codegen backends.
- `crates/relon-util`: Leaf utility helpers shared across crates.
- `crates/relon-wasm-bindings`: Browser-side wasm bindings for the playground.
- `examples/`, `fixtures/`: Demo / golden files.
