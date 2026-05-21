//! Flat bytecode opcode set + the per-function container.
//!
//! Each [`BcOp`] mirrors one [`relon_ir::Op`] variant that the
//! cranelift legacy-i64 entry shape exercises. The bytecode VM is a
//! stack machine — every variant documents its stack effect inline
//! and the resource accounting is one tick per dispatch.

use relon_ir::IrType;

/// A synthesized PC the recorder stamps onto each IR op. The
/// trace-JIT guard sites carry this opaque `u64` so the bytecode VM
/// can rehydrate "deopt to the exact next IR op" semantics via the
/// `ir_pc_map` table. `0` is reserved for the function entry slot.
///
/// Today the recorder stamps the PC as a per-function monotonic u32
/// widened to u64 (see `relon_trace_recorder::recorder`). The
/// bytecode compiler mirrors that scheme so resume requests round-
/// trip cleanly between the recorder, the trace-JIT and the VM.
pub type ExternalPc = u64;

/// One unit of bytecode work. Variants are kept flat (no nested
/// op-vector payload like [`relon_ir::Op::If`]) so the dispatch loop
/// never recurses — every branch costs one indexed jump.
#[derive(Debug, Clone, PartialEq)]
pub enum BcOp {
    /// `[] -> [i64]`. Push a 64-bit integer literal.
    ConstI64(i64),
    /// `[] -> [i32]`. Push a 32-bit boolean / null / i32 slot. Boolean
    /// values are stored as `0` / `1`; `Null` always pushes `0`.
    ConstI32(i32),
    /// `[] -> [T]`. Push the value of local slot `idx`. The slot
    /// width is `i64` regardless of the IR-level type — comparison
    /// ops down-cast when needed.
    LocalGet(u32),
    /// `[T] -> []`. Pop into local slot `idx`. Used for let-bindings.
    LocalSet(u32),
    /// `[T, T] -> [T]`. Signed add with overflow check; on overflow
    /// the VM emits `RuntimeError::NumericOverflow`.
    Add(IrType),
    /// `[T, T] -> [T]`. Signed sub with overflow check.
    Sub(IrType),
    /// `[T, T] -> [T]`. Signed mul with overflow check.
    Mul(IrType),
    /// `[T, T] -> [T]`. Signed integer / floating div. Divide-by-
    /// zero on integers emits `RuntimeError::DivisionByZero`; floats
    /// produce IEEE-754 inf / nan per spec.
    Div(IrType),
    /// `[T, T] -> [T]`. Signed integer / floating mod. Mod-by-zero
    /// on integers emits `RuntimeError::DivisionByZero` (matches
    /// tree-walker + cranelift).
    Mod(IrType),
    /// `[T, T] -> [Bool]`.
    Eq(IrType),
    /// `[T, T] -> [Bool]`.
    Ne(IrType),
    /// `[T, T] -> [Bool]`. Signed comparison for `I64`.
    Lt(IrType),
    /// `[T, T] -> [Bool]`.
    Le(IrType),
    /// `[T, T] -> [Bool]`.
    Gt(IrType),
    /// `[T, T] -> [Bool]`.
    Ge(IrType),

    /// Unconditional jump to a resolved bytecode index. The
    /// compiler pass turns IR `Br { label_depth }` into one of these
    /// against a pre-computed label table.
    Jump(usize),
    /// `[Bool] -> []`. Branch to `target` when the popped value is
    /// non-zero. Drives both wasm-style `br_if` and the `then` arm
    /// of `If { result_ty, .. }` after the compiler flattens it.
    JumpIfTrue(usize),
    /// `[Bool] -> []`. Branch to `target` when the popped value is
    /// zero. Used for the `else` arm of `If`.
    JumpIfFalse(usize),

    /// `[T] -> []`. Pop the top value and end the function. The
    /// popped value becomes the return value; arity validation is
    /// the caller's responsibility.
    Return,

    /// `[i32] -> []`. Trap with the supplied [`relon_ir::TrapKind`]
    /// code. The popped value is ignored — this carries the IR-level
    /// `Trap` op forward so a hand-built buffer test can validate
    /// the trap-prong without going through arith overflow.
    Trap(BcTrapKind),
}

