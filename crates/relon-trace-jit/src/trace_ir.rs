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
    /// `dst = a % b` (signed integer remainder; matches Rust's `%`
    /// and cranelift's `srem`). Trap surface is identical to `Div`:
    /// `b == 0` traps, and the `i64::MIN % -1` corner case is the
    /// only overflow scenario. Classed `RecoverableWrite` for the
    /// same divisor-zero deopt rationale as `Div`.
    ///
    /// F-D8-E.1: introduced so hot integer-modulo loops (W5's
    /// `i % 10` index hash) lower to a single SSA op instead of the
    /// `Div + Mul + Sub` triple the recorder had to emit while no
    /// matching trace op existed.
    Mod(SsaVar, SsaVar, SsaVar),
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

    // ---- F-D7 string fast path --------------------------------------
    /// `dst = StrConcat(lhs, rhs)` — `lhs` and `rhs` are opaque
    /// string pointers (the recorder passes them through as i64
    /// SSAs holding the `Arc<str>` payload pointer). The emitter
    /// lowers this to a direct `call __relon_str_concat(lhs, rhs)`
    /// — pure (no host-visible side effect) and the result is a
    /// freshly-owned `Arc<str>` whose payload pointer is returned
    /// in `dst`.
    ///
    /// Effect: `Pure`. The shim drops the result pointer's
    /// allocation tracking into the surrounding `TraceContext`
    /// memory budget so a runaway concat trace is bounded by the
    /// host's sandbox quota.
    StrConcat(SsaVar, SsaVar, SsaVar),

    /// `dst = StrConcatN(operands)` — N-operand single-allocation
    /// string concatenation. `operands` carry opaque i64 SSAs each
    /// holding a `*const StringRef` payload pointer; operand order
    /// matches the IR-side `Op::StrConcatN` operand stack
    /// (`operands[0]` is the deepest leaf / leftmost source-level
    /// argument, `operands[N-1]` is the rightmost / top-of-stack).
    /// The emitter lowers this to a single allocation + N-way
    /// payload memcpy, returning a freshly-owned `StringRef` whose
    /// `(ptr, len)` payload is the byte-concatenation of all N
    /// operand payloads.
    ///
    /// Effect: `Pure`. Same rationale as [`Self::StrConcat`]: each
    /// operand `Arc<str>` payload is immutable, the helper allocates
    /// a fresh `StringRef`, and there is no host-visible side effect.
    /// Classifying as `Pure` lets LICM hoist a loop-invariant
    /// `StrConcatN(a, b, c)` out of an inner loop and lets dead-store
    /// elision drop the op if `dst` is never consumed.
    ///
    /// `operands.len() >= 3` — two-operand concat goes through
    /// [`Self::StrConcat`] (and its inline short-rhs fast path); the
    /// IR-level fold pass never emits the degenerate one-operand
    /// shape. The emitter caps inline lowering at four operands
    /// today; recorders MUST abort cleanly when the IR-side
    /// `Op::StrConcatN { operand_count }` exceeds the cap so the
    /// outer tier router can fall back to the cranelift AOT backend.
    StrConcatN {
        /// SSA destination — holds the i64 result `*const StringRef`.
        dst: SsaVar,
        /// Operand SSAs in operand-stack order (`[0]` deepest leaf
        /// → `[N-1]` topmost / last pushed). Length matches the
        /// IR-side `Op::StrConcatN { operand_count }`.
        operands: Vec<SsaVar>,
    },

    /// `dst = StrContains(haystack, needle) -> i32 (0/1)` — typed
    /// as `i32` so callers can branch on the result with the
    /// existing `Guard(NotNull(dst))` / `Cmp` machinery. The
    /// emitter lowers this to `call __relon_str_contains(haystack,
    /// needle)`; the recorder may set an inline cache slot keyed
    /// on `(haystack, needle)` so a repeated probe with the same
    /// args (W4's pattern) skips the call entirely.
    ///
    /// Effect: `Pure`.
    StrContains(SsaVar, SsaVar, SsaVar),

    /// `dst = StrFind(haystack, needle) -> i64` returning the
    /// byte index of the first match, or `-1` on miss. Maps to
    /// `call __relon_str_find(haystack, needle)`.
    ///
    /// Effect: `Pure`.
    StrFind(SsaVar, SsaVar, SsaVar),

    /// `dst = StrSubstring(s, start, length)` — byte-indexed
    /// substring. Both `start` and `length` are i64 SSAs; the
    /// shim clamps them into `[0, len(s)]` and returns a freshly
    /// owned `Arc<str>` whose payload pointer is returned in
    /// `dst`. The emitter emits a `BoundsCheck(start, str_len)`
    /// guard before the call when the recorder has observed a
    /// `start <= len` invariant; absence of the guard falls back
    /// to the shim's runtime clamp.
    ///
    /// Effect: `Pure`.
    StrSubstring(SsaVar, SsaVar, SsaVar, SsaVar),

    /// `dst = StrGlobMatch(s, pattern) -> i32 (0/1)` — typed as
    /// `i32` so callers can branch on the result with the existing
    /// `Cmp` / `Guard` machinery. The emitter lowers this to
    /// `call __relon_str_glob_match(s, pattern)`; the helper runs
    /// the shared `relon_ir::glob::glob_match` matcher (Tier-2
    /// LuaJIT-pattern-subset glob: `*` / `?` / `[set]` / `[^set]`
    /// plus `\`-escapes, anchored on both ends, Unicode).
    ///
    /// 2026-05-21: glob_match's algorithm is intentionally NOT
    /// IR-inlined — the matcher is ~150 LoC with backtracking, and
    /// inlining it would balloon the trace body well past the
    /// per-iter cost budget. The helper-call path lands the trace-
    /// JIT in parity with the cranelift backend (which also routes
    /// through a host helper via `RelonGlobMatch` vtable slot)
    /// without the IR-inline complexity.
    ///
    /// Effect: `Pure`.
    StrGlobMatch(SsaVar, SsaVar, SsaVar),

    /// Return from the trace.
    Return(SsaVar),

    // ---- Dict / list ops (F-D8) -------------------------------------
    /// `dst = list_get(list_ptr, idx)`.
    ///
    /// F-D8: lowered into cranelift as a bounds-checked load against a
    /// `List<i64>`-shaped layout — `[len: u32 LE][pad: u32][i64 elements...]`.
    /// The emitter materialises an inline `cmp idx < len; brif ok, deopt`
    /// pair (the `BoundsCheck` guard) before the load so the trace
    /// deopts on out-of-range access instead of corrupting memory.
    ///
    /// `list_ptr` is the raw byte pointer to the record header (the
    /// same shape the cranelift-AOT backend uses for
    /// `Op::ConstListInt` / `Op::LoadListIntPtr`). For list-of-Value
    /// shapes the bench harness keeps the same `[len + i64 elements]`
    /// pre-flattened layout so the cranelift-side fast path is uniform;
    /// production wiring still goes through the host helper when the
    /// list is a `Arc<Vec<Value>>` (see
    /// `relon_trace_jit::runtime::dict_list::__relon_trace_list_get_value`).
    ///
    /// Effect class: `ReadOnly`.
    ListGet {
        /// SSA destination — holds the i64 element after the load.
        dst: SsaVar,
        /// SSA carrying the raw `list_ptr` (header start).
        list_ptr: SsaVar,
        /// SSA carrying the i64 index.
        idx: SsaVar,
    },

    /// `dst = dict_lookup(dict_ptr, key_ptr, shape_hash)` via the
    /// host's inline-cached hash table.
    ///
    /// F-D8: lowered into a single `call_indirect` to the host's
    /// `__relon_trace_dict_lookup_with_shape` helper. The helper reads
    /// the `shape_hash` immediate and compares it against the dict's
    /// recorded shape fingerprint; on mismatch it returns a sentinel
    /// the emitter turns into a deopt branch, leaving the slow path
    /// to re-record under the new shape.
    ///
    /// `key_ptr` carries a pointer to a `[len: u32][utf8...]` String
    /// record (same shape `Op::ConstString` uses); the host helper
    /// rehydrates a borrowed `&str` from it on the slow path. On the
    /// IC-hit fast path neither the key bytes nor the dict's BTreeMap
    /// are touched — the cached index lookup short-circuits.
    ///
    /// `shape_hash` is computed at recording time from the keys the
    /// recorder saw in the dict; the runtime side stamps the same
    /// fingerprint into the dict header. FxHash is used here so the
    /// per-key cost stays sub-cycle; collisions deopt cleanly.
    ///
    /// Effect class: `ReadOnly`.
    DictLookup {
        /// SSA destination — holds the i64 value (or pointer, in the
        /// future) after the lookup.
        dst: SsaVar,
        /// SSA carrying the raw `dict_ptr` (header start).
        dict_ptr: SsaVar,
        /// SSA carrying the raw `key_ptr` to a String record.
        key_ptr: SsaVar,
        /// Per-trace shape fingerprint. F-D8 uses an FxHash digest of
        /// the recorded keys in stable order. Forwarded verbatim to
        /// the host helper as the IC's tag.
        shape_hash: u64,
    },

    /// F-D8-E.2: inline dict-shape verification.
    ///
    /// Lowers to a few cranelift insns — `load.u64 dict_ptr + 0`,
    /// `icmp.eq vs imm shape_hash`, `brif → deopt` — and produces no
    /// SSA value. The optimizer pairs this op with a
    /// [`Self::DictLookupPrechecked`] body op when it can prove the
    /// `(dict_ptr, shape_hash)` pair is loop-invariant; LICM then
    /// hoists this op out of the loop so the shape check executes
    /// exactly once per trace entry instead of every iteration.
    ///
    /// Effect class: [`EffectClass::Pure`]. The op only reads from
    /// memory the caller is guaranteed to be allowed to read (the
    /// dict header), and its only side effect is a deopt branch on
    /// mismatch — same as any other guard op. LICM treats it as
    /// hoistable when `dict_ptr` is defined outside the loop body.
    DictShapeGuard {
        /// SSA carrying the raw `dict_ptr` (header start). Same
        /// pointer the matching `DictLookupPrechecked` will use.
        dict_ptr: SsaVar,
        /// Per-trace shape fingerprint expected to live at
        /// `*(dict_ptr as *const u64) + 0`. Mismatch deopts.
        shape_hash: u64,
    },

    /// F-D8-E.2: dict lookup whose shape compare was already proven
    /// elsewhere (typically a loop-hoisted [`Self::DictShapeGuard`]).
    ///
    /// Same semantics as [`Self::DictLookup`] except the runtime
    /// helper skips the shape compare on the IC fast path. The
    /// recorder never emits this op directly — only the optimizer's
    /// `dict_ic_hoist` pass produces it, and only when it has
    /// inserted a matching `DictShapeGuard` upstream. Invariant
    /// pairing is what keeps the runtime safe: a dict whose layout
    /// drifted between recorder time and trace execution gets
    /// rejected by the upstream `DictShapeGuard` before the
    /// prechecked helper would scan into garbage entries.
    ///
    /// Effect class: [`EffectClass::ReadOnly`] (same as
    /// `DictLookup`).
    DictLookupPrechecked {
        /// SSA destination — holds the i64 value after the lookup.
        dst: SsaVar,
        /// SSA carrying the raw `dict_ptr` (header start).
        dict_ptr: SsaVar,
        /// SSA carrying the raw `key_ptr` to a String record.
        key_ptr: SsaVar,
    },

    // ---- Loop markers -----------------------------------------------
    /// Marks the entry of a recorded loop. `loop_id` distinguishes
    /// nested loops; the same id pairs `MarkLoopHead` with its matching
    /// `MarkLoopBack`.
    ///
    /// ε-M0: `phis` describes the loop-carried values. Each entry
    /// `LoopPhi { init, phi }` says: the SSA `phi` is a φ-node visible
    /// inside the loop body, seeded by `init` on the first entry from
    /// the predecessor and updated on each back-edge by the matching
    /// position in [`TraceOp::MarkLoopBack::next_values`]. An empty
    /// `phis` vec keeps the historical "loop-carried-via-let-slot"
    /// semantics (LICM-only marker), so existing tests stay green.
    MarkLoopHead {
        loop_id: u32,
        /// Loop-carried φ pairs in stable order. The emitter creates
        /// one cranelift block-param per entry, in the same order.
        phis: Vec<LoopPhi>,
    },
    /// Marks the back-edge / exit of the loop with the matching
    /// `loop_id`. See [`TraceOp::MarkLoopHead`].
    ///
    /// ε-M0: `next_values` carries the SSAs that drive the matching
    /// `MarkLoopHead`'s φ nodes on each back-edge iteration. Must
    /// have the same length and stable order as the head's `phis`.
    /// Empty when the matching head has no φs (LICM-only marker).
    MarkLoopBack {
        loop_id: u32,
        /// SSAs forwarded to the head's φ nodes on the back-edge,
        /// one per entry in [`TraceOp::MarkLoopHead::phis`], same order.
        next_values: Vec<SsaVar>,
    },
}

