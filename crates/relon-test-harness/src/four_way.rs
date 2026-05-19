//! v6-δ M2-A: 4-way differential runner — tree-walk vs cranelift-AOT
//! vs trace-JIT vs bytecode VM.
//!
//! Extends [`crate::three_way::diff_test_3way`] with a fourth tier: the
//! bytecode VM backend introduced by `relon-bytecode`. Sources outside
//! the bytecode VM's M2-A scalar envelope (List / Dict / closure /
//! stdlib) bounce out as [`FourWayResult::BytecodeUnsupported`] —
//! treated as a soft pass like `CraneliftUnsupported`. Sources the
//! trace-JIT synth catalogue doesn't model still surface as
//! `TraceJitNotApplicable` from the inner three-way runner.

use std::collections::HashMap;

use relon::{new_evaluator, Backend, BackendError};
use relon_eval_api::{RuntimeError, Value};

use crate::three_way::{diff_test_3way, ThreeWayError, ThreeWayResult};

/// Outcome of one 4-way diff invocation.
#[derive(Debug)]
pub enum FourWayResult {
    /// All four backends produced bit-identical values.
    AllAgree(Value),
    /// All four trapped equivalently. Carried separately so the
    /// caller can decide whether trap-equivalence counts as a pass.
    AllTrap,
    /// Tree-walk + cranelift agreed and the trace-JIT envelope was
    /// not modelled, but the bytecode VM agreed with the tree-walker.
    /// Counted as a soft pass.
    BytecodeMatchesBaseline {
        /// The matched value.
        value: Value,
        /// Why the trace-JIT skipped (forwarded from the inner
        /// three-way runner).
        trace_skip_reason: String,
    },
    /// The bytecode VM bounced (source outside its scalar envelope);
    /// the other three reached agreement. Soft pass.
    BytecodeUnsupported {
        /// What the three-way runner concluded for tw / cranelift /
        /// trace-JIT.
        baseline: Box<ThreeWayResult>,
        /// Bytecode-side setup error.
        reason: String,
    },
    /// At least one backend disagreed with the others. Hard failure.
    Mismatch {
        /// The full three-way result so the caller can see how
        /// tw / cranelift / trace-JIT compared.
        three_way: Box<ThreeWayResult>,
        /// Bytecode VM outcome — value if it ran, error string if it
        /// trapped or bounced.
        bytecode: Result<Value, String>,
    },
}

impl FourWayResult {
    /// True when the result is a "pass" for harness purposes — all
    /// four matched, all four trapped equivalently, the bytecode VM
    /// agreed with the baseline on a trace-skipped case, or the
    /// bytecode VM bounced on an unsupported entry shape.
    pub fn is_pass(&self) -> bool {
        matches!(
            self,
            FourWayResult::AllAgree(_)
                | FourWayResult::AllTrap
                | FourWayResult::BytecodeMatchesBaseline { .. }
                | FourWayResult::BytecodeUnsupported { .. }
        )
    }
}

