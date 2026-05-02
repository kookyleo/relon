# Relon

A programmable configuration language that extends JSON with functional logic, references, and a self-describing meta-system.

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

## 🛠 Features

- **Unified Closures**: Use `@fn` to define logic that works as both functions and decorators.
- **Deep References**: Access data via `&root`, `&sibling`, or `&uncle`.
- **List Comprehensions**: Python-style iteration: `[x for x in list if cond]`.
- **Piped Processing**: Chain operations with `|`.
- **Modular**: Structured code organization with `@import`.
- **Embedded Stdlib**: Built-in logic for math and list operations.

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

## 🏗 Project Structure
- `crates/relon-parser`: The core parser built with `winnow`.
- `crates/relon-evaluator`: The execution engine and standard library.
- `crates/relon-cli`: Command-line tool.
- `examples/`: Demo files.
