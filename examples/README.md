# `examples/` — user-facing `.relon` showcases

Small, self-contained `.relon` programs meant to be **read and run by
users** evaluating the language:

```bash
cargo run -p relon-cli -- run examples/demo.relon
```

Distinct from the two sibling locations (deliberately not merged):

- `fixtures/` — the cross-backend **test corpus**. Files there are
  pinned by the differential harness and golden outputs; editing them
  changes tests, not documentation.
- `crates/relon/examples/*.rs` — **Rust embedding examples** for the
  host API (`cargo run -p relon --example use_builder`), kept in the
  conventional cargo location.

Files here are covered by the `relon-fmt --check` CI gate — run
`cargo run -p relon-fmt -- examples/*.relon` after editing.
