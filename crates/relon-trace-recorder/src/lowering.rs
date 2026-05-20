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
const STDLIB_IDX_CONCAT: u32 = 6;
const STDLIB_IDX_SUBSTRING: u32 = 9;
/// F-D7-B: reserved stdlib index for the `(String, String) -> Bool`
/// `contains` body. The constant is the slot the future
/// `contains_string()` entry will occupy when added to
/// [`relon_ir::stdlib::builtin_stdlib`] (one past the last current
/// entry — see the doc-comment list above
/// [`relon_ir::stdlib::builtin_stdlib`]). Until that lands, no
/// real Relon program produces `Op::Call { fn_index: 36 }`; the
/// recorder rule below stays here so a hand-built IR fragment (or
/// the AST-level shortcut in
/// [`record_method_call_contains`](crate::RecorderState::record_method_call_contains))
/// can route through the same fast path the F-D7-C inline emit
/// already supports.
pub const STDLIB_IDX_CONTAINS: u32 = 36;

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
        // ([`crate::RecorderState::record_method_call_contains`])
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
        _ => None,
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
}
