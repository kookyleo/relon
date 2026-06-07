//! Wave R3c — three-way differential for the String-result list `map`
//! family: homogeneous `List<String>` map (`String -> String`) and the
//! element-type-changing String-result maps from a numeric source
//! (`List<Int> -> List<String>`, `List<Float> -> List<String>`).
//!
//! Each case runs tree-walk (golden oracle) vs cranelift-AOT vs LLVM-AOT
//! (under the `llvm-aot` feature) through [`assert_all_backends_bit_equal`],
//! which compares the returned `Value` deep / bit-for-bit (every string
//! byte, every element).
//!
//! Coverage here is THREE-WAY (tree-walk, cranelift-native, llvm-native).
//! The wasm (fourth) leg for these EXACT scratch-built `List<String>` map
//! results is now established separately, in
//! `relon-codegen-llvm/tests/inplace_return_four_way.rs` (the `r13_*` cases
//! plus the `diff_scratch_list_string_fstring` proptest): Wave R13 routed
//! the wasm decode through the shared, verifier-gated
//! `relon_eval_api::inplace_return` decoder over a linear-memory slice
//! (`wasm_buffer_decode`), which handles `List<String>` and the negative
//! in-place sentinel a scratch-region pointer-array root reports — so the
//! headline `range(n).map(x => f"v${x}")`, the `String -> String` suffix
//! map, and the const/empty/free-form bodies all run four-way bit-equal,
//! through a real LLVM→wasm32 → wasm-ld → wasmtime pipeline, with NO
//! codegen change (the sentinel was already emitted).
//!
//! This three-way file is retained as the harness-level oracle differential
//! (it compares the full `Value` deep / bit-for-bit across the native
//! backends); the wasm bit-equality lives in the four-way file above so the
//! coverage is not duplicated. (Sibling numeric-list HOF coverage is also
//! four-way: `list_float_hof_four_way` — those return inline-fixed
//! `List<Int|Float>` the wasm harness already decodes.)
//!
//! The bundled `list_string_map` / `list_int_map_to_string` /
//! `list_float_map_to_string` bodies build the result as a `List<String>`
//! pointer-array record (`[count][off_0..]`, 4-byte slots) in scratch;
//! every `off_i` slot is an arena-relative String handle the closure
//! already produced (a const-pool literal or a scratch-built
//! `StrConcatN` / `IntToStr` record — all in the same flat arena), so the
//! record is self-contained under the single global arena-relative pointer
//! convention. The entry returns it via the in-place region-walk ABI (the
//! backend reports the root header's arena-absolute offset; the host
//! verifies + decodes in place over the scratch region) — no relocation,
//! byte-equal to the tree-walk `_list_map`.
//!
//! `List<String>` filter stays capped: no `String -> Bool` predicate
//! lowers four-way yet (the analyzer cannot derive the return type of a
//! String-receiver method predicate, and cranelift does not lower String
//! `Eq`/`Ne`), so the shape is not provable byte-equal and is not shipped.

use std::collections::HashMap;

use relon_evaluator::Value;
use relon_test_harness::assert_all_backends_bit_equal;

fn no_args() -> HashMap<String, Value> {
    HashMap::new()
}

fn arg_n(n: i64) -> HashMap<String, Value> {
    HashMap::from([("n".to_string(), Value::Int(n))])
}

/// Assert every available AOT backend actually compiled the shape (a
/// decline would silently degrade the test to tree-walk-only).
fn assert_compiled(source: &str, args: HashMap<String, Value>) {
    let report = assert_all_backends_bit_equal(source, args);
    assert!(
        report.cranelift_compared,
        "cranelift must compile {source:?}; skipped: {:?}",
        report.cranelift_skip_reason
    );
    #[cfg(feature = "llvm-aot")]
    assert!(
        report.llvm_compared,
        "llvm must compile {source:?}; skipped: {:?}",
        report.llvm_skip_reason
    );
}

// ---- homogeneous List<String> map (String -> String) ----------------------

#[test]
fn string_map_concat_suffix() {
    assert_compiled(
        "#main() -> List<String>\n[\"a\", \"b\", \"c\"].map((String s) => s + \"!\")",
        no_args(),
    );
}

#[test]
fn string_map_double() {
    assert_compiled(
        "#main() -> List<String>\n_list_map([\"x\", \"y\"], (String s) => s + s)",
        no_args(),
    );
}

#[test]
fn string_map_identity_empty_strings() {
    // Empty-string elements: the per-element String record is `[0]` (a
    // bare length prefix, no payload). The identity map must preserve each
    // empty entry, byte-for-byte, across backends.
    assert_compiled(
        "#main() -> List<String>\n[\"\", \"ab\", \"\"].map((String s) => s + \"\")",
        no_args(),
    );
}

// ---- element-type-changing String-result map: Int source ------------------

#[test]
fn int_to_string_const_body() {
    assert_compiled(
        "#main(Int n) -> List<String>\nrange(n).map((Int x) => \"v\")",
        arg_n(3),
    );
}

#[test]
fn int_to_string_fstring_headline() {
    // The headline shape: `range(n).map((Int x) => f"v${x}")`. The closure
    // builds a fresh String per element (`IntToStr` + concat in scratch);
    // the map collects the handles into the result pointer array.
    assert_compiled(
        "#main(Int n) -> List<String>\nrange(n).map((Int x) => f\"v${x}\")",
        arg_n(4),
    );
}

#[test]
fn int_to_string_empty_source() {
    // Empty source → `[]`: the body allocates the header (count 0) and the
    // loop never runs, so the result is an empty `List<String>`.
    assert_compiled(
        "#main(Int n) -> List<String>\nrange(n).map((Int x) => f\"v${x}\")",
        arg_n(0),
    );
}

#[test]
fn int_to_string_free_form() {
    assert_compiled(
        "#main() -> List<String>\n_list_map([1, 2, 3], (Int x) => f\"#${x}\")",
        no_args(),
    );
}

// ---- element-type-changing String-result map: Float source ----------------

#[test]
fn float_to_string_const_body() {
    assert_compiled(
        "#main() -> List<String>\n[1.5, 2.5].map((Float x) => \"f\")",
        no_args(),
    );
}
