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

/// F-D8-B: outcome variant the recorder uses to dispatch onto its
/// dedicated [`crate::RecorderState::emit_list_get`] /
/// [`crate::RecorderState::emit_dict_lookup`] entry points.
///
/// Both rely on emit-time state the pure `lower_op` rule does not have
/// (allocator + buffer + IC fingerprint), so we surface the lookup as a
/// `SubscriptDispatch` outcome instead of an `Emit` carrying a
/// `TraceOp` — the recorder's `apply_outcome` arm then calls the right
/// helper.
#[derive(Debug, Clone, Copy)]
pub enum SubscriptKind {
    /// Lower onto `RecorderState::emit_list_get`. `inputs[1]` carries
    /// the list pointer SSA, `inputs[0]` carries the i64 index SSA.
    ListGet,
    /// Lower onto `RecorderState::emit_dict_lookup`. `inputs[1]` carries
    /// the dict pointer SSA, `inputs[0]` carries the key pointer SSA.
    /// `shape_hash` flows verbatim into the resulting `TraceOp::DictLookup`.
    ///
    /// F-D8-E.7: `entry_count_hint` — when the IR-level
    /// `Op::DictGetByStringKey` carried a static `entry_count_hint`,
    /// forward it so the recorder can stash the value in the buffer's
    /// `dict_entry_count_hints` side table, keyed by the dict_ptr SSA.
    /// The active v2 helper path uses `record_len_hint`; entry-count
    /// hints are retained as advisory metadata for inline lowering.
    DictLookup {
        shape_hash: u64,
        entry_count_hint: Option<u32>,
        record_len_hint: Option<u32>,
    },
}

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
    /// F-D8-B: dict / list subscript dispatch. The recorder calls the
    /// matching `emit_list_get` / `emit_dict_lookup` helper, which
    /// allocates the fresh dst SSA, appends the bounds / IC guards,
    /// and updates the operand-stack mirror.
    SubscriptDispatch {
        kind: SubscriptKind,
        /// Static observed-type hint for the resulting SSA. The
        /// recorder writes this into the buffer's `type_info` table so
        /// downstream `TypeCheck` predicates resolve cleanly.
        ty_hint: ObservedType,
    },
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
}

impl<'a> OpLoweringContext<'a> {
    pub fn new(inputs: &'a [SsaVar], fresh_dst: SsaVar) -> Self {
        Self {
            inputs,
            fresh_dst,
            call_effect_override: None,
        }
    }

    pub fn with_call_effect_override(mut self, eff: TraceEffect) -> Self {
        self.call_effect_override = Some(eff);
        self
    }
}

