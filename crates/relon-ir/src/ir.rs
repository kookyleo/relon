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
    /// Phase 10-c: pointer to a `[len: u32 LE][pad to 8][f64 elements]`
    /// record. Same wasm-side representation as `ListInt` — the
    /// distinct tag drives `length` / future map-/fold-style
    /// dispatch.
    ListFloat,
    /// Phase 10-c: pointer to a `[len: u32 LE][u8 booleans]` record.
    /// Booleans pack tightly per spec (no inter-element padding).
    /// Same wasm slot shape as the other list pointers.
    ListBool,
    /// Phase 10-c: pointer to a `[len: u32 LE][off_0: u32 LE]...`
    /// record whose entries each name a buffer-relative String
    /// `[len: u32 LE][utf8 bytes]` payload. Same wasm slot shape as
    /// the other list pointers.
    ListString,
    /// Phase 10-c: pointer to a `[len: u32 LE][off_0: u32 LE]...`
    /// record whose entries each name a buffer-relative sub-record
    /// fixed-area base. Carries no schema info on the IR-level tag —
    /// the lowering / codegen pass tracks the schema separately
    /// through the field's declared `TypeRepr`.
    ListSchema,
    /// Phase 10-a: pointer to an 8-byte closure handle record laid
    /// out in scratch memory as `[fn_table_idx: u32 LE][captures_ptr:
    /// u32 LE]`. Same wasm-side representation as `String` /
    /// `ListInt` (an `i32` pointer), but tagged at IR-level so the
    /// lowering pass can dispatch higher-order argument shapes
    /// (`xs.map(|x| ...)`) and codegen can route them through
    /// `call_indirect`.
    Closure,
}

impl IrType {
    /// Wasm operand-stack representation. `Int`/`Float` keep their
    /// `i64`/`f64` shape; `Bool`/`Null`/`String`/`ListInt`/`Closure`
    /// all occupy an `i32` slot (a 0/1 byte, a 0 tag, or a pointer).
    /// Used by the codegen vstack to compare across-branch frame
    /// types in `If`.
    pub fn wasm_slot(self) -> IrType {
        match self {
            IrType::I64 | IrType::F64 => self,
            IrType::I32
            | IrType::Bool
            | IrType::Null
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::Closure => IrType::I32,
        }
    }

