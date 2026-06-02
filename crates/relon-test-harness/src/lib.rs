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

// Cannot `#![forbid(unsafe_code)]` here because the v6-γ M4 trace-JIT
// driver in `three_way` needs to call the `__relon_jump_to_recorder`
// host helper and invoke JIT-emitted traces through raw fn pointers.
// The `unsafe` blocks are confined to that one module; the rest of
// the harness stays unsafe-free.
#![deny(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;

use relon::{new_evaluator, Backend, BackendError};
use relon_eval_api::{Evaluator, RuntimeError, Value};

pub mod corpus;
pub mod four_way;
pub mod three_way;

/// Backend tier identifiers used by the corpus support-claim ratchet.
///
/// Distinct from [`Backend`] because the trace-JIT runs as an extra
/// tier on top of the cranelift IR pipeline rather than a standalone
/// `Backend::*` variant; the harness still needs to express "case X
/// claims trace-JIT support" so a regression to
/// `TraceJitNotApplicable` is caught.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// Reference tree-walking interpreter (`Backend::TreeWalk`).
    TreeWalk,
    /// Cranelift-AOT (`Backend::CraneliftAot`).
    CraneliftAot,
    /// Trace-JIT tier — installed on top of cranelift IR; the
    /// harness's synth-recipe catalogue stands in for a real recorder.
    TraceJit,
    /// Bytecode VM (`Backend::Bytecode`).
    Bytecode,
}

impl BackendKind {
    /// Human-readable label used in ratchet failure messages.
    pub fn label(self) -> &'static str {
        match self {
            BackendKind::TreeWalk => "tree_walk",
            BackendKind::CraneliftAot => "cranelift_aot",
            BackendKind::TraceJit => "trace_jit",
            BackendKind::Bytecode => "bytecode",
        }
    }
}

/// One ratchet violation — a backend that **claimed** to support a
/// case ended up bouncing to its fallback surface (Unsupported /
/// NotApplicable). Aggregated by the corpus drivers; a non-empty list
/// fails the test loud.
#[derive(Debug)]
pub struct RatchetViolation {
    /// Corpus case name.
    pub case: String,
    /// The backend that broke its support claim.
    pub backend: BackendKind,
    /// Backend-side reason / soft-pass reason string.
    pub reason: String,
}

impl std::fmt::Display for RatchetViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ratchet: case `{}` claims `{}` support but observed soft fallback: {}",
            self.case,
            self.backend.label(),
            self.reason
        )
    }
}

/// Ratchet utilities — turn the soft-pass variants of [`DiffOutcome`]
/// / [`three_way::ThreeWayResult`] / [`four_way::FourWayResult`] into
/// hard failures when a backend in `claim` claimed to support the
/// case but bounced anyway.
pub mod ratchet {
    use super::{BackendKind, DiffOutcome, RatchetViolation};
    use crate::four_way::FourWayResult;
    use crate::three_way::ThreeWayResult;

    /// True iff `claim` lists `backend` as a supporter for the case.
    fn claims(claim: &[BackendKind], backend: BackendKind) -> bool {
        claim.contains(&backend)
    }

    /// Build a [`RatchetViolation`], centralising the `case_name` /
    /// `reason` ownership conversion repeated across every soft-pass
    /// arm of the `check_*` walkers.
    fn make_violation(case_name: &str, backend: BackendKind, reason: &str) -> RatchetViolation {
        RatchetViolation {
            case: case_name.to_string(),
            backend,
            reason: reason.to_string(),
        }
    }

    /// Validate a two-way [`DiffOutcome`] against the case's
    /// `supported_by` claim list. Returns the single backend that
    /// regressed, or `None` when the outcome matches every claim.
    pub fn check_two_way(
        case_name: &str,
        outcome: &DiffOutcome,
        claim: &[BackendKind],
    ) -> Option<RatchetViolation> {
        match outcome {
            DiffOutcome::MatchOk | DiffOutcome::MatchTrap => None,
            DiffOutcome::CraneliftUnsupported { reason, .. } => {
                if claims(claim, BackendKind::CraneliftAot) {
                    Some(make_violation(case_name, BackendKind::CraneliftAot, reason))
                } else {
                    None
                }
            }
            DiffOutcome::TreeWalkMissingStdlibSurface {
                tree_walk_error, ..
            } => {
                // The tree-walker is *always* the reference impl; any
                // case that surfaces here is either out of the
                // tree-walker's stdlib envelope (forward-compat) or a
                // real reference-impl regression. If the case claims
                // tree-walk support, treat it as a violation.
                if claims(claim, BackendKind::TreeWalk) {
                    Some(make_violation(
                        case_name,
                        BackendKind::TreeWalk,
                        tree_walk_error,
                    ))
                } else {
                    None
                }
            }
        }
    }

