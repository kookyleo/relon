//! TraceBuffer -- linear op stream + guards + observed-type info.
//!
//! Used by the recorder during a hot-path observation pass and as the
//! input to the optimiser pipeline. After optimisation the buffer is
//! frozen into an [`OptimizedTrace`], which is what a v6-gamma
//! cranelift IR emitter will eventually consume (TODO: that emitter
//! lives outside this crate).
//!
//! The buffer keeps three side tables in sync with the linear op
//! stream:
//!
//! * `guards`     -- guard sites by `trace_pc`.
//! * `type_info`  -- observed concrete type per SSA var.
//! * `consts`     -- captured literal values per SSA var, used by the
//!   constant-folding pass.
//!
//! All three tables are indexed by SSA id (not by op position) so
//! optimiser passes can rewrite the op vector without invalidating
//! lookups.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::guard::GuardSite;
use crate::trace_ir::{ObservedType, SsaVar, TraceConst, TraceOp};

/// Linear, mutable trace under construction. Recorder appends ops;
/// optimiser passes mutate in place; finalisation produces an
/// [`OptimizedTrace`].
#[derive(Debug, Default, Clone)]
pub struct TraceBuffer {
    pub ops: Vec<TraceOp>,
    pub guards: Vec<GuardSite>,
    pub type_info: HashMap<SsaVar, ObservedType>,
    pub consts: HashMap<SsaVar, TraceConst>,
    next_ssa: u32,
}

impl TraceBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh SSA id. Must be used for every new value the
    /// recorder produces.
    pub fn fresh_ssa(&mut self) -> SsaVar {
        let v = SsaVar(self.next_ssa);
        self.next_ssa = self
            .next_ssa
            .checked_add(1)
            .expect("ssa id space exhausted");
        v
    }

    /// Append an op to the buffer. Returns the op's trace pc.
    pub fn append(&mut self, op: TraceOp) -> u32 {
        let pc = self.ops.len() as u32;
        // Keep `next_ssa` consistent if a recorder hand-rolls SSA ids.
        if let Some(out) = op.output() {
            if out != SsaVar::NONE && out.raw() >= self.next_ssa {
                self.next_ssa = out.raw() + 1;
            }
        }
        self.ops.push(op);
        pc
    }

    pub fn record_guard(&mut self, guard: GuardSite) {
        self.guards.push(guard);
    }

    pub fn record_type(&mut self, var: SsaVar, ty: ObservedType) {
        self.type_info.insert(var, ty);
    }

    pub fn record_const(&mut self, var: SsaVar, value: TraceConst) {
        self.consts.insert(var, value);
    }

    /// Convenience: how many ops the buffer currently holds.
    pub fn op_count(&self) -> usize {
        self.ops.len()
    }

    /// Convenience: how many guard sites the buffer currently holds.
    pub fn guard_count(&self) -> usize {
        self.guards.len()
    }

    /// Freeze into an immutable [`OptimizedTrace`]. Called after the
    /// optimiser pipeline finishes.
    pub fn into_optimized(self) -> OptimizedTrace {
        OptimizedTrace {
            ops: self.ops.into(),
            guards: self.guards.into(),
            type_info: self.type_info,
            consts: self.consts,
            ssa_high_water: self.next_ssa,
        }
    }
}

/// Immutable post-optimisation trace ready for cranelift lowering.
///
/// The serialisable subset (`guards`, `type_info`, `consts`,
/// `ssa_high_water`) round-trips via `bincode`; `ops` is not yet
/// serialisable because `TraceOp::Call` carries a `Vec<SsaVar>` and
/// the variant is intentionally non-Serialize until we pin the host
/// FFI shape (TODO v6-gamma).
#[derive(Debug, Clone)]
pub struct OptimizedTrace {
    pub ops: Box<[TraceOp]>,
    pub guards: Box<[GuardSite]>,
    pub type_info: HashMap<SsaVar, ObservedType>,
    pub consts: HashMap<SsaVar, TraceConst>,
    pub ssa_high_water: u32,
}

impl OptimizedTrace {
    pub fn op_count(&self) -> usize {
        self.ops.len()
    }

    pub fn guard_count(&self) -> usize {
        self.guards.len()
    }

    /// Side tables only, exposed so callers can round-trip the parts
    /// of an optimised trace that have stable serialisation today.
    pub fn side_tables(&self) -> SerializableSideTables {
        SerializableSideTables {
            guards: self.guards.to_vec(),
            type_info: self.type_info.clone(),
            consts: self.consts.clone(),
            ssa_high_water: self.ssa_high_water,
        }
    }
}

/// The serialisable subset of an optimised trace. Used by tests and
/// later by an on-disk trace cache (TODO v6-gamma+).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerializableSideTables {
    pub guards: Vec<GuardSite>,
    pub type_info: HashMap<SsaVar, ObservedType>,
    pub consts: HashMap<SsaVar, TraceConst>,
    pub ssa_high_water: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace_ir::Offset;

    #[test]
    fn fresh_ssa_is_monotonic() {
        let mut b = TraceBuffer::new();
        let a = b.fresh_ssa();
        let c = b.fresh_ssa();
        assert!(c.raw() > a.raw());
    }

    #[test]
    fn append_returns_pc_and_grows() {
        let mut b = TraceBuffer::new();
        let dst = b.fresh_ssa();
        let pc = b.append(TraceOp::ConstI64(dst, 100));
        assert_eq!(pc, 0);
        assert_eq!(b.op_count(), 1);
    }

    #[test]
    fn record_type_and_const_persist() {
        let mut b = TraceBuffer::new();
        b.record_type(SsaVar(7), ObservedType::I32);
        b.record_const(SsaVar(7), TraceConst::I32(42));
        assert_eq!(b.type_info[&SsaVar(7)], ObservedType::I32);
        assert_eq!(b.consts[&SsaVar(7)], TraceConst::I32(42));
    }

    #[test]
    fn into_optimized_freezes_buffer() {
        let mut b = TraceBuffer::new();
        let dst = b.fresh_ssa();
        b.append(TraceOp::ConstI32(dst, 1));
        let base = b.fresh_ssa();
        b.append(TraceOp::Store(base, Offset(0), dst));
        let t = b.into_optimized();
        assert_eq!(t.op_count(), 2);
        assert_eq!(t.guard_count(), 0);
    }
}
