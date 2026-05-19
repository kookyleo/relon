//! Op → TraceOp lowering decision table.
//!
//! Every [`relon_ir::Op`] variant is funnelled through [`lower_op`],
//! which returns a [`LowerOutcome`] the recorder turns into a
//! `TraceBuffer` append + optional `Guard` emission. The function is
//! intentionally pure (no recorder state) so unit tests can sweep the
//! op spectrum without spinning up a full recorder.
//!
//! Coverage matches the v6-γ design doc §1.4 hot-path subset:
//! arithmetic, comparisons, simple control flow, `LoadField` /
//! `StoreField`, plus stdlib `Op::Call`. Everything else surfaces as
//! [`crate::AbortReason::UnsupportedOp`] — the cranelift-generic
//! backend keeps handling those ops on the cold path until a later
//! v6-γ phase teaches the recorder how to trace through them.

use ordered_float::OrderedFloat;
use relon_ir::{EffectClass as IrEffect, IrType, Op};
use relon_trace_jit::{
    CmpKind, EffectClass as TraceEffect, FuncId, GuardKind, ObservedType, Offset, SsaVar, TraceOp,
};

use crate::abort::AbortReason;

/// What the lowering pass wants the recorder to do for a given op.
///
/// The recorder is responsible for appending the [`TraceOp`] to its
/// [`relon_trace_jit::TraceBuffer`] and for emitting any
/// follow-up guard the variant requires. Keeping the decision data
/// in a plain enum lets us unit-test each lowering rule without
/// touching a real buffer.
#[derive(Debug, Clone)]
pub enum LowerOutcome {
    /// Append `op` to the buffer; record the produced SSA value as
    /// `dst`. Optional `guards_before` / `guards_after` lists allow
    /// the rule to insert guards around the main op (e.g.
    /// `BoundsCheck` before a `Load`, `ArithOverflow` after a `Div`).
    Emit {
        op: TraceOp,
        dst: Option<SsaVar>,
        guards_before: Vec<GuardKind>,
        guards_after: Vec<GuardKind>,
        /// Side-effect class to record on the buffer. Mirrors the
        /// op's `effect_class()` so the optimiser pipeline can apply
        /// reorder-barrier rules consistently.
        effect: TraceEffect,
    },
    /// The op manipulates the recorder's local state but does not
    /// produce a `TraceOp`. `LocalSet` / `LetSet` / `Block` boundaries
    /// fall into this bucket.
    SideEffectOnly {
        /// Optional SSA value to bind into the recorder's
        /// `ir_to_ssa` table — e.g. for `LocalSet { idx }` the recorder
        /// re-binds the local-slot key. Carried out-of-band because
        /// the recorder owns the slot map.
        rebind: Option<SsaVar>,
    },
    /// The op recovers a previously bound SSA value (`LocalGet` /
    /// `LetGet`). The recorder uses `lookup_key` to fetch the
    /// existing SSA from its `ir_to_ssa` table; if the key is
    /// missing it allocates a fresh SSA and seeds the table.
    Lookup {
        kind: LookupKind,
        /// Static `ObservedType` hint the recorder uses when allocating
        /// a fresh SSA — comes from the op's `IrType` tag.
        ty_hint: ObservedType,
    },
    /// Trace-terminal op (`Return`). The recorder appends the op and
    /// flips its `aborted = None, terminated = true` flag so callers
    /// can finalize the buffer.
    Terminate { op: TraceOp },
    /// The op tells the recorder a loop boundary has been crossed
    /// (`Block` start, `Loop` start, branch label). Carries the
    /// marker op to emit verbatim.
    LoopMarker { op: TraceOp },
    /// The op cannot be traced; emit an abort with the carried reason.
    Abort(AbortReason),
}

/// Source-side key the recorder looks up in its `ir_to_ssa` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LookupKind {
    /// Wasm-handshake `LocalGet(idx)`.
    Local(u32),
    /// Per-function `LetGet { idx }`.
    Let(u32),
}

