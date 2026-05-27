//! Map a parsed Relon `Node` to a Z.1 `WasmProgram` variant.
//!
//! Z.1 only recognises three sources (W1 / W6 / W12 from
//! `crates/relon-bench/benches/cmp_lua.rs`). Anything else maps to
//! `Err(ClassifyError::ScopeCut(tag))`, which the caller surfaces as a
//! "tree-walker fallback" so the row is honestly labelled rather than
//! silently passing.
//!
//! The classifier walks the source string — not the AST — because the
//! production sources are short and stable; matching on raw text is
//! good enough for the POC. Z.3 replaces this with a real AST walker
//! at the same time it widens the lowering surface.

use relon_codegen_wasm::WasmProgram;
use thiserror::Error;

/// Classifier failures.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClassifyError {
    /// The source's `#main(...)` body doesn't match a Z.1 program
    /// shape. The carrier tag is one of the
    /// `relon_codegen_wasm::LowerError::ScopeCut` workload names so
    /// downstream logging can join them.
    #[error("classify: source not on Z.1 lowering surface: {0}")]
    ScopeCut(&'static str),

    /// No `#main(...)` declaration at all. `WasmEvaluator::run_main`
    /// would error anyway, but flagging it at classify time gives a
    /// clearer signal than the wasmtime-side "missing __main".
    #[error("classify: source lacks `#main(...)` declaration")]
    NoMain,
}

/// Classify the source's `#main(...)` body into a Z.1 program shape.
///
/// The classifier is text-based for Z.1 — it matches the three
/// production sources byte-identical and returns a `ScopeCut` for
/// anything else. Z.3 will switch to AST-driven classification.
pub fn classify_main(source: &str) -> Result<WasmProgram, ClassifyError> {
    if !source.contains("#main") {
        return Err(ClassifyError::NoMain);
    }

    // Normalise whitespace so we don't false-negative on a stray space.
    // The production source uses double-quoted string concat literals
    // (`\"`) — those round-trip through the tokenizer's display; we
    // strip the escape just for matching purposes.
    let normalised = source.split_whitespace().collect::<Vec<_>>().join(" ");

    // W12 — single-op body `x + 1`. Match by suffix because the
    // `#main(Int x) -> Int` declaration carries no other clutter.
    if normalised.contains("#main(Int x) -> Int") && normalised.contains("x + 1") {
        return Ok(WasmProgram::W12IncrementInt);
    }

    // W1 — `list.sum(range(n))`.
    if normalised.contains("list.sum(range(n))") {
        return Ok(WasmProgram::W1IntSumRange);
    }

    // W6 — `list.sum(range(n).map((i) => i + 1))`.
    if normalised.contains("list.sum(range(n).map((i) => i + 1))") {
        return Ok(WasmProgram::W6ListSumPlusOne);
    }

    // W2 — `list.sum(range(n).map((i) => (i + 1) * (i + 2)))`. Z.3c-a
    // promotes this from ScopeCut to a pure-WASM accumulator loop
    // emitted by `emit_w2_dot_product`. The match is anchored on the
    // full chain (not just the per-iter expression) so a future
    // workload that reuses the `(i+1)*(i+2)` arithmetic in a different
    // shape doesn't accidentally pick up the W2 lowering.
    if normalised.contains("list.sum(range(n).map((i) => (i + 1) * (i + 2)))") {
        return Ok(WasmProgram::W2DotProduct);
    }
    if normalised.contains("reduce(\"\", (acc, s) => acc + s)") {
        return Err(ClassifyError::ScopeCut("W3-string-concat"));
    }
    if normalised.contains(".contains(\"x\")") {
        return Err(ClassifyError::ScopeCut("W4-string-contains"));
    }
    if normalised.contains("d[keys[i % 10]]") {
        return Err(ClassifyError::ScopeCut("W5-dict-access"));
    }
    if normalised.contains("fib(k - 1) + fib(k - 2)") {
        return Err(ClassifyError::ScopeCut("W7-fib-recursion"));
    }
    if normalised.contains("dispatch(i % 4)") {
        return Err(ClassifyError::ScopeCut("W8-polymorphic-dispatch"));
    }
    if normalised.contains("rows[i][j]") || normalised.contains("(i * n + j)") {
        return Err(ClassifyError::ScopeCut("W9-nested-matrix"));
    }
    if normalised.contains("i % 24 >= 8") {
        return Err(ClassifyError::ScopeCut("W10-config-eval"));
    }

    Err(ClassifyError::ScopeCut("unknown-shape"))
}
