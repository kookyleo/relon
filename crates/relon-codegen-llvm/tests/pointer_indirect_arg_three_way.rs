//! Pointer-indirect `#main` **input** materialisation — cross-backend
//! parity (tree-walk gold standard / cranelift-native / llvm-native).
//!
//! A `String` / `List<…>` `#main` parameter (and a `#schema` struct's
//! `String` / `List<…>` field) arrives over the buffer protocol as a
//! 4-byte buffer-relative offset slot pointing at a tail record:
//!
//! * `String` — `[len: u32 LE][utf8]`.
//! * `List<Int/Float/Bool>` — `[len: u32 LE][payload]` (8-byte i64 /
//!   f64 elements, tightly-packed u8 booleans).
//! * `List<String>` — a `[len][off_0]…` pointer array of `[len][utf8]`
//!   String records.
//!
//! The host serialises the arg into that record (`write_string` /
//! `write_list_*` on both backends); the JIT body reads it back via
//! `Op::LoadStringPtr` / `Op::LoadList*Ptr` (top-level params, rebased
//! to arena-relative) or `Op::LoadFieldAtAbsolute` with a pointer-
//! indirect field type (schema fields, same rebase).
//!
//! Multi-segment nested-schema walks (`o.inner.x`) now resolve end-to-end
//! through chained `LoadSchemaPtr` / `LoadFieldAtAbsolute` rebases (see the
//! `nested_schema_*` cases below).
//!
//! Scope / loud caps (see the report's honesty note): `List<Schema>`
//! params/fields, nested `List<List<…>>`, and `Dict` params stay loudly
//! rejected — no silent fallthrough. A nested schema whose field is itself
//! a `List<Schema>` also stays rejected (the inner list-of-schema decode
//! is unimplemented). The `len(...)` free-call alias does not lower on
//! the compiled backends for `List<String>`; the cross-backend consumer
//! here is the `.length()` method form (`list_string_length`), which
//! lowers identically on all three executors.

use std::collections::HashMap;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

/// Tree-walk gold-standard run for a source + args.
fn run_tree_walk(src: &str, args: HashMap<String, Value>) -> Value {
    use relon_evaluator::{Context, TreeWalkEvaluator};
    use relon_parser::parse_document;
    let node = parse_document(src).expect("parse");
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    let ctx = Context::new()
        .with_root(node)
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let ctx = std::sync::Arc::new({
        let mut ctx = ctx;
        TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    });
    TreeWalkEvaluator::new(std::sync::Arc::clone(&ctx))
        .run_main(
            &std::sync::Arc::new(relon_eval_api::scope::Scope::default()),
            args,
        )
        .expect("tree-walk run_main")
}

/// Assert tree-walk == cranelift == llvm for `src` + `args`, returning
/// the agreed value.
fn assert_three_way(src: &str, args: HashMap<String, Value>) -> Value {
    let tw = run_tree_walk(src, args.clone());

    let cl = AotEvaluator::from_source(src).expect("cranelift from_source");
    let cl_v = cl.run_main(args.clone()).expect("cranelift run_main");
    assert_eq!(tw, cl_v, "tree-walk vs cranelift divergence");

    let llvm = LlvmAotEvaluator::from_source(src).expect("llvm from_source");
    let llvm_v = llvm.run_main(args.clone()).expect("llvm run_main");
    assert_eq!(tw, llvm_v, "tree-walk vs llvm divergence");

    assert_eq!(cl_v, llvm_v, "cranelift vs llvm divergence");
    tw
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

fn schema_val(brand: &str, fields: Vec<(&str, Value)>) -> Value {
    Value::branded_dict(fields, Some(brand.to_string()))
}

// ----------------------- String param (stage 1) ----------------------

/// `#main(String s) -> String = s` — identity return of a String param.
/// Proves `LoadStringPtr` rebases the buffer-relative slot to an
/// arena-relative record pointer the String-return tail copy consumes.
#[test]
fn string_param_identity_three_way() {
    const SRC: &str = "#main(String s) -> String\ns";
    let args = HashMap::from([("s".to_string(), s("hello world"))]);
    assert_eq!(assert_three_way(SRC, args), s("hello world"));
}

/// `#main(String s) -> Int = s.length()` — a String param consumed by
/// `ReadStringLen` rather than returned by identity.
#[test]
fn string_param_length_three_way() {
    const SRC: &str = "#main(String s) -> Int\ns.length()";
    let args = HashMap::from([("s".to_string(), s("héllo"))]);
    // 6 UTF-8 bytes (the length body reads the byte-length prefix).
    assert_eq!(assert_three_way(SRC, args), Value::Int(6));
}

// ------------------------ List params (stage 2) -----------------------

/// `#main(List<Int> xs) -> Int = xs.length()`. `LoadListIntPtr` rebase +
/// `ReadStringLen` over the shared `[len]` header.
#[test]
fn list_int_param_length_three_way() {
    const SRC: &str = "#main(List<Int> xs) -> Int\nxs.length()";
    let args = HashMap::from([(
        "xs".to_string(),
        Value::list(vec![Value::Int(10), Value::Int(20), Value::Int(30)]),
    )]);
    assert_eq!(assert_three_way(SRC, args), Value::Int(3));
}

/// `#main(List<Int> xs) -> List<Int> = xs` — identity return of a list
/// param, exercising the `EmitTailRecordFromAbsoluteAddr` copy from the
/// rebased arena-relative pointer.
#[test]
fn list_int_param_identity_three_way() {
    const SRC: &str = "#main(List<Int> xs) -> List<Int>\nxs";
    let args = HashMap::from([(
        "xs".to_string(),
        Value::list(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4),
        ]),
    )]);
    assert_eq!(
        assert_three_way(SRC, args),
        Value::list(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4)
        ])
    );
}