    /// `true` when the IR type is a pointer-indirect list. Used by
    /// the codegen pass when it needs to detect "any list pointer"
    /// without enumerating every concrete element-tagged variant.
    pub fn is_list_pointer(self) -> bool {
        matches!(
            self,
            IrType::ListInt
                | IrType::ListFloat
                | IrType::ListBool
                | IrType::ListString
                | IrType::ListSchema
        )
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
    /// Phase 10-a: IR-side function indices the codegen must place
    /// into the module's `funcref` table. Each entry's position in
    /// this vector is the closure's wasm `Table` slot, which
    /// `Op::MakeClosure` stores into its handle's `fn_table_idx`
    /// field. Empty when the module contains no lambdas — codegen
    /// then skips the table / element sections entirely.
    pub closure_table: Vec<u32>,
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
    /// Push an `i32` constant. Stack: `[] -> [i32]`. Phase 4.c-2:
    /// added so stdlib bodies that perform pointer / length
    /// arithmetic can materialise immediate sizes without going
    /// through the i64 slot (which would force wrap/extend
    /// conversions on every use). Not surfaced as a user-facing
    /// literal — `Int` literals still lower through
    /// [`Op::ConstI64`].
    ConstI32(i32),
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
    /// Phase 10-c: push an absolute wasm linear-memory address of a
    /// constant `List<Float>` record. Stack: `[] -> [ListFloat]`.
    ///
    /// Data-section layout mirrors `ConstListInt`:
    /// `[len: u32 LE][pad: u32 zero][f64 elements]`. Codegen aligns
    /// the record start to 8 so the f64 payload sits on an 8-byte
    /// boundary, matching what `BufferBuilder::write_list_float`
    /// would have produced for the same value.
    ConstListFloat {
        /// Per-module identifier mapped to the record's absolute
        /// memory offset by the codegen layout pass.
        idx: u32,
        /// The f64 elements — codegen materialises them into the
        /// data section in little-endian order. Stored as `u64`
        /// bitwise so the op can derive `Eq` / `Hash` alongside the
        /// rest of the IR variants.
        elements: Vec<u64>,
    },
    /// Phase 10-c: push an absolute wasm linear-memory address of a
    /// constant `List<Bool>` record. Stack: `[] -> [ListBool]`.
    ///
    /// Data-section layout: `[len: u32 LE][u8 booleans]`. The booleans
    /// pack tightly per spec, no padding between elements.
    ConstListBool {
        /// Per-module identifier mapped to the record's absolute
        /// memory offset by the codegen layout pass.
        idx: u32,
        /// The boolean elements — codegen materialises them as
        /// `0u8` / `1u8` bytes.
        elements: Vec<bool>,
    },
    /// Phase 10-c: push an absolute wasm linear-memory address of a
    /// constant `List<String>` record. Stack: `[] -> [ListString]`.
    ///
    /// Data-section layout: each entry's String `[len: u32 LE][utf8]`
    /// record is emitted into the data section first; then the list
    /// header `[len: u32 LE][off_0: u32 LE]...` is emitted afterwards,
    /// with each `off_i` resolved to the absolute address of the
    /// matching String record. The op's `idx` lookup returns the
    /// header offset (= the pushed pointer value).
    ConstListString {
        /// Per-module identifier mapped to the header record's
        /// absolute memory offset by the codegen layout pass.
        idx: u32,
        /// The string elements — codegen materialises each into its
        /// own data-section record (no dedup with `ConstString`
        /// occurrences in v1, kept simple).
        elements: Vec<String>,
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
    /// Phase 4.c-2: bitwise AND on two operands of the tagged type.
    /// Stack effect: `[T, T] -> [T]`. Only `I32` / `I64` are valid;
    /// other tags surface as `CodegenError::MixedNumericTypes`.
    ///
    /// Not part of the user-facing surface (Relon-level boolean `and`
    /// short-circuits); stdlib bodies use it for power-of-two
    /// alignment masks (e.g. `(x + 7) & -8` to round up to 8).
    BitAnd(IrType),
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
    /// Phase 10-c: load a `[len: u32 LE][pad][f64 elements]` pointer
    /// from the input buffer at `offset` bytes. Stack: `[] -> [ListFloat]`.
    /// Same wasm emission shape as [`Op::LoadListIntPtr`] — the IR
    /// keeps the tag distinct so downstream dispatch is unambiguous.
    LoadListFloatPtr {
        /// Byte offset of the pointer slot inside the input buffer.
        offset: u32,
    },
    /// Phase 10-c: load a `[len: u32 LE][u8 booleans]` pointer from
    /// the input buffer at `offset` bytes. Stack: `[] -> [ListBool]`.
    LoadListBoolPtr {
        /// Byte offset of the pointer slot inside the input buffer.
        offset: u32,
    },
    /// Phase 10-c: load a `[len: u32 LE][off_0: u32]...` pointer
    /// from the input buffer at `offset` bytes. Stack: `[] -> [ListString]`.
    /// Pulls the list header pointer; each per-entry String payload
    /// stays in tail memory until the host (or future stdlib body)
    /// dereferences the pointer-array entries.
    LoadListStringPtr {
        /// Byte offset of the pointer slot inside the input buffer.
        offset: u32,
    },
    /// Phase 10-c: load a `[len: u32 LE][off_0: u32]...` pointer
    /// from the input buffer at `offset` bytes for a list of branded
    /// sub-records. Stack: `[] -> [ListSchema]`.
    LoadListSchemaPtr {
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

    /// Phase 4.c-1 control flow primitive.
    ///
    /// Emit a wasm `block <blocktype>` containing `body`, followed by
    /// the matching `end`. The `block` form does not loop — a `br`
    /// inside `body` jumps past the closing `end` (i.e. forward exit).
    ///
    /// Stack effect: when `result_ty` is `None` the block is
    /// stack-neutral (`[] -> []` after the body, modulo any side
    /// effects). When `result_ty` is `Some(t)`, the body must end with
    /// one value of `t` on top of the operand stack and the block
    /// pushes that value into the outer stack (`[] -> [t]`). Codegen
    /// emits the matching `BlockType::Empty` or
    /// `BlockType::Result(<valtype>)` form respectively.
    ///
    /// Nested [`Op::Block`] / [`Op::Loop`] enter a new vstack frame —
    /// inner branches cannot leak intermediate operands out through
    /// the surrounding block. The frame discipline is enforced by
    /// codegen via the same `emit_op_seq` recursion used for
    /// [`Op::If`].
    Block {
        /// Optional result type. `None` for stack-neutral blocks
        /// (the common shape for loop carriers); `Some(t)` for blocks
        /// that produce a single value on exit.
        result_ty: Option<IrType>,
        /// Op stream forming the block body. Lowering / codegen
        /// recursively walks the body with a fresh vstack frame.
        body: Vec<TaggedOp>,
    },

    /// Phase 4.c-1 control flow primitive.
    ///
    /// Emit a wasm `loop <blocktype>` containing `body`, followed by
    /// the matching `end`. A `br` inside `body` targeting this loop
    /// jumps **back** to the `loop` header (i.e. continue); to exit
    /// the loop the body must `br` to an enclosing [`Op::Block`]
    /// (forward exit pattern).
    ///
    /// Stack effect mirrors [`Op::Block`] — `result_ty == None` for
    /// stack-neutral bodies (the iteration carrier lives in locals);
    /// `result_ty == Some(t)` for loops that yield a single value.
    /// Most loop shapes use `None` and stash the running aggregate in
    /// a wasm local declared via [`Op::LetSet`].
    Loop {
        /// Optional loop-block result type. See [`Op::Block`] for
        /// the stack-effect semantics.
        result_ty: Option<IrType>,
        /// Op stream forming the loop body.
        body: Vec<TaggedOp>,
    },

    /// Phase 4.c-1 control flow primitive.
    ///
    /// Unconditional branch to the enclosing labelled construct at
    /// `label_depth` (0 = innermost). For a [`Op::Block`] the branch
    /// jumps past the matching `end`; for a [`Op::Loop`] the branch
    /// jumps back to the `loop` header. Stack effect: `[] -> []` from
    /// the IR's point of view, but the wasm verifier treats the
    /// remainder of the surrounding block as unreachable after a
    /// `br` — codegen relies on the verifier rather than tracking
    /// dead code in the IR.
    Br {
        /// Label depth: 0 names the innermost enclosing block/loop.
        label_depth: u32,
    },

    /// Phase 4.c-1 control flow primitive.
    ///
    /// Conditional branch — pop one `i32` (Bool) and, if non-zero,
    /// branch to the construct at `label_depth`. The stack effect is
    /// `[Bool] -> []`. As with [`Op::Br`], the wasm verifier handles
    /// the branched-out arm; ops after a `br_if` that fires are
    /// reached only when the condition was zero.
    BrIf {
        /// Label depth: 0 names the innermost enclosing block/loop.
        label_depth: u32,
    },

    /// Phase 4.c-1 control flow primitive.
    ///
    /// Indirect branch — pop one `i32` index `n`. When `n < targets.len()`
    /// branch to `targets[n]`; otherwise branch to `default`. Useful
    /// for jump tables (`match` on a small-cardinality discriminant);
    /// not used by any current lowering but exposed so a future
    /// `match` lowering can hand the discriminant straight to wasm
    /// without manual chained `BrIf` cascades.
    ///
    /// Stack effect: `[i32] -> []` (verifier-side, like [`Op::Br`]).
    BrTable {
        /// Default label depth used when the index is out of range.
        default: u32,
        /// Per-index label depths.
        targets: Vec<u32>,
    },

    /// Phase 4.c-1 wasm-internal bump allocator.
    ///
    /// Reserve `size_bytes` of scratch space starting at the current
    /// value of the module-internal `relon_scratch_cursor` global,
    /// bump the cursor by `size_bytes`, and push the **pre-bump**
    /// cursor value (the allocated region's base wasm-memory address)
    /// onto the operand stack as an `i32`.
    ///
    /// Stack effect: `[] -> [i32]`.
    ///
    /// Trap discipline: codegen emits a `cursor + size_bytes >
    /// memory.size_in_bytes` bounds check before bumping; overflow
    /// surfaces as a wasm `unreachable` recorded as
    /// `UnreachableKind::ScratchOOM` so the trap translator can
    /// produce a `RuntimeError::WasmScratchOOM`.
    ///
    /// The scratch region is owned by the wasm module — host SDKs
    /// do not need to allocate it, and the region is reset to the
    /// post-out_buf base on every entry-function invocation (the
    /// prologue writes `out_ptr + out_cap` into the cursor before
    /// the body runs). The single-threaded execution model means the
    /// bump itself does not need atomic semantics.
    AllocScratch {
        /// Static byte count to reserve. Codegen emits this as an
        /// immediate `i32.const` in the bump sequence.
        size_bytes: u32,
    },

    /// Phase 4.c-1 wasm-internal bump allocator — dynamic size form.
    ///
    /// Same shape as [`Op::AllocScratch`] but the size is taken from
    /// the top of the operand stack instead of an op immediate.
    ///
    /// Stack effect: `[i32] -> [i32]`. The pre-bump cursor is pushed
    /// after the dynamic size is consumed.
    AllocScratchDyn,

    /// Phase 4.c-2 raw-memory primitive.
    ///
    /// Pop an `i32` absolute wasm-memory address and load the
    /// little-endian `i32` value at `addr + offset`. Stack effect:
    /// `[i32] -> [i32]`. Lowers to `i32.load offset=N align=2` after
    /// the address is consumed from the stack.
    ///
    /// Distinct from [`Op::LoadField`] / [`Op::LoadFieldAtAbsolute`]
    /// because the base is supplied by an arbitrary expression (e.g.
    /// the return of [`Op::AllocScratch`] / [`Op::AllocScratchDyn`])
    /// rather than the `in_buf` handshake or a schema-instance
    /// pointer. Stdlib bodies that walk freshly-allocated scratch
    /// buffers use this op together with [`Op::StoreI32AtAbsolute`]
    /// to read/write u32 length prefixes without going through the
    /// fixed-area record machinery.
    LoadI32AtAbsolute {
        /// Byte offset added to the popped base address before the
        /// load. Encoded as the wasm `memarg.offset` immediate.
        offset: u32,
    },

    /// Phase 4.c-2 raw-memory primitive.
    ///
    /// Pop an `i32` absolute wasm-memory address and load the
    /// little-endian `i64` value at `addr + offset`. Stack effect:
    /// `[i32] -> [i64]`. Lowers to `i64.load offset=N align=3`.
    ///
    /// Used by stdlib reducers (`list_int_sum`, `list_int_max`) when
    /// they index into a `List<Int>` record's i64 payload area.
    LoadI64AtAbsolute {
        /// Byte offset added to the popped base address before the
        /// load.
        offset: u32,
    },

    /// Phase 4.c-2 raw-memory primitive.
    ///
    /// Pop an `i32` value and an `i32` absolute wasm-memory address,
    /// then store the value at `addr + offset` as a little-endian
    /// `i32`. Stack discipline mirrors wasm `i32.store`:
    /// `[addr, value] -> []` (address pushed first, value pushed on
    /// top). Lowers to `i32.store offset=N align=2`.
    ///
    /// Stdlib bodies that build a fresh `String` / `List<Int>`
    /// record in scratch space use this op to write the leading
    /// `u32 LE` length prefix.
    StoreI32AtAbsolute {
        /// Byte offset added to the address operand before the
        /// store.
        offset: u32,
    },

    /// Phase 4.c-2 raw-memory primitive.
    ///
    /// Pop an `i64` value and an `i32` absolute wasm-memory address,
    /// then store the value at `addr + offset` as a little-endian
    /// `i64`. Stack discipline: `[addr, value] -> []`. Lowers to
    /// `i64.store offset=N align=3`.
    ///
    /// Used by stdlib reducers when writing `i64` payload elements
    /// into a freshly allocated `List<Int>` record's payload area.
    StoreI64AtAbsolute {
        /// Byte offset added to the address operand before the
        /// store.
        offset: u32,
    },

    /// Phase 4.c-2 raw-memory primitive.
    ///
    /// Pop three `i32` values — destination address, source address,
    /// and byte length — and emit a wasm `memory.copy` instruction.
    /// Stack discipline mirrors the wasm instruction:
    /// `[dst, src, len] -> []` (dst pushed first, src next, len on
    /// top). Codegen lowers to `memory.copy dst_mem=0 src_mem=0`.
    ///
    /// Used by stdlib bodies that splice bytes between scratch
    /// buffers and existing String / List<Int> records without
    /// going through the tail-record machinery (`concat`,
    /// `substring`, ...). The `MemoryCopy` instruction is part of
    /// the wasm bulk-memory proposal which wasmtime keeps enabled
    /// by default, so no engine feature gate is required.
    MemcpyAtAbsolute,

    /// Phase 4.c-2 raw-memory primitive.
    ///
    /// Pop an `i32` absolute address and push the unsigned byte at
    /// `addr + offset` widened to `i32`. Stack effect: `[i32] -> [i32]`.
    /// Lowers to `i32.load8_u offset=N align=0`.
    ///
    /// Used by ASCII case-fold stdlib bodies (`upper` / `lower`)
    /// and prefix predicates (`starts_with`) to read one byte at a
    /// time without bitmasking a wider load.
    LoadI8UAtAbsolute {
        /// Byte offset added to the popped base address before the
        /// load.
        offset: u32,
    },

    /// Phase 4.c-2 raw-memory primitive.
    ///
    /// Pop an `i32` value and an `i32` absolute address, then store
    /// the value's low byte at `addr + offset`. Stack discipline
    /// mirrors `i32.store8`: `[addr, value] -> []`. Codegen lowers
    /// to `i32.store8 offset=N align=0`.
    ///
    /// Mirror of [`Op::LoadI8UAtAbsolute`] on the store side.
    StoreI8AtAbsolute {
        /// Byte offset added to the address operand before the
        /// store.
        offset: u32,
    },

    /// Phase 10-a raw-memory primitive.
    ///
    /// Pop an `i32` absolute address and load the little-endian
    /// `f64` value at `addr + offset`. Stack effect: `[i32] -> [f64]`.
    /// Lowers to `f64.load offset=N align=3`.
    ///
    /// Added alongside the closure-capture machinery so an `F64`
    /// captured into a lambda struct can be read back into the
    /// lambda's body without going through the `LoadFieldAtAbsolute`
    /// path (which rebases `in_ptr` and is the wrong shape for
    /// scratch-allocated captures).
    LoadF64AtAbsolute {
        /// Byte offset added to the popped base address before the
        /// load.
        offset: u32,
    },

    /// Phase 10-a raw-memory primitive.
    ///
    /// Pop an `f64` value and an `i32` absolute address, then store
    /// the value at `addr + offset` as little-endian `f64`. Stack
    /// discipline: `[addr, value] -> []`. Lowers to
    /// `f64.store offset=N align=3`.
    ///
    /// Mirror of [`Op::LoadF64AtAbsolute`] on the store side; used
    /// by the closure-conversion pass when writing an `F64` capture
    /// into the freshly allocated captures struct.
    StoreF64AtAbsolute {
        /// Byte offset added to the address operand before the
        /// store.
        offset: u32,
    },

    /// Phase 4.c-2: emit a wasm `unreachable` whose
    /// `relon.uctab` entry tags the trap kind. Codegen routes the
    /// runtime trap through `WasmModule::translate_trap` so the
    /// surfaced [`relon_eval_api::RuntimeError`] picks up the
    /// matching tag (`WasmIndexOutOfBounds`, `WasmEmptyList`).
    /// Stack effect: `[] -> [...]` — the wasm verifier treats every
    /// op after a `Trap` as unreachable.
    ///
    /// Reserved for stdlib bodies and future analyzer-driven
    /// invariant checks; user-surface lowering currently does not
    /// emit this op directly. The `kind` is restricted to the
    /// trap variants that have no semantic payload — capability /
    /// scratch / value-too-large traps still go through their own
    /// codegen helpers.
    Trap {
        /// Tag the codegen records in `relon.uctab` for this trap.
        /// Restricted to [`TrapKind::IndexOutOfBounds`] and
        /// [`TrapKind::EmptyList`] in this phase.
        kind: TrapKind,
    },

    /// Phase 10-a closure construction.
    ///
    /// Build an 8-byte closure handle in scratch memory:
    /// `[fn_table_idx: u32 LE][captures_ptr: u32 LE]`. Stack effect:
    /// `[] -> [Closure]`. Captures are read by codegen via
    /// `local.get $(first_let_local + capture.let_idx)`; the lowering
    /// pass pre-binds every captured value into a let-local before
    /// emitting this op.
    ///
    /// `fn_table_idx` is the wasm `Table` slot for the lambda's
    /// compiled function. The lowering pass assigns slots in source
    /// order; codegen materialises a `funcref` table sized to cover
    /// every emitted lambda and populates it via an
    /// `ElementSection`.
    ///
    /// When `captures` is empty the captures_ptr field is zero — the
    /// handle still allocates 8 bytes so the load discipline at the
    /// call site stays uniform.
    MakeClosure {
        /// Wasm `Table` slot for the lambda function. Codegen stores
        /// this index verbatim in the handle's first i32 field; the
        /// `call_indirect` at the call site picks the function up
        /// from `table[fn_table_idx]`.
        fn_table_idx: u32,
        /// Captured values laid out in the captures struct, in field
        /// order matching the operand-stack push order. Each entry
        /// carries the IR type (which drives the wasm store opcode)
        /// and a precomputed byte offset inside the captures struct.
        captures: Vec<ClosureCapture>,
        /// Total size of the captures struct in bytes. Codegen passes
        /// this to `Op::AllocScratch` so the alloc happens in a
        /// single wasm bump. Zero when `captures` is empty (codegen
        /// then skips the captures alloc entirely and writes 0 to
        /// the handle's captures_ptr slot).
        captures_size: u32,
    },

    /// Phase 10-a closure invocation.
    ///
    /// Indirect call through a closure handle. Stack discipline:
    /// `[Closure, arg0, arg1, ...] -> [ret_ty]`. The closure handle
    /// is pushed first, the user-visible arguments follow in
    /// declaration order. Codegen rearranges the operand stack so
    /// the wasm `call_indirect` sees `[captures_ptr, arg0, ..., argN,
    /// fn_table_idx]`:
    ///
    /// 1. Pop user-visible args off the operand stack in reverse and
    ///    spill them into the per-function closure-arg scratch
    ///    locals (`closure_arg_i32`, `closure_arg_i64_a`,
    ///    `closure_arg_i64_b`).
    /// 2. Pop the closure handle and stash it into the
    ///    `closure_handle` scratch local.
    /// 3. Re-push: `closure_handle`, then `i32.load(closure_handle +
    ///    4)` for captures_ptr, then each spilled arg in original
    ///    order, then `i32.load(closure_handle + 0)` for
    ///    fn_table_idx.
    /// 4. Emit `call_indirect` against the wasm type
    ///    `(captures_ptr, ...param_tys) -> ret_ty`.
    ///
    /// `param_tys` describes the *user-visible* arguments — the
    /// implicit captures_ptr first parameter is not included. Codegen
    /// prepends it when computing the wasm `call_indirect` type
    /// signature.
    ///
    /// Phase 10-a only emits this op from stdlib higher-order bodies
    /// (`list_int_map`, `list_int_filter`, `list_int_fold`) where the
    /// arg shape is statically known to fit the three reserved
    /// scratch slots. Future user-facing closure invocations may
    /// need a dynamically-sized spill area; the op's contract stays
    /// the same.
    CallClosure {
        /// User-visible parameter types in declaration order. Codegen
        /// pops `param_tys.len()` operands plus the closure handle
        /// before emitting `call_indirect`, then verifies each
        /// popped operand's wasm slot matches the matching entry.
        param_tys: Vec<IrType>,
        /// IR type pushed back onto the stack after the call.
        ret_ty: IrType,
    },

    /// v3+ a-4 Unicode case-folding primitive.
    ///
    /// Push the absolute wasm-memory address of one of the two
    /// codegen-managed simple case-folding tables. Stack effect:
    /// `[] -> [i32]`. Codegen lowers to `i32.const <table_addr>`
    /// where `<table_addr>` is the wasm-memory location of the
    /// `[count: u32 LE][(input_cp: u32, output_cp: u32) × count]`
    /// blob the codegen pass laid out in the const data section.
    ///
    /// The op only appears inside the bundled `upper` / `lower`
    /// stdlib bodies (and the shared `__casefold_lookup` helper they
    /// call). It is **not** part of the user-facing surface — there is
    /// no Relon-level syntax that lowers to it.
    ///
    /// The pre-DCE codegen scan treats this op the same way as
    /// `Op::ConstString`: when present in a reachable function body,
    /// the matching table is added to the const pool so the embedded
    /// `i32.const` resolves to a valid address. Unreachable
    /// upper/lower bodies stay pruned and the table is never emitted.
    CaseFoldTableAddr {
        /// `true` selects the upper-mapping table (lowercase ->
        /// uppercase), `false` selects the lower-mapping table.
        upper: bool,
    },

    /// v3++ b-4 grapheme-awareness primitive.
    ///
    /// Push the absolute wasm-memory address of the embedded Unicode
    /// `Mark` (Mn + Mc + Me) range table. Stack effect: `[] -> [i32]`.
    /// Codegen lowers to `i32.const <table_addr>` where `<table_addr>`
    /// is the wasm-memory location of the `[count: u32 LE][(start: u32,
    /// end: u32) × count]` blob the codegen pass laid out in the const
    /// data section.
    ///
    /// The op only appears inside the bundled `title` / `upper` /
    /// `lower` stdlib bodies (and the shared `__is_combining_mark`
    /// helper they call). Not surfaced through user-facing syntax —
    /// `at_word_start` / grapheme-aware case-folding decisions are
    /// implementation details of the case-folding bodies.
    ///
    /// The pre-DCE codegen collector treats this op the same way as
    /// `Op::CaseFoldTableAddr`: when present in a reachable body, the
    /// matching table is added to the const pool so the embedded
    /// `i32.const` resolves to a valid address. Unreachable bodies
    /// keep the table out of the data section entirely.
    CombiningMarkRangesAddr,

    /// v3++ b-4 word-boundary primitive.
    ///
    /// Push the absolute wasm-memory address of the embedded non-ASCII
    /// Unicode whitespace range table. Stack effect: `[] -> [i32]`.
    /// Codegen lowers to `i32.const <table_addr>` with the same
    /// `[count: u32 LE][(start: u32, end: u32) × count]` layout as
    /// [`Op::CombiningMarkRangesAddr`]. ASCII whitespace is checked via
    /// a direct comparison in the `title` body so the common case
    /// avoids the table lookup entirely; only non-ASCII codepoints
    /// fall through to the binary search.
    WhitespaceRangesAddr,

    /// v3++ b-5 Unicode normalization primitive.
    ///
    /// Push the absolute wasm-memory address of the embedded
    /// canonical (`upper = false` -> NFD) or compatibility
    /// (`upper = true` -> NFKD) decomposition table. Stack effect:
    /// `[] -> [i32]`. The boolean flag is reused for "table family"
    /// even though "upper" semantically does not apply here — the
    /// alternative would be a second variant, but the runtime body
    /// already toggles the table address through a flag, so we keep
    /// the same pattern.
    ///
    /// Layout in the const data section (see
    /// [`crate::normalization::encode_decomp_table_bytes`]):
    ///
    /// ```text
    /// [index_count: u32 LE]
    /// [(cp: u32 LE, pool_off: u32 LE, pool_len: u32 LE) x index_count]
    /// [pool_count: u32 LE]
    /// [cp: u32 LE x pool_count]
    /// ```
    ///
    /// Surfaces only inside the bundled `nfd` / `nfkd` / `nfc` / `nfkc`
    /// stdlib bodies and the shared `__decomp_lookup` helper they call.
    /// The DCE-friendly const-pool collector treats the op identically
    /// to [`Op::CaseFoldTableAddr`]: the matching table is laid out
    /// exactly when at least one reachable body references it.
    DecompTableAddr {
        /// `true` selects the compatibility (NFKD) decomposition
        /// table, `false` selects the canonical (NFD) table.
        compatibility: bool,
    },

    /// v3++ b-5 Unicode normalization primitive.
    ///
    /// Push the absolute wasm-memory address of the embedded
    /// Canonical_Combining_Class table. Stack effect: `[] -> [i32]`.
    /// Layout (see
    /// [`crate::normalization::encode_ccc_table_bytes`]):
    ///
    /// ```text
    /// [count: u32 LE]
    /// [(cp: u32 LE, ccc: u32 LE) x count]
    /// ```
    ///
    /// The CCC value is widened from `u8` to `u32` on the wire so the
    /// per-entry stride stays at 8 bytes — same arithmetic shape as
    /// the case-folding helper, allowing `(table_addr + 4 + mid * 8)`
    /// rebasing.
    ///
    /// Only the bundled normalization bodies reference this op.
    CccTableAddr,

    /// v3++ b-5 Unicode normalization primitive.
    ///
    /// Push the absolute wasm-memory address of the embedded canonical
    /// composition pair table. Stack effect: `[] -> [i32]`.
    /// Layout (see
    /// [`crate::normalization::encode_composition_table_bytes`]):
    ///
    /// ```text
    /// [count: u32 LE]
    /// [(first: u32 LE, second: u32 LE, composed: u32 LE) x count]
    /// ```
    ///
    /// Each entry is 12 bytes; the runtime helper binary-searches by
    /// the `(first, second)` lexicographic key. Composition exclusions
    /// (Full_Composition_Exclusion + CompositionExclusions.txt) are
    /// filtered at table-generation time, so the runtime never
    /// re-checks.
    ///
    /// Only the bundled `nfc` / `nfkc` stdlib bodies reference this
    /// op.
    CompositionTableAddr,

    /// v3++ b-6 full Unicode case folding (UAX #21) primitive.
    ///
    /// Push the absolute wasm-memory address of the embedded FULL
    /// multi-codepoint upper or lower folding table. Stack effect:
    /// `[] -> [i32]`. Codegen lowers to `i32.const <table_addr>`
    /// where the layout is
    /// `[count: u32 LE][(input_cp: u32, out0: u32, out1: u32, out2: u32, out_len: u32) × count]`
    /// — 20 bytes per entry so the runtime helper can rebase against
    /// `table_addr + 4 + mid * 20`. The output codepoints are
    /// inlined (max 3 per UAX #21) so the helper avoids a
    /// payload-pool indirection.
    ///
    /// Used by `upper` / `lower` / `title` / `upper_locale` /
    /// `lower_locale` / `title_locale` for the multi-codepoint cases
    /// (`ß` -> `SS`, `ﬁ` -> `FI`, `İ` -> `i` + combining-dot-above, …).
    /// Simple 1:1 mappings stay with `Op::CaseFoldTableAddr`.
    FullCaseFoldTableAddr {
        /// `true` selects the FULL upper mapping table; `false`
        /// selects the FULL lower table.
        upper: bool,
    },

    /// v3++ b-6 cased / case-ignorable property primitive.
    ///
    /// Push the absolute wasm-memory address of the UCD Cased ranges
    /// table. Stack effect: `[] -> [i32]`. Layout matches the
    /// combining-mark range tables — `[count: u32 LE][(start: u32,
    /// end: u32) × count]` — so the runtime helper reuses the
    /// `table_addr + 4 + mid * 8` rebase arithmetic.
    ///
    /// Used by the final-sigma right-scan inside the `lower` body to
    /// decide whether `Σ` (U+03A3) maps to `ς` (word-final) or `σ`
    /// (otherwise).
    CasedRangesAddr,

    /// v3++ b-6 case-ignorable range primitive. Mirrors
    /// [`Op::CasedRangesAddr`]; layout and rebase arithmetic match.
    CaseIgnorableRangesAddr,

    /// v3++ b-6 locale-specific override primitive.
    ///
    /// Push the absolute wasm-memory address of the Turkish /
    /// Azerbaijani upper or lower folding override table. Stack
    /// effect: `[] -> [i32]`. Layout matches
    /// [`Op::FullCaseFoldTableAddr`]; the locale-aware stdlib bodies
    /// consult this table before falling back to the default FULL /
    /// SIMPLE chain.
    TurkishCaseFoldTableAddr {
        /// `true` selects the Turkish upper table; `false` selects
        /// the Turkish lower table.
        upper: bool,
    },
}

/// Phase 10-a closure-capture record. One per captured variable on a
/// `MakeClosure` op.
///
/// Each capture references a per-function let-local the lowering pass
/// stashed the captured value into before emitting `MakeClosure`.
/// Codegen reads each value via `local.get $(first_let_local +
/// let_idx)` and stores it at its declared offset inside the freshly
/// allocated captures struct.
#[derive(Debug, Clone, PartialEq)]
pub struct ClosureCapture {
    /// Per-function let-local index holding the captured value. The
    /// lowering pass pre-binds every captured variable to a fresh
    /// let-local immediately before emitting `MakeClosure`, even
    /// when the captured value was already in a let-local at source
    /// level — this keeps the codegen pass agnostic about whether
    /// the source-level identifier is a let-binding, a `#main`
    /// param, or a method-bound parameter.
    pub let_idx: u32,
    /// IR type of the captured value. Drives both the read opcode
    /// (`local.get` + the value type) and the store opcode
    /// (`i64.store` for `I64`, `i32.store` for pointer / `I32`
    /// slots, `f64.store` for `F64`) when materialising the captures
    /// struct.
    pub ty: IrType,
    /// Byte offset of the field inside the captures struct. The
    /// lowering pass picks offsets so each field is naturally aligned
    /// (8 for `I64` / `F64`, 4 for everything else); codegen trusts
    /// the precomputed offset and emits the matching store opcode.
    pub offset: u32,
}

/// Phase 4.c-2 stdlib trap discriminator. Mirrors the relevant
/// [`relon_codegen_wasm::UnreachableKind`] variants but stays
/// independent of the codegen crate so the IR has no upward
/// dependency. Codegen maps each variant 1:1 to the matching
/// `UnreachableKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrapKind {
    /// `substring` / future `xs[i]` accessors tripped because the
    /// requested range walks past the receiver's end.
    IndexOutOfBounds,
    /// A reducer that requires at least one element (`list_int_max`)
    /// was called on an empty receiver.
    EmptyList,
    /// v3+ a-4: the Unicode-aware `upper` / `lower` stdlib body
    /// encountered a byte sequence that does not decode as valid
    /// UTF-8 (truncated continuation, lone continuation byte, etc.).
    /// In practice the host SDK validates input via
    /// `BufferBuilder::write_string` so this trap should only fire on
    /// modules with hand-crafted byte buffers; the variant ships so
    /// the diagnostic surface stays honest.
    InvalidUtf8,
}

/// v5-β-2 + v6-γ trace JIT hook: per-Op effect classification.
///
/// Each `Op` variant returns one of these so the v6-γ trace recorder
/// can decide whether to keep recording, ABORT the trace, or schedule
/// a side-effect replay in the deopt path. See
/// `docs/internal/v6-gamma-trace-jit-design.md` §3.3 for the
/// rationale.
///
/// The classification is conservative: when uncertain, surface the
/// stricter class (a `Pure` op miscategorised as
/// `UnrecoverableEffect` only loses optimization opportunity; the
/// reverse risks correctness).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectClass {
    /// No side effects; pure function of operands. Trace optimizer
    /// is free to reorder / eliminate / hoist across other Pure or
    /// ReadOnly ops.
    Pure,
    /// Reads side-effecting state (memory, host clock, capability
    /// vtable) but does not mutate it. Safe to reorder with other
    /// Pure / ReadOnly ops, but **not** with any Write op against
    /// the same backing store.
    ReadOnly,
    /// Mutates state, but the mutation is recoverable: the trace
    /// optimizer records sufficient deopt state to undo / replay the
    /// mutation if a guard later fires. Scratch arena cursor
    /// mutation, sub-record allocation, and bump-allocated string
    /// writes fall into this class.
    RecoverableWrite,
    /// Mutates externally-visible state in a way the trace optimizer
    /// cannot replay (host fn call into user-provided
    /// `register_pure_fn`, IO, etc). Encountering an Unrecoverable op
    /// during trace recording **must** ABORT the trace.
    UnrecoverableEffect,
}

impl Op {
    /// Return the [`EffectClass`] of this op. Conservative when in
    /// doubt — see the type's doc-comment for rationale.
    ///
    /// v5-β-2 establishes this classification ahead of v6-γ trace
    /// JIT work so the IR's surface is frozen against trace recorder
    /// expectations before the recorder lands.
    pub fn effect_class(&self) -> EffectClass {
        use EffectClass::*;
        match self {
            // Pure compute / loads of immediate constants: zero
            // observable effect.
            Op::ConstBool(_)
            | Op::ConstI32(_)
            | Op::ConstI64(_)
            | Op::ConstF64(_)
            | Op::ConstString { .. }
            | Op::ConstListInt { .. }
            | Op::ConstListFloat { .. }
            | Op::ConstListBool { .. }
            | Op::ConstListString { .. }
            | Op::LocalGet(_)
            | Op::LetGet { .. }
            | Op::Add(_)
            | Op::Sub(_)
            | Op::Mul(_)
            | Op::Div(_)
            | Op::Mod(_)
            | Op::BitAnd(_)
            | Op::Eq(_)
            | Op::Ne(_)
            | Op::Lt(_)
            | Op::Le(_)
            | Op::Gt(_)
            | Op::Ge(_)
            | Op::Select { .. }
            | Op::CaseFoldTableAddr { .. }
            | Op::CombiningMarkRangesAddr
            | Op::WhitespaceRangesAddr
            | Op::DecompTableAddr { .. }
            | Op::CccTableAddr
            | Op::CompositionTableAddr
            | Op::FullCaseFoldTableAddr { .. }
            | Op::CasedRangesAddr
            | Op::CaseIgnorableRangesAddr
            | Op::TurkishCaseFoldTableAddr { .. } => Pure,

            // Variable writes: scoped to the executing function;
            // recoverable via a pre-write snapshot if needed.
            Op::LetSet { .. } => RecoverableWrite,

            // Memory loads from input buffer / record bases — read
            // side-effecting state but do not mutate it. The bounds
            // check itself can trap, but read-only ops are still safe
            // to record so long as the trap path is part of the
            // recorded trace.
            Op::LoadField { .. }
            | Op::LoadStringPtr { .. }
            | Op::LoadListIntPtr { .. }
            | Op::LoadListFloatPtr { .. }
            | Op::LoadListBoolPtr { .. }
            | Op::LoadListStringPtr { .. }
            | Op::LoadListSchemaPtr { .. }
            | Op::LoadFieldAtAbsolute { .. }
            | Op::LoadSchemaPtr { .. }
            | Op::LoadI32AtAbsolute { .. }
            | Op::LoadI64AtAbsolute { .. }
            | Op::LoadI8UAtAbsolute { .. }
            | Op::LoadF64AtAbsolute { .. }
            | Op::ReadStringLen => ReadOnly,

            // Output buffer writes — RecoverableWrite because trace
            // optimizer can stash the prior cursor value (or the
            // initial `out_ptr`) and unwind.
            Op::StoreField { .. }
            | Op::StoreFieldAtRecord { .. }
            | Op::StoreI32AtAbsolute { .. }
            | Op::StoreI64AtAbsolute { .. }
            | Op::StoreI8AtAbsolute { .. }
            | Op::StoreF64AtAbsolute { .. }
            | Op::AllocScratch { .. }
            | Op::AllocScratchDyn
            | Op::AllocRootRecord { .. }
            | Op::AllocSubRecord { .. }
            | Op::PushRecordBase { .. }
            | Op::EmitTailRecordFromAbsoluteAddr { .. }
            | Op::MemcpyAtAbsolute
            | Op::MakeClosure { .. } => RecoverableWrite,

            // Control flow + intra-function ops — pure from the
            // recorder's perspective (no externally visible mutation;
            // child bodies are walked recursively when the recorder
            // expands the op).
            Op::Return
            | Op::If { .. }
            | Op::Block { .. }
            | Op::Loop { .. }
            | Op::Br { .. }
            | Op::BrIf { .. }
            | Op::BrTable { .. }
            | Op::CheckCap { .. } => Pure,

            // Trap is terminal — the trace ends at this point; the
            // recorder treats Trap as a guard fail rather than a
            // forward-progress op. Classifying as Pure keeps the
            // recorder honest without unnecessary aborts.
            Op::Trap { .. } => Pure,

            // Calls into stdlib bodies (`Op::Call`) and closures
            // (`Op::CallClosure`): the callee's effect class is the
            // composition of its body's ops. Conservative default is
            // UnrecoverableEffect because the recorder can't see
            // through the dispatch without inlining; stdlib bodies
            // that are known pure-or-recoverable will be inlined by
            // the recorder before this classification matters.
            Op::Call { .. } | Op::CallClosure { .. } => UnrecoverableEffect,

            // Native imports — opaque to the trace recorder by
            // construction. Always ABORTs the trace.
            Op::CallNative { .. } => UnrecoverableEffect,
        }
    }
}

#[cfg(test)]
mod effect_tests {
    use super::*;
    use ordered_float::OrderedFloat;

