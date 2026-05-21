//! TraceBuffer -- linear op stream + guards + observed-type info.
//!
//! Used by the recorder during a hot-path observation pass and as the
//! input to the optimiser pipeline. After optimisation the buffer is
//! frozen into an [`OptimizedTrace`], which is what a v6-gamma
//! cranelift IR emitter will eventually consume (TODO: that emitter
//! lives outside this crate).
//!
//! ## Side-table contract
//!
//! Beyond the linear `ops` vector and the position-anchored `guards`
//! list, the buffer carries five SSA-keyed side tables. All five obey
//! the same contract — documented here once and referenced from each
//! accessor / field rather than re-stated:
//!
//! * `type_info`               — observed concrete type per SSA
//!   (`SsaVar -> ObservedType`). Drives `TypeSpec` /
//!   `NoopTypeCheckElim`.
//! * `consts`                  — captured literal scalar value per SSA
//!   (`SsaVar -> TraceConst`). Drives `ConstFold`.
//! * `const_bytes` (F-D7-C)    — raw byte payload of a string-typed
//!   constant SSA (`SsaVar -> Vec<u8>`). Consumed by the emitter to
//!   switch `StrContains` lowering to an inline byte-scan.
//! * `str_payload` (F-D7-H)    — `(ptr_ssa, len_ssa)` pair holding
//!   the pre-loaded `StringRef::{ptr, len}` for a haystack SSA
//!   (`SsaVar -> (SsaVar, SsaVar)`). Lets LICM hoist the payload
//!   deref out of hot loops.
//! * `dict_entry_count_hints`  (F-D8-E.7) — static entry count for a
//!   dict-pointer SSA (`SsaVar -> u32`). Lets the emitter pick the
//!   fully-unrolled `DictLookupPrechecked` lowering.
//!
//! Shared invariants:
//!
//! 1. **Keyed by SSA, not op position.** Optimiser passes can splice
//!    or delete ops without invalidating lookups; an SSA id remains
//!    bound to the same observed value across rewrites.
//! 2. **Filled by the recorder, frozen at `into_optimized`.** The
//!    recorder is the only writer during op-append. Once
//!    `TraceBuffer::into_optimized` runs, the resulting
//!    [`OptimizedTrace`] exposes the same maps read-only.
//! 3. **Optimiser passes never invent new SSA values.** Passes may
//!    delete ops, swap one op for another with the same output SSA
//!    (`ConstFold`'s in-place rewrite), or insert ops whose outputs
//!    reuse already-allocated SSAs (`DictIcHoist`'s
//!    `DictShapeGuard` has no output). They MUST NOT allocate new
//!    SSA ids — if they did, the side-table keys would go stale
//!    relative to the post-pass op stream.
//! 4. **Stale keys are harmless.** When an op whose output SSA had
//!    a side-table entry gets deleted (e.g. dead-store elim drops
//!    a `Store`), the entry is intentionally left behind. Lookups
//!    are by-need and only consulted on surviving ops, so the dead
//!    entry never fires.
//! 5. **The `guards` vector** is anchored by `trace_pc` (op index),
//!    not SSA, and is therefore the exception — passes that delete
//!    or move ops MUST rebind `GuardSite::trace_pc` in lock-step
//!    (see `rebind_guard_pcs` in `optimizer::licm` /
//!    `noop_typecheck_elim`).
//!
//! See each `record_*` / `*_for` accessor for the per-table specifics.

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
    /// Observed concrete type per SSA. See the module-level
    /// "Side-table contract" section.
    pub type_info: HashMap<SsaVar, ObservedType>,
    /// Captured literal scalar value per SSA. See the module-level
    /// "Side-table contract" section.
    pub consts: HashMap<SsaVar, TraceConst>,
    /// F-D7-C: const-string side table. Maps an SSA var to the raw
    /// UTF-8 payload bytes of the string constant it carries.
    ///
    /// Populated by the recorder when it observes a literal string
    /// flowing into a `TraceOp::StrContains` (or sibling) needle slot;
    /// consumed by the emitter to switch `StrContains` lowering from a
    /// `call __relon_str_contains` round-trip into an inline cranelift
    /// byte-scan when the needle bytes are short (≤ 16). Storage is
    /// `Vec<u8>` rather than `Box<str>` because the emitter only needs
    /// byte equality, never UTF-8 inspection.
    ///
    /// The non-Copy payload is why this lives outside [`TraceConst`].
    ///
    /// Follows the module-level "Side-table contract" — SSA-keyed,
    /// recorder-written, frozen after `into_optimized`.
    pub const_bytes: HashMap<SsaVar, Vec<u8>>,
    /// F-D7-H: StringRef payload pre-load side table. Maps a haystack
    /// SSA (`*const StringRef`) to the `(ptr_ssa, len_ssa)` pair that
    /// holds the deref'd `(StringRef::ptr, StringRef::len)` payload.
    ///
    /// Populated by the recorder when it emits a `TraceOp::StrContains`:
    /// it appends `TraceOp::Load(ptr_ssa, haystack, Offset(0))` and
    /// `TraceOp::Load(len_ssa, haystack, Offset(8))` BEFORE the
    /// `StrContains` op, then registers the pair here. The emitter's
    /// `emit_str_contains` lowering consults this map and — when the
    /// pair is present — calls the inline scan with
    /// `HaystackHandle::Preloaded` so the per-iter `StringRef` deref
    /// disappears from the StrContains lowering (the loads are now
    /// independent ops in the trace stream).
    ///
    /// Why this matters: the F-D7-G LICM pass admits
    /// `TraceOp::Load { Offset(0|8) }` as hoistable when the loop
    /// body has no writes. By promoting the StringRef payload deref
    /// from a raw cranelift `builder.ins().load` (inside
    /// `load_string_ref_payload`, invisible to the optimiser) into
    /// real `TraceOp::Load` ops, LICM can move them to the loop
    /// preheader and the per-iter cost drops to just the inline
    /// memchr/byte scan.
    ///
    /// Follows the module-level "Side-table contract" — SSA-keyed,
    /// recorder-written, frozen after `into_optimized`.
    pub str_payload: HashMap<SsaVar, (SsaVar, SsaVar)>,
    /// F-D8-E.7: dict static-entry-count hint side table. Maps a
    /// dict_ptr SSA (the same SSA that flows into the matching
    /// `TraceOp::DictLookup` / `DictLookupPrechecked`) to the
    /// statically known number of entries in the dict literal.
    ///
    /// Populated by [`crate::RecorderState::emit_dict_lookup_with_hint`]
    /// when the source-level `Op::DictGetByStringKey` carried a
    /// `entry_count_hint`; consumed by the trace-emitter to switch
    /// the `DictLookupPrechecked` lowering from a data-driven scan
    /// loop into a fully-unrolled cmov chain when the entry count is
    /// small (≤ `MAX_INLINE_UNROLL` in dict_inline.rs).
    ///
    /// Why this is safe: the value is a static IR hint — the dict's
    /// `DictShapeGuard` runs upstream and verifies the shape hash,
    /// which by construction pins the key set (and hence the entry
    /// count). A mismatch deopts at the shape guard before the
    /// unrolled lookup reads anything from the entry table.
    ///
    /// Follows the module-level "Side-table contract" — SSA-keyed,
    /// recorder-written, frozen after `into_optimized`.
    pub dict_entry_count_hints: HashMap<SsaVar, u32>,
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

    /// Register `var`'s observed concrete type. Drives
    /// [`crate::optimizer::type_spec`] guard insertion and
    /// [`crate::optimizer::noop_typecheck_elim`] folding. See the
    /// module-level "Side-table contract" for invariants.
    pub fn record_type(&mut self, var: SsaVar, ty: ObservedType) {
        self.type_info.insert(var, ty);
    }

    /// Capture a literal scalar value flowing through `var`. Drives
    /// [`crate::optimizer::const_fold`]. See the module-level
    /// "Side-table contract" for invariants.
    pub fn record_const(&mut self, var: SsaVar, value: TraceConst) {
        self.consts.insert(var, value);
    }

    /// F-D7-C: record the raw byte payload of a string-typed constant
    /// flowing through `var`. Used by the recorder when it observes a
    /// literal string source so the emitter can specialise
    /// `TraceOp::StrContains` into an inline byte-scan; see
    /// [`Self::const_bytes`].
    pub fn record_const_bytes(&mut self, var: SsaVar, bytes: Vec<u8>) {
        self.const_bytes.insert(var, bytes);
    }

    /// F-D7-H: register that `haystack` (a `*const StringRef` SSA) has
    /// had its `(ptr, len)` payload pre-loaded into `(ptr_ssa, len_ssa)`
    /// by upstream `TraceOp::Load` ops at `Offset(0)` / `Offset(8)`.
    /// The emitter's `emit_str_contains` consults this table to skip
    /// the per-call `load_string_ref_payload` deref and feed the inline
    /// scan via `HaystackHandle::Preloaded` instead. See
    /// [`Self::str_payload`].
    pub fn record_str_payload(&mut self, haystack: SsaVar, ptr_ssa: SsaVar, len_ssa: SsaVar) {
        self.str_payload.insert(haystack, (ptr_ssa, len_ssa));
    }

    /// F-D8-E.7: stash the static `entry_count` hint for the dict
    /// whose pointer is held by `dict_ptr`. See
    /// [`Self::dict_entry_count_hints`].
    pub fn record_dict_entry_count_hint(&mut self, dict_ptr: SsaVar, entry_count: u32) {
        self.dict_entry_count_hints.insert(dict_ptr, entry_count);
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
            const_bytes: self.const_bytes,
            str_payload: self.str_payload,
            dict_entry_count_hints: self.dict_entry_count_hints,
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
    /// Frozen mirror of [`TraceBuffer::type_info`]. See the
    /// "Side-table contract" in the [`crate::buffer`] module docs.
    pub type_info: HashMap<SsaVar, ObservedType>,
    /// Frozen mirror of [`TraceBuffer::consts`]. See the
    /// "Side-table contract" in the [`crate::buffer`] module docs.
    pub consts: HashMap<SsaVar, TraceConst>,
    /// F-D7-C: const-string side table — see [`TraceBuffer::const_bytes`].
    pub const_bytes: HashMap<SsaVar, Vec<u8>>,
    /// F-D7-H: StringRef payload pre-load side table — see
    /// [`TraceBuffer::str_payload`].
    pub str_payload: HashMap<SsaVar, (SsaVar, SsaVar)>,
    /// F-D8-E.7: dict static-entry-count hint side table — see
    /// [`TraceBuffer::dict_entry_count_hints`].
    pub dict_entry_count_hints: HashMap<SsaVar, u32>,
    pub ssa_high_water: u32,
}

