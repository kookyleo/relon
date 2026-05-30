//! AOT-1 correctness: compile `Float` scalar arithmetic + comparison
//! `#main` programs through the LLVM AOT backend and assert the JIT
//! result matches the tree-walking interpreter **bit-for-bit** (via
//! `f64::to_bits`, so NaN payloads and signed-zero divergences are
//! caught — not just `abs() <= tol`).
//!
//! Before this lane the LLVM emitter had no F64 arithmetic / comparison
//! lowering, `ConstF64` fell into the unsupported-op catch-all, the
//! `LoadField(F64)` path loaded a `double` then called `.into_int_value()`
//! (a latent miscompile / panic), and a Float `#main` return could not
//! be decoded out of the result buffer. These tests drive the full
//! `from_source` pipeline (parse -> analyze -> lower -> LLVM -> JIT) so
//! the new lowering is exercised end-to-end, with the `relon-evaluator`
//! tree-walker as the differential oracle.
//!
//! The `(x, y)` operands are passed at runtime through the HashMap arg
//! pack, so LLVM cannot const-fold the program down to a literal — the
//! arithmetic has to run on opaque function arguments.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// Build a tree-walking oracle for `src`, mirroring the cranelift
/// crate's `helloworld_float_arith.rs` harness shape.
fn build_tree_walker(src: &str) -> (TreeWalkEvaluator, Arc<Scope>) {
    let node = parse_document(src)
        .unwrap_or_else(|e| panic!("parse failed for source:\n{src}\nerror: {e:?}"));
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    (
        TreeWalkEvaluator::new(Arc::new(ctx)),
        Arc::new(Scope::default()),
    )
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Float(f) => f.into_inner(),
        Value::Int(n) => *n as f64,
        other => panic!("expected Float result, got {other:?}"),
    }
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        Value::Bool(b) => i64::from(*b),
        other => panic!("expected Int/Bool result, got {other:?}"),
    }
}

/// Run `src` on the LLVM JIT with two runtime Float args.
fn llvm_xy(src: &str, x: f64, y: f64) -> Value {
    let ev = LlvmAotEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("LLVM compile failed for:\n{src}\nerror: {e:?}"));
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Float(x.into()));
    args.insert("y".to_string(), Value::Float(y.into()));
    ev.run_main(args).expect("LLVM run_main")
}

/// Run `src` on the tree-walker oracle with the same args.
fn oracle_xy(src: &str, x: f64, y: f64) -> Value {
    let (walker, scope) = build_tree_walker(src);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Float(x.into()));
    args.insert("y".to_string(), Value::Float(y.into()));
    walker.run_main(&scope, args).expect("tree-walker run_main")
}

/// Compile + run `src` on both backends with `(x, y)` and assert the
/// Float results are bit-identical (catches NaN / signed-zero). Returns
/// the LLVM result bits so callers can also pin the closed-form value.
fn assert_float_matches_oracle(src: &str, x: f64, y: f64) -> u64 {
    let llvm = as_f64(&llvm_xy(src, x, y));
    let oracle = as_f64(&oracle_xy(src, x, y));
    assert_eq!(
        llvm.to_bits(),
        oracle.to_bits(),
        "LLVM Float result diverged from tree-walker for:\n{src}\n\
         x={x}, y={y}: llvm_bits={:#018x} ({llvm}), oracle_bits={:#018x} ({oracle})",
        llvm.to_bits(),
        oracle.to_bits(),
    );
    llvm.to_bits()
}

/// Same, but the program returns a `Bool` (comparison result).
fn assert_bool_matches_oracle(src: &str, x: f64, y: f64) -> i64 {
    let llvm = as_i64(&llvm_xy(src, x, y));
    let oracle = as_i64(&oracle_xy(src, x, y));
    assert_eq!(
        llvm, oracle,
        "LLVM Bool result diverged from tree-walker for:\n{src}\nx={x}, y={y}",
    );
    llvm
}

#[test]
fn f64_add_returns_sum() {
    let src = "#main(Float x, Float y) -> Float\nx + y";
    let bits = assert_float_matches_oracle(src, 40.5, 1.5);
    assert_eq!(bits, 42.0_f64.to_bits(), "expected 42.0");
}

#[test]
fn f64_sub_mul_div() {
    let sub = "#main(Float x, Float y) -> Float\nx - y";
    assert_eq!(
        assert_float_matches_oracle(sub, 3.25, 10.75),
        (-7.5_f64).to_bits()
    );

    let mul = "#main(Float x, Float y) -> Float\nx * y";
    assert_eq!(
        assert_float_matches_oracle(mul, 6.5, 7.0),
        45.5_f64.to_bits()
    );

    let div = "#main(Float x, Float y) -> Float\nx / y";
    assert_eq!(
        assert_float_matches_oracle(div, 22.0, 7.0),
        (22.0_f64 / 7.0).to_bits()
    );
}

