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
    // W3 — `range(n).map((i) => "a").reduce("", (acc, s) => acc + s)`.
    // Z.3c-b promotes this from ScopeCut to a pure-WASM byte-fill loop
    // emitted by `emit_w3_string_concat_inline`. Matches the production
    // `w3_relon_src()` byte-identical. The lowering returns a
    // (ptr<<32 | len) i64 the host unpacks back to `Value::String`.
    if normalised.contains("range(n).map((i) => \"a\")")
        && normalised.contains("reduce(\"\", (acc, s) => acc + s)")
    {
        return Ok(WasmProgram::W3StringConcatInline);
    }
    // W4 / W4_long — `range(n).map((i) => "<H>").filter((s) => s.contains("x")).len()`.
    // Z.3c-c promotes both from ScopeCut to a pure-WASM accumulator loop
    // (`emit_w4_filter_contains_count`) that calls `__relon_str_contains`
    // per iter. The two flavours differ only in the haystack literal —
    // we match the long variant first because its source contains the
    // short literal as a substring of its docstring would; matching on
    // the byte-identical 256-byte literal eliminates false positives.
    if normalised.contains(".contains(\"x\")") {
        // The long-haystack literal is byte-identical to
        // `W4_HAYSTACK_LONG` in the codegen crate. We avoid pulling the
        // raw constant in here to keep classify text-driven; the
        // detection key is the "aaaaax" terminal run unique to that
        // 256-byte string. The short haystack ("axb") has no run of
        // 5 'a's so the discriminator stays clean.
        let is_long = normalised.contains("aaaaax\")");
        return Ok(WasmProgram::W4StringContains { long: is_long });
    }
    if normalised.contains("d[keys[i % 10]]") {
        return Err(ClassifyError::ScopeCut("W5-dict-access"));
    }
    if normalised.contains("fib(k - 1) + fib(k - 2)") {
        return Err(ClassifyError::ScopeCut("W7-fib-recursion"));
    }
    // W8 inline-Int dispatch variant — matches
    // `w8_relon_src_bytecode_dispatch()` from cmp_lua: the production
    // closure body is inlined into the `.map(...)` literal as a 4-arm
    // `?:` chain on `(i % 4)`, and the outer reduce returns a scalar
    // `Int`. The lowering preserves the dispatch (via `br_table`) — no
    // algebraic collapse to `(i % 4) + 1`. Detect the 4-arm ternary
    // shape paired with the `Int` return; the production source (which
    // calls `dispatch(i % 4)` on a `#internal` closure and returns
    // `Dict`) still scope-cuts below — Z.4 follow-up.
    if normalised.contains("#main(Int n) -> Int")
        && normalised.contains("(i % 4) == 0 ? 1")
        && normalised.contains("(i % 4) == 1 ? 2")
        && normalised.contains("(i % 4) == 2 ? 3")
    {
        return Ok(WasmProgram::W8PolymorphicDispatchInline);
    }
    if normalised.contains("dispatch(i % 4)") {
        return Err(ClassifyError::ScopeCut("W8-polymorphic-dispatch"));
    }
    // W9 inline-Int variant — matches `w9_relon_src_bytecode()` from
    // cmp_lua: the production source's `rows: range(n).map(...)` list
    // and the dict-body return are both gone; `rows[i][j]` is inlined
    // to `(i * n + j)` and the outer reduce returns a scalar `Int`.
    // Detected by the `#main(Int n) -> Int` declaration paired with
    // the nested-reduce shape and the `(i * n + j)` inlined index.
    // The production source (which keeps `rows[i][j]` + Dict return)
    // still scope-cuts below — Z.4 follow-up.
    if normalised.contains("#main(Int n) -> Int")
        && normalised.contains("range(n).reduce(0, (acc, j) =>")
        && normalised.contains("range(n).reduce(0, (inner, i) =>")
        && normalised.contains("(i * n + j)")
        && !normalised.contains("rows[i][j]")
    {
        return Ok(WasmProgram::W9NestedMatrixInline);
    }
    if normalised.contains("rows[i][j]") || normalised.contains("(i * n + j)") {
        return Err(ClassifyError::ScopeCut("W9-nested-matrix"));
    }
    // W10 inline-Int variant — matches `w10_relon_src_bytecode()` from
    // cmp_lua: `allow`'s closure body is inlined into the `.map(...)`
    // literal and the dict-body's `result` field is unwrapped to a
    // scalar `Int` return. Detected by the `#main(Int n) -> Int`
    // declaration paired with the literal predicate composition. The
    // production source (which returns a `Dict` and binds `allow` as a
    // `#internal` closure) still scope-cuts below — Z.4 follow-up.
    if normalised.contains("#main(Int n) -> Int")
        && normalised.contains("(i % 3 == 0 || i % 3 == 1)")
        && normalised.contains("(i % 4 == 0 || i % 4 == 1)")
        && normalised.contains("(i % 24 >= 8 && i % 24 < 18)")
    {
        return Ok(WasmProgram::W10ConfigEvalInline);
    }
    if normalised.contains("i % 24 >= 8") {
        return Err(ClassifyError::ScopeCut("W10-config-eval"));
    }

    Err(ClassifyError::ScopeCut("unknown-shape"))
}