impl OptimizedTrace {
    pub fn op_count(&self) -> usize {
        self.ops.len()
    }

    pub fn guard_count(&self) -> usize {
        self.guards.len()
    }

    /// F-D7-C: lookup the const byte payload bound to `var`, if any.
    ///
    /// The emitter uses this on `TraceOp::StrContains` to decide
    /// between an inline byte-scan and the extern shim call.
    pub fn const_bytes_for(&self, var: SsaVar) -> Option<&[u8]> {
        self.const_bytes.get(&var).map(|v| v.as_slice())
    }

    /// F-D7-H: lookup the pre-loaded `(ptr_ssa, len_ssa)` pair bound to
    /// the haystack SSA `var`, if any. Populated by the recorder when
    /// it inserts the explicit `TraceOp::Load { Offset(0|8) }` ops
    /// upstream of a `TraceOp::StrContains`; consumed by the emitter
    /// to switch the inline scan onto `HaystackHandle::Preloaded`.
    pub fn str_payload_for(&self, var: SsaVar) -> Option<(SsaVar, SsaVar)> {
        self.str_payload.get(&var).copied()
    }

    /// F-D8-E.7: lookup the static dict `entry_count` hint bound to
    /// `dict_ptr`, if any. The emitter uses this on
    /// `TraceOp::DictLookupPrechecked` to switch between an unrolled
    /// `n`-cmov chain (when `n <= MAX_INLINE_UNROLL`) and the legacy
    /// data-driven scan loop.
    pub fn dict_entry_count_hint(&self, dict_ptr: SsaVar) -> Option<u32> {
        self.dict_entry_count_hints.get(&dict_ptr).copied()
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
