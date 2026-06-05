//! `#main() -> Dict { ... }` anon-Dict return marshalling — cross-backend
//! parity + silent-miscompile guards.
//!
//! The anon-Dict return path (`anon_dict_return_plan` /
//! `lower_anon_dict_body`) lifts a bare `-> Dict { ... }` body into a
//! synthesised return record. Pointer-indirect fields (String,
//! `List<scalar>`, `List<String>`) must be copied into the return
//! buffer's tail via `EmitTailRecordFromAbsoluteAddr` before their
//! fixed-area slot can hold the (buffer-relative) offset — without that
//! copy the slot held a raw arena pointer and the host reader decoded
//! garbage (the W7 silent empty-String mis-compile).
//!
//! These tests pin the marshalling against the tree-walk gold standard
//! (bit-equal, field-by-field including String contents) on both native
//! compiled backends, and assert that every still-unsupported return
//! shape fails *loudly* at compile time rather than returning wrong data.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

/// Tree-walk gold standard for a `#main()` source (no params).
fn run_tree_walk(src: &str) -> Value {
    use relon_evaluator::{Context, TreeWalkEvaluator};
    use relon_parser::parse_document;
    let node = parse_document(src).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    let ctx = Arc::new({
        let mut ctx = ctx;
        TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    });
    TreeWalkEvaluator::new(Arc::clone(&ctx))
        .run_main(
            &Arc::new(relon_eval_api::scope::Scope::default()),
            HashMap::new(),
        )
        .expect("tree-walk run_main")
}

/// Normalise a returned Dict into a sorted `(key, Value)` list so the
/// brand / map-iteration-order differences between executors do not
/// matter — only the field set + per-field value content does.
fn fields_of(v: &Value) -> Vec<(String, Value)> {
    match v {
        Value::Dict(d) => {
            let mut out: Vec<(String, Value)> = d
                .map
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        }
        other => panic!("expected Dict return, got {other:?}"),
    }
}

/// Assert tree-walk == cranelift == llvm field-by-field for a source.
fn assert_three_way(src: &str) -> Vec<(String, Value)> {
    let tw = fields_of(&run_tree_walk(src));

    let cl = AotEvaluator::from_source(src).expect("cranelift from_source");
    let cl_v = fields_of(&cl.run_main(HashMap::new()).expect("cranelift run_main"));
    assert_eq!(cl_v, tw, "cranelift vs tree-walk mismatch for `{src}`");

    let llvm = LlvmAotEvaluator::from_source(src).expect("llvm from_source");
    let llvm_v = fields_of(&llvm.run_main(HashMap::new()).expect("llvm run_main"));
    assert_eq!(llvm_v, tw, "llvm vs tree-walk mismatch for `{src}`");

    tw
}

/// The original silent-miscompile: a String field of an anon-Dict
/// return was stored as a raw arena pointer and decoded to "". Must now
/// be the literal `"alpha"` on every backend.
#[test]
fn anon_dict_string_field_is_not_silently_empty() {
    const SRC: &str = "#main() -> Dict\n{ host: \"alpha\", port: 1 }";
    let fields = assert_three_way(SRC);
    assert_eq!(
        fields,
        vec![
            ("host".to_string(), Value::String("alpha".into())),
            ("port".to_string(), Value::Int(1)),
        ],
        "host must marshal as the literal \"alpha\", not an empty string"
    );
}

/// `List<scalar>` fields (Int / Float / Bool) marshal field-by-field.
#[test]
fn anon_dict_scalar_list_fields() {
    assert_three_way("#main() -> Dict\n{ name: \"x\", nums: [1, 2, 3] }");
    assert_three_way("#main() -> Dict\n{ flags: [true, false], fs: [1.5, 2.5] }");
}

/// `List<String>` field marshals through the pointer-array copy +
/// inner-offset relocation.
#[test]
fn anon_dict_list_string_field() {
    let fields =
        assert_three_way("#main() -> Dict\n{ name: \"x\", keys: [\"a\", \"bb\", \"ccc\"] }");
    let keys = fields
        .iter()
        .find(|(k, _)| k == "keys")
        .map(|(_, v)| v.clone())
        .expect("keys field present");
    assert_eq!(
        keys,
        Value::List(Arc::new(vec![
            Value::String("a".into()),
            Value::String("bb".into()),
            Value::String("ccc".into()),
        ]))
    );
}

/// Mixed scalar + String + list fields in one record.
#[test]
fn anon_dict_mixed_fields() {
    assert_three_way(
        "#main() -> Dict\n{ host: \"alpha\", port: 8080, tags: [\"a\", \"b\"], scores: [10, 20] }",
    );
}

/// A `#internal` collection field stays off the host-visible surface on
/// every backend (the tree-walk oracle drops it too), while the
/// non-internal sibling is still marshalled.
#[test]
fn anon_dict_internal_field_dropped_consistently() {
    let fields =
        assert_three_way("#main() -> Dict\n{ name: \"x\", #internal\n keys: [\"a\", \"b\"] }");
    assert!(
        fields.iter().all(|(k, _)| k != "keys"),
        "#internal keys must not surface to the host"
    );
    assert_eq!(fields.len(), 1, "only `name` should survive");
}

