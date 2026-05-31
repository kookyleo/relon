//! ENG cranelift completeness: `Op::Mod(IrType::F64)` lowering.
//!
//! Cranelift has no native float-remainder instruction (x86 has no
//! `frem`; LLVM itself lowers `frem` to an `fmod` libcall). #362 left
//! the cranelift backend rejecting `Float %` with a clean Codegen
//! error; this lane upgrades that to a real `fmod` libcall. The JIT
//! path pins the `fmod` symbol to a Rust `a % b` shim so the result is
//! bit-identical to the tree-walker oracle (`a.as_f64() % b.as_f64()`).
//!
//! These tests drive the full `from_source` pipeline (parse -> analyze
//! -> lower -> cranelift -> JIT) against the `relon-evaluator`
//! tree-walker as the differential oracle. Equality is asserted on the
//! raw `f64::to_bits` pattern — not a tolerance — because `fmod` and
//! Rust `%` are the *same* IEEE-754 operation and must agree exactly,
//! including the sign/truncation edges of a negative dividend and a
//! non-even divisor. The zero-divisor path must trap
//! `RuntimeError::DivisionByZero` like the oracle (the divisor == 0.0
//! guard runs *before* the operation, matching Float `/` — for both
//! `+0.0` and `-0.0`).

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_eval_api::{Evaluator, RuntimeError, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

const MOD_SRC: &str = "#main(Float x, Float y) -> Float\nx % y";

/// Build a tree-walking oracle for `src`, mirroring the
/// `relon-bench` consistency-test harness shape.
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

/// Run the tree-walker oracle on `MOD_SRC` with `(x, y)` and return its
/// `f64` result (panics if it traps).
fn oracle_mod(x: f64, y: f64) -> f64 {
    let (walker, scope) = build_tree_walker(MOD_SRC);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Float(x.into()));
    args.insert("y".to_string(), Value::Float(y.into()));
    as_f64(&walker.run_main(&scope, args).expect("oracle run_main"))
}

/// Compile `MOD_SRC` through cranelift, run it with `(x, y)` and assert
/// the JIT result is **bit-identical** (`f64::to_bits`) to both the
/// tree-walker oracle and the direct Rust `x % y` (which the oracle and
/// the JIT shim both compute).
fn assert_mod_bit_identical(x: f64, y: f64) {
    let aot = AotEvaluator::from_source(MOD_SRC)
        .unwrap_or_else(|e| panic!("cranelift compile failed for `{MOD_SRC}`: {e:?}"));
    let mut aot_args = HashMap::new();
    aot_args.insert("x".to_string(), Value::Float(x.into()));
    aot_args.insert("y".to_string(), Value::Float(y.into()));
    let aot_f = as_f64(&aot.run_main(aot_args).expect("cranelift run_main"));

    let walk_f = oracle_mod(x, y);
    let rust_f = x % y;

    assert_eq!(
        aot_f.to_bits(),
        walk_f.to_bits(),
        "cranelift Float `%` diverged from tree-walker for x={x}, y={y}: \
         cranelift={aot_f} (bits {:#018x}), tree-walker={walk_f} (bits {:#018x})",
        aot_f.to_bits(),
        walk_f.to_bits(),
    );
    assert_eq!(
        aot_f.to_bits(),
        rust_f.to_bits(),
        "cranelift Float `%` diverged from Rust `%` for x={x}, y={y}: \
         cranelift={aot_f} (bits {:#018x}), rust={rust_f} (bits {:#018x})",
        aot_f.to_bits(),
        rust_f.to_bits(),
    );
}

/// Even divisor, positive dividend — the textbook case.
#[test]
fn float_mod_basic() {
    assert_mod_bit_identical(10.0, 3.0);
    assert_mod_bit_identical(7.5, 2.5);
    assert_mod_bit_identical(100.0, 10.0); // exact-zero remainder
}

/// Negative dividend: `fmod` (and Rust `%`) take the **sign of the
/// dividend** and truncate toward zero, so `-10 % 3 == -1` (not the
/// Euclidean `2`). This is the classic place a wrong identity would
/// diverge.
#[test]
fn float_mod_negative_dividend() {
    assert_mod_bit_identical(-10.0, 3.0); // -> -1.0
    assert_mod_bit_identical(-7.5, 2.0); // -> -1.5
    assert_mod_bit_identical(-1.0, 4.0); // -> -1.0 (|dividend| < divisor)
}

