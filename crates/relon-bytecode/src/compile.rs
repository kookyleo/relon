//! Compile [`relon_ir::Func`] into a flat [`crate::BcFunction`].
//!
//! Two-pass walk: pass 1 emits ops with **placeholder** branch
//! targets (`usize::MAX`) and records each pending fixup; pass 2
//! patches the targets once every IR-level `block` / `loop` has a
//! resolved bytecode index. The pattern mirrors the wasm verifier's
//! `pop_label` discipline so a hand-built IR module that wouldn't
//! verify on wasm bounces out here as `BcCompileError::BranchTarget`.
//!
//! ## Buffer-protocol lowering
//!
//! `lower_workspace_single` emits the buffer-protocol entry shape:
//! `params = [I32 in_ptr, I32 in_len, I32 out_ptr, I32 out_cap,
//! I64 caps]` and the body uses `LoadField { offset }` /
//! `StoreField { offset }` against the in/out buffers. The bytecode
//! VM doesn't talk arenas; instead, `compile_buffer_protocol`
//! synthesises a virtual-local layout where each `#main` parameter
//! occupies one slot and every `LoadField` / `StoreField` is
//! rewritten to the matching `LocalGet` / `LocalSet`. The arg
//! packing (in [`crate::evaluator::BytecodeEvaluator`]) writes args
//! into those virtual slots directly.

use std::collections::BTreeMap;

use thiserror::Error;

use ordered_float::OrderedFloat;

use relon_eval_api::layout::OffsetTable;
use relon_ir::{
    op_visitor::{walk_op, OpVisitor},
    ClosureCapture, Func, IrType, Op, TaggedOp, TrapKind,
};

use crate::op::{BcFunction, BcOp, BcTrapKind, ExternalPc, StackOrigin};

/// Compile errors. Each variant pins one structural mismatch so the
/// caller can decide whether to fall back to the cranelift / tree-
/// walker path or to log + bail.
#[derive(Debug, Clone, Error)]
pub enum BcCompileError {
    /// The IR contains an op the bytecode VM has no lowering for.
    /// Today the legacy-i64 entry shape covers arith + control flow
    /// + locals + return; everything else (Call / CallNative / record
    ///   construction / list ops / stdlib indirection) surfaces here.
    #[error("unsupported IR op for bytecode VM: {0}")]
    UnsupportedOp(String),
    /// A branch target depth exceeded the resolved label stack.
    /// Symptom of either a malformed IR module or a compiler bug.
    #[error("branch target depth {depth} out of range (label stack height {stack})")]
    BranchTarget {
        /// Label depth requested by the branch op.
        depth: u32,
        /// Active label-stack height at the branch site.
        stack: u32,
    },
    /// `If` arm produced no ops. The IR shouldn't ever emit an empty
    /// arm, so flag it loudly rather than silently dropping the
    /// fixup.
    #[error("empty If arm at IR PC {pc}")]
    EmptyArm {
        /// IR-level PC of the offending `If` op.
        pc: u64,
    },
    /// A `LoadField` / `StoreField` op references an offset that
    /// doesn't appear in the schema's offset table. Indicates the
    /// IR doesn't match the schema the caller passed in — symptom
    /// of cross-module lowering drift.
    #[error("LoadField/StoreField offset {offset} has no matching schema field")]
    UnknownFieldOffset {
        /// The unresolved offset value.
        offset: u32,
    },
}

/// Compile one IR function to bytecode. The op stream and the
/// `ir_pc_map` come back paired so the partial-resume routing in
/// M2-B can walk both in lockstep.
///
/// `field_offset_to_local` maps each `LoadField` / `StoreField`
/// offset to the local slot the bytecode VM should read / write.
/// `BytecodeEvaluator::from_source` builds this from the main +
/// return schemas; legacy-i64 direct-IR callers can pass an empty
/// map (their IR uses `LocalGet(idx)` directly).
pub fn compile_function(
    func: &Func,
    field_offset_to_local: &BTreeMap<u32, u32>,
    return_field_offset_to_local: &BTreeMap<u32, u32>,
) -> Result<BcFunction, BcCompileError> {
    compile_function_in_module(
        func,
        &[],
        field_offset_to_local,
        return_field_offset_to_local,
    )
}

/// v6-δ M2-B widening: compile a function with access to the full
/// IR `funcs` slice so the bytecode compiler can inline simple
/// callees (`Op::Call { fn_index, ... }`).
///
/// The bytecode VM has no real Call dispatcher yet (M2-C work);
/// instead the compile pass walks the callee's body inline,
/// rewriting its `LocalGet(N)` references into reads against
/// fresh scratch slots seeded from the caller's stack. This is the
/// classic "tree-shake by inlining" approach — bounded in
/// per-function expansion by `MAX_INLINE_OPS` so a maliciously
/// deep call chain can't blow the compile pass.
///
/// Callers without a module (legacy-i64 direct-IR tests) pass an
/// empty slice; any `Op::Call` in that mode bounces out as
/// `BcCompileError::UnsupportedOp` exactly as before.
pub fn compile_function_in_module(
    func: &Func,
    funcs: &[Func],
    field_offset_to_local: &BTreeMap<u32, u32>,
    return_field_offset_to_local: &BTreeMap<u32, u32>,
) -> Result<BcFunction, BcCompileError> {
    let mut state = CompileState {
        ops: Vec::new(),
        ir_pc_map: Vec::new(),
        ir_pc_next: 0,
        labels: Vec::new(),
        field_offset_to_local,
        return_field_offset_to_local,
        stack_recipe: Vec::new(),
        current_stack: Vec::new(),
        next_snapshot_idx: 0,
        funcs,
        scratch_local_top: func.params.len() as u32
            + field_offset_to_local.len() as u32
            + return_field_offset_to_local.len() as u32,
        inline_depth: 0,
        current_pc: 0,
        string_pool: Vec::new(),
        inline_frame: None,
    };
    state.compile_seq(&func.body, /*depth_base=*/ 0)?;
    state.emit_ret_if_missing();

    // Locals count: derive from the maximum LocalGet/LocalSet index
    // observed plus the buffer-protocol epilogue slot for the return
    // value placeholder (so StoreField → LocalSet can land cleanly).
    let max_local = state
        .ops
        .iter()
        .filter_map(|op| match op {
            BcOp::LocalGet(i) | BcOp::LocalSet(i) => Some(*i),
            _ => None,
        })
        .max();
    let locals_used = max_local.map(|m| m + 1).unwrap_or(0);
    let locals = (func.params.len() as u32).max(locals_used);

    // #166 M2-B from_source full cap-gate activation: scan the emitted
    // ops for capability-sensitive shapes. `BcOp::CallNative` and
    // `BcOp::CheckCap` are the only IR-level ops that touch the gate
    // today; either presence means the dispatch path needs the
    // installed gate consulted at entry too, so a trust-level downgrade
    // between compile and run is caught before the first sensitive op
    // observes any state. The flag is `false` for the scalar
    // `from_source` envelope (arith / cmp / control flow only), which
    // keeps the historical zero-overhead posture intact.
    let requires_cap_consult = ops_contain_sensitive(&state.ops);
    Ok(BcFunction {
        ops: state.ops,
        locals,
        ir_pc_map: state.ir_pc_map,
        stack_recipe: state.stack_recipe,
        string_pool: state.string_pool,
        // M2-B phase 4c: the compile pass leaves `fn_id` blank by
        // default. Hosts that wire the bytecode VM into the cross-
        // backend trace-JIT registry stamp the id post-compile via
        // [`BcFunction::with_fn_id`] so the bytecode artefact and the
        // matching cranelift trace function share the same hot-counter
        // slot.
        fn_id: None,
        // M3 closure bodies: empty by default. The IR-level
        // `Op::MakeClosure` path remains gated on follow-up work that
        // hoists the lambda body through the bytecode compile pipeline;
        // hand-built `BcFunction` instances (tests / future direct-IR
        // closure constructions) populate this slice via the public
        // field.
        closure_bodies: Vec::new(),
        requires_cap_consult,
    })
}

/// #166 M2-B from_source full cap-gate activation: scan a flat
/// bytecode op stream for capability-sensitive shapes. Today only
/// `BcOp::CallNative` and `BcOp::CheckCap` reach the gate, so the
/// scan is a single pass. Scalar / control-flow / string-arena /
/// list-arena ops do not consult the gate and are excluded.
///
/// Centralised here (instead of inlined at the compile-pass tail)
/// so hand-built `BcFunction` constructors and the closure-body
/// walker share the same predicate.
pub(crate) fn ops_contain_sensitive(ops: &[BcOp]) -> bool {
    ops.iter()
        .any(|op| matches!(op, BcOp::CallNative { .. } | BcOp::CheckCap { .. }))
}

