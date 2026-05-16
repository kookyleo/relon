//! Phase 1.beta linear-typed IR.
//!
//! The IR is a flat, stack-machine instruction stream — one `Func`
//! per `#main` (later: per top-level callable). Each op carries the
//! source [`TokenRange`] so Phase 1.gamma's `relon.srcmap` custom
//! section can emit a wasm-offset -> source-range table without
//! re-walking the analyzer tree.
//!
//! Stack discipline (v1.beta): `ConstI64` / `ConstF64` / `LocalGet`
//! push one value of the carried [`IrType`]; the binary arithmetic
//! ops pop two operands of the same type and push the result of the
//! same type; `Return` pops the single remaining value and ends the
//! function. Mixed-type bodies are rejected at codegen — see
//! `crates/relon-codegen-wasm/src/error.rs`.
//!
//! Operand types are recorded on the op itself (the `(IrType)`
//! suffix on `Add` / `Sub` / `Mul` / `Div` / `Mod`) so the wasm
//! emitter can pick `i64.add` vs `f64.add` in O(1) per op without
//! re-running a type-inference pass. The decision was made when
//! AnalyzerTree was still in scope during lowering; carrying the
//! type forward is strictly cheaper than re-deriving it from a
//! virtual stack inside the codegen pass.

use ordered_float::OrderedFloat;
use relon_parser::TokenRange;

/// Scalar value type in v1.beta / Phase 2.c. Mirrors the wasm
/// value-type subset the codegen pass emits — `Int` lowers to `I64`,
/// `Float` lowers to `F64`, `Bool` and `Null` lower to `I32` (a single
/// byte on the wire but loaded into an i32 wasm slot). Phase 2.c adds
/// the variable-length leaves `String` / `ListInt` as i32 pointers on
/// the wasm operand stack (the pointer points at the tail-area record
/// `[len: u32 LE][bytes...]`). Later phases extend this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IrType {
    /// 32-bit signed integer. Used for wasm-level handshake slots
    /// only — `in_ptr` / `in_len` / `out_ptr` / `out_cap` parameters
    /// and the `bytes_written` return. Not surfaced as a user-facing
    /// Relon scalar.
    I32,
    /// 64-bit signed integer (Relon `Int`).
    I64,
    /// IEEE-754 double-precision float (Relon `Float`).
    F64,
    /// Boolean (Relon `Bool`). 1 byte on the wire, lifted to `i32` on
    /// the wasm operand stack via `i32.load8_u`.
    Bool,
    /// Unit / `Null` placeholder. 1 byte on the wire (always `0`); the
    /// codegen path emits `i32.const 0` rather than reading memory.
    Null,
    /// Pointer to a tail-area `[len: u32 LE][utf8 bytes]` record. The
    /// pointer is a wasm `i32` (relative to the linear memory base);
    /// the IR keeps a distinct tag so diagnostics + srcmap can tell a
    /// raw `i32` slot apart from a String pointer.
    String,
    /// Pointer to a tail-area `[len: u32 LE][i64 elements]` record.
    /// Same wasm-side representation as `String` (an `i32` pointer),
    /// but tagged separately at IR-level so we can later distinguish
    /// `List<Int>` operations from raw byte pointers.
    ListInt,
}

impl IrType {
    /// Wasm operand-stack representation. `Int`/`Float` keep their
    /// `i64`/`f64` shape; `Bool`/`Null`/`String`/`ListInt` all occupy
    /// an `i32` slot (a 0/1 byte, a 0 tag, or a pointer). Used by the
    /// codegen vstack to compare across-branch frame types in `If`.
    pub fn wasm_slot(self) -> IrType {
        match self {
            IrType::I64 | IrType::F64 => self,
            IrType::I32 | IrType::Bool | IrType::Null | IrType::String | IrType::ListInt => {
                IrType::I32
            }
        }
    }
}

