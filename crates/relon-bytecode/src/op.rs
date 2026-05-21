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

    /// M2-B phase 3: invoke a host `#native` function.
    ///
    /// `[arg_count operands] -> [ret_ty value]`. The dispatcher:
    ///
    /// 1. Consults the installed `CapabilityGate` for `cap_bit`. A
    ///    denial trips `BcVmError::CapabilityDenied { cap_bit }`
    ///    before any args are observed.
    /// 2. Looks up the host fn in the per-call `CapabilityVtable`
    ///    native-slot table. An empty slot (no host registry wired up
    ///    yet — M2-B phase 3 ships the dispatch shape but the host-fn
    ///    pointer registry remains placeholder) also trips
    ///    `BcVmError::CapabilityDenied { cap_bit }`.
    /// 3. When implemented, the dispatcher pops `arg_count` operands
    ///    in declaration order, invokes the host fn, and pushes the
    ///    return value. Today the host slot is always empty so step
    ///    3 is unreachable; the capability prong fires at step 1 / 2.
    ///
    /// `cap_bit == u32::MAX` (`relon_ir::NO_CAPABILITY_BIT`) means
    /// "no capability required" — the gate consult is skipped and
    /// the host fn would dispatch unconditionally if present.
    CallNative {
        /// Position of the `NativeImport` entry in the module's
        /// imports table (re-emitted into bytecode for diagnostics
        /// and for the future host-fn registry lookup).
        import_idx: u32,
        /// Number of operands the host fn consumes.
        arg_count: u32,
        /// Capability bit guarding the call. `u32::MAX` skips the gate.
        cap_bit: u32,
        /// IR-level return type — used to decide what to push back
        /// on the operand stack. Today the dispatch path traps before
        /// emitting a return value so the type is informational.
        ret_ty: IrType,
    },

    /// M2-B phase 3: standalone capability consult — wasm
    /// `Op::CheckCap` lower target. `[] -> []`. Consults the installed
    /// `CapabilityGate` for `cap_bit`; denial trips
    /// `BcVmError::CapabilityDenied { cap_bit }`. `cap_bit == u32::MAX`
    /// is a no-op (matches `Op::CheckCap`'s wasm-elision semantics).
    CheckCap {
        /// Bit position in the `Capabilities` bitmap.
        cap_bit: u32,
    },

    /// M2-B phase 3: scalar-pure stdlib dispatch — pops `arg_count`
    /// operands, evaluates the matching [`BcStdlibKind`] handler, and
    /// pushes the result. `arg_count` matches the handler's declared
    /// arity; mismatches trip `BcVmError::StackUnderflow`.
    ///
    /// Reserved for stdlib bodies that operate on the bytecode VM's
    /// i64-shaped slots without touching record memory — `int.abs`,
    /// `int.min`, `int.max`. Wider stdlib (list / dict / string)
    /// requires the buffer-protocol envelope and stays unsupported
    /// per the M2-A scaffold.
    CallStdlibScalar {
        /// Which scalar-pure stdlib body to evaluate.
        kind: BcStdlibKind,
        /// Number of operands the handler pops.
        arg_count: u32,
    },

    /// M2-B phase 3: read the pre-computed length of a constant
    /// list / string slot. `[i64 len] -> [i64 len]` — a no-op against
    /// the M2-A constant-fold representation (where `Op::ConstList*`
    /// already lowers to `BcOp::ConstI64(len)`). Emitted by the
    /// compile path when an IR `Op::Call { stdlib(length) }` would
    /// have run after the fold; the op stays in the table as a
    /// witness slot so the dispatch loop has a `length`-shaped
    /// instruction to step over rather than synthesising one.
    ListLen,

    /// M2-B phase 4b: allocate a list slot from the supplied operands.
    /// `[v_0, v_1, ..., v_{len-1}] -> [list_handle]`. Pops `len` slots
    /// in declaration order (top-of-stack is `v_{len-1}`), copies them
    /// into a fresh [`crate::arena::ListArena`] slot, and pushes the
    /// handle. Element values travel through the same `u64` lane the
    /// rest of the dispatch loop uses — int / bool / f64-via-bits all
    /// share the slot shape.
    ///
    /// Phase 4b only models type-uniform lists (matching the IR
    /// `Op::ConstList*` surface). The handle is VM-local — it must
    /// not be observed across `invoke` boundaries; phase 4b-continuation
    /// adds the host-fn boundary `encode_list_for_ret` /
    /// `decode_list_arg` pair.
    MakeList {
        /// Number of operands the op pops off the stack.
        len: u32,
    },

    /// M2-B phase 4b: index into a list by integer. `[list_handle, i64
    /// index] -> [u64 element]`. Pops the index first (top-of-stack)
    /// then the handle, consults the list arena, and pushes the slot
    /// element. Out-of-range indices (including negative) trip
    /// [`crate::vm::BcVmError::IndexOutOfBounds`] without observing
    /// the element value.
    ///
    /// The pushed element travels through the same `u64` lane as the
    /// rest of the dispatch loop — consumer ops know the type from
    /// their own BcOp variant (e.g. `BcOp::Add(I64)` reads it as `i64`).
    ListGetInt,

    /// M2-B phase 4b-continuation: append `[list_handle, elem] -> [list_handle']`.
    /// Pops the element (top-of-stack) then the handle. If the
    /// underlying `Arc<Vec<u64>>` slot has refcount 1 (no other owner)
    /// the push happens in place and the handle is reused; otherwise
    /// a fresh slot is allocated holding the cloned-then-extended
    /// vector. Either way the pushed handle points at a list whose
    /// trailing element is the popped value.
    ///
    /// Phase 4b-continuation models type-uniform lists; the element
    /// travels through the same `u64` lane as the rest of the dispatch
    /// loop. Mixed-element lists are out of scope until the trace-JIT
    /// bridge revisits the storage shape (phase 4c+).
    ListPush,

    /// M2-B phase 4b-continuation: push a string handle from the
    /// per-function string constant pool. `[] -> [string_handle]`.
    /// `idx` is the index into [`BcFunction::string_pool`]; out-of-
    /// range traps `StackUnderflow` (compiler bug — the lowering pass
    /// allocates pool entries before emitting the op).
    StrConst {
        /// Index into the per-function string pool.
        idx: u32,
    },

    /// M2-B phase 4b-continuation: code-point length of a string.
    /// `[string_handle] -> [i64 len]`. The arena lift counts
    /// `String::chars()` for tree-walker parity.
    StrLen,

    /// M2-B phase 4b-continuation: concatenate two strings.
    /// `[s_lhs, s_rhs] -> [s_concat]`. Pops the right-hand side first
    /// (top-of-stack), then the left-hand side. Allocates a fresh
    /// string slot whose bytes are the concatenation; both operand
    /// strings remain reachable via their original handles.
    StrConcat,

    /// M2-B phase 4b-continuation: byte-equal string compare.
    /// `[s_lhs, s_rhs] -> [bool]`. Pops both handles, pushes `1`
    /// when their byte payloads match exactly; `0` otherwise.
    StrEq,

    /// M2-B phase 4b-continuation: allocate a dict slot from
    /// `len` key/value pairs. `[k_0, v_0, k_1, v_1, ..., k_{n-1},
    /// v_{n-1}] -> [dict_handle]`. Pops `len * 2` slots in
    /// declaration order (top-of-stack is `v_{n-1}`). Keys are
    /// interpreted as string handles (a key produced by `BcOp::StrConst`
    /// is the only supported shape in phase 4b-continuation); values
    /// travel through the same `u64` lane as the rest of the dispatch
    /// loop. Duplicate keys are stored as-is — the lookup arm scans
    /// in reverse so last-write-wins on hit (tree-walker parity).
    MakeDict {
        /// Number of key/value pairs the op pops.
        len: u32,
    },

    /// M2-B phase 4b-continuation: look up a string key in a dict.
    /// `[dict_handle, key_handle] -> [value]`. Pops the key first
    /// (top-of-stack) then the dict handle; pushes the slot value on
    /// hit, traps `IndexOutOfBounds` on miss (matches the tree-walker
    /// envelope where `dict[absent]` raises). Phase 4b-continuation
    /// keeps the storage as a `Vec<(Arc<str>, u64)>` so the scan cost
    /// stays linear; richer storage shapes are deferred to phase 4c.
    DictLookupStr,
}

