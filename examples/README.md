# `examples/` — user-facing `.relon` showcases

Small, self-contained `.relon` programs meant to be **read and run by
users** evaluating the language. They should stay strict by default,
deterministic, and directly runnable:

```bash
cargo run -q -p relon-cli -- run --backend tree-walk examples/demo.relon
```

The three main learning tracks are:

- **Schema-backed validation** — `examples/validation.relon`.
- **Feature / pricing policy** — `examples/feature_flag.relon` and
  `examples/pricing.relon`.
- **Workflow / integration glue** — `examples/workflow.relon`.

Every `.relon` file in this directory has a header with:

- `Try:` — the canonical runnable command.
- `Recommended backend:` — currently `tree-walk`, the full-surface
  reference backend for learning examples.
- `Expected output:` — the golden JSON path the command should match.

Compiled backend coverage is tested separately in the fixture and parity
suites; examples are the user-facing learning entry, while fixtures are
the test corpus.

Distinct from the two sibling locations (deliberately not merged):

- `fixtures/` — the cross-backend **test corpus**. Files there are
  pinned by the differential harness and golden outputs; editing them
  changes tests, not documentation.
- `crates/relon/examples/*.rs` — **Rust embedding examples** for the
  host API (`cargo run -p relon --example use_builder`), kept in the
  conventional cargo location.

Files here are covered by the `relon-fmt --check` CI gate — run
`cargo run -p relon-fmt -- examples/*.relon` after editing.
