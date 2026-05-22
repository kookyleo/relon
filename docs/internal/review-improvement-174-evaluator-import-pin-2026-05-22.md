# review-improvement-174: evaluator-side `#import` integrity pin

## Finding

`TreeWalkEvaluator` ignored the `integrity` field on
`DirectiveBody::Import`. A host that parses straight into the
evaluator (bench harness, ad-hoc embedding, the wasm playground,
anything skipping `analyze_entry`) silently accepted a remote module
whose body's sha256 disagreed with the inline `sha256:"..."` pin. The
analyzer enforced the pin in `crates/relon-analyzer/src/workspace_build.rs`
but only on the workspace path — the runtime had a bypass.

## Choice: Option A (forward the pin to the evaluator's loader)

Rejected B (require analyzer-built workspace) — breaks every host
that legitimately calls the evaluator directly, including the wasm
build. Rejected C (deny-by-default unpinned) — penalises every
local `./util.relon` import for an attack that only matters on
network paths. A keeps pinning opt-in but makes verification
unskippable wherever the directive carries a pin.

## Changes

* `crates/relon-eval-api/src/error.rs`: three new `RuntimeError`
  variants — `ImportHashMismatch`, `ImportHashUnknownAlgorithm`,
  `ImportHashInvalidHex` — plus `ImportHashMismatchDetail` payload.
  Kept distinct from the existing remote-only
  `RemoteImportHashMismatch` so operators can tell the two
  defense-in-depth layers apart.
* `crates/relon-evaluator/Cargo.toml`: promotes `sha2` + `hex` from
  the `cfg(not(wasm32))` block to the top-level dep table. The
  pin check has to apply on every target; otherwise the wasm
  playground would be a bypass.
* `crates/relon-evaluator/src/eval.rs`:
  * `apply_directive_pre` now destructures `integrity` off the
    `Import` body and forwards it.
  * `apply_directive_import` and `load_module` gain
    `integrity: Option<&IntegrityHash>` parameters.
  * `verify_module_integrity` runs after `resolve_module_source`
    returns the bytes but before parse / cache / evaluation —
    fail-closed, zero side effects on rejection.
  * Branch order: unknown algorithm → invalid hex → digest
    mismatch (mirrors the analyzer for diagnostic parity).

## Verification

`cargo fmt --all --check`, `cargo clippy --workspace --all-targets
-- -D warnings`, `cargo test --workspace`, `cargo check -p relon-wasm
--target wasm32-unknown-unknown` — all clean. Total tests: 2288 (was
2282; +6 from `import_pin_tests`). New cases cover the happy path,
mismatch (with the actual computed digest asserted so renaming the
algorithm constant cannot regress silently), unknown algorithm,
malformed hex, case-insensitive digest comparison, and the unpinned
"still works" regression guard.
