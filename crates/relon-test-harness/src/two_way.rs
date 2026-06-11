//! Two-way differential runner — tree-walk (oracle) vs cranelift-AOT,
//! reporting outcomes (including mismatches) as values.
//!
//! The runner takes the same `(source, args)` shape as
//! [`crate::diff_test`] but returns every classification — including
//! `Mismatch` — as a [`TwoWayResult`] variant instead of an error, and
//! carries the agreed `Value` on [`TwoWayResult::Agree`] so callers can
//! additionally assert the expected output, not just backend agreement.
//!
//! Historical note: this module used to host a third "trace-JIT" leg.
//! That leg was a string-pattern synthesiser keyed off recipe source
//! strings — it never executed an engine (the trace-JIT crate is gone
//! from the workspace) and therefore added zero differential value. It
//! has been retired; the real compiled legs beyond cranelift (wasm,
//! llvm-native) are covered by the `aot_wasm_parity` /
//! `inplace_return_four_way` codegen tests.

use std::collections::HashMap;

use relon::{new_evaluator, Backend, BackendError};
use relon_eval_api::{RuntimeError, Value};

/// Outcome of one [`diff_test_2way`] invocation.
#[derive(Debug)]
pub enum TwoWayResult {
    /// Both backends produced the same `Value` (carried for callers
    /// that also want to assert the expected output).
    Agree(Value),
    /// Both ran but produced different values (or one trapped while
    /// the other returned a value). Hard correctness failure.
    Mismatch {
        tree_walk: Result<Value, String>,
        cranelift: Result<Value, String>,
    },
    /// Cranelift-AOT couldn't compile the source; tree-walk's result
    /// is recorded for context.
    CraneliftUnsupported {
        tree_walk: Result<Value, String>,
        reason: String,
    },
    /// Tree-walker reports `FunctionNotFound` / `MethodNotFound` for
    /// stdlib surfaces the IR / cranelift pipeline accepts as
    /// free-function calls. Mirrors `DiffOutcome::TreeWalkMissingStdlibSurface`
    /// from [`crate::diff_test`]; documented as a non-fatal divergence
    /// on the differential corpus.
    TreeWalkMissingStdlibSurface {
        cranelift: Result<Value, String>,
        tree_walk_error: String,
    },
    /// Both trapped equivalently. Carried separately from `Agree` so
    /// the caller can decide whether trap-equivalence counts toward a
    /// pass.
    BothTrap,
}

impl TwoWayResult {
    /// True when the result is a "pass" for harness purposes — both
    /// backends matched, both trapped equivalently, or cranelift
    /// legitimately declined the source.
    pub fn is_pass(&self) -> bool {
        matches!(
            self,
            TwoWayResult::Agree(_)
                | TwoWayResult::BothTrap
                | TwoWayResult::CraneliftUnsupported { .. }
                | TwoWayResult::TreeWalkMissingStdlibSurface { .. }
        )
    }
}

/// Two-way diff: tree-walk (oracle) vs cranelift-AOT, with every
/// classification reported as a [`TwoWayResult`] value.
pub fn diff_test_2way(
    source: &str,
    args: HashMap<String, Value>,
) -> Result<TwoWayResult, TwoWayError> {
    // 1. Tree-walk reference.
    let tw_ev = new_evaluator(source, Backend::TreeWalk)
        .map_err(|e| TwoWayError::Setup(format!("tree-walk: {e}")))?;
    let tw_outcome = tw_ev.run_main(args.clone());

    // 2. Cranelift-AOT.
    let cr_ev = match new_evaluator(source, Backend::CraneliftAot) {
        Ok(ev) => ev,
        Err(BackendError::CraneliftAot(reason)) => {
            return Ok(TwoWayResult::CraneliftUnsupported {
                tree_walk: tw_outcome.map_err(|e| format!("{e}")),
                reason,
            });
        }
        Err(other) => return Err(TwoWayError::Setup(format!("cranelift: {other}"))),
    };
    let cr_outcome = cr_ev.run_main(args);

    match (&tw_outcome, &cr_outcome) {
        (Ok(tw), Ok(cr)) => {
            if values_equal(tw, cr) {
                Ok(TwoWayResult::Agree(tw.clone()))
            } else {
                Ok(TwoWayResult::Mismatch {
                    tree_walk: Ok(tw.clone()),
                    cranelift: Ok(cr.clone()),
                })
            }
        }
        (Err(tw_err), Err(cr_err)) => {
            if trap_kinds_match(tw_err, cr_err) {
                Ok(TwoWayResult::BothTrap)
            } else {
                Ok(TwoWayResult::Mismatch {
                    tree_walk: Err(format!("{tw_err:?}")),
                    cranelift: Err(format!("{cr_err:?}")),
                })
            }
        }
        (Err(tw_err), Ok(cr)) => {
            // Tree-walker stdlib-surface gap (`FunctionNotFound`):
            // mirror the `diff_test` driver's
            // `TreeWalkMissingStdlibSurface` outcome rather than
            // failing the case.
            if matches!(tw_err, RuntimeError::FunctionNotFound(_, _)) {
                Ok(TwoWayResult::TreeWalkMissingStdlibSurface {
                    cranelift: Ok(cr.clone()),
                    tree_walk_error: format!("{tw_err:?}"),
                })
            } else {
                Ok(TwoWayResult::Mismatch {
                    tree_walk: Err(format!("{tw_err:?}")),
                    cranelift: Ok(cr.clone()),
                })
            }
        }
        (Ok(tw), Err(cr_err)) => Ok(TwoWayResult::Mismatch {
            tree_walk: Ok(tw.clone()),
            cranelift: Err(format!("{cr_err:?}")),
        }),
    }
}

/// Errors that can surface from [`diff_test_2way`] before the runner
/// reaches the per-backend compare.
#[derive(Debug, thiserror::Error)]
pub enum TwoWayError {
    /// Backend setup (tree-walk or cranelift) failed in an
    /// unrecoverable way.
    #[error("backend setup failed: {0}")]
    Setup(String),
}

fn values_equal(a: &Value, b: &Value) -> bool {
    crate::value_bit_eq(a, b)
}

fn trap_kinds_match(a: &RuntimeError, b: &RuntimeError) -> bool {
    crate::trap_equivalent(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_args(x: i64, y: i64) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("x".to_string(), Value::Int(x));
        m.insert("y".to_string(), Value::Int(y));
        m
    }

    #[test]
    fn diff_test_2way_arith_add_agrees() {
        let args = make_args(100, 23);
        let result = diff_test_2way("#main(Int x, Int y) -> Int\nx + y", args).expect("ok");
        assert!(
            matches!(result, TwoWayResult::Agree(Value::Int(123))),
            "got {result:?}"
        );
    }

    #[test]
    fn diff_test_2way_arith_sub_agrees() {
        let args = make_args(50, 8);
        let result = diff_test_2way("#main(Int x, Int y) -> Int\nx - y", args).expect("ok");
        assert!(
            matches!(result, TwoWayResult::Agree(Value::Int(42))),
            "got {result:?}"
        );
    }

    #[test]
    fn diff_test_2way_chained_arith_agrees() {
        let args = make_args(40, 2);
        let result = diff_test_2way("#main(Int x, Int y) -> Int\nx + y + 1", args).expect("ok");
        assert!(
            matches!(result, TwoWayResult::Agree(Value::Int(43))),
            "got {result:?}"
        );
    }
}