/// Context an op brings to the lowering function. The recorder fills
/// these out from its own bookkeeping (SSA allocator + observed-type
/// snapshot) so the rule logic stays a pure function.
#[derive(Debug, Clone)]
pub struct OpLoweringContext<'a> {
    /// SSA ids of the values this op consumes from the operand stack,
    /// in push order (innermost first).
    pub inputs: &'a [SsaVar],
    /// Fresh SSA id the lowering rule may use for the op's output.
    /// The recorder pre-allocates this before calling [`lower_op`] so
    /// the pure rule does not need an allocator handle.
    pub fresh_dst: SsaVar,
    /// Override effect class for `Op::Call` when the host has proved
    /// the callee is `Pure` / `ReadOnly` / `RecoverableWrite`. None
    /// means use the IR's conservative classification.
    pub call_effect_override: Option<TraceEffect>,
    /// Hint for the branch direction the recorder is following. We
    /// always record the taken path; the rule uses this to pick the
    /// right `GuardKind::BranchTaken`-style emit.
    pub branch_taken: Option<bool>,
}

impl<'a> OpLoweringContext<'a> {
    pub fn new(inputs: &'a [SsaVar], fresh_dst: SsaVar) -> Self {
        Self {
            inputs,
            fresh_dst,
            call_effect_override: None,
            branch_taken: None,
        }
    }

    pub fn with_call_effect_override(mut self, eff: TraceEffect) -> Self {
        self.call_effect_override = Some(eff);
        self
    }

    pub fn with_branch_taken(mut self, taken: bool) -> Self {
        self.branch_taken = Some(taken);
        self
    }
}

/// Translate `relon_ir::EffectClass` to the shared trace ABI variant.
///
/// v6-γ M1 keeps these as **two separate enums** by design:
///
/// - `relon_ir::EffectClass` is the IR-level Op effect annotation. Its
///   variant `UnrecoverableEffect` uses the longer historical name so
///   v5-β-2 doc strings + golden tests keep round-tripping.
/// - `relon_trace_abi::EffectClass` (re-exported through
///   `relon_trace_jit::EffectClass` aka [`TraceEffect`] here) is the
///   runtime classification the recorder / emitter share. Variant
///   `Unrecoverable` lines up byte-for-byte with `Pure`/`ReadOnly`/
///   `RecoverableWrite` on the IR side, so the mapping is total.
///
/// We use a plain function rather than `From<IrEffect> for TraceEffect`
/// because both types are foreign to this crate (orphan rules). Adding
/// the `From` impl would require either `relon-ir -> relon-trace-abi`
/// or `relon-trace-abi -> relon-ir` — both edges leak concerns across
/// layers v6-γ wants to keep separated, so the explicit translation
/// stays here in the recorder's lowering crate where it belongs.
pub fn map_effect_class(ir: IrEffect) -> TraceEffect {
    match ir {
        IrEffect::Pure => TraceEffect::Pure,
        IrEffect::ReadOnly => TraceEffect::ReadOnly,
        IrEffect::RecoverableWrite => TraceEffect::RecoverableWrite,
        IrEffect::UnrecoverableEffect => TraceEffect::Unrecoverable,
    }
}

/// Project an `IrType` onto the observed-type grid used in the trace
/// IR. Mirror of `crate::type_obs::observed_type_from_ir_type` so the
/// lowering rule can be pure (no cross-module call).
fn ty_to_observed(ty: IrType) -> ObservedType {
    match ty {
        IrType::I32 => ObservedType::I32,
        IrType::I64 => ObservedType::I64,
        IrType::F64 => ObservedType::F64,
        IrType::Bool => ObservedType::Bool,
        _ => ObservedType::Ptr,
    }
}

