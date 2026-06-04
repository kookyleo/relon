//! W5-P3: `DictGetByStringKey` static path — `d[k]` on a materialised
//! `{String -> Int}` dict, three-way aligned (tree-walk gold standard /
//! cranelift golden / LLVM AOT).
//!
//! ## What this pins
//!
//! The third + final W5-epic read primitive: indexing a materialised
//! dict (`Op::ConstDict` arena record, P1) by a `String` key. Before P3
//! both backends rejected the lookup (`DictGetByStringKey` was
//! trace-recorder-only / `unsupported`) and `d[k]` routed through the
//! tree-walker's `try_index_method`. P3 lowers `d[k]` to a fully
//! IR-lowered linear-scan + byte-compare probe over the arena entry
//! table — no new runtime helper / wasm import, so native + wasm32 run
//! byte-identically (same op set the bundled `starts_with` body uses).
//!
//! 1. **IR shape** — the source lowers through `Op::ConstDict` (the
//!    dict value) and the probe primitives (`Op::Block`/`Op::Loop`/
//!    `Op::BrIf`/`Op::LoadI8UAtAbsolute`/`Op::LoadI64AtAbsolute`), and
//!    NEVER emits `Op::DictGetByStringKey` (that op stays
//!    trace-recorder-only; static codegen would reject it).
//! 2. **Codegen parity** — both backends accept the source through
//!    `from_source`; the cranelift golden's acceptance is the oracle.
//! 3. **Value three-way** — tree-walk / cranelift / LLVM all return the
//!    same `d[key]` value for every probed key (first / middle / last
//!    of the sorted entry table).
//!
//! The probe key here is a `ConstString` literal (the `from_source`-
//! reachable shape in P3; a `keys[i]` `List<String>` index needs the
//! `#internal keys` list-field capture, which the anon-dict classifier
//! still scope-cuts until P4). The value is NOT constant-folded — the
//! probe runs its full length + byte compare at runtime for every key.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_ir::ir::{Op, TaggedOp};
use relon_parser::parse_document;

/// A `#main` dict body binds `#internal d` (a `{String -> Int}` dict
/// value); the `result` field probes it with the `ConstString` literal
/// `key`. The five-entry dict spans the sorted table so distinct keys
/// land first / middle / last.
fn src_for(key: &str) -> String {
    format!(
        "#main(Int i) -> Dict\n\
         {{\n\
           #internal\n\
           d: {{ a: 1, b: 2, c: 3, d: 4, e: 5 }},\n\
           result: d[\"{key}\"]\n\
         }}"
    )
}

/// Reference source for the IR-shape assertions.
fn ref_src() -> String {
    src_for("c")
}

fn flatten_ops(src: &str) -> Vec<Op> {
    let options = relon_analyzer::AnalyzeOptions {
        strict_mode: false,
        ..Default::default()
    };
    let lowered = relon_ir::compile(src, &options).expect("frontend compile");
    let mut out = Vec::new();
    for func in &lowered.module.funcs {
        collect(&func.body, &mut out);
    }
    out
}

fn collect(body: &[TaggedOp], out: &mut Vec<Op>) {
    for t in body {
        out.push(t.op.clone());
        match &t.op {
            Op::Block { body, .. } | Op::Loop { body, .. } => collect(body, out),
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                collect(then_body, out);
                collect(else_body, out);
            }
            _ => {}
        }
    }
}

/// Pull the `result` Int field out of the returned dict.
fn result_of(v: &Value) -> i64 {
    let dict = match v {
        Value::Dict(d) => d,
        other => panic!("expected dict return, got {other:?}"),
    };
    match dict.map.get("result").expect("return dict has `result`") {
        Value::Int(n) => *n,
        other => panic!("expected Int `result`, got {other:?}"),
    }
}

