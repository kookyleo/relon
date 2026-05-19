//! Self-contained low-level trace IR used by the v6-gamma scaffolding.
//!
//! The op set is intentionally **not** a clone of `relon_ir::Op`. It
//! reflects what a LuaJIT-style recorder records while observing the
//! cranelift-generic backend execute — closer to register-machine
//! primitives than to the surface-syntax-driven IR. A future v6-gamma
//! lowering pass will translate Relon IR ops into one or more
//! [`TraceOp`]s (TODO: that lowering lives outside this crate).
//!
//! ## Opaque ids
//!
//! v6-γ M1 promotes [`ExternalPc`], [`ExternalSlot`], [`ExternalAddr`]
//! and [`ObservedType`] to the shared `relon-trace-abi` crate. This
//! module re-exports them so existing `relon_trace_jit::ExternalPc`
//! call sites keep compiling, but the layout invariants live there.
//! Reviewers MUST NOT redeclare these types here.

use serde::{Deserialize, Serialize};

use crate::effect::EffectClass;

pub use relon_trace_abi::{ExternalAddr, ExternalPc, ExternalSlot, ObservedType};

/// Dense SSA variable id local to a single trace. 32-bit so trace
/// buffers stay cache friendly (LuaJIT uses the same width).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SsaVar(pub u32);

impl SsaVar {
    /// Sentinel "no variable" — used by ops that produce no value.
    pub const NONE: SsaVar = SsaVar(u32::MAX);

    /// Raw u32 view, exposed for storage in side tables.
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// Identifier for a callee referenced by [`TraceOp::Call`]. Opaque
/// token — the trace recorder learns the symbol id from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FuncId(pub u32);

/// Byte offset used by load/store ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Offset(pub i32);

/// Comparison kind used by [`TraceOp::Cmp`]. Maps 1:1 to the integer
/// compare operators we'll emit cranelift IR for during the v6-gamma
/// phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CmpKind {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpKind {
    pub fn apply_i64(self, a: i64, b: i64) -> bool {
        match self {
            CmpKind::Eq => a == b,
            CmpKind::Ne => a != b,
            CmpKind::Lt => a < b,
            CmpKind::Le => a <= b,
            CmpKind::Gt => a > b,
            CmpKind::Ge => a >= b,
        }
    }
}

/// Compact constant captured during recording. Used by the recorder's
/// type/const sidetable and by the constant-folding pass.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum TraceConst {
    I32(i32),
    I64(i64),
    Bool(bool),
}

/// The trace-recorder-level op stream.
///
/// Each variant carries its operand SSA ids explicitly so optimiser
/// passes can rewrite them without touching the rest of the buffer
/// layout. Effect classification is per-variant via
/// [`TraceOp::effect_class`].
///
/// Notes:
/// - `Div` is classed `RecoverableWrite` to model the trap-on-zero
///   side channel: a deopt has to roll back any partial work done in
///   a fused-op chain (TODO v6-gamma: revisit -- may instead be
///   modelled as `Pure` + an explicit `Guard(NotZero,...)` op).
/// - `Call` carries its [`EffectClass`] inline because the recorder
///   can only know a callee's effect from a host-provided sidetable.
#[derive(Debug, Clone)]
pub enum TraceOp {
    // ---- Arithmetic / compare ----------------------------------------
    /// `dst = a + b` (i64 wrap-around; overflow guarded externally).
    Add(SsaVar, SsaVar, SsaVar),
    /// `dst = a - b`.
    Sub(SsaVar, SsaVar, SsaVar),
    /// `dst = a * b`.
    Mul(SsaVar, SsaVar, SsaVar),
    /// `dst = a / b`. Treated as `RecoverableWrite` so the recorder
    /// captures the divisor's pre-value for deopt rollback if the
    /// trace fuses around it.
    Div(SsaVar, SsaVar, SsaVar),
    /// `dst = cmp a b` (bool result via i64-typed slot).
    Cmp(CmpKind, SsaVar, SsaVar, SsaVar),

    // ---- Memory ------------------------------------------------------
    /// `dst = *(base + offset)`.
    Load(SsaVar, SsaVar, Offset),
    /// `*(base + offset) = src`.
    Store(SsaVar, Offset, SsaVar),

    // ---- Constants ---------------------------------------------------
    ConstI32(SsaVar, i32),
    ConstI64(SsaVar, i64),

    // ---- Arg materialisation ----------------------------------------
    /// `dst = args_ptr[slot_idx]` — pulls a packed `u64` from the
    /// trace entry's `args_ptr` second-arg.
    ///
    /// v6-δ M1: the recorder emits this op when the IR walker hits
    /// `Op::LocalGet(idx)` so the emitter can materialise the SSA
    /// value from the cranelift prologue's packed arg array, instead
    /// of leaving the SSA unbound and surfacing `EmitError::UnboundSsa`
    /// at install time.
    ///
    /// `slot_idx` is the packed-array index (`0` for first arg, `1`
    /// for second, ...). The emitter computes the byte offset as
    /// `slot_idx * 8`.
    LocalGet(SsaVar, u32),

