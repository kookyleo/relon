//! Stateful trace recorder.
//!
//! Holds the [`relon_trace_jit::TraceBuffer`] under construction plus
//! the sidetables the recorder needs across `record_op` calls:
//!
//! * an SSA allocator producing dense, monotone [`SsaVar`] ids,
//! * an `ir_to_ssa` map binding `LocalGet` / `LetGet` keys to the
//!   most-recently-stored SSA value,
//! * an observed-type map driving TypeCheck guard emission per the
//!   policy in `type_obs.rs`,
//! * an `aborted` slot the state machine flips on the first failed
//!   record_op so subsequent calls short-circuit without touching the
//!   buffer.
//!
//! The recorder is intentionally synchronous — it expects to be
//! driven by whatever loop the host uses to execute the Relon IR's
//! op stream (today the cranelift-generic backend, tomorrow a tiny
//! interpreter). Each call corresponds to one observed op execution.

use std::collections::HashMap;

use relon_ir::Op;
use relon_trace_jit::{
    EffectClass as TraceEffect, ExternalPc, GuardKind, GuardSite, LoopPhi, ObservedType, Offset,
    SsaVar, TraceBuffer, TraceOp,
};

use crate::abort::AbortReason;
use crate::lowering::{lower_op, LookupKind, LowerOutcome, OpLoweringContext, SubscriptKind};
use crate::type_obs::{classify_observation, TypeObsDecision};

/// Description of one loop-carried value the recorder should weave
/// through a [`TraceOp::MarkLoopHead`] / [`TraceOp::MarkLoopBack`]
/// φ-pair.
///
/// ε-M0: the higher-level walker driver builds one of these per
/// let-slot that gets re-assigned inside the loop body.
#[derive(Debug, Clone, Copy)]
pub struct LoopCarry {
    /// Pre-loop seed SSA — the value visible at the slot at the
    /// instant the loop header is entered.
    pub init: SsaVar,
    /// Observed type the recorder should remember for the fresh phi
    /// SSA. Drives the emitter's type-info side-table lookup at
    /// guard-emission time.
    pub ty: ObservedType,
    /// Optional source-side key the φ should be wired into the
    /// recorder's `ir_to_ssa` table for. When supplied, the
    /// recorder rebinds `ir_to_ssa[key] = phi_ssa` so subsequent
    /// `LetGet` / `LocalGet` lookups inside the loop body observe
    /// the φ SSA instead of the pre-loop SSA. Without this rebind
    /// the recorder would silently propagate the stale pre-loop
    /// SSA into every body op, defeating the φ pair.
    pub key: Option<crate::lowering::LookupKind>,
}

impl LoopCarry {
    pub fn new(init: SsaVar, ty: ObservedType) -> Self {
        Self {
            init,
            ty,
            key: None,
        }
    }

    /// Same as [`Self::new`] but also rebinds the recorder's
    /// `ir_to_ssa` table for `key` to the fresh φ SSA. Pass this when
    /// the φ is the loop-carried view of a let-slot / local-slot the
    /// body reads via `Op::LetGet` / `Op::LocalGet`.
    pub fn with_key(init: SsaVar, ty: ObservedType, key: crate::lowering::LookupKind) -> Self {
        Self {
            init,
            ty,
            key: Some(key),
        }
    }
}

/// Maximum number of ops a single trace may accumulate before the
/// recorder gives up. Hard-coded so the buffer's growth stays
/// predictable; the caller bumps this via [`RecorderState::with_capacity`]
/// when it has more headroom.
pub const DEFAULT_MAX_OPS: usize = 1024;

/// Result of a single [`RecorderState::record_op`] call. Carried as a
/// dedicated enum so callers can pattern-match without inspecting the
/// recorder's internal flags.
#[derive(Debug, Clone)]
pub enum RecordResult {
    /// The op was recorded; if it produced an SSA value, it's
    /// available as `value`.
    Ok { value: Option<SsaVar> },
    /// The op was recorded *and* the recorder appended a guard whose
    /// kind the caller should mirror in its own deopt-state
    /// bookkeeping. Returned in addition to `Ok` when a TypeCheck or
    /// arithmetic overflow guard fires.
    NeedsGuard {
        value: Option<SsaVar>,
        guard: GuardKind,
    },
    /// The op terminated the trace (e.g. `Op::Return`). The recorder
    /// is now in a finalisable state; calling `finalize()` returns
    /// the buffer.
    Terminated,
    /// The op caused the trace to abort. The recorder will return
    /// this on every subsequent call until reset.
    Abort(AbortReason),
}

/// Monotone SSA id allocator. Kept as its own type so unit tests can
/// drive it directly without spinning up a recorder; the
/// [`RecorderState`] embeds one and bumps it via [`SsaAllocator::alloc`].
#[derive(Debug, Default)]
pub struct SsaAllocator {
    next: u32,
}

impl SsaAllocator {
    /// Allocate the next SSA id. Panics if the id space is exhausted —
    /// the recorder's `TraceTooLong` budget hits long before u32::MAX.
    pub fn alloc(&mut self) -> SsaVar {
        let v = SsaVar(self.next);
        self.next = self
            .next
            .checked_add(1)
            .expect("trace SSA id space exhausted (>u32::MAX vars)");
        v
    }

    /// High-water mark — number of distinct SSA ids allocated so far.
    pub fn count(&self) -> u32 {
        self.next
    }

    /// Reset for a fresh trace. Used by long-running hosts that
    /// recycle the allocator across recordings.
    pub fn reset(&mut self) {
        self.next = 0;
    }
}

/// Recorder state machine. Holds the buffer being filled plus all the
/// sidetables the recording needs across op boundaries.
#[derive(Debug)]
pub struct RecorderState {
    buffer: TraceBuffer,
    ssa: SsaAllocator,
    ir_to_ssa: HashMap<LookupKind, SsaVar>,
    type_obs: HashMap<SsaVar, ObservedType>,
    /// SSA values that have already had a TypeCheck guard emitted —
    /// used by the de-dupe logic in `maybe_emit_type_guard` so the
    /// optimiser pipeline can rely on at most one TypeCheck per var
    /// before its own LICM pass runs.
    guarded_vars: HashMap<SsaVar, ObservedType>,
    /// Hot-loop nesting depth so the recorder can stamp matching
    /// `MarkLoopHead` / `MarkLoopBack` ids when it sees an
    /// `Op::Loop` / its closing boundary.
    loop_depth: u32,
    next_loop_id: u32,
    /// ε-M0: open `begin_loop` calls awaiting a matching `end_loop`.
    /// Each frame remembers the loop-id stamped at begin time so the
    /// matching back-edge stamps the same id. LIFO so nested loops
    /// pair correctly.
    open_loops: Vec<u32>,
    /// Maximum number of ops the buffer may collect before the
    /// recorder aborts with `TraceTooLong`.
    capacity: usize,
    /// Synthetic external PC the recorder stamps on each emitted
    /// `GuardSite`. The cranelift-generic backend supplies a real
    /// instruction pointer; until the IR walker wires that through,
    /// we use a monotone counter so the emitter's guard lookup keeps
    /// finding a matching site for every `TraceOp::Guard` it sees.
    next_external_pc: u64,
    /// v6-δ M2-C: mirror of the IR walker's operand stack in SSA
    /// space. Each `record_op` call pops `inputs.len()` SSAs from
    /// the top and (if the op produced a value) pushes the fresh
    /// `dst`. The deopt machinery captures this slice into the
    /// guard site's `ssa_stack_snapshot` so the bytecode-side
    /// `Snapshot(idx)` recipe can materialise mid-expression operand
    /// stacks at resume time without needing an extra translation
    /// table (per M2-B carry-over §10.1).
    ssa_stack: Vec<SsaVar>,
    /// Maximum observed operand-stack depth across the recording.
    /// Mainly diagnostic — emitter side allocates `value_stack_copy`
    /// boxes by the per-guard snapshot length, not this watermark.
    ssa_stack_high_water: u32,
    aborted: Option<AbortReason>,
    terminated: bool,
}