    #[test]
    fn arith_ops_are_pure() {
        assert_eq!(Op::Add(IrType::I64).effect_class(), EffectClass::Pure);
        assert_eq!(Op::Sub(IrType::I64).effect_class(), EffectClass::Pure);
        assert_eq!(Op::Mul(IrType::I64).effect_class(), EffectClass::Pure);
        assert_eq!(Op::Div(IrType::I64).effect_class(), EffectClass::Pure);
        assert_eq!(Op::Mod(IrType::I64).effect_class(), EffectClass::Pure);
        assert_eq!(Op::ConstI64(42).effect_class(), EffectClass::Pure);
        assert_eq!(
            Op::ConstF64(OrderedFloat(1.5)).effect_class(),
            EffectClass::Pure
        );
        assert_eq!(Op::Eq(IrType::I64).effect_class(), EffectClass::Pure);
        assert_eq!(Op::LocalGet(0).effect_class(), EffectClass::Pure);
    }

    #[test]
    fn load_ops_are_read_only() {
        assert_eq!(
            Op::LoadField {
                offset: 0,
                ty: IrType::I64
            }
            .effect_class(),
            EffectClass::ReadOnly
        );
        assert_eq!(Op::ReadStringLen.effect_class(), EffectClass::ReadOnly);
        assert_eq!(
            Op::LoadStringPtr { offset: 0 }.effect_class(),
            EffectClass::ReadOnly
        );
    }