/// Tree-walk gold standard for `src` (the `#main` Int param is unused).
fn tree_walk(src: &str) -> i64 {
    let node = parse_document(src).expect("parse src");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope = Arc::new(Scope::default());
    let mut args = HashMap::new();
    args.insert("i".to_string(), Value::Int(0));
    result_of(&walker.run_main(&scope, args).expect("tree-walk run_main"))
}

fn arg_i() -> HashMap<String, Value> {
    let mut a = HashMap::new();
    a.insert("i".to_string(), Value::Int(0));
    a
}

#[test]
fn dict_get_lowers_to_probe_not_dict_get_op() {
    let ops = flatten_ops(&ref_src());
    // The dict value is materialised via Op::ConstDict.
    assert!(
        ops.iter().any(|o| matches!(o, Op::ConstDict { .. })),
        "src must lower to Op::ConstDict; ops:\n{ops:#?}"
    );
    // The probe is IR-lowered: a byte-compare loop reading entry bytes
    // and the matched i64 value.
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::LoadI8UAtAbsolute { .. })),
        "probe must byte-compare via LoadI8UAtAbsolute; ops:\n{ops:#?}"
    );
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::LoadI64AtAbsolute { .. })),
        "probe must load the matched i64 value via LoadI64AtAbsolute; ops:\n{ops:#?}"
    );
    // Static codegen must NOT emit Op::DictGetByStringKey (it stays
    // trace-recorder-only; the cranelift / llvm catch-alls reject it).
    assert!(
        !ops.iter().any(|o| matches!(o, Op::DictGetByStringKey { .. })),
        "static codegen must NOT emit Op::DictGetByStringKey; ops:\n{ops:#?}"
    );
}

#[test]
fn dict_get_codegen_parity() {
    for key in ["a", "c", "e"] {
        let src = src_for(key);
        AotEvaluator::from_source(&src)
            .unwrap_or_else(|e| panic!("cranelift golden must compile d[{key:?}]\nerr: {e:?}"));
        LlvmAotEvaluator::from_source(&src)
            .unwrap_or_else(|e| panic!("llvm backend must compile d[{key:?}]\nerr: {e:?}"));
    }
}

#[test]
fn dict_get_three_way() {
    // first ("a"=1), middle ("c"=3), last ("e"=5) of the sorted table.
    for (key, want) in [("a", 1), ("b", 2), ("c", 3), ("d", 4), ("e", 5)] {
        let src = src_for(key);
        let oracle = tree_walk(&src);
        assert_eq!(oracle, want, "tree-walk oracle sanity at key={key}");

        let llvm = LlvmAotEvaluator::from_source(&src).expect("llvm compiles");
        let cl = AotEvaluator::from_source(&src).expect("cranelift compiles");

        let got_llvm = result_of(&llvm.run_main(arg_i()).expect("llvm run_main"));
        let got_cl = result_of(&cl.run_main(arg_i()).expect("cranelift run_main"));

        assert_eq!(got_cl, oracle, "cranelift dict-get diverged at key={key}");
        assert_eq!(got_llvm, oracle, "llvm dict-get diverged at key={key}");
    }
}

/// Honest-miss: a key absent from the dict must NOT silently return a
/// wrong value. The probe emits an `Op::Trap { IndexOutOfBounds }` on
/// the not-found path (a portable wasm `unreachable` / native CPU trap,
/// not a catchable in-process error in AOT — so this is asserted at the
/// IR level rather than by running the trap, which would SIGILL the
/// test process). w5 itself always hits, so this path never fires
/// there; it exists purely so a miss can never surface a wrong value.
#[test]
fn dict_get_miss_emits_trap_not_silent_value() {
    let ops = flatten_ops(&src_for("z"));
    assert!(
        ops.iter().any(|o| matches!(
            o,
            Op::Trap {
                kind: relon_ir::ir::TrapKind::IndexOutOfBounds
            }
        )),
        "dict-get probe must trap on miss (no silent wrong value); ops:\n{ops:#?}"
    );
}