/// Resolve a stdlib `Op::Call { fn_index }` to its bundled
/// [`relon_ir::StdlibFunction`]. Returns `None` when `fn_index`
/// falls outside the bundled-stdlib range. Centralised here so the
/// inline-call expander doesn't pull in the full
/// `relon_ir::stdlib::builtin_stdlib` surface on every callsite.
fn resolve_stdlib_func(fn_index: u32) -> Option<relon_ir::StdlibFunction> {
    if fn_index >= relon_ir::stdlib_function_count() {
        return None;
    }
    // F-D2-G: the static registry slice is cached behind a `OnceLock`;
    // `clone()` shares the lazy `Arc<OnceLock>` body cell so the
    // bytecode-side `Func` lift below pays the body-build cost at
    // most once per process per stdlib slot.
    relon_ir::builtin_stdlib().get(fn_index as usize).cloned()
}

/// Build a `field_offset → local_idx` map from a schema offset
/// table. Each declared field gets the next sequential slot index;
/// the order is whatever `BTreeMap` iteration produces (offset-asc),
/// which matches the schema declaration order for v1 inline layouts.
pub fn build_offset_to_local(layout: &OffsetTable) -> BTreeMap<u32, u32> {
    let mut by_offset: BTreeMap<u32, u32> = BTreeMap::new();
    for (i, fo) in layout.fields.iter().enumerate() {
        by_offset.insert(fo.offset as u32, i as u32);
    }
    by_offset
}

/// Maximum number of bytecode ops a single `Op::Call` inlining
/// expansion may produce. Acts as the guard against deep / cyclic
/// inline chains; if exceeded, the compiler bounces out with
/// `BcCompileError::UnsupportedOp` carrying the offending callee
/// signature so the caller falls back to cranelift / tree-walker.
const MAX_INLINE_OPS: usize = 64;

/// Maximum nested inline depth. A single user `f()` whose body
/// contains `g()` whose body contains `h()` is depth 3; anything
/// deeper trips the same "too complex to inline" envelope reject.
const MAX_INLINE_DEPTH: u32 = 3;

/// Curated whitelist of IR ops the inline expander accepts in a
/// callee body. The bytecode VM only models a scalar-shaped
/// operand stack + virtual locals, so any op that touches dict /
/// list / buffer / jump-table / closure surface is rejected so
/// the four-way harness routes the callsite through cranelift or
/// the tree-walker. `If` recurses into its arms so nested
/// disallowed ops surface at the same boundary, matching the
/// pre-P1-20 behaviour of the recursive `compile_inline_one`
/// walker.
fn validate_inline_op(op: &Op) -> Result<(), BcCompileError> {
    match op {
        Op::LocalGet(_)
        | Op::ConstI64(_)
        | Op::ConstI32(_)
        | Op::ConstBool(_)
        | Op::LetGet { .. }
        | Op::LetSet { .. }
        | Op::Add(_)
        | Op::Sub(_)
        | Op::Mul(_)
        | Op::Div(_)
        | Op::Mod(_)
        | Op::Eq(_)
        | Op::Ne(_)
        | Op::Lt(_)
        | Op::Le(_)
        | Op::Gt(_)
        | Op::Ge(_)
        | Op::Return
        | Op::Trap { .. }
        | Op::Select { .. }
        | Op::ConstString { .. }
        | Op::ConstListInt { .. }
        | Op::ConstListBool { .. }
        | Op::ConstListFloat { .. }
        | Op::ConstListString { .. }
        | Op::ReadStringLen
        | Op::Call { .. } => Ok(()),
        Op::If {
            then_body,
            else_body,
            ..
        } => {
            for t in then_body {
                validate_inline_op(&t.op)?;
            }
            for t in else_body {
                validate_inline_op(&t.op)?;
            }
            Ok(())
        }
        other => Err(BcCompileError::UnsupportedOp(format!(
            "inline body op unsupported: {other:?}"
        ))),
    }
}

struct CompileState<'a> {
    ops: Vec<BcOp>,
    ir_pc_map: Vec<ExternalPc>,
    ir_pc_next: ExternalPc,
    labels: Vec<LabelFrame>,
    /// Map LoadField offsets to virtual local slots; empty for
    /// legacy-i64 sources.
    field_offset_to_local: &'a BTreeMap<u32, u32>,
    /// Map StoreField offsets — same scheme as `field_offset_to_local`
    /// but indexed off the **return** schema. We dump StoreField into
    /// a per-return-field slot then the evaluator unpacks it back
    /// into a `Value` after the run.
    return_field_offset_to_local: &'a BTreeMap<u32, u32>,
    /// v6-δ M2-B: per-bc_idx operand-stack recipe.
    /// `stack_recipe[i]` snapshots `current_stack` **before** op `i`
    /// runs, used by partial-resume to rebuild the operand stack at
    /// `i` without re-running the producers from entry.
    stack_recipe: Vec<Vec<StackOrigin>>,
    /// Live abstract operand stack tracked across compilation. Each
    /// entry mirrors what the VM's runtime stack would hold at that
    /// point; see [`StackOrigin`] for the recipe taxonomy.
    current_stack: Vec<StackOrigin>,
    /// Monotonic counter for [`StackOrigin::Snapshot`] indices. Every
    /// arith / cmp result claims one slot in
    /// `DeoptStateSnapshot::value_stack_copy`; resume-time consumers
    /// read the slot by this index.
    next_snapshot_idx: u32,
    /// v6-δ M2-B: the full IR `funcs` slice — used to look up
    /// `Op::Call { fn_index }` bodies for the inlining widener.
    /// Empty for legacy-i64 callers; any `Op::Call` then surfaces as
    /// `UnsupportedOp` exactly as in M2-A.
    funcs: &'a [Func],
    /// Next free virtual-local slot for inline-scratch storage. Each
    /// nested `Op::Call` expansion claims a contiguous block of
    /// `arg_count` slots starting here for the callee's `LocalGet(N)`
    /// references; the slots stay reserved for the rest of the
    /// function (cheap — the bytecode VM's local array is sized off
    /// the max-observed slot).
    scratch_local_top: u32,
    /// Current inline nesting depth. Tripping
    /// [`MAX_INLINE_DEPTH`] surfaces as
    /// `BcCompileError::UnsupportedOp("inline depth exceeded")`.
    inline_depth: u32,
    /// IR PC bound to the op currently being lowered. Re-entry into
    /// the visitor methods (e.g. during `compile_seq` recursion for
    /// `If` / `Block` / `Loop`) re-bumps the counter via
    /// [`Self::next_pc`] before dispatching; this slot caches the
    /// value once per outer dispatch so every visitor method sees the
    /// matching PC without threading it through the trait surface.
    current_pc: ExternalPc,
    /// M2-B phase 4b-continuation: per-function string constant pool.
    /// `visit_const_string` (in the **real-handle** path, opted into by
    /// the phase-4b-continuation widening) registers the value here
    /// and emits `BcOp::StrConst { idx }` against the slot. The legacy
    /// length-fold path keeps emitting `BcOp::ConstI64(len)` and never
    /// touches the pool, so existing callers don't observe a change.
    string_pool: Vec<String>,
    /// P1-20: active inline-call frame, if we're walking a callee body.
    /// When set, `visit_local_get` / `visit_let_get` / `visit_let_set`
    /// rewrite indices through `local_base` / `let_base`, `visit_return`
    /// becomes a no-op (the callee's `Return` is a fallthrough — the
    /// caller drains the result off the operand stack), and
    /// `self.current_pc` reflects the **caller's** call-site PC so
    /// every emitted op stays attributed to the inline expansion site.
    /// Restored to `None` after the inline body has been walked, so
    /// sibling top-level ops resume normal slot resolution.
    inline_frame: Option<InlineFrame>,
}

/// Active state while a callee body is being walked through the
/// inliner. See `CompileState::inline_frame`.
struct InlineFrame {
    /// Caller-side virtual local slot that holds the callee's
    /// `LocalGet(0)` (i.e. its first parameter). Subsequent params
    /// are at `local_base + 1`, `local_base + 2`, ... .
    local_base: u32,
    /// Caller-side virtual local slot that holds the callee's
    /// `LetGet(0)` / `LetSet(0)` (its first let-local). Disjoint
    /// from `local_base` and from the caller's own let-locals so
    /// nested inlining never aliases.
    let_base: u32,
}