/// Relon's tree-walker oracle raises `DivisionByZero` for `x / 0.0`
/// (see `relon-evaluator::arithmetic::eval_numeric_division`, which
/// checks `right.as_f64() == 0.0` *before* the Int/Float split). So
/// the F64 div path must NOT skip the trap guard — it has to trap on
/// the same operands the oracle rejects, not emit IEEE ±inf.
///
/// `llvm.trap` lowers to `ud2` (SIGILL), which aborts the test binary
/// rather than surfacing a catchable error on stable Rust — mirroring
/// `llvm_divmod_trap.rs`, we assert (1) the oracle rejects the divide,
/// and (2) the emitted LLVM IR carries the `llvm.trap` guard (it
/// survives -O3 because the divisor is a runtime arg, not a constant).
#[test]
fn f64_div_by_zero_traps() {
    let src = "#main(Float x, Float y) -> Float\nx / y";

    // Oracle: a Float divide-by-zero is a runtime error, not ±inf.
    let (walker, scope) = build_tree_walker(src);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Float(1.0.into()));
    args.insert("y".to_string(), Value::Float(0.0.into()));
    let oracle = walker.run_main(&scope, args);
    assert!(
        oracle.is_err(),
        "tree-walker should reject Float div-by-zero, got {oracle:?}"
    );

    // Codegen: the F64 div lowering emits the `llvm.trap` guard so the
    // JIT traps on the same divisor the oracle rejects.
    let ev = LlvmAotEvaluator::from_source(src).expect("LLVM compile");
    let dump = ev.emit_ir_dump();
    assert!(
        dump.contains("llvm.trap"),
        "F64 div IR dump missing llvm.trap guard:\n{dump}"
    );

    // A non-zero divisor still computes the IEEE quotient and agrees
    // with the oracle bit-for-bit.
    let ok = assert_float_matches_oracle(src, 22.0, 7.0);
    assert_eq!(ok, (22.0_f64 / 7.0).to_bits());
}

/// Ordered comparisons: `<` `<=` `>` `>=` route to OLT / OLE / OGT / OGE.
#[test]
fn f64_cmp_ordering() {
    let lt = "#main(Float x, Float y) -> Bool\nx < y";
    assert_eq!(assert_bool_matches_oracle(lt, 1.0, 2.0), 1);
    assert_eq!(assert_bool_matches_oracle(lt, 2.0, 1.0), 0);
    assert_eq!(assert_bool_matches_oracle(lt, 2.0, 2.0), 0);

    let le = "#main(Float x, Float y) -> Bool\nx <= y";
    assert_eq!(assert_bool_matches_oracle(le, 2.0, 2.0), 1);
    assert_eq!(assert_bool_matches_oracle(le, 3.0, 2.0), 0);

    let gt = "#main(Float x, Float y) -> Bool\nx > y";
    assert_eq!(assert_bool_matches_oracle(gt, 3.0, 2.0), 1);
    assert_eq!(assert_bool_matches_oracle(gt, 2.0, 3.0), 0);

    let ge = "#main(Float x, Float y) -> Bool\nx >= y";
    assert_eq!(assert_bool_matches_oracle(ge, 2.0, 2.0), 1);
    assert_eq!(assert_bool_matches_oracle(ge, 1.0, 2.0), 0);
}

/// `==` / `!=` follow `OrderedFloat` equality (OEQ OR'd with a both-NaN
/// test), not raw IEEE OEQ / UNE — see `f64_nan_ne` for the NaN pin.
#[test]
fn f64_eq_ne() {
    let eq = "#main(Float x, Float y) -> Bool\nx == y";
    assert_eq!(assert_bool_matches_oracle(eq, 2.5, 2.5), 1);
    assert_eq!(assert_bool_matches_oracle(eq, 2.5, 2.6), 0);

    let ne = "#main(Float x, Float y) -> Bool\nx != y";
    assert_eq!(assert_bool_matches_oracle(ne, 2.5, 2.6), 1);
    assert_eq!(assert_bool_matches_oracle(ne, 2.5, 2.5), 0);
}

/// NaN comparison semantics — pinned against the tree-walker oracle.
///
/// Relon compares `Value::Float` through `OrderedFloat`'s `PartialEq`
/// for `==` / `!=`, so `NaN == NaN` is **true** and `NaN != NaN` is
/// **false** — the OPPOSITE of raw IEEE `OEQ` / `UNE`. The emitter ORs
/// an explicit both-NaN test into the equality path to reproduce this,
/// so a naive UNE lowering (which would say `NaN != NaN == true`) is
/// caught here. Ordering comparisons stay ordered (NaN → false),
/// matching the evaluator's native `f64` `<` / `>=`.
#[test]
fn f64_nan_ne() {
    let nan = f64::NAN;

    // `OrderedFloat` equality: NaN equals NaN.
    let ne = "#main(Float x, Float y) -> Bool\nx != y";
    assert_eq!(
        assert_bool_matches_oracle(ne, nan, nan),
        0,
        "NaN != NaN must be false (OrderedFloat equality)"
    );

    let eq = "#main(Float x, Float y) -> Bool\nx == y";
    assert_eq!(
        assert_bool_matches_oracle(eq, nan, nan),
        1,
        "NaN == NaN must be true (OrderedFloat equality)"
    );

    // But a NaN is never equal to a non-NaN value.
    assert_eq!(
        assert_bool_matches_oracle(eq, nan, 1.0),
        0,
        "NaN == 1.0 must be false"
    );
    assert_eq!(
        assert_bool_matches_oracle(ne, nan, 1.0),
        1,
        "NaN != 1.0 must be true"
    );

    // Ordering against NaN stays ordered (false).
    let lt = "#main(Float x, Float y) -> Bool\nx < y";
    assert_eq!(
        assert_bool_matches_oracle(lt, nan, 1.0),
        0,
        "NaN < 1.0 must be false (OLT)"
    );

    let ge = "#main(Float x, Float y) -> Bool\nx >= y";
    assert_eq!(
        assert_bool_matches_oracle(ge, nan, 1.0),
        0,
        "NaN >= 1.0 must be false (OGE)"
    );
}