impl RecorderState {
    /// Build a recorder with the default op budget.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_OPS)
    }

    /// Build a recorder that aborts after `capacity` ops.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: TraceBuffer::new(),
            ssa: SsaAllocator::default(),
            ir_to_ssa: HashMap::new(),
            type_obs: HashMap::new(),
            guarded_vars: HashMap::new(),
            loop_depth: 0,
            next_loop_id: 0,
            open_loops: Vec::new(),
            capacity,
            next_external_pc: 0,
            ssa_stack: Vec::with_capacity(16),
            ssa_stack_high_water: 0,
            aborted: None,
            terminated: false,
        }
    }

    /// Snapshot of the current operand-stack (in SSA-id form), oldest
    /// first / top last. Used by deopt-snapshot construction (the
    /// emitter stamps `value_stack_copy` from this view at guard-fire
    /// time, indirectly via `GuardSite`).
    pub fn ssa_stack_snapshot(&self) -> Vec<SsaVar> {
        self.ssa_stack.clone()
    }

    /// High-water-mark of operand-stack depth observed so far.
    pub fn ssa_stack_high_water(&self) -> u32 {
        self.ssa_stack_high_water
    }

    /// Override the next-external-PC the recorder stamps on its
    /// emitted [`GuardSite`]s. Used by hosts (the cranelift-generic
    /// backend driving the IR walker) that have a real resume PC
    /// available; the default monotone counter just keeps each guard
    /// site's `external_pc` unique so the emitter / deopt path can
    /// still tell them apart.
    pub fn set_next_external_pc(&mut self, pc: u64) {
        self.next_external_pc = pc;
    }

    /// Emit a branch-direction guard against `cond_ssa`.
    ///
    /// v6-γ M5: the trace-recording IR walker decides at recording
    /// time which arm of an `Op::If` / `Op::Select` to follow. The
    /// emitter still needs a deopt site for the **other** arm so a
    /// future invocation with the opposite condition kicks back to
    /// the generic backend instead of executing recorded-arm code
    /// against the wrong branch.
    ///
    /// `taken_truthy` is informational — both `true` and `false`
    /// arms get the same `NotNull(cond_ssa)` guard; the recorder
    /// only follows the taken arm, so the guard's polarity is
    /// implicit in which path the walker actually recorded.
    ///
    /// Returns `None` if the recorder is already aborted / terminated;
    /// `Some(GuardKind::NotNull(_))` on the happy path.
    pub fn emit_branch_guard(
        &mut self,
        cond_ssa: SsaVar,
        _taken_truthy: bool,
    ) -> Option<GuardKind> {
        if self.aborted.is_some() || self.terminated {
            return None;
        }
        if cond_ssa == SsaVar::NONE {
            return None;
        }
        self.emit_guard(GuardKind::NotNull(cond_ssa))
    }

    /// ε-M0: emit a guard whose deopt polarity matches the **falsy
    /// path** of a branch — i.e. "deopt if `cond_ssa` becomes
    /// non-zero". Used by `Op::BrIf` recording when the walker's
    /// runtime cond is false (fall through, branch NOT taken).
    ///
    /// Emits a single [`GuardKind::IsZero`] op (the dual of
    /// `NotNull`); the emitter lowers it as `brif (cond != 0, deopt,
    /// ok)` — a single icmp + brif per iter rather than the earlier
    /// double-Cmp shape, matching the hand-built bench row's
    /// per-iter cost profile.
    ///
    /// Returns `Some(GuardKind::IsZero(cond_ssa))` on the happy path,
    /// `None` if the recorder is already aborted / terminated.
    pub fn emit_branch_falsy_guard(&mut self, cond_ssa: SsaVar) -> Option<GuardKind> {
        if self.aborted.is_some() || self.terminated {
            return None;
        }
        if cond_ssa == SsaVar::NONE {
            return None;
        }
        self.emit_guard(GuardKind::IsZero(cond_ssa))
    }

    /// True when the recorder has accepted an abort decision and is
    /// no longer touching the buffer.
    pub fn is_aborted(&self) -> bool {
        self.aborted.is_some()
    }

    /// Returns the sticky abort reason, if any. v6-γ M4 exposes this
    /// so the IR-walker driver can decide whether to fall back to
    /// the generic backend immediately or keep trying.
    pub fn abort_reason(&self) -> Option<AbortReason> {
        self.aborted
    }

    /// True when the recorder has observed a terminator
    /// (`Op::Return`) and the trace can be finalised.
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }

    /// Current op count. Mirrors `TraceBuffer::op_count` for
    /// convenience.
    pub fn op_count(&self) -> usize {
        self.buffer.op_count()
    }

    /// Read-only access to the buffer — useful in unit tests that
    /// want to introspect emitted ops without consuming the recorder.
    pub fn buffer(&self) -> &TraceBuffer {
        &self.buffer
    }

    /// F-D7-D: record a const-byte side-table entry for `var`. The
    /// trace emitter's `emit_str_contains` consults
    /// [`relon_trace_jit::OptimizedTrace::const_bytes_for`] to decide between the F-D7-C
    /// inline byte-scan lowering and the extern `__relon_str_contains`
    /// shim — pre-filling the side table from a const-needle observed
    /// at recording time keeps the trace on the inline fast path.
    ///
    /// The walker calls this after a successful `record_op` for an
    /// `Op::Call { fn_index = STDLIB_IDX_CONTAINS }` whose needle
    /// argument carries a host-side pointer the walker can safely
    /// dereference (`*const StringRef`).
    pub fn record_const_bytes(&mut self, var: SsaVar, bytes: Vec<u8>) {
        self.buffer.record_const_bytes(var, bytes);
    }

    /// Force an abort. Idempotent; the first reason wins so a
    /// downstream `UnsupportedOp` cannot mask an earlier
    /// `UnrecoverableEffect`.
    pub fn abort(&mut self, reason: AbortReason) {
        if self.aborted.is_none() {
            self.aborted = Some(reason);
        }
    }

    /// F-D8: emit a `TraceOp::ListGet` op into the trace buffer.
    ///
    /// Used by the IR walker / source-side lowering when it recognises
    /// a list indexed access pattern (`xs[i]` shape). The recorder
    /// allocates a fresh SSA dst, records its observed type as `I64`,
    /// updates the operand stack mirror so the next consumer sees the
    /// load result on top, and prepends a `BoundsCheck` guard to the
    /// buffer so the optimiser pipeline can lift it under LICM.
    ///
    /// `list_ssa` / `idx_ssa` are the SSAs the IR walker pushed for
    /// the list pointer and the index; both must already be bound in
    /// the recorder's `ir_to_ssa` table. The emitter side performs
    /// the actual bounds compare inline (`cmp idx < len; brif ok,
    /// deopt`), so the buffer-side `BoundsCheck` guard serves as the
    /// deopt site for the optimiser passes to anchor LICM decisions
    /// against — it does NOT emit a redundant runtime compare.
    ///
    /// Returns the destination SSA, or `None` if the recorder is in a
    /// sticky abort/terminated state.
    pub fn emit_list_get(&mut self, list_ssa: SsaVar, idx_ssa: SsaVar) -> Option<SsaVar> {
        if self.aborted.is_some() || self.terminated {
            return None;
        }
        if self.buffer.op_count() >= self.capacity {
            self.aborted = Some(AbortReason::TraceTooLong);
            return None;
        }
        let dst = self.ssa.alloc();
        // Bounds guard anchored on the list_ssa / idx_ssa pair. The
        // emitter folds it into an inline compare; the buffer-side
        // record gives the LICM pass a hoist target.
        self.append_guard_with_site(GuardKind::BoundsCheck(idx_ssa, list_ssa), idx_ssa);
        self.buffer.append(TraceOp::ListGet {
            dst,
            list_ptr: list_ssa,
            idx: idx_ssa,
        });
        // Mirror the operand-stack update: pop (list, idx), push dst.
        self.pop_inputs(&[list_ssa, idx_ssa]);
        self.push_ssa(dst);
        self.buffer.record_type(dst, ObservedType::I64);
        Some(dst)
    }

    /// F-D8: emit a `TraceOp::DictLookup` op into the trace buffer.
    ///
    /// Same contract as [`Self::emit_list_get`]: the IR walker pushes
    /// `(dict_ssa, key_ssa)` onto the operand stack, the recorder
    /// pops both, emits the lookup op, and pushes a fresh dst SSA.
    /// `shape_hash` is the recorder-time fingerprint of the keys the
    /// dict carries — computed via
    /// [`relon_trace_jit::fx_hash_bytes`] over the sorted key set so
    /// re-recordings of the same logical dict produce identical
    /// fingerprints.
    ///
    /// The emitter lowers this to a single `call_indirect` to the
    /// `__relon_trace_dict_lookup` helper plus a sentinel-compare
    /// brif into the shared deopt block on IC miss. No
    /// `TraceOp::Guard` is appended here — the deopt encoding lives
    /// inside the host helper's return value.
    pub fn emit_dict_lookup(
        &mut self,
        dict_ssa: SsaVar,
        key_ssa: SsaVar,
        shape_hash: u64,
    ) -> Option<SsaVar> {
        self.emit_dict_lookup_with_hint(dict_ssa, key_ssa, shape_hash, None)
    }

    /// F-D8-E.7 variant of [`Self::emit_dict_lookup`] that also stamps
    /// the dict's static `entry_count` (when the source-level IR
    /// carried one via `Op::DictGetByStringKey::entry_count_hint`) into
    /// the trace buffer's `dict_entry_count_hints` side table.
    ///
    /// Keying on `dict_ssa` lets the optimizer's `dict_ic_hoist` pass
    /// rewrite the dict-pointer use site without losing the hint —
    /// `DictShapeGuard` + `DictLookupPrechecked` share the same
    /// dict_ptr SSA. The trace-emitter then queries the side table
    /// from `emit_dict_lookup_prechecked` and switches the inline
    /// scan into a fully-unrolled cmov chain when `entry_count_hint
    /// <= MAX_INLINE_UNROLL`.
    pub fn emit_dict_lookup_with_hint(
        &mut self,
        dict_ssa: SsaVar,
        key_ssa: SsaVar,
        shape_hash: u64,
        entry_count_hint: Option<u32>,
    ) -> Option<SsaVar> {
        if self.aborted.is_some() || self.terminated {
            return None;
        }
        if self.buffer.op_count() >= self.capacity {
            self.aborted = Some(AbortReason::TraceTooLong);
            return None;
        }
        let dst = self.ssa.alloc();
        self.buffer.append(TraceOp::DictLookup {
            dst,
            dict_ptr: dict_ssa,
            key_ptr: key_ssa,
            shape_hash,
        });
        self.pop_inputs(&[dict_ssa, key_ssa]);
        self.push_ssa(dst);
        self.buffer.record_type(dst, ObservedType::I64);
        if let Some(n) = entry_count_hint {
            self.buffer.record_dict_entry_count_hint(dict_ssa, n);
        }
        Some(dst)
    }

    /// F-D7-B: emit a `TraceOp::StrConcat(dst, lhs, rhs)` op into the
    /// trace buffer.
    ///
    /// Used by an AST-level / IR-walking driver that has recognised the
    /// `String + String` shape directly (the parser/IR lowering pipeline
    /// does not yet produce `Op::Add(IrType::String)` for real source —
    /// see the comment near `crate::lowering::lower_str_add`). Both
    /// operand SSAs must already be bound in the recorder's tables.
    ///
    /// Recorder state side-effects:
    /// - Appends two `NotNull` guards (one per operand) so the trace
    ///   deopts cleanly rather than returning a null `StringRef`.
    /// - Appends `TraceOp::StrConcat(dst, lhs, rhs)` and rebinds the
    ///   operand-stack mirror (pops both operands, pushes the dst).
    /// - Records the dst's observed type as `ObservedType::Ptr`
    ///   (`StringRef *` carries through the trace as a pointer slot).
    ///
    /// Returns the destination SSA, or `None` if the recorder is in a
    /// sticky abort / terminated state.
    pub fn emit_str_concat(&mut self, lhs: SsaVar, rhs: SsaVar) -> Option<SsaVar> {
        if self.aborted.is_some() || self.terminated {
            return None;
        }
        if self.buffer.op_count() >= self.capacity {
            self.aborted = Some(AbortReason::TraceTooLong);
            return None;
        }
        // Guards before the op so a future invocation with a null
        // operand deopts at the guard rather than crashing inside the
        // shim. Mirror the order the lowering rule uses.
        self.emit_guard(GuardKind::NotNull(lhs));
        self.emit_guard(GuardKind::NotNull(rhs));
        let dst = self.ssa.alloc();
        self.buffer.append(TraceOp::StrConcat(dst, lhs, rhs));
        self.pop_inputs(&[rhs, lhs]);
        self.push_ssa(dst);
        self.buffer.record_type(dst, ObservedType::Ptr);
        Some(dst)
    }

    /// F-D7-B: emit a `TraceOp::StrContains(dst, haystack, needle)` op
    /// into the trace buffer, optionally pre-filling the needle's
    /// const-byte payload so the F-D7-C inline emit path can specialise
    /// the trace into a literal byte-scan instead of a
    /// `__relon_str_contains` shim call.
    ///
    /// Source-side wiring: a future AST-walker integration recognises
    /// `s.contains(needle)` and (when `needle` is a literal string node)
    /// passes `needle_bytes = Some(literal_bytes)`. The recorder
    /// forwards the bytes into the buffer's `const_bytes` side table —
    /// the same table `relon_trace_emitter::emit_str_contains_inline`
    /// consults at lowering time. Variable-length needles still call
    /// through the extern shim.
    ///
    /// Recorder state side-effects:
    /// - Appends a `NotNull` guard on the haystack (mirror of the
    ///   `STDLIB_IDX_CONTAINS` lowering rule).
    /// - Appends `TraceOp::StrContains(dst, haystack, needle)` and
    ///   updates the operand-stack mirror (pops both operands, pushes
    ///   the dst).
    /// - Records the dst's observed type as `ObservedType::Bool`
    ///   (the shim returns `i32 in {0,1}`).
    /// - Optionally records the const-byte payload via
    ///   [`TraceBuffer::record_const_bytes`].
    ///
    /// Returns the destination SSA, or `None` if the recorder is in a
    /// sticky abort / terminated state.
    pub fn emit_str_contains(
        &mut self,
        haystack: SsaVar,
        needle: SsaVar,
        needle_bytes: Option<Vec<u8>>,
    ) -> Option<SsaVar> {
        if self.aborted.is_some() || self.terminated {
            return None;
        }
        if self.buffer.op_count() >= self.capacity {
            self.aborted = Some(AbortReason::TraceTooLong);
            return None;
        }
        // F-D7 lowering policy: guard the haystack but not the needle.
        // The inline / shim paths both tolerate a zero-length needle
        // (return `true`) — emitting a `NotNull(needle)` here would
        // surface a spurious deopt on the empty-string case.
        self.emit_guard(GuardKind::NotNull(haystack));
        let dst = self.ssa.alloc();
        // F-D7-H: pre-load the StringRef `(ptr, len)` payload as real
        // `TraceOp::Load { Offset(0|8) }` ops upstream of the
        // `StrContains` so the F-D7-G LICM pass can hoist them when
        // `haystack` is loop-invariant. The emitter routes the inline
        // scan through `HaystackHandle::Preloaded` whenever the
        // `str_payload` side-table carries an entry for `haystack`.
        self.inject_str_payload_loads(haystack);
        self.buffer
            .append(TraceOp::StrContains(dst, haystack, needle));
        self.pop_inputs(&[needle, haystack]);
        self.push_ssa(dst);
        self.buffer.record_type(dst, ObservedType::Bool);
        // F-D7-C inline-emit hook: with a known-constant needle the
        // emitter can lower into a tight cranelift byte-scan, saving
        // the C ABI crossing. The recorder populates the side table
        // here so the optimiser pipeline and emitter both see the
        // same view.
        if let Some(bytes) = needle_bytes {
            self.buffer.record_const_bytes(needle, bytes);
        }
        Some(dst)
    }

    /// ε-M0: open a loop frame and emit a `TraceOp::MarkLoopHead`
    /// carrying one φ pair per [`LoopCarry`].
    ///
    /// Returns the freshly-allocated φ SSAs in the same order as
    /// `carries`. The caller (the IR walker) is expected to update
    /// its let-slot map so subsequent `LetGet` reads observe the φ
    /// SSAs while the body is being recorded.
    ///
    /// Recorder state side-effects:
    /// - Allocates one fresh SSA per carry; records its observed type
    ///   in the buffer's type_info table so the emitter's guard
    ///   predicate builder can resolve `TypeCheck(phi, ty)`.
    /// - Appends one `TraceOp::MarkLoopHead { loop_id, phis }` op.
    /// - Pushes the loop_id onto the `open_loops` stack so the
    ///   matching `end_loop` stamps the correct id.
    /// - Bumps `loop_depth` so the diagnostic counter mirrors nesting
    ///   (the historical depth gauge still works).
    ///
    /// If the recorder is already aborted / terminated this is a
    /// silent no-op and returns an empty vec — the caller's walker
    /// already saw the sticky state on the prior op.
    pub fn begin_loop(&mut self, carries: &[LoopCarry]) -> Vec<SsaVar> {
        if self.aborted.is_some() || self.terminated {
            return Vec::new();
        }
        if self.buffer.op_count() >= self.capacity {
            self.aborted = Some(AbortReason::TraceTooLong);
            return Vec::new();
        }

        let loop_id = self.next_loop_id;
        self.next_loop_id += 1;
        self.loop_depth = self.loop_depth.saturating_add(1);

        let mut phis: Vec<LoopPhi> = Vec::with_capacity(carries.len());
        let mut phi_ssas: Vec<SsaVar> = Vec::with_capacity(carries.len());
        for c in carries {
            let phi = self.ssa.alloc();
            // Persist the φ's observed type so the emitter's
            // type-info side-table resolves `TypeCheck(phi, ty)`
            // predicates at install time. We intentionally do NOT
            // seed `type_obs` here — that map drives the "first
            // observed vs re-observed" guard-emission policy. By
            // letting the first body `LetGet(phi)` hit the
            // `FirstSeen` branch, we avoid emitting a redundant
            // `Guard(TypeCheck(phi, ty))` brif on every loop iter
            // (each such guard is `brif (iconst 1), ok, deopt` —
            // technically a no-op once cranelift folds, but it adds
            // pressure to the per-iter machine code).
            self.buffer.record_type(phi, c.ty);
            // ε-M0 critical rebind: if the carry has a source-side
            // `ir_to_ssa` key, make subsequent body `LetGet` /
            // `LocalGet` ops resolve to the φ SSA rather than the
            // pre-loop SSA. Without this the recorder lowers every
            // body read of a carried slot to the stale pre-loop SSA,
            // defeating the φ pair (the SSA never changes, LICM
            // hoists the entire body, the trace deopts on the first
            // iter because the body's overflow / branch guards see
            // recording-time values that don't match runtime).
            if let Some(key) = c.key {
                self.ir_to_ssa.insert(key, phi);
                // Re-arm TypeCheck guard emission for the φ SSA so
                // the next observation can emit a fresh guard if the
                // type drifts; without clearing `guarded_vars` the
                // φ would carry the seed's type-guard suppression.
                self.guarded_vars.remove(&phi);
            }
            phis.push(LoopPhi::new(c.init, phi));
            phi_ssas.push(phi);
        }

        self.buffer.append(TraceOp::MarkLoopHead { loop_id, phis });
        self.open_loops.push(loop_id);
        phi_ssas
    }

    /// ε-M0: close the most-recently-opened loop frame with the
    /// supplied back-edge `next_values` and emit the matching
    /// `TraceOp::MarkLoopBack`. The next-values vec must be the same
    /// length and order as the `carries` passed to the corresponding
    /// `begin_loop`.
    ///
    /// Returns `true` on success, `false` if the recorder is in a
    /// sticky aborted/terminated state or there is no open loop frame.
    /// Mismatched length aborts the recorder with `UnsupportedOp`.
    pub fn end_loop(&mut self, next_values: &[SsaVar]) -> bool {
        if self.aborted.is_some() || self.terminated {
            return false;
        }
        let loop_id = match self.open_loops.pop() {
            Some(id) => id,
            None => {
                self.aborted = Some(AbortReason::UnsupportedOp("LoopBackWithoutHead"));
                return false;
            }
        };
        self.loop_depth = self.loop_depth.saturating_sub(1);
        if self.buffer.op_count() >= self.capacity {
            self.aborted = Some(AbortReason::TraceTooLong);
            return false;
        }
        self.buffer.append(TraceOp::MarkLoopBack {
            loop_id,
            next_values: next_values.to_vec(),
        });
        true
    }

    /// Drop the recorder, returning the underlying [`TraceBuffer`] iff
    /// no abort was recorded. The returned buffer may then be fed
    /// through the optimiser pipeline.
    pub fn finalize(self) -> Result<TraceBuffer, AbortReason> {
        if let Some(reason) = self.aborted {
            Err(reason)
        } else {
            Ok(self.buffer)
        }
    }

    /// Record one Relon IR op.
    ///
    /// `inputs` are the SSA ids of the values currently sitting on
    /// the host's operand stack in push order (last pushed first), as
    /// observed by the cranelift-generic backend's runtime. `observed`
    /// is the runtime [`ObservedType`] of the value the op produces;
    /// the recorder uses it to drive its TypeCheck guard policy.
    ///
    /// Returns the SSA id of the emitted value (if any) wrapped in
    /// the appropriate [`RecordResult`] variant.
    ///
    /// v6-δ M2-B: every `record_op` call advances `next_external_pc`
    /// by 1 **before** invoking the lowering rule. The bytecode
    /// compiler's compile pass uses the same per-op monotonic counter
    /// (`ir_pc_next`); aligning them lets partial-resume route a
    /// guard's `external_pc` straight into the matching bytecode
    /// index without an extra translation table. Hosts that prefer
    /// the legacy guard-only scheme can still
    /// [`Self::set_next_external_pc`] manually before each
    /// `record_op` call to override.
    pub fn record_op(
        &mut self,
        op: &Op,
        inputs: &[SsaVar],
        observed: Option<ObservedType>,
    ) -> RecordResult {
        // Short-circuit terminated / aborted state without touching
        // the buffer. The recorder is a sticky state machine.
        if let Some(reason) = self.aborted {
            return RecordResult::Abort(reason);
        }
        if self.terminated {
            return RecordResult::Abort(AbortReason::UnsupportedOp("PostTerminator"));
        }
        if self.buffer.op_count() >= self.capacity {
            self.aborted = Some(AbortReason::TraceTooLong);
            return RecordResult::Abort(AbortReason::TraceTooLong);
        }

        // v6-δ M2-B: bump the IR-side PC so any guard the lowering
        // emits for THIS op carries the per-op counter the bytecode
        // compile pass uses. Mirrors the bytecode compiler's
        // `next_pc()` (`ir_pc_next += 1`) increment-then-stamp order.
        self.next_external_pc = self.next_external_pc.wrapping_add(1);

        let fresh_dst = self.ssa.alloc();
        let cx = self.build_lowering_cx(op, inputs, fresh_dst);
        let outcome = lower_op(op, cx);
        self.apply_outcome(op, outcome, inputs, fresh_dst, observed)
    }

    /// Override the effect class the lowering rule applies to the
    /// next `Op::Call`. Used by hosts that have classified the
    /// callee out-of-band (e.g. via a per-stdlib effect table).
    pub fn record_op_with_call_effect(
        &mut self,
        op: &Op,
        inputs: &[SsaVar],
        observed: Option<ObservedType>,
        call_effect: TraceEffect,
    ) -> RecordResult {
        if let Some(reason) = self.aborted {
            return RecordResult::Abort(reason);
        }
        if self.terminated {
            return RecordResult::Abort(AbortReason::UnsupportedOp("PostTerminator"));
        }
        if self.buffer.op_count() >= self.capacity {
            self.aborted = Some(AbortReason::TraceTooLong);
            return RecordResult::Abort(AbortReason::TraceTooLong);
        }

        let fresh_dst = self.ssa.alloc();
        let cx = self
            .build_lowering_cx(op, inputs, fresh_dst)
            .with_call_effect_override(call_effect);
        let outcome = lower_op(op, cx);
        self.apply_outcome(op, outcome, inputs, fresh_dst, observed)
    }

    fn build_lowering_cx<'a>(
        &self,
        _op: &Op,
        inputs: &'a [SsaVar],
        fresh_dst: SsaVar,
    ) -> OpLoweringContext<'a> {
        OpLoweringContext::new(inputs, fresh_dst)
    }

    fn apply_outcome(
        &mut self,
        op: &Op,
        outcome: LowerOutcome,
        inputs: &[SsaVar],
        fresh_dst: SsaVar,
        observed: Option<ObservedType>,
    ) -> RecordResult {
        match outcome {
            LowerOutcome::Emit {
                op: trace_op,
                dst,
                guards_before,
                guards_after,
                effect: _,
            } => {
                for g in guards_before {
                    self.emit_guard(g);
                }
                // F-D7-H: when the recorder is about to emit a
                // `TraceOp::StrContains(_, haystack, _)`, also emit
                // two `TraceOp::Load` ops reading the StringRef
                // `(ptr, len)` payload upstream — and register the
                // resulting SSA pair in the buffer's `str_payload`
                // side-table. The emitter's `emit_str_contains` then
                // consults the table to skip the per-call
                // `load_string_ref_payload` raw deref and route the
                // inline scan through `HaystackHandle::Preloaded`.
                //
                // Why two real `TraceOp::Load` ops instead of the raw
                // cranelift `builder.ins().load` the old path used:
                // F-D7-G LICM admits `TraceOp::Load { Offset(0|8) }`
                // as hoistable when the loop body contains no writes
                // (the W4 hot loop's case — no `Store`, no
                // `RecoverableWrite`). Moving the loads from "hidden
                // cranelift IR inside the StrContains emit" into "real
                // TraceOps in the OptimizedTrace stream" gives LICM a
                // chance to actually hoist them.
                self.maybe_inject_str_contains_loads(&trace_op);
                self.buffer.append(trace_op);
                // v6-δ M2-C: update operand-stack mirror BEFORE the
                // `guards_after` emit so a guard emitted for THIS op
                // sees the post-op stack (dst already pushed). The
                // bytecode-side `Snapshot(idx)` recipe expects to see
                // the value the trapping op was about to produce on
                // top of the stack — matching the way the bytecode
                // compile pass arranges its `current_stack` per op.
                self.pop_inputs(inputs);
                if let Some(d) = dst {
                    self.push_ssa(d);
                }
                let mut surfaced_guard = None;
                for g in guards_after {
                    let kind = self.emit_guard(g);
                    surfaced_guard = surfaced_guard.or(kind);
                }
                if let Some(d) = dst {
                    if let Some(ty) = observed {
                        if let Some(g) = self.maybe_emit_type_guard(d, ty) {
                            surfaced_guard = surfaced_guard.or(Some(g));
                        }
                    }
                }
                if let Some(g) = surfaced_guard {
                    RecordResult::NeedsGuard {
                        value: dst,
                        guard: g,
                    }
                } else {
                    RecordResult::Ok { value: dst }
                }
            }
            LowerOutcome::SideEffectOnly { rebind } => {
                // For LocalSet / LetSet we re-bind the slot to the
                // SSA the caller indicated.
                if let Some(ssa) = rebind {
                    if let Some(key) = local_or_let_key(op) {
                        self.ir_to_ssa.insert(key, ssa);
                    }
                }
                // SideEffectOnly ops still consume operand-stack
                // entries (e.g. `LocalSet`, `LetSet` pop the stored
                // value, `Br` may pop nothing depending on lowering
                // semantics); the recorder's caller supplies the
                // popped SSA ids in `inputs`. Mirror the pop so the
                // ssa_stack stays in sync with the IR walker's view.
                self.pop_inputs(inputs);
                RecordResult::Ok { value: None }
            }
            LowerOutcome::Lookup { kind, ty_hint } => {
                let (var, first_seen) = if let Some(existing) = self.ir_to_ssa.get(&kind).copied() {
                    (existing, false)
                } else {
                    // First time this slot is read — seed the map
                    // with `fresh_dst` so subsequent reads alias the
                    // same SSA id.
                    //
                    // F-D8-D: do NOT pre-stamp `type_obs[fresh_dst] = ty_hint`
                    // here. The lower_op rule's `ty_hint` is a static
                    // best-effort (e.g. `Op::LocalGet` always hints I32
                    // because the wasm-handshake slots are i32). The
                    // walker's actual observation (from
                    // `step_local_get`'s `observed_from_ir`) is the
                    // authoritative type; the maybe_emit_type_guard
                    // call a few lines down stamps it via the same
                    // FirstSeen path used by `Emit`. Pre-seeding the
                    // i32 hint here would turn the first observation
                    // of an i64-arg LocalGet (pointer payload, IrType
                    // ListInt/etc.) into a TypeObsDecision::Mismatch
                    // and force an avoidable abort.
                    let _ = ty_hint;
                    self.ir_to_ssa.insert(kind, fresh_dst);
                    // F-D7-D + F-D8-D merge: the walker's `observed`
                    // is the authoritative first observation when it
                    // knows the real `IrType` (e.g. `String` for a
                    // host-pointer arg riding through
                    // `TraceRecordingEvaluator::args`). Stamp it
                    // straight onto `type_obs` so subsequent type-
                    // guard decisions see the right shape from the
                    // start. When `observed` is `None` we deliberately
                    // skip the insert so the `FirstSeen` path lower
                    // down picks up the runtime value and avoids the
                    // stale `I32` seed F-D8-D documented above.
                    if let Some(obs) = observed {
                        self.type_obs.insert(fresh_dst, obs);
                    }
                    (fresh_dst, true)
                };
                // Lookup pushes one value onto the operand stack.
                self.push_ssa(var);
                // v6-δ M1: emit `TraceOp::LocalGet` on first observation
                // of a `LookupKind::Local(idx)` so the emitter can
                // materialise the SSA value from the cranelift entry
                // helper's packed-arg pointer. Without this, an arith
                // op referencing the LocalGet'd SSA fails to install
                // with `EmitError::UnboundSsa`. Let-slots are bound by
                // `Op::LetSet` instead (no entry-arg materialisation
                // needed), so we only emit for `Local`.
                if first_seen {
                    if let LookupKind::Local(slot_idx) = kind {
                        self.buffer.append(TraceOp::LocalGet(var, slot_idx));
                    }
                }
                if let Some(ty) = observed.or(Some(ty_hint)) {
                    if let Some(g) = self.maybe_emit_type_guard(var, ty) {
                        return RecordResult::NeedsGuard {
                            value: Some(var),
                            guard: g,
                        };
                    }
                }
                RecordResult::Ok { value: Some(var) }
            }
            LowerOutcome::Terminate { op: trace_op } => {
                self.buffer.append(trace_op);
                // Terminator (`Return`) pops its return value off the
                // operand stack. Mirror that so the final ssa_stack
                // depth reads as 0 — diagnostic-friendly.
                self.pop_inputs(inputs);
                self.terminated = true;
                RecordResult::Terminated
            }
            LowerOutcome::SubscriptDispatch { kind, ty_hint } => {
                // F-D8-B: dict / list subscript ops surface here.
                // `apply_outcome` already received the popped SSA window
                // via `inputs` (length checked in lower_op). Top-of-stack
                // is at `inputs[0]`, dict/list pointer at `inputs[1]`.
                // We dispatch into the dedicated helper so the buffer
                // sees a `TraceOp::DictLookup` / `TraceOp::ListGet` plus
                // the bounds / IC guards the helpers stamp.
                //
                // The `fresh_dst` allocated by `record_op` is unused
                // here — the helpers allocate their own dst SSA so the
                // operand-stack mirror update lines up with the ssa-id
                // the dst gets. We log this so future readers don't
                // chase the discrepancy.
                let _unused = fresh_dst;
                if inputs.len() < 2 {
                    self.aborted = Some(AbortReason::UnsupportedOp("SubscriptUnderflow"));
                    return RecordResult::Abort(AbortReason::UnsupportedOp("SubscriptUnderflow"));
                }
                let top = inputs[0];
                let container = inputs[1];
                let dst = match kind {
                    SubscriptKind::ListGet => self.emit_list_get(container, top),
                    SubscriptKind::DictLookup {
                        shape_hash,
                        entry_count_hint,
                    } => self.emit_dict_lookup_with_hint(
                        container,
                        top,
                        shape_hash,
                        entry_count_hint,
                    ),
                };
                let Some(d) = dst else {
                    // emit_* short-circuits on aborted / terminated;
                    // both update `self.aborted` themselves, so we just
                    // surface whatever reason landed.
                    let reason = self.aborted.unwrap_or(AbortReason::TraceTooLong);
                    return RecordResult::Abort(reason);
                };
                // Override the helper-time observed type with the
                // static hint from the IR op so a follow-up TypeCheck
                // resolves against the correct width (the helper
                // defaults to I64; non-I64 lists are rejected upstream
                // but dict value types may still differ).
                self.buffer.record_type(d, ty_hint);
                // Surface a TypeCheck guard if observed disagrees with
                // the recorder's running expectation.
                if let Some(ty) = observed {
                    if let Some(g) = self.maybe_emit_type_guard(d, ty) {
                        return RecordResult::NeedsGuard {
                            value: Some(d),
                            guard: g,
                        };
                    }
                }
                RecordResult::Ok { value: Some(d) }
            }
            LowerOutcome::LoopMarker { op: marker_op } => {
                let marker = match marker_op {
                    TraceOp::MarkLoopHead { phis, .. } => {
                        let id = self.next_loop_id;
                        self.next_loop_id += 1;
                        self.loop_depth = self.loop_depth.saturating_add(1);
                        TraceOp::MarkLoopHead { loop_id: id, phis }
                    }
                    other => other,
                };
                self.buffer.append(marker);
                let _ = inputs;
                // Loop markers don't push or pop operand-stack values.
                RecordResult::Ok { value: None }
            }
            LowerOutcome::Abort(reason) => {
                self.aborted = Some(reason);
                RecordResult::Abort(reason)
            }
        }
    }

    /// Pop `inputs.len()` entries off the ssa_stack mirror. Used by
    /// every LowerOutcome arm except `Lookup`/`LoopMarker`. Tolerates
    /// underflow because not every `record_op` call originates from
    /// the production IR walker — direct test drivers may feed
    /// synthetic SSA inputs that don't correspond to operand-stack
    /// pushes. In production those callers run through
    /// `TraceRecordingEvaluator` which pops the operand stack first
    /// and feeds the popped SSAs as `inputs`, so the mirror stays in
    /// sync.
    ///
    /// F-D7-H: pattern-match a freshly-built `TraceOp` and, when it is
    /// a `TraceOp::StrContains(_, haystack, _)`, prepend two
    /// `TraceOp::Load` ops reading the StringRef payload from
    /// `haystack` so the F-D7-G LICM pass can hoist them.
    ///
    /// Called from `apply_outcome`'s `Emit` arm — i.e. whenever the
    /// `lower_string_call` `STDLIB_IDX_CONTAINS` rule fires through the
    /// generic lowering path. The direct
    /// [`Self::emit_str_contains`] entry calls
    /// [`Self::inject_str_payload_loads`] explicitly so both call
    /// paths produce the same op stream.
    fn maybe_inject_str_contains_loads(&mut self, op: &TraceOp) {
        if let TraceOp::StrContains(_, haystack, _) = *op {
            self.inject_str_payload_loads(haystack);
        }
    }

    /// F-D7-H: append the two `TraceOp::Load { Offset(0|8) }` reads
    /// for the StringRef payload of `haystack` and record the
    /// resulting SSA pair in the buffer's `str_payload` side-table.
    ///
    /// Idempotent on a per-(buffer, haystack) basis: if a prior call
    /// already injected the loads for this haystack we skip — the
    /// emitter's `emit_str_contains` will pick up the existing pair.
    /// This keeps the load count bounded when the same haystack
    /// appears in multiple `StrContains` calls in one trace (the W4
    /// hot loop's pattern, where the haystack SSA is loop-invariant
    /// and consumed once per iter).
    ///
    /// The op count gate matches the rest of the recorder — if we
    /// would push past the per-trace `capacity` ceiling we set
    /// `aborted = TraceTooLong` and bail without touching the buffer.
    fn inject_str_payload_loads(&mut self, haystack: SsaVar) {
        if self.aborted.is_some() || self.terminated {
            return;
        }
        if self.buffer.str_payload.contains_key(&haystack) {
            return;
        }
        // We are about to push two TraceOp::Load ops; if either would
        // overflow `capacity`, abort cleanly. The emitter falls back
        // to `HaystackHandle::Raw` when the side-table entry is
        // missing, so a bail here just degrades to the previous
        // per-iter deref path — no correctness fault.
        if self.buffer.op_count() + 2 > self.capacity {
            self.aborted = Some(AbortReason::TraceTooLong);
            return;
        }
        let ptr_ssa = self.ssa.alloc();
        let len_ssa = self.ssa.alloc();
        self.buffer
            .append(TraceOp::Load(ptr_ssa, haystack, Offset(0)));
        self.buffer
            .append(TraceOp::Load(len_ssa, haystack, Offset(8)));
        // Both reads return i64-typed values (host StringRef is
        // `{ ptr: *const u8, len: usize }`, both 8 bytes on x86_64 /
        // aarch64). Stamp the type into the buffer so any future
        // optimiser pass that walks `type_info` sees a consistent
        // view, even though the emitter's `emit_load` always
        // materialises an I64 cranelift value regardless.
        self.buffer.record_type(ptr_ssa, ObservedType::I64);
        self.buffer.record_type(len_ssa, ObservedType::I64);
        self.buffer.record_str_payload(haystack, ptr_ssa, len_ssa);
    }

    /// Underflow is **silent** (the mirror simply clears) so unit
    /// tests that exercise lowering rules with synthetic inputs don't
    /// panic. The mirror's view is best-effort — guard sites stamped
    /// during such tests carry an under-populated snapshot, which is
    /// equivalent to the M2-B "empty value_stack_copy" behaviour.
    fn pop_inputs(&mut self, inputs: &[SsaVar]) {
        let n = inputs.len();
        let drain_from = self.ssa_stack.len().saturating_sub(n);
        self.ssa_stack.truncate(drain_from);
    }

    /// Push one SSA value onto the ssa_stack mirror and update the
    /// high-water mark.
    fn push_ssa(&mut self, v: SsaVar) {
        self.ssa_stack.push(v);
        if (self.ssa_stack.len() as u32) > self.ssa_stack_high_water {
            self.ssa_stack_high_water = self.ssa_stack.len() as u32;
        }
    }

    /// Apply the TypeCheck-guard policy from `type_obs`. Returns the
    /// emitted `GuardKind` so the caller can surface it via
    /// [`RecordResult::NeedsGuard`]; returns `None` when no guard was
    /// emitted (first-seen observation).
    fn maybe_emit_type_guard(&mut self, var: SsaVar, ty: ObservedType) -> Option<GuardKind> {
        // v6-γ M5 fix: mirror the observation into the buffer's
        // shared `type_info` table so the emitter's guard predicate
        // builder can resolve `TypeCheck(var, _)` / `ArithOverflow(var)`
        // sites. Without this, traces with any TypeCheck-eligible
        // op fail to install with `EmitError::Guard(MissingTypeInfo)`.
        self.buffer.record_type(var, ty);
        let prev = self.type_obs.insert(var, ty);
        match classify_observation(prev, ty) {
            TypeObsDecision::FirstSeen => None,
            TypeObsDecision::EmitGuard => {
                // De-dupe: only emit one TypeCheck per (var, ty).
                if self.guarded_vars.get(&var) == Some(&ty) {
                    return None;
                }
                self.guarded_vars.insert(var, ty);
                let kind = GuardKind::TypeCheck(var, ty);
                self.append_guard_with_site(kind, var);
                Some(kind)
            }
            TypeObsDecision::Mismatch { .. } => {
                self.aborted = Some(AbortReason::GuardFailureInRecording);
                None
            }
        }
    }

    fn emit_guard(&mut self, kind: GuardKind) -> Option<GuardKind> {
        // BoundsCheck whose base equals SsaVar::NONE is a recorder
        // sentinel — we never emit guards over invalid SSA ids.
        if let GuardKind::BoundsCheck(v, _) = kind {
            if v == SsaVar::NONE {
                return None;
            }
        }
        let payload = match kind {
            GuardKind::TypeCheck(v, _)
            | GuardKind::NotNull(v)
            | GuardKind::BoundsCheck(v, _)
            | GuardKind::ArithOverflow(v)
            | GuardKind::IsZero(v) => v,
        };
        self.append_guard_with_site(kind, payload);
        Some(kind)
    }

    /// Append a `TraceOp::Guard(kind, payload)` to the buffer *and*
    /// record the matching [`GuardSite`] in the buffer's side-table.
    ///
    /// v6-γ M4 fix: previously only the linear op was appended; the
    /// emitter's per-pc guard lookup then surfaced
    /// `EmitError::OrphanGuardOp`. The side-table entry carries the
    /// trace_pc + a synthetic external_pc so deopt dispatch keeps a
    /// stable id even before the IR walker wires through a real
    /// resume IP.
    ///
    /// `deopt_state` is left empty here. The recorder doesn't see the
    /// generic-frame slot mapping; the cranelift-generic backend
    /// fills it in via [`TraceBuffer::guards`] post-recording.
    fn append_guard_with_site(&mut self, kind: GuardKind, payload: SsaVar) {
        let trace_pc = self.buffer.append(TraceOp::Guard(kind, payload));
        // v6-δ M2-B: the recorder bumps `next_external_pc` once per
        // `record_op` call (see the comment in `record_op`). The
        // current op's PC is therefore the current value — DO NOT
        // increment again here, else a guard-emitting op would skip
        // a PC slot and the bytecode-side index lookup would drift.
        let external_pc = ExternalPc(self.next_external_pc);
        // v6-δ M2-C: stamp the current operand-stack snapshot onto
        // the site. The bytecode-side resume path reads
        // `value_stack_copy` (built from these SSAs via
        // `ssa_slots_copy` at deopt-fire time) to rehydrate the
        // mid-expression stack. Empty snapshot ⇒ guard-at-empty-stack
        // (e.g. immediately after a `Return`); the resume path falls
        // back to recipe-only materialisation.
        let snap = self.ssa_stack.clone();
        self.buffer.record_guard(
            GuardSite::new(trace_pc, external_pc, kind).with_ssa_stack_snapshot(snap),
        );
    }
}

