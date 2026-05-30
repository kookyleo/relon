//! Phase Z.4.0 — IR walker scaffolding (extended with Z.4.1 Dict-return
//! and Z.4.2 control-flow support).
//!
//! Replaces the variant-per-workload [`crate::WasmProgram`] shape with
//! a real walker over [`relon_ir::Op`]. This is the canonical lowering
//! path the design doc §10.2 promised; the per-variant emit functions
//! in [`crate::programs`] now live alongside it as the fallback the
//! host (`relon-wasm-evaluator`) tries when the IR walker reports an
//! unsupported op shape.
//!
//! ## Z.4.0 scope (initial)
//!
//! The walker handles the **scalar-Int** subset that maps cleanly to a
//! `__main(i64, ..., i64) -> i64` typed-func ABI, side-stepping the
//! buffer-protocol handshake the LLVM AOT backend uses
//! (`(in_ptr, in_len, out_ptr, out_cap, caps) -> i32` per
//! `lower_workspace_single` §2.b). The host calls `__main` directly
//! with each `#main(Int n, ...)` arg as a wasm `i64`; for scalar-Int
//! returns the result comes back as an `i64` that the host wraps in
//! `Value::Int`.
//!
//! ## Z.4.2 scope (this commit) — structured control flow
//!
//! `range(n).reduce(init, (acc, i) => body)` (and the rest of the
//! `range-chain` consumer family in `relon_ir::lowering`) lowers to a
//! `Block { Loop { BrIf, Block { ... }, Br } }` skeleton with i64
//! `LetGet`/`LetSet` carrying the loop counter + accumulator. The
//! walker emits the matching wasm `block`/`loop`/`br`/`br_if`
//! primitives one-for-one — the IR's nested block depth becomes the
//! wasm verifier's structured-control-flow depth verbatim, so the
//! same `BrIf { label_depth: 1 }` the IR encodes routes to the
//! enclosing `block`'s exit on the wasm side too. No flattening, no
//! unrolling, no closed-form rewrites.
//!
//! Sources unlocked: any `range(...).reduce(...)`,
//! `range(...).map(...).reduce(...)`, nested-reduce, factorial / pow
//! style accumulator loops that previously scope-cut to the tree-
//! walker fallback. The cmp_lua W9 inline-Int variant still routes
//! via the classifier (its hand-emit predates the walker); the
//! walker covers everything outside that frozen panel.
//!
//! Out of Z.4.2 scope (still scope-cuts): `Op::ConstListInt` &
//! sibling literal pushes, `Op::ListGetByIntIdx`, the W9 production
//! `rows: range(n).map(...)` list-of-list materialization. The list-
//! literal path lands when the closure-as-value follow-up clears in
//! Z.4.3 (the production source needs both).
//!
//! ## Z.4.1 scope — Dict-return mini-ABI
//!
//! `#main(...) -> Dict { ... }` sources whose lowering ends in
//! `AllocRootRecord { idx } ... StoreFieldAtRecord ... Return` now
//! route through the walker too. The typed-func signature stays
//! `(i64, ..., i64) -> i64` — the trailing i64 carries the record
//! base pointer (zero-extended from the i32 arena offset) instead of
//! a scalar value. The host's `WasmEvaluator::run_main` recognises the
//! Dict-shape return by inspecting the IR module's `return_schema`
//! (multi-field, or single field whose name is not the canonical
//! `value`), and walks the schema layout to decode each field out of
//! linear memory into a `Value::Dict`. Closure-typed fields stay
//! scope-cut (Z.4.3); the Dict-return path here only covers scalar
//! Int / Bool record fields.
//!
//! Supported ops (Z.4.0):
//!
//! - `ConstBool`, `ConstI32`, `ConstI64` — scalar literals.
//! - `LetGet { idx, ty: Int|Bool }` / `LetSet { idx, ty: Int|Bool }` —
//!   per-function let-locals, allocated as i64/i32 wasm locals.
//! - `LoadField { offset, ty: Int }` — reads an `#main` Int parameter
//!   by buffer offset; the walker resolves the matching typed-func
//!   param via the `MainParams` schema offsets.
//! - `StoreField { offset, ty: Int }` — writing into the canonical
//!   `Ret.value` slot becomes the function return. Other offsets
//!   surface as `UnsupportedOp` until Z.4.1 lands the Dict return
//!   path.
//! - `Add(Int) / Sub(Int) / Mul(Int) / Div(Int) / Mod(Int)` — i64
//!   arithmetic.
//! - `Eq(Int) / Ne(Int) / Lt(Int) / Le(Int) / Gt(Int) / Ge(Int)` —
//!   i64 signed comparisons, results pushed as i32 booleans.
//! - `If { result_ty: Int|Bool, then_body, else_body }` — branch
//!   with a single-value yield, lowered via wasm `if (result T) ...
//!   else ... end`.
//! - `Block { result_ty: Option<Int|Bool>, body }` / `Loop { ... }`
//!   (Z.4.2) — labelled structured-control-flow regions; `result_ty
//!   == None` is the stack-neutral loop carrier shape, `Some(t)`
//!   yields a single value on exit.
//! - `Br { label_depth }` / `BrIf { label_depth }` (Z.4.2) — branch
//!   to the enclosing `Block`/`Loop` at `label_depth` (0 = innermost);
//!   `BrIf` consumes the i32 condition on top of stack.
//! - `Select { ty: Int|Bool }` — ternary `?:` lowering, lowers to
//!   wasm `select` / typed `select t`.
//! - `Return` — pops the top value into the function result.
//!
//! ## Z.4.3 scope (this commit) — closure-as-value
//!
//! `#internal fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2)` plus
//! `result: fib(n)` is the canonical W7 production source the
//! closure-as-value path unlocks. The walker now emits each
//! `#internal` lambda as its own wasm function whose signature is
//! `(captures_ptr: i32, ...user_params) -> ret_ty`, materialises a
//! funcref table + element section so `MakeClosure` /
//! `CallClosure`-stamped handles resolve at runtime, and emits
//! wasm `call_indirect` against the deduped type-pool entry for
//! each call site. Self-recursive closures (the W7 shape) stamp
//! the handle pointer into the captures struct at `MakeClosure`
//! time — same trick the LLVM AOT emitter's `emit_make_closure`
//! `not yet bound` branch uses (see
//! `crates/relon-codegen-llvm/src/emitter.rs`).
//!
//! ## Out of Z.4.x scope (stubs return `UnsupportedOp`)
//!
//! - **Z.4.2 — List literal / index / iter** (partially open):
//!   `ConstListInt`, `LoadListIntPtr`, `ListGetByIntIdx`. The
//!   range-chain consumer family (`range(n).reduce(...)`, etc.)
//!   reaches Compiled via Z.4.2; list-literal materialisation
//!   (`#internal rows: range(n).map(...)`) stays scope-cut. See
//!   [`UnsupportedOpReason::ListLiteral`].
//! - **Z.4 follow-up — String / stdlib calls**: `ConstString`,
//!   `StrConcatN`, `ReadStringLen`, `Call { ... stdlib idx ... }`.
//!   The hand-emit W3/W4 variants still cover these via the
//!   classifier path for now.
//!
//! ## Note on `UnsupportedOpReason::ClosureValue`
//!
//! The variant is retained as a defensive surface for malformed
//! closure-construction shapes the walker can't yet model
//! (e.g. a `MakeClosure` whose `fn_table_idx` exceeds the closure
//! table, or a non-`Closure`-typed self-recursive capture). The
//! canonical W7 production source no longer routes through it — it
//! emits a valid wasm module instead.
//!
//! Each sub-phase has a parking-spot constructor on
//! [`UnsupportedOpReason`] so the host's tracing layer can group
//! scope-cuts by follow-up phase without grepping op names.
//!
//! ## Honesty (design §7)
//!
//! The walker produces a wasm module whose `__main` body computes
//! exactly what the IR Op stream does, op-by-op. No
//! algorithm-substitution shortcuts; no closed-form rewrites; no
//! per-workload special cases. If the IR has a doubly-recursive
//! call shape (W7 production) we either emit the equivalent wasm
//! calls or scope-cut to the existing hand-emit fallback —
//! the walker never silently swaps in an iterative form.

use relon_eval_api::layout::{OffsetTable, SchemaLayout};
use relon_eval_api::schema_canonical::{Schema, TypeRepr};
use relon_ir::{
    IrType, LoweredEntry, Op, TaggedOp, MAIN_RETURN_SCHEMA_NAME, RETURN_VALUE_FIELD_NAME,
};
use wasm_encoder::{
    CodeSection, ConstExpr, ElementSection, Elements, EntityType, ExportKind, ExportSection,
    Function, FunctionSection, ImportSection, MemArg, MemorySection, MemoryType, Module, RefType,
    TableSection, TableType, TypeSection, ValType,
};

use crate::host_abi::{import_index, HOST_IMPORTS};
use crate::LowerError;

/// Why the IR walker declined to lower a given op. Carried in
/// [`LowerError::UnsupportedOp`] so the host's diagnostic layer can
/// group scope-cuts by follow-up phase (Z.4.1 Dict, Z.4.2 List,
/// Z.4.3 Closure, ...) without grepping op names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsupportedOpReason {
    /// Z.4.1 follow-up: Dict-return or Dict-literal construction.
    /// Triggered by `AllocRootRecord`, `AllocSubRecord`,
    /// `StoreFieldAtRecord`, `PushRecordBase`,
    /// `EmitTailRecordFromAbsoluteAddr` ops.
    DictReturn,
    /// Z.4.2 follow-up: List literal / index / nested iter.
    /// Triggered by `ConstListInt`, `LoadListIntPtr`,
    /// `ListGetByIntIdx` ops.
    ListLiteral,
    /// Z.4.3 follow-up: First-class closure value.
    /// Triggered by `MakeClosure`, `CallClosure` ops.
    ClosureValue,
    /// Z.4 follow-up: String / stdlib host calls. The hand-emit
    /// variants in [`crate::programs`] still cover W3/W4 here.
    StringOrStdlib,
    /// Z.4 follow-up: Float arithmetic. The buffer-protocol layer is
    /// the natural home for these; Z.4.0 stays Int-only.
    FloatArithmetic,
    /// The op is part of the buffer-protocol record handshake the
    /// walker side-steps (the typed-func ABI does not need it).
    /// Reached when the IR contains a non-Ret-value `StoreField` or
    /// a non-MainParams `LoadField` — both signal the source needs
    /// the buffer-protocol path, not the typed-func fast lane.
    BufferProtocolRecord,
    /// Catch-all: the op exists in `relon_ir::Op` but the Z.4.0
    /// walker hasn't wired it. Carries the op's debug name so the
    /// scope-cut shows up in tracing.
    Other(&'static str),
}

impl UnsupportedOpReason {
    /// Static tag for diagnostics. Stable across Z.4.x — host code
    /// can switch on it without depending on the enum's `Debug`
    /// projection.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::DictReturn => "Z.4.1-dict-return",
            Self::ListLiteral => "Z.4.2-list-literal",
            Self::ClosureValue => "Z.4.3-closure-value",
            Self::StringOrStdlib => "Z.4-string-stdlib",
            Self::FloatArithmetic => "Z.4-float-arith",
            Self::BufferProtocolRecord => "Z.4-buffer-protocol",
            Self::Other(name) => name,
        }
    }
}

/// Per-param info derived from the `MainParams` schema offset table.
/// Maps the IR `LoadField { offset }` operand to the typed-func wasm
/// param index.
#[derive(Debug, Clone)]
struct ParamSlot {
    /// IR-side `LoadField { offset }` operand. Resolves uniquely to
    /// the matching param within `MainParams`.
    offset: u32,
    /// Wasm typed-func param index (0-based). For a `#main(Int n)`
    /// program with a single Int param this is `0`.
    wasm_param_idx: u32,
    /// Declared IR type — Z.4.0 only accepts `Int`. Float / Bool /
    /// String params surface as `LowerError::UnsupportedOp` until
    /// Z.4 follow-ups widen the typed-func envelope.
    ty: IrType,
}

/// Z.4.3 — which function variant the walker is emitting. Drives
/// per-op semantics that differ between the `#main` entry and a
/// lambda body (e.g. `LocalGet(N)` is unused in the entry — params
/// arrive via `LoadField` against the `MainParams` layout — but is
/// the canonical way a lambda reads its `(captures_ptr, ...args)`
/// signature).
#[derive(Debug, Clone)]
enum FunctionKind<'a> {
    /// `#main` entry function. Param decoding goes through the
    /// `MainParams` schema → typed-func i64 mapping; return decoding
    /// follows the `ReturnShape` enum.
    Entry {
        param_slots: &'a [ParamSlot],
        return_shape: &'a ReturnShape,
    },
    /// One `#internal fib: (k) => ...` lambda function. The wasm
    /// signature is `(captures_ptr: i32, ...user_params) -> ret_ty`;
    /// `LocalGet(0)` reads the captures pointer, `LocalGet(i+1)` reads
    /// the i-th user-visible arg. `Return` ends the function by
    /// yielding the top-of-stack value (which must already be
    /// `ret_ty`).
    Lambda {
        /// Wasm param valtypes in declaration order. `params[0]` is
        /// always `i32` (captures_ptr); `params[1..]` are the
        /// user-visible argument types.
        params: &'a [IrType],
        /// IR-level return type. Recorded here so a future
        /// type-checking widening can verify the top-of-stack type
        /// at `Op::Return`; the current op-by-op walker trusts the
        /// IR pipeline's invariants.
        #[allow(dead_code)]
        ret_ty: IrType,
    },
}