/// Identity passthrough kept for backwards compatibility with callers
/// that import the historical name. `relon_ir::EffectClass` is now a
/// re-export of `relon_trace_abi::EffectClass` (= [`TraceEffect`]) so
/// the two are the same type and no translation is needed.
pub fn map_effect_class(ir: IrEffect) -> TraceEffect {
    ir
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
        // F-D7-B: `String + String` short-circuits onto the dedicated
        // `TraceOp::StrConcat` fast path. The recorder mirrors the
        // operand-stack order of `binary_arith` (rhs on top → inputs[0])
        // and emits NotNull guards on both operands so the trace deopts
        // cleanly rather than returning a null `StringRef` from the
        // extern shim. We dispatch this before the generic
        // `binary_arith` arm because that helper only accepts integer
        // widths.
        //
        // Source-side wiring: the AST parser/analyzer pair lowers
        // `expr_lhs + expr_rhs` to `Op::Binary(Add, ...)` and onto
        // `Op::Add(IrType::String)` whenever both sides are typed
        // `IrType::String`. The IR-side AST→Op pipeline gating for
        // this shape is tracked in the F-D7-B follow-up; until that
        // lands no real Relon program produces `Op::Add(IrType::String)`,
        // but the recorder rule is kept here so a hand-built IR fragment
        // (or a future lowering pass) can drive `StrConcat` through
        // the same code path the stdlib `concat()` call already uses.
        Op::Add(IrType::String) => lower_str_add(cx),
        Op::Add(ty) => binary_arith(cx, *ty, BinaryArith::Add),
        // #168: `Op::StrConcatN` lowers to the dedicated
        // [`TraceOp::StrConcatN`] N-input variant. The emitter caps
        // inline lowering at [`MAX_INLINE_STR_CONCAT_N`] operands —
        // longer chains abort cleanly here so the outer tier router
        // falls back to cranelift AOT (which has the single-alloc
        // path) instead of recording a trace the emitter would
        // reject at install time.
        Op::StrConcatN { operand_count } => lower_str_concat_n(cx, *operand_count),
        Op::Sub(ty) => binary_arith(cx, *ty, BinaryArith::Sub),
        Op::Mul(ty) => binary_arith(cx, *ty, BinaryArith::Mul),
        Op::Div(ty) => binary_arith(cx, *ty, BinaryArith::Div),
        Op::Mod(ty) => binary_arith(cx, *ty, BinaryArith::Mod),

        Op::Eq(ty) => binary_cmp(cx, *ty, CmpKind::Eq),
        Op::Ne(ty) => binary_cmp(cx, *ty, CmpKind::Ne),
        Op::Lt(ty) => binary_cmp(cx, *ty, CmpKind::Lt),
        Op::Le(ty) => binary_cmp(cx, *ty, CmpKind::Le),
        Op::Gt(ty) => binary_cmp(cx, *ty, CmpKind::Gt),
        Op::Ge(ty) => binary_cmp(cx, *ty, CmpKind::Ge),

        // BitAnd is pure but the trace IR has no matching op today.
        // Surface as UnsupportedOp so the abort path stays honest — a
        // future phase adds the variant.
        //
        // F-D8-E.1: `Op::Mod` is wired into `binary_arith` above and
        // lowers to `TraceOp::Mod`; the legacy abort arm here is gone.
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

        // ---- F-D8-B subscript ops ---------------------------------------
        Op::DictGetByStringKey {
            shape_hash,
            value_ty,
            entry_count_hint,
            record_len_hint,
        } => {
            // Operand stack at call time: `[..., dict_ptr, key_ptr]`.
            // The recorder's apply_outcome arm peels both SSAs off
            // `inputs` (key on top of dict) and dispatches to
            // `RecorderState::emit_dict_lookup`. F-D8's helper currently
            // returns i64 — non-i64 element types still get a hint so
            // future widening keeps a single observed-type entry per dst.
            if cx.inputs.len() < 2 {
                return LowerOutcome::Abort(AbortReason::UnsupportedOp(
                    "DictGetByStringKeyUnderflow",
                ));
            }
            LowerOutcome::SubscriptDispatch {
                kind: SubscriptKind::DictLookup {
                    shape_hash: *shape_hash,
                    entry_count_hint: *entry_count_hint,
                    record_len_hint: *record_len_hint,
                },
                ty_hint: ty_to_observed(*value_ty),
            }
        }
        Op::ListGetByIntIdx { element_ty } => {
            // Operand stack at call time: `[..., list_ptr, idx]`. The
            // F-D8 host helper emits the bounds check inline; the
            // recorder also stamps a `Guard(BoundsCheck(idx, list))`
            // so LICM has a hoist target.
            if cx.inputs.len() < 2 {
                return LowerOutcome::Abort(AbortReason::UnsupportedOp("ListGetByIntIdxUnderflow"));
            }
            // F-D8 helper today reads i64 elements only. Refuse other
            // element shapes so the recorder aborts cleanly rather than
            // silently downcast a float / pointer slot.
            if !matches!(element_ty, IrType::I64) {
                return LowerOutcome::Abort(AbortReason::UnsupportedOp("ListGetByIntIdxNonI64"));
            }
            LowerOutcome::SubscriptDispatch {
                kind: SubscriptKind::ListGet,
                ty_hint: ty_to_observed(*element_ty),
            }
        }

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
            // F-D7: short-circuit the recognized string stdlib indices
            // onto the dedicated `TraceOp::Str*` fast path. These ops
            // sidestep the conservative `Unrecoverable` classification
            // because their bodies are pure (no host-visible side
            // effects beyond a fresh heap allocation handled by the
            // shim's allocator).
            if let Some(specialised) = lower_string_call(*fn_index, &cx) {
                return specialised;
            }
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

/// F-D7: stdlib indices that map straight onto the dedicated
/// `TraceOp::Str*` fast path. These must stay in sync with the
/// `builtin_stdlib()` ordering in [`relon_ir::stdlib`]:
///
/// - 6  → `concat(String, String) -> String`
/// - 9  → `substring(String, Int, Int) -> String`
/// - 36 → `contains(String, String) -> Bool` (F-D7-B placeholder;
///   not yet registered in `builtin_stdlib()`, see
///   [`STDLIB_IDX_CONTAINS`] below).
///
/// `starts_with` (idx 10) is intentionally NOT specialised here yet
/// — it returns a `Bool`, falls under the same TraceOp envelope as
/// `StrContains`, and will be added in a later F-D7 sub-phase to
/// keep the recorder's surface narrow until the bench confirms each
/// op pulls its weight.
pub const STDLIB_IDX_CONCAT: u32 = 6;
pub const STDLIB_IDX_SUBSTRING: u32 = 9;
/// F-D7-B: reserved stdlib index for the `(String, String) -> Bool`
/// `contains` body. The constant is the slot the future
/// `contains_string()` entry will occupy when added to
/// [`relon_ir::stdlib::builtin_stdlib`] (one past the last current
/// entry — see the doc-comment list above
/// [`relon_ir::stdlib::builtin_stdlib`]). Until that lands, no
/// real Relon program produces `Op::Call { fn_index: 36 }`; the
/// recorder rule below stays here so a hand-built IR fragment (or
/// the AST-level shortcut in
/// `record_method_call_contains`)
/// can route through the same fast path the F-D7-C inline emit
/// already supports.
///
/// **Drift guard.** The `stdlib_index_consistency` test below cross-
/// checks `stdlib_function_index("contains")` against this constant
/// on every workspace test run. The two states the test allows are:
///
/// 1. `stdlib_function_index("contains") == None` (current pre-F-D7-D
///    state): nothing to drift against; test passes.
/// 2. `stdlib_function_index("contains") == Some(STDLIB_IDX_CONTAINS)`:
///    F-D7-D registered the body at the expected slot; test passes.
///
/// Any other state — `Some(other)` — fails the test, blocking F-D7-D
/// from landing without updating either this constant or the bundle
/// order. The same guard exists for [`STDLIB_IDX_CONCAT`] and
/// [`STDLIB_IDX_SUBSTRING`], which are already-registered slots.
pub const STDLIB_IDX_CONTAINS: u32 = 36;

/// 2026-05-21: stdlib slot for `glob_match(s, pattern) -> Bool`. Pinned
/// to the same constant the IR side exports via
/// [`relon_ir::GLOB_MATCH_INDEX`]; the drift guard below cross-checks
/// the two against [`relon_ir::stdlib_function_index`] on every test
/// run so the recorder fast path stays aligned with the bundle order.
///
/// The recorder routes this slot onto [`TraceOp::StrGlobMatch`] —
/// dedicated extern call rather than the generic `TraceOp::Call`, so
/// the optimiser sees the op as `Pure` (loop-invariant `glob_match(s,
/// pat)` with the same operands hoists out of inner loops) and the
/// emitter wires the call directly to the `__relon_str_glob_match`
/// helper without a `resolve_call` round trip.
pub const STDLIB_IDX_GLOB_MATCH: u32 = 37;

fn lower_string_call(fn_index: u32, cx: &OpLoweringContext<'_>) -> Option<LowerOutcome> {
    match fn_index {
        STDLIB_IDX_CONCAT => {
            // Stack at call time: `[..., lhs, rhs]` (rhs pushed last
            // is `inputs[0]`). Mirror `binary_arith`'s operand order
            // so the emitted shim sees `(lhs, rhs)`.
            if cx.inputs.len() < 2 {
                return Some(LowerOutcome::Abort(AbortReason::UnsupportedOp(
                    "StrConcatUnderflow",
                )));
            }
            let rhs = cx.inputs[0];
            let lhs = cx.inputs[1];
            Some(LowerOutcome::Emit {
                op: TraceOp::StrConcat(cx.fresh_dst, lhs, rhs),
                dst: Some(cx.fresh_dst),
                // F-D7: emit a NotNull guard against both operands so
                // the trace deopts cleanly instead of returning a
                // null pointer from the shim. The shim itself also
                // guards against null, but emitting the guard here
                // surfaces a clear deopt cause to the rest of the
                // pipeline.
                guards_before: vec![GuardKind::NotNull(lhs), GuardKind::NotNull(rhs)],
                guards_after: vec![],
                effect: TraceEffect::Pure,
            })
        }
        STDLIB_IDX_SUBSTRING => {
            // Stack at call time: `[..., s, start, length]` (length
            // pushed last → `inputs[0]`).
            if cx.inputs.len() < 3 {
                return Some(LowerOutcome::Abort(AbortReason::UnsupportedOp(
                    "StrSubstringUnderflow",
                )));
            }
            let length = cx.inputs[0];
            let start = cx.inputs[1];
            let s = cx.inputs[2];
            Some(LowerOutcome::Emit {
                op: TraceOp::StrSubstring(cx.fresh_dst, s, start, length),
                dst: Some(cx.fresh_dst),
                // Bounds-style guard so the trace deopts when the
                // recorder's observed `start <= len` invariant breaks
                // at runtime; the shim still clamps for safety. We
                // use `BoundsCheck(start, s)` carrying the receiver
                // SSA as a stand-in for the length — the emitter's
                // bounds-check predicate reads the StringRef's len
                // field at runtime.
                guards_before: vec![GuardKind::NotNull(s)],
                guards_after: vec![],
                effect: TraceEffect::Pure,
            })
        }
        // F-D7-B: `contains(haystack, needle) -> Bool` short-circuits
        // onto `TraceOp::StrContains`. Operand-stack order at call
        // time: `[..., haystack, needle]` (needle pushed last →
        // `inputs[0]`). We emit a NotNull guard on the haystack only;
        // a null needle is handled by the F-D7-C inline-emit path
        // (zero-length needle returns true / shim returns true) so
        // guarding it here would surface spurious deopts. The
        // const-bytes side table (`OptimizedTrace::const_bytes`) is
        // filled by a dedicated recorder API
        // (`crate::RecorderState::record_method_call_contains`)
        // because this pure-function lowering cannot reach into the
        // walker's per-arg constant view; routing only via this arm
        // means inline-needle specialisation stays off until the
        // higher-level entry fires.
        STDLIB_IDX_CONTAINS => {
            if cx.inputs.len() < 2 {
                return Some(LowerOutcome::Abort(AbortReason::UnsupportedOp(
                    "StrContainsUnderflow",
                )));
            }
            let needle = cx.inputs[0];
            let haystack = cx.inputs[1];
            Some(LowerOutcome::Emit {
                op: TraceOp::StrContains(cx.fresh_dst, haystack, needle),
                dst: Some(cx.fresh_dst),
                guards_before: vec![GuardKind::NotNull(haystack)],
                guards_after: vec![],
                effect: TraceEffect::Pure,
            })
        }
        // 2026-05-21 Tier-2: `glob_match(s, pattern) -> Bool` short-
        // circuits onto `TraceOp::StrGlobMatch`. Operand-stack order
        // at call time: `[..., s, pattern]` (pattern pushed last so it
        // lands in `inputs[0]`). Mirrors the `STDLIB_IDX_CONTAINS` arm
        // — the haystack is the receiver, the pattern follows, both
        // share the `Pure` effect class.
        STDLIB_IDX_GLOB_MATCH => {
            if cx.inputs.len() < 2 {
                return Some(LowerOutcome::Abort(AbortReason::UnsupportedOp(
                    "StrGlobMatchUnderflow",
                )));
            }
            let pattern = cx.inputs[0];
            let s = cx.inputs[1];
            Some(LowerOutcome::Emit {
                op: TraceOp::StrGlobMatch(cx.fresh_dst, s, pattern),
                dst: Some(cx.fresh_dst),
                // Guard the haystack against null so the trace deopts
                // cleanly rather than letting the helper return 0 on a
                // stale arena reference. Pattern null-ness is rarer in
                // recorded surfaces; we leave it to the helper's own
                // null check (matches `StrContains`'s asymmetry).
                guards_before: vec![GuardKind::NotNull(s)],
                guards_after: vec![],
                effect: TraceEffect::Pure,
            })
        }
        _ => None,
    }
}

/// #168: maximum `operand_count` the trace-JIT inline emitter accepts
/// for `Op::StrConcatN`. Chains with more operands abort the recording
/// cleanly so the outer tier router can fall back to the cranelift AOT
/// backend (which has its own single-alloc `StrConcatN` lowering).
///
/// Sized at 4 because the inline emit unrolls the per-operand `(ptr,
/// len)` loads + stack-slot pointer stores into straight-line cranelift
/// IR; past 4 operands the trace tail balloons (each operand costs ~3
/// loads + 1 store) and the alloc-then-helper-memcpy path stops paying
/// off vs the bytecode VM's identically-shaped single-alloc concat. The
/// IR-side fold pass produces `operand_count` values in `2..=N` (with
/// N=2 already covered by [`TraceOp::StrConcat`]); 4 matches the
/// cranelift backend's empirically observed inline cap.
pub const MAX_INLINE_STR_CONCAT_N: u32 = 4;

/// #168: lower `Op::StrConcatN { operand_count }` onto the dedicated
/// [`TraceOp::StrConcatN`] N-input fast path.
///
/// Operand-stack order at call time mirrors `binary_arith`'s rhs-on-top
/// convention extended to N operands: `inputs[0]` is the rightmost /
/// last-pushed leaf, `inputs[operand_count - 1]` is the leftmost /
/// deepest leaf. We reverse into `operands` so `operands[0]` lines up
/// with the source-level leftmost argument (matches the IR-side
/// `Op::StrConcatN` operand semantics from `relon-ir/src/ir.rs`).
///
/// Emits one [`GuardKind::NotNull`] per operand so the trace deopts
/// cleanly rather than leaving the emitter to write through a stale
/// `*const StringRef`.
///
/// Bails out for `operand_count < 3` (the AST fold pass never produces
/// `<= 2`-arity `StrConcatN`; the two-operand shape is covered by the
/// pair-wise [`TraceOp::StrConcat`] fast path) and for `operand_count >
/// [`MAX_INLINE_STR_CONCAT_N`]` (per the constant's doc).
fn lower_str_concat_n(cx: OpLoweringContext<'_>, operand_count: u32) -> LowerOutcome {
    if operand_count < 3 {
        // Defensive: the IR-level fold pass guarantees `>= 3`. A
        // hand-built fragment with `operand_count` of 0 / 1 / 2 would
        // be malformed — abort cleanly so the recorder surfaces the
        // shape mismatch instead of writing an empty / pair-wise
        // `StrConcatN` the emitter can't safely interpret.
        return LowerOutcome::Abort(AbortReason::UnsupportedOp("StrConcatNTooFewOperands"));
    }
    if operand_count > MAX_INLINE_STR_CONCAT_N {
        return LowerOutcome::Abort(AbortReason::UnsupportedOp("StrConcatNOverCap"));
    }
    let n = operand_count as usize;
    if cx.inputs.len() < n {
        return LowerOutcome::Abort(AbortReason::UnsupportedOp("StrConcatNUnderflow"));
    }
    // Reverse so `operands[0]` is the deepest leaf (leftmost source
    // arg) and `operands[N-1]` is the topmost rhs — matches the IR-
    // side `Op::StrConcatN` operand-stack semantics. Keeps the
    // emitter's per-operand memcpy loop walking left-to-right through
    // the source-level chain.
    let operands: Vec<SsaVar> = cx.inputs[..n].iter().rev().copied().collect();
    let guards_before = operands
        .iter()
        .map(|s| GuardKind::NotNull(*s))
        .collect::<Vec<_>>();
    LowerOutcome::Emit {
        op: TraceOp::StrConcatN {
            dst: cx.fresh_dst,
            operands,
        },
        dst: Some(cx.fresh_dst),
        guards_before,
        guards_after: vec![],
        effect: TraceEffect::Pure,
    }
}

/// F-D7-B: lower `Op::Add(IrType::String)` onto the dedicated
/// `TraceOp::StrConcat` fast path.
///
/// Mirrors [`lower_string_call`]'s `STDLIB_IDX_CONCAT` arm operand
/// ordering — at call time the stack is `[..., lhs, rhs]` so rhs is
/// `cx.inputs[0]` and lhs is `cx.inputs[1]`. Emits NotNull guards on
/// both operands so the trace deopts cleanly rather than returning a
/// null `StringRef` from the extern shim.
fn lower_str_add(cx: OpLoweringContext<'_>) -> LowerOutcome {
    if cx.inputs.len() < 2 {
        return LowerOutcome::Abort(AbortReason::UnsupportedOp("StrConcatUnderflow"));
    }
    let rhs = cx.inputs[0];
    let lhs = cx.inputs[1];
    LowerOutcome::Emit {
        op: TraceOp::StrConcat(cx.fresh_dst, lhs, rhs),
        dst: Some(cx.fresh_dst),
        guards_before: vec![GuardKind::NotNull(lhs), GuardKind::NotNull(rhs)],
        guards_after: vec![],
        effect: TraceEffect::Pure,
    }
}

enum BinaryArith {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

fn binary_arith(cx: OpLoweringContext<'_>, ty: IrType, kind: BinaryArith) -> LowerOutcome {
    // F64 arithmetic is not part of the Phase-1 trace op set — the
    // trace IR's Add/Sub/Mul/Div are integer-only. Surface as an
    // UnsupportedOp until a typed variant lands.
    if matches!(ty, IrType::F64) {
        return LowerOutcome::Abort(AbortReason::UnsupportedOp("FloatArith"));
    }
    // F-D7-B: `Op::Add(IrType::String)` is short-circuited before
    // this helper is reached (see `lower_op`); a `Sub`/`Mul`/`Div`
    // with String operands is nonsense at the source level and
    // would never lower through the IR. Defensive guard rejects it
    // here so a hand-built fuzz fragment cannot smuggle two String
    // SSAs into the integer `TraceOp::Sub`.
    if matches!(ty, IrType::String) {
        return LowerOutcome::Abort(AbortReason::UnsupportedOp("StringArith"));
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
        BinaryArith::Mod => (
            TraceOp::Mod(cx.fresh_dst, lhs, rhs),
            // F-D8-E.1: same RecoverableWrite rationale as Div —
            // `b == 0` traps, and the trace must be able to roll
            // back to the pre-modulo state on deopt. The same
            // `ArithOverflow(dst)` guard covers the `i64::MIN % -1`
            // overflow corner case the cranelift `srem` exposes.
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

    /// Drift guard for the [`STDLIB_IDX_CONCAT`] / [`STDLIB_IDX_SUBSTRING`]
    /// / [`STDLIB_IDX_CONTAINS`] constants in this module. Each
    /// constant is the position the recorder expects the matching
    /// stdlib body to occupy in `relon_ir::stdlib::builtin_stdlib`.
    /// Re-ordering the bundle without bumping the constant would
    /// silently route a different op through the trace-JIT fast path
    /// (or fail to route at all). This test pins the contract.
    ///
    /// Each constant has two acceptable states: the body is either
    /// not registered (lookup returns `None`, pre-F-D7-D for
    /// `contains`), or registered at exactly the constant's slot.
    /// Any other state — `Some(other)` — is a real drift and fails
    /// the test loudly, blocking the offending bundle change.
    #[test]
    fn stdlib_index_consistency() {
        for (name, expected) in [
            ("concat", STDLIB_IDX_CONCAT),
            ("substring", STDLIB_IDX_SUBSTRING),
            ("contains", STDLIB_IDX_CONTAINS),
            ("glob_match", STDLIB_IDX_GLOB_MATCH),
        ] {
            match relon_ir::stdlib::stdlib_function_index(name) {
                None => {
                    // Not yet registered — F-D7-D is expected to land
                    // `contains` at index `STDLIB_IDX_CONTAINS`. Print
                    // a note so the test log makes the pre-F-D7-D
                    // state visible; concat / substring should not hit
                    // this branch today (they are already registered).
                    eprintln!(
                        "note: stdlib `{name}` not registered yet; expected to land at index {expected}"
                    );
                }
                Some(actual) if actual == expected => {}
                Some(actual) => panic!(
                    "stdlib `{name}` is at index {actual} but the recorder constant pins it to {expected}. \
                     Either rebase the constant or restore the bundle ordering."
                ),
            }
        }
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

    /// F-D8-E.1: `Op::Mod(I64)` collapses to a single `TraceOp::Mod`
    /// plus one `ArithOverflow` guard, instead of the old
    /// `Div + Mul + Sub` triple. This drift guard pins both the
    /// opcode and the guard shape so a future refactor cannot
    /// silently revert to the expansion that cost W5 three guards
    /// per iter.
    #[test]
    fn mod_i64_emits_trace_mod() {
        let inputs = [SsaVar(1), SsaVar(2)];
        let outcome = lower_op(&Op::Mod(IrType::I64), cx_with(&inputs, SsaVar(3)));
        match outcome {
            LowerOutcome::Emit {
                op,
                dst,
                guards_before,
                guards_after,
                effect,
            } => {
                assert!(
                    matches!(op, TraceOp::Mod(_, _, _)),
                    "Op::Mod(I64) must lower to TraceOp::Mod, got {:?}",
                    op
                );
                assert_eq!(dst, Some(SsaVar(3)));
                assert!(guards_before.is_empty());
                assert!(matches!(
                    guards_after.as_slice(),
                    [GuardKind::ArithOverflow(_)]
                ));
                assert_eq!(effect, TraceEffect::RecoverableWrite);
            }
            other => panic!("unexpected outcome for Mod(I64): {:?}", other),
        }
    }

    #[test]
    fn mod_f64_aborts_float_arith() {
        let inputs = [SsaVar(1), SsaVar(2)];
        let outcome = lower_op(&Op::Mod(IrType::F64), cx_with(&inputs, SsaVar(3)));
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("FloatArith"))
        ));
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
            map_effect_class(IrEffect::Unrecoverable),
            TraceEffect::Unrecoverable
        );
    }

    // ---- F-D7 string lowering rules ----

    #[test]
    fn concat_stdlib_index_emits_str_concat() {
        // Stack [lhs, rhs] → recorder feeds inputs as `[rhs, lhs]`
        // (rhs is on top, push order).
        let inputs = [SsaVar(20), SsaVar(10)]; // rhs=20, lhs=10
        let outcome = lower_op(
            &Op::Call {
                fn_index: STDLIB_IDX_CONCAT,
                arg_count: 2,
                param_tys: vec![IrType::String, IrType::String],
                ret_ty: IrType::String,
            },
            cx_with(&inputs, SsaVar(99)),
        );
        match outcome {
            LowerOutcome::Emit {
                op,
                dst: Some(d),
                guards_before,
                effect,
                ..
            } => {
                assert_eq!(d, SsaVar(99));
                assert_eq!(effect, TraceEffect::Pure);
                match op {
                    TraceOp::StrConcat(_, l, r) => {
                        assert_eq!(l, SsaVar(10), "lhs must be the second-from-top input");
                        assert_eq!(r, SsaVar(20), "rhs must be the top-of-stack input");
                    }
                    other => panic!("expected StrConcat, got {:?}", other),
                }
                // Two NotNull guards (one per operand).
                assert!(
                    matches!(
                        guards_before.as_slice(),
                        [GuardKind::NotNull(_), GuardKind::NotNull(_)]
                    ),
                    "expected NotNull guards on both operands, got {:?}",
                    guards_before
                );
            }
            other => panic!("expected Emit, got {:?}", other),
        }
    }

    #[test]
    fn substring_stdlib_index_emits_str_substring() {
        // Stack [s, start, length] → inputs ordered `[length, start, s]`.
        let inputs = [SsaVar(3), SsaVar(2), SsaVar(1)]; // length=3, start=2, s=1
        let outcome = lower_op(
            &Op::Call {
                fn_index: STDLIB_IDX_SUBSTRING,
                arg_count: 3,
                param_tys: vec![IrType::String, IrType::I64, IrType::I64],
                ret_ty: IrType::String,
            },
            cx_with(&inputs, SsaVar(77)),
        );
        match outcome {
            LowerOutcome::Emit {
                op,
                dst: Some(d),
                effect,
                ..
            } => {
                assert_eq!(d, SsaVar(77));
                assert_eq!(effect, TraceEffect::Pure);
                match op {
                    TraceOp::StrSubstring(_, s, start, length) => {
                        assert_eq!(s, SsaVar(1));
                        assert_eq!(start, SsaVar(2));
                        assert_eq!(length, SsaVar(3));
                    }
                    other => panic!("expected StrSubstring, got {:?}", other),
                }
            }
            other => panic!("expected Emit, got {:?}", other),
        }
    }

    #[test]
    fn concat_underflow_aborts() {
        // Only one input on the stack — recorder must abort.
        let inputs = [SsaVar(5)];
        let outcome = lower_op(
            &Op::Call {
                fn_index: STDLIB_IDX_CONCAT,
                arg_count: 2,
                param_tys: vec![IrType::String, IrType::String],
                ret_ty: IrType::String,
            },
            cx_with(&inputs, SsaVar(99)),
        );
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("StrConcatUnderflow"))
        ));
    }

    #[test]
    fn substring_underflow_aborts() {
        // Two inputs (need three) — recorder must abort.
        let inputs = [SsaVar(1), SsaVar(2)];
        let outcome = lower_op(
            &Op::Call {
                fn_index: STDLIB_IDX_SUBSTRING,
                arg_count: 3,
                param_tys: vec![IrType::String, IrType::I64, IrType::I64],
                ret_ty: IrType::String,
            },
            cx_with(&inputs, SsaVar(99)),
        );
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("StrSubstringUnderflow"))
        ));
    }

    /// 2026-05-21 Tier-2: `Op::Call { fn_index = STDLIB_IDX_GLOB_MATCH }`
    /// short-circuits onto `TraceOp::StrGlobMatch`. Operand layout
    /// mirrors `STDLIB_IDX_CONTAINS`: pattern is on top of the stack
    /// (inputs[0]) and the haystack is just below (inputs[1]). A
    /// NotNull guard on the haystack only — pattern null is handled by
    /// the helper itself, same asymmetry `StrContains` uses.
    #[test]
    fn glob_match_stdlib_index_emits_str_glob_match() {
        let inputs = [SsaVar(30), SsaVar(20)]; // pattern=30 (top), s=20
        let outcome = lower_op(
            &Op::Call {
                fn_index: STDLIB_IDX_GLOB_MATCH,
                arg_count: 2,
                param_tys: vec![IrType::String, IrType::String],
                ret_ty: IrType::Bool,
            },
            cx_with(&inputs, SsaVar(88)),
        );
        match outcome {
            LowerOutcome::Emit {
                op,
                dst: Some(d),
                guards_before,
                effect,
                ..
            } => {
                assert_eq!(d, SsaVar(88));
                assert_eq!(effect, TraceEffect::Pure);
                match op {
                    TraceOp::StrGlobMatch(_, s, pat) => {
                        assert_eq!(s, SsaVar(20));
                        assert_eq!(pat, SsaVar(30));
                    }
                    other => panic!("expected StrGlobMatch, got {:?}", other),
                }
                assert!(
                    matches!(guards_before.as_slice(), [GuardKind::NotNull(_)]),
                    "expected one NotNull guard on the haystack, got {:?}",
                    guards_before
                );
            }
            other => panic!("expected Emit, got {:?}", other),
        }
    }

    /// Underflow guard: too few inputs aborts cleanly under the
    /// `StrGlobMatchUnderflow` label so the recorder logs surface the
    /// failure mode the same way the sibling stdlib short-circuits do.
    #[test]
    fn glob_match_underflow_aborts() {
        let inputs = [SsaVar(10)];
        let outcome = lower_op(
            &Op::Call {
                fn_index: STDLIB_IDX_GLOB_MATCH,
                arg_count: 2,
                param_tys: vec![IrType::String, IrType::String],
                ret_ty: IrType::Bool,
            },
            cx_with(&inputs, SsaVar(88)),
        );
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("StrGlobMatchUnderflow"))
        ));
    }

    #[test]
    fn non_string_stdlib_index_falls_through_to_generic_call() {
        // fn_index=11 (list_int_sum) is NOT in the F-D7 specialised
        // set. With a Pure override the lowering rule must produce a
        // generic `TraceOp::Call`, not a `TraceOp::Str*`.
        let inputs = [SsaVar(1)];
        let outcome = lower_op(
            &Op::Call {
                fn_index: 11,
                arg_count: 1,
                param_tys: vec![IrType::ListInt],
                ret_ty: IrType::I64,
            },
            cx_with(&inputs, SsaVar(99)).with_call_effect_override(TraceEffect::Pure),
        );
        match outcome {
            LowerOutcome::Emit { op, .. } => {
                assert!(
                    matches!(op, TraceOp::Call(_, _, _, _)),
                    "expected generic Call, got {:?}",
                    op
                );
            }
            other => panic!("expected Emit, got {:?}", other),
        }
    }

    // ---- F-D7-B: String + / contains recognition ----

    #[test]
    fn add_irtype_string_emits_str_concat() {
        // Stack ordering matches `binary_arith`: rhs on top → inputs[0].
        let inputs = [SsaVar(20), SsaVar(10)]; // rhs=20, lhs=10
        let outcome = lower_op(&Op::Add(IrType::String), cx_with(&inputs, SsaVar(99)));
        match outcome {
            LowerOutcome::Emit {
                op,
                dst: Some(d),
                guards_before,
                effect,
                ..
            } => {
                assert_eq!(d, SsaVar(99));
                assert_eq!(effect, TraceEffect::Pure);
                match op {
                    TraceOp::StrConcat(_, l, r) => {
                        assert_eq!(l, SsaVar(10), "lhs is the second-from-top input");
                        assert_eq!(r, SsaVar(20), "rhs is the top input");
                    }
                    other => panic!("expected StrConcat, got {other:?}"),
                }
                assert!(
                    matches!(
                        guards_before.as_slice(),
                        [GuardKind::NotNull(_), GuardKind::NotNull(_)]
                    ),
                    "expected NotNull guards on both operands, got {guards_before:?}"
                );
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn add_irtype_int_keeps_generic_binary_arith() {
        // `Op::Add(IrType::I64)` must NOT route into `lower_str_add` —
        // type mismatch should fall through to `binary_arith`.
        let inputs = [SsaVar(2), SsaVar(1)];
        let outcome = lower_op(&Op::Add(IrType::I64), cx_with(&inputs, SsaVar(7)));
        match outcome {
            LowerOutcome::Emit { op, .. } => {
                assert!(
                    matches!(op, TraceOp::Add(_, _, _)),
                    "expected integer Add, got {op:?}"
                );
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    // ---- #168: N-operand StrConcatN lowering ----

    #[test]
    fn str_concat_n_three_operands_emits_trace_op() {
        // Stack at call time: [.., leaf0, leaf1, leaf2] with leaf2 on
        // top. `inputs` are popped top-first by the walker, so
        // `inputs[0] = leaf2` (rhs / topmost), `inputs[2] = leaf0`
        // (lhs / deepest leaf). The lowering reverses into operand
        // order so `operands[0]` is the source-level leftmost arg.
        let inputs = [SsaVar(30), SsaVar(20), SsaVar(10)]; // top → bottom
        let outcome = lower_op(
            &Op::StrConcatN { operand_count: 3 },
            cx_with(&inputs, SsaVar(99)),
        );
        match outcome {
            LowerOutcome::Emit {
                op,
                dst: Some(d),
                guards_before,
                effect,
                ..
            } => {
                assert_eq!(d, SsaVar(99));
                assert_eq!(effect, TraceEffect::Pure);
                match op {
                    TraceOp::StrConcatN { dst, operands } => {
                        assert_eq!(dst, SsaVar(99));
                        // operands run left-to-right: leaf0, leaf1, leaf2.
                        assert_eq!(operands, vec![SsaVar(10), SsaVar(20), SsaVar(30)]);
                    }
                    other => panic!("expected StrConcatN, got {other:?}"),
                }
                // One NotNull guard per operand so the trace deopts on
                // a stale `*const StringRef` instead of segfaulting.
                assert_eq!(guards_before.len(), 3);
                for g in &guards_before {
                    assert!(matches!(g, GuardKind::NotNull(_)));
                }
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn str_concat_n_four_operands_at_cap_emits_trace_op() {
        let inputs = [SsaVar(4), SsaVar(3), SsaVar(2), SsaVar(1)];
        let outcome = lower_op(
            &Op::StrConcatN { operand_count: 4 },
            cx_with(&inputs, SsaVar(50)),
        );
        match outcome {
            LowerOutcome::Emit { op, .. } => match op {
                TraceOp::StrConcatN { dst, operands } => {
                    assert_eq!(dst, SsaVar(50));
                    assert_eq!(operands.len(), 4);
                    assert_eq!(operands, vec![SsaVar(1), SsaVar(2), SsaVar(3), SsaVar(4)]);
                }
                other => panic!("expected StrConcatN, got {other:?}"),
            },
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn str_concat_n_above_cap_aborts() {
        // Five operands exceed MAX_INLINE_STR_CONCAT_N — recorder aborts
        // so the outer tier router falls back to the cranelift AOT
        // backend's identically shaped single-alloc lowering.
        let inputs = [SsaVar(5), SsaVar(4), SsaVar(3), SsaVar(2), SsaVar(1)];
        let outcome = lower_op(
            &Op::StrConcatN { operand_count: 5 },
            cx_with(&inputs, SsaVar(99)),
        );
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("StrConcatNOverCap"))
        ));
    }

    #[test]
    fn str_concat_n_too_few_operands_aborts() {
        let inputs = [SsaVar(2), SsaVar(1)];
        let outcome = lower_op(
            &Op::StrConcatN { operand_count: 2 },
            cx_with(&inputs, SsaVar(99)),
        );
        // operand_count < 3 is a malformed IR fragment (the fold pass
        // never produces it); rejecting it cleanly keeps the recorder
        // from emitting a degenerate StrConcatN the emitter can't safely
        // interpret.
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("StrConcatNTooFewOperands"))
        ));
    }

    #[test]
    fn str_concat_n_underflow_aborts() {
        // operand_count=3 but only two inputs available — caller is
        // malformed (walker should have refused to dispatch). Surface as
        // a distinct abort reason for diagnostic clarity.
        let inputs = [SsaVar(20), SsaVar(10)];
        let outcome = lower_op(
            &Op::StrConcatN { operand_count: 3 },
            cx_with(&inputs, SsaVar(99)),
        );
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("StrConcatNUnderflow"))
        ));
    }

    #[test]
    fn add_irtype_string_underflow_aborts() {
        let inputs = [SsaVar(1)];
        let outcome = lower_op(&Op::Add(IrType::String), cx_with(&inputs, SsaVar(99)));
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("StrConcatUnderflow"))
        ));
    }

    #[test]
    fn sub_irtype_string_aborts_as_string_arith() {
        // Defensive: Sub/Mul/Div with String operands is nonsense and
        // never produced by real source. The recorder rejects it
        // explicitly rather than handing two String SSAs to the
        // integer Sub op.
        let inputs = [SsaVar(2), SsaVar(1)];
        let outcome = lower_op(&Op::Sub(IrType::String), cx_with(&inputs, SsaVar(7)));
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("StringArith"))
        ));
    }

    #[test]
    fn contains_stdlib_index_emits_str_contains() {
        // Stack [haystack, needle] → inputs ordered [needle, haystack].
        let inputs = [SsaVar(7), SsaVar(3)]; // needle=7, haystack=3
        let outcome = lower_op(
            &Op::Call {
                fn_index: STDLIB_IDX_CONTAINS,
                arg_count: 2,
                param_tys: vec![IrType::String, IrType::String],
                ret_ty: IrType::Bool,
            },
            cx_with(&inputs, SsaVar(50)),
        );
        match outcome {
            LowerOutcome::Emit {
                op,
                dst: Some(d),
                guards_before,
                effect,
                ..
            } => {
                assert_eq!(d, SsaVar(50));
                assert_eq!(effect, TraceEffect::Pure);
                match op {
                    TraceOp::StrContains(_, h, n) => {
                        assert_eq!(h, SsaVar(3), "haystack is the second-from-top input");
                        assert_eq!(n, SsaVar(7), "needle is the top input");
                    }
                    other => panic!("expected StrContains, got {other:?}"),
                }
                // Single NotNull(haystack) guard — needle stays
                // unguarded so the zero-length case can succeed.
                assert!(
                    matches!(guards_before.as_slice(), [GuardKind::NotNull(_)]),
                    "expected single NotNull guard, got {guards_before:?}"
                );
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn contains_underflow_aborts() {
        let inputs = [SsaVar(5)];
        let outcome = lower_op(
            &Op::Call {
                fn_index: STDLIB_IDX_CONTAINS,
                arg_count: 2,
                param_tys: vec![IrType::String, IrType::String],
                ret_ty: IrType::Bool,
            },
            cx_with(&inputs, SsaVar(50)),
        );
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("StrContainsUnderflow"))
        ));
    }

    // ---- F-D8-B subscript lowering rules ----

    #[test]
    fn dict_get_by_string_key_dispatches_dict_lookup() {
        // Operand stack at call time: `[dict_ptr, key_ptr]` →
        // `inputs[0] = key_ptr (top)`, `inputs[1] = dict_ptr`.
        let inputs = [SsaVar(20), SsaVar(10)];
        let outcome = lower_op(
            &Op::DictGetByStringKey {
                shape_hash: 0xfeed_face_dead_beef,
                value_ty: IrType::I64,
                entry_count_hint: None,
                record_len_hint: None,
            },
            cx_with(&inputs, SsaVar(99)),
        );
        match outcome {
            LowerOutcome::SubscriptDispatch { kind, ty_hint } => {
                match kind {
                    SubscriptKind::DictLookup {
                        shape_hash,
                        entry_count_hint,
                        record_len_hint,
                    } => {
                        assert_eq!(shape_hash, 0xfeed_face_dead_beef);
                        assert!(entry_count_hint.is_none());
                        assert!(record_len_hint.is_none());
                    }
                    other => panic!("expected DictLookup, got {:?}", other),
                }
                assert_eq!(ty_hint, ObservedType::I64);
            }
            other => panic!("expected SubscriptDispatch, got {:?}", other),
        }
    }

    /// F-D8-E.7: when the IR carries an entry_count_hint, the lowering
    /// rule must forward it verbatim so the recorder can stash it in
    /// the buffer's `dict_entry_count_hints` side table for advisory
    /// inline-lowering metadata.
    #[test]
    fn dict_get_by_string_key_forwards_entry_count_hint() {
        let inputs = [SsaVar(21), SsaVar(11)];
        let outcome = lower_op(
            &Op::DictGetByStringKey {
                shape_hash: 0xabc,
                value_ty: IrType::I64,
                entry_count_hint: Some(10),
                record_len_hint: Some(256),
            },
            cx_with(&inputs, SsaVar(99)),
        );
        match outcome {
            LowerOutcome::SubscriptDispatch { kind, .. } => match kind {
                SubscriptKind::DictLookup {
                    entry_count_hint,
                    record_len_hint,
                    ..
                } => {
                    assert_eq!(entry_count_hint, Some(10));
                    assert_eq!(record_len_hint, Some(256));
                }
                other => panic!("expected DictLookup, got {:?}", other),
            },
            other => panic!("expected SubscriptDispatch, got {:?}", other),
        }
    }

    #[test]
    fn list_get_by_int_idx_dispatches_list_get() {
        let inputs = [SsaVar(30), SsaVar(20)]; // idx=30, list_ptr=20
        let outcome = lower_op(
            &Op::ListGetByIntIdx {
                element_ty: IrType::I64,
            },
            cx_with(&inputs, SsaVar(99)),
        );
        match outcome {
            LowerOutcome::SubscriptDispatch { kind, ty_hint } => {
                assert!(matches!(kind, SubscriptKind::ListGet));
                assert_eq!(ty_hint, ObservedType::I64);
            }
            other => panic!("expected SubscriptDispatch, got {:?}", other),
        }
    }

    #[test]
    fn dict_subscript_underflow_aborts() {
        // Only one input on the stack — the lowering rule must abort
        // cleanly rather than reach into `inputs[1]`.
        let inputs = [SsaVar(5)];
        let outcome = lower_op(
            &Op::DictGetByStringKey {
                shape_hash: 0,
                value_ty: IrType::I64,
                entry_count_hint: None,
                record_len_hint: None,
            },
            cx_with(&inputs, SsaVar(99)),
        );
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("DictGetByStringKeyUnderflow"))
        ));
    }

    #[test]
    fn list_subscript_non_i64_element_aborts() {
        // F-D8 helper only handles i64 elements today; non-i64 must
        // abort to avoid emitting a TraceOp::ListGet against a
        // mis-sized payload.
        let inputs = [SsaVar(2), SsaVar(1)];
        let outcome = lower_op(
            &Op::ListGetByIntIdx {
                element_ty: IrType::F64,
            },
            cx_with(&inputs, SsaVar(99)),
        );
        assert!(matches!(
            outcome,
            LowerOutcome::Abort(AbortReason::UnsupportedOp("ListGetByIntIdxNonI64"))
        ));
    }
}