/// Trap reasons the bytecode VM can raise without an extra runtime
/// guard. Mirrors a subset of [`relon_ir::TrapKind`] that the cranelift
/// legacy-i64 shape exercises; the wider IR trap surface is added in
/// M2-B alongside the partial-resume routing table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BcTrapKind {
    /// Pop-an-index-past-list-length style trap.
    IndexOutOfBounds,
    /// Operate-on-empty-list style trap.
    EmptyList,
    /// UTF-8 / encoding trap (rare for legacy-i64).
    InvalidUtf8,
    /// Capability denied (host fn slot empty in the vtable).
    CapabilityDenied,
}

/// Recipe describing how to materialise a single operand-stack slot
/// at resume time. The bytecode compiler emits a recipe per stack
/// slot at each bytecode index; the
/// `crate::evaluator::BytecodeEvaluator::resume_from_pc` consults
/// the recipe array for the target `bc_idx` to rebuild the operand
/// stack before continuing dispatch.
///
/// The recipe taxonomy is deliberately narrow — three variants cover
/// every stack slot the bytecode compiler can statically reconstruct
/// from compile-time + snapshot data. Arith / cmp result values can't
/// be reconstructed from locals alone, so the compiler emits
/// [`StackOrigin::Snapshot`] for those; the caller's
/// `DeoptStateSnapshot::value_stack_copy` carries the runtime
/// payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackOrigin {
    /// Materialise this stack slot by reading `locals[slot]` at
    /// resume entry. Used for every operand-stack value produced by
    /// a `LocalGet` / `LetGet` (and the offset-rewritten `LoadField`).
    Local(u32),
    /// Materialise this stack slot by pushing the embedded constant.
    /// Used for `ConstI64` / `ConstI32` / `ConstBool` producers.
    Const(u64),
    /// Materialise this stack slot by reading the supplied
    /// `value_stack_copy[idx]` at resume entry. Used for any stack
    /// slot whose producer is an arith / cmp op — the runtime value
    /// can't be re-derived without re-executing the producer, so the
    /// snapshot carries it inline.
    Snapshot(u32),
}

/// One compiled function. The op stream is dense (no `TaggedOp`
/// wrapper) because the bytecode VM doesn't carry source ranges on
/// every dispatch — the range comes back via `ir_pc_map` when the
/// runtime needs it for diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct BcFunction {
    /// Compiled op stream. Indexed by bytecode PC.
    pub ops: Vec<BcOp>,
    /// Number of local slots the function reads / writes. `params +
    /// let-bindings`; the VM pre-allocates a `[u64; locals]` array
    /// at call entry.
    pub locals: u32,
    /// `ir_pc_map[bc_idx] = ExternalPc` — the IR-level PC the op was
    /// lowered from. M2-A core deliverable: this is the table
    /// `Evaluator::resume_from_pc` consults when the trace-JIT
    /// deopts and asks for "next op past the failing guard".
    pub ir_pc_map: Vec<ExternalPc>,
    /// v6-δ M2-B: operand-stack recipe per bytecode index.
    ///
    /// `stack_recipe[bc_idx]` is the bottom-up list of
    /// [`StackOrigin`] entries the VM should push onto its operand
    /// stack **before** dispatching op `bc_idx` at resume time.
    ///
    /// Producer ops (LocalGet, ConstI64, Add, ...) extend the recipe
    /// by appending one entry; consumer ops (LocalSet, Return, Add's
    /// rhs+lhs pops) shrink it. Branch targets get the recipe of the
    /// dominator point — for the bytecode compiler's straight-line
    /// envelope this is sufficient because every branch target is
    /// either an empty-stack boundary (post-LocalSet) or a join
    /// point of the same depth coming from both arms.
    pub stack_recipe: Vec<Vec<StackOrigin>>,
}

impl BcFunction {
    /// Locate the bytecode index matching `external_pc`. Returns
    /// `None` when the PC was never stamped (defensive fallback —
    /// the caller restarts the function from entry in that case).
    pub fn bc_index_for_pc(&self, external_pc: ExternalPc) -> Option<usize> {
        if external_pc == 0 {
            return Some(0);
        }
        self.ir_pc_map.iter().position(|&pc| pc == external_pc)
    }

    /// Total number of bytecode ops. Used by the differential test
    /// harness to assert the compiler emitted a non-empty body.
    pub fn op_count(&self) -> usize {
        self.ops.len()
    }

    /// Expected operand-stack depth right before op `bc_idx` runs.
    /// `None` for out-of-range indices.
    pub fn stack_depth_at(&self, bc_idx: usize) -> Option<usize> {
        self.stack_recipe.get(bc_idx).map(|v| v.len())
    }
}
