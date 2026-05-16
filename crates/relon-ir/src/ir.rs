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
#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    /// Lowered functions in declaration order.
    pub funcs: Vec<Func>,
    /// Index into `funcs` of the `#main` entry, when one was lowered.
    /// `None` for library modules (no `#main`); v1.beta lowering
    /// rejects this shape with `LoweringError::MissingMain` before
    /// returning, so the field is informational.
    pub entry_func_index: Option<usize>,
}

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
    /// Push an `i64` constant. Stack: `[] -> [i64]`.
    ConstI64(i64),
    /// Push an `f64` constant. Stack: `[] -> [f64]`.
    /// `OrderedFloat` so the enum can derive `PartialEq` and `Eq` —
    /// same trick the parser uses for `Expr::Float`.
    ConstF64(OrderedFloat<f64>),
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
}