/// How the IR walker interprets the trailing `Return` op.
///
/// The typed-func signature stays `(i64, ...) -> i64` in both shapes;
/// the i64 result's *meaning* differs:
///
/// - [`ReturnShape::ScalarValue`] — the i64 is the user's scalar Int
///   return (Z.4.0 behaviour). The body's trailing
///   `StoreField { offset: ret.value }` leaves the value on the wasm
///   operand stack, and the function's implicit `End` returns it.
/// - [`ReturnShape::DictRecordPtr`] — the i64 is a zero-extended i32
///   arena pointer to a record that the body populated via
///   `AllocRootRecord` + `StoreFieldAtRecord`. The host walks the
///   schema layout to decode each field out of linear memory.
#[derive(Debug, Clone)]
enum ReturnShape {
    /// The canonical `Ret { value: Int }` wrapper. Single-field
    /// schema named [`MAIN_RETURN_SCHEMA_NAME`] with field name
    /// [`RETURN_VALUE_FIELD_NAME`] and `Int` type.
    ScalarValue {
        /// Buffer offset of the `value` field. Used to match
        /// `StoreField { offset }` against the return path.
        offset: u32,
        /// Declared IR type. Z.4.0 only accepts `Int`.
        #[allow(dead_code)]
        ty: IrType,
    },
    /// Z.4.1 — Dict-return shape. The body emits
    /// `AllocRootRecord { idx }` to bind a record-base local, walks
    /// the dict body emitting `StoreFieldAtRecord { idx, offset, ty }`
    /// per field, then `Return` (which loads the record-base local,
    /// zero-extends to i64, and returns it as the typed-func result).
    ///
    /// The walker side only needs the alloc size + alignment to
    /// emit the `__relon_arena_alloc` call; the matching per-field
    /// offsets travel with each `StoreFieldAtRecord` op directly,
    /// and the host-side Dict decode re-derives them from the IR
    /// module's `return_schema` (no need to ship the layout through
    /// the walker output).
    DictRecordPtr {
        /// Total fixed-area size of the record (= `arena_alloc` size
        /// arg the walker passes to the host import at the matching
        /// `AllocRootRecord` op). Padded to the schema's natural
        /// alignment so subsequent record allocs in nested Dicts
        /// align cleanly when Z.4.1+ widens this path.
        root_size: u32,
        /// Schema layout's natural alignment in bytes (`arena_alloc`'s
        /// `align` arg). Mirrors the LLVM-side rule that the root
        /// record's first field's alignment dominates.
        root_align: u32,
    },
    /// Z.4.2 — `List<Int>`-return shape. The body emits one or more
    /// `Op::ConstListInt` ops (each materialised into the wasm
    /// module's data section) plus a trailing
    /// `StoreField { offset: 0, ty: ListInt }` whose effect is to
    /// leave the list-record's absolute pointer (i32) on the wasm
    /// stack. The function's implicit `End` then returns it as an
    /// i64 — wasm's typed-func `i64` result carries the zero-extended
    /// pointer; the host's `WasmEvaluator::run_main` recognises the
    /// shape from the IR module's `return_schema` and decodes the
    /// list out of linear memory into a `Value::List`.
    ///
    /// Today's reach: bare-literal returns
    /// (`#main(...) -> List<Int>\n[1, 2, 3]`) plus future shapes that
    /// flow a constant list through the body (let-bound literal,
    /// ternary-selected literal). Dynamic list construction
    /// (`range(n).map(...)` materialising into a fresh list) lands
    /// alongside the Z.4.3 closure-as-value follow-up — it needs a
    /// runtime list-builder against the arena allocator, not a
    /// data-section blob.
    ListIntPtr {
        /// Buffer offset of the `Ret.value` slot the IR's lowering
        /// pass writes the list pointer to. The walker matches a
        /// `StoreField { offset, ty: ListInt }` against this offset
        /// so a non-Ret list write would still scope-cut (the buffer
        /// protocol's parallel Ret slot isn't visible through the
        /// typed-func ABI).
        offset: u32,
    },
}