/// Core lowering rule.
///
/// The function is exhaustive across `Op` for the v6-γ Phase-1 hot
/// subset; anything outside it returns
/// `LowerOutcome::Abort(AbortReason::UnsupportedOp(name))`. The
/// caller is expected to feed the recorder one op at a time —
/// nested-body ops (`Op::If`, `Op::Block`, `Op::Loop`) are reported
/// via `LoopMarker` / `SideEffectOnly` and the recorder recurses
/// into the body itself.
pub fn lower_op(op: &Op, cx: OpLoweringContext<'_>) -> LowerOutcome {
    match op {
        // ---- Const ops ---------------------------------------------------
        Op::ConstI32(v) => LowerOutcome::Emit {
            op: TraceOp::ConstI32(cx.fresh_dst, *v),
            dst: Some(cx.fresh_dst),
            guards_before: vec![],
            guards_after: vec![],
            effect: TraceEffect::Pure,
        },
        Op::ConstI64(v) => LowerOutcome::Emit {
            op: TraceOp::ConstI64(cx.fresh_dst, *v),
            dst: Some(cx.fresh_dst),
            guards_before: vec![],
            guards_after: vec![],
            effect: TraceEffect::Pure,
        },
        Op::ConstBool(v) => LowerOutcome::Emit {
            // Booleans pack into the i32 slot at codegen time; mirror
            // that here so optimiser passes see a consistent
            // representation.
            op: TraceOp::ConstI32(cx.fresh_dst, if *v { 1 } else { 0 }),
            dst: Some(cx.fresh_dst),
            guards_before: vec![],
            guards_after: vec![],
            effect: TraceEffect::Pure,
        },
        // F64 const is conservatively unsupported in the Phase-1
        // op set — the trace IR's ConstI32/ConstI64 variants do not
        // cover floats and the optimiser pipeline never folds them.
        // Future phases will add a TraceOp::ConstF64; until then we
        // abort cleanly rather than smuggle a float through an i64
        // slot.
        Op::ConstF64(OrderedFloat(_)) => {
            LowerOutcome::Abort(AbortReason::UnsupportedOp("ConstF64"))
        }

        // ---- Arithmetic / comparison ------------------------------------
        Op::Add(ty) => binary_arith(cx, *ty, BinaryArith::Add),
        Op::Sub(ty) => binary_arith(cx, *ty, BinaryArith::Sub),
        Op::Mul(ty) => binary_arith(cx, *ty, BinaryArith::Mul),
        Op::Div(ty) => binary_arith(cx, *ty, BinaryArith::Div),

        Op::Eq(ty) => binary_cmp(cx, *ty, CmpKind::Eq),
        Op::Ne(ty) => binary_cmp(cx, *ty, CmpKind::Ne),
        Op::Lt(ty) => binary_cmp(cx, *ty, CmpKind::Lt),
        Op::Le(ty) => binary_cmp(cx, *ty, CmpKind::Le),
        Op::Gt(ty) => binary_cmp(cx, *ty, CmpKind::Gt),
        Op::Ge(ty) => binary_cmp(cx, *ty, CmpKind::Ge),

        // Mod / BitAnd are pure but the trace IR has no matching op
        // today. Surface as UnsupportedOp so the abort path stays
        // honest — a future phase adds the variants.
        Op::Mod(_) => LowerOutcome::Abort(AbortReason::UnsupportedOp("Mod")),
        Op::BitAnd(_) => LowerOutcome::Abort(AbortReason::UnsupportedOp("BitAnd")),

        // ---- Local / let -------------------------------------------------
        Op::LocalGet(idx) => LowerOutcome::Lookup {
            kind: LookupKind::Local(*idx),
            // Local handshake slots are i32 today (the four wasm-
            // handshake params). Sticking with I32 as the conservative
            // observed type means TypeCheck guards bail out cleanly if
            // a future phase promotes one to i64.
            ty_hint: ObservedType::I32,
        },
        Op::LetGet { idx, ty } => LowerOutcome::Lookup {
            kind: LookupKind::Let(*idx),
            ty_hint: ty_to_observed(*ty),
        },
        Op::LetSet { .. } => LowerOutcome::SideEffectOnly {
            // The recorder grabs the value off its operand stack and
            // re-binds the let-slot. Caller threads the SSA in via
            // its own `inputs` window — see recorder.rs.
            rebind: cx.inputs.first().copied(),
        },

        // ---- Field load / store -----------------------------------------
        Op::LoadField { offset, ty } => {
            let base = cx
                .inputs
                .first()
                .copied()
                // No-input loads use SsaVar::NONE as a sentinel base;
                // the codegen pass will compute the actual base from
                // `$in_ptr` at emit time.
                .unwrap_or(SsaVar::NONE);
            LowerOutcome::Emit {
                op: TraceOp::Load(cx.fresh_dst, base, Offset(*offset as i32)),
                dst: Some(cx.fresh_dst),
                guards_before: vec![GuardKind::BoundsCheck(base, base)],
                guards_after: vec![GuardKind::TypeCheck(cx.fresh_dst, ty_to_observed(*ty))],
                effect: TraceEffect::ReadOnly,
            }
        }
        Op::StoreField { offset, .. } => {
            let value = cx.inputs.first().copied().unwrap_or(SsaVar::NONE);
            let base = cx.inputs.get(1).copied().unwrap_or(SsaVar::NONE);
            LowerOutcome::Emit {
                op: TraceOp::Store(base, Offset(*offset as i32), value),
                dst: None,
                guards_before: vec![GuardKind::BoundsCheck(base, base)],
                guards_after: vec![],
                effect: TraceEffect::RecoverableWrite,
            }
        }

        // ---- Control flow -----------------------------------------------
        Op::Br { .. } => LowerOutcome::SideEffectOnly { rebind: None },
        Op::BrIf { .. } => {
            // Recorder follows the taken arm. If the caller did not
            // provide the direction, fall back to "true" — the
            // recorded trace will only contain ops we actually saw
            // execute.
            let var = cx.inputs.first().copied().unwrap_or(SsaVar::NONE);
            LowerOutcome::Emit {
                // We model BrIf as an inline guard so the optimised
                // trace deopts if a future execution would have taken
                // the *other* arm.
                op: TraceOp::Guard(GuardKind::NotNull(var), var),
                dst: None,
                guards_before: vec![],
                guards_after: vec![],
                effect: TraceEffect::Pure,
            }
        }
        Op::BrTable { .. } => LowerOutcome::Abort(AbortReason::UnsupportedOp("BrTable")),
        Op::Block { .. } => LowerOutcome::SideEffectOnly { rebind: None },
        Op::Loop { .. } => LowerOutcome::LoopMarker {
            // Reuse the input SSA window's first slot as a loop id —
            // the recorder allocates a fresh marker id and rewrites
            // this before appending. Using SsaVar::NONE as a sentinel
            // is fine because MarkLoopHead carries the id inline.
            //
            // ε-M0: the φ list is empty here because this lowering
            // rule fires from the per-op path that has no view of
            // loop-carried let-slots; the recorder's higher-level
            // [`crate::record_loop`] entry point is what builds the
            // full [`TraceOp::MarkLoopHead`] / [`TraceOp::MarkLoopBack`]
            // pair with real φ pairs.
            op: TraceOp::MarkLoopHead {
                loop_id: 0,
                phis: vec![],
            },
        },
        Op::If { .. } => LowerOutcome::Abort(AbortReason::UnsupportedOp("If")),

        // ---- Calls -------------------------------------------------------
        Op::Call { fn_index, .. } => {
            // The IR conservatively classifies every stdlib `Op::Call`
            // as `UnrecoverableEffect`; the recorder accepts an
            // override via `OpLoweringContext` for callees the host
            // has proved are safe to trace. Without an override we
            // mirror the IR's decision and abort.
            let effect = cx
                .call_effect_override
                .unwrap_or_else(|| map_effect_class(op.effect_class()));
            if matches!(effect, TraceEffect::Unrecoverable) {
                LowerOutcome::Abort(AbortReason::UnrecoverableEffect)
            } else {
                LowerOutcome::Emit {
                    op: TraceOp::Call(cx.fresh_dst, FuncId(*fn_index), cx.inputs.to_vec(), effect),
                    dst: Some(cx.fresh_dst),
                    guards_before: vec![],
                    guards_after: vec![],
                    effect,
                }
            }
        }
        Op::CallNative { .. } => LowerOutcome::Abort(AbortReason::UnrecoverableEffect),
        Op::CallClosure { .. } => LowerOutcome::Abort(AbortReason::UnrecoverableEffect),

        // ---- Terminators ------------------------------------------------
        Op::Return => {
            let v = cx.inputs.first().copied().unwrap_or(SsaVar::NONE);
            LowerOutcome::Terminate {
                op: TraceOp::Return(v),
            }
        }
        Op::Trap { .. } => LowerOutcome::Abort(AbortReason::UnsupportedOp("Trap")),

        // ---- Everything else --------------------------------------------
        Op::Select { .. } => LowerOutcome::Abort(AbortReason::UnsupportedOp("Select")),
        Op::CheckCap { .. } => LowerOutcome::SideEffectOnly { rebind: None },
        Op::ReadStringLen => LowerOutcome::Abort(AbortReason::UnsupportedOp("ReadStringLen")),

        // Catch-all for the long tail of IR ops (string constants,
        // list constants, schema constructors, allocator ops, table
        // address constants, etc.). Each one is a future trace-jit
        // phase decision; until that lands we abort with the variant
        // name so logs make the gap obvious.
        other => LowerOutcome::Abort(AbortReason::UnsupportedOp(unsupported_op_name(other))),
    }
}