#[derive(Debug)]
struct LabelFrame {
    kind: LabelKind,
    target: Option<usize>,
    pending_patches: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LabelKind {
    Block,
    Loop,
}

impl<'a> CompileState<'a> {
    fn next_pc(&mut self) -> ExternalPc {
        self.ir_pc_next += 1;
        self.ir_pc_next
    }

    fn emit(&mut self, op: BcOp, pc: ExternalPc) {
        // Snapshot the operand-stack recipe **before** the op runs —
        // partial-resume jumps to `bc_idx` and replays the recipe to
        // rebuild the stack to the state the op expects.
        self.stack_recipe.push(self.current_stack.clone());
        self.ops.push(op);
        self.ir_pc_map.push(pc);
    }

    /// Drop the top `n` slots from the abstract operand stack.
    fn pop_n(&mut self, n: usize) {
        for _ in 0..n {
            self.current_stack.pop();
        }
    }

    /// Push a fresh `StackOrigin::Snapshot(idx)` onto the abstract
    /// operand stack. Used for any op whose pushed value is produced
    /// by the dispatcher (arena handle, host-fn return, etc.) rather
    /// than re-derivable from locals / consts.
    fn push_snapshot(&mut self) {
        let snap_idx = self.next_snapshot_idx;
        self.next_snapshot_idx += 1;
        self.current_stack.push(StackOrigin::Snapshot(snap_idx));
    }

    /// Apply the abstract stack effect of `op` to `current_stack`.
    /// Called immediately after [`Self::emit`] so the next op's
    /// recorded recipe reflects the producer/consumer behaviour.
    /// Most arms reduce to "pop N inputs, push a single Snapshot for
    /// the dispatcher-produced result" — see `pop_n` + `push_snapshot`.
    fn apply_stack_effect(&mut self, op: &BcOp) {
        match op {
            BcOp::ConstI64(v) => self.current_stack.push(StackOrigin::Const(*v as u64)),
            BcOp::ConstI32(v) => self
                .current_stack
                .push(StackOrigin::Const(*v as u32 as u64)),
            BcOp::LocalGet(idx) => self.current_stack.push(StackOrigin::Local(*idx)),
            BcOp::LocalSet(_) => self.pop_n(1),
            // M2-C lever 3: typed arith / cmp ops carry no payload after
            // the per-type specialization split; the stack effect is the
            // same regardless of i64 / f64 lane.
            BcOp::AddI64
            | BcOp::SubI64
            | BcOp::MulI64
            | BcOp::DivI64
            | BcOp::ModI64
            | BcOp::AddF64
            | BcOp::SubF64
            | BcOp::MulF64
            | BcOp::DivF64
            | BcOp::ModF64
            | BcOp::EqI64
            | BcOp::NeI64
            | BcOp::LtI64
            | BcOp::LeI64
            | BcOp::GtI64
            | BcOp::GeI64
            | BcOp::EqF64
            | BcOp::NeF64
            | BcOp::LtF64
            | BcOp::LeF64
            | BcOp::GtF64
            | BcOp::GeF64 => {
                // Pop two operands, push one snapshot-backed result.
                self.pop_n(2);
                self.push_snapshot();
            }
            BcOp::Jump(_) => {
                // Unconditional jump: no stack effect at this point.
                // Branch targets reset their entry recipe via the
                // label-fixup pass; for the straight-line envelope
                // the next recipe is whatever the join produces.
            }
            BcOp::JumpIfTrue(_) | BcOp::JumpIfFalse(_) => self.pop_n(1),
            BcOp::Return => {
                // Return pops one value (or zero in the buffer-
                // protocol path). The abstract pop here is best-effort
                // and only matters for tail-of-function recipes;
                // subsequent recipes (post-Return) won't be consulted.
                self.pop_n(1);
            }
            BcOp::Trap(_) => {
                // Trap doesn't pop in the bytecode VM (it ignores its
                // payload). Keep `current_stack` unchanged so any
                // tail recipe stays consistent.
            }
            BcOp::CallNative { arg_count, .. } => {
                // Pops `arg_count` operands, pushes one return slot
                // tagged as Snapshot — the value can't be derived
                // from locals / consts alone (a host fn produced it).
                self.pop_n(*arg_count as usize);
                self.push_snapshot();
            }
            BcOp::CheckCap { .. } => {
                // No stack effect — capability check is a pure side
                // effect over the gate / vtable state.
            }
            BcOp::CallStdlibScalar { arg_count, .. } => {
                self.pop_n(*arg_count as usize);
                self.push_snapshot();
            }
            BcOp::ListLen => {
                // Witness op against a pre-computed length on the
                // stack — net zero effect (pops the length, pushes it
                // back). Modelled as a snapshot to keep partial-resume
                // safe even though today the value is observable.
                self.pop_n(1);
                self.push_snapshot();
            }
            // Arena-handle producers: the pushed handle is the
            // dispatcher's index into an arena slot, not re-derivable
            // from consts/locals at resume time. The deopt path reads
            // it back out of the snapshot's value_stack_copy.
            BcOp::MakeList { len } => {
                self.pop_n(*len as usize);
                self.push_snapshot();
            }
            BcOp::ListGetInt | BcOp::ListPush | BcOp::DictLookupStr => {
                // Pops [a, b], pushes one snapshot-backed slot.
                self.pop_n(2);
                self.push_snapshot();
            }
            BcOp::StrConst { .. } | BcOp::CaptureGet { .. } => {
                // Pure producers: no input pops, just a snapshot push.
                // CaptureGet's value is reproducible from the closure
                // handle but the handle isn't visible to the straight-
                // line recipe walker, so we honestly tag it Snapshot.
                self.push_snapshot();
            }
            BcOp::StrLen => {
                self.pop_n(1);
                self.push_snapshot();
            }
            BcOp::StrConcat | BcOp::StrEq | BcOp::StrGlobMatch => {
                self.pop_n(2);
                self.push_snapshot();
            }
            BcOp::StrConcatN { argc } => {
                // #165 — pops `argc` string handles in source order
                // (deepest leaf is bottom-most operand), pushes a
                // single fresh handle.
                self.pop_n(*argc as usize);
                self.push_snapshot();
            }
            BcOp::MakeDict { len } => {
                // Pops `len * 2` slots (key/value pairs), pushes one
                // dict handle.
                self.pop_n(*len as usize * 2);
                self.push_snapshot();
            }
            BcOp::MakeClosure { capture_count, .. } => {
                // M3: pops capture values, pushes one closure handle.
                self.pop_n(*capture_count as usize);
                self.push_snapshot();
            }
            BcOp::CallClosure { argc } => {
                // M3: pops `argc` args + 1 closure handle, pushes the
                // return value.
                self.pop_n(*argc as usize + 1);
                self.push_snapshot();
            }
        }
    }

    fn current_idx(&self) -> usize {
        self.ops.len()
    }

    fn compile_seq(&mut self, ops: &[TaggedOp], _depth_base: u32) -> Result<(), BcCompileError> {
        for tagged in ops {
            self.compile_one(&tagged.op)?;
        }
        Ok(())
    }

    fn compile_one(&mut self, op: &Op) -> Result<(), BcCompileError> {
        // Bind one IR PC per outer dispatch and stash it so the
        // visitor methods see a stable value while they emit / recurse.
        // Nested `compile_seq` calls (`If` / `Block` / `Loop` body
        // walks) re-enter this function and overwrite `current_pc`,
        // which is exactly the historical match-arm behaviour.
        self.current_pc = self.next_pc();
        walk_op(op, self)
    }