    // ---- Control / guard --------------------------------------------
    /// Inline guard. Failure deopts to the most recent enclosing
    /// [`crate::GuardSite`].
    Guard(GuardKind, SsaVar),

    /// Callee referenced by id. Recorder is responsible for knowing
    /// (and asserting) the callee's effect class — opaque here.
    Call(SsaVar, FuncId, Vec<SsaVar>, EffectClass),

    /// Return from the trace.
    Return(SsaVar),

    // ---- Loop markers -----------------------------------------------
    /// Marks the entry of a recorded loop. `loop_id` distinguishes
    /// nested loops; the same id pairs `MarkLoopHead` with its matching
    /// `MarkLoopBack`. Pure marker op, no SSA effect, used exclusively
    /// by the LICM pass to identify hoistable regions.
    MarkLoopHead {
        loop_id: u32,
    },
    /// Marks the back-edge / exit of the loop with the matching
    /// `loop_id`. See [`TraceOp::MarkLoopHead`].
    MarkLoopBack {
        loop_id: u32,
    },
}

/// Inline guard kind. Mirrors the design doc §2 enum without the
/// deopt-state payload — that lives on the enclosing
/// [`crate::GuardSite`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GuardKind {
    /// Trace assumed `var` had observed type `ty`.
    TypeCheck(SsaVar, ObservedType),
    /// Trace assumed `var != null`.
    NotNull(SsaVar),
    /// Trace assumed `var < limit`.
    BoundsCheck(SsaVar, SsaVar),
    /// Trace assumed an arithmetic op didn't overflow.
    ArithOverflow(SsaVar),
}

impl TraceOp {
    /// Static side-effect classification.
    ///
    /// Pre-conditions for the optimizer rely on this being correct;
    /// any variant added here must update the const-fold / dead-store
    /// passes if its class differs from existing ones.
    pub fn effect_class(&self) -> EffectClass {
        match self {
            TraceOp::Add(_, _, _)
            | TraceOp::Sub(_, _, _)
            | TraceOp::Mul(_, _, _)
            | TraceOp::Cmp(_, _, _, _)
            | TraceOp::ConstI32(_, _)
            | TraceOp::ConstI64(_, _)
            | TraceOp::Guard(_, _)
            | TraceOp::Return(_)
            | TraceOp::MarkLoopHead { .. }
            | TraceOp::MarkLoopBack { .. } => EffectClass::Pure,

            TraceOp::Load(_, _, _) | TraceOp::LocalGet(_, _) => EffectClass::ReadOnly,

            TraceOp::Div(_, _, _) | TraceOp::Store(_, _, _) => EffectClass::RecoverableWrite,

            TraceOp::Call(_, _, _, eff) => *eff,
        }
    }

    /// SSA var produced by this op, if any. `Return`, `Store`,
    /// `Guard`, and the loop markers produce no SSA value.
    pub fn output(&self) -> Option<SsaVar> {
        match self {
            TraceOp::Add(dst, _, _)
            | TraceOp::Sub(dst, _, _)
            | TraceOp::Mul(dst, _, _)
            | TraceOp::Div(dst, _, _)
            | TraceOp::Cmp(_, dst, _, _)
            | TraceOp::Load(dst, _, _)
            | TraceOp::ConstI32(dst, _)
            | TraceOp::ConstI64(dst, _)
            | TraceOp::LocalGet(dst, _)
            | TraceOp::Call(dst, _, _, _) => Some(*dst),

            TraceOp::Store(_, _, _)
            | TraceOp::Guard(_, _)
            | TraceOp::Return(_)
            | TraceOp::MarkLoopHead { .. }
            | TraceOp::MarkLoopBack { .. } => None,
        }
    }

    /// SSA vars *read* by this op. Returned in fixed order for
    /// deterministic rewriting by optimizer passes.
    pub fn inputs(&self) -> Vec<SsaVar> {
        match self {
            TraceOp::Add(_, a, b)
            | TraceOp::Sub(_, a, b)
            | TraceOp::Mul(_, a, b)
            | TraceOp::Div(_, a, b) => vec![*a, *b],
            TraceOp::Cmp(_, _, a, b) => vec![*a, *b],
            TraceOp::Load(_, base, _) => vec![*base],
            TraceOp::Store(base, _, src) => vec![*base, *src],
            TraceOp::ConstI32(_, _) | TraceOp::ConstI64(_, _) | TraceOp::LocalGet(_, _) => vec![],
            TraceOp::Guard(kind, _check) => match *kind {
                GuardKind::TypeCheck(v, _) => vec![v],
                GuardKind::NotNull(v) => vec![v],
                GuardKind::BoundsCheck(v, limit) => vec![v, limit],
                GuardKind::ArithOverflow(v) => vec![v],
            },
            TraceOp::Call(_, _, args, _) => args.clone(),
            TraceOp::Return(v) => vec![*v],
            TraceOp::MarkLoopHead { .. } | TraceOp::MarkLoopBack { .. } => vec![],
        }
    }

