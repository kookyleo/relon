//! Phase 1 Stage 2.② — `List<Int>` native-signature marshalling.
//!
//! Before this lane the LLVM AOT-binding signature surface
//! (`emitted_field_type_for` / `lower_field_descriptors`) rejected every
//! `List<T>`, and 0b recorded the *return-value decode* gap:
//! `read_value_from_reader` had no `List` arm, so even when the JIT body
//! materialised a `[len][i64…]` tail record the host could not turn it
//! back into a `Value::List`. This test pins what S2.② closes:
//!
//!   1. **Native AOT-binding path** — `emit_object` now lowers a
//!      `List<Int>`-typed slot through the buffer protocol, emitting a
//!      4/4 pointer-indirect slot (`EmittedFieldType::ListInt`) whose
//!      8-aligned tail record is byte-identical to the ConstPool
//!      `add_list_int` blob. This is the descriptor surface the binding
//!      generator stamps.
//!
//!   2. **Real value e2e (return decode)** — driving a const-list return
//!      `#main(...) -> List<Int>` produces a real `Value::List` of
//!      `Value::Int`, cross-checked against the `TreeWalkEvaluator` gold
//!      standard and the cranelift backend. `run_main` for a `List<Int>`
//!      return routes through `run_main_buffer`, which shares
//!      `read_value_from_reader` (and thus `marshal_list_int_out`)
//!      byte-for-byte with the native binary's tail-record reader — so a
//!      buffer-path value match is the body half of the native e2e.
//!
//! ## Reachability (honest — no fake-green)
//!
//! * `List<Int>` **return** is fully reachable: const-list and
//!   param-derived-element-list (`[n, n + 1, 7]`) returns both emit a
//!   real `.o` *and* decode bit-correctly three ways.
//! * A `List<Int>` **parameter** lowers its descriptor (`emit_object`
//!   accepts `#main(List<Int> xs) -> List<Int>`) and the host packs a
//!   correct `[len][i64…]` tail record (`marshal_list_int_in` delegates
//!   to the same `BufferBuilder::write_list_int` the eval-api buffer
//!   tests round-trip). But returning a *param* list by identity
//!   (`xs`) yields garbage out of the JIT body — the frozen codegen
//!   writes the input-buffer-relative pointer into the *output*
//!   buffer's slot without copying the record across the two arenas.
//!   That is a body/codegen limitation (codegen is frozen for this
//!   lane), not a marshalling-seam gap, so this test asserts the
//!   descriptor surface for the param but does **not** claim a working
//!   param→list-return value path. See the module note rather than a
//!   silently-passing assertion.
//! * `List<Int>` parameters consumed *into a scalar* (`xs[0] + xs[1]`)
//!   are rejected by the analyzer (`Analyze(1)`) — a front-end gap noted
//!   in 0b, unreachable here.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::{EmittedEntryShape, EmittedFieldType, LlvmAotEvaluator};
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// `#main() -> List<Int>`: a pure const-list return.
const CONST_LIST_SRC: &str = "#main() -> List<Int>\n[1, 2, 3]\n";

/// `#main(Int n) -> List<Int>`: list whose elements derive from a scalar
/// param — exercises Int param marshalling alongside the new ListInt
/// return decode.
const PARAM_LIST_SRC: &str = "#main(Int n) -> List<Int>\n[n, n + 1, 7]\n";

/// `#main(List<Int> xs) -> List<Int>`: list-param identity. Reachable
/// only for the *descriptor* surface (see the module note on the
/// param→list-return body limitation).
const LIST_PARAM_SRC: &str = "#main(List<Int> xs) -> List<Int>\nxs\n";

/// Pull a `Vec<i64>` out of a `Value::List` of `Value::Int`.
fn as_int_list(v: &Value) -> Vec<i64> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|e| match e {
                Value::Int(n) => *n,
                other => panic!("expected Int list element, got {other:?}"),
            })
            .collect(),
        other => panic!("expected List result, got {other:?}"),
    }
}

/// Tree-walk gold standard for `src` on the given arg map.
fn oracle(src: &str, args: HashMap<String, Value>) -> Vec<i64> {
    let node = parse_document(src).expect("parse src");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope = Arc::new(Scope::default());
    as_int_list(&walker.run_main(&scope, args).expect("tree-walker run_main"))
}