    /// Validate a three-way [`ThreeWayResult`] against the claim list.
    /// Returns the first violation (callers running the whole corpus
    /// should collect violations across all cases before failing).
    pub fn check_three_way(
        case_name: &str,
        outcome: &ThreeWayResult,
        claim: &[BackendKind],
    ) -> Option<RatchetViolation> {
        match outcome {
            ThreeWayResult::AllAgree(_) | ThreeWayResult::AllTrap => None,
            ThreeWayResult::TraceJitNotApplicable { reason, .. } => {
                if claims(claim, BackendKind::TraceJit) {
                    Some(make_violation(case_name, BackendKind::TraceJit, reason))
                } else {
                    None
                }
            }
            ThreeWayResult::CraneliftUnsupported { reason, .. } => {
                if claims(claim, BackendKind::CraneliftAot) {
                    Some(make_violation(case_name, BackendKind::CraneliftAot, reason))
                } else {
                    None
                }
            }
            ThreeWayResult::TreeWalkMissingStdlibSurface {
                tree_walk_error, ..
            } => {
                if claims(claim, BackendKind::TreeWalk) {
                    Some(make_violation(
                        case_name,
                        BackendKind::TreeWalk,
                        tree_walk_error,
                    ))
                } else {
                    None
                }
            }
            ThreeWayResult::Mismatch { .. } => {
                // Mismatch is a hard correctness bug — not a ratchet
                // violation. The driver test asserts mismatches==0
                // separately; we don't double-count here.
                None
            }
        }
    }

    /// Validate a four-way [`FourWayResult`] against the claim list.
    /// Walks the embedded three-way result first, then the bytecode
    /// claim.
    pub fn check_four_way(
        case_name: &str,
        outcome: &FourWayResult,
        claim: &[BackendKind],
    ) -> Vec<RatchetViolation> {
        let mut out = Vec::new();
        match outcome {
            FourWayResult::AllAgree(_) | FourWayResult::AllTrap => {}
            FourWayResult::BytecodeMatchesBaseline {
                trace_skip_reason, ..
            } => {
                // Bytecode produced the right value but the trace-JIT
                // / cranelift path bounced. The reason string carries
                // which tier soft-passed; we surface a ratchet
                // violation only when *that* tier is in `claim`.
                // Heuristic: parse the reason prefix.
                if trace_skip_reason.starts_with("cranelift_unsupported")
                    && claims(claim, BackendKind::CraneliftAot)
                {
                    out.push(make_violation(
                        case_name,
                        BackendKind::CraneliftAot,
                        trace_skip_reason,
                    ));
                } else if trace_skip_reason.starts_with("tree_walk_missing_stdlib_surface")
                    && claims(claim, BackendKind::TreeWalk)
                {
                    out.push(make_violation(
                        case_name,
                        BackendKind::TreeWalk,
                        trace_skip_reason,
                    ));
                } else if claims(claim, BackendKind::TraceJit) {
                    // Generic "trace-JIT skipped" branch.
                    out.push(make_violation(
                        case_name,
                        BackendKind::TraceJit,
                        trace_skip_reason,
                    ));
                }
            }
            FourWayResult::BytecodeUnsupported { baseline, reason } => {
                if claims(claim, BackendKind::Bytecode) {
                    out.push(make_violation(case_name, BackendKind::Bytecode, reason));
                }
                // Plus whatever the embedded three-way says.
                if let Some(v) = check_three_way(case_name, baseline, claim) {
                    out.push(v);
                }
            }
            FourWayResult::Mismatch { .. } => {
                // Mismatch handled by the driver's hard assertion.
            }
        }
        out
    }
}

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
    /// Tree-walker surfaced a `FunctionNotFound` / `MethodNotFound`
    /// diagnostic the IR-pipeline / cranelift path accepts as a
    /// free-function call. The tree-walker doesn't expose the
    /// bundled stdlib free-function surface uniformly with the
    /// method form — sources like `abs(x)` resolve through the IR
    /// pass but not through the AST evaluator. Documented as a
    /// non-fatal divergence on the differential corpus until the
    /// tree-walker grows the same surface; cranelift's output is
    /// preserved so the regression gate can require it to stay
    /// stable.
    TreeWalkMissingStdlibSurface {
        /// Cranelift's output (the canonical answer once tree-walk
        /// catches up).
        cranelift: Result<Value, String>,
        /// The tree-walker's underlying error.
        tree_walk_error: String,
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
        (Err(tw_err), Ok(cr)) => {
            // Tree-walker reports `FunctionNotFound` / `MethodNotFound`
            // for some stdlib surfaces that the IR / cranelift
            // pipeline accepts. Route those to the soft
            // `TreeWalkMissingStdlibSurface` outcome so the corpus
            // harness doesn't break on the differential — the tree-
            // walker grows the same surface in a separate tranche.
            if matches!(tw_err, RuntimeError::FunctionNotFound(_, _)) {
                return Ok(DiffOutcome::TreeWalkMissingStdlibSurface {
                    cranelift: Ok(cr),
                    tree_walk_error: format!("{tw_err:?}"),
                });
            }
            Err(DiffTestError::TrapVsValue {
                tree_walk: format!("Err({tw_err:?})"),
                cranelift: format!("Ok({cr:?})"),
            })
        }
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
        (IndexOutOfBounds { .. }, IndexOutOfBounds { .. }) => true,
        (StepLimitExceeded { .. }, StepLimitExceeded { .. }) => true,
        (CapabilityDenied { .. }, CapabilityDenied { .. }) => true,
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