/// Stable static name for `UnsupportedOp` diagnostics. Centralised so
/// the catch-all in `lower_op` doesn't grow a giant match per call.
fn unsupported_op_name(op: &Op) -> &'static str {
    match op {
        Op::ConstString { .. } => "ConstString",
        Op::ConstListInt { .. } => "ConstListInt",
        Op::ConstListFloat { .. } => "ConstListFloat",
        Op::ConstListBool { .. } => "ConstListBool",
        Op::ConstListString { .. } => "ConstListString",
        Op::LoadStringPtr { .. } => "LoadStringPtr",
        Op::LoadListIntPtr { .. } => "LoadListIntPtr",
        Op::LoadListFloatPtr { .. } => "LoadListFloatPtr",
        Op::LoadListBoolPtr { .. } => "LoadListBoolPtr",
        Op::LoadListStringPtr { .. } => "LoadListStringPtr",
        Op::LoadListSchemaPtr { .. } => "LoadListSchemaPtr",
        Op::LoadFieldAtAbsolute { .. } => "LoadFieldAtAbsolute",
        Op::LoadSchemaPtr { .. } => "LoadSchemaPtr",
        Op::AllocRootRecord { .. } => "AllocRootRecord",
        Op::AllocSubRecord { .. } => "AllocSubRecord",
        Op::AllocScratch { .. } => "AllocScratch",
        Op::AllocScratchDyn => "AllocScratchDyn",
        Op::StoreFieldAtRecord { .. } => "StoreFieldAtRecord",
        Op::PushRecordBase { .. } => "PushRecordBase",
        Op::EmitTailRecordFromAbsoluteAddr { .. } => "EmitTailRecordFromAbsoluteAddr",
        Op::MakeClosure { .. } => "MakeClosure",
        Op::MemcpyAtAbsolute => "MemcpyAtAbsolute",
        Op::CaseFoldTableAddr { .. } => "CaseFoldTableAddr",
        Op::CombiningMarkRangesAddr => "CombiningMarkRangesAddr",
        Op::WhitespaceRangesAddr => "WhitespaceRangesAddr",
        Op::DecompTableAddr { .. } => "DecompTableAddr",
        Op::CccTableAddr => "CccTableAddr",
        Op::CompositionTableAddr => "CompositionTableAddr",
        Op::FullCaseFoldTableAddr { .. } => "FullCaseFoldTableAddr",
        Op::CasedRangesAddr => "CasedRangesAddr",
        Op::CaseIgnorableRangesAddr => "CaseIgnorableRangesAddr",
        Op::TurkishCaseFoldTableAddr { .. } => "TurkishCaseFoldTableAddr",
        _ => "Other",
    }
}

