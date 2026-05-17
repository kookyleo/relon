//! Phase 10-c smoke tests for the `List<Float / Bool / String / Schema>`
//! support added to the wasm-AOT backend.
//!
//! Each test drives the full pipeline (parse → analyze → IR lowering
//! → codegen-wasm → WasmAotEvaluator) and confirms either the
//! `length()` dispatch returns the expected element count for an
//! input parameter, or that a constant list literal emitted via
//! `Op::ConstList*` materialises with the right payload.

use ordered_float::OrderedFloat;
use relon_codegen_wasm::WasmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use std::collections::HashMap;

fn run(src: &str, args: HashMap<String, Value>) -> Value {
    let aot = WasmAotEvaluator::from_source(src).expect("compile");
    aot.run_main(args).expect("run_main")
}

#[test]
fn list_float_field_roundtrip() {
    // List<Float> parameter -> Int length via the new ListFloat
    // dispatch in stdlib_method_index.
    let src = "#main(List<Float> xs, Int idx) -> Int\nxs.length() + idx";
    let mut args = HashMap::new();
    args.insert(
        "xs".to_string(),
        Value::list(
            vec![1.5_f64, -2.25, 3.125, 4.0]
                .into_iter()
                .map(|f| Value::Float(OrderedFloat(f)))
                .collect(),
        ),
    );
    args.insert("idx".to_string(), Value::Int(10));
    let out = run(src, args);
    assert_eq!(out, Value::Int(14));
}

#[test]
fn list_bool_field_roundtrip() {
    let src = "#main(List<Bool> xs, Int idx) -> Int\nxs.length() + idx";
    let mut args = HashMap::new();
    args.insert(
        "xs".to_string(),
        Value::list(vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(true),
        ]),
    );
    args.insert("idx".to_string(), Value::Int(100));
    let out = run(src, args);
    assert_eq!(out, Value::Int(103));
}

#[test]
fn list_string_field_roundtrip() {
    let src = "#main(List<String> xs, Int idx) -> Int\nxs.length() + idx";
    let mut args = HashMap::new();
    args.insert(
        "xs".to_string(),
        Value::list(vec![
            Value::String("alpha".to_string()),
            Value::String("beta".to_string()),
            Value::String("".to_string()),
            Value::String("gamma".to_string()),
            Value::String("delta".to_string()),
        ]),
    );
    args.insert("idx".to_string(), Value::Int(2));
    let out = run(src, args);
    assert_eq!(out, Value::Int(7));
}

#[test]
fn list_record_field_roundtrip() {
    // Branded sub-record element. Confirms the host bridge builds
    // each entry via list_record_writer + finish_entry and the wasm
    // side resolves the length through `list_schema_length`.
    let src = "#schema User { Int age: *, String name: * }\n\
        #main(List<User> users) -> Int\nusers.length()";
    let mut users = Vec::new();
    for (age, name) in [(36_i64, "ada"), (41, "bob"), (19, "zoe")] {
        let mut map = std::collections::BTreeMap::new();
        map.insert("age".to_string(), Value::Int(age));
        map.insert("name".to_string(), Value::String(name.to_string()));
        users.push(Value::branded_dict(map, Some("User".to_string())));
    }
    let mut args = HashMap::new();
    args.insert("users".to_string(), Value::list(users));
    let out = run(src, args);
    assert_eq!(out, Value::Int(3));
}

#[test]
fn const_list_float_output() {
    let src = "#main(Int x) -> List<Float>\n[1.0, 2.0, 3.0]";
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(0));
    let out = run(src, args);
    let expected = Value::list(
        vec![1.0_f64, 2.0, 3.0]
            .into_iter()
            .map(|f| Value::Float(OrderedFloat(f)))
            .collect(),
    );
    assert_eq!(out, expected);
}

#[test]
fn const_list_bool_output() {
    let src = "#main(Int x) -> List<Bool>\n[true, false, true]";
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(0));
    let out = run(src, args);
    let expected = Value::list(vec![
        Value::Bool(true),
        Value::Bool(false),
        Value::Bool(true),
    ]);
    assert_eq!(out, expected);
}
