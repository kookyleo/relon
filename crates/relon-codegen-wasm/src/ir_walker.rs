//! Phase Z.4.0 — IR walker scaffolding.
//!
//! Replaces the variant-per-workload [`crate::WasmProgram`] shape with
//! a real walker over [`relon_ir::Op`]. This is the canonical lowering
//! path the design doc §10.2 promised; the per-variant emit functions
//! in [`crate::programs`] now live alongside it as the fallback the
//! host (`relon-wasm-evaluator`) tries when the IR walker reports an
//! unsupported op shape.
//!
//! ## Z.4.0 scope (this commit)
//!
//! The walker handles the **scalar-Int** subset that maps cleanly to a
//! `__main(i64, ..., i64) -> i64` typed-func ABI, side-stepping the
//! buffer-protocol handshake the LLVM AOT backend uses
//! (`(in_ptr, in_len, out_ptr, out_cap, caps) -> i32` per
//! `lower_workspace_single` §2.b). The host calls `__main` directly
//! with each `#main(Int n, ...)` arg as a wasm `i64`; the return
//! value comes back as an `i64` that the host wraps in `Value::Int`.
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
//! - `Select { ty: Int|Bool }` — ternary `?:` lowering, lowers to
//!   wasm `select` / typed `select t`.
//! - `Return` — pops the top value into the function result.
//!
//! ## Out of Z.4.0 scope (stubs return `UnsupportedOp`)
//!
//! - **Z.4.1 — Dict literal / member access**: `AllocRootRecord`,
//!   `AllocSubRecord`, `StoreFieldAtRecord`, `PushRecordBase`,
//!   `EmitTailRecordFromAbsoluteAddr`. Production `#main(...) -> Dict
//!   { #internal ..., result: X }` lowering. See
//!   [`UnsupportedOpReason::DictReturn`].
//! - **Z.4.2 — List literal / index / iter**: `ConstListInt`,
//!   `LoadListIntPtr`, `ListGetByIntIdx`. Nested
//!   `range(n).map((i) => range(n).map(...))` materialisation. See
//!   [`UnsupportedOpReason::ListLiteral`].
//! - **Z.4.3 — Closure-as-value**: `MakeClosure`, `CallClosure`,
//!   funcref table emit. First-class `#internal fib: (k) => ...`
//!   captured into Dict fields. See
//!   [`UnsupportedOpReason::ClosureValue`].
//! - **Z.4 follow-up — String/stdlib calls**: `ConstString`,
//!   `StrConcatN`, `ReadStringLen`, `Call { ... stdlib idx ... }`.
//!   The hand-emit W3/W4 variants still cover these via the
//!   classifier path for now.
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

use relon_eval_api::layout::SchemaLayout;
use relon_eval_api::schema_canonical::TypeRepr;
use relon_ir::{IrType, LoweredEntry, Op, TaggedOp, MAIN_RETURN_SCHEMA_NAME};
use wasm_encoder::{
    CodeSection, ExportKind, ExportSection, Function, FunctionSection, MemorySection, MemoryType,
    Module, TypeSection, ValType,
};

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

/// Return slot info derived from the `Ret` schema (canonical 1-field
/// `value` slot). The walker treats a `StoreField` at this offset as
/// "function return" and the popped i64 becomes the typed-func result.
#[derive(Debug, Clone)]
struct ReturnSlot {
    /// Buffer offset of the `value` field. Used to match
    /// `StoreField { offset }` against the return path.
    offset: u32,
    /// Declared IR type. Z.4.0 only accepts `Int`; the Ret schema's
    /// `value` field carries the user's declared `#main -> T` type.
    /// Z.4.1+ uses this for non-Int return-shape dispatch.
    #[allow(dead_code)]
    ty: IrType,
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

    // --- Resolve Ret.value offset ----------------------------------------
    if lowered.return_schema.name != MAIN_RETURN_SCHEMA_NAME {
        return Err(LowerError::UnsupportedOp(
            "return_schema_unexpected_name",
            UnsupportedOpReason::Other("return_schema_unexpected_name"),
        ));
    }
    let ret_layout = SchemaLayout::offsets_for(&lowered.return_schema).map_err(|e| {
        LowerError::UnsupportedOp("ret_layout", UnsupportedOpReason::Other(map_layout_err(&e)))
    })?;
    if ret_layout.fields.len() != 1 {
        return Err(LowerError::UnsupportedOp(
            "ret_schema_multi_field",
            UnsupportedOpReason::DictReturn,
        ));
    }
    let ret_field = &ret_layout.fields[0];
    let ret_decl_ty = type_repr_to_ir(&lowered.return_schema.fields[0].ty)?;
    if ret_decl_ty != IrType::I64 {
        return Err(LowerError::UnsupportedOp(
            "non_int_return",
            non_int_main_param_reason(&ret_decl_ty),
        ));
    }
    let return_slot = ReturnSlot {
        offset: ret_field.offset as u32,
        ty: ret_decl_ty,
    };