    /// Inline the body of `funcs[fn_index]` against the caller's
    /// stack. Pre-condition: `arg_count` operands are on the
    /// `current_stack` (top is the last argument). We:
    ///
    /// 1. Pop the `arg_count` operands into `scratch_local_top ..
    ///    scratch_local_top + arg_count` slots via `LocalSet` ops.
    /// 2. Reserve the scratch slot block (`scratch_local_top` grows
    ///    by `arg_count`).
    /// 3. Walk the callee's body, rewriting `LocalGet(N) /
    ///    LocalSet(N)` to address the scratch-block base + N, and
    ///    treating callee `Op::Return` as a fallthrough (we don't
    ///    actually return out of the caller — the value left on the
    ///    operand stack is the callee's return value).
    /// 4. Restore `scratch_local_top` so subsequent inlinings get
    ///    fresh slots.
    fn compile_inline_call(
        &mut self,
        fn_index: u32,
        arg_count: u32,
        call_pc: ExternalPc,
    ) -> Result<(), BcCompileError> {
        // 2026-05-21: Tier-2 glob_match short-circuit. The bundled
        // stdlib slot at `GLOB_MATCH_INDEX` carries a sentinel `Trap`
        // body (see `relon_ir::stdlib::defs::glob_match_string`);
        // walking it via the inliner would emit `BcOp::Trap` and never
        // produce a match. Route the call onto the dedicated
        // `BcOp::StrGlobMatch` dispatch instead so the VM defers to
        // `relon_ir::glob::glob_match` with the live string handles.
        if fn_index == relon_ir::GLOB_MATCH_INDEX && arg_count == 2 {
            self.emit_with_effect(BcOp::StrGlobMatch, call_pc);
            return Ok(());
        }
        if self.inline_depth >= MAX_INLINE_DEPTH {
            return Err(BcCompileError::UnsupportedOp(format!(
                "inline depth exceeded {} for fn_index {}",
                MAX_INLINE_DEPTH, fn_index
            )));
        }
        // v6-δ M2-B widening: the IR module's `funcs` slice only
        // contains user-defined functions; stdlib bodies are linked
        // at codegen time but the bytecode VM doesn't have a real
        // call dispatcher. Look up bundled stdlib bodies directly
        // from `builtin_stdlib()` indexed by the wire-format slot.
        // Index discipline: stdlib_function_count() returns the
        // count of bundled bodies; user funcs start at that offset.
        let callee: Func = if let Some(stdlib_func) = resolve_stdlib_func(fn_index) {
            // Lift the StdlibFunction body into a Func shape so the
            // existing inline-walker doesn't need to special-case it.
            // F-D2-G: `body_owned()` forces the lazy body cell on
            // first touch and returns an owned clone for the lifted
            // `Func` — subsequent calls hit the cached vector.
            Func {
                name: stdlib_func.name.to_string(),
                params: stdlib_func.params.clone(),
                ret: stdlib_func.ret,
                body: stdlib_func.body_owned(),
                range: relon_parser::TokenRange::default(),
            }
        } else {
            let user_idx = fn_index
                .checked_sub(relon_ir::stdlib_function_count())
                .ok_or_else(|| {
                    BcCompileError::UnsupportedOp(format!(
                        "Call fn_index {fn_index} below stdlib range; module has {} stdlib fns",
                        relon_ir::stdlib_function_count()
                    ))
                })? as usize;
            self.funcs
                .get(user_idx)
                .ok_or_else(|| {
                    BcCompileError::UnsupportedOp(format!(
                        "Call fn_index {fn_index} (user idx {user_idx}) not in module"
                    ))
                })?
                .clone()
        };
        if callee.params.len() as u32 != arg_count {
            return Err(BcCompileError::UnsupportedOp(format!(
                "Call fn_index {fn_index}: param count mismatch ({} declared vs {arg_count} args)",
                callee.params.len()
            )));
        }
        // Pop args into scratch slots (top of stack is the last arg).
        // Push the LocalSets in reverse order so slot[0] == lhs etc.
        let scratch_base = self.scratch_local_top;
        for i in (0..arg_count).rev() {
            self.emit_with_effect(BcOp::LocalSet(scratch_base + i), call_pc);
        }
        self.scratch_local_top += arg_count;
        self.inline_depth += 1;

        // Walk the callee body through the same `OpVisitor` dispatch
        // the top-level lowering uses. The active `inline_frame`
        // rewrites `LocalGet` / `LetGet` / `LetSet` against the
        // caller-side bases and turns the callee's `Return` into a
        // fallthrough; `current_pc` is pinned to `call_pc` so every
        // emitted op stays attributed to the call site.
        let ops_before = self.ops.len();
        let inline_let_base = self.scratch_local_top;
        let prev_frame = self.inline_frame.take();
        let prev_pc = self.current_pc;
        self.inline_frame = Some(InlineFrame {
            local_base: scratch_base,
            let_base: inline_let_base,
        });
        self.current_pc = call_pc;

        let walk_result = (|| -> Result<(), BcCompileError> {
            for tagged in &callee.body {
                validate_inline_op(&tagged.op)?;
                relon_ir::walk_op(&tagged.op, self)?;
                if self.ops.len() - ops_before > MAX_INLINE_OPS {
                    return Err(BcCompileError::UnsupportedOp(format!(
                        "Call fn_index {fn_index}: inline expansion >{} ops",
                        MAX_INLINE_OPS
                    )));
                }
            }
            Ok(())
        })();

        self.inline_frame = prev_frame;
        self.current_pc = prev_pc;
        walk_result?;

        self.inline_depth -= 1;
        // Restore scratch_local_top to its pre-call value so siblings
        // get fresh slots; the callee's local space stays
        // permanently reserved in the locals array (cheap — it's
        // just an integer max), but the bump pointer rolls back.
        self.scratch_local_top = scratch_base;
        Ok(())
    }

    /// Emit `op` and apply its abstract stack effect so the next
    /// op's recipe captures the post-effect operand-stack contents.
    fn emit_with_effect(&mut self, op: BcOp, pc: ExternalPc) {
        let cloned = op.clone();
        self.emit(op, pc);
        self.apply_stack_effect(&cloned);
    }

    fn input_arg_count(&self) -> u32 {
        self.field_offset_to_local.len() as u32
    }

    fn return_field_count(&self) -> u32 {
        self.return_field_offset_to_local.len() as u32
    }

    fn compile_if(
        &mut self,
        _result_ty: IrType,
        then_body: &[TaggedOp],
        else_body: &[TaggedOp],
        if_pc: ExternalPc,
    ) -> Result<(), BcCompileError> {
        if then_body.is_empty() && else_body.is_empty() {
            return Err(BcCompileError::EmptyArm { pc: if_pc });
        }
        let if_idx = self.current_idx();
        // JumpIfFalse pops the condition.
        self.emit(BcOp::JumpIfFalse(usize::MAX), if_pc);
        self.current_stack.pop();
        // Snapshot the stack at the branch boundary so both arms
        // start with identical depth.
        let branch_stack = self.current_stack.clone();

        self.compile_seq(then_body, /*depth_base=*/ 0)?;
        let after_then = self.current_idx();
        let then_jump_pc = self.next_pc();
        self.emit(BcOp::Jump(usize::MAX), then_jump_pc);
        // Capture the post-then stack so we can validate the join.
        let post_then_stack = self.current_stack.clone();

        let else_start = self.current_idx();
        // Reset to branch boundary for the else arm.
        self.current_stack = branch_stack;
        self.compile_seq(else_body, /*depth_base=*/ 0)?;
        let join = self.current_idx();

        match &mut self.ops[if_idx] {
            BcOp::JumpIfFalse(t) => *t = else_start,
            _ => unreachable!("emitted JumpIfFalse above"),
        }
        match &mut self.ops[after_then] {
            BcOp::Jump(t) => *t = join,
            _ => unreachable!("emitted Jump above"),
        }
        // At the join: rather than try to reconcile divergent stack
        // recipes (which would require phi-style metadata the
        // bytecode VM doesn't model), unify by canonicalising every
        // join-point stack slot to a fresh Snapshot index. Resume at
        // the join is then only safe via `Snapshot` payloads from
        // `value_stack_copy` — the M2-B trade-off documented in the
        // stage report.
        let join_depth = post_then_stack.len().max(self.current_stack.len());
        let mut joined = Vec::with_capacity(join_depth);
        for _ in 0..join_depth {
            let snap_idx = self.next_snapshot_idx;
            self.next_snapshot_idx += 1;
            joined.push(StackOrigin::Snapshot(snap_idx));
        }
        self.current_stack = joined;
        Ok(())
    }

    fn compile_block(&mut self, body: &[TaggedOp]) -> Result<(), BcCompileError> {
        self.labels.push(LabelFrame {
            kind: LabelKind::Block,
            target: None,
            pending_patches: Vec::new(),
        });
        self.compile_seq(body, /*depth_base=*/ 0)?;
        let frame = self.labels.pop().expect("balanced push/pop");
        let target = self.current_idx();
        for patch in frame.pending_patches {
            match &mut self.ops[patch] {
                BcOp::Jump(t) | BcOp::JumpIfTrue(t) | BcOp::JumpIfFalse(t) => *t = target,
                _ => unreachable!("Block fixup pointed at a non-jump op"),
            }
        }
        Ok(())
    }

