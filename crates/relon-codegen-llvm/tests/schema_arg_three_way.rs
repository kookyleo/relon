//! `#main(MySchema cfg)` schema-struct **input** — cross-backend parity.
//!
//! A user-`#schema`-typed `#main` parameter arrives over the
//! buffer-protocol input record as a 4-byte buffer-relative offset slot
//! pointing at a sub-record carrying the struct's scalar fields. The
//! host serialises the branded `Value::Dict` arg into that sub-record
//! via `BufferBuilder::sub_record` / `finish_sub_record`
//! (`write_schema_arg_into_builder` on cranelift,
//! `marshal_schema_in` on llvm); the JIT body reads each field back with
//! `Op::LoadSchemaPtr` (lift the slot to the struct's base) +
//! `Op::LoadFieldAtAbsolute` (load a scalar at base + field_offset).
//!
//! This pins schema-struct input decoding on all three native executors
//! (tree-walk gold standard + cranelift + llvm). It is independent of
//! any effectful builtin: the body is pure field arithmetic over the
//! `#main` param.
//!
//! Scope (matches the agent report's honesty note): only **scalar**
//! schema fields (`Int` / `Float` / `Bool` / `Null`) are exercised —
//! the analyzer cannot yet type `String` / `List` schema-field reads or
//! multi-segment nested-schema walks (`o.inner.x`), and `Dict` /
//! nested-list `#main` params are rejected by the shared IR lowering
//! with a loud `UnsupportedTypeInMain`.

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

/// A branded `Value::Dict` standing in for an instance of a `#schema`.
fn schema_val(brand: &str, fields: Vec<(&str, Value)>) -> Value {
    Value::branded_dict(fields, Some(brand.to_string()))
}

/// All-`Int` schema struct: `cfg.a + cfg.b` reads two scalar slots out
/// of the sub-record. The first field offset exercises the
/// `LoadSchemaPtr` base lift; the second proves field-offset arithmetic.
#[test]
fn schema_struct_all_int_three_way() {
    const SRC: &str = "#schema Cfg { a: Int, b: Int }\n#main(Cfg cfg) -> Int\ncfg.a + cfg.b";
    let args = HashMap::from([(
        "cfg".to_string(),
        schema_val("Cfg", vec![("a", Value::Int(7)), ("b", Value::Int(5))]),
    )]);
    assert_eq!(assert_three_way(SRC, args), Value::Int(12));
}

/// A non-scalar-leading field layout: a `Bool` field precedes the `Int`
/// field the body reads, so the `Int` slot sits at a non-zero offset
/// inside the sub-record. Proves `LoadFieldAtAbsolute`'s `base + offset`
/// composition lands on the right slot across backends.
#[test]
fn schema_struct_bool_then_int_three_way() {
    const SRC: &str = "#schema Cfg { flag: Bool, count: Int }\n#main(Cfg cfg) -> Int\ncfg.count";
    let args = HashMap::from([(
        "cfg".to_string(),
        schema_val(
            "Cfg",
            vec![("flag", Value::Bool(true)), ("count", Value::Int(99))],
        ),
    )]);
    assert_eq!(assert_three_way(SRC, args), Value::Int(99));
}

/// Reading the `Bool` field itself back out as the result, with a wider
/// field set, so several scalar offsets are walked in one record.
#[test]
fn schema_struct_read_bool_field_three_way() {
    const SRC: &str =
        "#schema Cfg { a: Int, b: Int, flag: Bool, c: Int }\n#main(Cfg cfg) -> Bool\ncfg.flag";
    let args = HashMap::from([(
        "cfg".to_string(),
        schema_val(
            "Cfg",
            vec![
                ("a", Value::Int(1)),
                ("b", Value::Int(2)),
                ("flag", Value::Bool(true)),
                ("c", Value::Int(3)),
            ],
        ),
    )]);
    assert_eq!(assert_three_way(SRC, args), Value::Bool(true));
}

/// `Dict<_, _>` and nested-list `#main` params are an intentional,
/// **loud** cap (no silent fallthrough): the shared IR lowering rejects
/// the entry signature before any backend codegen. This pins that
/// verdict so the cap can't regress into a silent mis-compile.
#[test]
fn dict_and_nested_list_params_loudly_capped() {
    for src in [
        "#main(Dict<String, Int> d) -> Dict<String, Int>\nd",
        "#main(List<List<Int>> xss) -> List<List<Int>>\nxss",
    ] {
        let cl = AotEvaluator::from_source(src);
        assert!(cl.is_err(), "cranelift must loudly reject `{src}`, got Ok");
        let llvm = LlvmAotEvaluator::from_source(src);
        assert!(llvm.is_err(), "llvm must loudly reject `{src}`, got Ok");
    }
}
