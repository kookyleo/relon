# `fixtures/` — cross-backend test corpus

`.relon` sources consumed by the test suites, primarily the
differential harness in `crates/relon-test-harness` (every fixture is
evaluated on all execution backends and the results must agree).
**These are tests, not documentation** — user-facing showcases live in
`examples/`.

| Path | Role |
| --- | --- |
| `*.relon` | Feature-area corpus (one surface area per file) evaluated by the differential harness |
| `errors/` | Programs whose evaluation must fail with a specific stable error label |
| `modules/` | Multi-file import graph (`main.relon` + `lib.relon`) for module-resolution tests |
| `golden/` | Pinned expected outputs (`success/`, `errors/`, `examples_main/`, `tier2_treewalk/`) |

Editing a fixture changes test inputs: expect golden files and
harness assertions to need a matching update, and run the full
suite (`./scripts/verify.sh`). The `relon-fmt --check` CI gate covers
`fixtures/*.relon`, `fixtures/modules/*.relon`, and
`fixtures/errors/*.relon`.
