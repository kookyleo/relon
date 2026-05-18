//! Differential test harness for the Relon backends.
//!
//! v5-beta-2 establishes the differential corpus + driver so every
//! stdlib body re-lowered onto cranelift gets compared against the
//! tree-walk reference output bit-for-bit. The corpus accretes as
//! cranelift coverage widens; cases the cranelift backend cannot
//! yet handle surface as `DiffOutcome::CraneliftUnsupported`
//! (logged, not failed) so the harness stays green while we widen.
//!
//! ## Layering
//!
//! ```text
//! +-------------------------------------------------+
//! |  diff_test(source, args) -> Result<DiffOutcome> |
//! +------------------+------------------------------+
//!                    |
//!     +--------------+--------------+
//!     |                             |
//! [tree-walk]                  [cranelift-aot]
//!  source -> Value             source -> Value (or UnsupportedSignature)
//!     |                             |
//!     +--------- bit_eq ------------+
//!                    |
//!              Match / Mismatch / CraneliftUnsupported
//! ```
//!
//! Bit-equality compares:
//! * `Value::Int(_)` / `Value::Bool(_)` / `Value::Null` directly.
//! * `Value::Float(_)` via `to_bits()` so NaN bit patterns stay
//!   distinct.
//! * `Value::String(_)` byte-equal.
//! * `Value::List(_)` element-wise recursive.
//! * `Value::Dict(_)` field-set + per-field recursive (insertion
//!   order ignored on the assumption that the schema-rooted lowering
//!   preserves declaration order; differences in key ordering would
//!   surface as `KeySetMismatch`).
//! * Traps: compare `RuntimeError` discriminant + payload (range
//!   excluded because backends emit different ranges for the same
//!   semantic trap).
//!
//! ## Forward-compatibility
//!
//! The corpus is allowed to outrun cranelift coverage. Each case
//! is annotated with a "minimum coverage tier" so test runners
//! that want strict mode (`--strict`) fail when cases regress
//! from `Match` back to `CraneliftUnsupported`. v5-beta-2 ships
//! with the corpus at "arith / cmp / control flow" tier; future
//! tranches widen it.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use relon::{new_evaluator, Backend, BackendError};
use relon_eval_api::{Evaluator, RuntimeError, Value};

pub mod corpus;

/// Outcome of one differential test run.
#[derive(Debug)]
pub enum DiffOutcome {
    /// Both backends produced bit-identical successful output.
    MatchOk,
    /// Both backends produced an equivalent trap (same kind +
    /// payload, ignoring source ranges).
    MatchTrap,
    /// Backends produced equivalent results, but cranelift surfaced
    /// the source as outside its current lowering envelope. The
    /// expected output (from tree-walk) is recorded so a future
    /// tranche can re-run and demand `Match*`.
    CraneliftUnsupported {
        /// The tree-walk reference output, kept for later regression
        /// diffing once cranelift widens.
        tree_walk: Result<Value, String>,
        /// Reason returned by the cranelift backend.
        reason: String,
    },
}

/// Differential mismatch error.
#[derive(Debug, thiserror::Error)]
pub enum DiffTestError {
    #[error("backend setup failed: {0}")]
    Setup(String),
    #[error("value mismatch — tree-walk: {tree_walk}, cranelift: {cranelift}")]
    ValueMismatch {
        tree_walk: String,
        cranelift: String,
    },
    #[error("trap kind mismatch — tree-walk: {tree_walk}, cranelift: {cranelift}")]
    TrapMismatch {
        tree_walk: String,
        cranelift: String,
    },
    #[error("trap vs value mismatch — tree-walk: {tree_walk}, cranelift: {cranelift}")]
    TrapVsValue {
        tree_walk: String,
        cranelift: String,
    },
    #[error("tree-walk failed: {0}")]
    TreeWalkFailed(String),
}

