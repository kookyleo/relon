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

### Local Validation
Run the full test suite and strict lint gate before shipping changes:
```bash
cargo test
cargo clippy --workspace --all-targets -- -D warnings
cargo run -q -p relon-fmt -- --check fixtures/*.relon fixtures/modules/*.relon fixtures/errors/*.relon examples/*.relon
```

## 🛠 Features

- **Sandboxed by default**: a script declares the capabilities it needs
  (`reads_fs`, `network`, allow-listed native fns); the host grants
  them. There is no "trusted mode" the script can fall back to.
- **Self-describing schemas**: `@schema`, sum-type tagged enums,
  recursive contracts, branded values — type information travels with
  the payload.
- **Context-aware references**: `&root`, `&sibling`, `&prev`, `&next`
  let logic reference its surrounding data without hard-coded paths.
- **Functional core**: unified closures (`@fn`), comprehensions, pipes,
  pattern match — pure expressions, no IO or side effects.
- **Canonical std**: `@import("std/list", as="list")` is part of the
  language, not a host extension — scripts can rely on it without the
  embedder wiring anything up.

## 📖 Example

```javascript
{
    @fn(val, symbol)
    "currency": val + " " + symbol,

    "price": 100,
    
    @currency("USD")
    "display": &sibling.price
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
- `crates/relon-parser`: The core parser built with `winnow`.
- `crates/relon-analyzer`: Semantic-analysis layer (schema desugar, name resolution, diagnostics).
- `crates/relon-evaluator`: The execution engine and standard library.
- `crates/relon`: Public API facade (`evaluate_source`, `json_from_*`, `Projector`).
- `crates/relon-cli`: Command-line tool.
- `crates/relon-fmt`: Formatter / syntax checker.
- `crates/relon-lsp`: Language Server (parse + analyze + diagnostics).
- `examples/`, `fixtures/`: Demo / golden files.