/// A Float `#main` return has to round-trip out of the result buffer
/// (`read_value_from_reader` Float arm + `StoreField(F64)` write side).
/// A mixed-operator body stresses ConstF64 + every arithmetic op
/// together.
#[test]
fn f64_main_return_via_buffer() {
    let src = "#main(Float x, Float y) -> Float\nx * 2.5 + y / 4.0 - 1.0";
    let bits = assert_float_matches_oracle(src, 8.0, 12.0);
    assert_eq!(bits, (8.0_f64 * 2.5 + 12.0 / 4.0 - 1.0).to_bits());
}

/// A `let`-bound Float accumulator exercises the F64 let-slot
/// (`ensure_let_slot` / `coerce_to_let_ty` 64-bit arms) — the value
/// rides through an alloca as i64 bits and comes back out unchanged.
#[test]
fn f64_let_accumulator() {
    // `where` binds a Float accumulator the body squares; the bound
    // value rides through an alloca as i64 bits and comes back out
    // unchanged.
    let src = "#main(Float x, Float y) -> Float\n\
               (acc * acc) where { acc: x + y }";
    let bits = assert_float_matches_oracle(src, 1.5, 2.5);
    let acc = 1.5_f64 + 2.5;
    assert_eq!(bits, (acc * acc).to_bits());
}

/// An `if` whose arms both produce Float exercises the F64 phi arm in
/// `emit_if` (both incomings feed an i64-typed phi carrying the bits).
#[test]
fn f64_if_phi() {
    let src = "#main(Float x, Float y) -> Float\n\
               x > y ? x * 2.0 : y * 2.0";

    // then-arm taken
    let then_bits = assert_float_matches_oracle(src, 5.0, 1.0);
    assert_eq!(then_bits, (5.0_f64 * 2.0).to_bits());

    // else-arm taken
    let else_bits = assert_float_matches_oracle(src, 1.0, 5.0);
    assert_eq!(else_bits, (5.0_f64 * 2.0).to_bits());
}

/// #359: mixing an Int operand into a Float arithmetic context now
/// COMPILES — the IR lowering inserts an `Op::ConvertI64ToF64` promotion
/// on the Int operand, mirroring the tree-walker's
/// `NumericValue::as_f64()` (`value as f64`). This was previously a
/// reject invariant (`lhs_ty != rhs_ty` bailed); flipping it is
/// legitimate precisely because the new behaviour matches the
/// source-of-truth tree-walker bit-for-bit, asserted below.
#[test]
fn f64_mixed_int_float_promotes_and_matches_oracle() {
    // `x` is Float, the literal `1` is Int. `x + 1` promotes the Int to
    // f64 (sitofp), runs `fadd`, result Float. Run on the LLVM JIT and
    // the tree-walker oracle and assert bit-identical f64 results.
    let src = "#main(Float x) -> Float\nx + 1";
    for &x in &[0.0_f64, 1.5, -3.25, 41.0, 1e16, -0.0] {
        let llvm = {
            let ev = LlvmAotEvaluator::from_source(src)
                .unwrap_or_else(|e| panic!("LLVM compile failed for:\n{src}\nerror: {e:?}"));
            let mut args = HashMap::new();
            args.insert("x".to_string(), Value::Float(x.into()));
            as_f64(&ev.run_main(args).expect("LLVM run_main"))
        };
        let oracle = {
            let (walker, scope) = build_tree_walker(src);
            let mut args = HashMap::new();
            args.insert("x".to_string(), Value::Float(x.into()));
            as_f64(&walker.run_main(&scope, args).expect("tree-walker run_main"))
        };
        assert_eq!(
            llvm.to_bits(),
            oracle.to_bits(),
            "x + 1 (Int promotion) diverged for x={x}: \
             llvm_bits={:#018x} ({llvm}) oracle_bits={:#018x} ({oracle})",
            llvm.to_bits(),
            oracle.to_bits(),
        );
        // Closed-form check: x + 1.0.
        assert_eq!(llvm.to_bits(), (x + 1.0_f64).to_bits());
    }
}