    fn compile_loop(&mut self, body: &[TaggedOp]) -> Result<(), BcCompileError> {
        let header = self.current_idx();
        self.labels.push(LabelFrame {
            kind: LabelKind::Loop,
            target: Some(header),
            pending_patches: Vec::new(),
        });
        self.compile_seq(body, /*depth_base=*/ 0)?;
        let frame = self.labels.pop().expect("balanced push/pop");
        for patch in frame.pending_patches {
            match &mut self.ops[patch] {
                BcOp::Jump(t) | BcOp::JumpIfTrue(t) | BcOp::JumpIfFalse(t) => *t = header,
                _ => unreachable!("Loop fixup pointed at a non-jump op"),
            }
        }
        Ok(())
    }

    fn compile_br(&mut self, depth: u32, conditional: bool) -> Result<(), BcCompileError> {
        let stack_height = self.labels.len() as u32;
        if depth >= stack_height {
            return Err(BcCompileError::BranchTarget {
                depth,
                stack: stack_height,
            });
        }
        let frame_idx = (stack_height - 1 - depth) as usize;
        let placeholder_pc = self.next_pc();
        let bc_idx = self.current_idx();
        let placeholder = if conditional {
            BcOp::JumpIfTrue(usize::MAX)
        } else {
            BcOp::Jump(usize::MAX)
        };
        self.emit(placeholder, placeholder_pc);
        // Stack effect: BrIf pops the condition; Br has no effect at
        // this point (the jump leaves the rest as-is for the target).
        if conditional {
            self.current_stack.pop();
        }
        let frame = &mut self.labels[frame_idx];
        match frame.kind {
            LabelKind::Loop => {
                let header = frame
                    .target
                    .expect("loop target seeded at compile_loop entry");
                match &mut self.ops[bc_idx] {
                    BcOp::Jump(t) | BcOp::JumpIfTrue(t) => *t = header,
                    _ => unreachable!("emitted Jump/JumpIfTrue above"),
                }
            }
            LabelKind::Block => {
                frame.pending_patches.push(bc_idx);
            }
        }
        Ok(())
    }

    fn emit_ret_if_missing(&mut self) {
        if !matches!(self.ops.last(), Some(BcOp::Return)) {
            let pc = self.next_pc();
            self.emit_with_effect(BcOp::Return, pc);
        }
    }

    /// Lower `Op::Select` (`[val_true, val_false, cond] -> [chosen]`).
    ///
    /// On entry the abstract operand stack holds (bottom-up):
    /// `[val_true, val_false, cond]` (`cond` on top). We pop them in
    /// reverse order — cond first, then val_false, then val_true —
    /// each into a scratch local, then synthesise an if/else
    /// branching on the saved cond and pushing the matching value.
    /// The chosen value ends up on top; the abstract stack reflects
    /// that via a single Snapshot slot at the join.
    fn compile_select(&mut self, select_pc: ExternalPc) -> Result<(), BcCompileError> {
        let s_cond = self.scratch_local_top;
        let s_false = s_cond + 1;
        let s_true = s_false + 1;
        self.scratch_local_top += 3;

        // Drain operands: cond on top, then val_false, then val_true.
        self.emit_with_effect(BcOp::LocalSet(s_cond), select_pc);
        self.emit_with_effect(BcOp::LocalSet(s_false), select_pc);
        self.emit_with_effect(BcOp::LocalSet(s_true), select_pc);

        // Push cond + branch.
        self.emit_with_effect(BcOp::LocalGet(s_cond), select_pc);
        let cond_idx = self.current_idx();
        self.emit(BcOp::JumpIfFalse(usize::MAX), select_pc);
        self.current_stack.pop();

        // True arm: push val_true, jump to join.
        self.emit_with_effect(BcOp::LocalGet(s_true), select_pc);
        let after_true = self.current_idx();
        self.emit(BcOp::Jump(usize::MAX), select_pc);
        // Roll the abstract stack back one slot so the else arm
        // starts from the same depth as the true arm did (one item
        // post-LocalGet was pushed; the join produces a single
        // canonical slot).
        self.current_stack.pop();

        // False arm: push val_false.
        let else_start = self.current_idx();
        self.emit_with_effect(BcOp::LocalGet(s_false), select_pc);

        let join = self.current_idx();
        match &mut self.ops[cond_idx] {
            BcOp::JumpIfFalse(t) => *t = else_start,
            _ => unreachable!(),
        }
        match &mut self.ops[after_true] {
            BcOp::Jump(t) => *t = join,
            _ => unreachable!(),
        }
        // Join: both arms left one slot on the stack. Canonicalise as
        // a Snapshot slot so partial-resume at the join consults
        // value_stack_copy rather than synthesising a local read.
        // The else arm's emit_with_effect already pushed one entry;
        // we replace it with the canonical Snapshot.
        let snap_idx = self.next_snapshot_idx;
        self.next_snapshot_idx += 1;
        self.current_stack = vec![StackOrigin::Snapshot(snap_idx)];
        Ok(())
    }
}

/// Convenience: every `Op` variant outside the bytecode VM's M2-B
/// envelope (record memory, scratch alloc, raw-memory primitives,
/// native imports, closures, Unicode tables) surfaces as
/// `UnsupportedOp` so the four-way harness can route the source
/// through cranelift / tree-walker. Centralised here so every
/// matching `OpVisitor` method shares the same diagnostic shape.
#[inline]
fn unsupported(label: &'static str) -> Result<(), BcCompileError> {
    Err(BcCompileError::UnsupportedOp(format!(
        "bytecode VM has no lowering for Op::{label}"
    )))
}

/// M2-C lever 3: per-op specialization. The five arith op-flavours the
/// bytecode lowering routes through.
#[derive(Clone, Copy)]
enum ArithKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// M2-C lever 3: per-op specialization. The six cmp op-flavours.
#[derive(Clone, Copy)]
enum CmpKind {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Resolve the typed arith `BcOp` variant matching `(ty, kind)`.
/// I32 routes through the I64 variants because both ride the same
/// u64 lane on the operand stack; non-arith types (Bool / Null /
/// pointer-shaped) surface as `UnsupportedOp` so the compile pass
/// fails clearly rather than emitting an op the dispatch can't honour.
fn arith_bcop_for(ty: IrType, kind: ArithKind) -> Result<BcOp, BcCompileError> {
    let bc = match (ty, kind) {
        (IrType::I64 | IrType::I32, ArithKind::Add) => BcOp::AddI64,
        (IrType::I64 | IrType::I32, ArithKind::Sub) => BcOp::SubI64,
        (IrType::I64 | IrType::I32, ArithKind::Mul) => BcOp::MulI64,
        (IrType::I64 | IrType::I32, ArithKind::Div) => BcOp::DivI64,
        (IrType::I64 | IrType::I32, ArithKind::Mod) => BcOp::ModI64,
        (IrType::F64, ArithKind::Add) => BcOp::AddF64,
        (IrType::F64, ArithKind::Sub) => BcOp::SubF64,
        (IrType::F64, ArithKind::Mul) => BcOp::MulF64,
        (IrType::F64, ArithKind::Div) => BcOp::DivF64,
        (IrType::F64, ArithKind::Mod) => BcOp::ModF64,
        (other, _) => {
            return Err(BcCompileError::UnsupportedOp(format!(
                "bytecode arith for IrType::{other:?} outside scalar envelope",
            )));
        }
    };
    Ok(bc)
}

/// Resolve the typed cmp `BcOp` variant matching `(ty, kind)`. Same
/// routing convention as [`arith_bcop_for`]: I32 shares the I64 path,
/// non-scalar types bounce out.
fn cmp_bcop_for(ty: IrType, kind: CmpKind) -> Result<BcOp, BcCompileError> {
    let bc = match (ty, kind) {
        (IrType::I64 | IrType::I32 | IrType::Bool | IrType::Null, CmpKind::Eq) => BcOp::EqI64,
        (IrType::I64 | IrType::I32 | IrType::Bool | IrType::Null, CmpKind::Ne) => BcOp::NeI64,
        (IrType::I64 | IrType::I32, CmpKind::Lt) => BcOp::LtI64,
        (IrType::I64 | IrType::I32, CmpKind::Le) => BcOp::LeI64,
        (IrType::I64 | IrType::I32, CmpKind::Gt) => BcOp::GtI64,
        (IrType::I64 | IrType::I32, CmpKind::Ge) => BcOp::GeI64,
        (IrType::F64, CmpKind::Eq) => BcOp::EqF64,
        (IrType::F64, CmpKind::Ne) => BcOp::NeF64,
        (IrType::F64, CmpKind::Lt) => BcOp::LtF64,
        (IrType::F64, CmpKind::Le) => BcOp::LeF64,
        (IrType::F64, CmpKind::Gt) => BcOp::GtF64,
        (IrType::F64, CmpKind::Ge) => BcOp::GeF64,
        (other, _) => {
            return Err(BcCompileError::UnsupportedOp(format!(
                "bytecode cmp for IrType::{other:?} outside scalar envelope",
            )));
        }
    };
    Ok(bc)
}

/// `OpVisitor` impl driving the per-variant lowering. The dispatch
/// table is generated by `walk_op` in `relon-ir`; adding a new `Op`
/// variant forces this impl to gain a matching method, eliminating
/// the historical risk where bytecode / cranelift / wasm backends
/// drifted out of sync.
impl<'a> OpVisitor for CompileState<'a> {
    type Output = ();
    type Error = BcCompileError;