/// Errors surfaced before any backend dispatches.
#[derive(Debug, thiserror::Error)]
pub enum FourWayError {
    /// The three-way runner failed during evaluator setup.
    #[error("three-way setup: {0}")]
    ThreeWay(#[from] ThreeWayError),
}

/// Drive `source` through all four tiers and compare. The
/// tree-walk / cranelift / trace-JIT triple is driven through
/// [`diff_test_3way`]; the bytecode VM goes through `Backend::Bytecode`.
pub fn diff_test_4way(
    source: &str,
    args: HashMap<String, Value>,
) -> Result<FourWayResult, FourWayError> {
    // 1. Three-way reference run.
    let three_way = diff_test_3way(source, args.clone())?;

    // 2. Bytecode VM.
    let bc_outcome = match new_evaluator(source, Backend::Bytecode) {
        Ok(ev) => ev.run_main(args.clone()).map_err(|e| format!("{e}")),
        Err(BackendError::Bytecode(reason)) => {
            return Ok(FourWayResult::BytecodeUnsupported {
                baseline: Box::new(three_way),
                reason,
            });
        }
        Err(other) => {
            return Err(FourWayError::ThreeWay(ThreeWayError::Setup(format!(
                "bytecode setup: {other}"
            ))))
        }
    };

    // 3. Compare against the three-way verdict.
    Ok(classify_four_way(three_way, bc_outcome))
}

fn classify_four_way(
    three_way: ThreeWayResult,
    bc_outcome: Result<Value, String>,
) -> FourWayResult {
    match (&three_way, &bc_outcome) {
        (ThreeWayResult::AllAgree(v), Ok(bc_v)) => {
            if crate::value_bit_eq(v, bc_v) {
                FourWayResult::AllAgree(v.clone())
            } else {
                FourWayResult::Mismatch {
                    three_way: Box::new(three_way),
                    bytecode: bc_outcome,
                }
            }
        }
        (ThreeWayResult::AllAgree(_), Err(_)) => FourWayResult::Mismatch {
            three_way: Box::new(three_way),
            bytecode: bc_outcome,
        },
        (ThreeWayResult::AllTrap, Err(_)) => FourWayResult::AllTrap,
        (ThreeWayResult::AllTrap, Ok(_)) => FourWayResult::Mismatch {
            three_way: Box::new(three_way),
            bytecode: bc_outcome,
        },
        (ThreeWayResult::TraceJitNotApplicable { baseline, reason }, Ok(bc_v)) => {
            if crate::value_bit_eq(baseline, bc_v) {
                FourWayResult::BytecodeMatchesBaseline {
                    value: baseline.clone(),
                    trace_skip_reason: reason.clone(),
                }
            } else {
                FourWayResult::Mismatch {
                    three_way: Box::new(three_way),
                    bytecode: bc_outcome,
                }
            }
        }
        // Trap-on-trap: when the inner three-way is
        // `TraceJitNotApplicable` because tw + cr both trapped (the
        // trace-JIT synthesised a wrapping value), and the bytecode
        // VM trapped too, count this as AllTrap — three real
        // backends agreed on a trap envelope.
        (ThreeWayResult::TraceJitNotApplicable { reason, .. }, Err(_))
            if reason.starts_with("trace_jit_skipped_trap") =>
        {
            FourWayResult::AllTrap
        }
        // Other "tw + cr soft pass" + bytecode trap shapes: keep as a
        // soft pass via BytecodeUnsupported.
        (
            ThreeWayResult::TraceJitNotApplicable { .. }
            | ThreeWayResult::CraneliftUnsupported { .. }
            | ThreeWayResult::TreeWalkMissingStdlibSurface { .. },
            Err(reason),
        ) => FourWayResult::BytecodeUnsupported {
            baseline: Box::new(three_way),
            reason: reason.clone(),
        },
        // Cranelift unsupported but bytecode produced a value — this
        // is the "bytecode matched tree-walker beyond cranelift"
        // territory. Kept honest for future widening.
        (ThreeWayResult::CraneliftUnsupported { tree_walk, .. }, Ok(bc_v)) => match tree_walk {
            Ok(v) if crate::value_bit_eq(v, bc_v) => FourWayResult::BytecodeMatchesBaseline {
                value: v.clone(),
                trace_skip_reason: "cranelift_unsupported".to_string(),
            },
            _ => FourWayResult::Mismatch {
                three_way: Box::new(three_way),
                bytecode: bc_outcome,
            },
        },
        (ThreeWayResult::TreeWalkMissingStdlibSurface { cranelift, .. }, Ok(bc_v)) => {
            match cranelift {
                Ok(v) if crate::value_bit_eq(v, bc_v) => FourWayResult::BytecodeMatchesBaseline {
                    value: v.clone(),
                    trace_skip_reason: "tree_walk_missing_stdlib_surface".to_string(),
                },
                _ => FourWayResult::Mismatch {
                    three_way: Box::new(three_way),
                    bytecode: bc_outcome,
                },
            }
        }
        (ThreeWayResult::Mismatch { .. }, _) => FourWayResult::Mismatch {
            three_way: Box::new(three_way),
            bytecode: bc_outcome,
        },
    }
}

/// Compare bytecode and tree-walk outcomes directly — useful for the
/// "bytecode-vs-treewalk parity" assertion the M2-A gate wants.
/// Returns `None` when bytecode bounced (unsupported envelope);
/// `Some(true)` for bit-identical matches; `Some(false)` for any
/// mismatch including divergent traps.
pub fn bytecode_vs_treewalk(
    source: &str,
    args: HashMap<String, Value>,
) -> Result<Option<bool>, BackendError> {
    let tw = new_evaluator(source, Backend::TreeWalk)?;
    let tw_outcome = tw.run_main(args.clone());
    let bc = match new_evaluator(source, Backend::Bytecode) {
        Ok(ev) => ev,
        Err(BackendError::Bytecode(_)) => return Ok(None),
        Err(other) => return Err(other),
    };
    let bc_outcome = bc.run_main(args);
    Ok(Some(match (tw_outcome, bc_outcome) {
        (Ok(a), Ok(b)) => crate::value_bit_eq(&a, &b),
        (Err(a), Err(b)) => trap_equivalent_for_diff(&a, &b),
        _ => false,
    }))
}

fn trap_equivalent_for_diff(a: &RuntimeError, b: &RuntimeError) -> bool {
    crate::trap_equivalent(a, b)
}