/// Negative divisor: the result keeps the dividend's sign, the divisor
/// sign is irrelevant to the remainder magnitude.
#[test]
fn float_mod_negative_divisor() {
    assert_mod_bit_identical(10.0, -3.0); // -> 1.0
    assert_mod_bit_identical(-10.0, -3.0); // -> -1.0
}

/// Non-even / fractional divisor — the remainder is a fraction, so the
/// truncation edge is exercised at full mantissa precision (bit-exact,
/// not within a tolerance).
#[test]
fn float_mod_non_even_divisor() {
    assert_mod_bit_identical(10.0, 3.3);
    assert_mod_bit_identical(22.0, 7.0);
    assert_mod_bit_identical(1.0, 0.3);
    assert_mod_bit_identical(-5.5, 1.1);
}

/// `|dividend| < divisor` returns the dividend unchanged (a common
/// fmod identity edge).
#[test]
fn float_mod_dividend_smaller_than_divisor() {
    assert_mod_bit_identical(2.0, 5.0); // -> 2.0
    assert_mod_bit_identical(0.25, 1.0); // -> 0.25
}

/// A zero divisor (`+0.0`) must trap `DivisionByZero` on BOTH the
/// oracle and cranelift — the divisor == 0.0 guard runs before the
/// fmod call, exactly like Float `/`. (A raw fmod would return NaN.)
#[test]
fn float_mod_by_pos_zero_traps_like_oracle() {
    // Oracle traps.
    let (walker, scope) = build_tree_walker(MOD_SRC);
    let mut walk_args = HashMap::new();
    walk_args.insert("x".to_string(), Value::Float(1.0.into()));
    walk_args.insert("y".to_string(), Value::Float(0.0.into()));
    let oracle = walker.run_main(&scope, walk_args);
    assert!(
        matches!(oracle, Err(RuntimeError::DivisionByZero(_))),
        "tree-walker should reject Float mod-by-zero, got {oracle:?}"
    );

    // Cranelift traps with the same typed error.
    let aot = AotEvaluator::from_source(MOD_SRC).expect("cranelift compile");
    let mut aot_args = HashMap::new();
    aot_args.insert("x".to_string(), Value::Float(1.0.into()));
    aot_args.insert("y".to_string(), Value::Float(0.0.into()));
    let err = aot
        .run_main(aot_args)
        .expect_err("Float mod-by-+0.0 must trap");
    assert!(
        matches!(err, RuntimeError::DivisionByZero(_)),
        "expected DivisionByZero from cranelift Float mod, got {err:?}"
    );
}

/// `-0.0` divisor must trap too — the ordered-equal guard (`== 0.0`)
/// catches both signed zeros, matching the oracle (`right.as_f64() ==
/// 0.0` is true for `-0.0`).
#[test]
fn float_mod_by_neg_zero_traps_like_oracle() {
    // Oracle traps on -0.0 divisor.
    let (walker, scope) = build_tree_walker(MOD_SRC);
    let mut walk_args = HashMap::new();
    walk_args.insert("x".to_string(), Value::Float(3.0.into()));
    walk_args.insert("y".to_string(), Value::Float((-0.0_f64).into()));
    let oracle = walker.run_main(&scope, walk_args);
    assert!(
        matches!(oracle, Err(RuntimeError::DivisionByZero(_))),
        "tree-walker should reject Float mod by -0.0, got {oracle:?}"
    );

    let aot = AotEvaluator::from_source(MOD_SRC).expect("cranelift compile");
    let mut aot_args = HashMap::new();
    aot_args.insert("x".to_string(), Value::Float(3.0.into()));
    aot_args.insert("y".to_string(), Value::Float((-0.0_f64).into()));
    let err = aot
        .run_main(aot_args)
        .expect_err("Float mod by -0.0 must trap");
    assert!(
        matches!(err, RuntimeError::DivisionByZero(_)),
        "expected DivisionByZero for -0.0 divisor, got {err:?}"
    );
}

/// A non-zero divisor right after a trapping one still computes the
/// correct remainder — the guard does not poison the normal path.
#[test]
fn float_mod_normal_divisor_after_zero_guard() {
    assert_mod_bit_identical(3.0, 2.0); // -> 1.0
    assert_mod_bit_identical(-3.0, 2.0); // -> -1.0
}