/// A `#internal` *scalar* field must also be dropped from the host
/// surface — the compiled backends used to keep it (surfacing a field
/// the tree-walk oracle omits), a silent field-set divergence.
#[test]
fn anon_dict_internal_scalar_dropped_consistently() {
    let fields = assert_three_way("#main() -> Dict\n{ #internal\n tmp: 5, result: 10 }");
    assert_eq!(
        fields,
        vec![("result".to_string(), Value::Int(10))],
        "#internal scalar `tmp` must not surface to the host"
    );
}

/// Red-line guard: every still-unsupported anon-Dict return shape MUST
/// fail loudly at compile time (`from_source` Err) rather than return
/// wrong/partial data. A silent miscompile here is the bug class this
/// whole change set exists to kill, so the assertion is on *both*
/// compiled backends.
#[test]
fn unsupported_return_shapes_fail_loudly_not_silently() {
    // Shapes neither AOT backend lifts yet — both must Err loudly.
    const BOTH_CAP: &[&str] = &[
        // Plain (non-#internal) Dict-valued field — Dict is not a buffer
        // return type; tree-walk would surface it, so it must error.
        "#main() -> Dict\n{ name: \"x\", m: { a: 1, b: 2 } }",
        // List<Schema> field from a *literal* (not a param identity) — the
        // const-pool pointer-array-element marshaller does not handle it.
        "#schema P {\n    Int x: *\n}\n#main() -> Dict\n{ name: \"x\", ps: [{x:1},{x:2}] }",
        // List<List<scalar>> field from a literal.
        "#main() -> Dict\n{ name: \"x\", nested: [[1, 2], [3, 4]] }",
        // Empty list — element type cannot be inferred for marshalling.
        "#main() -> Dict\n{ name: \"x\", empty: [] }",
        // Heterogeneous list.
        "#main() -> Dict\n{ name: \"x\", mixed: [1, \"a\"] }",
        // Top-level List<Schema> / List<List> returns from a *literal*.
        "#schema P {\n    Int x: *\n}\n#main() -> List<P>\n[{x:1},{x:2}]",
        "#main() -> List<List<Int>>\n[[1, 2], [3, 4]]",
        // Doubly-nested pointer-array (`List<List<Schema>>`) field — out of
        // scope (F5).
        "#schema Server { name: String }\n\
         #main(List<List<Server>> xs) -> Dict\n{ xs: xs, n: 1 }",
    ];
    // F2/F3: cross-region object fields sourced by parameter identity now
    // ship on cranelift AND llvm (wasm shares the IR + the same store path).
    // The object head sits in out_buf and the field slot holds the parameter
    // list root's arena-absolute offset into in_buf; the host's multi-region
    // verifier classifies + bounds-checks it before the reader follows it
    // cross-region (bit-equal to tree-walk, proven four-way in
    // `relon-test-harness/tests/return_cross_region_object.rs` and
    // `relon-codegen-llvm/tests/cross_region_object_four_way.rs`). F2 covers
    // the anon-Dict `List<Schema>` / `List<List<scalar>>` fields; F3 adds the
    // branded-struct path and the scalar/String list field types.
    const BOTH_COMPILE: &[&str] = &[
        "#schema Server { name: String, port: Int }\n\
         #main(List<Server> servers) -> Dict\n{ servers: servers, n: 1 }",
        "#main(List<List<Int>> grid) -> Dict\n{ g: grid, n: 1 }",
        // F3: branded-struct List<String> field.
        "#schema S {\n    tags: List<String>\n}\n#main(List<String> t) -> S\n{ tags: t }",
        // F3: branded-struct List<Schema> field.
        "#schema Server { name: String }\n#schema Cfg { servers: List<Server> }\n\
         #main(List<Server> servers) -> Cfg\n{ servers: servers }",
        // F3: anon-Dict scalar/String list fields sourced by param identity.
        "#main(List<String> tags) -> Dict\n{ tags: tags, n: 1 }",
        "#main(List<Int> xs) -> Dict\n{ xs: xs, n: 1 }",
        // F4: object field sourced by a parameter *field* walk (`w.items`).
        // Post-F1 the field-load pushes the field list root's arena-absolute
        // offset; the cross-region store + multi-region verifier/reader are
        // identical to the identity path (proven four-way bit-equal).
        "#schema Server { name: String }\n#schema W { items: List<Server> }\n\
         #main(W w) -> Dict\n{ servers: w.items, n: 1 }",
        // F4: top-level parameter-field list returns.
        "#schema Outer { tags: List<String>, n: Int }\n\
         #main(Outer o) -> List<String>\no.tags",
        "#schema Outer { grid: List<List<Int>>, n: Int }\n\
         #main(Outer o) -> List<List<Int>>\no.grid",
    ];
    for src in BOTH_CAP {
        let cl = AotEvaluator::from_source(src);
        assert!(
            cl.is_err(),
            "cranelift must reject unsupported return shape loudly, but compiled: `{src}`"
        );
        let llvm = LlvmAotEvaluator::from_source(src);
        assert!(
            llvm.is_err(),
            "llvm must reject unsupported return shape loudly, but compiled: `{src}`"
        );
    }
    for src in BOTH_COMPILE {
        let cl = AotEvaluator::from_source(src);
        assert!(
            cl.is_ok(),
            "cranelift must compile the F1b/F2 cross-region object shape, but declined: `{src}`"
        );
        let llvm = LlvmAotEvaluator::from_source(src);
        assert!(
            llvm.is_ok(),
            "F2: llvm must compile the cross-region object shape, but declined: `{src}`"
        );
    }
}