/// One lowered module — a flat list of functions plus an optional
/// pointer to the entry. v1.beta only ever populates a single
/// function (the `#main` body, named `run_main`); the vector form
/// keeps the data model honest for Phase 2+ multi-function emit.
///
/// Phase 6 adds the `imports` slot: every `#native` declaration in
/// scope contributes one [`NativeImport`] entry. Codegen emits a
/// `(import "env" <name> ...)` wasm import per entry **before** any
/// stdlib / user function so wasm function indices stay stable
/// (imports first, then stdlib, then user code).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Module {
    /// Host-provided `#native` functions the module needs at
    /// instantiate time. Each entry becomes one wasm `import` line;
    /// `Op::CallNative { import_idx }` references the entry by
    /// position in this vector.
    pub imports: Vec<NativeImport>,
    /// Lowered functions in declaration order.
    pub funcs: Vec<Func>,
    /// Index into `funcs` of the `#main` entry, when one was lowered.
    /// `None` for library modules (no `#main`); v1.beta lowering
    /// rejects this shape with `LoweringError::MissingMain` before
    /// returning, so the field is informational.
    pub entry_func_index: Option<usize>,
}

/// One declared `#native` function in the IR module — a host import
/// the wasm runtime materialises through the `env` module at
/// instantiate time.
///
/// The host SDK validates declared imports against its registered
/// `Context::functions` table when loading the module; mismatch
/// surfaces as a load-time error (see `relon-codegen-wasm`'s
/// `LoadError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeImport {
    /// Import name. Must match both the wasm `(import "env" <name>)`
    /// line and the `relon.host_fns` table entry codegen emits.
    pub name: String,
    /// Param types in declaration order. Codegen uses these to
    /// derive the wasm function signature and to validate
    /// `Op::CallNative`'s operand discipline.
    pub param_tys: Vec<IrType>,
    /// Return type. Single value (no tuples) in v1.
    pub ret_ty: IrType,
    /// Capability bit required to invoke this fn. Codegen emits the
    /// `check_cap` prologue ahead of every `Op::CallNative` whose
    /// `cap_bit` is anything other than [`NO_CAPABILITY_BIT`].
    pub cap_bit: u32,
}

/// Sentinel `cap_bit` meaning "no capability required". Mirrors
/// `relon-codegen-wasm::host_fns::NO_CAPABILITY` so both crates agree
/// on the encoding without an explicit cross-dependency.
pub const NO_CAPABILITY_BIT: u32 = u32::MAX;

/// One lowered function. Stack-based body; locals are addressed by
/// the function-parameter declaration order index (no separate symbol
/// table — v1.beta only ever sees the wasm-side handshake params).
#[derive(Debug, Clone, PartialEq)]
pub struct Func {
    /// Wasm export name. Phase 2.b always emits `"run_main"` for the
    /// entry function; non-entry functions stay unexported.
    pub name: String,
    /// Wasm function-parameter types in declaration order. Phase 2.b
    /// pins these to the four binary-handshake slots
    /// `(in_ptr i32, in_len i32, out_ptr i32, out_cap i32)` for the
    /// entry function; user-declared `#main` parameters are surfaced
    /// via [`Op::LoadField`]. Index into this vector is the operand
    /// of `Op::LocalGet`.
    pub params: Vec<IrType>,
    /// Wasm return type. Phase 2.b pins this to `I64` — the
    /// `bytes_written` count returned by the binary handshake. Single
    /// value (no tuples) in this phase.
    pub ret: IrType,
    /// Op stream. Pushes / pops follow the discipline documented at
    /// the module level.
    pub body: Vec<TaggedOp>,
    /// Source range of the function's declaration (the `#main(...)`
    /// directive range, or the function declaration range in later
    /// phases). Used by the wasm srcmap section.
    pub range: TokenRange,
}

/// One IR op paired with the source range it lowered from. The
/// range is what Phase 1.gamma's `relon.srcmap` section turns into
/// per-instruction source positions; v1.beta retains it eagerly so
/// the gamma pass is a non-breaking emit-only addition.
#[derive(Debug, Clone, PartialEq)]
pub struct TaggedOp {
    /// The actual op.
    pub op: Op,
    /// Source range that produced it (literal token, variable token,
    /// or binary-operator-spanning node range).
    pub range: TokenRange,
}