/// Emit a native object for `src` to a unique tmp path and return the
/// build.rs-facing metadata. Proves the AOT-binding signature surface
/// (the path that used to reject every `List<T>`).
fn emit_to_tmp(name: &str, src: &str) -> Result<relon_codegen_llvm::EmitObjectInfo, String> {
    let tmp_dir =
        std::env::temp_dir().join(format!("relon_aot_list_{name}_{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("create tmp dir: {e}"))?;
    let out = tmp_dir.join(format!("{name}.o"));
    let symbol = format!("__test_aot_list_{name}");
    let info =
        LlvmAotEvaluator::emit_object(src, &symbol, &out).map_err(|e| format!("{e:?}"))?;
    let bytes = std::fs::metadata(&out).map_err(|e| format!("stat .o: {e}"))?.len();
    if bytes == 0 {
        return Err("emit_object produced an empty .o".to_string());
    }
    Ok(info)
}

/// The native AOT-binding path lowers a `List<Int>` *return* through the
/// buffer protocol: a pointer-indirect slot the binding generator stamps
/// as `EmittedFieldType::ListInt`. The const-list shape has no params.
#[test]
fn const_list_emit_object_native_descriptors() {
    let info = emit_to_tmp("const_list", CONST_LIST_SRC).expect("emit_object accepts List<Int>");
    assert_eq!(info.shape, EmittedEntryShape::Buffer);
    assert!(info.main_fields.is_empty(), "no #main params");
    assert_eq!(info.return_fields.len(), 1);
    assert_eq!(info.return_fields[0].ty, EmittedFieldType::ListInt);
    // List<Int> returns need the tail region for the [len][i64…] record.
    assert!(info.return_has_tail, "List<Int> return is pointer-indirect");
}

/// A scalar param feeding a `List<Int>` return: one Int main field, one
/// ListInt return field.
#[test]
fn param_list_emit_object_native_descriptors() {
    let info = emit_to_tmp("param_list", PARAM_LIST_SRC).expect("emit_object accepts Int->List");
    assert_eq!(info.shape, EmittedEntryShape::Buffer);
    assert_eq!(info.main_fields.len(), 1);
    assert_eq!(info.main_fields[0].ty, EmittedFieldType::Int);
    assert_eq!(info.return_fields.len(), 1);
    assert_eq!(info.return_fields[0].ty, EmittedFieldType::ListInt);
}

/// A `List<Int>` *parameter* lowers its descriptor surface — the slot
/// the binding generator emits as `EmittedFieldType::ListInt`. (The
/// param→list-return *value* path is a frozen-codegen limitation; see
/// the module note. This only pins that `emit_object` accepts the
/// signature so the binding generator can stamp the ListInt arg row.)
#[test]
fn list_param_emit_object_descriptor_only() {
    let info = emit_to_tmp("list_param", LIST_PARAM_SRC)
        .expect("emit_object accepts List<Int> param descriptor");
    assert_eq!(info.shape, EmittedEntryShape::Buffer);
    assert_eq!(info.main_fields.len(), 1);
    assert_eq!(info.main_fields[0].ty, EmittedFieldType::ListInt);
    assert_eq!(info.param_names, vec!["xs".to_string()]);
    assert_eq!(info.return_fields.len(), 1);
    assert_eq!(info.return_fields[0].ty, EmittedFieldType::ListInt);
}

/// Three-way value e2e for `#main() -> List<Int>`: the LLVM buffer body
/// (sharing `marshal_list_int_out` with the native binary's tail-record
/// reader) and the cranelift backend each match the tree-walk oracle
/// element-for-element. This is the return-decode gap 0b recorded,
/// genuinely closed.
#[test]
fn const_list_value_e2e_three_way() {
    let llvm = LlvmAotEvaluator::from_source(CONST_LIST_SRC)
        .unwrap_or_else(|e| panic!("LLVM from_source: {e:?}"));
    let cl = AotEvaluator::from_source(CONST_LIST_SRC)
        .unwrap_or_else(|e| panic!("cranelift from_source: {e:?}"));

    let want = oracle(CONST_LIST_SRC, HashMap::new());
    assert_eq!(want, vec![1, 2, 3], "oracle sanity");

    let got_llvm = as_int_list(&llvm.run_main(HashMap::new()).expect("llvm run_main"));
    let got_cl = as_int_list(&cl.run_main(HashMap::new()).expect("cranelift run_main"));

    assert_eq!(got_llvm, want, "LLVM List<Int> return decode diverged");
    assert_eq!(got_cl, want, "cranelift List<Int> return decode diverged");
}

/// Three-way value e2e for `#main(Int n) -> List<Int>` over a spread of
/// `n` — the Int param marshalling-in and the new ListInt
/// marshalling-out exercised together.
#[test]
fn param_list_value_e2e_three_way() {
    let llvm = LlvmAotEvaluator::from_source(PARAM_LIST_SRC)
        .unwrap_or_else(|e| panic!("LLVM from_source: {e:?}"));
    let cl = AotEvaluator::from_source(PARAM_LIST_SRC)
        .unwrap_or_else(|e| panic!("cranelift from_source: {e:?}"));

    for n in [0_i64, 1, 5, -3, 100] {
        let mut a = HashMap::new();
        a.insert("n".to_string(), Value::Int(n));

        let want = oracle(PARAM_LIST_SRC, a.clone());
        assert_eq!(want, vec![n, n + 1, 7], "oracle sanity at n={n}");

        let got_llvm = as_int_list(&llvm.run_main(a.clone()).expect("llvm run_main"));
        let got_cl = as_int_list(&cl.run_main(a.clone()).expect("cranelift run_main"));

        assert_eq!(got_llvm, want, "LLVM Int->List<Int> diverged at n={n}");
        assert_eq!(got_cl, want, "cranelift Int->List<Int> diverged at n={n}");
    }
}