/// A loop-carried φ pair recorded on [`TraceOp::MarkLoopHead`].
///
/// Inside the loop body, references to the loop-carried value see the
/// `phi` SSA id. On the first entry, `init` (a value defined before
/// the loop) flows into `phi`; on each back-edge, the matching slot
/// in [`TraceOp::MarkLoopBack::next_values`] flows into `phi`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LoopPhi {
    /// Pre-loop seed value visible at the loop header entry edge.
    pub init: SsaVar,
    /// φ SSA id visible inside the loop body.
    pub phi: SsaVar,
}

impl LoopPhi {
    pub fn new(init: SsaVar, phi: SsaVar) -> Self {
        Self { init, phi }
    }
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
    /// ε-M0: trace assumed `var == 0` — the dual of [`Self::NotNull`].
    /// Used when the recorder is following the **fall-through** path
    /// of a `BrIf` (cond was 0, branch not taken). Deopts when the
    /// runtime cond flips to non-zero.
    IsZero(SsaVar),
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
            | TraceOp::MarkLoopBack { .. }
            // F-D7 string ops are referentially transparent: input
            // `Arc<str>` pointers are immutable, the shim allocates a
            // fresh result, and there is no host-visible side effect.
            // Classifying as `Pure` lets the optimiser hoist a
            // `StrContains(s, "x")` out of an inner loop when `s` is
            // loop-invariant (W4's hot pattern).
            | TraceOp::StrConcat(_, _, _)
            | TraceOp::StrConcatN { .. }
            | TraceOp::StrContains(_, _, _)
            | TraceOp::StrFind(_, _, _)
            | TraceOp::StrSubstring(_, _, _, _)
            | TraceOp::StrGlobMatch(_, _, _)
            // F-D8-E.2: `DictShapeGuard` is a guard-like op — it
            // reads the dict header's first 8 bytes (immutable for
            // the trace's lifetime) and only deopts on mismatch. No
            // host-visible side effect, no SSA output. Classified
            // `Pure` so LICM lifts loop-invariant shape probes out
            // of the loop body.
            | TraceOp::DictShapeGuard { .. } => EffectClass::Pure,

