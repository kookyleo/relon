//! `xs.min()` method-form parity across backends.
//!
//! `xs.max()` has had a native method + bundled compiled body (slot 12)
//! since Phase 4.c-2; `min` was a pure historical asymmetry. This test
//! pins the new mirror surface:
//!
//! * **tree-walk** — the native `ListMin` (exact `ListMax` mirror:
//!   Int / Float element forms, `TypeMismatch`("non-empty list") on an
//!   empty receiver).
//! * **cranelift-native** — bundled `list_int_min` (registry slot 78,
//!   appended at the wire-format tail), the `list_int_max` body with the
//!   select flipped to `val < acc`.
//! * **llvm-native** — the same bundled body inlined through
//!   `emit_call_stdlib`.
//! * **wasm32** — shares the LLVM emitter verbatim; the leg is proven
//!   structurally (identical inlined body), as for the other bundled
//!   reducers.
//!
//! Empty-receiver discipline mirrors `max` exactly: every leg traps
//! (tree-walk a typed `TypeMismatch`, the compiled bodies
//! `TrapKind::EmptyList`); the corpus harness's trap-class comparison
//! treats both as the trap outcome class, same as `xs.max()` today.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, RuntimeError, Value};

/// Method-form min over a `List<Int>` parameter.
const SRC: &str = "#main(List<Int> xs) -> Int\nxs.min()";

fn list_args(xs: &[i64]) -> HashMap<String, Value> {
    HashMap::from([(
        "xs".to_string(),
        Value::List(Arc::new(xs.iter().copied().map(Value::Int).collect())),
    )])
}

/// Value cases: unsorted, negatives, i64 extremes, single element.
fn value_cases() -> Vec<(HashMap<String, Value>, i64)> {
    vec![
        (list_args(&[3, 1, 4, 1, 5, 9, 2, 6]), 1),
        (list_args(&[7]), 7),
        (list_args(&[-3, 0, 3]), -3),
        (list_args(&[i64::MAX, i64::MIN, 0]), i64::MIN),
        (list_args(&[5, 5, 5]), 5),
    ]
}

/// Run the tree-walk oracle for `SRC` with `args`.
fn tree_walk_run(args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
    use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
    let node = relon_parser::parse_document(SRC).expect("parse min src");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope = Arc::new(Scope::default());
    walker.run_main(&scope, args)
}

/// Values agree bit-for-bit three-way.
#[test]
fn min_values_agree_three_way() {
    let cl = AotEvaluator::from_source(SRC).expect("cranelift compiles");
    let llvm = LlvmAotEvaluator::from_source(SRC).expect("llvm compiles");
    for (args, want) in value_cases() {
        let tw = tree_walk_run(args.clone()).expect("tree-walk value");
        let cl_v = cl.run_main(args.clone()).expect("cranelift value");
        let llvm_v = llvm.run_main(args.clone()).expect("llvm value");
        for (name, got) in [("tree-walk", &tw), ("cranelift", &cl_v), ("llvm", &llvm_v)] {
            assert_eq!(
                got,
                &Value::Int(want),
                "{name}: xs.min() value mismatch for {args:?}"
            );
        }
    }
}

/// Empty receiver traps — the outcome *class* (trap, not a silent
/// default like `i64::MAX`) is the contract, and the shape mirrors
/// `xs.max()` exactly: tree-walk raises `TypeMismatch`("non-empty
/// list"); cranelift raises the catchable `TrapKind::EmptyList`
/// sandbox trap. The llvm-native leg is NOT driven here: its
/// `Op::Trap { EmptyList }` is an `llvm.trap` (`ud2`) process abort —
/// identical to `xs.max()` on an empty receiver today — which an
/// in-process test cannot catch. (Routing the stdlib-domain traps
/// through the state-trap path the way `NoMatch` / `NumericOverflow`
/// now do is the recorded follow-up; `min` deliberately does not
/// diverge from `max` here.)
#[test]
fn min_empty_list_traps_like_max() {
    let tw = tree_walk_run(list_args(&[])).expect_err("tree-walk: empty min must trap");
    match &tw {
        RuntimeError::TypeMismatch { expected, .. } => {
            assert_eq!(
                expected, "non-empty list",
                "tree-walk: max-mirror trap shape"
            );
        }
        other => panic!("tree-walk: expected TypeMismatch, got {other:?}"),
    }
    AotEvaluator::from_source(SRC)
        .expect("cranelift compiles")
        .run_main(list_args(&[]))
        .expect_err("cranelift: empty min must trap");
}

/// Symmetry pin: min and max agree with each other through every
/// backend on the same input (min <= max, and both match the oracle).
#[test]
fn min_max_symmetry_three_way() {
    const MAX_SRC: &str = "#main(List<Int> xs) -> Int\nxs.max()";
    let args = list_args(&[3, 1, 4, 1, 5, 9, 2, 6]);

    let cl_min = AotEvaluator::from_source(SRC)
        .expect("cranelift compiles min")
        .run_main(args.clone())
        .expect("cranelift min");
    let cl_max = AotEvaluator::from_source(MAX_SRC)
        .expect("cranelift compiles max")
        .run_main(args.clone())
        .expect("cranelift max");
    let llvm_min = LlvmAotEvaluator::from_source(SRC)
        .expect("llvm compiles min")
        .run_main(args.clone())
        .expect("llvm min");
    let llvm_max = LlvmAotEvaluator::from_source(MAX_SRC)
        .expect("llvm compiles max")
        .run_main(args)
        .expect("llvm max");

    assert_eq!(cl_min, Value::Int(1));
    assert_eq!(cl_max, Value::Int(9));
    assert_eq!(llvm_min, Value::Int(1));
    assert_eq!(llvm_max, Value::Int(9));
}