/// Lower a workspace's IR module into a wasm binary with a
/// `__main(i64, ..., i64) -> i64` typed-func signature.
///
/// The walker is the canonical Z.4.0+ entry point: it walks the IR
/// Op stream, emits wasm instructions one-to-one, and surfaces
/// scope-cuts as [`LowerError::UnsupportedOp`] so the host can route
/// the source to the hand-emit fallback or the tree-walker tier.
///
/// On success the emitted module:
///
/// - exports a single linear memory (`memory`, 16 pages),
/// - exports `__main` with one `i64` param per `#main` declared param,
/// - returns the result of `Ret.value` as an `i64`.
///
/// Programs that need linear-memory traffic (Dict, List, String)
/// scope-cut here today; Z.4.1+ widens the surface incrementally.
pub fn lower_ir_module(lowered: &LoweredEntry) -> Result<Vec<u8>, LowerError> {
    let entry_idx = lowered
        .module
        .entry_func_index
        .ok_or(LowerError::UnsupportedOp(
            "no_entry_func",
            UnsupportedOpReason::Other("no_entry_func"),
        ))?;
    let entry_fn = &lowered.module.funcs[entry_idx];

    // --- Resolve MainParams offsets --------------------------------------
    //
    // The IR's `Func::params` slot is pinned to the buffer-protocol
    // 4-slot (`in_ptr, in_len, out_ptr, out_cap`) + caps i64. We
    // ignore those and walk the canonical `MainParams` schema instead
    // — that's what the body's `LoadField { offset }` ops reference.
    let params_layout = SchemaLayout::offsets_for(&lowered.main_schema).map_err(|e| {
        LowerError::UnsupportedOp(
            "main_params_layout",
            UnsupportedOpReason::Other(map_layout_err(&e)),
        )
    })?;
    let mut param_slots: Vec<ParamSlot> = Vec::with_capacity(params_layout.fields.len());
    for (i, f) in params_layout.fields.iter().enumerate() {
        let decl_ty = type_repr_to_ir(&lowered.main_schema.fields[i].ty)?;
        if decl_ty != IrType::I64 {
            return Err(LowerError::UnsupportedOp(
                "non_int_main_param",
                non_int_main_param_reason(&decl_ty),
            ));
        }
        param_slots.push(ParamSlot {
            offset: f.offset as u32,
            wasm_param_idx: i as u32,
            ty: decl_ty,
        });
    }

    // --- Classify the return shape --------------------------------------
    //
    // The IR pipeline emits two return shapes that the walker can lower:
    //
    // - Canonical `Ret { value: Int }` (Z.4.0) — single-field record
    //   whose field is named [`RETURN_VALUE_FIELD_NAME`]. The body's
    //   trailing `StoreField { offset: ret.value }` leaves the i64 on
    //   the operand stack and the typed-func returns it directly.
    // - Dict-return (Z.4.1) — multi-field record OR single-field
    //   record whose field name is not "value" (the anon-Dict-return
    //   path reuses the `Ret` schema name but renames the field after
    //   the user's `result:` dict key). The body emits
    //   `AllocRootRecord` + per-field `StoreFieldAtRecord` ops; the
    //   walker turns the first AllocRootRecord into a host
    //   `__relon_arena_alloc(root_size, root_align)` call, the
    //   StoreFieldAtRecord ops into typed stores against the
    //   record-base local, and the trailing `Return` zero-extends
    //   the record-base i32 into the typed-func's i64 result.
    if lowered.return_schema.name != MAIN_RETURN_SCHEMA_NAME {
        return Err(LowerError::UnsupportedOp(
            "return_schema_unexpected_name",
            UnsupportedOpReason::Other("return_schema_unexpected_name"),
        ));
    }
    let ret_layout = SchemaLayout::offsets_for(&lowered.return_schema).map_err(|e| {
        LowerError::UnsupportedOp("ret_layout", UnsupportedOpReason::Other(map_layout_err(&e)))
    })?;
    let return_shape = classify_return_shape(&lowered.return_schema, &ret_layout)?;

    // --- Walk the entry body --------------------------------------------
    let mut entry_emit = EmitState::new(FunctionKind::Entry {
        param_slots: &param_slots,
        return_shape: &return_shape,
    });
    entry_emit.walk(&entry_fn.body)?;

    // --- Walk each lambda body ------------------------------------------
    //
    // The IR module's `closure_table` is a list of IR func indices,
    // one per `#internal fib: (k) => ...` lambda lifted by the
    // lowering pass. Each lambda's wasm signature is `(captures_ptr:
    // i32, ...user_params) -> ret_ty` — i.e. exactly what the
    // closure-as-value `MakeClosure`/`CallClosure` discipline writes
    // into the funcref table. We walk every lambda eagerly so the
    // emitted module can resolve every `call_indirect`'s type-index
    // against a single deduped signature pool.
    let closure_table = &lowered.module.closure_table;
    let mut lambda_emits: Vec<(LambdaInfo, EmitState<'_>)> =
        Vec::with_capacity(closure_table.len());
    for (slot, ir_fn_idx) in closure_table.iter().enumerate() {
        let lambda_fn =
            lowered
                .module
                .funcs
                .get(*ir_fn_idx as usize)
                .ok_or(LowerError::UnsupportedOp(
                    "closure_table_idx_out_of_range",
                    UnsupportedOpReason::ClosureValue,
                ))?;
        if lambda_fn.params.is_empty()
            || !matches!(lambda_fn.params[0], IrType::I32 | IrType::Closure)
        {
            return Err(LowerError::UnsupportedOp(
                "lambda_missing_captures_ptr_param",
                UnsupportedOpReason::ClosureValue,
            ));
        }
        let info = LambdaInfo {
            slot: slot as u32,
            params: lambda_fn.params.clone(),
            ret_ty: lambda_fn.ret,
        };
        let mut emit = EmitState::new(FunctionKind::Lambda {
            params: &lambda_fn.params,
            ret_ty: lambda_fn.ret,
        });
        // Safety/lifetime: the borrow on `lambda_fn.params` lives
        // only as long as this iteration; we extract the emitted
        // instructions + aux locals into `info` immediately after
        // the walk so the borrow can drop.
        emit.walk(&lambda_fn.body)?;
        // We need to drop the &lambda_fn.params borrow before
        // pushing into the Vec (which contains an EmitState borrowing
        // the same lifetime). Move the EmitState carefully.
        lambda_emits.push((info, emit));
    }

    // --- Assemble the wasm module ----------------------------------------
    //
    // Determine whether the module needs the host-imports section.
    // Both Z.4.1 (Dict-return alloc) and Z.4.3 (MakeClosure / captures
    // alloc) drive `__relon_arena_alloc` calls; we OR across the
    // entry + every lambda so a lambda-only call still wires imports.
    let n_params = param_slots.len();
    // Z.4.1 / Z.4.3 — host-imports section is wired when the entry OR
    // any lambda emitted an arena-alloc host call. Z.4.3 lambdas allocate
    // captures + handle storage, the entry allocates the root record for
    // Dict-return shapes, so either side can drive the import.
    let needs_imports =
        entry_emit.needs_arena_alloc || lambda_emits.iter().any(|(_, e)| e.needs_arena_alloc);
    let has_lambdas = !lambda_emits.is_empty();
    // Z.4.2 — pull the const-list data-segment table out of entry_emit
    // before `finalise` consumes it. The walker today only emits
    // `Op::ConstListInt` from the entry body (List<Int> return shapes
    // are `#main`-only); a defensive check below catches the case
    // where a lambda body produced const-list entries we haven't
    // designed offset-aggregation for yet.
    let const_list_data = std::mem::take(&mut entry_emit.const_list_ints);
    for (_, lambda_emit) in &lambda_emits {
        if !lambda_emit.const_list_ints.is_empty() {
            return Err(LowerError::UnsupportedOp(
                "lambda_const_list_int_not_supported",
                UnsupportedOpReason::ListLiteral,
            ));
        }
    }
    let mut module = Module::new();

    // --- Build the wasm type pool ---------------------------------------
    //
    // Type 0: entry signature `(i64; n_params) -> i64`. Type 1+: host-
    // import sigs (one slot per `HOST_IMPORTS` entry, deduped); then
    // one slot per lambda's `(i32, ...) -> ret` signature, also
    // deduped; then one slot per distinct `CallClosure` signature
    // (`(i32 captures_ptr, ...param_tys) -> ret_ty`). Deduplication
    // is by `(params, results)` tuple so a lambda's signature and
    // the matching call_indirect signature collapse to one entry.
    let mut types = TypeSection::new();
    let mut sig_pool: Vec<(Vec<ValType>, Vec<ValType>)> = Vec::new();
    let intern_sig = |pool: &mut Vec<(Vec<ValType>, Vec<ValType>)>,
                      types: &mut TypeSection,
                      sig: (Vec<ValType>, Vec<ValType>)|
     -> u32 {
        if let Some(p) = pool.iter().position(|s| s == &sig) {
            return p as u32;
        }
        let idx = pool.len() as u32;
        types
            .ty()
            .function(sig.0.iter().copied(), sig.1.iter().copied());
        pool.push(sig);
        idx
    };
    let entry_type_idx = intern_sig(
        &mut sig_pool,
        &mut types,
        (vec![ValType::I64; n_params], vec![ValType::I64]),
    );

    // Per-host-import type entries (only when imports are wired).
    let mut host_type_indices: Vec<u32> = Vec::new();
    if needs_imports {
        host_type_indices.reserve(HOST_IMPORTS.len());
        for imp in HOST_IMPORTS {
            let sig = (imp.params.to_vec(), imp.results.to_vec());
            host_type_indices.push(intern_sig(&mut sig_pool, &mut types, sig));
        }
    }

    // Per-lambda type indices. A lambda's signature is its IR-side
    // `Func::params` translated to wasm valtypes, returning its IR
    // `ret`'s wasm valtype.
    let mut lambda_type_indices: Vec<u32> = Vec::with_capacity(lambda_emits.len());
    for (info, _) in &lambda_emits {
        let params: Vec<ValType> = info
            .params
            .iter()
            .map(|t| ir_ty_to_wasm_param_valtype(*t))
            .collect::<Result<Vec<_>, _>>()?;
        let result = vec![ir_ty_to_wasm_param_valtype(info.ret_ty)?];
        lambda_type_indices.push(intern_sig(&mut sig_pool, &mut types, (params, result)));
    }

    // Per-`CallClosure` type indices: one per (param_tys, ret_ty)
    // pair the walker recorded across the entry + every lambda.
    // Collected in walk order so the post-walk patcher matches the
    // sentinels left in each `EmitState::insns`.
    fn cc_sig(
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(Vec<ValType>, Vec<ValType>), LowerError> {
        let mut params: Vec<ValType> = Vec::with_capacity(1 + param_tys.len());
        params.push(ValType::I32); // implicit captures_ptr
        for t in param_tys {
            params.push(ir_ty_to_wasm_param_valtype(*t)?);
        }
        let result = vec![ir_ty_to_wasm_param_valtype(ret_ty)?];
        Ok((params, result))
    }
    // Resolve CallClosure type-indices per-EmitState (in walk order).
    let entry_call_indirect_type_indices: Vec<u32> = entry_emit
        .call_indirect_sigs_requested
        .iter()
        .map(|(p, r)| -> Result<u32, LowerError> {
            Ok(intern_sig(&mut sig_pool, &mut types, cc_sig(p, *r)?))
        })
        .collect::<Result<_, _>>()?;
    let mut lambda_call_indirect_type_indices: Vec<Vec<u32>> =
        Vec::with_capacity(lambda_emits.len());
    for (_, e) in &lambda_emits {
        let v: Vec<u32> = e
            .call_indirect_sigs_requested
            .iter()
            .map(|(p, r)| -> Result<u32, LowerError> {
                Ok(intern_sig(&mut sig_pool, &mut types, cc_sig(p, *r)?))
            })
            .collect::<Result<_, _>>()?;
        lambda_call_indirect_type_indices.push(v);
    }

    module.section(&types);

    // Section 2 — imports (Z.4.1 / Z.4.3 arena alloc).
    if needs_imports {
        let mut imports = ImportSection::new();
        for (i, imp) in HOST_IMPORTS.iter().enumerate() {
            imports.import(
                imp.module,
                imp.name,
                EntityType::Function(host_type_indices[i]),
            );
        }
        module.section(&imports);
    }

    // Section 3 — functions: declare entry + each lambda. The wasm
    // function index of `__main` lands at
    // `HOST_IMPORTS.len()` when imports are wired (imports occupy the
    // low end of the fn-index namespace) and `0` otherwise; lambdas
    // follow immediately. The matching closure-table slots are
    // resolved into wasm fn-indices via `lambda_fn_index(slot)`
    // below.
    let mut funcs = FunctionSection::new();
    funcs.function(entry_type_idx);
    for ti in &lambda_type_indices {
        funcs.function(*ti);
    }
    module.section(&funcs);

    // Section 4 — table (Z.4.3 funcref). One funcref table sized to
    // the lambda count; entries are populated via the element
    // section below. Skipped when no lambdas — keeps the Z.4.0/Z.4.2
    // modules byte-identical.
    let host_import_count = if needs_imports {
        HOST_IMPORTS.len() as u32
    } else {
        0
    };
    let main_fn_idx = host_import_count;
    let lambda_fn_index = |slot: u32| -> u32 { main_fn_idx + 1 + slot };

    if has_lambdas {
        let mut tables = TableSection::new();
        tables.table(TableType {
            element_type: RefType::FUNCREF,
            table64: false,
            minimum: lambda_emits.len() as u64,
            maximum: Some(lambda_emits.len() as u64),
            shared: false,
        });
        module.section(&tables);
    }

    // Section 5 — memories: one 16-page linear memory, exported as
    // `memory`. Z.4.0 scalar-Int ops don't touch it; Z.4.1 Dict-return
    // sources use it for the record alloc; Z.4.3 closure-as-value
    // sources use it for handle + captures storage.
    let mut mems = MemorySection::new();
    mems.memory(MemoryType {
        minimum: 16,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&mems);

    // Section 7 — exports: memory + __main.
    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("__main", ExportKind::Func, main_fn_idx);
    module.section(&exports);

    // Section 9 — elements: populate the funcref table with each
    // lambda's wasm function index in closure-table slot order. The
    // call_indirect at each `Op::CallClosure` site reads the slot
    // out of the closure handle and looks the funcref up here.
    if has_lambdas {
        let mut elements = ElementSection::new();
        let fn_indices: Vec<u32> = (0..lambda_emits.len())
            .map(|i| lambda_fn_index(i as u32))
            .collect();
        let offset = ConstExpr::i32_const(0);
        elements.active(
            None,
            &offset,
            Elements::Functions(fn_indices.as_slice().into()),
        );
        module.section(&elements);
    }

    // Section 10 — code: entry function + every lambda. We patch
    // each EmitState's recorded `CallIndirect { type_index: u32::MAX
    // }` placeholders in walk order before finalising.
    let mut code = CodeSection::new();
    patch_call_indirect_type_indices(&mut entry_emit.insns, &entry_call_indirect_type_indices)?;
    let entry_func = entry_emit.finalise()?;
    code.function(&entry_func);
    for ((_, mut emit), patches) in lambda_emits
        .into_iter()
        .zip(lambda_call_indirect_type_indices.iter())
    {
        patch_call_indirect_type_indices(&mut emit.insns, patches)?;
        let func = emit.finalise()?;
        code.function(&func);
    }
    module.section(&code);

    // Section 11 — data: active data segments for `Op::ConstListInt`
    // literals. Each entry installs the matching list record
    // (`[len: u32 LE][pad: u32 zero][i64 elements...]`) at the
    // resolved absolute offset. Empty list keeps modules without
    // list literals byte-identical to the Z.4.0/Z.4.1 emit.
    if !const_list_data.is_empty() {
        let mut data = wasm_encoder::DataSection::new();
        for entry in &const_list_data {
            let bytes = encode_const_list_int_record(entry);
            data.active(
                0,
                &wasm_encoder::ConstExpr::i32_const(entry.abs_offset as i32),
                bytes.iter().copied(),
            );
        }
        module.section(&data);
    }

    Ok(module.finish())
}

/// Z.4.2 — serialize one `Op::ConstListInt` record into its
/// little-endian wire form. Layout: `[len: u32 LE][pad: u32 zero][i64
/// elements...]`. The pad keeps the i64 payload 8-aligned inside the
/// record when the record itself sits at an 8-aligned absolute
/// offset (the walker's `intern_const_list_int` cursor guarantees
/// this for every entry).
fn encode_const_list_int_record(entry: &ConstListIntEntry) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + 8 * entry.elements.len());
    let len_u32 = entry.elements.len() as u32;
    bytes.extend_from_slice(&len_u32.to_le_bytes());
    // 4-byte pad to push elements onto an 8-byte boundary inside the
    // record. Mirrors the LLVM AOT layout exactly.
    bytes.extend_from_slice(&[0u8; 4]);
    for v in &entry.elements {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes
}

/// Z.4.3 — per-lambda metadata captured before its `EmitState` is
/// walked. Used by `lower_ir_module` to compute the lambda's wasm
/// signature + funcref-table slot when assembling the module-level
/// sections.
#[derive(Debug, Clone)]
struct LambdaInfo {
    /// Closure-table slot (0-based, source order). Matches
    /// `Op::MakeClosure { fn_table_idx }`.
    #[allow(dead_code)]
    slot: u32,
    /// IR-side wasm parameter types — `params[0]` is always
    /// captures_ptr (i32), `params[1..]` are the user-visible args.
    params: Vec<IrType>,
    /// IR-side return type. Mapped to the wasm fn's single-result
    /// slot.
    ret_ty: IrType,
}

/// Walker-side IrType → wasm `ValType` mapping for the lambda
/// signature / call_indirect signature lanes. Closure-typed slots
/// are i32 arena handles; list / string / null pointers are also
/// i32. Anything else surfaces as `UnsupportedOp` so the host's
/// tracing layer pins the scope-cut to the matching follow-up.
fn ir_ty_to_wasm_param_valtype(ty: IrType) -> Result<ValType, LowerError> {
    match ty {
        IrType::I64 => Ok(ValType::I64),
        IrType::F64 => Ok(ValType::F64),
        IrType::Bool | IrType::I32 => Ok(ValType::I32),
        IrType::Closure
        | IrType::Null
        | IrType::String
        | IrType::ListInt
        | IrType::ListFloat
        | IrType::ListBool
        | IrType::ListString
        | IrType::ListSchema => Ok(ValType::I32),
    }
}

/// Patch the `CallIndirect { type_index: u32::MAX }` sentinels left
/// in the walker's recorded instruction stream with the resolved
/// type-indices from the matching `call_indirect_sigs_requested`
/// vector. The two lists are aligned by walk order — the i-th
/// sentinel resolves against `resolved[i]`.
fn patch_call_indirect_type_indices(
    insns: &mut [wasm_encoder::Instruction<'static>],
    resolved: &[u32],
) -> Result<(), LowerError> {
    use wasm_encoder::Instruction as I;
    let mut i = 0;
    for insn in insns.iter_mut() {
        if let I::CallIndirect {
            type_index,
            table_index: _,
        } = insn
        {
            if *type_index == u32::MAX {
                let resolved_idx = *resolved.get(i).ok_or(LowerError::UnsupportedOp(
                    "call_indirect_resolve_mismatch",
                    UnsupportedOpReason::ClosureValue,
                ))?;
                *type_index = resolved_idx;
                i += 1;
            }
        }
    }
    if i != resolved.len() {
        return Err(LowerError::UnsupportedOp(
            "call_indirect_resolve_count_mismatch",
            UnsupportedOpReason::ClosureValue,
        ));
    }
    Ok(())
}

/// Classify a `Ret`-schema layout into a [`ReturnShape`].
///
/// The canonical scalar-Int wrapper has exactly one field whose name
/// is the [`RETURN_VALUE_FIELD_NAME`] sentinel and whose type is `Int`;
/// anything else routes to the Dict-record-ptr path. Schemas the
/// walker can't lower (e.g. a single-field `value: Float`) surface as
/// `UnsupportedOp` with the matching follow-up tag.
fn classify_return_shape(schema: &Schema, layout: &OffsetTable) -> Result<ReturnShape, LowerError> {
    let is_canonical_value_wrapper = schema.fields.len() == 1
        && schema.fields[0].name == RETURN_VALUE_FIELD_NAME
        && matches!(schema.fields[0].ty, TypeRepr::Int);
    if is_canonical_value_wrapper {
        let ret_field = &layout.fields[0];
        return Ok(ReturnShape::ScalarValue {
            offset: ret_field.offset as u32,
            ty: IrType::I64,
        });
    }
    // Z.4.2 — `List<Int>` return. The canonical `Ret { value: List<Int> }`
    // wrapper matches the same single-field-named-`value` shape as the
    // scalar-Int wrapper; the discriminator is the element type. The
    // body emits `ConstListInt` (materialised in the data section)
    // followed by `StoreField { offset, ty: ListInt }` to leave the
    // i32 pointer on the wasm stack, then `Return` zero-extends to
    // the typed-func's i64 result.
    if schema.fields.len() == 1 && schema.fields[0].name == RETURN_VALUE_FIELD_NAME {
        if let TypeRepr::List { element } = &schema.fields[0].ty {
            if matches!(**element, TypeRepr::Int) {
                let ret_field = &layout.fields[0];
                return Ok(ReturnShape::ListIntPtr {
                    offset: ret_field.offset as u32,
                });
            }
            // Other list element types (Float / Bool / String / Schema)
            // each need their own data-section / runtime alloc + host
            // decode arm. Group them under the same Z.4.2 follow-up tag
            // so the host's tracing layer pins them to this batch.
            return Err(LowerError::UnsupportedOp(
                "ret_list_non_int",
                UnsupportedOpReason::ListLiteral,
            ));
        }
    }
    // Reject Dict-return shapes the walker can't lower yet:
    // - Empty record (no fields to store).
    // - Any field whose type isn't currently storeable by the walker
    //   (Int-only for Z.4.1; Bool / String / List / Schema would each
    //   need a separate `StoreFieldAtRecord` arm extension).
    if schema.fields.is_empty() {
        return Err(LowerError::UnsupportedOp(
            "ret_schema_empty",
            UnsupportedOpReason::DictReturn,
        ));
    }
    for f in &schema.fields {
        match &f.ty {
            TypeRepr::Int => {}
            TypeRepr::Bool => {
                // Z.4.1 widens StoreFieldAtRecord to Bool too, but the
                // host decode still needs the per-type decode arm.
                // Routed under DictReturn so the host's tracing layer
                // groups it with the Z.4.1 follow-up batch.
                return Err(LowerError::UnsupportedOp(
                    "ret_field_bool",
                    UnsupportedOpReason::DictReturn,
                ));
            }
            TypeRepr::Float => {
                return Err(LowerError::UnsupportedOp(
                    "ret_field_float",
                    UnsupportedOpReason::FloatArithmetic,
                ));
            }
            TypeRepr::String => {
                return Err(LowerError::UnsupportedOp(
                    "ret_field_string",
                    UnsupportedOpReason::StringOrStdlib,
                ));
            }
            TypeRepr::List { .. } => {
                return Err(LowerError::UnsupportedOp(
                    "ret_field_list",
                    UnsupportedOpReason::ListLiteral,
                ));
            }
            TypeRepr::Schema { .. } => {
                // Nested sub-record returns need AllocSubRecord +
                // EmitTailRecordFromAbsoluteAddr — Z.4.1+ follow-up.
                return Err(LowerError::UnsupportedOp(
                    "ret_field_nested_schema",
                    UnsupportedOpReason::DictReturn,
                ));
            }
            _ => {
                return Err(LowerError::UnsupportedOp(
                    "ret_field_unsupported",
                    UnsupportedOpReason::DictReturn,
                ));
            }
        }
    }
    Ok(ReturnShape::DictRecordPtr {
        root_size: layout.root_size as u32,
        root_align: layout.root_align as u32,
    })
}

/// Translate a canonical `TypeRepr` into the IR's `IrType` for the
/// scalar subset the walker handles. Anything beyond `Int` / `Bool`
/// surfaces as `UnsupportedOp` so the host re-routes.
fn type_repr_to_ir(ty: &TypeRepr) -> Result<IrType, LowerError> {
    match ty {
        TypeRepr::Int => Ok(IrType::I64),
        TypeRepr::Bool => Ok(IrType::Bool),
        // Float / String / List / Dict / Schema / Closure all need
        // either the buffer-protocol path or a Z.4 follow-up sub-phase.
        TypeRepr::Float => Err(LowerError::UnsupportedOp(
            "main_param_float",
            UnsupportedOpReason::FloatArithmetic,
        )),
        TypeRepr::String => Err(LowerError::UnsupportedOp(
            "main_param_string",
            UnsupportedOpReason::StringOrStdlib,
        )),
        TypeRepr::List { .. } => Err(LowerError::UnsupportedOp(
            "main_param_list",
            UnsupportedOpReason::ListLiteral,
        )),
        TypeRepr::Schema { .. } => Err(LowerError::UnsupportedOp(
            "main_param_schema",
            UnsupportedOpReason::DictReturn,
        )),
        _ => Err(LowerError::UnsupportedOp(
            "main_param_unsupported",
            UnsupportedOpReason::Other("main_param_unsupported"),
        )),
    }
}

/// Classify a non-Int IR type into the matching scope-cut reason so
/// the host can group by follow-up sub-phase.
fn non_int_main_param_reason(ty: &IrType) -> UnsupportedOpReason {
    match ty {
        IrType::F64 => UnsupportedOpReason::FloatArithmetic,
        IrType::String => UnsupportedOpReason::StringOrStdlib,
        IrType::ListInt | IrType::ListFloat | IrType::ListBool | IrType::ListString => {
            UnsupportedOpReason::ListLiteral
        }
        _ => UnsupportedOpReason::Other("non_int_main_param"),
    }
}

/// Stringify a `LayoutError` into a static tag for the Other(...)
/// scope-cut reason — `Layout::offsets_for` errors out only on
/// unsupported types, which is a Z.4 widening signal.
fn map_layout_err(_e: &relon_eval_api::layout::LayoutError) -> &'static str {
    "schema_layout_unsupported_type"
}

/// Z.4.2 — base absolute offset of the walker's `Op::ConstListInt`
/// data-segment region. Set to 1024 (4 KiB) to leave the wasm-page-0
/// low region available for future extensions (the LLVM AOT layout
/// pass uses a similar reserve); the cursor advances upward from
/// here as each new `ConstListInt` is interned.
const CONST_LIST_DATA_BASE: u32 = 1024;

/// Z.4.2 — translate an `Op::Block` / `Op::Loop` `result_ty` into a
/// `wasm_encoder::BlockType`. `None` becomes the stack-neutral
/// `BlockType::Empty`; otherwise pick the matching wasm valtype.
/// Unsupported IR types surface as `UnsupportedOp` so the body's
/// scope-cut tracks the matching follow-up phase.
fn block_type_from_ir(ty: Option<IrType>) -> Result<wasm_encoder::BlockType, LowerError> {
    use wasm_encoder::BlockType;
    match ty {
        None => Ok(BlockType::Empty),
        Some(IrType::I64) => Ok(BlockType::Result(ValType::I64)),
        Some(IrType::Bool) | Some(IrType::I32) => Ok(BlockType::Result(ValType::I32)),
        Some(other) => Err(LowerError::UnsupportedOp(
            "block_result_unsupported",
            non_int_main_param_reason(&other),
        )),
    }
}

/// Per-let-local wasm-local slot. The walker allocates one wasm local
/// per unique `(idx, ty)` pair seen in the Op stream.
#[derive(Debug, Clone, Copy)]
struct LetLocal {
    /// IR-side per-function let-local index. Same value across
    /// `LetGet` and `LetSet` for one logical binding.
    ir_idx: u32,
    /// Wasm local index. Allocated in alloc-order past the params.
    wasm_idx: u32,
    /// IR type of the bound value (drives wasm `ValType`).
    ty: IrType,
}

/// Z.4.3 — one pooled set of `CallClosure` arg-spill scratch
/// locals. Two ops with the same `param_tys` share one pool so the
/// emitted local section stays compact on recursive lambdas (the
/// W7 `fib` body has two back-to-back `CallClosure { param_tys:
/// [I64] }` ops; they share one i64 scratch slot).
#[derive(Debug, Clone)]
struct CallArgPool {
    /// IR-arg types in push order. Pools are keyed off this.
    tys: Vec<IrType>,
    /// Wasm-local indices, one per arg position. `slots[i]` is the
    /// scratch slot for the i-th IR arg.
    slots: Vec<u32>,
}

/// Per-record-local wasm slot for the Z.4.1 Dict-return path. Each
/// `AllocRootRecord { record_local_idx }` allocates one i32 wasm
/// local that holds the record's arena pointer; the matching
/// `StoreFieldAtRecord` ops read it as the GEP base.
#[derive(Debug, Clone, Copy)]
struct RecordLocal {
    /// IR-side record-local index (same value across the matching
    /// `AllocRootRecord` / `StoreFieldAtRecord` / `PushRecordBase`
    /// ops for one logical record).
    ir_idx: u32,
    /// Wasm local index. Allocated in alloc-order past the params.
    wasm_idx: u32,
}

/// One auxiliary wasm local allocation in alloc-order. The walker
/// allocates locals lazily as the IR walk visits the matching op —
/// let-locals on first `LetGet`/`LetSet`, record-base i32 locals on
/// `AllocRootRecord`, scratch i64 on first `StoreFieldAtRecord`.
/// `finalise` declares them in alloc-order so each entry's
/// `wasm_idx` matches the encoded position.
#[derive(Debug, Clone, Copy)]
enum AuxLocal {
    /// User let-binding. `ty` drives the wasm `ValType`.
    Let(IrType),
    /// Z.4.1 — record-base i32 (arena pointer).
    RecordBase,
    /// Z.4.1 — scratch i64 used to spill `StoreFieldAtRecord` rhs.
    ScratchI64,
    /// Z.4.3 — scratch i32 used by `MakeClosure` / `CallClosure` to
    /// stash arena pointers / closure handles across the multi-step
    /// linear-memory write sequence.
    ScratchI32,
    /// Z.4.3 — scratch local used by `CallClosure` to spill a user
    /// arg of `ty` while the operand stack is being re-ordered
    /// for the indirect-call ABI `(captures_ptr, args..., fn_idx)`.
    CallArgScratch(IrType),
}

/// Z.4.2 — per-`Op::ConstListInt` data-segment entry. Each unique
/// `ConstListInt { idx, elements }` materialises into one active
/// data segment whose absolute address `lower_ir_module` emits at
/// the matching `Op::ConstListInt` site (as `i32.const <abs_offset>`).
///
/// Record layout (mirrors the LLVM AOT side, see
/// `relon_ir::ir::Op::ConstListInt` docstring): `[len: u32 LE][pad:
/// u32 zero][i64 elements...]`, total `8 + 8 * elements.len()` bytes.
/// Aligned to 8 so the i64 payload sits on an 8-byte boundary.
#[derive(Debug, Clone)]
struct ConstListIntEntry {
    /// IR-side per-module identifier (`Op::ConstListInt { idx }`).
    /// The walker interns by `idx` so two ops referencing the same
    /// const-list share one data segment.
    ir_idx: u32,
    /// Absolute wasm linear-memory offset the active data segment
    /// installs the record at. Resolved at body-walk time by bumping
    /// a per-module cursor by `8 + 8 * len`, padded to 8 alignment.
    abs_offset: u32,
    /// The i64 elements — copied so the IR can be dropped without
    /// invalidating the data-segment payload.
    elements: Vec<i64>,
}

/// Walker state — accumulates wasm locals declarations and the
/// function-body instruction stream as we walk the IR Op vector.
struct EmitState<'a> {
    /// Which IR function flavour is being emitted (entry vs lambda).
    /// Drives `LoadField` / `LocalGet` / `Return` interpretation.
    kind: FunctionKind<'a>,
    /// User-let-local declarations seen so far. Each unique `(idx,
    /// ty)` is allocated one wasm local; reseeing the same idx
    /// reuses the wasm slot.
    let_locals: Vec<LetLocal>,
    /// Z.4.1 — record-base i32 locals, one per unique
    /// `AllocRootRecord { record_local_idx }` seen.
    record_locals: Vec<RecordLocal>,
    /// Z.4.1 — scratch i64 local used to spill the
    /// `StoreFieldAtRecord` rhs while the walker re-pushes the
    /// `(addr, value)` operand stack pair wasm needs. Lazily
    /// allocated on first use so the Z.4.0 scalar-Int path stays
    /// byte-identical.
    store_field_scratch_i64: Option<u32>,
    /// Z.4.3 — scratch i32 local used by `MakeClosure` to remember
    /// the freshly-allocated handle pointer across the captures
    /// initialisation sequence (so a self-recursive capture writes
    /// `handle_ptr` itself before the matching `LetSet` runs).
    /// Lazily allocated.
    make_closure_handle_scratch: Option<u32>,
    /// Z.4.3 — scratch i32 local mirroring
    /// `make_closure_handle_scratch` for the captures-struct base.
    /// Used when `captures_size > 0` so we can write multiple
    /// capture fields against the same pointer without re-emitting
    /// the alloc.
    make_closure_captures_scratch: Option<u32>,
    /// Z.4.3 — pools of scratch locals used by `CallClosure` ops to
    /// spill user-visible args off the operand stack before
    /// re-pushing them with the `(captures_ptr, args...)` ABI order.
    /// Pools are keyed by the IR-arg type sequence so two calls with
    /// the same signature reuse the same slots — keeping the local
    /// section compact when a recursive lambda (W7 `fib(k-1) +
    /// fib(k-2)`) has multiple in-body `CallClosure` ops.
    call_closure_arg_pools: Vec<CallArgPool>,
    /// Z.4.3 — scratch i32 local used by `CallClosure` to spill the
    /// closure handle (after popping all user args off the stack)
    /// so the indirect-call discipline can re-push `captures_ptr`
    /// then the user args then `fn_table_idx`.
    call_closure_handle_scratch: Option<u32>,
    /// Auxiliary local allocations in declaration order. `finalise`
    /// reads this vector to emit the wasm `local` section run-length
    /// encoded; the i-th entry's wasm-local index is
    /// `n_params + i`. Indices stored on [`LetLocal`] /
    /// [`RecordLocal`] / `store_field_scratch_i64` mirror this layout.
    aux_locals: Vec<AuxLocal>,
    /// Z.4.2 — interned `Op::ConstListInt` entries seen during the
    /// walk. `lower_ir_module` reads this vector to install one
    /// active data segment per entry; the matching `i32.const
    /// <abs_offset>` is already in the body's instruction stream.
    const_list_ints: Vec<ConstListIntEntry>,
    /// Z.4.2 — running cursor for the data section's next available
    /// absolute offset. Starts at `CONST_LIST_DATA_BASE` (1024 — past
    /// wasm-page 0's reserved low region) and advances by each list
    /// record's padded length. Reset per-module via `EmitState::new`.
    const_list_cursor: u32,
    /// Instructions captured by walking the body. Encoded into a
    /// `wasm_encoder::Function` in `finalise`.
    insns: Vec<wasm_encoder::Instruction<'static>>,
    /// `true` after the body emitted its sentinel return-prep op:
    /// either a scalar `StoreField(ret.value)` (scalar-Int shape) or
    /// at least one `AllocRootRecord` (Dict shape). The walker's
    /// contract: a body that doesn't honour the shape's prep
    /// sequence fails closed at `finalise`. Lambda bodies opt out —
    /// their `Return` op directly yields the top-of-stack and the
    /// contract is satisfied by the trailing `End`.
    saw_return_prep: bool,
    /// Z.4.1 — `true` once the walker has emitted a host-import call
    /// (today: `__relon_arena_alloc`). Flips the
    /// import-section / fn-index discipline in `lower_ir_module` so
    /// modules without host calls keep their byte-identical Z.4.0
    /// shape.
    needs_arena_alloc: bool,
    /// Z.4.3 — the call_indirect type-index per `CallClosure` op
    /// emitted. Set on the global side-table during walk so the
    /// caller can populate the wasm `TypeSection` with the matching
    /// signatures. The walker doesn't know its module's type-index
    /// layout, so it records each requested signature here and
    /// reconciles after both function walks complete.
    call_indirect_sigs_requested: Vec<(Vec<IrType>, IrType)>,
}

impl<'a> EmitState<'a> {
    fn new(kind: FunctionKind<'a>) -> Self {
        Self {
            kind,
            let_locals: Vec::new(),
            record_locals: Vec::new(),
            store_field_scratch_i64: None,
            make_closure_handle_scratch: None,
            make_closure_captures_scratch: None,
            call_closure_arg_pools: Vec::new(),
            call_closure_handle_scratch: None,
            aux_locals: Vec::new(),
            const_list_ints: Vec::new(),
            const_list_cursor: CONST_LIST_DATA_BASE,
            insns: Vec::new(),
            saw_return_prep: false,
            needs_arena_alloc: false,
            call_indirect_sigs_requested: Vec::new(),
        }
    }

    /// How many wasm params this function declares. Drives the
    /// `wasm_idx` allocation for auxiliary locals (which sit after
    /// the params in the local namespace).
    fn n_params(&self) -> u32 {
        match &self.kind {
            FunctionKind::Entry { param_slots, .. } => param_slots.len() as u32,
            FunctionKind::Lambda { params, .. } => params.len() as u32,
        }
    }

    /// Reserve the next wasm-local index past the params; appends an
    /// [`AuxLocal`] tag so [`finalise`] re-emits the matching
    /// `ValType` declaration. Returns the new index.
    fn alloc_aux(&mut self, tag: AuxLocal) -> u32 {
        let idx = self.n_params() + self.aux_locals.len() as u32;
        self.aux_locals.push(tag);
        idx
    }

    /// Walk a body's Op vector, emitting wasm instructions in order.
    fn walk(&mut self, body: &[TaggedOp]) -> Result<(), LowerError> {
        for tagged in body {
            self.emit_op(&tagged.op)?;
        }
        Ok(())
    }

    /// Emit one Op's wasm instructions. Each Op variant is documented
    /// inline at its branch — the comments mirror the source IR doc
    /// so future widening can grep by op name.
    fn emit_op(&mut self, op: &Op) -> Result<(), LowerError> {
        use wasm_encoder::Instruction as I;
        match op {
            // ----- Scalar literals -----------------------------------
            Op::ConstBool(b) => {
                self.insns.push(I::I32Const(if *b { 1 } else { 0 }));
            }
            Op::ConstI32(v) => {
                self.insns.push(I::I32Const(*v));
            }
            Op::ConstI64(v) => {
                self.insns.push(I::I64Const(*v));
            }

            // ----- Let-local push/pop --------------------------------
            Op::LetGet { idx, ty } => {
                let local = self.intern_let_local(*idx, *ty)?;
                self.insns.push(I::LocalGet(local));
            }
            Op::LetSet { idx, ty } => {
                let local = self.intern_let_local(*idx, *ty)?;
                self.insns.push(I::LocalSet(local));
            }

            // ----- Param read (LoadField on MainParams.x slot) ------
            Op::LoadField { offset, ty } => {
                if *ty != IrType::I64 {
                    return Err(LowerError::UnsupportedOp(
                        "load_field_non_int",
                        non_int_main_param_reason(ty),
                    ));
                }
                let param_slots = match &self.kind {
                    FunctionKind::Entry { param_slots, .. } => *param_slots,
                    FunctionKind::Lambda { .. } => {
                        // Lambda bodies read their args via `LocalGet`
                        // (captures_ptr at idx 0, user args at 1..);
                        // a `LoadField` reaching one signals an IR
                        // shape the walker can't handle in a closure
                        // body today.
                        return Err(LowerError::UnsupportedOp(
                            "load_field_in_lambda_body",
                            UnsupportedOpReason::BufferProtocolRecord,
                        ));
                    }
                };
                let slot = param_slots.iter().find(|p| p.offset == *offset).ok_or(
                    LowerError::UnsupportedOp(
                        "load_field_offset_not_main_param",
                        UnsupportedOpReason::BufferProtocolRecord,
                    ),
                )?;
                self.insns.push(I::LocalGet(slot.wasm_param_idx));
                let _ = slot.ty; // future widening reads this
            }

            // ----- Wasm-local read (lambda captures_ptr + args) -----
            //
            // Z.4.3 — `Op::LocalGet(N)` is the canonical way a lambda
            // body reads its `(captures_ptr, ...user_args)`-shaped
            // signature: `LocalGet(0)` is the captures pointer,
            // `LocalGet(i + 1)` is the i-th user-visible argument.
            // Entry bodies never emit `LocalGet` directly — their
            // params come in via the `LoadField` against the
            // `MainParams` schema — so a `LocalGet` reaching the
            // entry walker is treated as an out-of-envelope op.
            Op::LocalGet(idx) => match &self.kind {
                FunctionKind::Lambda { params, .. } => {
                    if (*idx as usize) >= params.len() {
                        return Err(LowerError::UnsupportedOp(
                            "local_get_out_of_range",
                            UnsupportedOpReason::Other("local_get_out_of_range"),
                        ));
                    }
                    self.insns.push(I::LocalGet(*idx));
                }
                FunctionKind::Entry { .. } => {
                    return Err(LowerError::UnsupportedOp(
                        "local_get_in_entry_body",
                        UnsupportedOpReason::BufferProtocolRecord,
                    ));
                }
            },

            // ----- Z.4.3 — raw-memory absolute loads ----------------
            //
            // The lambda body's capture-prologue uses these ops to
            // peel each captured value off the captures struct that
            // the `MakeClosure` on the outer side wrote. Stack
            // discipline mirrors the wasm load family: pop an i32
            // address, push the value at `addr + offset`.
            Op::LoadI32AtAbsolute { offset } => {
                self.insns.push(I::I32Load(MemArg {
                    offset: u64::from(*offset),
                    align: 2,
                    memory_index: 0,
                }));
            }
            Op::LoadI64AtAbsolute { offset } => {
                self.insns.push(I::I64Load(MemArg {
                    offset: u64::from(*offset),
                    align: 3,
                    memory_index: 0,
                }));
            }
            Op::LoadF64AtAbsolute { offset } => {
                self.insns.push(I::F64Load(MemArg {
                    offset: u64::from(*offset),
                    align: 3,
                    memory_index: 0,
                }));
            }
            Op::LoadI8UAtAbsolute { offset } => {
                self.insns.push(I::I32Load8U(MemArg {
                    offset: u64::from(*offset),
                    align: 0,
                    memory_index: 0,
                }));
            }

            // ----- Return store: leave value on stack as fn result --
            Op::StoreField { offset, ty } => {
                // `StoreField` is the scalar-Int / List<Int>-return
                // path's sentinel: the IR's trailing
                // `Op::StoreField { offset: Ret.value }` corresponds
                // to "this value is the function's return value".
                // The Dict-return shape never emits `StoreField` —
                // it emits `StoreFieldAtRecord` against the
                // record-base local instead. A `StoreField` reaching
                // the Dict-return walker indicates a buffer-protocol
                // record write the typed-func ABI can't model.
                let return_shape = match &self.kind {
                    FunctionKind::Entry { return_shape, .. } => *return_shape,
                    FunctionKind::Lambda { .. } => {
                        // Lambda bodies never emit StoreField — their
                        // Return is the canonical return prep.
                        return Err(LowerError::UnsupportedOp(
                            "store_field_in_lambda_body",
                            UnsupportedOpReason::BufferProtocolRecord,
                        ));
                    }
                };
                match return_shape {
                    ReturnShape::ScalarValue {
                        offset: ret_offset, ..
                    } => {
                        if *offset != *ret_offset {
                            return Err(LowerError::UnsupportedOp(
                                "store_field_non_return",
                                UnsupportedOpReason::BufferProtocolRecord,
                            ));
                        }
                        if *ty != IrType::I64 {
                            return Err(LowerError::UnsupportedOp(
                                "store_field_non_int",
                                non_int_main_param_reason(ty),
                            ));
                        }
                        // Nothing to emit — the value is already on
                        // the stack; the typed-func's `End` will
                        // return the top. We just record the
                        // contract was honoured.
                        self.saw_return_prep = true;
                    }
                    ReturnShape::DictRecordPtr { .. } => {
                        return Err(LowerError::UnsupportedOp(
                            "store_field_in_dict_return",
                            UnsupportedOpReason::BufferProtocolRecord,
                        ));
                    }
                    ReturnShape::ListIntPtr { offset: ret_offset } => {
                        // Z.4.2 — `List<Int>` return. The body's
                        // trailing `StoreField { offset: ret.value,
                        // ty: ListInt }` leaves the i32 list-record
                        // pointer on the operand stack; the matching
                        // `Op::Return` zero-extends it to i64 below.
                        if *offset != *ret_offset {
                            return Err(LowerError::UnsupportedOp(
                                "store_field_non_return",
                                UnsupportedOpReason::BufferProtocolRecord,
                            ));
                        }
                        if *ty != IrType::ListInt {
                            return Err(LowerError::UnsupportedOp(
                                "store_field_list_ret_non_int_elem",
                                UnsupportedOpReason::ListLiteral,
                            ));
                        }
                        self.saw_return_prep = true;
                    }
                }
            }

            // ----- Arithmetic ----------------------------------------
            Op::Add(IrType::I64) => self.insns.push(I::I64Add),
            Op::Sub(IrType::I64) => self.insns.push(I::I64Sub),
            Op::Mul(IrType::I64) => self.insns.push(I::I64Mul),
            Op::Div(IrType::I64) => self.insns.push(I::I64DivS),
            Op::Mod(IrType::I64) => self.insns.push(I::I64RemS),

            // #359: signed-int → float promotion (`sitofp`). The wasm
            // backend still rejects the surrounding F64 arithmetic as a
            // scope-cut (see the Float-arith arm below), so this is not
            // reached by a real Relon program today; the op is wired for
            // parity with the AOT/cranelift backends and surfaces the
            // native `f64.convert_i64_s` when the Float scope-cut lifts.
            Op::ConvertI64ToF64 => self.insns.push(I::F64ConvertI64S),

            // ----- Comparisons (i64 -> i32 bool) --------------------
            Op::Eq(IrType::I64) => self.insns.push(I::I64Eq),
            Op::Ne(IrType::I64) => self.insns.push(I::I64Ne),
            Op::Lt(IrType::I64) => self.insns.push(I::I64LtS),
            Op::Le(IrType::I64) => self.insns.push(I::I64LeS),
            Op::Gt(IrType::I64) => self.insns.push(I::I64GtS),
            Op::Ge(IrType::I64) => self.insns.push(I::I64GeS),

            // Bool comparisons (Eq/Ne against i32).
            Op::Eq(IrType::Bool) => self.insns.push(I::I32Eq),
            Op::Ne(IrType::Bool) => self.insns.push(I::I32Ne),

            // ----- Branch ---------------------------------------------
            Op::If {
                result_ty,
                then_body,
                else_body,
            } => {
                let block_ty = match result_ty {
                    IrType::I64 => wasm_encoder::BlockType::Result(ValType::I64),
                    IrType::Bool | IrType::I32 => wasm_encoder::BlockType::Result(ValType::I32),
                    other => {
                        return Err(LowerError::UnsupportedOp(
                            "if_result_unsupported",
                            non_int_main_param_reason(other),
                        ))
                    }
                };
                self.insns.push(I::If(block_ty));
                self.walk(then_body)?;
                self.insns.push(I::Else);
                self.walk(else_body)?;
                self.insns.push(I::End);
            }

            // ----- Z.4.2 — labelled block / loop / br / br_if --------
            //
            // The `range(n).reduce(...)` lowering (and other range-
            // chain consumers in `relon-ir::lowering`) emits a
            // `Block { Loop { ... } }` skeleton with `Br`/`BrIf` ops
            // driving the iteration. These are the wasm-native
            // primitives the LLVM AOT backend's `emit_block` /
            // `emit_loop` / `emit_br` paths target; the walker
            // mirrors them one-for-one so the same source lands on
            // the Compiled tier here too.
            //
            // Honesty (design §7): the IR's nested block depth maps
            // directly to wasm's structured-control-flow depth. No
            // flattening, no unrolling, no closed-form rewrites —
            // every iteration the IR describes runs as one wasm
            // loop pass.
            Op::Block { result_ty, body } => {
                let block_ty = block_type_from_ir(*result_ty)?;
                self.insns.push(I::Block(block_ty));
                self.walk(body)?;
                self.insns.push(I::End);
            }
            Op::Loop { result_ty, body } => {
                let block_ty = block_type_from_ir(*result_ty)?;
                self.insns.push(I::Loop(block_ty));
                self.walk(body)?;
                self.insns.push(I::End);
            }
            Op::Br { label_depth } => {
                self.insns.push(I::Br(*label_depth));
            }
            Op::BrIf { label_depth } => {
                self.insns.push(I::BrIf(*label_depth));
            }

            // ----- Ternary select ------------------------------------
            Op::Select { ty } => match ty {
                IrType::I64 => self.insns.push(I::TypedSelect(ValType::I64)),
                IrType::Bool | IrType::I32 => self.insns.push(I::TypedSelect(ValType::I32)),
                other => {
                    return Err(LowerError::UnsupportedOp(
                        "select_unsupported_ty",
                        non_int_main_param_reason(other),
                    ))
                }
            },

            // ----- Return --------------------------------------------
            Op::Return => match &self.kind {
                FunctionKind::Entry { return_shape, .. } => match return_shape {
                    ReturnShape::ScalarValue { .. } => {
                        // No-op for the scalar-Int typed-func ABI — the
                        // function's trailing `End` already returns the
                        // top of stack. We still record the explicit op
                        // so a malformed body (missing both `StoreField`
                        // and `Return`) fails closed via `finalise`.
                    }
                    ReturnShape::DictRecordPtr { .. } => {
                        // Z.4.1 — Dict shape. The body's
                        // `AllocRootRecord` stashed the arena pointer in
                        // a wasm i32 local; the trailing `Return` reads
                        // it back, zero-extends to i64, and the typed-
                        // func returns it. The host's `WasmEvaluator::
                        // run_main` recognises the Dict-shape return by
                        // schema name + field shape and decodes the
                        // record out of linear memory.
                        let root_local = self
                            .record_locals
                            .first()
                            .ok_or(LowerError::UnsupportedOp(
                                "dict_return_no_root_record",
                                UnsupportedOpReason::DictReturn,
                            ))?
                            .wasm_idx;
                        self.insns.push(I::LocalGet(root_local));
                        self.insns.push(I::I64ExtendI32U);
                    }
                    ReturnShape::ListIntPtr { .. } => {
                        // Z.4.2 — `List<Int>` return. The body's trailing
                        // `StoreField { offset, ty: ListInt }` already
                        // left the i32 list-record pointer on the operand
                        // stack; widen to i64 so the typed-func's i64
                        // result carries the zero-extended pointer.
                        self.insns.push(I::I64ExtendI32U);
                    }
                },
                FunctionKind::Lambda { .. } => {
                    // Lambda bodies return the top-of-stack directly;
                    // the trailing `End` yields it as the function
                    // result. `saw_return_prep` is satisfied by the
                    // explicit `Return` op.
                    self.saw_return_prep = true;
                }
            },

            // ----- Z.4.1 — Dict root record allocation ---------------
            Op::AllocRootRecord { record_local_idx } => {
                let return_shape = match &self.kind {
                    FunctionKind::Entry { return_shape, .. } => *return_shape,
                    FunctionKind::Lambda { .. } => {
                        return Err(LowerError::UnsupportedOp(
                            "alloc_root_record_in_lambda_body",
                            UnsupportedOpReason::DictReturn,
                        ));
                    }
                };
                let (root_size, root_align) = match return_shape {
                    ReturnShape::DictRecordPtr {
                        root_size,
                        root_align,
                        ..
                    } => (*root_size, *root_align),
                    ReturnShape::ScalarValue { .. } | ReturnShape::ListIntPtr { .. } => {
                        // `AllocRootRecord` reaching a non-Dict-return
                        // body means the IR pipeline emitted Dict-
                        // construction ops against a non-Dict return
                        // shape — that's a contract violation.
                        return Err(LowerError::UnsupportedOp(
                            "alloc_root_record_in_non_dict_return",
                            UnsupportedOpReason::DictReturn,
                        ));
                    }
                };
                // Allocate the i32 record-base wasm local on first
                // sight of this `record_local_idx`. Subsequent
                // `StoreFieldAtRecord` / `Return` ops re-resolve via
                // `intern_record_local`.
                let wasm_idx = self.intern_record_local(*record_local_idx)?;
                // emit:
                //   i32.const <root_size>
                //   i32.const <root_align>
                //   call $__relon_arena_alloc
                //   local.set $record_<idx>
                self.insns.push(I::I32Const(root_size as i32));
                self.insns.push(I::I32Const(root_align.max(1) as i32));
                let arena_alloc_fn_idx = import_index(1); // §4.1 / table id 1
                self.insns.push(I::Call(arena_alloc_fn_idx));
                self.insns.push(I::LocalSet(wasm_idx));
                self.needs_arena_alloc = true;
                self.saw_return_prep = true;
                // `__relon_arena_alloc` traps via `Err` on OOM (see
                // `relon-wasm-evaluator::host_imports`); the wasmtime
                // trap surfaces as `RuntimeError::IoError` on the
                // host side, mirroring how the LLVM AOT side reports
                // arena exhaustion.
            }

            // ----- Z.4.1 — store into a record field -----------------
            Op::StoreFieldAtRecord {
                record_local_idx,
                offset,
                ty,
            } => {
                // The walker only handles Int (I64) fields for Z.4.1;
                // Bool / String / nested-schema field writes land
                // under the Z.4.1+ widening tasks (matching the
                // `classify_return_shape` per-field guard).
                if *ty != IrType::I64 {
                    return Err(LowerError::UnsupportedOp(
                        "store_field_at_record_non_int",
                        non_int_main_param_reason(ty),
                    ));
                }
                let record_wasm_idx = self
                    .record_locals
                    .iter()
                    .find(|r| r.ir_idx == *record_local_idx)
                    .ok_or(LowerError::UnsupportedOp(
                        "store_field_at_record_no_alloc",
                        UnsupportedOpReason::DictReturn,
                    ))?
                    .wasm_idx;
                // Wasm `i64.store` expects `[addr, value]` on the
                // stack — but the IR producer left the value on top
                // already. Spill it to a scratch local, push the
                // record base, push the spilled value, then emit
                // the typed store with the field-offset immediate.
                let scratch = self.scratch_i64();
                self.insns.push(I::LocalSet(scratch));
                self.insns.push(I::LocalGet(record_wasm_idx));
                self.insns.push(I::LocalGet(scratch));
                self.insns.push(I::I64Store(MemArg {
                    offset: u64::from(*offset),
                    align: 3, // log2(8) for i64 alignment
                    memory_index: 0,
                }));
            }

            // ----- Scope-cut: Dict construction (Z.4.1 follow-up) ----
            // `AllocSubRecord` / `PushRecordBase` /
            // `EmitTailRecordFromAbsoluteAddr` cover nested-record /
            // pointer-indirect Dict fields the Z.4.1 root-only path
            // doesn't yet model. Surface the cut under `DictReturn`
            // so the host's tracing layer groups them with the
            // matching follow-up batch.
            Op::AllocSubRecord { .. }
            | Op::PushRecordBase { .. }
            | Op::EmitTailRecordFromAbsoluteAddr { .. } => {
                return Err(LowerError::UnsupportedOp(
                    "dict_construction_nested",
                    UnsupportedOpReason::DictReturn,
                ));
            }

            // ----- Z.4.2 — `List<Int>` literal materialization -------
            //
            // Intern the (idx, elements) pair into the walker's data-
            // segment table; emit `i32.const <abs_offset>` so the
            // body pushes the list-record's absolute wasm linear-
            // memory address. `lower_ir_module` installs the matching
            // active data segment (record layout `[len: u32 LE][pad:
            // u32 zero][i64 elements...]`) at the resolved offset.
            //
            // Honesty (design §7): the data-section blob is byte-
            // identical to what the LLVM AOT side produces for the
            // same literal (see `relon_ir::ir::Op::ConstListInt`
            // docstring) — no closed-form rewrites, no const-fold-
            // through-stdlib hacks. The pointer is the value the
            // body pushes; downstream consumption (Call into stdlib
            // `length`, `list.sum`, …) still scope-cuts pending the
            // string/stdlib follow-up batch.
            Op::ConstListInt { idx, elements } => {
                let abs_offset = self.intern_const_list_int(*idx, elements);
                self.insns.push(I::I32Const(abs_offset as i32));
            }

            // ----- Scope-cut: other list shapes / iter ---------------
            //
            // Float / Bool / String list literals each need their own
            // data-segment encoding (f64 / packed u8 / nested-record);
            // `LoadList*Ptr` is the buffer-protocol param-load path the
            // typed-func ABI sidesteps; `ListGetByIntIdx` is the
            // trace-recorder hot-path op that the standard IR lowering
            // doesn't emit yet. All routed to the Z.4.2 follow-up bucket.
            Op::ConstListFloat { .. }
            | Op::ConstListBool { .. }
            | Op::ConstListString { .. }
            | Op::LoadListIntPtr { .. }
            | Op::LoadListFloatPtr { .. }
            | Op::LoadListBoolPtr { .. }
            | Op::LoadListStringPtr { .. }
            | Op::LoadListSchemaPtr { .. }
            | Op::ListGetByIntIdx { .. } => {
                return Err(LowerError::UnsupportedOp(
                    "list_literal_or_index",
                    UnsupportedOpReason::ListLiteral,
                ));
            }

            // ----- Scope-cut: String / stdlib calls ------------------
            Op::ConstString { .. } | Op::StrConcatN { .. } | Op::ReadStringLen => {
                return Err(LowerError::UnsupportedOp(
                    "string_op",
                    UnsupportedOpReason::StringOrStdlib,
                ));
            }
            Op::Call { .. } => {
                // Stdlib / user-fn call. Z.4 follow-up — the IR Op
                // carries an `fn_index` that points at the bundled
                // stdlib offset table, which the IR walker would need
                // to resolve through the host imports table. Kept
                // scope-cut for now so the W2/W6 chains don't silently
                // route to a half-built call site.
                return Err(LowerError::UnsupportedOp(
                    "stdlib_call",
                    UnsupportedOpReason::StringOrStdlib,
                ));
            }

            // ----- Scope-cut: Dict / List lookups --------------------
            Op::DictGetByStringKey { .. } => {
                return Err(LowerError::UnsupportedOp(
                    "dict_lookup",
                    UnsupportedOpReason::DictReturn,
                ));
            }

            // ----- Z.4.3 — Closure-as-value construction -------------
            //
            // Build an 8-byte handle in arena memory laid out as
            // `[fn_table_idx: i32 LE][captures_ptr: i32 LE]`. The
            // handle pointer (i32) is left on the operand stack so a
            // following `LetSet { ty: Closure }` stashes it under the
            // source-level closure name. For self-recursive closures
            // (the W7 `fib: (k) => fib(k - 1) + fib(k - 2)` shape) the
            // matching capture's `let_idx` references a let-binding
            // the IR's `LetSet` has not yet run; we detect that case
            // and write `handle_ptr` itself as the capture, mirroring
            // the LLVM emitter's `not yet bound` branch (see
            // `emit_make_closure` in `relon-codegen-llvm`).
            Op::MakeClosure {
                fn_table_idx,
                captures,
                captures_size,
            } => {
                // Step 1 — alloc 8 bytes for the handle and stash
                // its pointer in the scratch i32.
                self.insns.push(I::I32Const(8));
                self.insns.push(I::I32Const(4));
                let arena_alloc_fn_idx = import_index(1); // §4.1
                self.insns.push(I::Call(arena_alloc_fn_idx));
                self.needs_arena_alloc = true;
                let handle_scratch = self.make_closure_handle_scratch();
                self.insns.push(I::LocalSet(handle_scratch));

                // Step 2 — alloc the captures struct (if any) and
                // stash its pointer in a second scratch i32. When
                // there are no captures the captures_ptr slot in
                // the handle stays zero.
                let captures_scratch = if *captures_size > 0 {
                    self.insns.push(I::I32Const(*captures_size as i32));
                    self.insns.push(I::I32Const(8)); // captures align — match LLVM's 8B rule
                    self.insns.push(I::Call(arena_alloc_fn_idx));
                    let s = self.make_closure_captures_scratch();
                    self.insns.push(I::LocalSet(s));
                    Some(s)
                } else {
                    None
                };

                // Step 3 — write fn_table_idx at handle+0.
                self.insns.push(I::LocalGet(handle_scratch));
                self.insns.push(I::I32Const(*fn_table_idx as i32));
                self.insns.push(I::I32Store(MemArg {
                    offset: 0,
                    align: 2,
                    memory_index: 0,
                }));

                // Step 4 — write captures_ptr at handle+4. Zero when
                // the closure has no captures (matches LLVM AOT).
                self.insns.push(I::LocalGet(handle_scratch));
                if let Some(s) = captures_scratch {
                    self.insns.push(I::LocalGet(s));
                } else {
                    self.insns.push(I::I32Const(0));
                }
                self.insns.push(I::I32Store(MemArg {
                    offset: 4,
                    align: 2,
                    memory_index: 0,
                }));

                // Step 5 — populate captures. Each capture either
                // references an existing let-local (the common case
                // for outer-scope captures) or refers to the
                // closure's own handle for self-recursion (the
                // matching let-local hasn't been bound yet — the
                // `LetSet` immediately follows `MakeClosure`).
                if let Some(s) = captures_scratch {
                    for cap in captures {
                        let existing = self.let_locals.iter().find(|l| l.ir_idx == cap.let_idx);
                        // Push the captures-struct base + per-field
                        // offset so the typed store has `[addr, value]`
                        // on the operand stack.
                        self.insns.push(I::LocalGet(s));
                        // Value source: either an existing let-local
                        // (read it back) or — for self-recursion —
                        // the handle pointer we just allocated.
                        let cap_ty = cap.ty;
                        match existing {
                            Some(local) => {
                                if local.ty != cap_ty {
                                    return Err(LowerError::UnsupportedOp(
                                        "make_closure_capture_type_mismatch",
                                        UnsupportedOpReason::Other(
                                            "make_closure_capture_type_mismatch",
                                        ),
                                    ));
                                }
                                self.insns.push(I::LocalGet(local.wasm_idx));
                            }
                            None => {
                                // Self-recursive capture: only legal
                                // when the capture type is Closure
                                // (anything else can't refer to a
                                // not-yet-bound let-local in source).
                                if cap_ty != IrType::Closure {
                                    return Err(LowerError::UnsupportedOp(
                                        "make_closure_capture_unbound_non_closure",
                                        UnsupportedOpReason::ClosureValue,
                                    ));
                                }
                                self.insns.push(I::LocalGet(handle_scratch));
                            }
                        }
                        // Emit the per-type store at the capture's
                        // declared offset inside the captures struct.
                        match cap_ty {
                            IrType::I64 => {
                                self.insns.push(I::I64Store(MemArg {
                                    offset: u64::from(cap.offset),
                                    align: 3,
                                    memory_index: 0,
                                }));
                            }
                            IrType::F64 => {
                                self.insns.push(I::F64Store(MemArg {
                                    offset: u64::from(cap.offset),
                                    align: 3,
                                    memory_index: 0,
                                }));
                            }
                            IrType::Bool => {
                                self.insns.push(I::I32Store8(MemArg {
                                    offset: u64::from(cap.offset),
                                    align: 0,
                                    memory_index: 0,
                                }));
                            }
                            IrType::I32
                            | IrType::Null
                            | IrType::String
                            | IrType::ListInt
                            | IrType::ListFloat
                            | IrType::ListBool
                            | IrType::ListString
                            | IrType::ListSchema
                            | IrType::Closure => {
                                self.insns.push(I::I32Store(MemArg {
                                    offset: u64::from(cap.offset),
                                    align: 2,
                                    memory_index: 0,
                                }));
                            }
                        }
                    }
                }

                // Step 6 — push the handle pointer as the
                // `Op::MakeClosure`'s result. The IR pipeline
                // immediately follows with a `LetSet { ty: Closure }`
                // that stashes it under the source-level name.
                self.insns.push(I::LocalGet(handle_scratch));
            }

            // ----- Z.4.3 — Closure invocation ------------------------
            //
            // Stack discipline: `[handle, arg0, ..., argN] -> [ret_ty]`.
            // wasm `call_indirect` needs the operands in a different
            // order — `[captures_ptr, arg0, ..., argN, fn_table_idx]`.
            // We spill all user args + the handle into per-arg
            // scratch locals, then re-push in the indirect-call
            // order.
            Op::CallClosure { param_tys, ret_ty } => {
                // Reserve / reuse the per-signature arg-spill scratch
                // pool. `slots[i]` is the scratch local for the i-th
                // IR arg (in push order).
                let slots = self.call_arg_scratches_for(param_tys);
                // Pop user args off in reverse (wasm pops right-to-
                // left) into the matching scratch slot.
                for i in (0..param_tys.len()).rev() {
                    self.insns.push(I::LocalSet(slots[i]));
                }
                // Pop the closure handle (i32).
                let handle_scratch = self.call_handle_scratch();
                self.insns.push(I::LocalSet(handle_scratch));

                // Re-push: captures_ptr (handle[+4]) first ...
                self.insns.push(I::LocalGet(handle_scratch));
                self.insns.push(I::I32Load(MemArg {
                    offset: 4,
                    align: 2,
                    memory_index: 0,
                }));
                // ... then user args in declaration order ...
                for slot in &slots {
                    self.insns.push(I::LocalGet(*slot));
                }
                // ... then fn_table_idx (handle[+0]) on top.
                self.insns.push(I::LocalGet(handle_scratch));
                self.insns.push(I::I32Load(MemArg {
                    offset: 0,
                    align: 2,
                    memory_index: 0,
                }));
                // Record the call-indirect signature; the module-level
                // assembly resolves it against the wasm `TypeSection`
                // and patches each placeholder `CallIndirect` in the
                // emitted instruction stream. We emit a placeholder
                // with `type_index = u32::MAX`; the post-walk
                // `patch_call_indirect_type_indices` pass replaces it
                // with the resolved index in IR order.
                self.call_indirect_sigs_requested
                    .push((param_tys.clone(), *ret_ty));
                self.insns.push(I::CallIndirect {
                    type_index: u32::MAX, // patched in finalise_module
                    table_index: 0,
                });
            }

            // ----- Scope-cut: Float arithmetic -----------------------
            Op::Add(IrType::F64)
            | Op::Sub(IrType::F64)
            | Op::Mul(IrType::F64)
            | Op::Div(IrType::F64)
            | Op::Eq(IrType::F64)
            | Op::Ne(IrType::F64)
            | Op::Lt(IrType::F64)
            | Op::Le(IrType::F64)
            | Op::Gt(IrType::F64)
            | Op::Ge(IrType::F64)
            | Op::ConstF64(_) => {
                return Err(LowerError::UnsupportedOp(
                    "float_arith",
                    UnsupportedOpReason::FloatArithmetic,
                ));
            }

            // ----- Fallthrough: anything else is a future widening --
            other => {
                let name = op_debug_tag(other);
                return Err(LowerError::UnsupportedOp(
                    name,
                    UnsupportedOpReason::Other(name),
                ));
            }
        }
        Ok(())
    }

    /// Lookup or allocate a wasm local slot for a let-binding.
    fn intern_let_local(&mut self, ir_idx: u32, ty: IrType) -> Result<u32, LowerError> {
        if let Some(existing) = self.let_locals.iter().find(|l| l.ir_idx == ir_idx) {
            if existing.ty != ty {
                return Err(LowerError::UnsupportedOp(
                    "let_local_type_mismatch",
                    UnsupportedOpReason::Other("let_local_type_mismatch"),
                ));
            }
            return Ok(existing.wasm_idx);
        }
        let wasm_idx = self.alloc_aux(AuxLocal::Let(ty));
        self.let_locals.push(LetLocal {
            ir_idx,
            wasm_idx,
            ty,
        });
        Ok(wasm_idx)
    }

    /// Z.4.1 — lookup or allocate a wasm i32 local for a record base
    /// pointer. The IR pipeline emits one `AllocRootRecord` per root
    /// record; the wasm local holds the arena pointer
    /// `__relon_arena_alloc` returned.
    fn intern_record_local(&mut self, ir_idx: u32) -> Result<u32, LowerError> {
        if let Some(existing) = self.record_locals.iter().find(|r| r.ir_idx == ir_idx) {
            return Ok(existing.wasm_idx);
        }
        let wasm_idx = self.alloc_aux(AuxLocal::RecordBase);
        self.record_locals.push(RecordLocal { ir_idx, wasm_idx });
        Ok(wasm_idx)
    }

    /// Z.4.1 — lazy scratch i64 local used by `StoreFieldAtRecord`
    /// to spill the rhs while the walker re-orders the operand stack
    /// for `i64.store`. One scratch slot per function suffices —
    /// `StoreFieldAtRecord` always consumes its value in the same op,
    /// so the slot's lifetime is one op long.
    fn scratch_i64(&mut self) -> u32 {
        if let Some(idx) = self.store_field_scratch_i64 {
            return idx;
        }
        let idx = self.alloc_aux(AuxLocal::ScratchI64);
        self.store_field_scratch_i64 = Some(idx);
        idx
    }

    /// Z.4.2 — intern an `Op::ConstListInt { idx, elements }` into the
    /// walker's data-segment table. Returns the resolved absolute
    /// offset (the value the walker emits as `i32.const <abs_offset>`
    /// at the matching op site).
    ///
    /// Layout: `[len: u32 LE][pad: u32 zero][i64 elements...]`. The
    /// record occupies `8 + 8 * elements.len()` bytes and the next
    /// cursor advances by that amount, rounded up to 8 so any
    /// subsequent list record's i64 payload stays 8-aligned (the
    /// `i64.load` the host decode uses doesn't care, but keeping the
    /// invariant matches what the LLVM AOT layout pass guarantees).
    fn intern_const_list_int(&mut self, ir_idx: u32, elements: &[i64]) -> u32 {
        if let Some(existing) = self.const_list_ints.iter().find(|e| e.ir_idx == ir_idx) {
            return existing.abs_offset;
        }
        let abs_offset = self.const_list_cursor;
        let record_len = 8u32 + 8u32 * (elements.len() as u32);
        // Round up to 8-byte alignment for the next record.
        let next_cursor = (abs_offset + record_len + 7) & !7u32;
        self.const_list_cursor = next_cursor;
        self.const_list_ints.push(ConstListIntEntry {
            ir_idx,
            abs_offset,
            elements: elements.to_vec(),
        });
        abs_offset
    }

    /// Z.4.3 — lazy scratch i32 used by `MakeClosure` to stash the
    /// freshly-allocated handle pointer across the captures-init
    /// sequence (so self-recursive captures can write the handle
    /// itself before the matching `LetSet` runs).
    fn make_closure_handle_scratch(&mut self) -> u32 {
        if let Some(idx) = self.make_closure_handle_scratch {
            return idx;
        }
        let idx = self.alloc_aux(AuxLocal::ScratchI32);
        self.make_closure_handle_scratch = Some(idx);
        idx
    }

    /// Z.4.3 — lazy scratch i32 used by `MakeClosure` to stash the
    /// captures-struct base pointer across the per-field stores.
    fn make_closure_captures_scratch(&mut self) -> u32 {
        if let Some(idx) = self.make_closure_captures_scratch {
            return idx;
        }
        let idx = self.alloc_aux(AuxLocal::ScratchI32);
        self.make_closure_captures_scratch = Some(idx);
        idx
    }

    /// Z.4.3 — reserve enough scratch slots for one `CallClosure`'s
    /// args, returning the wasm-local indices in IR push order. The
    /// slots are pooled across `CallClosure` ops in the same body —
    /// a later call with the same param-type-sequence reuses the
    /// same slots, keeping the local-section compact.
    fn call_arg_scratches_for(&mut self, param_tys: &[IrType]) -> Vec<u32> {
        // Look for an existing pool whose ty-sequence matches.
        let need: Vec<IrType> = param_tys.to_vec();
        if let Some(pool) = self.call_closure_arg_pools.iter().find(|p| p.tys == need) {
            return pool.slots.clone();
        }
        let mut slots = Vec::with_capacity(param_tys.len());
        for ty in param_tys {
            let idx = self.alloc_aux(AuxLocal::CallArgScratch(*ty));
            slots.push(idx);
        }
        self.call_closure_arg_pools.push(CallArgPool {
            tys: need,
            slots: slots.clone(),
        });
        slots
    }

    /// Z.4.3 — lazy scratch i32 used by `CallClosure` to stash the
    /// closure handle while the args are popped + re-pushed in
    /// indirect-call order.
    fn call_handle_scratch(&mut self) -> u32 {
        if let Some(idx) = self.call_closure_handle_scratch {
            return idx;
        }
        let idx = self.alloc_aux(AuxLocal::ScratchI32);
        self.call_closure_handle_scratch = Some(idx);
        idx
    }

    /// Encode the walker's accumulated state into a `wasm_encoder::Function`.
    fn finalise(self) -> Result<Function, LowerError> {
        if !self.saw_return_prep {
            // The IR didn't honour the return-shape prep contract —
            // scalar-Int needs a trailing `StoreField(Ret.value)`,
            // Dict shape needs at least one `AllocRootRecord`. Refuse
            // to emit so the host re-routes to the tree-walker tier.
            return Err(LowerError::UnsupportedOp(
                "no_return_prep",
                UnsupportedOpReason::BufferProtocolRecord,
            ));
        }

        // Group local declarations by wasm `ValType` so wasm-encoder's
        // run-length local declaration shape stays compact. The walk
        // honours alloc-order ([`aux_locals`]) so each entry's
        // pre-recorded `wasm_idx` matches its position in the emitted
        // section.
        let mut groups: Vec<(u32, ValType)> = Vec::new();
        let push_group = |vt: ValType, groups: &mut Vec<(u32, ValType)>| match groups.last_mut() {
            Some((count, ref last_ty)) if *last_ty == vt => *count += 1,
            _ => groups.push((1, vt)),
        };
        for aux in &self.aux_locals {
            let vt = match aux {
                AuxLocal::Let(ty) => match ty {
                    IrType::I64 => ValType::I64,
                    IrType::Bool | IrType::I32 => ValType::I32,
                    IrType::F64 => ValType::F64,
                    // Closure-typed lets are i32 arena handles. Other
                    // pointer-shaped types (String / List variants)
                    // currently surface as walker errors at the
                    // matching op-emit arms, but we accept them here
                    // for future widening.
                    IrType::Closure
                    | IrType::Null
                    | IrType::String
                    | IrType::ListInt
                    | IrType::ListFloat
                    | IrType::ListBool
                    | IrType::ListString
                    | IrType::ListSchema => ValType::I32,
                },
                AuxLocal::RecordBase => ValType::I32,
                AuxLocal::ScratchI64 => ValType::I64,
                AuxLocal::ScratchI32 => ValType::I32,
                AuxLocal::CallArgScratch(ty) => match ty {
                    IrType::I64 => ValType::I64,
                    IrType::F64 => ValType::F64,
                    IrType::Bool | IrType::I32 => ValType::I32,
                    IrType::Closure
                    | IrType::Null
                    | IrType::String
                    | IrType::ListInt
                    | IrType::ListFloat
                    | IrType::ListBool
                    | IrType::ListString
                    | IrType::ListSchema => ValType::I32,
                },
            };
            push_group(vt, &mut groups);
        }

        let mut func = Function::new(groups);
        for ins in self.insns {
            func.instruction(&ins);
        }
        func.instruction(&wasm_encoder::Instruction::End);
        Ok(func)
    }
}

/// Map an IR op to a stable debug tag — used by the fallthrough
/// `Other(...)` scope-cut so tracing groups by op name without
/// pulling `Debug` strings into the error payload.
fn op_debug_tag(op: &Op) -> &'static str {
    match op {
        Op::ConstBool(_) => "ConstBool",
        Op::ConstI32(_) => "ConstI32",
        Op::ConstI64(_) => "ConstI64",
        Op::ConstF64(_) => "ConstF64",
        Op::ConstString { .. } => "ConstString",
        Op::ConstListInt { .. } => "ConstListInt",
        Op::ConstListFloat { .. } => "ConstListFloat",
        Op::ConstListBool { .. } => "ConstListBool",
        Op::ConstListString { .. } => "ConstListString",
        Op::LetGet { .. } => "LetGet",
        Op::LetSet { .. } => "LetSet",
        Op::LocalGet(_) => "LocalGet",
        Op::LoadField { .. } => "LoadField",
        Op::StoreField { .. } => "StoreField",
        Op::DictGetByStringKey { .. } => "DictGetByStringKey",
        Op::ListGetByIntIdx { .. } => "ListGetByIntIdx",
        Op::Add(_) => "Add",
        Op::StrConcatN { .. } => "StrConcatN",
        Op::Sub(_) => "Sub",
        Op::Mul(_) => "Mul",
        Op::Div(_) => "Div",
        Op::Mod(_) => "Mod",
        Op::BitAnd(_) => "BitAnd",
        Op::Eq(_) => "Eq",
        Op::Ne(_) => "Ne",
        Op::Lt(_) => "Lt",
        Op::Le(_) => "Le",
        Op::Gt(_) => "Gt",
        Op::Ge(_) => "Ge",
        Op::If { .. } => "If",
        Op::LoadStringPtr { .. } => "LoadStringPtr",
        Op::LoadListIntPtr { .. } => "LoadListIntPtr",
        Op::LoadListFloatPtr { .. } => "LoadListFloatPtr",
        Op::LoadListBoolPtr { .. } => "LoadListBoolPtr",
        Op::LoadListStringPtr { .. } => "LoadListStringPtr",
        Op::LoadListSchemaPtr { .. } => "LoadListSchemaPtr",
        Op::Return => "Return",
        Op::AllocRootRecord { .. } => "AllocRootRecord",
        Op::AllocSubRecord { .. } => "AllocSubRecord",
        Op::StoreFieldAtRecord { .. } => "StoreFieldAtRecord",
        Op::PushRecordBase { .. } => "PushRecordBase",
        Op::EmitTailRecordFromAbsoluteAddr { .. } => "EmitTailRecordFromAbsoluteAddr",
        Op::Call { .. } => "Call",
        Op::ReadStringLen => "ReadStringLen",
        Op::Select { .. } => "Select",
        Op::LoadFieldAtAbsolute { .. } => "LoadFieldAtAbsolute",
        Op::LoadSchemaPtr { .. } => "LoadSchemaPtr",
        _ => "unknown_op",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a Relon source through parse + analyze +
    /// `lower_workspace_single` and return the resulting
    /// [`LoweredEntry`]. Helper for the walker round-trip tests.
    fn lower_source(src: &str) -> LoweredEntry {
        let ast = relon_parser::parse_document(src).expect("parse");
        let analyzed = relon_analyzer::analyze(&ast);
        relon_ir::lower_workspace_single(&analyzed, &ast).expect("lower_workspace_single")
    }

    #[test]
    fn walker_lowers_w12_increment() {
        // `x + 1` — the simplest possible body: one LoadField on the
        // Int param, one ConstI64, one Add(I64), one StoreField on
        // Ret.value, one Return.
        let lowered = lower_source("#main(Int x) -> Int\nx + 1");
        let bytes = lower_ir_module(&lowered).expect("lower_ir_module(W12)");
        // Round-trip through wasmparser so a malformed emit fails
        // here instead of at wasmtime instantiate time.
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates W12 walker output");
    }

    #[test]
    fn walker_lowers_arithmetic_chain() {
        // Slightly larger body: nested arithmetic + parens. Still
        // single-Int param, single Int return.
        let lowered = lower_source("#main(Int n) -> Int\n(n + 1) * (n + 2) - n");
        let bytes = lower_ir_module(&lowered).expect("lower arithmetic chain");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates arithmetic chain");
    }

    #[test]
    fn walker_lowers_ternary() {
        let lowered = lower_source("#main(Int n) -> Int\nn < 0 ? 0 : n");
        let bytes = lower_ir_module(&lowered).expect("lower ternary");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates ternary");
    }

    #[test]
    fn walker_lowers_range_reduce_loop() {
        // Z.4.2 — `range(n).reduce(0, (acc, i) => acc + i)` lowers to
        // a `Block { Loop { BrIf, Block { ... }, Br } }` skeleton with
        // i64 LetGet/LetSet ops carrying the loop counter + accumulator.
        // The walker emits wasm-native `block`/`loop`/`br`/`br_if`,
        // mirroring how the LLVM AOT backend lowers the same IR shape.
        // Validate the module round-trips through wasmparser; the
        // host-evaluator smoke (`tests/z4_list_smoke.rs`) covers
        // semantic correctness end-to-end.
        let lowered = lower_source("#main(Int n) -> Int\nrange(n).reduce(0, (acc, i) => acc + i)");
        let bytes = lower_ir_module(&lowered).expect("lower range.reduce");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates range.reduce walker output");
    }

    #[test]
    fn walker_lowers_const_list_int_return() {
        // Z.4.2 — `#main(...) -> List<Int>\n[1, 2, 3]` lowers to:
        //
        //   ConstListInt { idx: 0, elements: [1, 2, 3] }
        //   StoreField { offset: 0, ty: ListInt }
        //   Return
        //
        // The walker materialises the list record into an active data
        // segment (layout `[len: u32 LE][pad: u32 zero][i64
        // elements...]`) at offset `CONST_LIST_DATA_BASE` and emits
        // `i32.const 1024` for the `ConstListInt` op. The trailing
        // `Return` zext's the i32 pointer to i64 for the typed-func
        // result; the host's `WasmEvaluator::run_main` decodes the
        // list out of linear memory.
        let lowered = lower_source("#main(Int n) -> List<Int>\n[1, 2, 3]");
        let bytes = lower_ir_module(&lowered).expect("lower const-list-int return");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates const-list-int return walker output");
        // The emitted module MUST carry exactly one data segment
        // (the list record); count it via a quick wasmparser walk.
        let mut data_segments = 0usize;
        for payload in wasmparser::Parser::new(0).parse_all(&bytes) {
            if let Ok(wasmparser::Payload::DataSection(reader)) = payload {
                data_segments += reader.count() as usize;
            }
        }
        assert_eq!(
            data_segments, 1,
            "const-list-int return should install exactly one data segment"
        );
    }

    #[test]
    fn walker_lowers_nested_range_reduce() {
        // Z.4.2 — W9 inline-Int shape, nested O(n²) accumulator loop.
        // Two `Block { Loop { ... } }` regions stacked under the outer
        // reduce; the walker honours the depth discipline by emitting
        // matching wasm `block`/`loop` boundaries. The IR's BrIf
        // depths (0/1) translate to wasm branch depths verbatim.
        let lowered = lower_source(
            "#main(Int n) -> Int\n\
             range(n).reduce(0, (acc, j) =>\n\
               acc + range(n).reduce(0, (inner, i) => inner + (i * n + j)))",
        );
        let bytes = lower_ir_module(&lowered).expect("lower nested range.reduce");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates nested range.reduce walker output");
    }

    #[test]
    fn walker_lowers_simple_dict_return() {
        // Z.4.1 — the minimum-viable Dict-return shape: a single Int
        // field whose value is derived from an `#main` Int param. The
        // IR pipeline lowers this to:
        //
        //   AllocRootRecord { record_local_idx: 0 }
        //   LoadField { offset: <n>, ty: I64 }
        //   ConstI64(1)
        //   Add(I64)
        //   StoreFieldAtRecord { record_local_idx: 0, offset: 0, ty: I64 }
        //   Return
        //
        // and the walker emits `(i64) -> i64` where the i64 result is
        // a zext'd i32 arena pointer to a one-field record. Validate
        // the module round-trips through wasmparser; the
        // host-evaluator integration covers semantic correctness
        // separately in `relon-wasm-evaluator`'s smoke suite.
        let lowered = lower_source("#main(Int n) -> Dict\n{ result: n + 1 }");
        let bytes = lower_ir_module(&lowered).expect("lower_ir_module(Dict { result: n + 1 })");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates Dict-return walker output");
    }

    #[test]
    fn walker_lowers_multi_field_dict_return() {
        // Z.4.1 — two Int fields. Stresses the per-field offset wiring
        // in `StoreFieldAtRecord` (field 0 at offset 0, field 1 at
        // offset 8 per the schema layout's natural Int alignment).
        let lowered = lower_source(
            "#main(Int n) -> Dict\n\
             { first: n, second: n + 1 }",
        );
        let bytes =
            lower_ir_module(&lowered).expect("lower_ir_module(Dict { first: n, second: n + 1 })");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates multi-field Dict-return");
    }

    #[test]
    fn walker_lowers_w7_production_closure() {
        // Z.4.3 — production W7 shape: `#main(Int n) -> Dict { ... }`
        // with an `#internal fib: (k) => ...` first-class recursive
        // closure called via `result: fib(n)`. The IR pipeline
        // lowers this into:
        //
        //   * an outer entry body emitting `AllocRootRecord` +
        //     `MakeClosure { fn_table_idx: 0, captures: [
        //     ClosureCapture { let_idx: 0, ty: Closure, offset: 0 }] }`
        //     + `LetSet { ty: Closure }` + `LetGet { ty: Closure }`
        //     + `LoadField { ty: I64 }` + `CallClosure { param_tys:
        //     [I64], ret_ty: I64 }` + `StoreFieldAtRecord` +
        //     `Return`, and
        //   * one lambda function (`__closure_0`) whose body reads
        //     its self-handle out of the captures struct (LocalGet 0
        //     + LoadI32AtAbsolute), the loop var k out of LocalGet
        //     1, then doubly recurses through two `CallClosure {
        //     param_tys: [I64], ret_ty: I64 }` ops.
        //
        // Z.4.3 wires the funcref table + `MakeClosure` / `CallClosure`
        // lowering so the walker emits a valid wasm module instead of
        // scope-cutting under `ClosureValue`. Validate the module
        // round-trips through wasmparser; the semantic smoke
        // (Compiled tier + correct fib(22) = 17711) lives on the
        // host side in `tests/z4_closure_smoke.rs`.
        let lowered = lower_source(
            "#main(Int n) -> Dict\n\
             {\n\
               #internal\n\
               fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
               result: fib(n)\n\
             }",
        );
        let bytes = lower_ir_module(&lowered).expect("lower_ir_module(W7 production)");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates W7 production walker output");
    }

    #[test]
    fn walker_scope_cuts_string_concat() {
        // W3 production source — `String` return + reduce + closure.
        // The IR lowering itself surfaces the String concat as a
        // `StrConcatN` op the walker scope-cuts on. We assert the
        // tag groups under `StringOrStdlib` so the host's tracing
        // layer routes it to the Z.4 string follow-up queue.
        let lowered = lower_source(
            "#import list from \"std/list\"\n\
             #main(Int n) -> String\n\
             range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)",
        );
        let err = lower_ir_module(&lowered).expect_err("walker should reject W3");
        match err {
            LowerError::UnsupportedOp(_, reason) => {
                // The exact op depends on lowering pass; both
                // String-return and stdlib reduce paths group here.
                assert!(
                    matches!(
                        reason,
                        UnsupportedOpReason::StringOrStdlib | UnsupportedOpReason::Other(_)
                    ),
                    "W3 scope-cut should route to StringOrStdlib follow-up, got {reason:?}"
                );
            }
            other => panic!("expected UnsupportedOp, got {other:?}"),
        }
    }
}