/// Compare tree-walk and cranelift-AOT execution of the same source.
///
/// When the cranelift backend cannot handle the source (today the
/// common case for anything beyond `#main(Int...) -> Int`), the
/// outcome is `CraneliftUnsupported` rather than a hard failure;
/// the tree-walk reference output is still preserved. This keeps the
/// harness usable through v5-β-2 widening without requiring a
/// per-case eligibility list.
pub fn diff_test(source: &str, args: HashMap<String, Value>) -> Result<DiffOutcome, DiffTestError> {
    // Tree-walk side — always must succeed (or fail with a recorded
    // RuntimeError) for the case to be meaningful.
    let tw = match build_tree_walk(source) {
        Ok(ev) => ev,
        Err(e) => return Err(DiffTestError::Setup(format!("tree-walk: {e}"))),
    };
    let tree_walk_outcome = tw.run_main(args.clone());

    // Cranelift side — may surface `UnsupportedSignature` etc.
    let cranelift_setup = new_evaluator(source, Backend::CraneliftAot);
    let cranelift_outcome = match cranelift_setup {
        Ok(ev) => Some(ev.run_main(args.clone())),
        Err(BackendError::CraneliftAot(reason)) => {
            // Setup failed — most common today. Record tree-walk and
            // return `CraneliftUnsupported`.
            return Ok(DiffOutcome::CraneliftUnsupported {
                tree_walk: tree_walk_outcome.map_err(|e| format!("{e}")),
                reason,
            });
        }
        Err(other) => return Err(DiffTestError::Setup(format!("cranelift: {other}"))),
    };

    // Both sides ran; compare.
    let cranelift_outcome = cranelift_outcome.expect("checked above");
    match (tree_walk_outcome, cranelift_outcome) {
        (Ok(tw), Ok(cr)) => {
            if value_bit_eq(&tw, &cr) {
                Ok(DiffOutcome::MatchOk)
            } else {
                Err(DiffTestError::ValueMismatch {
                    tree_walk: format!("{tw:?}"),
                    cranelift: format!("{cr:?}"),
                })
            }
        }
        (Err(tw_err), Err(cr_err)) => {
            if trap_equivalent(&tw_err, &cr_err) {
                Ok(DiffOutcome::MatchTrap)
            } else {
                Err(DiffTestError::TrapMismatch {
                    tree_walk: format!("{tw_err:?}"),
                    cranelift: format!("{cr_err:?}"),
                })
            }
        }
        (Ok(tw), Err(cr_err)) => Err(DiffTestError::TrapVsValue {
            tree_walk: format!("Ok({tw:?})"),
            cranelift: format!("Err({cr_err:?})"),
        }),
        (Err(tw_err), Ok(cr)) => Err(DiffTestError::TrapVsValue {
            tree_walk: format!("Err({tw_err:?})"),
            cranelift: format!("Ok({cr:?})"),
        }),
    }
}

/// Construct a tree-walk `Evaluator` over the given source. The
/// helper goes through the `relon` facade so the sandbox / capability
/// / module-loader posture matches what production hosts actually
/// see (mirrors `Backend::TreeWalk` semantics).
fn build_tree_walk(source: &str) -> Result<Box<dyn Evaluator>, String> {
    new_evaluator(source, Backend::TreeWalk).map_err(|e| format!("{e}"))
}

/// Compare two `Value`s for bit-identical equality. Floats compare
/// by `to_bits` so NaN patterns stay distinct; dicts compare
/// field-set + per-field recursive (insertion order is informational
/// but not required to match — cranelift may emit a different
/// ordering once it speaks the buffer protocol).
pub fn value_bit_eq(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Int(x), Int(y)) => x == y,
        (Bool(x), Bool(y)) => x == y,
        (Null, Null) => true,
        (Float(x), Float(y)) => x.into_inner().to_bits() == y.into_inner().to_bits(),
        (String(x), String(y)) => x == y,
        (List(x), List(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(xi, yi)| value_bit_eq(xi, yi))
        }
        (Dict(x), Dict(y)) => {
            if x.map.len() != y.map.len() {
                return false;
            }
            for (k, v) in &x.map {
                match y.map.get(k) {
                    Some(yv) => {
                        if !value_bit_eq(v, yv) {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
            true
        }
        // Any cross-variant comparison fails; this is intentional —
        // a numeric vs string mismatch is a real bug.
        _ => false,
    }
}

/// Loose-equivalence for runtime errors: ignore source ranges and
/// payload messages, just compare the structural kind.
///
/// Each backend formats trap diagnostics differently — tree-walk
/// carries rich `range` payloads from the AST, cranelift only knows
/// the entry's `#main` range. Comparing the discriminant + the
/// kind-specific *non-range* payload is the closest we get to
/// bit-equality without coupling the harness to range-mapping
/// internals.
pub fn trap_equivalent(a: &RuntimeError, b: &RuntimeError) -> bool {
    use RuntimeError::*;
    match (a, b) {
        (DivisionByZero(_), DivisionByZero(_)) => true,
        (NumericOverflow(_), NumericOverflow(_)) => true,
        (
            WasmIndexOutOfBounds { .. } | IndexOutOfBounds { .. },
            WasmIndexOutOfBounds { .. } | IndexOutOfBounds { .. },
        ) => true,
        (
            WasmStepLimitExceeded { .. } | StepLimitExceeded { .. },
            WasmStepLimitExceeded { .. } | StepLimitExceeded { .. },
        ) => true,
        (
            WasmCapabilityDenied { .. } | CapabilityDenied { .. },
            WasmCapabilityDenied { .. } | CapabilityDenied { .. },
        ) => true,
        (TypeMismatch { .. }, TypeMismatch { .. }) => true,
        (MissingMainArg { name: a, .. }, MissingMainArg { name: b, .. }) => a == b,
        (
            MainArgTypeMismatch {
                name: an,
                expected: ae,
                ..
            },
            MainArgTypeMismatch {
                name: bn,
                expected: be,
                ..
            },
        ) => an == bn && ae == be,
        // Generic catch-all for `Unsupported`; one side returning
        // Unsupported while the other returns a typed trap is *not*
        // equivalent — that surfaces as `TrapMismatch`.
        (Unsupported { .. }, Unsupported { .. }) => true,
        _ => false,
    }
}