    fn visit_const_i64(&mut self, v: i64) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::ConstI64(v), pc);
        Ok(())
    }

    fn visit_const_i32(&mut self, v: i32) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::ConstI32(v), pc);
        Ok(())
    }

    fn visit_const_bool(&mut self, b: bool) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::ConstI32(if b { 1 } else { 0 }), pc);
        Ok(())
    }

    fn visit_const_f64(&mut self, _v: OrderedFloat<f64>) -> Result<(), BcCompileError> {
        // The bytecode VM operates over i64-shaped slots; F64 literals
        // arrive only through corpus paths that already bounce to
        // cranelift before compile.
        unsupported("ConstF64")
    }

    // v6-δ M2-B widening: `ConstString` / `ConstListInt` /
    // `ConstListBool` / `ConstListFloat` / `ConstListString` emit a
    // record pointer in cranelift / wasm. The bytecode VM has no
    // record memory model, so we encode them as "the length as i64"
    // — adequate for the corpus's `"...".length()` /
    // `[...].length()` / `is_empty()` patterns. The companion
    // `ReadStringLen` then becomes a no-op because the length is
    // already on the stack.
    fn visit_const_string(&mut self, _idx: u32, value: &str) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let len = value.chars().count() as i64;
        self.emit_with_effect(BcOp::ConstI64(len), pc);
        Ok(())
    }

    fn visit_const_list_int(&mut self, _idx: u32, elements: &[i64]) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::ConstI64(elements.len() as i64), pc);
        Ok(())
    }

    fn visit_const_list_float(
        &mut self,
        _idx: u32,
        elements: &[u64],
    ) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::ConstI64(elements.len() as i64), pc);
        Ok(())
    }

    fn visit_const_list_bool(
        &mut self,
        _idx: u32,
        elements: &[bool],
    ) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::ConstI64(elements.len() as i64), pc);
        Ok(())
    }

    fn visit_const_list_string(
        &mut self,
        _idx: u32,
        elements: &[String],
    ) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::ConstI64(elements.len() as i64), pc);
        Ok(())
    }

    fn visit_local_get(&mut self, idx: u32) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        // P1-20: when walking an inline callee body, the callee's
        // `LocalGet(idx)` addresses **its** local arr, which the
        // inliner has parked starting at `local_base` in the
        // caller's scratch space.
        let slot = match &self.inline_frame {
            Some(frame) => frame.local_base + idx,
            None => idx,
        };
        self.emit_with_effect(BcOp::LocalGet(slot), pc);
        Ok(())
    }

    fn visit_let_get(&mut self, idx: u32, _ty: IrType) -> Result<(), BcCompileError> {
        // Let-locals sit past the buffer-protocol arg slots. The
        // buffer-protocol layout reserves the first
        // `main_schema.fields.len()` virtual slots for inputs and the
        // return-schema field slots after that; let locals come
        // **on top of** those reserved slots so they don't collide.
        // P1-20: inside an inline callee body, `let_base` overrides
        // the buffer-protocol calculation — the callee's let-locals
        // are parked in a disjoint slot range right above the
        // callee's param scratch block so nested inlining never
        // aliases the caller's let-locals.
        let pc = self.current_pc;
        let base = match &self.inline_frame {
            Some(frame) => frame.let_base,
            None => self.input_arg_count() + self.return_field_count(),
        };
        self.emit_with_effect(BcOp::LocalGet(base + idx), pc);
        Ok(())
    }

    fn visit_let_set(&mut self, idx: u32, _ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let base = match &self.inline_frame {
            Some(frame) => frame.let_base,
            None => self.input_arg_count() + self.return_field_count(),
        };
        self.emit_with_effect(BcOp::LocalSet(base + idx), pc);
        Ok(())
    }

    fn visit_load_field(&mut self, offset: u32, _ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let slot = self
            .field_offset_to_local
            .get(&offset)
            .copied()
            .ok_or(BcCompileError::UnknownFieldOffset { offset })?;
        self.emit_with_effect(BcOp::LocalGet(slot), pc);
        Ok(())
    }

    fn visit_store_field(&mut self, offset: u32, _ty: IrType) -> Result<(), BcCompileError> {
        // Map onto a return-field virtual slot, positioned **after**
        // the input arg slots so the evaluator can read them back as
        // `locals[input_args + i]`.
        let pc = self.current_pc;
        let return_slot = self
            .return_field_offset_to_local
            .get(&offset)
            .copied()
            .ok_or(BcCompileError::UnknownFieldOffset { offset })?;
        let slot = self.input_arg_count() + return_slot;
        self.emit_with_effect(BcOp::LocalSet(slot), pc);
        Ok(())
    }

    fn visit_dict_get_by_string_key(
        &mut self,
        _shape_hash: u64,
        _value_ty: IrType,
        _entry_count_hint: Option<u32>,
        _record_len_hint: Option<u32>,
    ) -> Result<(), BcCompileError> {
        // M2-B phase 4b-continuation: real IR-lift. Stack at this point
        // is `[dict_handle, key_handle]` (the IR `[Dict, String]` shape
        // mapped onto bytecode VM handles). The producers — dict and
        // string handles — currently come from hand-built IR tests
        // (phase 4b-continuation does not yet wire the from-source
        // pipeline through `Op::MakeDict` / `Op::ConstString`-as-
        // -handle; that's phase 4c work).
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::DictLookupStr, pc);
        Ok(())
    }

    fn visit_list_get_by_int_idx(&mut self, _ty: IrType) -> Result<(), BcCompileError> {
        // M2-B phase 4b-continuation: real IR-lift. Stack at this point
        // is `[list_handle, i64 idx]` (the IR `[List, Int]` shape).
        // Producers for list handles arrive via hand-built IR until
        // the from-source pipeline switches `Op::ConstListInt` from
        // length-fold to a real `MakeList` lowering (phase 4c).
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::ListGetInt, pc);
        Ok(())
    }

    // F-D7-D: `Op::Add(IrType::String)` (source-side `s + t` lowering)
    // needs a record-aware concat — outside the bytecode VM's M2-A
    // scalar envelope. Bounce so the four-way harness routes the
    // source through `BytecodeUnsupported`. Sub / Mul / etc. with
    // String never escape the analyzer; the explicit check here is
    // belt-and-braces.
    fn visit_add(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        if ty == IrType::String {
            return Err(BcCompileError::UnsupportedOp(
                "Op::Add(IrType::String) — string concat is outside the bytecode VM's scalar envelope"
                    .to_string(),
            ));
        }
        let pc = self.current_pc;
        let bc = arith_bcop_for(ty, ArithKind::Add)?;
        self.emit_with_effect(bc, pc);
        Ok(())
    }

    // #165 — `Op::StrConcatN { operand_count }` is the IR-level fold of
    // a left-leaning `String + String + ... + String` source chain.
    // The bytecode VM has a matching `BcOp::StrConcatN { argc }`
    // (handles the single-allocation join in the `StringArena`), so
    // wire the op through directly. Two-operand `Op::Add(String)`
    // still bails via `visit_add` — the AST fold pass only emits
    // `StrConcatN` for chains of length 3+, and pair-wise concat
    // falls back to the tree-walker via `BytecodeUnsupported`.
    fn visit_str_concat_n(&mut self, operand_count: u32) -> Result<(), BcCompileError> {
        if operand_count < 2 {
            return Err(BcCompileError::UnsupportedOp(format!(
                "Op::StrConcatN with operand_count={operand_count} (expected >= 2)"
            )));
        }
        let pc = self.current_pc;
        self.emit_with_effect(
            BcOp::StrConcatN {
                argc: operand_count,
            },
            pc,
        );
        Ok(())
    }

    fn visit_sub(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let bc = arith_bcop_for(ty, ArithKind::Sub)?;
        self.emit_with_effect(bc, pc);
        Ok(())
    }

    fn visit_mul(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let bc = arith_bcop_for(ty, ArithKind::Mul)?;
        self.emit_with_effect(bc, pc);
        Ok(())
    }

    fn visit_div(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let bc = arith_bcop_for(ty, ArithKind::Div)?;
        self.emit_with_effect(bc, pc);
        Ok(())
    }

    fn visit_mod_(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let bc = arith_bcop_for(ty, ArithKind::Mod)?;
        self.emit_with_effect(bc, pc);
        Ok(())
    }

    fn visit_bit_and(&mut self, _ty: IrType) -> Result<(), BcCompileError> {
        unsupported("BitAnd")
    }

    fn visit_eq(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let bc = cmp_bcop_for(ty, CmpKind::Eq)?;
        self.emit_with_effect(bc, pc);
        Ok(())
    }

    fn visit_ne(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let bc = cmp_bcop_for(ty, CmpKind::Ne)?;
        self.emit_with_effect(bc, pc);
        Ok(())
    }

    fn visit_lt(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let bc = cmp_bcop_for(ty, CmpKind::Lt)?;
        self.emit_with_effect(bc, pc);
        Ok(())
    }

    fn visit_le(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let bc = cmp_bcop_for(ty, CmpKind::Le)?;
        self.emit_with_effect(bc, pc);
        Ok(())
    }

    fn visit_gt(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let bc = cmp_bcop_for(ty, CmpKind::Gt)?;
        self.emit_with_effect(bc, pc);
        Ok(())
    }

    fn visit_ge(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let bc = cmp_bcop_for(ty, CmpKind::Ge)?;
        self.emit_with_effect(bc, pc);
        Ok(())
    }

    fn visit_if(
        &mut self,
        result_ty: IrType,
        then_body: &[TaggedOp],
        else_body: &[TaggedOp],
    ) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.compile_if(result_ty, then_body, else_body, pc)
    }

    fn visit_block(
        &mut self,
        _result_ty: Option<IrType>,
        body: &[TaggedOp],
    ) -> Result<(), BcCompileError> {
        self.compile_block(body)
    }

    fn visit_loop_(
        &mut self,
        _result_ty: Option<IrType>,
        body: &[TaggedOp],
    ) -> Result<(), BcCompileError> {
        self.compile_loop(body)
    }

    fn visit_br(&mut self, label_depth: u32) -> Result<(), BcCompileError> {
        self.compile_br(label_depth, /*conditional=*/ false)
    }

    fn visit_br_if(&mut self, label_depth: u32) -> Result<(), BcCompileError> {
        self.compile_br(label_depth, /*conditional=*/ true)
    }

    fn visit_br_table(&mut self, _default: u32, _targets: &[u32]) -> Result<(), BcCompileError> {
        unsupported("BrTable")
    }

    fn visit_return(&mut self) -> Result<(), BcCompileError> {
        // P1-20: a callee's `Return` is a fallthrough in the inline
        // expansion — the value left on the operand stack is the
        // call's result, drained by the surrounding caller. Emitting
        // `BcOp::Return` here would exit the **caller**, not the
        // callee. Drop the terminator; the callee is straight-line
        // at this depth so falling through is safe.
        if self.inline_frame.is_some() {
            return Ok(());
        }
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::Return, pc);
        Ok(())
    }

    // v6-δ M2-B: `Op::Select` is the wasm-typed-select primitive
    // (`[val_true, val_false, cond] -> [chosen]`). Lower it through
    // the if/else scratch-local helper.
    fn visit_select(&mut self, _ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.compile_select(pc)
    }

    fn visit_trap(&mut self, kind: TrapKind) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let bc_kind = match kind {
            TrapKind::IndexOutOfBounds => BcTrapKind::IndexOutOfBounds,
            TrapKind::EmptyList => BcTrapKind::EmptyList,
            TrapKind::InvalidUtf8 => BcTrapKind::InvalidUtf8,
        };
        self.emit_with_effect(BcOp::Trap(bc_kind), pc);
        Ok(())
    }

    fn visit_load_string_ptr(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("LoadStringPtr")
    }

    fn visit_load_list_int_ptr(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("LoadListIntPtr")
    }

    fn visit_load_list_float_ptr(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("LoadListFloatPtr")
    }

    fn visit_load_list_bool_ptr(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("LoadListBoolPtr")
    }

    fn visit_load_list_string_ptr(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("LoadListStringPtr")
    }

    fn visit_load_list_schema_ptr(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("LoadListSchemaPtr")
    }

    fn visit_load_schema_ptr(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("LoadSchemaPtr")
    }

    fn visit_load_field_at_absolute(
        &mut self,
        _offset: u32,
        _ty: IrType,
    ) -> Result<(), BcCompileError> {
        unsupported("LoadFieldAtAbsolute")
    }

    fn visit_read_string_len(&mut self) -> Result<(), BcCompileError> {
        // No-op — see the `visit_const_string` comment: the
        // M2-B widening replaces the record pointer with its
        // pre-computed length, so the companion `ReadStringLen` has
        // nothing to do.
        Ok(())
    }

    // v6-δ M2-B: `AllocRootRecord` / `AllocSubRecord` / `PushRecordBase`
    // are buffer-protocol-only ops — they allocate / address slots in
    // the wasm / cranelift output buffer. The bytecode VM uses
    // virtual locals (no actual buffer), so these ops are no-ops in
    // our envelope: emit no bytecode, leave the abstract stack
    // unchanged, but consume the per-op PC (already done by
    // `compile_one`'s `next_pc` bump).
    fn visit_alloc_root_record(&mut self, _idx: u32) -> Result<(), BcCompileError> {
        Ok(())
    }

    fn visit_alloc_sub_record(
        &mut self,
        _idx: u32,
        _size: u32,
        _align: u32,
    ) -> Result<(), BcCompileError> {
        Ok(())
    }

    fn visit_store_field_at_record(
        &mut self,
        _record_local_idx: u32,
        offset: u32,
        _ty: IrType,
    ) -> Result<(), BcCompileError> {
        // Pop the operand value into the matching return-field
        // virtual slot. The `record_local_idx` is part of the buffer-
        // protocol bookkeeping (lets the wasm codegen address fields
        // relative to the record's base offset); in the bytecode VM
        // we collapse the base-relative offset to a flat virtual slot
        // via the return-schema offset table.
        let pc = self.current_pc;
        let return_slot = self
            .return_field_offset_to_local
            .get(&offset)
            .copied()
            .ok_or(BcCompileError::UnknownFieldOffset { offset })?;
        let slot = self.input_arg_count() + return_slot;
        self.emit_with_effect(BcOp::LocalSet(slot), pc);
        Ok(())
    }

    fn visit_push_record_base(&mut self, _idx: u32) -> Result<(), BcCompileError> {
        Ok(())
    }

    fn visit_emit_tail_record_from_absolute_addr(
        &mut self,
        _ty: IrType,
    ) -> Result<(), BcCompileError> {
        unsupported("EmitTailRecordFromAbsoluteAddr")
    }

    fn visit_call(
        &mut self,
        fn_index: u32,
        arg_count: u32,
        _param_tys: &[IrType],
        _ret_ty: IrType,
    ) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.compile_inline_call(fn_index, arg_count, pc)
    }

    fn visit_call_native(
        &mut self,
        import_idx: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
        cap_bit: u32,
    ) -> Result<(), BcCompileError> {
        // M2-B phase 3: lower the IR `Op::CallNative` into a real
        // bytecode op carrying the capability bit. The dispatcher
        // consults the installed `CapabilityGate` (or the legacy
        // grant table) at op-dispatch time and traps before any host
        // state is observed when the bit is denied. The host-fn
        // pointer registry that finishes the dispatch lands in phase
        // 4; today the op surfaces `BcVmError::NativeNotImplemented`
        // after the capability prong passes, matching the other
        // backends' "no registry → unsupported" envelope.
        let pc = self.current_pc;
        self.emit_with_effect(
            BcOp::CallNative {
                import_idx,
                arg_count: param_tys.len() as u32,
                cap_bit,
                ret_ty,
            },
            pc,
        );
        Ok(())
    }

    fn visit_check_cap(&mut self, cap_bit: u32) -> Result<(), BcCompileError> {
        // M2-B phase 3: standalone capability consult. The op is a
        // no-op when `cap_bit == u32::MAX` (NO_CAPABILITY_BIT) so the
        // analyzer can emit unconditional `CheckCap` ops without
        // forcing every backend to special-case the sentinel.
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::CheckCap { cap_bit }, pc);
        Ok(())
    }

    fn visit_make_closure(
        &mut self,
        _fn_table_idx: u32,
        _captures: &[ClosureCapture],
        _captures_size: u32,
    ) -> Result<(), BcCompileError> {
        unsupported("MakeClosure")
    }

    fn visit_call_closure(
        &mut self,
        _param_tys: &[IrType],
        _ret_ty: IrType,
    ) -> Result<(), BcCompileError> {
        unsupported("CallClosure")
    }

    fn visit_alloc_scratch(&mut self, _size_bytes: u32) -> Result<(), BcCompileError> {
        unsupported("AllocScratch")
    }

    fn visit_alloc_scratch_dyn(&mut self) -> Result<(), BcCompileError> {
        unsupported("AllocScratchDyn")
    }

    fn visit_load_i32_at_absolute(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("LoadI32AtAbsolute")
    }

    fn visit_load_i64_at_absolute(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("LoadI64AtAbsolute")
    }

    fn visit_load_i8u_at_absolute(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("LoadI8UAtAbsolute")
    }

    fn visit_load_f64_at_absolute(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("LoadF64AtAbsolute")
    }

    fn visit_store_i32_at_absolute(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("StoreI32AtAbsolute")
    }

    fn visit_store_i64_at_absolute(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("StoreI64AtAbsolute")
    }

    fn visit_store_i8_at_absolute(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("StoreI8AtAbsolute")
    }

    fn visit_store_f64_at_absolute(&mut self, _offset: u32) -> Result<(), BcCompileError> {
        unsupported("StoreF64AtAbsolute")
    }

    fn visit_memcpy_at_absolute(&mut self) -> Result<(), BcCompileError> {
        unsupported("MemcpyAtAbsolute")
    }

    fn visit_case_fold_table_addr(&mut self, _upper: bool) -> Result<(), BcCompileError> {
        unsupported("CaseFoldTableAddr")
    }

    fn visit_combining_mark_ranges_addr(&mut self) -> Result<(), BcCompileError> {
        unsupported("CombiningMarkRangesAddr")
    }

    fn visit_whitespace_ranges_addr(&mut self) -> Result<(), BcCompileError> {
        unsupported("WhitespaceRangesAddr")
    }

    fn visit_decomp_table_addr(&mut self, _compatibility: bool) -> Result<(), BcCompileError> {
        unsupported("DecompTableAddr")
    }

    fn visit_ccc_table_addr(&mut self) -> Result<(), BcCompileError> {
        unsupported("CccTableAddr")
    }

    fn visit_composition_table_addr(&mut self) -> Result<(), BcCompileError> {
        unsupported("CompositionTableAddr")
    }

    fn visit_full_case_fold_table_addr(&mut self, _upper: bool) -> Result<(), BcCompileError> {
        unsupported("FullCaseFoldTableAddr")
    }

    fn visit_cased_ranges_addr(&mut self) -> Result<(), BcCompileError> {
        unsupported("CasedRangesAddr")
    }

    fn visit_case_ignorable_ranges_addr(&mut self) -> Result<(), BcCompileError> {
        unsupported("CaseIgnorableRangesAddr")
    }

    fn visit_turkish_case_fold_table_addr(&mut self, _upper: bool) -> Result<(), BcCompileError> {
        unsupported("TurkishCaseFoldTableAddr")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_ir::IrType;
    use relon_parser::TokenRange;

    fn tagged(op: Op) -> TaggedOp {
        TaggedOp {
            op,
            range: TokenRange::default(),
        }
    }

    #[test]
    fn compiles_simple_add() {
        let func = Func {
            name: "f".into(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body: vec![
                tagged(Op::LocalGet(0)),
                tagged(Op::LocalGet(1)),
                tagged(Op::Add(IrType::I64)),
                tagged(Op::Return),
            ],
            range: TokenRange::default(),
        };
        let empty = BTreeMap::new();
        let bc = compile_function(&func, &empty, &empty).unwrap();
        assert_eq!(bc.locals, 2);
        assert_eq!(bc.ops.len(), 4);
        assert_eq!(bc.ir_pc_map.len(), 4);
        for w in bc.ir_pc_map.windows(2) {
            assert!(w[1] > w[0]);
        }
    }

    #[test]
    fn compiles_if_expression() {
        let func = Func {
            name: "f".into(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body: vec![
                tagged(Op::LocalGet(0)),
                tagged(Op::LocalGet(1)),
                tagged(Op::Gt(IrType::I64)),
                tagged(Op::If {
                    result_ty: IrType::I64,
                    then_body: vec![tagged(Op::LocalGet(0))],
                    else_body: vec![tagged(Op::LocalGet(1))],
                }),
                tagged(Op::Return),
            ],
            range: TokenRange::default(),
        };
        let empty = BTreeMap::new();
        let bc = compile_function(&func, &empty, &empty).unwrap();
        for op in &bc.ops {
            match op {
                BcOp::Jump(t) | BcOp::JumpIfTrue(t) | BcOp::JumpIfFalse(t) => {
                    assert!(*t <= bc.ops.len(), "branch target {t} > op count");
                    assert_ne!(*t, usize::MAX, "branch target left as placeholder");
                }
                _ => {}
            }
        }
    }

    #[test]
    fn unsupported_op_surfaces_compile_error() {
        let func = Func {
            name: "f".into(),
            params: vec![IrType::I64],
            ret: IrType::I64,
            body: vec![tagged(Op::Call {
                fn_index: 0,
                arg_count: 0,
                param_tys: vec![],
                ret_ty: IrType::I64,
            })],
            range: TokenRange::default(),
        };
        let empty = BTreeMap::new();
        let err = compile_function(&func, &empty, &empty).unwrap_err();
        assert!(matches!(err, BcCompileError::UnsupportedOp(_)));
    }

    /// M2-B phase 4b-continuation: `Op::ListGetByIntIdx` lifts cleanly
    /// into `BcOp::ListGetInt`. Pinned because the visitor previously
    /// returned `unsupported("ListGetByIntIdx")` and we want a
    /// regression alarm if the lowering bounces again.
    #[test]
    fn list_get_by_int_idx_lifts_to_list_get_int() {
        // Hand-build a function that takes a list handle in local 0
        // and pushes an i64 idx via local 1, then runs the subscript.
        // The IR `LocalGet(0)` slot would carry a real list handle
        // when wired end-to-end (today's from-source pipeline still
        // length-folds `ConstListInt`, so this is the direct-IR path).
        let func = Func {
            name: "f".into(),
            params: vec![IrType::ListInt, IrType::I64],
            ret: IrType::I64,
            body: vec![
                tagged(Op::LocalGet(0)),
                tagged(Op::LocalGet(1)),
                tagged(Op::ListGetByIntIdx {
                    element_ty: IrType::I64,
                }),
                tagged(Op::Return),
            ],
            range: TokenRange::default(),
        };
        let empty = BTreeMap::new();
        let bc = compile_function(&func, &empty, &empty).unwrap();
        assert!(
            bc.ops.iter().any(|op| matches!(op, BcOp::ListGetInt)),
            "expected BcOp::ListGetInt in lowered ops: {:?}",
            bc.ops
        );
    }

    /// M2-B phase 4b-continuation: `Op::DictGetByStringKey` lifts
    /// cleanly into `BcOp::DictLookupStr`. Same pinning rationale as
    /// the list test above.
    #[test]
    fn dict_get_by_string_key_lifts_to_dict_lookup_str() {
        let func = Func {
            name: "f".into(),
            params: vec![IrType::String, IrType::String],
            ret: IrType::I64,
            body: vec![
                tagged(Op::LocalGet(0)), // dict
                tagged(Op::LocalGet(1)), // key
                tagged(Op::DictGetByStringKey {
                    shape_hash: 0,
                    value_ty: IrType::I64,
                    entry_count_hint: None,
                    record_len_hint: None,
                }),
                tagged(Op::Return),
            ],
            range: TokenRange::default(),
        };
        let empty = BTreeMap::new();
        let bc = compile_function(&func, &empty, &empty).unwrap();
        assert!(
            bc.ops.iter().any(|op| matches!(op, BcOp::DictLookupStr)),
            "expected BcOp::DictLookupStr in lowered ops: {:?}",
            bc.ops
        );
    }
}
