//! LLVM sandbox bounds-check smoke tests.
//!
//! These pin the first bounds slice on the native buffer-protocol path:
//! arena-relative address formation now reads `ArenaState::arena_len` and
//! branches to a trap-code path before forming host pointers.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

const STRING_LEN_SRC: &str = "\
#main(String s) -> Int
s.length()";

const STRING_CONCAT_SRC: &str = "\
#main(String a, String b) -> String
a + b";

const LIST_STRING_SRC: &str = "\
#main() -> List<String>
[\"a\", \"\", \"bb\"]";

const EMPTY_LIST_STRING_SRC: &str = "\
#main(Int n) -> List<String>
range(n).map((Int x) => \"s\")";

const CLOSURE_SRC: &str = "\
#main(Int n) -> Dict
{
  #internal
  fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),
  result: fib(n)
}";

const NESTED_FIELD_SRC: &str = "\
#schema Inner { Int pad: *, Int v: * }
#schema Outer { Inner inner: * }
#main(Outer o) -> Int
o.inner.v";

fn args(s: &str) -> HashMap<String, Value> {
    let mut out = HashMap::new();
    out.insert("s".to_string(), Value::String(s.to_string().into()));
    out
}

fn two_string_args(a: &str, b: &str) -> HashMap<String, Value> {
    HashMap::from([
        ("a".to_string(), Value::String(a.to_string().into())),
        ("b".to_string(), Value::String(b.to_string().into())),
    ])
}

fn int_arg(name: &str, n: i64) -> HashMap<String, Value> {
    HashMap::from([(name.to_string(), Value::Int(n))])
}

fn assert_bounds_guard_count(src: &str, min_count: usize) -> LlvmAotEvaluator {
    let ev = LlvmAotEvaluator::from_source(src).expect("LLVM from_source");
    let dump = ev.emit_ir_dump();
    assert!(
        dump.contains("bounds_trap"),
        "LLVM IR dump missing bounds trap block:\n{dump}"
    );
    assert!(
        dump.contains("bounds_ok"),
        "LLVM IR dump missing bounds continuation block:\n{dump}"
    );
    let count = dump.matches("bounds_arena_len").count();
    assert!(
        count >= min_count,
        "LLVM IR dump has {count} arena_len bounds loads, expected at least {min_count}:\n{dump}"
    );
    ev
}

fn as_string_list(v: Value) -> Vec<String> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => s.to_string(),
                other => panic!("expected String element, got {other:?}"),
            })
            .collect(),
        other => panic!("expected List<String>, got {other:?}"),
    }
}

fn fib_result(v: Value) -> i64 {
    match v {
        Value::Dict(d) => match d.map.get("result") {
            Some(Value::Int(n)) => *n,
            other => panic!("expected Dict.result Int, got {other:?}"),
        },
        other => panic!("expected Dict result, got {other:?}"),
    }
}

#[test]
fn string_len_emits_arena_bounds_guards() {
    let _ = assert_bounds_guard_count(STRING_LEN_SRC, 1);
}

#[test]
fn string_len_still_runs_after_bounds_guard() {
    let ev = LlvmAotEvaluator::from_source(STRING_LEN_SRC).expect("LLVM from_source");
    let got = ev.run_main(args("relon")).expect("LLVM run_main");
    assert_eq!(got, Value::Int(5));
}

#[test]
fn string_concat_emits_payload_bounds_guards_and_runs() {
    let ev = assert_bounds_guard_count(STRING_CONCAT_SRC, 6);
    let got = ev
        .run_main(two_string_args("re", "lon"))
        .expect("LLVM run_main");
    assert_eq!(got, Value::String("relon".to_string().into()));
}

#[test]
fn list_string_return_emits_block_bounds_guards_and_runs() {
    let ev = assert_bounds_guard_count(LIST_STRING_SRC, 4);
    let got = ev.run_main(HashMap::new()).expect("LLVM run_main");
    assert_eq!(as_string_list(got), vec!["a", "", "bb"]);
}

#[test]
fn empty_list_string_return_avoids_speculative_off0_load() {
    let ev = assert_bounds_guard_count(EMPTY_LIST_STRING_SRC, 4);
    let got = ev.run_main(int_arg("n", 0)).expect("LLVM run_main");
    assert!(as_string_list(got).is_empty());
}

#[test]
fn closure_handle_paths_emit_bounds_guards_and_run() {
    let ev = assert_bounds_guard_count(CLOSURE_SRC, 4);
    let got = ev.run_main(int_arg("n", 8)).expect("LLVM run_main");
    assert_eq!(fib_result(got), 21);
}

#[test]
fn absolute_field_compose_widens_before_bounds_check() {
    let ev = assert_bounds_guard_count(NESTED_FIELD_SRC, 2);
    let dump = ev.emit_ir_dump();
    assert!(
        dump.contains("bounds_base64")
            && dump.contains("add nuw nsw i64 %bounds_base64")
            && dump.contains(", 16"),
        "LoadFieldAtAbsolute must widen base+offset before bounds checking:\n{dump}"
    );
    assert!(
        !dump.contains("abs_offset_compose = add i32"),
        "LoadFieldAtAbsolute must not wrap base+offset in i32 before bounds checking:\n{dump}"
    );
}