/// M2-B phase 3: scalar-pure stdlib handlers the bytecode VM can
/// evaluate without record / list memory. Each variant pops the
/// declared arity and pushes a single i64 result.
///
/// The set is deliberately narrow — anything that touches list
/// elements / string bytes / dict entries needs the buffer-protocol
/// envelope (phase 4). Extending this enum is a phase-4 task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BcStdlibKind {
    /// `int.abs(i64) -> i64`. Pops one operand; pushes
    /// `i64::wrapping_abs` of it (tree-walker parity; the cranelift
    /// path emits a 2-op sequence that matches modulo wrapping).
    IntAbs,
    /// `int.min(i64, i64) -> i64`. Pops two operands; pushes the
    /// signed min.
    IntMin,
    /// `int.max(i64, i64) -> i64`. Pops two operands; pushes the
    /// signed max.
    IntMax,
}

impl BcStdlibKind {
    /// Declared arity. Used by the compile pass to validate
    /// `arg_count` matches at lower time.
    pub fn arity(self) -> u32 {
        match self {
            BcStdlibKind::IntAbs => 1,
            BcStdlibKind::IntMin | BcStdlibKind::IntMax => 2,
        }
    }
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
    /// M2-B phase 4b-continuation: per-function string constant pool.
    /// `BcOp::StrConst { idx }` indexes this vector; the dispatch arm
    /// interns the slot into the VM's [`crate::arena::StringArena`]
    /// on first touch.
    ///
    /// Stored as a `Vec<String>` (not `Vec<Arc<str>>`) because the
    /// compile-time pool is module-local and the dispatch-time
    /// interning produces fresh `Arc<str>` slots regardless. Phase
    /// 4c can revisit if pool re-use becomes hot.
    pub string_pool: Vec<String>,
}

impl Default for BcFunction {
    /// Empty bytecode function. Used as a "fill in just the fields I
    /// care about" base in hand-built tests — every field is its
    /// natural zero state so the standard `Default::default()` trick
    /// stays ergonomic even when the struct gains new optional surface
    /// (e.g. the phase 4b-continuation `string_pool` slot).
    fn default() -> Self {
        Self {
            ops: Vec::new(),
            locals: 0,
            ir_pc_map: Vec::new(),
            stack_recipe: Vec::new(),
            string_pool: Vec::new(),
        }
    }
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