    /// Is this op a guard?
    pub fn is_guard(&self) -> bool {
        matches!(self, TraceOp::Guard(_, _))
    }

    /// Is this op a `MarkLoopHead` marker?
    pub fn is_loop_head(&self) -> bool {
        matches!(self, TraceOp::MarkLoopHead { .. })
    }

    /// Is this op a `MarkLoopBack` marker?
    pub fn is_loop_back(&self) -> bool {
        matches!(self, TraceOp::MarkLoopBack { .. })
    }

    /// Returns `Some(loop_id)` if this is a loop head marker.
    pub fn loop_head_id(&self) -> Option<u32> {
        match self {
            TraceOp::MarkLoopHead { loop_id } => Some(*loop_id),
            _ => None,
        }
    }

    /// Returns `Some(loop_id)` if this is a loop back marker.
    pub fn loop_back_id(&self) -> Option<u32> {
        match self {
            TraceOp::MarkLoopBack { loop_id } => Some(*loop_id),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_class_lookup() {
        assert_eq!(
            TraceOp::Add(SsaVar(0), SsaVar(1), SsaVar(2)).effect_class(),
            EffectClass::Pure
        );
        assert_eq!(
            TraceOp::Load(SsaVar(0), SsaVar(1), Offset(0)).effect_class(),
            EffectClass::ReadOnly
        );
        assert_eq!(
            TraceOp::Store(SsaVar(0), Offset(0), SsaVar(1)).effect_class(),
            EffectClass::RecoverableWrite
        );
        assert_eq!(
            TraceOp::Call(SsaVar(0), FuncId(7), vec![], EffectClass::Unrecoverable).effect_class(),
            EffectClass::Unrecoverable
        );
    }

    #[test]
    fn output_and_inputs() {
        let add = TraceOp::Add(SsaVar(3), SsaVar(1), SsaVar(2));
        assert_eq!(add.output(), Some(SsaVar(3)));
        assert_eq!(add.inputs(), vec![SsaVar(1), SsaVar(2)]);

        let ret = TraceOp::Return(SsaVar(9));
        assert_eq!(ret.output(), None);
        assert_eq!(ret.inputs(), vec![SsaVar(9)]);

        let store = TraceOp::Store(SsaVar(0), Offset(8), SsaVar(1));
        assert_eq!(store.output(), None);
        assert_eq!(store.inputs(), vec![SsaVar(0), SsaVar(1)]);

        let cst = TraceOp::ConstI64(SsaVar(0), 42);
        assert_eq!(cst.output(), Some(SsaVar(0)));
        assert!(cst.inputs().is_empty());
    }

    #[test]
    fn cmp_apply_truth_table() {
        assert!(CmpKind::Eq.apply_i64(3, 3));
        assert!(!CmpKind::Eq.apply_i64(3, 4));
        assert!(CmpKind::Ne.apply_i64(3, 4));
        assert!(CmpKind::Lt.apply_i64(1, 2));
        assert!(!CmpKind::Lt.apply_i64(2, 2));
        assert!(CmpKind::Le.apply_i64(2, 2));
        assert!(CmpKind::Gt.apply_i64(3, 2));
        assert!(CmpKind::Ge.apply_i64(3, 3));
    }

    #[test]
    fn loop_markers_are_pure_and_outputless() {
        let head = TraceOp::MarkLoopHead { loop_id: 3 };
        let back = TraceOp::MarkLoopBack { loop_id: 3 };
        assert!(head.is_loop_head());
        assert!(back.is_loop_back());
        assert_eq!(head.loop_head_id(), Some(3));
        assert_eq!(back.loop_back_id(), Some(3));
        assert_eq!(head.effect_class(), EffectClass::Pure);
        assert_eq!(back.effect_class(), EffectClass::Pure);
        assert_eq!(head.output(), None);
        assert_eq!(back.output(), None);
        assert!(head.inputs().is_empty());
        assert!(back.inputs().is_empty());
    }

    #[test]
    fn guard_is_pure() {
        let g = TraceOp::Guard(
            GuardKind::TypeCheck(SsaVar(1), ObservedType::I64),
            SsaVar(1),
        );
        assert!(g.is_guard());
        assert_eq!(g.effect_class(), EffectClass::Pure);
    }
}