enum BinaryArith {
    Add,
    Sub,
    Mul,
    Div,
}

fn binary_arith(cx: OpLoweringContext<'_>, ty: IrType, kind: BinaryArith) -> LowerOutcome {
    // F64 arithmetic is not part of the Phase-1 trace op set — the
    // trace IR's Add/Sub/Mul/Div are integer-only. Surface as an
    // UnsupportedOp until a typed variant lands.
    if matches!(ty, IrType::F64) {
        return LowerOutcome::Abort(AbortReason::UnsupportedOp("FloatArith"));
    }
    if cx.inputs.len() < 2 {
        return LowerOutcome::Abort(AbortReason::UnsupportedOp("ArithUnderflow"));
    }
    let rhs = cx.inputs[0];
    let lhs = cx.inputs[1];
    let (op, effect, guard_after) = match kind {
        BinaryArith::Add => (
            TraceOp::Add(cx.fresh_dst, lhs, rhs),
            TraceEffect::Pure,
            Some(GuardKind::ArithOverflow(cx.fresh_dst)),
        ),
        BinaryArith::Sub => (
            TraceOp::Sub(cx.fresh_dst, lhs, rhs),
            TraceEffect::Pure,
            Some(GuardKind::ArithOverflow(cx.fresh_dst)),
        ),
        BinaryArith::Mul => (
            TraceOp::Mul(cx.fresh_dst, lhs, rhs),
            TraceEffect::Pure,
            Some(GuardKind::ArithOverflow(cx.fresh_dst)),
        ),
        BinaryArith::Div => (
            TraceOp::Div(cx.fresh_dst, lhs, rhs),
            // Div is RecoverableWrite at the trace-IR level — the
            // optimiser captures the dividend pre-value so a
            // div-by-zero deopt can re-execute the divisor.
            TraceEffect::RecoverableWrite,
            Some(GuardKind::ArithOverflow(cx.fresh_dst)),
        ),
    };
    LowerOutcome::Emit {
        op,
        dst: Some(cx.fresh_dst),
        guards_before: vec![],
        guards_after: guard_after.into_iter().collect(),
        effect,
    }
}

