//! Integration smoke for the `relon-codegen-wasm` public emit API.
//!
//! The crate's 26 unit tests live inside `src/` and reach private
//! helpers; correctness of the *emitted modules* has otherwise been
//! delegated to the `relon-wasm-evaluator` end-to-end smokes (which run
//! the bytes through wasmtime). This file closes that gap from the
//! codegen side: it drives only the two public emit entry points and
//! validates every produced module through `wasmparser`, the same
//! family `wasm-encoder` targets, so a section-ordering / type-index /
//! missing-body regression in the emitter surfaces here without a
//! runtime dependency.
//!
//! The shape mirrors `crates/relon-wasm-evaluator/tests/w*_smoke.rs`
//! (parse -> analyze -> lower -> assert), but the assertion target is
//! the codegen crate's public surface (`lower`, `lower_ir_module`)
//! rather than the host evaluator.

use relon_codegen_wasm::{const_segment_end, lower, lower_ir_module, WasmProgram};
use relon_ir::LoweredEntry;

/// parse -> analyze -> `lower_workspace_single`, returning the IR entry
/// the walker emit path consumes. Same pipeline the host evaluator runs
/// before handing the module to wasmtime.
fn lower_source(src: &str) -> LoweredEntry {
    let ast = relon_parser::parse_document(src).expect("parse_document");
    let analyzed = relon_analyzer::analyze(&ast);
    relon_ir::lower_workspace_single(&analyzed, &ast).expect("lower_workspace_single")
}

/// Round-trip a wasm binary through the validator, attributing failure
/// to the named module so a regression names the offending program.
fn assert_valid_module(bytes: &[u8], label: &str) {
    wasmparser::Validator::new()
        .validate_all(bytes)
        .unwrap_or_else(|e| panic!("wasmparser must validate {label}: {e}"));
}

/// Public `lower(&WasmProgram)` entry: emit the closed-form variant
/// modules across the imports-bearing, inline-loop, and data-segment
/// shapes and confirm each is a structurally valid wasm module. This is
/// the classifier-side emit path the host tries first.
#[test]
fn lower_program_variants_emit_valid_modules() {
    // Pure accumulator loops (no host imports, no linear memory traffic).
    let w1 = lower(&WasmProgram::W1IntSumRange).expect("emit W1");
    assert_valid_module(&w1, "W1IntSumRange");

    let w2 = lower(&WasmProgram::W2DotProduct).expect("emit W2");
    assert_valid_module(&w2, "W2DotProduct");

    // Data-segment-bearing variant: the dispatch table forces a
    // non-trivial const segment, which the host arena-reset relies on.
    let w5 = lower(&WasmProgram::W5DictAccessInline).expect("emit W5 inline");
    assert_valid_module(&w5, "W5DictAccessInline");
    assert!(
        const_segment_end(&WasmProgram::W5DictAccessInline) > 16,
        "W5 inline must report a non-empty const segment for the host arena floor"
    );

    // Recursion variant: two local wasm functions ($fib + $__main),
    // exercising the multi-function emit path through the public API.
    let w7 = lower(&WasmProgram::W7FibRecursionInline).expect("emit W7 inline");
    assert_valid_module(&w7, "W7FibRecursionInline");
}

/// Public `lower(&WasmProgram)` scope-cut contract: the roadmap-deferred
/// variants must surface an explicit error rather than emit a silently
/// wrong module. Asserting through the public API keeps the host from
/// ever mistaking a scope-cut for a valid lowering.
#[test]
fn lower_program_scope_cuts_are_explicit() {
    use relon_codegen_wasm::LowerError;

    match lower(&WasmProgram::W7FibRecursion) {
        Err(LowerError::ScopeCut(tag)) => assert_eq!(tag, "W7-fib-recursion"),
        other => panic!("expected explicit ScopeCut for W7FibRecursion, got {other:?}"),
    }
}

/// Public `lower_ir_module(&LoweredEntry)` entry: drive real Relon
/// sources through the parse/analyze/lower pipeline and confirm the
/// IR-walker emit path produces valid modules for the scalar-Int and
/// structured-control-flow subsets it claims to cover. This is the
/// canonical Z.4 lowering path, asserted directly against the codegen
/// crate's public surface.
#[test]
fn lower_ir_module_emits_valid_modules_for_walker_subset() {
    // Scalar-Int arithmetic body (`x + 1` shape).
    let incr = lower_source("#main(Int x) -> Int\nx + 1");
    assert_valid_module(
        &lower_ir_module(&incr).expect("lower_ir_module(increment)"),
        "walker increment",
    );

    // Structured control flow: `range(n).reduce(...)` lowers to a
    // block/loop/br/br_if skeleton, the heart of the Z.4.2 surface.
    let reduce = lower_source("#main(Int n) -> Int\nrange(n).reduce(0, (acc, i) => acc + i)");
    assert_valid_module(
        &lower_ir_module(&reduce).expect("lower_ir_module(range.reduce)"),
        "walker range.reduce",
    );
}