            TraceOp::Load(_, _, _)
            | TraceOp::LocalGet(_, _)
            | TraceOp::ListGet { .. }
            | TraceOp::DictLookup { .. }
            | TraceOp::DictLookupPrechecked { .. } => EffectClass::ReadOnly,

            TraceOp::Div(_, _, _) | TraceOp::Mod(_, _, _) | TraceOp::Store(_, _, _) => {
                EffectClass::RecoverableWrite
            }

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
            | TraceOp::Mod(dst, _, _)
            | TraceOp::Cmp(_, dst, _, _)
            | TraceOp::Load(dst, _, _)
            | TraceOp::ConstI32(dst, _)
            | TraceOp::ConstI64(dst, _)
            | TraceOp::LocalGet(dst, _)
            | TraceOp::Call(dst, _, _, _)
            | TraceOp::StrConcat(dst, _, _)
            | TraceOp::StrContains(dst, _, _)
            | TraceOp::StrFind(dst, _, _)
            | TraceOp::StrSubstring(dst, _, _, _)
            | TraceOp::StrGlobMatch(dst, _, _) => Some(*dst),

            TraceOp::ListGet { dst, .. }
            | TraceOp::DictLookup { dst, .. }
            | TraceOp::DictLookupPrechecked { dst, .. }
            | TraceOp::StrConcatN { dst, .. } => Some(*dst),