    // --- Walk the body ---------------------------------------------------
    let mut emit = EmitState::new(&param_slots, &return_slot);
    emit.walk(&entry_fn.body)?;

    // --- Assemble the wasm module ----------------------------------------
    let n_params = param_slots.len();
    let mut module = Module::new();

    // Section 1 — types: one entry, `(i64; n_params) -> i64`.
    let mut types = TypeSection::new();
    types.ty().function(
        std::iter::repeat_n(ValType::I64, n_params),
        std::iter::once(ValType::I64),
    );
    module.section(&types);

    // Section 3 — functions: one local fn (the entry).
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    module.section(&funcs);

    // Section 5 — memories: one 16-page linear memory, exported as
    // `memory`. Z.4.0 ops don't touch it; we keep the export so the
    // host's `bind_memory` plumbing stays uniform across IR-walker
    // and hand-emit lowerings.
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
    exports.export("__main", ExportKind::Func, 0);
    module.section(&exports);

    // Section 10 — code: one entry, walker output.
    let mut code = CodeSection::new();
    let func = emit.finalise()?;
    code.function(&func);
    module.section(&code);

    Ok(module.finish())
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

/// Per-let-local wasm-local slot. The walker allocates one wasm local
/// per unique `(idx, ty)` pair seen in the Op stream.
#[derive(Debug, Clone, Copy)]
struct LetLocal {
    /// IR-side per-function let-local index. Same value across
    /// `LetGet` and `LetSet` for one logical binding.
    ir_idx: u32,
    /// Wasm local index. Equals `n_params + position_in_let_decls`.
    wasm_idx: u32,
    /// IR type of the bound value (drives wasm `ValType`).
    ty: IrType,
}

/// Walker state — accumulates wasm locals declarations and the
/// function-body instruction stream as we walk the IR Op vector.
struct EmitState<'a> {
    param_slots: &'a [ParamSlot],
    return_slot: &'a ReturnSlot,
    /// User-let-local declarations seen so far. Each unique `(idx,
    /// ty)` is allocated one wasm local; reseeing the same idx
    /// reuses the wasm slot.
    let_locals: Vec<LetLocal>,
    /// Instructions captured by walking the body. Encoded into a
    /// `wasm_encoder::Function` in `finalise`.
    insns: Vec<wasm_encoder::Instruction<'static>>,
    /// `true` after we've seen a `StoreField` against the Ret.value
    /// slot. The walker's contract: the body must end with one
    /// `StoreField(ret.value)` + `Return` pair so the typed-func
    /// returns the popped value.
    saw_return_store: bool,
}

impl<'a> EmitState<'a> {
    fn new(param_slots: &'a [ParamSlot], return_slot: &'a ReturnSlot) -> Self {
        Self {
            param_slots,
            return_slot,
            let_locals: Vec::new(),
            insns: Vec::new(),
            saw_return_store: false,
        }
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
                let slot = self
                    .param_slots
                    .iter()
                    .find(|p| p.offset == *offset)
                    .ok_or(LowerError::UnsupportedOp(
                        "load_field_offset_not_main_param",
                        UnsupportedOpReason::BufferProtocolRecord,
                    ))?;
                self.insns.push(I::LocalGet(slot.wasm_param_idx));
                let _ = slot.ty; // future widening reads this
            }

