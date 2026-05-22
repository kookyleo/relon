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
    // ---- M2-C lever 3: per-op specialization ----
    //
    // Each arith / cmp op is split into a per-type monomorphic variant
    // so the dispatch arm reads the operand slots directly through the
    // i64 / f64 lane without an inner `match ty` round-trip. The
    // compile pass at [`crate::compile`] lowers `Op::Add(IrType)` into
    // the matching `AddI64` / `AddF64` variant; the I32 lane shares the
    // `*I64` variants because both ride the same i64-shaped u64 slot
    // (the bytecode VM keeps no I32 storage of its own).
    //
    // The variants intentionally carry no `IrType` payload — the
    // monomorphic shape is the entire point. Tests that match the old
    // `BcOp::Add(_)` umbrella should switch to `matches!(op, BcOp::AddI64
    // | BcOp::AddF64)` (or the i64-only `BcOp::AddI64`) per the
    // intended typing of the program under test.
    /// `[i64, i64] -> [i64]`. Signed integer add with overflow check;
    /// on overflow the VM emits `RuntimeError::NumericOverflow`. Also
    /// the lowering target for `Op::Add(IrType::I32)` since the I32
    /// lane rides the same u64 slot.
    AddI64,
    /// `[i64, i64] -> [i64]`. Signed integer sub with overflow check.
    SubI64,
    /// `[i64, i64] -> [i64]`. Signed integer mul with overflow check.
    MulI64,
    /// `[i64, i64] -> [i64]`. Signed integer div. Divide-by-zero
    /// emits `RuntimeError::DivisionByZero`; `i64::MIN / -1` traps as
    /// `RuntimeError::NumericOverflow` (matches tree-walker).
    DivI64,
    /// `[i64, i64] -> [i64]`. Signed integer mod. Mod-by-zero emits
    /// `RuntimeError::DivisionByZero`.
    ModI64,
    /// `[f64, f64] -> [f64]`. IEEE-754 add.
    AddF64,
    /// `[f64, f64] -> [f64]`. IEEE-754 sub.
    SubF64,
    /// `[f64, f64] -> [f64]`. IEEE-754 mul.
    MulF64,
    /// `[f64, f64] -> [f64]`. IEEE-754 div (inf / nan per spec).
    DivF64,
    /// `[f64, f64] -> [f64]`. IEEE-754 mod (rust `%` semantics).
    ModF64,
    /// `[i64, i64] -> [bool]`. Also the lowering target for
    /// `Op::Eq(IrType::I32)`.
    EqI64,
    /// `[i64, i64] -> [bool]`. Also covers `Op::Ne(IrType::I32)`.
    NeI64,
    /// `[i64, i64] -> [bool]`. Signed integer less-than.
    LtI64,
    /// `[i64, i64] -> [bool]`.
    LeI64,
    /// `[i64, i64] -> [bool]`.
    GtI64,
    /// `[i64, i64] -> [bool]`.
    GeI64,
    /// `[f64, f64] -> [bool]`. IEEE-754 equality (NaN != NaN).
    EqF64,
    /// `[f64, f64] -> [bool]`. IEEE-754 inequality.
    NeF64,
    /// `[f64, f64] -> [bool]`.
    LtF64,
    /// `[f64, f64] -> [bool]`.
    LeF64,
    /// `[f64, f64] -> [bool]`.
    GtF64,
    /// `[f64, f64] -> [bool]`.
    GeF64,

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

    /// 2026-05-21: Tier-2 `glob_match(s, pattern) -> Bool` dispatch.
    /// `[s_handle, pattern_handle] -> [bool]`. Pops the pattern first
    /// (top-of-stack) then the haystack, looks both up in the
    /// `StringArena`, and runs [`relon_ir::glob::glob_match`]. Pushes
    /// `1` on match, `0` otherwise.
    ///
    /// Emitted by the bytecode compile pass when it spots an
    /// `Op::Call { fn_index = relon_ir::GLOB_MATCH_INDEX }` instead of
    /// walking the bundled stdlib body (the body is a sentinel `Trap`
    /// — see [`relon_ir::stdlib::defs::glob_match_string`]). Centralises
    /// the algorithm in a single Rust impl so the bytecode VM stays
    /// behaviour-equivalent with the tree-walker and cranelift backends.
    StrGlobMatch,

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

    /// M3 closure construction. Pops `capture_count` operands in
    /// declaration order (top-of-stack is the last capture), allocates
    /// a fresh closure slot in the VM's [`crate::arena::ClosureArena`]
    /// carrying the supplied `body_idx` (an index into the enclosing
    /// `BcFunction::closure_bodies` slice), and pushes the resulting
    /// handle. Stack effect: `[c_0, c_1, ..., c_{n-1}] -> [closure_handle]`.
    ///
    /// `body_idx` is resolved against the closure_bodies on the function
    /// that emitted the op. Cross-function closures are out of scope
    /// for M3 — each lambda is compiled as a sub-body of its enclosing
    /// `BcFunction`, mirroring the wasm-AOT design where the IR-level
    /// `Op::MakeClosure { fn_table_idx }` keys into the per-module
    /// funcref table. The bytecode VM compresses that into a
    /// per-function index because the bytecode crate has no module
    /// surface yet.
    MakeClosure {
        /// Index into the enclosing `BcFunction::closure_bodies` slice
        /// of the lambda body. The dispatch path looks the body up at
        /// call time via the parent function passed to `dispatch_one`.
        body_idx: u32,
        /// Number of operands the op pops to populate the captures
        /// vector. Must match the count of `BcOp::CaptureGet { idx }`
        /// references inside the lambda body; mismatches surface as
        /// out-of-range captures access at dispatch time.
        capture_count: u32,
    },

    /// M3 closure invocation. Pops `argc` arguments (top-of-stack is
    /// the last argument) then the closure handle, looks up the
    /// closure slot in the VM's [`crate::arena::ClosureArena`], and
    /// recursively dispatches the closure body. The popped args are
    /// laid out into `locals[0..argc]` of the inner invocation frame;
    /// the captures travel through a per-frame slot the dispatch loop
    /// consults via [`BcOp::CaptureGet`].
    ///
    /// Stack effect: `[closure_handle, a_0, ..., a_{argc-1}] -> [ret]`.
    /// The closure body must end with `BcOp::Return` (returning a
    /// single value); the popped return value is what the call site
    /// observes on its operand stack.
    CallClosure {
        /// Number of user-visible arguments the op pops. The argument
        /// slot count is independent of the capture count — captures
        /// live in the closure handle, not on the operand stack at the
        /// call site.
        argc: u32,
    },

    /// M3 closure capture access. Push the value at `idx` of the
    /// currently-executing closure's `captures` vector. Stack effect:
    /// `[] -> [capture_value]`. Out-of-range / outside-a-closure-body
    /// use trips [`crate::vm::BcVmError::StackUnderflow`] (compiler
    /// bug — the lowering pass should only emit `CaptureGet` inside a
    /// closure body and only against indices the matching `MakeClosure`
    /// reserved).
    CaptureGet {
        /// Index into the active closure slot's `captures` vector.
        idx: u32,
    },
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
    /// M2-B phase 4c: opaque function id used as the hot-counter slot
    /// key. `None` means the function is not eligible for trace-JIT
    /// promotion — the VM's hot-counter prologue stays inert.
    ///
    /// Mirrors the `fn_id` the cranelift HotCounter prologue uses
    /// (`relon_codegen_native::trace_install::__relon_jump_to_recorder`),
    /// so a single tracer registry can host both backends without
    /// collisions: hosts that wire bytecode + cranelift dispatchers to
    /// the same source program assign the same `fn_id` to both
    /// compiled artefacts. The bytecode crate itself never interprets
    /// the integer; it just hands it back to the
    /// [`crate::vm::HotTraceTrigger`] hook on threshold crossing.
    pub fn_id: Option<u32>,
    /// M3 closure bodies: pre-compiled lambda bodies the parent
    /// function references by index. `BcOp::MakeClosure { body_idx, .. }`
    /// keys into this slice; the dispatch path resolves the body at
    /// call time and recursively re-enters the VM dispatch loop against
    /// it.
    ///
    /// Stored as `Vec<BcFunction>` (not `Vec<Arc<BcFunction>>`) because
    /// the parent function already lives behind a hot-path pointer
    /// (`&BcFunction` flows through `BytecodeVm::dispatch_one`); the
    /// closure body's `Vec<BcOp>` indirection is the dispatch-cost slot
    /// that matters, and `Vec<BcFunction>` keeps the bodies contiguous
    /// in memory so the `body_idx` indexed read stays cache-friendly.
    /// Empty on every function that doesn't introduce lambdas — the
    /// historical M2-A scaffold path observes no behaviour change.
    pub closure_bodies: Vec<BcFunction>,
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
            fn_id: None,
            closure_bodies: Vec::new(),
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

    /// M2-B phase 4c: stamp the hot-counter `fn_id` on the compiled
    /// function. Hosts that share the trace-JIT recording registry
    /// between the cranelift backend and the bytecode VM call this
    /// after `compile_function` so the bytecode dispatch prologue
    /// routes through the same `__relon_jump_to_recorder` slot the
    /// cranelift entry function uses.
    ///
    /// Returns `self` so the call chains cleanly off a compile-pass
    /// result.
    pub fn with_fn_id(mut self, fn_id: u32) -> Self {
        self.fn_id = Some(fn_id);
        self
    }
}