fn binary_cmp(cx: OpLoweringContext<'_>, _ty: IrType, kind: CmpKind) -> LowerOutcome {
    if cx.inputs.len() < 2 {
        return LowerOutcome::Abort(AbortReason::UnsupportedOp("CmpUnderflow"));
    }
    let rhs = cx.inputs[0];
    let lhs = cx.inputs[1];
    LowerOutcome::Emit {
        op: TraceOp::Cmp(kind, cx.fresh_dst, lhs, rhs),
        dst: Some(cx.fresh_dst),
        guards_before: vec![],
        guards_after: vec![],
        effect: TraceEffect::Pure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ordered_float::OrderedFloat;

    fn cx_with(inputs: &[SsaVar], dst: SsaVar) -> OpLoweringContext<'_> {
        OpLoweringContext::new(inputs, dst)
    }

    #[test]
    fn const_i64_emits_consti64() {
        let dst = SsaVar(7);
        let outcome = lower_op(&Op::ConstI64(42), cx_with(&[], dst));
        match outcome {
            LowerOutcome::Emit {
                op, dst: Some(d), ..
            } => {
                assert_eq!(d, dst);
                assert!(matches!(op, TraceOp::ConstI64(_, 42)));
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn const_bool_packs_as_i32() {
        let dst = SsaVar(1);
        let LowerOutcome::Emit { op, .. } = lower_op(&Op::ConstBool(true), cx_with(&[], dst))
        else {
            panic!()
        };
        assert!(matches!(op, TraceOp::ConstI32(_, 1)));
    }

    #[test]
    fn const_f64_aborts_unsupported() {
        let outcome = lower_op(&Op::ConstF64(OrderedFloat(1.0)), cx_with(&[], SsaVar(0)));
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("ConstF64"))
        ));
    }

    #[test]
    fn add_i64_emits_add_with_overflow_guard() {
        let inputs = [SsaVar(1), SsaVar(2)];
        let outcome = lower_op(&Op::Add(IrType::I64), cx_with(&inputs, SsaVar(3)));
        match outcome {
            LowerOutcome::Emit {
                op,
                dst,
                guards_after,
                effect,
                ..
            } => {
                assert!(matches!(op, TraceOp::Add(_, _, _)));
                assert_eq!(dst, Some(SsaVar(3)));
                assert_eq!(effect, TraceEffect::Pure);
                assert!(matches!(
                    guards_after.as_slice(),
                    [GuardKind::ArithOverflow(_)]
                ));
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn div_is_recoverable_write() {
        let inputs = [SsaVar(1), SsaVar(2)];
        let LowerOutcome::Emit { effect, .. } =
            lower_op(&Op::Div(IrType::I64), cx_with(&inputs, SsaVar(3)))
        else {
            panic!()
        };
        assert_eq!(effect, TraceEffect::RecoverableWrite);
    }

    #[test]
    fn float_arith_aborts() {
        let inputs = [SsaVar(1), SsaVar(2)];
        let outcome = lower_op(&Op::Add(IrType::F64), cx_with(&inputs, SsaVar(3)));
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("FloatArith"))
        ));
    }

    #[test]
    fn cmp_lowers_to_cmp_op() {
        let inputs = [SsaVar(1), SsaVar(2)];
        let outcome = lower_op(&Op::Lt(IrType::I64), cx_with(&inputs, SsaVar(3)));
        match outcome {
            LowerOutcome::Emit { op, .. } => {
                assert!(matches!(op, TraceOp::Cmp(CmpKind::Lt, _, _, _)));
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn local_get_is_lookup() {
        let outcome = lower_op(&Op::LocalGet(2), cx_with(&[], SsaVar(0)));
        assert!(matches!(
            outcome,
            LowerOutcome::Lookup {
                kind: LookupKind::Local(2),
                ..
            }
        ));
    }

    #[test]
    fn let_get_carries_ty_hint() {
        let outcome = lower_op(
            &Op::LetGet {
                idx: 0,
                ty: IrType::I64,
            },
            cx_with(&[], SsaVar(0)),
        );
        match outcome {
            LowerOutcome::Lookup { kind, ty_hint } => {
                assert_eq!(kind, LookupKind::Let(0));
                assert_eq!(ty_hint, ObservedType::I64);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn return_is_terminate() {
        let outcome = lower_op(&Op::Return, cx_with(&[SsaVar(9)], SsaVar(0)));
        assert!(matches!(outcome, LowerOutcome::Terminate { .. }));
    }

    #[test]
    fn call_with_override_emits_call() {
        let outcome = lower_op(
            &Op::Call {
                fn_index: 5,
                arg_count: 2,
                param_tys: vec![IrType::I64, IrType::I64],
                ret_ty: IrType::I64,
            },
            cx_with(&[SsaVar(1), SsaVar(2)], SsaVar(3))
                .with_call_effect_override(TraceEffect::Pure),
        );
        match outcome {
            LowerOutcome::Emit { op, effect, .. } => {
                assert!(matches!(op, TraceOp::Call(_, FuncId(5), _, _)));
                assert_eq!(effect, TraceEffect::Pure);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn call_without_override_aborts() {
        let outcome = lower_op(
            &Op::Call {
                fn_index: 5,
                arg_count: 0,
                param_tys: vec![],
                ret_ty: IrType::I64,
            },
            cx_with(&[], SsaVar(0)),
        );
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnrecoverableEffect)
        ));
    }

    #[test]
    fn map_effect_class_round_trip() {
        assert_eq!(map_effect_class(IrEffect::Pure), TraceEffect::Pure);
        assert_eq!(map_effect_class(IrEffect::ReadOnly), TraceEffect::ReadOnly);
        assert_eq!(
            map_effect_class(IrEffect::RecoverableWrite),
            TraceEffect::RecoverableWrite
        );
        assert_eq!(
            map_effect_class(IrEffect::UnrecoverableEffect),
            TraceEffect::Unrecoverable
        );
    }
}
