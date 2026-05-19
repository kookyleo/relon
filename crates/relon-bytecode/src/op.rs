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
}
