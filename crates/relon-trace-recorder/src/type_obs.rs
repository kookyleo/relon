//! Observed-type inference + the recorder's TypeCheck guard policy.
//!
//! Two responsibilities:
//!
//! 1. Translate a runtime [`relon_eval_api::Value`] (the cranelift-
//!    generic backend's tagged value form) into the optimiser-friendly
//!    [`relon_trace_jit::ObservedType`] enum the trace IR uses.
//! 2. Provide a small policy helper that turns the "first time this
//!    var has been observed" / "type mismatch on re-observation"
//!    distinction into a [`TypeObsDecision`] the recorder can act on.
//!
//! We deliberately re-export `ObservedType` from `relon-trace-jit` —
//! defining a parallel enum here would double the maintenance burden
//! whenever the trace IR's type spec changes.

use ordered_float::OrderedFloat;
use relon_eval_api::Value;
use relon_ir::IrType;

pub use relon_trace_jit::ObservedType;

/// Map a runtime [`Value`] tag onto the optimiser's coarse
/// [`ObservedType`] grid.
///
/// The trace IR keeps only five buckets (I32 / I64 / F64 / Bool / Ptr)
/// because that's what the cranelift-generic backend's value slots
/// distinguish; we collapse every Relon reference shape (`String` /
/// `List` / `Dict` / `Closure` / `Schema` / `EnumSchema` / `Type` /
/// `Wildcard` / `Null`) onto `Ptr`. `Null` is intentionally a
/// pointer-tag rather than its own variant — the cranelift slot is
/// still i32 with the tag bit set, and we never want a `Pure`
/// constant-folding pass to confuse a literal `0` with a null tag.
pub fn infer_observed_type(value: &Value) -> ObservedType {
    match value {
        Value::Bool(_) => ObservedType::Bool,
        Value::Int(_) => ObservedType::I64,
        Value::Float(_) => ObservedType::F64,
        // Every reference-shape value lives in an i32 slot at the
        // cranelift level. The optimiser only cares about "equal to
        // the previously observed tag" so collapsing into `Ptr` is
        // both correct and minimal.
        Value::Null
        | Value::String(_)
        | Value::List(_)
        | Value::Dict(_)
        | Value::Closure(_)
        | Value::Schema(_)
        | Value::EnumSchema(_)
        | Value::Type(_)
        | Value::Wildcard => ObservedType::Ptr,
    }
}

/// Same inference but driven by the static [`IrType`] tag carried on
/// many `Op` variants (e.g. `Op::Add(IrType::I64)`). Used by the
/// recorder when no runtime [`Value`] is available — for instance
/// when projecting constant ops where the type is decided by the op
/// itself.
pub fn observed_type_from_ir_type(ty: IrType) -> ObservedType {
    match ty {
        IrType::I32 => ObservedType::I32,
        IrType::I64 => ObservedType::I64,
        IrType::F64 => ObservedType::F64,
        IrType::Bool => ObservedType::Bool,
        IrType::Null
        | IrType::String
        | IrType::ListInt
        | IrType::ListFloat
        | IrType::ListBool
        | IrType::ListString
        | IrType::ListSchema
        | IrType::Closure => ObservedType::Ptr,
    }
}

/// Outcome of a single observation. Wraps the three signals the
/// recorder needs to drive its TypeCheck-guard emission policy:
///
/// * `FirstSeen` — the recorder has not observed this var yet, so it
///   should remember the type for future comparisons. No guard is
///   emitted yet.
/// * `EmitGuard` — the recorder has seen this var with the *same*
///   type before; emit a `TypeCheck` guard so the optimised trace
///   bails out if a future execution sees a different type. The pass
///   pipeline (LICM + dead-store) cleans up duplicates.
/// * `Mismatch` — the recorder has previously seen this var with a
///   *different* type. The recorder must abort with
///   `AbortReason::GuardFailureInRecording` because no single guard
///   can validate both observations within the same trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeObsDecision {
    FirstSeen,
    EmitGuard,
    Mismatch { previous: ObservedType },
}

/// Tiny helper that drives [`TypeObsDecision`] from the recorder's
/// type-observation map. Pulled out as a free function so unit tests
/// can exercise the policy without spinning up a full recorder.
pub fn classify_observation(
    previous: Option<ObservedType>,
    observed: ObservedType,
) -> TypeObsDecision {
    match previous {
        None => TypeObsDecision::FirstSeen,
        Some(prev) if prev == observed => TypeObsDecision::EmitGuard,
        Some(prev) => TypeObsDecision::Mismatch { previous: prev },
    }
}

/// Compute the bitwise representation a float observation surfaces
/// when packed into a deopt restore slot. Kept here to avoid a stray
/// `f64::to_bits` line in the recorder; the abstraction also lets a
/// future replay layer choose a different mantissa encoding without
/// touching the recorder body.
pub fn float_to_restore_bits(v: OrderedFloat<f64>) -> u64 {
    v.into_inner().to_bits()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn infer_observed_type_scalars() {
        assert_eq!(infer_observed_type(&Value::Bool(true)), ObservedType::Bool);
        assert_eq!(infer_observed_type(&Value::Int(42)), ObservedType::I64);
        assert_eq!(
            infer_observed_type(&Value::Float(OrderedFloat(1.5))),
            ObservedType::F64
        );
        assert_eq!(infer_observed_type(&Value::Null), ObservedType::Ptr);
    }

    #[test]
    fn infer_observed_type_reference_shapes_are_ptr() {
        assert_eq!(
            infer_observed_type(&Value::String("hi".into())),
            ObservedType::Ptr
        );
        assert_eq!(infer_observed_type(&Value::list(vec![])), ObservedType::Ptr);
        assert_eq!(
            infer_observed_type(&Value::dict(BTreeMap::new())),
            ObservedType::Ptr
        );
        assert_eq!(infer_observed_type(&Value::Wildcard), ObservedType::Ptr);
    }

    #[test]
    fn ir_type_inference_matches_value_inference_for_scalars() {
        assert_eq!(observed_type_from_ir_type(IrType::I32), ObservedType::I32);
        assert_eq!(observed_type_from_ir_type(IrType::I64), ObservedType::I64);
        assert_eq!(observed_type_from_ir_type(IrType::F64), ObservedType::F64);
        assert_eq!(observed_type_from_ir_type(IrType::Bool), ObservedType::Bool);
        assert_eq!(
            observed_type_from_ir_type(IrType::String),
            ObservedType::Ptr
        );
        assert_eq!(
            observed_type_from_ir_type(IrType::Closure),
            ObservedType::Ptr
        );
    }

    #[test]
    fn classify_first_then_emit_then_mismatch() {
        assert_eq!(
            classify_observation(None, ObservedType::I64),
            TypeObsDecision::FirstSeen
        );
        assert_eq!(
            classify_observation(Some(ObservedType::I64), ObservedType::I64),
            TypeObsDecision::EmitGuard
        );
        assert_eq!(
            classify_observation(Some(ObservedType::I64), ObservedType::F64),
            TypeObsDecision::Mismatch {
                previous: ObservedType::I64
            }
        );
    }

    #[test]
    fn float_bits_round_trip() {
        let v = OrderedFloat(2.5_f64);
        let bits = float_to_restore_bits(v);
        assert_eq!(f64::from_bits(bits), 2.5);
    }
}