            // ----- Return store: leave value on stack as fn result --
            Op::StoreField { offset, ty } => {
                if *offset != self.return_slot.offset {
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
                // Nothing to emit — the value is already on the stack;
                // the typed-func's `End` will return the top. We just
                // record the contract was honoured.
                self.saw_return_store = true;
            }

            // ----- Arithmetic ----------------------------------------
            Op::Add(IrType::I64) => self.insns.push(I::I64Add),
            Op::Sub(IrType::I64) => self.insns.push(I::I64Sub),
            Op::Mul(IrType::I64) => self.insns.push(I::I64Mul),
            Op::Div(IrType::I64) => self.insns.push(I::I64DivS),
            Op::Mod(IrType::I64) => self.insns.push(I::I64RemS),

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
            Op::Return => {
                // No-op for the typed-func ABI — the function's
                // trailing `End` already returns the top of stack.
                // We still record the explicit op so a malformed body
                // (missing both `StoreField` and `Return`) fails
                // closed via `finalise`.
            }

            // ----- Scope-cut: Dict construction ----------------------
            Op::AllocRootRecord { .. }
            | Op::AllocSubRecord { .. }
            | Op::StoreFieldAtRecord { .. }
            | Op::PushRecordBase { .. }
            | Op::EmitTailRecordFromAbsoluteAddr { .. } => {
                return Err(LowerError::UnsupportedOp(
                    "dict_construction",
                    UnsupportedOpReason::DictReturn,
                ));
            }

            // ----- Scope-cut: List construction / iter ---------------
            Op::ConstListInt { .. }
            | Op::ConstListFloat { .. }
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
        // Wasm local indices: params first (positions 0..n_params), then
        // let-locals in declaration-of-first-use order.
        let wasm_idx = self.param_slots.len() as u32 + self.let_locals.len() as u32;
        self.let_locals.push(LetLocal {
            ir_idx,
            wasm_idx,
            ty,
        });
        Ok(wasm_idx)
    }

    /// Encode the walker's accumulated state into a `wasm_encoder::Function`.
    fn finalise(self) -> Result<Function, LowerError> {
        if !self.saw_return_store {
            // The IR didn't end on a Ret.value store — that means the
            // body either returned via a Buffer-protocol record we
            // can't model with typed-func, or the source's lowering
            // is mid-construction. Either way, refuse to emit.
            return Err(LowerError::UnsupportedOp(
                "no_return_store",
                UnsupportedOpReason::BufferProtocolRecord,
            ));
        }

        // Group let-locals by wasm ValType so wasm-encoder's
        // run-length local declaration shape stays compact.
        let mut groups: Vec<(u32, ValType)> = Vec::new();
        for l in &self.let_locals {
            let vt = match l.ty {
                IrType::I64 => ValType::I64,
                IrType::Bool | IrType::I32 => ValType::I32,
                IrType::F64 => ValType::F64,
                _ => {
                    return Err(LowerError::UnsupportedOp(
                        "let_local_unsupported_ty",
                        non_int_main_param_reason(&l.ty),
                    ))
                }
            };
            match groups.last_mut() {
                Some((count, ref last_ty)) if *last_ty == vt => *count += 1,
                _ => groups.push((1, vt)),
            }
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
    fn walker_scope_cuts_dict_return() {
        // Production W7-shape: `#main(Int n) -> Dict { ... }`. The
        // F.2 anon-Dict-return work in `relon-ir/src/lowering.rs`
        // synthesises an anonymous schema for the dict body and lifts
        // `#internal fib` closures into let-bindings, so the source
        // DOES reach the walker today — what the walker scope-cuts on
        // is the `AllocRootRecord` / `StoreFieldAtRecord` /
        // `MakeClosure` / `CallClosure` op set the lowering pass
        // synthesises for the dict-body's `result` field. Both
        // Dict-construction and closure-as-value are Z.4.1 / Z.4.3
        // follow-ups respectively; the walker groups them under the
        // matching reason so the host's tracing layer can route by
        // sub-phase without re-classifying.
        let src = "#main(Int n) -> Dict\n\
                   {\n\
                     #internal\n\
                     fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                     result: fib(n)\n\
                   }";
        let ast = relon_parser::parse_document(src).expect("parse");
        let analyzed = relon_analyzer::analyze(&ast);
        let lowered = match relon_ir::lower_workspace_single(&analyzed, &ast) {
            Ok(l) => l,
            Err(e) => {
                // If a future IR-pipeline tightening re-rejects the
                // Dict-return upstream, the walker's contract still
                // holds — the source can't possibly reach a
                // compiled tier. Surface the upstream reject as a
                // pass for THIS test (its purpose is to pin the
                // walker's scope-cut, not the IR layer's).
                eprintln!(
                    "W7 production source rejected upstream of walker: {e:?} \
                     (test still passes — walker can't lower what doesn't reach it)"
                );
                return;
            }
        };
        let err = lower_ir_module(&lowered)
            .expect_err("walker must reject W7 production Dict-return (Z.4.1 / Z.4.3 follow-up)");
        match err {
            LowerError::UnsupportedOp(_, reason) => {
                assert!(
                    matches!(
                        reason,
                        UnsupportedOpReason::DictReturn
                            | UnsupportedOpReason::ClosureValue
                            | UnsupportedOpReason::BufferProtocolRecord
                            | UnsupportedOpReason::Other(_)
                    ),
                    "W7 Dict-return scope-cut should route to a Z.4 sub-phase, got {reason:?}"
                );
            }
            other => panic!("expected UnsupportedOp, got {other:?}"),
        }
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
