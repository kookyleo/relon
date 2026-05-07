# Relon

**Logic as portable data.** Relon is an executable data format: business
logic — validation rules, pricing formulas, workflow steps — is written
once, stored like JSON, and evaluated identically by any conformant
runtime. The reference runtime is Rust; the language spec is
runtime-agnostic and deterministic by construction.

> Write the rule once. Store it in your database, your config file, your
> RPC payload. Run it from Go, TypeScript, Swift, the browser. Get the
> same answer everywhere.

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

- **Deterministic by spec**: same source + same input → byte-identical
  output, regardless of host language. No floating-point quirks, no
  iteration-order leaks, no implicit ambient state.
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
- **Canonical std**: `@import("std/list", as="list")` ships with every
  conformant runtime; the std module set is part of the spec, not a
  per-runtime extension.

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

- **Language spec** (the cross-runtime contract):
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