/// `#main(List<String> xs) -> Int = xs.length()`. The `LoadListStringPtr`
/// rebase + `list_string_length` body. (`len(xs)` does not lower on the
/// compiled backends for `List<String>`; `.length()` does.)
#[test]
fn list_string_param_length_three_way() {
    const SRC: &str = "#main(List<String> xs) -> Int\nxs.length()";
    let args = HashMap::from([(
        "xs".to_string(),
        Value::list(vec![s("a"), s("bb"), s("ccc")]),
    )]);
    assert_eq!(assert_three_way(SRC, args), Value::Int(3));
}

// ------------------ schema String / List fields (stage 3) -------------

/// `#main(Cfg cfg) -> String = cfg.name` — a `String` schema field, read
/// through `LoadSchemaPtr` + pointer-indirect `LoadFieldAtAbsolute`
/// (load the buffer-relative slot, rebase by `in_ptr`). A `Bool` field
/// precedes it so the String slot sits at a non-zero offset.
#[test]
fn schema_string_field_three_way() {
    const SRC: &str =
        "#schema Cfg { active: Bool, name: String, port: Int }\n#main(Cfg cfg) -> String\ncfg.name";
    let args = HashMap::from([(
        "cfg".to_string(),
        schema_val(
            "Cfg",
            vec![
                ("active", Value::Bool(true)),
                ("name", s("web-frontend")),
                ("port", Value::Int(8080)),
            ],
        ),
    )]);
    assert_eq!(assert_three_way(SRC, args), s("web-frontend"));
}

/// `#main(Cfg cfg) -> Int = cfg.tags.length()` — a `List<String>` schema
/// field decoded via pointer-indirect `LoadFieldAtAbsolute`, then its
/// `[len]` header read by `list_string_length`.
#[test]
fn schema_list_string_field_three_way() {
    const SRC: &str = "#schema Cfg { tags: List<String>, port: Int }\n\
                       #main(Cfg cfg) -> Int\ncfg.tags.length()";
    let args = HashMap::from([(
        "cfg".to_string(),
        schema_val(
            "Cfg",
            vec![
                ("tags", Value::list(vec![s("x"), s("y"), s("z"), s("w")])),
                ("port", Value::Int(1)),
            ],
        ),
    )]);
    assert_eq!(assert_three_way(SRC, args), Value::Int(4));
}

/// `#main(Cfg cfg) -> Int = cfg.nums.length()` — a `List<Int>` schema
/// field (the IR schema-field lowering now widens past `List<Int>`-only
/// for the non-Int element lists, but `List<Int>` must still work).
#[test]
fn schema_list_int_field_three_way() {
    const SRC: &str = "#schema Cfg { nums: List<Int>, port: Int }\n\
                       #main(Cfg cfg) -> Int\ncfg.nums.length()";
    let args = HashMap::from([(
        "cfg".to_string(),
        schema_val(
            "Cfg",
            vec![
                (
                    "nums",
                    Value::list(vec![Value::Int(5), Value::Int(6), Value::Int(7)]),
                ),
                ("port", Value::Int(1)),
            ],
        ),
    )]);
    assert_eq!(assert_three_way(SRC, args), Value::Int(3));
}

