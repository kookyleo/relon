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
//! VM doesn't talk arenas; instead, [`compile_buffer_protocol`]
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
/// per-function expansion by [`MAX_INLINE_OPS`] so a maliciously
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

    Ok(BcFunction {
        ops: state.ops,
        locals,
        ir_pc_map: state.ir_pc_map,
        stack_recipe: state.stack_recipe,
    })
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

    /// Apply the abstract stack effect of `op` to `current_stack`.
    /// Called immediately after [`Self::emit`] so the next op's
    /// recorded recipe reflects the producer/consumer behaviour.
    fn apply_stack_effect(&mut self, op: &BcOp) {
        match op {
            BcOp::ConstI64(v) => self.current_stack.push(StackOrigin::Const(*v as u64)),
            BcOp::ConstI32(v) => self
                .current_stack
                .push(StackOrigin::Const(*v as u32 as u64)),
            BcOp::LocalGet(idx) => self.current_stack.push(StackOrigin::Local(*idx)),
            BcOp::LocalSet(_) => {
                self.current_stack.pop();
            }
            BcOp::Add(_)
            | BcOp::Sub(_)
            | BcOp::Mul(_)
            | BcOp::Div(_)
            | BcOp::Mod(_)
            | BcOp::Eq(_)
            | BcOp::Ne(_)
            | BcOp::Lt(_)
            | BcOp::Le(_)
            | BcOp::Gt(_)
            | BcOp::Ge(_) => {
                // Pop two operands, push one snapshot-backed result.
                self.current_stack.pop();
                self.current_stack.pop();
                let snap_idx = self.next_snapshot_idx;
                self.next_snapshot_idx += 1;
                self.current_stack.push(StackOrigin::Snapshot(snap_idx));
            }
            BcOp::Jump(_) => {
                // Unconditional jump: no stack effect at this point.
                // Branch targets reset their entry recipe via the
                // label-fixup pass; for the straight-line envelope
                // the next recipe is whatever the join produces.
            }
            BcOp::JumpIfTrue(_) | BcOp::JumpIfFalse(_) => {
                self.current_stack.pop();
            }
            BcOp::Return => {
                // Return pops one value (or zero in the buffer-
                // protocol path). The abstract pop here is best-
                // effort and only matters for tail-of-function
                // recipes; subsequent recipes (post-Return) won't be
                // consulted.
                self.current_stack.pop();
            }
            BcOp::Trap(_) => {
                // Trap doesn't pop in the bytecode VM (it ignores its
                // payload). Keep `current_stack` unchanged so any
                // tail recipe stays consistent.
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

        // Walk the callee body. We need to rewrite LocalGet/LocalSet
        // indices (which address the callee's local arr) to point
        // into our scratch block. Callee's own LetGet/LetSet are
        // index-disjoint by construction; we lift them too via a
        // private inline-let base.
        let ops_before = self.ops.len();
        let inline_let_base = self.scratch_local_top;
        // Reserve a generous slot count for callee let-locals.
        // The compile pass tracks the actual max via `locals_used`
        // post-walk; we widen `scratch_local_top` here only to keep
        // numbering disjoint.
        for tagged in &callee.body {
            self.compile_inline_one(&tagged.op, scratch_base, inline_let_base, call_pc)?;
            if self.ops.len() - ops_before > MAX_INLINE_OPS {
                return Err(BcCompileError::UnsupportedOp(format!(
                    "Call fn_index {fn_index}: inline expansion >{} ops",
                    MAX_INLINE_OPS
                )));
            }
        }

        self.inline_depth -= 1;
        // Restore scratch_local_top to its pre-call value so siblings
        // get fresh slots; the callee's local space stays
        // permanently reserved in the locals array (cheap — it's
        // just an integer max), but the bump pointer rolls back.
        self.scratch_local_top = scratch_base;
        Ok(())
    }

    /// Walk a single op inside an inlined callee body. The
    /// `local_base` is the scratch-block base the caller assigned to
    /// the callee's parameter slots; `let_base` is the base for the
    /// callee's let-locals (kept disjoint from caller let-locals so
    /// nested inlining doesn't alias).
    fn compile_inline_one(
        &mut self,
        op: &Op,
        local_base: u32,
        let_base: u32,
        call_pc: ExternalPc,
    ) -> Result<(), BcCompileError> {
        match op {
            Op::LocalGet(idx) => self.emit_with_effect(BcOp::LocalGet(local_base + *idx), call_pc),
            Op::ConstI64(v) => self.emit_with_effect(BcOp::ConstI64(*v), call_pc),
            Op::ConstI32(v) => self.emit_with_effect(BcOp::ConstI32(*v), call_pc),
            Op::ConstBool(b) => {
                self.emit_with_effect(BcOp::ConstI32(if *b { 1 } else { 0 }), call_pc)
            }
            Op::LetGet { idx, .. } => {
                self.emit_with_effect(BcOp::LocalGet(let_base + *idx), call_pc)
            }
            Op::LetSet { idx, .. } => {
                self.emit_with_effect(BcOp::LocalSet(let_base + *idx), call_pc)
            }
            Op::Add(ty) => self.emit_with_effect(BcOp::Add(*ty), call_pc),
            Op::Sub(ty) => self.emit_with_effect(BcOp::Sub(*ty), call_pc),
            Op::Mul(ty) => self.emit_with_effect(BcOp::Mul(*ty), call_pc),
            Op::Div(ty) => self.emit_with_effect(BcOp::Div(*ty), call_pc),
            Op::Mod(ty) => self.emit_with_effect(BcOp::Mod(*ty), call_pc),
            Op::Eq(ty) => self.emit_with_effect(BcOp::Eq(*ty), call_pc),
            Op::Ne(ty) => self.emit_with_effect(BcOp::Ne(*ty), call_pc),
            Op::Lt(ty) => self.emit_with_effect(BcOp::Lt(*ty), call_pc),
            Op::Le(ty) => self.emit_with_effect(BcOp::Le(*ty), call_pc),
            Op::Gt(ty) => self.emit_with_effect(BcOp::Gt(*ty), call_pc),
            Op::Ge(ty) => self.emit_with_effect(BcOp::Ge(*ty), call_pc),
            // Callee's `Return` is a fallthrough in the inlining:
            // the value left on the operand stack is the result of
            // the call (popped by the surrounding caller, if any).
            // Emitting an actual `Return` here would exit the
            // **caller** which is wrong. Skip emission entirely; the
            // callee is straight-line at this depth so dropping the
            // terminator is safe.
            Op::Return => {}
            Op::Trap { kind, .. } => {
                let bc_kind = match kind {
                    relon_ir::ir::TrapKind::IndexOutOfBounds => BcTrapKind::IndexOutOfBounds,
                    relon_ir::ir::TrapKind::EmptyList => BcTrapKind::EmptyList,
                    relon_ir::ir::TrapKind::InvalidUtf8 => BcTrapKind::InvalidUtf8,
                };
                self.emit_with_effect(BcOp::Trap(bc_kind), call_pc);
            }
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                // Reuse the existing compile_if path but inline-aware.
                if then_body.is_empty() && else_body.is_empty() {
                    return Err(BcCompileError::EmptyArm { pc: call_pc });
                }
                let if_idx = self.current_idx();
                self.emit(BcOp::JumpIfFalse(usize::MAX), call_pc);
                self.current_stack.pop();
                let branch_stack = self.current_stack.clone();

                for t in then_body {
                    self.compile_inline_one(&t.op, local_base, let_base, call_pc)?;
                }
                let after_then = self.current_idx();
                self.emit(BcOp::Jump(usize::MAX), call_pc);
                let post_then_stack = self.current_stack.clone();

                let else_start = self.current_idx();
                self.current_stack = branch_stack;
                for t in else_body {
                    self.compile_inline_one(&t.op, local_base, let_base, call_pc)?;
                }
                let join = self.current_idx();
                match &mut self.ops[if_idx] {
                    BcOp::JumpIfFalse(t) => *t = else_start,
                    _ => unreachable!(),
                }
                match &mut self.ops[after_then] {
                    BcOp::Jump(t) => *t = join,
                    _ => unreachable!(),
                }
                let join_depth = post_then_stack.len().max(self.current_stack.len());
                let mut joined = Vec::with_capacity(join_depth);
                for _ in 0..join_depth {
                    let s = self.next_snapshot_idx;
                    self.next_snapshot_idx += 1;
                    joined.push(StackOrigin::Snapshot(s));
                }
                self.current_stack = joined;
            }
            Op::Select { ty: _ } => self.compile_select(call_pc)?,
            // `ConstString` / `ReadStringLen` lifted via the same
            // M2-B constant-fold trick as the caller path.
            Op::ConstString { idx: _, value } => {
                let len = value.chars().count() as i64;
                self.emit_with_effect(BcOp::ConstI64(len), call_pc);
            }
            Op::ConstListInt { idx: _, elements } => {
                self.emit_with_effect(BcOp::ConstI64(elements.len() as i64), call_pc);
            }
            Op::ConstListBool { idx: _, elements } => {
                self.emit_with_effect(BcOp::ConstI64(elements.len() as i64), call_pc);
            }
            Op::ConstListFloat { idx: _, elements } => {
                self.emit_with_effect(BcOp::ConstI64(elements.len() as i64), call_pc);
            }
            Op::ConstListString { idx: _, elements } => {
                self.emit_with_effect(BcOp::ConstI64(elements.len() as i64), call_pc);
            }
            Op::ReadStringLen => {
                let _ = call_pc;
            }
            // Nested call: recurse via the public path (bumps inline
            // depth + reserves a fresh scratch block).
            Op::Call {
                fn_index,
                arg_count,
                ..
            } => self.compile_inline_call(*fn_index, *arg_count, call_pc)?,
            other => {
                return Err(BcCompileError::UnsupportedOp(format!(
                    "inline body op unsupported: {other:?}"
                )));
            }
        }
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
        result_ty: IrType,
        then_body: &[TaggedOp],
        else_body: &[TaggedOp],
        if_pc: ExternalPc,
    ) -> Result<(), BcCompileError> {
        let _ = result_ty;
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
        let _ = frame.kind;
        let _ = frame.target;
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
        self.emit_with_effect(BcOp::LocalGet(idx), pc);
        Ok(())
    }

    fn visit_let_get(&mut self, idx: u32, _ty: IrType) -> Result<(), BcCompileError> {
        // Let-locals sit past the buffer-protocol arg slots. The
        // buffer-protocol layout reserves the first
        // `main_schema.fields.len()` virtual slots for inputs and the
        // return-schema field slots after that; let locals come
        // **on top of** those reserved slots so they don't collide.
        let pc = self.current_pc;
        let base = self.input_arg_count() + self.return_field_count();
        self.emit_with_effect(BcOp::LocalGet(base + idx), pc);
        Ok(())
    }

    fn visit_let_set(&mut self, idx: u32, _ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        let base = self.input_arg_count() + self.return_field_count();
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
    ) -> Result<(), BcCompileError> {
        unsupported("DictGetByStringKey")
    }

    fn visit_list_get_by_int_idx(&mut self, _ty: IrType) -> Result<(), BcCompileError> {
        unsupported("ListGetByIntIdx")
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
        self.emit_with_effect(BcOp::Add(ty), pc);
        Ok(())
    }

    fn visit_sub(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::Sub(ty), pc);
        Ok(())
    }

    fn visit_mul(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::Mul(ty), pc);
        Ok(())
    }

    fn visit_div(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::Div(ty), pc);
        Ok(())
    }

    fn visit_mod_(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::Mod(ty), pc);
        Ok(())
    }

    fn visit_bit_and(&mut self, _ty: IrType) -> Result<(), BcCompileError> {
        unsupported("BitAnd")
    }

    fn visit_eq(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::Eq(ty), pc);
        Ok(())
    }

    fn visit_ne(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::Ne(ty), pc);
        Ok(())
    }

    fn visit_lt(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::Lt(ty), pc);
        Ok(())
    }

    fn visit_le(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::Le(ty), pc);
        Ok(())
    }

    fn visit_gt(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::Gt(ty), pc);
        Ok(())
    }

    fn visit_ge(&mut self, ty: IrType) -> Result<(), BcCompileError> {
        let pc = self.current_pc;
        self.emit_with_effect(BcOp::Ge(ty), pc);
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
        _import_idx: u32,
        _param_tys: &[IrType],
        _ret_ty: IrType,
        _cap_bit: u32,
    ) -> Result<(), BcCompileError> {
        unsupported("CallNative")
    }

    fn visit_check_cap(&mut self, _cap_bit: u32) -> Result<(), BcCompileError> {
        unsupported("CheckCap")
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
}