/// Stack-machine ops. Each variant documents its stack effect.
///
/// The binary arithmetic ops carry an [`IrType`] tag so the wasm
/// emitter picks `i64.*` vs `f64.*` without re-deriving types. The
/// lowering pass guarantees the tag matches the actual operand
/// types on the virtual stack at emit time; mismatches are caller
/// bugs and codegen surfaces them via `CodegenError::MixedNumericTypes`.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Push a Bool literal. Stack: `[] -> [Bool]`. Codegen emits
    /// `i32.const 1` for `true` and `i32.const 0` for `false`.
    /// Carries its own constructor (rather than reusing `ConstI64`)
    /// so the virtual stack tracks the result as `Bool` — that lets
    /// downstream comparison / `if` paths refuse mismatched arms
    /// without re-deriving types.
    ConstBool(bool),
    /// Push an `i64` constant. Stack: `[] -> [i64]`.
    ConstI64(i64),
    /// Push an `f64` constant. Stack: `[] -> [f64]`.
    /// `OrderedFloat` so the enum can derive `PartialEq` and `Eq` —
    /// same trick the parser uses for `Expr::Float`.
    ConstF64(OrderedFloat<f64>),
    /// Push an absolute wasm linear-memory address of a constant
    /// String record laid out in the data section. Stack: `[] -> [String]`.
    ///
    /// The address points at the first byte of a `[len: u32 LE][utf8 bytes]`
    /// record; the record itself is materialised in a wasm `Data`
    /// section by the codegen pass. Codegen collects every
    /// `ConstString` op when scanning the IR module and emits a single
    /// passive-style initialiser at module load time.
    ///
    /// The `idx` is an arbitrary per-module identifier; codegen maps
    /// it to a concrete memory offset.
    ConstString {
        /// Per-module identifier the codegen layout pass uses to look
        /// up the record's absolute memory offset.
        idx: u32,
        /// The string bytes themselves — codegen copies these into
        /// the data section verbatim.
        value: String,
    },
    /// Push an absolute wasm linear-memory address of a constant
    /// List<Int> record. Stack: `[] -> [ListInt]`.
    ///
    /// Record layout in the data section: `[len: u32 LE][pad: u32 zero][i64 elements]`.
    /// Total size: `8 + 8 * elements.len()` bytes. The 4-byte pad
    /// keeps the elements 8-aligned **inside the record** when the
    /// record itself is placed at an 8-aligned absolute address. The
    /// codegen layout pass aligns each List<Int> data-section entry
    /// to 8 to satisfy that invariant.
    ConstListInt {
        /// Per-module identifier the codegen layout pass uses to look
        /// up the record's absolute memory offset.
        idx: u32,
        /// The i64 elements — codegen materialises them into the
        /// data section in little-endian order.
        elements: Vec<i64>,
    },
    /// Push a user-let-binding local. Stack: `[] -> [ty]`.
    ///
    /// The `idx` is a per-function local index for `let`-bound names
    /// — distinct from the wasm-handshake slots that [`Op::LocalGet`]
    /// addresses. Codegen allocates a wasm local of the matching
    /// `ValType` for each unique `idx` it sees and translates
    /// `LetGet { idx }` to `local.get $(WASM_FIRST_LET_LOCAL + idx)`.
    LetGet {
        /// Per-function let-local index.
        idx: u32,
        /// IR type of the bound value. Determines the wasm valtype
        /// of the underlying local declaration.
        ty: IrType,
    },
    /// Pop the top of the stack into a user-let-binding local.
    /// Stack: `[ty] -> []`. See [`Op::LetGet`] for the local-index
    /// semantics.
    LetSet {
        /// Per-function let-local index.
        idx: u32,
        /// IR type of the value being stored. Codegen uses this to
        /// pick the matching wasm valtype for the local declaration.
        ty: IrType,
    },
    /// Push the value of local `index`. Stack: `[] -> [T]`.
    /// `index` is a wasm function-local slot index. In Phase 2.b the
    /// `run_main` signature is `(in_ptr, in_len, out_ptr, out_cap)`,
    /// so the locals here are the four i32 handshake slots; user
    /// fields are loaded via [`Op::LoadField`].
    LocalGet(u32),
    /// Load a single field from the input buffer at `offset` bytes.
    /// Stack: `[] -> [T]` where `T` is dictated by `ty`. Codegen emits
    /// `local.get $in_ptr; <load>.offset=N` — `i64.load` for `I64`,
    /// `f64.load` for `F64`, `i32.load8_u` for `Bool`, and a literal
    /// `i32.const 0` for `Null` (no memory read needed for the unit
    /// placeholder).
    LoadField {
        /// Byte offset of the field inside the input buffer, supplied
        /// by `relon_eval_api::layout::OffsetTable`.
        offset: u32,
        /// Field's IR type. Determines which wasm load opcode the
        /// codegen pass picks.
        ty: IrType,
    },
    /// Store the top stack value to the output buffer at `offset`
    /// bytes. Stack: `[T] -> []`. Phase 2.b emits a single trailing
    /// `StoreField` per `run_main` body (one root return value); later
    /// phases extend this to multi-field record returns.
    StoreField {
        /// Byte offset of the slot inside the output buffer.
        offset: u32,
        /// Slot type. Determines the wasm store opcode (`i64.store`,
        /// `f64.store`, `i32.store8`).
        ty: IrType,
    },
    /// Pop two operands of the tagged type, push their sum. Stack:
    /// `[T, T] -> [T]`.
    Add(IrType),
    /// Pop two operands of the tagged type, push their difference.
    Sub(IrType),
    /// Pop two operands of the tagged type, push their product.
    Mul(IrType),
    /// Pop two operands of the tagged type, push their quotient.
    /// `I64` lowers to `i64.div_s` (signed); `F64` lowers to `f64.div`.
    Div(IrType),
    /// Pop two operands of the tagged type, push the remainder.
    /// `I64` lowers to `i64.rem_s` (signed); `F64` is rejected at
    /// lowering — wasm has no `f64.rem`, so `LoweringError::UnsupportedOperator`
    /// fires before this variant is even constructed for floats.
    Mod(IrType),
    /// Pop two operands of the tagged type, push the boolean result.
    /// Stack: `[T, T] -> [Bool]`. Lowers to `i64.eq` / `f64.eq` /
    /// `i32.eq` depending on `T`'s wasm slot. `Null == Null` always
    /// emits `i32.const 1` (no operand consumed from memory).
    Eq(IrType),
    /// Pop two operands of the tagged type, push the negated boolean
    /// result. Stack: `[T, T] -> [Bool]`.
    Ne(IrType),
    /// Pop two operands, push `lhs < rhs`. Stack: `[T, T] -> [Bool]`.
    /// Signed comparison for `I64`. Rejected at codegen for `Bool`
    /// / `Null` / `String` / `ListInt` — those types have no defined
    /// ordering relation at the wasm layer.
    Lt(IrType),
    /// Pop two operands, push `lhs <= rhs`. See [`Op::Lt`] for the
    /// type constraints; ordering rules mirror `Lt`.
    Le(IrType),
    /// Pop two operands, push `lhs > rhs`.
    Gt(IrType),
    /// Pop two operands, push `lhs >= rhs`.
    Ge(IrType),
    /// Conditional. Stack effect: `[Bool] -> [result_ty]`.
    ///
    /// Codegen emits `if (result <ty>) <then_body> else <else_body>
    /// end`. The `then_body` and `else_body` each leave one value of
    /// `result_ty` on the operand stack. Frame validation pairs the
    /// pop of the condition with the entry depth so a body that
    /// inadvertently grows the stack mid-branch surfaces at emit
    /// rather than producing an invalid wasm module.
    If {
        /// Type both branches push at the end. Codegen translates to
        /// `BlockType::Result(<valtype>)`.
        result_ty: IrType,
        /// Body executed when the condition is non-zero.
        then_body: Vec<TaggedOp>,
        /// Body executed when the condition is zero.
        else_body: Vec<TaggedOp>,
    },
    /// Load a `[len: u32 LE][utf8 bytes]` pointer from the input
    /// buffer at `offset` bytes. Stack: `[] -> [String]`. Codegen
    /// emits `local.get $in_ptr; i32.load offset=N` — the pointer
    /// value loaded is itself a wasm-linear-memory address (the host
    /// writes the pointer when building the in_buf).
    LoadStringPtr {
        /// Byte offset of the pointer slot inside the input buffer.
        offset: u32,
    },
    /// Load a `[len: u32 LE][i64 elements]` pointer from the input
    /// buffer at `offset` bytes. Stack: `[] -> [ListInt]`. Same wasm
    /// emission shape as [`Op::LoadStringPtr`]; the distinct IR tag
    /// lets later phases dispatch on element type.
    LoadListIntPtr {
        /// Byte offset of the pointer slot inside the input buffer.
        offset: u32,
    },
    /// Pop the top value and end the function (wasm `end` does the
    /// implicit return). Must be the last op in `Func::body`.
    Return,

    /// Phase 3.b dict literal construction.
    ///
    /// Mark the start of building the **return root** record (the
    /// dict the `#main` directly returns). The fixed area for the
    /// root record sits at `out_ptr + 0..return_root_size` — the
    /// host pre-sized `out_buf` to cover it, so the lowering doesn't
    /// need to bump `$tail_cursor` for the root itself. Codegen
    /// stores `0` into the wasm local at `record_local_idx`
    /// (FIRST_RECORD_LOCAL_INDEX + N) so subsequent
    /// [`Op::StoreFieldAtRecord`] / [`Op::PushRecordBase`] ops can
    /// address fields relative to the root base uniformly with sub-
    /// records.
    AllocRootRecord {
        /// Per-function index of the wasm local that holds the
        /// record's base (out_ptr-relative byte offset). Codegen
        /// allocates one i32 local per unique index past the
        /// per-function let-locals area.
        record_local_idx: u32,
    },
    /// Phase 3.b dict literal construction.
    ///
    /// Allocate a **nested** sub-record's fixed area in the parent
    /// buffer's tail area. Aligns `$tail_cursor` up to `root_align`,
    /// performs the out_cap bounds check, stores the aligned cursor
    /// into the record local, and bumps `$tail_cursor += root_size`.
    /// Subsequent [`Op::StoreFieldAtRecord`] ops write into the
    /// sub-record's fixed area; the parent's pointer slot receives
    /// the sub-record base via [`Op::PushRecordBase`] +
    /// [`Op::StoreFieldAtRecord`].
    AllocSubRecord {
        /// Per-function local index for the sub-record's base
        /// offset. Same allocation scheme as
        /// [`Op::AllocRootRecord::record_local_idx`].
        record_local_idx: u32,
        /// Fixed-area size of the sub-schema in bytes.
        root_size: u32,
        /// Required alignment of the sub-schema's fixed area in
        /// bytes — codegen aligns `$tail_cursor` up to this before
        /// recording the base.
        root_align: u32,
    },
    /// Phase 3.b dict literal construction.
    ///
    /// Pop a value off the stack and store it into the in-construction
    /// record at `out_ptr + $record_local + offset`. The op tag drives
    /// the wasm store opcode:
    ///
    /// * `I64` / `F64` — pop scalar, `i64.store` / `f64.store`.
    /// * `Bool` / `Null` — pop i32, `i32.store8`.
    /// * `String` / `ListInt` — pop i32 (an out_ptr-relative offset
    ///   produced by [`Op::EmitTailRecordFromAbsoluteAddr`] or
    ///   [`Op::PushRecordBase`]), store as i32 via `i32.store`. The
    ///   stored value is a buffer-relative pointer the host reader
    ///   dereferences directly.
    StoreFieldAtRecord {
        /// Per-function local index naming the record's base offset.
        record_local_idx: u32,
        /// Byte offset of the field inside the record's fixed area.
        offset: u32,
        /// Field's IR type. Drives the wasm store-opcode selection.
        ty: IrType,
    },
    /// Phase 3.b dict literal construction.
    ///
    /// Push the current value of `$record_local` onto the wasm operand
    /// stack as an i32. Used when the surrounding parent record needs
    /// to store the sub-record's base offset into its pointer slot.
    PushRecordBase {
        /// Per-function local index naming the sub-record's base
        /// offset.
        record_local_idx: u32,
    },
    /// Phase 3.b dict literal construction.
    ///
    /// Pop an absolute wasm-memory address pointing at a
    /// `[len:u32 LE][payload]` record (from `ConstString` /
    /// `ConstListInt` / `LoadStringPtr` / `LoadListIntPtr`), memcpy
    /// the record into the `out_buf` tail area at `$tail_cursor`,
    /// bump `$tail_cursor` by the record size, and push the new
    /// buffer-relative offset of the record onto the stack as i32.
    ///
    /// Used for emitting String / List<Int> fields inside a dict
    /// literal: the resulting offset is what the parent's pointer
    /// slot stores via [`Op::StoreFieldAtRecord`] with `ty:
    /// String`/`ListInt`.
    EmitTailRecordFromAbsoluteAddr {
        /// IR type of the record. Drives record-size computation
        /// (String: `len + 4`, ListInt: `8 + 8 * count`) and the
        /// pre-write alignment of `$tail_cursor` (4 for String,
        /// 8 for ListInt).
        ty: IrType,
    },

    /// Phase 4.a stdlib dispatch.
    ///
    /// Pop `arg_count` operands off the virtual stack in matching
    /// order (last argument pushed last) and emit a wasm `call
    /// <fn_index>`. Pushes one value of `ret_ty` back onto the stack.
    ///
    /// The `fn_index` is a wasm-level function index — that is, an
    /// index into the module's combined function table where Phase
    /// 4.a prepends the bundled stdlib functions before the user
    /// functions. Codegen does **not** rewrite this index; the
    /// lowering pass is responsible for picking the correct stdlib
    /// or user function slot via [`crate::stdlib::stdlib_function_index`].
    ///
    /// The lowering pass validates parameter types when it builds
    /// this op; codegen re-checks the popped operand types match the
    /// wasm slot of each declared param type so a hand-built IR
    /// with mismatched arg/ret types surfaces deterministically.
    Call {
        /// Combined wasm-module function index of the callee.
        fn_index: u32,
        /// Number of arguments to pop off the stack before emitting
        /// the call. Codegen pops them in reverse-push order and
        /// validates the wasm slot of each against the callee's
        /// declared param types.
        arg_count: u32,
        /// Argument types expected by the callee, in declaration
        /// order. Codegen pops `arg_count` operands and verifies
        /// each one's wasm slot matches the matching `param_tys`
        /// entry; mismatches surface as
        /// `CodegenError::CallTypeMismatch`.
        param_tys: Vec<IrType>,
        /// IR type pushed back onto the stack after the call.
        ret_ty: IrType,
    },

    /// Phase 4.a stdlib primitive.
    ///
    /// Pop a pointer-indirect record pointer (i32 wasm slot, absolute
    /// wasm-memory address of a `[len:u32 LE][payload]` record) and
    /// push the length as an `I64` value. Codegen lowers to:
    ///
    /// ```text
    /// i32.load offset=0 align=2   ;; u32 LE length prefix
    /// i64.extend_i32_u            ;; widen to i64 for the IR's Int slot
    /// ```
    ///
    /// Kept as its own op (rather than reusing `LoadField`) because
    /// the operation isn't field-name-driven: the pointer source is
    /// the value on top of the stack, and the load offset is fixed
    /// at zero by the record layout. A dedicated op also keeps
    /// diagnostics from confusing a stdlib byte-length read with a
    /// user-facing field load.
    ///
    /// Reused across both pointer-indirect leaves whose tail-area
    /// layout starts with a `u32 LE` length prefix: `String`
    /// (`[len][utf8...]`) and `ListInt` (`[len][pad][i64...]`). The
    /// name is kept for backward compatibility — semantically it now
    /// reads the leading u32 of any such record.
    ReadStringLen,

    /// Phase 4.b stdlib primitive.
    ///
    /// Wasm `select` / `select t` operator: pop `[a, b, cond_i32]` and
    /// push `a` if `cond` is non-zero, else `b`. Both `a` and `b` must
    /// share the same wasm slot; the op carries the result type so
    /// codegen can emit the typed `select t` form and validate
    /// branches at IR-level without re-deriving types.
    ///
    /// Stack effect: `[T, T, i32] -> [T]`.
    Select {
        /// IR type of both operands and the resulting value. Codegen
        /// translates this to a single `wasm-encoder::Instruction::TypedSelect`
        /// with the matching `ValType`.
        ty: IrType,
    },

    /// Phase 5 schema-method dispatch primitive.
    ///
    /// Pop an i32 absolute wasm-memory address pointing at the first
    /// byte of a schema instance's fixed area; push the field at
    /// `offset` of type `ty`. Mirrors [`Op::LoadField`] but the base
    /// address is supplied dynamically by the operand stack rather
    /// than implicitly read off the `in_ptr` handshake slot.
    ///
    /// Stack: `[i32] -> [T]` where `T` is decided by `ty`. Codegen
    /// emits the matching `i64.load` / `f64.load` / `i32.load*` (with
    /// `offset = N` baked into the memarg) after popping the address.
    /// Used both for `self.field` access inside a schema method's body
    /// and for chained-segment access (`obj.sub.leaf`) when `obj` is
    /// schema-typed.
    LoadFieldAtAbsolute {
        /// Byte offset of the field inside the schema's fixed area.
        offset: u32,
        /// Field's IR type — drives the wasm load opcode selection.
        /// `String` / `ListInt` here load the i32 pointer slot *as
        /// is* (buffer-relative offset preserved); call sites that
        /// need an absolute pointer must follow up with a separate
        /// lift step.
        ty: IrType,
    },

    /// Phase 5 schema-method dispatch primitive.
    ///
    /// Lift a schema-typed pointer slot in the `in_buf` to an
    /// absolute wasm-memory address. Stack: `[] -> [i32]`.
    ///
    /// Layout: the `in_buf`'s fixed area carries a 4-byte
    /// buffer-relative offset at `offset`; codegen reads it via
    /// `local.get $in_ptr; i32.load offset=N`, then adds `in_ptr` to
    /// produce the absolute address of the schema instance's fixed
    /// area. Mirrors [`Op::LoadStringPtr`] / [`Op::LoadListIntPtr`]
    /// but tags the pushed value as a schema instance pointer rather
    /// than a `[len][payload]` record pointer — the lowering pass
    /// tracks the schema brand alongside.
    LoadSchemaPtr {
        /// Byte offset of the pointer slot inside the input buffer.
        offset: u32,
    },

    /// Phase 6 host-fn dispatch.
    ///
    /// Invoke a host-provided `#native` function declared in the IR
    /// module's `imports` table. Pops `param_tys.len()` operands off
    /// the virtual stack in matching order (last argument pushed
    /// last) and pushes one value of `ret_ty` back onto the stack.
    ///
    /// `import_idx` is the **position** of the matching entry in
    /// `Module::imports`. Codegen translates the index to the wasm-
    /// level function index by adding the import-section offset
    /// (imports always occupy `0..imports.len()` in the combined
    /// wasm function table). `cap_bit` mirrors the entry's
    /// `NativeImport::cap_bit`; when it's anything other than
    /// [`NO_CAPABILITY_BIT`], codegen automatically prepends a
    /// `check_cap` prologue before the actual `call` so the trap
    /// fires before the host fn observes any arguments.
    CallNative {
        /// Position of the matching entry in `Module::imports`.
        import_idx: u32,
        /// Param types expected by the host fn, in declaration order.
        /// Codegen pops `param_tys.len()` operands off the vstack and
        /// verifies each one's wasm slot matches the matching entry.
        param_tys: Vec<IrType>,
        /// IR type pushed back onto the stack after the call.
        ret_ty: IrType,
        /// Capability bit guarding the call. [`NO_CAPABILITY_BIT`]
        /// means no guard is emitted.
        cap_bit: u32,
    },

    /// Phase 6 capability guard.
    ///
    /// Emits the wasm sequence
    /// `global.get $relon_caps_avail; i64.const (1 << cap_bit);
    /// i64.and; i64.eqz; if; unreachable; end`. The `unreachable`
    /// trap fires when the requested bit is not set in the host's
    /// granted bitmap, surfacing as a `CapabilityDenied` runtime
    /// error after Phase 7 wires the trap-translate path.
    ///
    /// Stack effect: `[] -> []`. Normally lowering inlines the check
    /// into `Op::CallNative` (cheaper to emit + tighter src-map
    /// locality), but the dedicated op stays available for callers
    /// that want to assert capability without performing a call —
    /// e.g. an analyzer that pre-flights a `cap_grants` snapshot.
    CheckCap {
        /// Bit position in the `relon_caps_avail` u64 bitmap.
        /// [`NO_CAPABILITY_BIT`] is a no-op (codegen elides the
        /// prologue).
        cap_bit: u32,
    },
}