    #[test]
    fn store_and_alloc_ops_are_recoverable_write() {
        assert_eq!(
            Op::StoreField {
                offset: 0,
                ty: IrType::I64
            }
            .effect_class(),
            EffectClass::RecoverableWrite
        );
        assert_eq!(
            Op::AllocScratch { size_bytes: 16 }.effect_class(),
            EffectClass::RecoverableWrite
        );
        assert_eq!(
            Op::LetSet {
                idx: 0,
                ty: IrType::I64
            }
            .effect_class(),
            EffectClass::RecoverableWrite
        );
    }

    #[test]
    fn call_native_is_unrecoverable() {
        assert_eq!(
            Op::CallNative {
                import_idx: 0,
                param_tys: vec![IrType::I64],
                ret_ty: IrType::I64,
                cap_bit: NO_CAPABILITY_BIT,
            }
            .effect_class(),
            EffectClass::UnrecoverableEffect
        );
    }

    #[test]
    fn checkcap_and_control_flow_are_pure() {
        assert_eq!(
            Op::CheckCap { cap_bit: 0 }.effect_class(),
            EffectClass::Pure
        );
        assert_eq!(
            Op::Block {
                result_ty: None,
                body: vec![]
            }
            .effect_class(),
            EffectClass::Pure
        );
        assert_eq!(Op::Br { label_depth: 0 }.effect_class(), EffectClass::Pure);
    }
}