// --------------- nested schema field walks (stage 4) ------------------

/// `#main(Outer o) -> Int = o.inner.x + o.tag` — a multi-segment walk
/// through a nested `#schema` field. `o.inner` rebases to the inner
/// record's base (pointer-indirect `LoadFieldAtAbsolute`), then `.x`
/// reads a scalar off that base. The value-position field spelling
/// (`inner: Inner`) desugars to the prefix form (`Inner inner: *`).
#[test]
fn nested_schema_field_three_way() {
    const SRC: &str = "#schema Inner { x: Int }\n\
                       #schema Outer { inner: Inner, tag: Int }\n\
                       #main(Outer o) -> Int\no.inner.x + o.tag";
    let args = HashMap::from([(
        "o".to_string(),
        schema_val(
            "Outer",
            vec![
                ("inner", schema_val("Inner", vec![("x", Value::Int(7))])),
                ("tag", Value::Int(3)),
            ],
        ),
    )]);
    assert_eq!(assert_three_way(SRC, args), Value::Int(10));
}

/// The prefix spelling (`Inner inner: *`) of the same nested walk — pins
/// that both field-declaration forms agree across all three backends.
#[test]
fn nested_schema_field_prefix_form_three_way() {
    const SRC: &str = "#schema Inner { Int x: * }\n\
                       #schema Outer { Inner inner: *, Int tag: * }\n\
                       #main(Outer o) -> Int\no.inner.x + o.tag";
    let args = HashMap::from([(
        "o".to_string(),
        schema_val(
            "Outer",
            vec![
                ("inner", schema_val("Inner", vec![("x", Value::Int(7))])),
                ("tag", Value::Int(3)),
            ],
        ),
    )]);
    assert_eq!(assert_three_way(SRC, args), Value::Int(10));
}

/// Three-deep walk (`c.b.a.v`) — each intermediate segment rebases to a
/// further-nested sub-record before the leaf scalar read.
#[test]
fn nested_schema_field_three_levels_three_way() {
    const SRC: &str = "#schema A { v: Int }\n\
                       #schema B { a: A }\n\
                       #schema C { b: B, k: Int }\n\
                       #main(C c) -> Int\nc.b.a.v + c.k";
    let args = HashMap::from([(
        "c".to_string(),
        schema_val(
            "C",
            vec![
                (
                    "b",
                    schema_val(
                        "B",
                        vec![("a", schema_val("A", vec![("v", Value::Int(40))]))],
                    ),
                ),
                ("k", Value::Int(2)),
            ],
        ),
    )]);
    assert_eq!(assert_three_way(SRC, args), Value::Int(42));
}

// ----------------------------- loud caps ------------------------------

/// `List<Schema>` params/fields, nested `List<List<…>>`, and `Dict`
/// params stay loudly rejected on both compiled backends — no silent
/// mis-compile. Pins the cap so it can't regress.
#[test]
fn unsupported_pointer_indirect_shapes_loudly_capped() {
    for src in [
        // List<Schema> param: decode not lowered (LoadListSchemaPtr).
        "#schema P { x: Int }\n#main(List<P> ps) -> List<P>\nps",
        // nested list param.
        "#main(List<List<Int>> xss) -> List<List<Int>>\nxss",
        // Dict param.
        "#main(Dict<String, Int> d) -> Dict<String, Int>\nd",
        // schema field whose type is a List<Schema> (nested decode).
        "#schema Inner { x: Int }\n#schema Cfg { items: List<Inner>, port: Int }\n\
         #main(Cfg cfg) -> Int\ncfg.port",
        // nested schema reachable through a multi-segment walk whose own
        // field is a List<Schema> — the nested-walk support must not
        // accidentally green-light the unimplemented inner list decode.
        "#schema Leaf { x: Int }\n#schema Mid { items: List<Leaf> }\n\
         #schema Outer { mid: Mid, tag: Int }\n#main(Outer o) -> Int\no.tag",
    ] {
        let cl = AotEvaluator::from_source(src);
        assert!(cl.is_err(), "cranelift must loudly reject `{src}`, got Ok");
        let llvm = LlvmAotEvaluator::from_source(src);
        assert!(llvm.is_err(), "llvm must loudly reject `{src}`, got Ok");
    }
}