            TraceOp::Store(_, _, _)
            | TraceOp::Guard(_, _)
            | TraceOp::Return(_)
            | TraceOp::MarkLoopHead { .. }
            | TraceOp::MarkLoopBack { .. }
            | TraceOp::DictShapeGuard { .. } => None,
        }
    }

    /// SSA vars *read* by this op. Returned in fixed order for
    /// deterministic rewriting by optimizer passes.
    pub fn inputs(&self) -> Vec<SsaVar> {
        match self {
            TraceOp::Add(_, a, b)
            | TraceOp::Sub(_, a, b)
            | TraceOp::Mul(_, a, b)
            | TraceOp::Div(_, a, b)
            | TraceOp::Mod(_, a, b) => vec![*a, *b],
            TraceOp::Cmp(_, _, a, b) => vec![*a, *b],
            TraceOp::Load(_, base, _) => vec![*base],
            TraceOp::Store(base, _, src) => vec![*base, *src],
            TraceOp::ConstI32(_, _) | TraceOp::ConstI64(_, _) | TraceOp::LocalGet(_, _) => vec![],
            TraceOp::Guard(kind, _check) => match *kind {
                GuardKind::TypeCheck(v, _) => vec![v],
                GuardKind::NotNull(v) => vec![v],
                GuardKind::BoundsCheck(v, limit) => vec![v, limit],
                GuardKind::ArithOverflow(v) => vec![v],
                GuardKind::IsZero(v) => vec![v],
            },
            TraceOp::Call(_, _, args, _) => args.clone(),
            TraceOp::StrConcat(_, a, b)
            | TraceOp::StrContains(_, a, b)
            | TraceOp::StrFind(_, a, b)
            | TraceOp::StrGlobMatch(_, a, b) => vec![*a, *b],
            TraceOp::StrConcatN { operands, .. } => operands.clone(),
            TraceOp::StrSubstring(_, s, start, len) => vec![*s, *start, *len],
            TraceOp::ListGet { list_ptr, idx, .. } => vec![*list_ptr, *idx],
            TraceOp::DictLookup {
                dict_ptr, key_ptr, ..
            } => vec![*dict_ptr, *key_ptr],
            // F-D8-E.2
            TraceOp::DictShapeGuard { dict_ptr, .. } => vec![*dict_ptr],
            TraceOp::DictLookupPrechecked {
                dict_ptr, key_ptr, ..
            } => vec![*dict_ptr, *key_ptr],
            TraceOp::Return(v) => vec![*v],
            // ε-M0: loop markers consume / produce φ pairs. The head
            // reads the `init` SSA (defined before the loop) for each
            // φ; the back reads the `next_values` SSA (defined inside
            // the body) on the back-edge.
            TraceOp::MarkLoopHead { phis, .. } => phis.iter().map(|p| p.init).collect(),
            TraceOp::MarkLoopBack { next_values, .. } => next_values.clone(),
        }
    }

    /// All SSA vars *defined* by this op. Returns multiple ids for
    /// loop heads that carry φ nodes; a thin wrapper over
    /// [`TraceOp::output`] for every other variant.
    ///
    /// Optimizer passes that track "what's defined inside a loop"
    /// MUST use this method so the head's φ SSAs are counted.
    pub fn defs(&self) -> Vec<SsaVar> {
        match self {
            TraceOp::MarkLoopHead { phis, .. } => phis.iter().map(|p| p.phi).collect(),
            other => other.output().into_iter().collect(),
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
            TraceOp::MarkLoopHead { loop_id, .. } => Some(*loop_id),
            _ => None,
        }
    }

    /// Returns `Some(loop_id)` if this is a loop back marker.
    pub fn loop_back_id(&self) -> Option<u32> {
        match self {
            TraceOp::MarkLoopBack { loop_id, .. } => Some(*loop_id),
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

    /// 2026-05-21: `StrGlobMatch` mirrors `StrContains` shape — pure,
    /// produces one i32 SSA, consumes (haystack, pattern). Pin the
    /// taxonomy so a future reordering of the enum doesn't silently
    /// drop the glob op out of the pure-op fast path the optimizer
    /// hoists across loop bodies.
    #[test]
    fn str_glob_match_is_pure_with_two_inputs() {
        let op = TraceOp::StrGlobMatch(SsaVar(3), SsaVar(1), SsaVar(2));
        assert_eq!(op.effect_class(), EffectClass::Pure);
        assert_eq!(op.output(), Some(SsaVar(3)));
        assert_eq!(op.inputs(), vec![SsaVar(1), SsaVar(2)]);
        assert!(!op.is_guard());
    }

    #[test]
    fn mod_is_recoverable_write_with_io() {
        let op = TraceOp::Mod(SsaVar(3), SsaVar(1), SsaVar(2));
        assert_eq!(op.effect_class(), EffectClass::RecoverableWrite);
        assert_eq!(op.output(), Some(SsaVar(3)));
        assert_eq!(op.inputs(), vec![SsaVar(1), SsaVar(2)]);
        assert!(!op.is_guard());
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
        let head = TraceOp::MarkLoopHead {
            loop_id: 3,
            phis: vec![],
        };
        let back = TraceOp::MarkLoopBack {
            loop_id: 3,
            next_values: vec![],
        };
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

    // ---- F-D8: dict / list ops --------------------------------------

    #[test]
    fn list_get_is_read_only() {
        let op = TraceOp::ListGet {
            dst: SsaVar(3),
            list_ptr: SsaVar(1),
            idx: SsaVar(2),
        };
        assert_eq!(op.effect_class(), EffectClass::ReadOnly);
        assert_eq!(op.output(), Some(SsaVar(3)));
        assert_eq!(op.inputs(), vec![SsaVar(1), SsaVar(2)]);
        assert!(!op.is_guard());
    }

    #[test]
    fn dict_lookup_is_read_only_and_carries_shape() {
        let op = TraceOp::DictLookup {
            dst: SsaVar(4),
            dict_ptr: SsaVar(1),
            key_ptr: SsaVar(2),
            shape_hash: 0xdead_beef_cafe_0001,
        };
        assert_eq!(op.effect_class(), EffectClass::ReadOnly);
        assert_eq!(op.output(), Some(SsaVar(4)));
        assert_eq!(op.inputs(), vec![SsaVar(1), SsaVar(2)]);
        match op {
            TraceOp::DictLookup { shape_hash, .. } => assert_eq!(shape_hash, 0xdead_beef_cafe_0001),
            _ => panic!("variant must round-trip its shape_hash payload"),
        }
    }

    // ---- F-D8-E.2: shape-guard + prechecked-lookup variants ---------

    #[test]
    fn dict_shape_guard_is_pure_outputless_single_input() {
        let op = TraceOp::DictShapeGuard {
            dict_ptr: SsaVar(7),
            shape_hash: 0xfeed_cafe_dead_0001,
        };
        // LICM hoists Pure-effect ops; DictShapeGuard MUST be Pure or
        // it will never lift out of the loop body.
        assert_eq!(op.effect_class(), EffectClass::Pure);
        assert_eq!(op.output(), None);
        assert_eq!(op.inputs(), vec![SsaVar(7)]);
        assert!(!op.is_guard()); // not a `Guard(...)` enum variant
        match op {
            TraceOp::DictShapeGuard { shape_hash, .. } => {
                assert_eq!(shape_hash, 0xfeed_cafe_dead_0001);
            }
            _ => panic!("variant must round-trip its shape_hash payload"),
        }
    }

    // ---- #168: N-operand string concat -----------------------------

    #[test]
    fn str_concat_n_is_pure_with_variable_inputs() {
        let op = TraceOp::StrConcatN {
            dst: SsaVar(10),
            operands: vec![SsaVar(1), SsaVar(2), SsaVar(3)],
        };
        // Same effect class as the two-operand `StrConcat` so LICM /
        // const-fold treat the N-way fold uniformly.
        assert_eq!(op.effect_class(), EffectClass::Pure);
        assert_eq!(op.output(), Some(SsaVar(10)));
        assert_eq!(op.inputs(), vec![SsaVar(1), SsaVar(2), SsaVar(3)]);
        assert!(!op.is_guard());
    }

    #[test]
    fn str_concat_n_inputs_preserve_operand_order() {
        // Operand order MUST round-trip through `inputs()` because the
        // emitter relies on `[0]` being the deepest leaf and `[N-1]`
        // being the top-of-stack rhs. Any reordering inside the helper
        // would silently swap source-level concat operands.
        let op = TraceOp::StrConcatN {
            dst: SsaVar(99),
            operands: vec![SsaVar(7), SsaVar(8), SsaVar(9), SsaVar(11)],
        };
        assert_eq!(
            op.inputs(),
            vec![SsaVar(7), SsaVar(8), SsaVar(9), SsaVar(11)]
        );
        // `defs()` mirrors `output()` for non-loop-head ops.
        assert_eq!(op.defs(), vec![SsaVar(99)]);
    }

    #[test]
    fn dict_lookup_prechecked_is_read_only() {
        let op = TraceOp::DictLookupPrechecked {
            dst: SsaVar(9),
            dict_ptr: SsaVar(3),
            key_ptr: SsaVar(4),
        };
        // Same effect class as DictLookup so the LICM rules + dead-
        // store passes treat both flavours uniformly.
        assert_eq!(op.effect_class(), EffectClass::ReadOnly);
        assert_eq!(op.output(), Some(SsaVar(9)));
        assert_eq!(op.inputs(), vec![SsaVar(3), SsaVar(4)]);
    }
}