impl Default for RecorderState {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the `ir_to_ssa` key for ops that drive the slot map.
/// Returns `None` for ops that do not touch the local / let table.
fn local_or_let_key(op: &Op) -> Option<LookupKind> {
    match op {
        Op::LetSet { idx, .. } => Some(LookupKind::Let(*idx)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_ir::IrType;

    #[test]
    fn ssa_allocator_monotonic() {
        let mut a = SsaAllocator::default();
        let v0 = a.alloc();
        let v1 = a.alloc();
        assert!(v1.raw() > v0.raw());
        assert_eq!(a.count(), 2);
        a.reset();
        assert_eq!(a.count(), 0);
    }

    #[test]
    fn new_recorder_is_empty() {
        let r = RecorderState::new();
        assert!(!r.is_aborted());
        assert!(!r.is_terminated());
        assert_eq!(r.op_count(), 0);
    }

    #[test]
    fn record_const_then_return() {
        let mut r = RecorderState::new();
        let res = r.record_op(&Op::ConstI64(7), &[], Some(ObservedType::I64));
        let val = match res {
            RecordResult::Ok { value: Some(v) } => v,
            other => panic!("unexpected {:?}", other),
        };
        let term = r.record_op(&Op::Return, &[val], None);
        assert!(matches!(term, RecordResult::Terminated));
        let buf = r.finalize().expect("no abort");
        assert_eq!(buf.op_count(), 2);
    }

    #[test]
    fn unsupported_op_aborts() {
        let mut r = RecorderState::new();
        let res = r.record_op(
            &Op::CallNative {
                import_idx: 0,
                param_tys: vec![],
                ret_ty: IrType::I64,
                cap_bit: 0,
            },
            &[],
            None,
        );
        assert!(matches!(
            res,
            RecordResult::Abort(AbortReason::UnrecoverableEffect)
        ));
        assert!(r.is_aborted());
        // Subsequent op short-circuits without touching the buffer.
        let res2 = r.record_op(&Op::ConstI64(1), &[], None);
        assert!(matches!(res2, RecordResult::Abort(_)));
        assert_eq!(r.op_count(), 0);
    }

    #[test]
    fn finalize_after_abort_returns_err() {
        let mut r = RecorderState::new();
        r.abort(AbortReason::TraceTooLong);
        assert_eq!(r.finalize().err(), Some(AbortReason::TraceTooLong));
    }

    #[test]
    fn capacity_overflow_aborts() {
        let mut r = RecorderState::with_capacity(1);
        let _ = r.record_op(&Op::ConstI64(1), &[], None);
        let res = r.record_op(&Op::ConstI64(2), &[], None);
        assert!(matches!(
            res,
            RecordResult::Abort(AbortReason::TraceTooLong)
        ));
    }

    // ---- v6-δ M2-C: ssa_stack mirror coverage ----

    #[test]
    fn const_push_grows_ssa_stack() {
        let mut r = RecorderState::new();
        assert_eq!(r.ssa_stack_high_water(), 0);
        let v0 = match r.record_op(&Op::ConstI64(1), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) } => v,
            other => panic!("unexpected {:?}", other),
        };
        assert_eq!(r.ssa_stack_snapshot(), vec![v0]);
        assert_eq!(r.ssa_stack_high_water(), 1);
    }

    #[test]
    fn add_consumes_two_pushes_one() {
        // Sequence: ConstI64(2); ConstI64(3); Add(I64) → stack = [add_ssa]
        let mut r = RecorderState::new();
        let v0 = match r.record_op(&Op::ConstI64(2), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) } => v,
            other => panic!("unexpected {:?}", other),
        };
        let v1 = match r.record_op(&Op::ConstI64(3), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) } => v,
            other => panic!("unexpected {:?}", other),
        };
        assert_eq!(r.ssa_stack_snapshot(), vec![v0, v1]);
        assert_eq!(r.ssa_stack_high_water(), 2);
        // Add(I64) surfaces NeedsGuard for ArithOverflow; both
        // `Ok` and `NeedsGuard` carry the result SSA in `value`.
        let add = match r.record_op(&Op::Add(IrType::I64), &[v0, v1], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) } => v,
            RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("unexpected {:?}", other),
        };
        // After Add: only the result is on the stack.
        assert_eq!(r.ssa_stack_snapshot(), vec![add]);
        // High water doesn't shrink.
        assert_eq!(r.ssa_stack_high_water(), 2);
    }

    #[test]
    fn return_drains_stack() {
        let mut r = RecorderState::new();
        let v0 = match r.record_op(&Op::ConstI64(7), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) } => v,
            _ => panic!("ConstI64 produced no SSA"),
        };
        assert_eq!(r.ssa_stack_snapshot().len(), 1);
        let term = r.record_op(&Op::Return, &[v0], None);
        assert!(matches!(term, RecordResult::Terminated));
        assert!(r.ssa_stack_snapshot().is_empty());
    }

    #[test]
    fn guard_site_carries_ssa_stack_snapshot() {
        // Recipe: ConstI64; ConstI64; Add(I64) — the Add emits an
        // ArithOverflow guard after the op. At that point ssa_stack
        // should be `[add_ssa]` (input consumed, result pushed, guard
        // emitted post-emit so it sees the post-op stack).
        let mut r = RecorderState::new();
        let v0 = match r.record_op(&Op::ConstI64(2), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) } => v,
            _ => panic!("ConstI64 produced no SSA"),
        };
        let v1 = match r.record_op(&Op::ConstI64(3), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) } => v,
            _ => panic!("ConstI64 produced no SSA"),
        };
        let res = r.record_op(&Op::Add(IrType::I64), &[v0, v1], Some(ObservedType::I64));
        // Add(I64) emits a NeedsGuard with ArithOverflow.
        let _guard = match res {
            RecordResult::NeedsGuard { guard, .. } => guard,
            other => panic!("unexpected {:?}", other),
        };
        // The buffer now holds [Const, Const, Add, Guard(ArithOverflow)].
        let buf = r.buffer();
        assert!(
            !buf.guards.is_empty(),
            "ArithOverflow guard must have been recorded"
        );
        let site = &buf.guards[0];
        // Snapshot should be exactly `[add_ssa]`: the result is on the
        // stack at guard-emit time.
        assert_eq!(
            site.ssa_stack_snapshot.len(),
            1,
            "post-Add stack depth is 1 (result on top)"
        );
    }

    // ---- ε-M0: begin_loop / end_loop ----

    #[test]
    fn begin_loop_emits_mark_head_with_phis() {
        let mut r = RecorderState::new();
        // Seed an acc=0 const so we have a real init SSA to carry.
        let acc = match r.record_op(&Op::ConstI64(0), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) } => v,
            other => panic!("unexpected {:?}", other),
        };
        let phis = r.begin_loop(&[LoopCarry::new(acc, ObservedType::I64)]);
        assert_eq!(phis.len(), 1, "one carry → one φ SSA");
        let phi = phis[0];
        assert_ne!(phi, acc, "φ SSA must differ from init SSA");

        // The buffer's last op should be a MarkLoopHead with the
        // recorded phi.
        let buf = r.buffer();
        let last = buf.ops.last().expect("buffer must have ≥1 op");
        match last {
            TraceOp::MarkLoopHead { loop_id, phis } => {
                assert_eq!(*loop_id, 0, "first loop id is 0");
                assert_eq!(phis.len(), 1);
                assert_eq!(phis[0].init, acc);
                assert_eq!(phis[0].phi, phi);
            }
            other => panic!("expected MarkLoopHead, got {:?}", other),
        }
    }

    #[test]
    fn end_loop_emits_mark_back_with_next_values() {
        let mut r = RecorderState::new();
        let acc = match r.record_op(&Op::ConstI64(0), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) } => v,
            other => panic!("unexpected {:?}", other),
        };
        let phis = r.begin_loop(&[LoopCarry::new(acc, ObservedType::I64)]);
        let phi = phis[0];
        // Fake an Add inside the body: phi + 1
        let one = match r.record_op(&Op::ConstI64(1), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) } => v,
            _ => panic!(),
        };
        let acc_next =
            match r.record_op(&Op::Add(IrType::I64), &[one, phi], Some(ObservedType::I64)) {
                RecordResult::Ok { value: Some(v) }
                | RecordResult::NeedsGuard { value: Some(v), .. } => v,
                other => panic!("unexpected {:?}", other),
            };
        assert!(r.end_loop(&[acc_next]));
        let buf = r.buffer();
        let last = buf.ops.last().expect("≥1 op");
        match last {
            TraceOp::MarkLoopBack {
                loop_id,
                next_values,
            } => {
                assert_eq!(*loop_id, 0);
                assert_eq!(next_values, &vec![acc_next]);
            }
            other => panic!("expected MarkLoopBack, got {:?}", other),
        }
    }

    #[test]
    fn end_loop_without_begin_aborts() {
        let mut r = RecorderState::new();
        let ok = r.end_loop(&[]);
        assert!(!ok);
        assert!(r.is_aborted());
    }

    // ---- F-D8: list / dict ops ---------------------------------------

    #[test]
    fn emit_list_get_appends_bounds_guard_and_op() {
        let mut r = RecorderState::new();
        // Seed a list ptr + index SSA.
        let list = match r.record_op(&Op::ConstI64(0x1000), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("unexpected {:?}", other),
        };
        let idx = match r.record_op(&Op::ConstI64(0), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("unexpected {:?}", other),
        };
        let dst = r
            .emit_list_get(list, idx)
            .expect("list_get must succeed on a fresh recorder");
        assert_ne!(dst, list);
        assert_ne!(dst, idx);
        // Buffer should now hold [Const, Const, Guard(BoundsCheck),
        // ListGet]; operand stack top is the dst.
        assert_eq!(r.ssa_stack_snapshot(), vec![dst]);
        let buf = r.buffer();
        let kinds: Vec<&TraceOp> = buf.ops.iter().collect();
        match kinds[kinds.len() - 1] {
            TraceOp::ListGet {
                dst: d,
                list_ptr,
                idx: i,
            } => {
                assert_eq!(*d, dst);
                assert_eq!(*list_ptr, list);
                assert_eq!(*i, idx);
            }
            other => panic!("last op must be ListGet, got {:?}", other),
        }
        assert!(
            buf.guards
                .iter()
                .any(|g| matches!(g.kind, GuardKind::BoundsCheck(v, l) if v == idx && l == list)),
            "BoundsCheck guard must be recorded"
        );
    }

    #[test]
    fn emit_dict_lookup_appends_op_with_shape_hash() {
        let mut r = RecorderState::new();
        let dict = match r.record_op(&Op::ConstI64(0x2000), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("unexpected {:?}", other),
        };
        let key = match r.record_op(&Op::ConstI64(0x3000), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("unexpected {:?}", other),
        };
        let shape: u64 = 0xfeed_face_dead_beef;
        let dst = r
            .emit_dict_lookup(dict, key, shape)
            .expect("dict_lookup must succeed");
        assert_eq!(r.ssa_stack_snapshot(), vec![dst]);
        let buf = r.buffer();
        match buf.ops.last().expect("≥1 op") {
            TraceOp::DictLookup {
                dst: d,
                dict_ptr,
                key_ptr,
                shape_hash,
            } => {
                assert_eq!(*d, dst);
                assert_eq!(*dict_ptr, dict);
                assert_eq!(*key_ptr, key);
                assert_eq!(*shape_hash, shape);
            }
            other => panic!("last op must be DictLookup, got {:?}", other),
        }
    }

    // ---- F-D8-B: record_op dispatch for DictGetByStringKey / ListGetByIntIdx ----

    #[test]
    fn record_dict_get_dispatches_dict_lookup() {
        // Drive record_op with the new IR op and confirm the buffer
        // gains a single `TraceOp::DictLookup` carrying the supplied
        // shape_hash, plus the operand-stack mirror is correctly
        // updated (pop two pushes, push one).
        let mut r = RecorderState::new();
        let dict = match r.record_op(&Op::ConstI64(0x4000), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("ConstI64 dict ptr setup failed: {other:?}"),
        };
        let key = match r.record_op(&Op::ConstI64(0x5000), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("ConstI64 key ptr setup failed: {other:?}"),
        };
        // Recorder expects inputs in push order: top first (key), then
        // container (dict).
        let res = r.record_op(
            &Op::DictGetByStringKey {
                shape_hash: 0xab,
                value_ty: IrType::I64,
                entry_count_hint: None,
            },
            &[key, dict],
            Some(ObservedType::I64),
        );
        let dst = match res {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("unexpected: {other:?}"),
        };
        // Top of operand-stack mirror is the dst.
        assert_eq!(r.ssa_stack_snapshot(), vec![dst]);
        // Buffer last op must be DictLookup with the carried shape_hash.
        let buf = r.buffer();
        match buf.ops.last().expect("≥1 op") {
            TraceOp::DictLookup {
                dst: d,
                dict_ptr,
                key_ptr,
                shape_hash,
            } => {
                assert_eq!(*d, dst);
                assert_eq!(*dict_ptr, dict);
                assert_eq!(*key_ptr, key);
                assert_eq!(*shape_hash, 0xab);
            }
            other => panic!("expected DictLookup, got {:?}", other),
        }
    }

    #[test]
    fn record_list_get_dispatches_list_get_with_bounds_guard() {
        let mut r = RecorderState::new();
        let list = match r.record_op(&Op::ConstI64(0x6000), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("ConstI64 list ptr setup failed: {other:?}"),
        };
        let idx = match r.record_op(&Op::ConstI64(0), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("ConstI64 idx setup failed: {other:?}"),
        };
        let res = r.record_op(
            &Op::ListGetByIntIdx {
                element_ty: IrType::I64,
            },
            &[idx, list],
            Some(ObservedType::I64),
        );
        let dst = match res {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!(r.ssa_stack_snapshot(), vec![dst]);
        let buf = r.buffer();
        // Last op must be ListGet.
        match buf.ops.last().expect("≥1 op") {
            TraceOp::ListGet {
                dst: d,
                list_ptr,
                idx: i,
            } => {
                assert_eq!(*d, dst);
                assert_eq!(*list_ptr, list);
                assert_eq!(*i, idx);
            }
            other => panic!("expected ListGet last op, got {:?}", other),
        }
        // BoundsCheck guard must have been recorded against (idx, list).
        assert!(
            buf.guards
                .iter()
                .any(|g| matches!(g.kind, GuardKind::BoundsCheck(v, l) if v == idx && l == list)),
            "BoundsCheck guard required for trace LICM"
        );
    }

    #[test]
    fn record_list_get_with_non_i64_element_aborts() {
        let mut r = RecorderState::new();
        let list = match r.record_op(&Op::ConstI64(0x7000), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("setup: {other:?}"),
        };
        let idx = match r.record_op(&Op::ConstI64(0), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("setup: {other:?}"),
        };
        let res = r.record_op(
            &Op::ListGetByIntIdx {
                element_ty: IrType::F64,
            },
            &[idx, list],
            None,
        );
        assert!(matches!(
            res,
            RecordResult::Abort(AbortReason::UnsupportedOp("ListGetByIntIdxNonI64"))
        ));
    }

    #[test]
    fn emit_list_get_short_circuits_when_aborted() {
        let mut r = RecorderState::new();
        r.abort(AbortReason::TraceTooLong);
        let result = r.emit_list_get(SsaVar(0), SsaVar(1));
        assert!(result.is_none());
    }

    // ---- F-D7-B: String + / .contains() recognition --------------------

    #[test]
    fn str_add_irtype_lowers_to_str_concat() {
        // Hand-build a `[lhs, rhs] -> Op::Add(IrType::String)` op
        // stream so the recorder's lowering rule sees the same shape
        // a future AST→IR pass would emit for `String + String`.
        let mut r = RecorderState::new();
        let lhs = match r.record_op(&Op::ConstI64(0xaa11), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("ConstI64 lhs: {other:?}"),
        };
        let rhs = match r.record_op(&Op::ConstI64(0xbb22), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("ConstI64 rhs: {other:?}"),
        };
        let inputs = [rhs, lhs];
        let dst = match r.record_op(&Op::Add(IrType::String), &inputs, Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("Add(String) unexpected: {other:?}"),
        };
        let buf = r.buffer();
        let last = buf.ops.last().expect(">=1 op");
        match last {
            TraceOp::StrConcat(d, l, rh) => {
                assert_eq!(*d, dst);
                assert_eq!(*l, lhs, "lhs must be second-from-top input");
                assert_eq!(*rh, rhs, "rhs must be top input");
            }
            other => panic!("expected StrConcat, got {other:?}"),
        }
        // Two NotNull guards (one per operand) recorded in the
        // buffer's guard side-table.
        let str_guards: Vec<_> = buf
            .guards
            .iter()
            .filter(|g| {
                matches!(
                    g.kind,
                    GuardKind::NotNull(v) if v == lhs || v == rhs
                )
            })
            .collect();
        assert_eq!(
            str_guards.len(),
            2,
            "expected NotNull guards on both operands, got {:?}",
            buf.guards
        );
    }

    #[test]
    fn str_add_mismatched_irtype_falls_through_to_int_add() {
        // `Op::Add(IrType::I64)` must still produce the integer Add
        // path — the String specialisation does NOT swallow integer
        // arithmetic.
        let mut r = RecorderState::new();
        let lhs = match r.record_op(&Op::ConstI64(1), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("ConstI64 lhs: {other:?}"),
        };
        let rhs = match r.record_op(&Op::ConstI64(2), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("ConstI64 rhs: {other:?}"),
        };
        let _ = match r.record_op(&Op::Add(IrType::I64), &[rhs, lhs], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("Add(I64) unexpected: {other:?}"),
        };
        // Last non-guard op must be the integer Add, NOT a StrConcat.
        let last_arith = r
            .buffer()
            .ops
            .iter()
            .rev()
            .find(|op| !matches!(op, TraceOp::Guard(_, _)))
            .expect(">=1 non-guard op");
        assert!(
            matches!(last_arith, TraceOp::Add(_, _, _)),
            "expected integer Add, got {last_arith:?}"
        );
    }

    #[test]
    fn emit_str_concat_appends_op_and_guards() {
        // Direct API entry — the AST-level driver passes the two
        // String SSAs and the recorder emits the canonical
        // StrConcat + NotNull guards.
        let mut r = RecorderState::new();
        let lhs = match r.record_op(&Op::ConstI64(0x1), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("ConstI64 lhs: {other:?}"),
        };
        let rhs = match r.record_op(&Op::ConstI64(0x2), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("ConstI64 rhs: {other:?}"),
        };
        let dst = r.emit_str_concat(lhs, rhs).expect("concat must succeed");
        assert_eq!(r.ssa_stack_snapshot(), vec![dst]);
        let last = r.buffer().ops.last().expect(">=1 op");
        match last {
            TraceOp::StrConcat(d, l, rh) => {
                assert_eq!(*d, dst);
                assert_eq!(*l, lhs);
                assert_eq!(*rh, rhs);
            }
            other => panic!("expected StrConcat, got {other:?}"),
        }
        // dst is typed Ptr in the buffer's type_info side table.
        assert_eq!(r.buffer().type_info.get(&dst), Some(&ObservedType::Ptr));
    }

    #[test]
    fn emit_str_contains_records_const_bytes() {
        // Constant needle → const_bytes side table is populated so
        // the F-D7-C inline emit path can specialise into a byte-scan.
        let mut r = RecorderState::new();
        let haystack = match r.record_op(&Op::ConstI64(0xa), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("haystack const: {other:?}"),
        };
        let needle = match r.record_op(&Op::ConstI64(0xb), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("needle const: {other:?}"),
        };
        let needle_payload = b"x".to_vec();
        let dst = r
            .emit_str_contains(haystack, needle, Some(needle_payload.clone()))
            .expect("contains must succeed");
        assert_eq!(r.ssa_stack_snapshot(), vec![dst]);
        let buf = r.buffer();
        match buf.ops.last().expect(">=1 op") {
            TraceOp::StrContains(d, h, n) => {
                assert_eq!(*d, dst);
                assert_eq!(*h, haystack);
                assert_eq!(*n, needle);
            }
            other => panic!("expected StrContains, got {other:?}"),
        }
        // const_bytes table carries the needle payload keyed on the
        // needle SSA, ready for the emitter's inline-emit lookup.
        assert_eq!(
            buf.const_bytes.get(&needle).map(|v| v.as_slice()),
            Some(needle_payload.as_slice())
        );
        // Bool result type so the emitter materialises an i32-extended
        // brif against the dst.
        assert_eq!(buf.type_info.get(&dst), Some(&ObservedType::Bool));
        // A NotNull(haystack) guard is recorded but NO NotNull(needle).
        let has_h_guard = buf
            .guards
            .iter()
            .any(|g| matches!(g.kind, GuardKind::NotNull(v) if v == haystack));
        let has_n_guard = buf
            .guards
            .iter()
            .any(|g| matches!(g.kind, GuardKind::NotNull(v) if v == needle));
        assert!(has_h_guard, "expected NotNull(haystack) guard");
        assert!(
            !has_n_guard,
            "must NOT emit NotNull(needle) — empty needle is valid"
        );
    }

    #[test]
    fn emit_str_contains_without_const_needle_skips_side_table() {
        // No needle bytes passed → const_bytes stays empty; the
        // emitter will route through the extern shim instead of the
        // inline byte-scan.
        let mut r = RecorderState::new();
        let haystack = match r.record_op(&Op::ConstI64(0xa), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("haystack: {other:?}"),
        };
        let needle = match r.record_op(&Op::ConstI64(0xb), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("needle: {other:?}"),
        };
        let _ = r
            .emit_str_contains(haystack, needle, None)
            .expect("contains must succeed");
        assert!(
            r.buffer().const_bytes.is_empty(),
            "const_bytes must stay empty without a known needle payload"
        );
    }

    #[test]
    fn emit_str_concat_short_circuits_when_aborted() {
        let mut r = RecorderState::new();
        r.abort(AbortReason::TraceTooLong);
        assert!(r.emit_str_concat(SsaVar(0), SsaVar(1)).is_none());
    }

    #[test]
    fn emit_str_contains_short_circuits_when_aborted() {
        let mut r = RecorderState::new();
        r.abort(AbortReason::TraceTooLong);
        assert!(r
            .emit_str_contains(SsaVar(0), SsaVar(1), Some(b"x".to_vec()))
            .is_none());
    }

    /// F-D7-H: `emit_str_contains` must prepend two `TraceOp::Load`
    /// ops reading the StringRef `(ptr, len)` payload off the haystack
    /// and register the dst pair in the buffer's `str_payload` side
    /// table. The emitter's `emit_str_contains` consults the side
    /// table at lowering time to route through `HaystackHandle::Preloaded`.
    #[test]
    fn emit_str_contains_injects_str_payload_loads() {
        use relon_trace_jit::Offset;
        let mut r = RecorderState::new();
        let haystack = match r.record_op(&Op::ConstI64(0xa), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("haystack: {other:?}"),
        };
        let needle = match r.record_op(&Op::ConstI64(0xb), &[], Some(ObservedType::Ptr)) {
            RecordResult::Ok { value: Some(v) }
            | RecordResult::NeedsGuard { value: Some(v), .. } => v,
            other => panic!("needle: {other:?}"),
        };
        let dst = r
            .emit_str_contains(haystack, needle, Some(b"x".to_vec()))
            .expect("contains must succeed");
        let buf = r.buffer();
        // Side-table populated.
        let (ptr_ssa, len_ssa) = buf
            .str_payload
            .get(&haystack)
            .copied()
            .expect("str_payload entry for haystack must be present");
        // Both Loads materialised against the haystack base, at
        // offsets 0 / 8 respectively — exactly the offsets LICM
        // admits as hoistable.
        let load_ops: Vec<&TraceOp> = buf
            .ops
            .iter()
            .filter(|op| matches!(op, TraceOp::Load(_, _, _)))
            .collect();
        assert_eq!(load_ops.len(), 2, "expected two Load ops: {load_ops:?}");
        let load_ptr = load_ops[0];
        let load_len = load_ops[1];
        assert!(
            matches!(load_ptr, TraceOp::Load(d, b, Offset(0)) if *d == ptr_ssa && *b == haystack),
            "first Load mismatch: {load_ptr:?}"
        );
        assert!(
            matches!(load_len, TraceOp::Load(d, b, Offset(8)) if *d == len_ssa && *b == haystack),
            "second Load mismatch: {load_len:?}"
        );
        // The StrContains op still sits at the trace tail — the
        // Loads were prepended, not appended.
        match buf.ops.last().expect(">=1 op") {
            TraceOp::StrContains(d, h, _) => {
                assert_eq!(*d, dst);
                assert_eq!(*h, haystack);
            }
            other => panic!("expected StrContains as last op, got {other:?}"),
        }
        // Both pre-loaded SSAs carry an i64 observed-type stamp so
        // the emitter's type-info lookup resolves cleanly.
        assert_eq!(buf.type_info.get(&ptr_ssa), Some(&ObservedType::I64));
        assert_eq!(buf.type_info.get(&len_ssa), Some(&ObservedType::I64));
    }

    /// F-D7-H: invoking `inject_str_payload_loads` twice against the
    /// same haystack SSA must NOT re-emit the Loads — the side-table
    /// entry is reused so the trace stream stays bounded even when the
    /// recorder lowers multiple `StrContains` calls sharing one
    /// haystack.
    #[test]
    fn inject_str_payload_loads_is_idempotent() {
        let mut r = RecorderState::new();
        let haystack = SsaVar(5);
        r.inject_str_payload_loads(haystack);
        let first = r
            .buffer()
            .ops
            .iter()
            .filter(|op| matches!(op, TraceOp::Load(_, _, _)))
            .count();
        assert_eq!(first, 2);
        r.inject_str_payload_loads(haystack);
        let second = r
            .buffer()
            .ops
            .iter()
            .filter(|op| matches!(op, TraceOp::Load(_, _, _)))
            .count();
        assert_eq!(
            second, 2,
            "second inject_str_payload_loads must reuse the existing str_payload entry"
        );
    }
}
