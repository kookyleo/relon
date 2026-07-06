//! `AnalyzedTree` -> [`Module`] lowering for Phase 2.b.
//!
//! Surface accepted (Phase 2.b widens v1.beta slightly):
//!
//! * `#main(<scalar> x [, ...]) -> <scalar>` on the entry module.
//!   `<scalar>` ∈ { `Int`, `Float`, `Bool` }. The pass packages every
//!   `#main` parameter into a 1-record schema `MainParams` (canonical
//!   form) and the return type into a 1-field schema `Ret` so codegen
//!   can apply the binary handshake uniformly.
//! * Root expression is the function body. Allowed shapes:
//!   - `Expr::Int(i)`           -> [`Op::ConstI64`]
//!   - `Expr::Float(f)`         -> [`Op::ConstF64`]
//!   - `Expr::Variable(path)`   -> [`Op::LoadField`] reading from the
//!     `in_buf` at the offset declared by the `MainParams` schema
//!   - `Expr::Binary(op, l, r)` with `op` in `{Add, Sub, Mul, Div, Mod}`
//!     -> recursive lower of `l`, `r`, then the matching [`Op`] tagged
//!     with the operands' [`IrType`]
//!
//! The wasm-level function signature emitted by codegen is
//! `(in_ptr i32, in_len i32, out_ptr i32, out_cap i32) -> i64`; the
//! IR records these wasm params on `Func::params`. User-declared
//! `#main` params are surfaced via `LoadField` operations, not as
//! `LocalGet` of wasm function locals.

use ordered_float::OrderedFloat;
use relon_analyzer::main_sig::MainSignature;
use relon_analyzer::schema::{SchemaDef, SchemaMethodInfo};
use relon_analyzer::tree::AnalyzedTree;
use relon_analyzer::workspace::WorkspaceTree;
use relon_eval_api::layout::{FieldKind, OffsetTable, SchemaLayout};
use relon_eval_api::schema_canonical::{
    EnumVariant as CanonicalEnumVariant, Field, Schema, TypeRepr,
};
use relon_parser::{
    is_builtin_type_name, CallArg, ClosureParam, Expr, FStringPart, Node, Operator, PatternBinding,
    RefBase, TokenKey, TokenRange, TypeNode,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::error::LoweringError;
use crate::intern::ConstInternTables;
use crate::ir::{
    ClosureCapture, Func, IrType, Module, NativeImport, Op, TaggedOp, TrapKind, NO_CAPABILITY_BIT,
};
use crate::stdlib::{
    builtin_stdlib, stdlib_closure_arg_signature, stdlib_function_count, stdlib_function_index,
    stdlib_method_index,
};

#[macro_use]
pub mod cap;
mod anon_dict;
mod closure;
mod peephole;
use anon_dict::*;
mod return_shape;
use return_shape::*;
mod native_import;
use native_import::*;
mod enum_match;
use enum_match::*;
mod call;
use call::*;
mod index;
use index::*;
mod stdlib_call;
use stdlib_call::*;
mod expr_ops;
use expr_ops::*;
mod canonical;
use canonical::*;
mod record_value;
use record_value::*;
mod dict_record;
use dict_record::*;
mod reference;
use reference::*;
mod methods;
use methods::*;
#[cfg(test)]
mod tests;

use closure::{lower_closure_as_value, lower_closure_as_value_with_expected_type};
use peephole::{
    classify_runtime_spread, emit_list_float_literal_materialize,
    emit_list_int_literal_materialize, emit_list_spread_runtime_materialize,
    emit_list_value_materialize, flatten_list_spread, list_has_computed_element, list_has_spread,
    list_is_float_shaped, match_bare_range, match_materializable_outer_map, probe_expr_ir_ty,
    try_lower_len_filter_range, try_lower_list_count, try_lower_list_filter, try_lower_list_len,
    try_lower_list_map, try_lower_list_pred, try_lower_list_reduce, try_lower_list_sum_range,
    try_lower_list_sum_value, try_lower_list_unique, try_lower_materialized_list_reduce,
    try_lower_nested_range_map_reduce, try_lower_range_chain_len, try_lower_range_chain_reduce,
    try_lower_range_value, try_lower_type_const,
};

/// Per-function lowering state shared across the recursive walk.
///
/// Phase 3.a introduces user-let bindings (`where { name: value }`)
/// and inline const literals (`true` / `"hello"` / `[1, 2, 3]`); each
/// of those needs a per-function counter the recursive walker hands
/// back to codegen. Phase 3.b extends the context with a record-local
/// counter (one per dict literal currently being constructed) and a
/// schema resolver so nested branded dict fields find their canonical
/// shape without re-walking the analyzer table.
#[derive(Debug)]
struct LowerCtx<'a> {
    /// `#main` parameter bindings (offset + IR type) used to resolve
    /// bare identifier references.
    params: &'a [LocalBinding],
    /// Stack of in-scope let bindings. Pushed when entering a
    /// `where { ... }` block and popped after the inner expression
    /// lowers — gives us lexical scoping for free.
    lets: Vec<LetBinding>,
    /// Next per-function let-local index. Stable across `where`
    /// blocks so a shadowed name still picks up a fresh wasm local.
    next_let_idx: u32,
    /// #151 — Module-wide intern + idx-allocation table shared across
    /// every [`LowerCtx`] in the same `Module` (entry body + each
    /// schema-method body + each emitted lambda). Two source-level
    /// `Op::ConstString { value }` with the same bytes intern to the
    /// same idx so the downstream const-pool walker stores one record
    /// instead of N. Per-list-variant counters also live here so the
    /// const-pool's `HashMap<idx, offset>` keys stay module-unique
    /// across the entry / method funcs that share it.
    const_intern: Rc<RefCell<ConstInternTables>>,
    /// Next per-function record-local index. Each
    /// [`Op::AllocRootRecord`] / [`Op::AllocSubRecord`] hands out a
    /// fresh local so nested dicts under construction don't clobber
    /// their parent's base offset.
    next_record_idx: u32,
    /// Output op stream. Appended to in postfix / stack order.
    out: Vec<TaggedOp>,
    /// Virtual operand stack tracking the IR type each pushed value
    /// has. Lets us validate arithmetic / store tags without a
    /// separate analysis pass.
    tstack: Vec<IrType>,
    /// Schema-name → analyzer-side `SchemaDef` resolver. Phase 3.b
    /// dict literal lowering consults it when a field's declared
    /// type-hint names a user-declared schema (the nested dict
    /// case).
    schema_resolver: SchemaResolver<'a>,
    /// Schema-method registry shared across the whole module. Phase
    /// 5 method-dispatch consults it when lowering `obj.method(...)`
    /// to a `Op::Call` whose `fn_index` targets a non-stdlib func.
    method_registry: SchemaMethodRegistry,
    /// `Some` when this context is lowering a schema method body —
    /// carries the `self` receiver's wasm-local index plus the
    /// schema's canonical shape. The walker uses both to resolve
    /// `self.field` (chained field loads off the absolute address)
    /// and `self.other_method()` (schema-method self-dispatch).
    self_binding: Option<SelfBinding>,
    /// Method parameters (non-`self`) for the current schema-method
    /// body. Lowered as wasm function locals — `LocalGet(wasm_idx)`
    /// pulls them onto the stack. Empty for entry bodies; populated
    /// only when `self_binding` is `Some`.
    method_params: Vec<MethodParam>,
    /// Variant known for an enum variable while lowering a concrete
    /// `match` arm. This enables flat payload access such as `msg.address`
    /// inside `msg match { Email: ... }` without adding pattern bindings yet.
    enum_variant_narrowing: HashMap<String, EnumVariantNarrowing>,
    /// When true, enum-like variant records are allocated in scratch instead
    /// of the entry output tail. Closure bodies use this because they receive
    /// `ArenaState` but do not have the entry-only `out_ptr` local.
    variant_records_in_scratch: bool,
    /// Phase 10-a: lambda functions emitted in this lowering pass.
    /// Each entry is a fully-lowered closure body with the implicit
    /// `captures_ptr: i32` as its first parameter; the closure-table
    /// emit step picks the entries up in declaration order.
    ///
    /// AOT-4 fix: this is a **module-wide shared** slot table (an
    /// `Rc<RefCell<...>>` cloned into every nested closure-body ctx) so
    /// the `fn_table_idx` an `Op::MakeClosure` bakes in is a GLOBAL
    /// closure-table slot — even for a lambda created *inside* another
    /// lambda's body (the W16 filter predicate built inside the
    /// recursive `sum_qs` helper). Pre-fix each nested ctx numbered its
    /// own lambdas from 0, so a predicate MakeClosure'd inside a helper
    /// got `fn_table_idx=0` and dispatched to the helper at runtime
    /// (wrong callee → trap / SIGSEGV). The slot is RESERVED (a `None`
    /// placeholder pushed) before the lambda body lowers so a nested
    /// lambda created during that body takes the next slot; the Func is
    /// filled in afterwards.
    lambda_table: Rc<RefCell<Vec<Option<Func>>>>,
    /// Phase F.2 (W7 anon-Dict-return): per-let-idx signature for
    /// closure-typed let-bindings. Populated when the W7 anon-Dict
    /// return path binds a closure-typed dict field as an internal
    /// let; consulted by [`lower_fn_call`] when a free-call's head
    /// resolves to a `Closure`-typed let so it can emit a
    /// [`Op::CallClosure { param_tys, ret_ty }`] with the matching
    /// signature.
    ///
    /// The signature is the **user-visible** surface (no implicit
    /// captures_ptr) — same convention as `Op::CallClosure`.
    closure_let_signatures: HashMap<u32, (Vec<IrType>, IrType)>,
    /// Wave B (Float rendering): per-let-idx mask of closure params
    /// whose `String` type came from the anon-Dict String-concat body
    /// inference (see [`plan_anon_dict_closure_sig`]) rather than an
    /// explicit annotation. Such a param is only ever used as a
    /// concat leaf inside the body, so
    /// [`try_lower_local_closure_call`] may render a scalar argument
    /// (Int / Bool / Float) to `String` *before* the call — byte-
    /// identical to the tree-walk oracle, which renders the value at
    /// the `+` inside the body via the same `Display`. Absent entries
    /// mean "no coercible params" (the where-binding closure path
    /// never infers String-from-concat).
    closure_concat_coercible: HashMap<u32, Vec<bool>>,
    /// #359 (W20 container perf): per-let-idx compile-time scalar
    /// constant value for where-bound scalar literals (`soft: 0.1`,
    /// `dt: 0.01`, `m0: 1.0`, ...). Recorded by [`lower_where`] when a
    /// binding's value is a bare `Expr::Int` / `Expr::Float` /
    /// `Expr::Bool` literal. [`lower_variable`] folds a bare reference to
    /// such a binding into the literal `Op::Const*`, and the
    /// capture-resolution path ([`closure::lower_closure_as_value`])
    /// inlines it into a closure body (e.g. `pair_force`) rather than
    /// capturing it through the arena captures struct. Folding `soft` /
    /// `dt` / the masses to compile-time constants lets LLVM `-O3` see
    /// the real inner-loop arithmetic (`dx*dx + 0.1`) instead of an
    /// opaque load, recovering the scalar half of the W20 gap
    /// (2.14x -> ~1.69x on s90). The inlined constant is the exact source
    /// literal, so every backend computes a bit-identical value.
    const_let_values: HashMap<u32, ScalarConst>,
    /// Module-wide `#native` import accumulator. Shared across the
    /// entry body, schema-method bodies, and lambda bodies so a native
    /// call from any of them interns into one [`Module::imports`]
    /// table. Empty `resolved` map (the common case, and every
    /// host-fn-free source) means [`lower_fn_call`] never resolves a
    /// native call and the free-call path keeps its prior behaviour.
    native_imports: Rc<RefCell<NativeImportBuilder>>,
}

/// A where-bound scalar literal recorded for closure-capture inlining.
/// See [`LowerCtx::const_let_values`]. The variants mirror the bare
/// literal `Op::Const*` shapes; lowering re-emits the matching const op
/// inside a closure body that references the binding.
#[derive(Debug, Clone, Copy)]
enum ScalarConst {
    I64(i64),
    /// Stored as the `f64` value; re-emitted via `OrderedFloat` exactly
    /// as the original `Expr::Float` literal lowers, so the inlined
    /// `Op::ConstF64` is bit-identical to the captured load.
    F64(f64),
    Bool(bool),
}

/// Information the lowering walker needs when handling `self`-prefixed
/// expressions inside a schema-method body.
#[derive(Debug, Clone)]
struct SelfBinding {
    /// Wasm-function-local index of the `self` slot. Phase 5 pins it
    /// to local `0` — the first param of a method's function
    /// signature — but the field stays explicit so future overhauls
    /// can shift the slot without touching the lowering walker.
    wasm_local_idx: u32,
    /// Canonical schema shape of the receiver. `self.field` resolves
    /// its offset and IR type from this; `self.other()` keys the
    /// schema-method registry off the schema's name.
    schema: Schema,
}

/// One non-`self` method parameter. Lowered as a wasm function local
/// referenced via [`Op::LocalGet`].
#[derive(Debug, Clone)]
struct MethodParam {
    /// Source-level parameter name.
    name: String,
    /// IR type of the parameter on the wasm operand stack. Schema-typed
    /// params occupy `I32` and carry `schema` below; scalar params
    /// match the declared canonical type.
    ty: IrType,
    /// Wasm-function-local slot for this parameter (declaration order).
    wasm_local_idx: u32,
    /// Canonical schema shape when the param is schema-typed (so
    /// chained field walks + method dispatch find the layout). `None`
    /// for scalar / pointer-record params.
    schema: Option<Schema>,
}

#[derive(Debug, Clone)]
struct DirectEnumPayload {
    field_name: String,
    ty: TypeRepr,
}

#[derive(Debug, Clone)]
struct EnumVariantNarrowing {
    enum_name: String,
    variant: CanonicalEnumVariant,
    direct_payload: Option<DirectEnumPayload>,
}

/// Schema-method dispatch table built once per `lower_workspace_*`
/// call. Phase 5 wires user-declared `with { ... }` methods into the
/// IR module's `funcs` list and records the wasm-level function index
/// each call site should jump to. The wasm-level index is the
/// **combined** index: `stdlib_count + ir_user_func_index`, so the
/// emitter can inject the `Op::Call`'s `fn_index` straight into a
/// wasm `call` instruction without further translation.
#[derive(Debug, Clone, Default)]
struct SchemaMethodRegistry {
    /// `(schema_name, method_name)` -> `(wasm fn_index, param IR
    /// types, return IR type)`. Single-map form so call sites resolve
    /// dispatch index + signature in one lookup. The same schema name
    /// keyed by both the original declaration site and any `#extend`
    /// contributions is fine — analyzer-level conflict detection
    /// happens upstream; the IR pass picks whichever lands first.
    methods: HashMap<(String, String), (u32, Vec<IrType>, IrType)>,
}

/// Name → `SchemaDef` lookup built once per `lower_workspace_*` call
/// from the analyzer's `tree.root_schemas` + `tree.schemas`. Cheap to
/// construct — only the schema declarations participate, not every
/// node in the source tree.
///
/// Phase 10-b: the resolver may aggregate schemas from multiple
/// reachable modules so a `#main(User u)` in the entry file can
/// resolve `User` when it lives in an imported file. The first tree
/// passed to `new_multi` wins on a name clash (entry first), matching
/// the analyzer's source-order import dedup; conflicting same-name
/// schemas across files are surfaced upstream by
/// `detect_cross_file_schema_conflicts` so the IR pass never silently
/// picks a "wrong" shape here.
#[derive(Debug, Clone)]
struct SchemaResolver<'a> {
    by_name: HashMap<&'a str, &'a SchemaDef>,
}

impl<'a> SchemaResolver<'a> {
    fn new(tree: &'a AnalyzedTree) -> Self {
        Self::new_multi(std::slice::from_ref(&tree))
    }

    /// Aggregate schema declarations from every `tree` in order. Used
    /// by [`lower_workspace`] so `#main(User u)` can resolve `User`
    /// when it is declared in an imported module — the entry tree
    /// alone has no `SchemaDef` for it. The first tree's declarations
    /// take precedence on collision, matching the analyzer's
    /// source-order import semantics; the IR pass relies on
    /// [`collect_cross_file_schema_conflicts`] to have raised
    /// `LoweringError::DuplicateSchemaAcrossFiles` already when two
    /// files disagree on a schema's shape.
    fn new_multi(trees: &[&'a AnalyzedTree]) -> Self {
        let mut by_name: HashMap<&'a str, &'a SchemaDef> = HashMap::new();
        for tree in trees {
            // Root-level `#schema X ...` directives are the standard
            // surface for top-level brand declarations; the schema body
            // lives in `tree.schemas` keyed by the body node id. We pick
            // the SchemaDef out of `tree.schemas` to get the analyzed
            // field shape.
            for decl in &tree.root_schemas {
                if let Some(def) = tree.schemas.get(&decl.schema_node.id) {
                    by_name.entry(decl.name.as_str()).or_insert(def);
                }
            }
            // Dict-field `#schema X { ... }` declarations also surface in
            // `tree.schemas`. Walk every entry that has a non-None name
            // and add it to the map (later declarations of the same name
            // are kept earliest — analyzer-level diagnostics already
            // catch duplicates).
            for def in tree.schemas.values() {
                if let Some(name) = &def.name {
                    by_name.entry(name.as_str()).or_insert(def);
                }
            }
        }
        Self { by_name }
    }

    fn resolve(&self, name: &str) -> Option<&'a SchemaDef> {
        self.by_name.get(name).copied()
    }
}

#[derive(Debug, Clone)]
struct LetBinding {
    name: String,
    idx: u32,
    ty: IrType,
    /// Schema name when the bound value is a schema instance pointer
    /// (an i32 absolute address tagged at the IR level). Carried so
    /// downstream `obj.method()` resolution can find the schema's
    /// method table without re-deriving the type. `None` for plain
    /// scalar / pointer-record let bindings.
    schema_brand: Option<String>,
    /// Optional surface type for let-locals whose IR slot (`I32`) is not
    /// enough to recover enum identity inside nested expressions.
    type_repr: Option<TypeRepr>,
}

impl<'a> LowerCtx<'a> {
    fn new(
        params: &'a [LocalBinding],
        schema_resolver: SchemaResolver<'a>,
        method_registry: SchemaMethodRegistry,
        const_intern: Rc<RefCell<ConstInternTables>>,
        native_imports: Rc<RefCell<NativeImportBuilder>>,
    ) -> Self {
        Self {
            params,
            lets: Vec::new(),
            next_let_idx: 0,
            const_intern,
            next_record_idx: 0,
            out: Vec::new(),
            tstack: Vec::new(),
            schema_resolver,
            method_registry,
            self_binding: None,
            method_params: Vec::new(),
            enum_variant_narrowing: HashMap::new(),
            variant_records_in_scratch: false,
            lambda_table: Rc::new(RefCell::new(Vec::new())),
            closure_let_signatures: HashMap::new(),
            closure_concat_coercible: HashMap::new(),
            const_let_values: HashMap::new(),
            native_imports,
        }
    }

    /// Variant used when lowering a schema-method body. The walker
    /// has no `#main` param index (`params` is an empty slice borrow);
    /// `self_binding` plus `method_params` carry the per-method
    /// surface instead.
    fn new_method(
        params: &'a [LocalBinding],
        schema_resolver: SchemaResolver<'a>,
        method_registry: SchemaMethodRegistry,
        self_binding: SelfBinding,
        method_params: Vec<MethodParam>,
        const_intern: Rc<RefCell<ConstInternTables>>,
        native_imports: Rc<RefCell<NativeImportBuilder>>,
    ) -> Self {
        Self {
            params,
            lets: Vec::new(),
            next_let_idx: 0,
            const_intern,
            next_record_idx: 0,
            out: Vec::new(),
            tstack: Vec::new(),
            schema_resolver,
            method_registry,
            self_binding: Some(self_binding),
            method_params,
            enum_variant_narrowing: HashMap::new(),
            variant_records_in_scratch: false,
            lambda_table: Rc::new(RefCell::new(Vec::new())),
            closure_let_signatures: HashMap::new(),
            closure_concat_coercible: HashMap::new(),
            const_let_values: HashMap::new(),
            native_imports,
        }
    }

    /// Clone the shared module-wide lambda slot table for a nested
    /// closure-body ctx so a lambda created inside that body reserves a
    /// GLOBAL `fn_table_idx`. Mirrors [`intern_handle`].
    fn lambda_table_handle(&self) -> Rc<RefCell<Vec<Option<Func>>>> {
        Rc::clone(&self.lambda_table)
    }

    /// Reserve the next global closure-table slot, returning its index.
    /// The slot is filled with [`None`] until the lambda Func is built;
    /// reserving up-front (before the body lowers) keeps a nested
    /// lambda's slot strictly after its parent's.
    fn reserve_lambda_slot(&self) -> u32 {
        let mut table = self.lambda_table.borrow_mut();
        let idx = table.len() as u32;
        table.push(None);
        idx
    }

    /// Fill a previously [`reserve_lambda_slot`]-reserved slot with its
    /// built Func.
    fn set_lambda_slot(&self, idx: u32, func: Func) {
        self.lambda_table.borrow_mut()[idx as usize] = Some(func);
    }

    /// Clone the shared intern handle for spawning a nested
    /// `LowerCtx` (lambda body / schema-method body) that must
    /// participate in the same module-wide idx space.
    fn intern_handle(&self) -> Rc<RefCell<ConstInternTables>> {
        Rc::clone(&self.const_intern)
    }

    /// Clone the shared module-wide native-import accumulator for a
    /// nested `LowerCtx` (lambda body) so a native call inside it
    /// interns into the same [`Module::imports`] table.
    fn native_imports_handle(&self) -> Rc<RefCell<NativeImportBuilder>> {
        Rc::clone(&self.native_imports)
    }

    /// Allocate a fresh per-function record-local index used by
    /// [`Op::AllocRootRecord`] / [`Op::AllocSubRecord`] /
    /// [`Op::StoreFieldAtRecord`] / [`Op::PushRecordBase`].
    fn alloc_record_local(&mut self) -> u32 {
        let idx = self.next_record_idx;
        self.next_record_idx += 1;
        idx
    }
}

/// Phase 10-c: detected element shape of a `[...]` literal. Used by
/// the lowering pass to pick between `ConstListInt` / `ConstListFloat`
/// / `ConstListBool` / `ConstListString` after sniffing the first
/// element. The shape gates further elements — a mixed literal
/// (`[1, 2.0]` outside the Int-promotes-to-Float path) surfaces as
/// `LoweringError::UnsupportedExpr`.
#[derive(Debug, Clone, Copy)]
enum ConstListKind {
    Int,
    Float,
    Bool,
    String,
}

/// Wasm-side handshake parameter index — `in_ptr` is local 0.
pub const WASM_LOCAL_IN_PTR: u32 = 0;
/// Wasm-side handshake parameter index — `in_len` is local 1.
pub const WASM_LOCAL_IN_LEN: u32 = 1;
/// Wasm-side handshake parameter index — `out_ptr` is local 2.
pub const WASM_LOCAL_OUT_PTR: u32 = 2;
/// Wasm-side handshake parameter index — `out_cap` is local 3.
pub const WASM_LOCAL_OUT_CAP: u32 = 3;
/// Phase 11 handshake parameter — capability grant bitmap, `caps_arg`
/// is local 4 (i64). Reserved as a run_main argument (rather than an
/// imported global) so the host SDK can build a single
/// `wasmtime::InstancePre` and reuse it across stores. The bitmap-fed
/// `check_cap` prologue is not yet emitted — the live wasm path gates
/// capabilities through the `__relon_check_cap` host import (today a
/// Z.3 follow-up stub); this slot is the seat reserved for it.
pub const WASM_LOCAL_CAPS_ARG: u32 = 4;

/// Canonical name used for the synthesised `#main` parameter schema.
/// Phase 2.b packages the parameter list into a single record under
/// this name so the canonical hash + layout pass treat it uniformly.
pub const MAIN_PARAMS_SCHEMA_NAME: &str = "MainParams";
/// Canonical name used for the synthesised `#main` return schema.
/// Phase 2.b wraps the scalar return type in a 1-field record named
/// `"value"` under this schema so codegen can write through the
/// generic `BufferBuilder` path.
pub const MAIN_RETURN_SCHEMA_NAME: &str = "Ret";
/// Field name for the synthesised return-value slot inside the
/// `Ret` schema. Kept as a constant so codegen and host-side test
/// fixtures agree on the spelling.
pub const RETURN_VALUE_FIELD_NAME: &str = "value";

/// Result of lowering an entry module: the IR plus the canonical
/// shapes of the `#main` parameter pack and return value.
#[derive(Debug, Clone, PartialEq)]
pub struct LoweredEntry {
    /// IR module ready to hand to the codegen pass.
    pub module: Module,
    /// Canonical schema describing the `in_buf` layout. Phase 2.b
    /// always synthesises a single record named [`MAIN_PARAMS_SCHEMA_NAME`].
    pub main_schema: Schema,
    /// Canonical schema describing the `out_buf` layout. Phase 2.b
    /// always synthesises a 1-field record named
    /// [`MAIN_RETURN_SCHEMA_NAME`] with a single `value` field.
    pub return_schema: Schema,
}

/// Lower the entry module of a workspace, inlining every reachable
/// module's `#schema` / `#extend` contributions so a `#main(User u)`
/// in the entry file resolves `User` even when it lives in an
/// imported file.
///
/// Phase 10-b: prior to this, `lower_workspace` was a thin wrapper
/// around the single-file `lower_workspace_single` and the IR pass
/// could not see cross-file schemas at all. The new implementation:
///
/// 1. Walks `ws.import_graph` BFS-from-entry to collect every
///    reachable module's analyzed tree (the same set the evaluator
///    side loads for runtime `#import`).
/// 2. Rejects the workspace when two modules declare the same
///    top-level schema name with structurally different bodies
///    (`LoweringError::DuplicateSchemaAcrossFiles`) — wasm-AOT
///    cannot pick non-deterministically between two `User`
///    definitions without breaking canonical-hash determinism.
/// 3. Rejects the workspace when more than one reachable module
///    carries a `#main` directive (`LoweringError::MultipleMainDirectives`).
/// 4. Builds a multi-tree `SchemaResolver` so the entry's
///    parameter / body lowering walks see every reachable schema
///    declaration.
/// 5. Delegates to the existing entry-body lowering against the
///    merged resolver. `schema_methods` on the entry tree is already
///    cross-file thanks to the analyzer's
///    `propagate_schema_methods_across_imports` pass — the IR side
///    just consumes it.
pub fn lower_workspace(
    ws: &WorkspaceTree,
    entry_module: &str,
) -> Result<LoweredEntry, LoweringError> {
    let entry_tree = ws.modules.get(entry_module).ok_or_else(|| {
        cap!(
            "lower_workspace.entry_module_not_found.1",
            LoweringError::EntryModuleNotFound {
                module: entry_module.to_string(),
            }
        )
    })?;
    let entry_root = ws.nodes.get(entry_module).ok_or_else(|| {
        cap!(
            "lower_workspace.entry_module_not_found.2",
            LoweringError::EntryModuleNotFound {
                module: entry_module.to_string(),
            }
        )
    })?;

    // Reachable modules, BFS from the entry. Entry first so it wins
    // on a schema-name collision (the conflict detection below catches
    // structurally-different duplicates; identical bodies fall through
    // silently, matching the analyzer's diamond-import dedup).
    let reachable_ids = reachable_modules(ws, entry_module);
    let mut reachable_trees: Vec<&AnalyzedTree> = Vec::with_capacity(reachable_ids.len());
    let mut reachable_pairs: Vec<(&str, &AnalyzedTree)> = Vec::with_capacity(reachable_ids.len());
    for id in &reachable_ids {
        if let Some(arc) = ws.modules.get(id) {
            reachable_trees.push(arc.as_ref());
            reachable_pairs.push((id.as_str(), arc.as_ref()));
        }
    }

    // (a) Surface cross-file `#schema` shape conflicts before the
    // resolver silently picks one — the entry-first ordering would
    // otherwise mask the conflict and produce a wasm module whose
    // canonical hash agrees with the entry but disagrees with the
    // imported file's expectation.
    detect_cross_file_schema_conflicts(&reachable_pairs)?;

    // (b) Only the entry file may carry `#main`. Imported libraries
    // accidentally tagged `#main(...)` would otherwise have their
    // signature silently dropped here.
    for (id, tree) in &reachable_pairs {
        if *id == entry_module {
            continue;
        }
        if tree.main_signature.is_some() {
            return Err(cap!(
                "lower_workspace.multiple_main_directives",
                LoweringError::MultipleMainDirectives {
                    entry_module: entry_module.to_string(),
                    other_module: (*id).to_string(),
                }
            ));
        }
    }

    // (c) Cross-file resolver. `new_multi` keeps the first-seen
    // SchemaDef per name so the entry's own declarations stay
    // authoritative — important for diagnostics that print the
    // entry-file source span.
    let resolver = SchemaResolver::new_multi(&reachable_trees);

    lower_entry_with_resolver(
        entry_tree.as_ref(),
        entry_root.as_ref(),
        entry_module,
        resolver,
    )
}

/// Single-file lowering convenience. Treats the supplied `(tree,
/// root)` pair as a one-module workspace with id `"main"`.
pub fn lower_workspace_single(
    tree: &AnalyzedTree,
    root: &Node,
) -> Result<LoweredEntry, LoweringError> {
    lower_workspace_single_with_module(tree, root, "main")
}

fn lower_workspace_single_with_module(
    tree: &AnalyzedTree,
    root: &Node,
    module_id: &str,
) -> Result<LoweredEntry, LoweringError> {
    let resolver = SchemaResolver::new(tree);
    lower_entry_with_resolver(tree, root, module_id, resolver)
}

/// BFS over `ws.import_graph` starting from `entry_module`. Returns
/// every module id reachable through `#import` edges (including the
/// entry), deduplicated, in BFS visit order. Modules whose canonical
/// id appears in `import_graph` edges but not in `ws.modules`
/// (failed-to-load slots) are skipped; the workspace pass already
/// surfaced their `ModuleNotFound` diagnostic.
fn reachable_modules(ws: &WorkspaceTree, entry_module: &str) -> Vec<String> {
    use std::collections::VecDeque;
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut out: Vec<String> = Vec::new();
    queue.push_back(entry_module.to_string());
    seen.insert(entry_module.to_string());
    while let Some(id) = queue.pop_front() {
        out.push(id.clone());
        if let Some(edges) = ws.import_graph.get(&id) {
            for edge in edges {
                // Only follow edges whose target was actually analyzed
                // — the entry's `import_graph` slot lists raw paths
                // when resolution fails, but those never land in
                // `ws.modules`.
                if !ws.modules.contains_key(edge) {
                    continue;
                }
                if seen.insert(edge.clone()) {
                    queue.push_back(edge.clone());
                }
            }
        }
    }
    out
}

/// Compare each pair of reachable modules' top-level schemas; emit
/// `LoweringError::DuplicateSchemaAcrossFiles` when the same name
/// names structurally-different bodies. Identical bodies (diamond
/// imports re-exporting the same definition) pass through.
fn detect_cross_file_schema_conflicts(
    reachable: &[(&str, &AnalyzedTree)],
) -> Result<(), LoweringError> {
    // First sighting wins: record `(module_id, canonical_schema)` so
    // the second sighting can compare. Schemas the IR can't even
    // canonicalize (variant / unsized) are skipped — the entry body's
    // own lowering walk will reject them with a precise error if it
    // ends up reaching for one. Anonymous (None-name) schemas live in
    // `tree.schemas` too but cannot collide cross-file.
    let mut first_seen: HashMap<String, (String, Schema)> = HashMap::new();
    for (id, tree) in reachable {
        // Build a per-module resolver so canonicalization can chase
        // inline references inside the schema's own body. The
        // cross-file resolver isn't valid yet — we'd be using it to
        // detect the very conflict it would silently paper over.
        let local_resolver = SchemaResolver::new(tree);
        for decl in &tree.root_schemas {
            let Some(def) = tree.schemas.get(&decl.schema_node.id) else {
                continue;
            };
            let mut stack: Vec<&str> = Vec::new();
            let Ok(canonical) =
                canonical_schema_from_def(def, &local_resolver, &mut stack, def.range)
            else {
                continue;
            };
            let name = decl.name.clone();
            if let Some((other_id, other_schema)) = first_seen.get(&name) {
                if schema_hashes_differ(other_schema, &canonical) {
                    return Err(cap!(
                        "detect_cross_file_schema_conflicts.duplicate_schema_across_files",
                        LoweringError::DuplicateSchemaAcrossFiles {
                            name,
                            first_module: other_id.clone(),
                            second_module: (*id).to_string(),
                        }
                    ));
                }
            } else {
                first_seen.insert(name, ((*id).to_string(), canonical));
            }
        }
    }
    Ok(())
}

/// Byte-compare two canonical schemas. We piggy-back on the
/// canonical-hash helper rather than implementing a structural
/// equality walk — the canonical form already collapses every
/// representation difference that the wasm-AOT pipeline cares about.
fn schema_hashes_differ(a: &Schema, b: &Schema) -> bool {
    use relon_eval_api::schema_canonical::schema_hash;
    schema_hash(a) != schema_hash(b)
}

fn lower_entry_with_resolver<'a>(
    tree: &'a AnalyzedTree,
    root: &Node,
    module_id: &str,
    resolver: SchemaResolver<'a>,
) -> Result<LoweredEntry, LoweringError> {
    let sig = tree.main_signature.as_ref().ok_or_else(|| {
        cap!(
            "lower_entry_with_resolver.missing_main",
            LoweringError::MissingMain {
                module: module_id.to_string(),
            }
        )
    })?;

    // Phase 10-a: reject closure-typed `#main` params + return type
    // up front. Wasm-side closure values are scratch-heap pointers
    // whose lifetime ends at `run_main` return — carrying one
    // through the binary handshake would dangle. Detected here so the
    // diagnostic message points at the directive declaration rather
    // than at a downstream schema-build failure.
    for p in &sig.params {
        if type_node_names_closure(&p.type_node) {
            return Err(cap!(
                "lower_entry_with_resolver.closure_across_boundary.1",
                LoweringError::ClosureAcrossBoundary {
                    context: format!("`#main` parameter `{}`", p.name),
                    range: p.type_node.range,
                }
            ));
        }
    }
    if let Some(rt) = sig.return_type.as_ref() {
        if type_node_names_closure(rt) {
            return Err(cap!(
                "lower_entry_with_resolver.closure_across_boundary.2",
                LoweringError::ClosureAcrossBoundary {
                    context: "`#main` return type".to_string(),
                    range: rt.range,
                }
            ));
        }
    }

    // Detect whether the return type names a user-declared schema.
    // When it does, the body must evaluate to a (possibly defaulted)
    // dict literal whose canonical shape comes from the schema; the
    // synthesised `Ret` schema in that case is structurally
    // equivalent to a 1-field record whose `value` is the user
    // schema, but the wasm-level layout pads the *user schema* into
    // the root return area directly (no extra pointer slot).
    //
    // Phase 10-b: `resolver` is supplied by the caller so the
    // `lower_workspace` cross-file aggregate is consulted here; for
    // single-file builds the helper still constructs a one-tree
    // resolver before delegating.
    let resolved_user_return_schema =
        resolve_return_user_schema(sig.return_type.as_ref(), &resolver)?;

    // A `#main(...) -> Tuple<...>` return uses an anonymous
    // positional-record schema. It reuses the existing record return ABI;
    // the host only changes the final projection by decoding the record as
    // a JSON array because `schema.is_tuple` is set.
    let tuple_return_schema: Option<Schema> = match resolved_user_return_schema.as_ref() {
        Some(schema) if schema.is_tuple => Some(schema.clone()),
        _ => match sig.return_type.as_ref() {
            Some(rt) => match return_tuple_canonical(rt, &resolver) {
                Some(res) => Some(res?),
                None => None,
            },
            None => None,
        },
    };
    let user_return_schema = resolved_user_return_schema.filter(|schema| !schema.is_tuple);

    // Phase F.2 (W7 anon-Dict-return): `#main(...) -> Dict { ... }`
    // synthesises an anonymous return schema by per-field inference
    // over the dict literal. Closure-typed fields are lifted to
    // internal let-bindings (they don't appear in the host-visible
    // schema — `SchemaLayout` rejects them by Phase B's guard), so
    // the synthesised schema only carries the scalar fields that
    // survive the boundary.
    // Wave R11: desugar field decorators in the anon-Dict-return body
    // (`@deco(args) k: v` → `deco(v, args)`) so both the per-field plan
    // and the body lowering see the decorator-call in place of the raw
    // value. `None` ⇒ no field carried a decorator, so the byte-exact
    // original `root` is used unchanged. Only the anon-Dict path is
    // rewritten here; a branded `-> Schema` return rejects field
    // decorators in `lower_dict_field_value` and returns an explicit error.
    let desugared_root: Option<Node> =
        if user_return_schema.is_none() && tuple_return_schema.is_none() {
            desugar_anon_dict_decorators(root)?
        } else {
            None
        };
    let anon_dict_root: &Node = desugared_root.as_ref().unwrap_or(root);
    let anon_dict_plan = if user_return_schema.is_none() && tuple_return_schema.is_none() {
        anon_dict_return_plan(sig, anon_dict_root, &resolver)?
    } else {
        None
    };

    // Build the canonical-form schemas for in_buf and out_buf, then
    // compute the offset table for the param schema so each
    // `Variable(x)` reference can be lowered to a typed LoadField.
    let main_schema = build_main_params_schema(sig, &resolver)?;
    let return_schema = if let Some(ref user_schema) = user_return_schema {
        // The dict-return path lays the user schema directly into the
        // fixed area — the host reads it back with the same
        // `BufferReader::new(...)` it would use for a hand-built dict
        // input. No `value` wrapping.
        user_schema.clone()
    } else if let Some(ref tuple_schema) = tuple_return_schema {
        // Tuple returns lay the positional record directly into the fixed
        // area, with no `value` wrapper. The host decodes that record as a
        // JSON array instead of a JSON object.
        tuple_schema.clone()
    } else if let Some(ref plan) = anon_dict_plan {
        plan.schema.clone()
    } else {
        build_main_return_schema(sig, &resolver)?
    };
    let main_layout = SchemaLayout::offsets_for(&main_schema)?;
    let return_layout = SchemaLayout::offsets_for(&return_schema)?;

    // Bind each parameter name to its (offset, IR type) so the body
    // walk can lower bare-identifier references to a typed LoadField
    // without a second pass over the layout pass.
    let locals = build_local_index(sig, &main_schema, &main_layout)?;

    // #151 — One shared `ConstInternTables` per `Module`. Threaded
    // through every `LowerCtx` (method funcs lowered next, the entry
    // body, plus any lambda body spawned inside either) so all
    // `Op::ConstString` / `Op::ConstList*` records share one module-
    // wide idx space. Same-bytes ConstString literals across funcs
    // collapse to one const-pool record; per-list-variant counters
    // stay collision-free across funcs that previously each restarted
    // at idx 0.
    let const_intern = ConstInternTables::shared();

    // Module-wide `#native` import accumulator. Built from the
    // analyzer's host-fn metadata (empty for host-fn-free sources) and
    // shared across the method bodies + entry body + lambdas so every
    // native call interns into one [`Module::imports`] table.
    let native_imports = Rc::new(RefCell::new(NativeImportBuilder::from_tree(tree)));

    // Phase 5: enumerate every user-declared schema method, assign
    // IR-side indices (and through them combined wasm-level
    // function indices), then lower each method body into a `Func`.
    // The entry body is appended last so it can resolve
    // `obj.method()` calls against the populated registry.
    let (method_funcs, method_registry) = lower_schema_methods(
        tree,
        &resolver,
        Rc::clone(&const_intern),
        Rc::clone(&native_imports),
    )?;
    let entry_ir_idx = method_funcs.len();

    // Walk the body into a single op stream + virtual stack via the
    // per-function lowering context. Phase 3.a's let-bindings + const
    // literals piggy-back on `LowerCtx` for their counters.
    let mut ctx = LowerCtx::new(
        &locals,
        resolver,
        method_registry,
        const_intern,
        Rc::clone(&native_imports),
    );

    if let Some(ref user_schema) = user_return_schema {
        // Branded dict-return path: emit `AllocRootRecord` + the
        // per-field stores into the root record, then `Return`.
        // Top-level dict expression must be a `Expr::Dict(...)` (the
        // brand is contributed by the return type).
        let dict_pairs = match &*root.expr {
            Expr::Dict(pairs) => pairs.as_slice(),
            _ => {
                return Err(cap!(
                    "lower_entry_with_resolver.unsupported_expr.1",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "Body-of-branded-#main must be a dict literal, got `{}`",
                            root.expr.kind()
                        ),
                        range: root.range,
                    }
                ));
            }
        };
        let record_local = ctx.alloc_record_local();
        ctx.out.push(TaggedOp {
            op: Op::AllocRootRecord {
                record_local_idx: record_local,
            },
            range: root.range,
        });
        lower_dict_into_record(
            user_schema,
            &return_layout,
            dict_pairs,
            root.range,
            record_local,
            &mut ctx,
        )?;
    } else if let Some(ref tuple_schema) = tuple_return_schema {
        // Tuple-return path. The body must be a tuple literal
        // (`Expr::Tuple`) whose arity matches the declared `Tuple<...>`.
        // Allocate the root record, then lower + store each element into
        // its positional slot per the element's canonical type — the same
        // `AllocRootRecord` + `StoreFieldAtRecord` shape the branded-dict
        // return uses, so the backends and verifier are reused unchanged.
        let elements = match &*root.expr {
            Expr::Tuple(elems) => elems.as_slice(),
            _ => {
                return Err(cap!(
                    "lower_tuple_return.unsupported_expr",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "Body-of-tuple-#main must be a tuple literal `(...)`, got `{}`",
                            root.expr.kind()
                        ),
                        range: root.range,
                    }
                ));
            }
        };
        if elements.len() != tuple_schema.fields.len() {
            return Err(cap!(
                "lower_tuple_return.arity_mismatch",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "tuple body has {} elements but `Tuple<...>` return declares {}",
                        elements.len(),
                        tuple_schema.fields.len()
                    ),
                    range: root.range,
                }
            ));
        }
        let record_local = ctx.alloc_record_local();
        ctx.out.push(TaggedOp {
            op: Op::AllocRootRecord {
                record_local_idx: record_local,
            },
            range: root.range,
        });
        lower_tuple_into_record(
            tuple_schema,
            &return_layout,
            elements,
            record_local,
            &mut ctx,
        )?;
    } else if let Some(plan) = anon_dict_plan {
        // Phase F.2 (W7): anon-Dict-return path. Walk the dict
        // literal in declaration order — closure fields become
        // internal let-bindings (with their signatures memoised in
        // `closure_let_signatures` so a recursive self-call inside
        // the closure body resolves through `Op::CallClosure`); the
        // surviving scalar fields are stored into the root record.
        let dict_pairs = match &*anon_dict_root.expr {
            Expr::Dict(pairs) => pairs.as_slice(),
            _ => {
                return Err(cap!(
                    "lower_entry_with_resolver.unsupported_expr.2",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "Body-of-anon-Dict-#main must be a dict literal, got `{}`",
                            anon_dict_root.expr.kind()
                        ),
                        range: anon_dict_root.range,
                    }
                ));
            }
        };
        let record_local = ctx.alloc_record_local();
        ctx.out.push(TaggedOp {
            op: Op::AllocRootRecord {
                record_local_idx: record_local,
            },
            range: root.range,
        });
        lower_anon_dict_body(&plan, &return_layout, dict_pairs, record_local, &mut ctx)?;
    } else {
        // Scalar-return path: existing v1 shape.
        let ret_ir_ty = type_repr_to_ir_type(&return_schema.fields[0].ty)?;
        // Pointer-array list returns (`List<String>` / `List<Schema>` /
        // `List<List<_>>`) are only marshalled correctly when the value
        // is a const-pool `ConstListString` block (a String-literal
        // list). Any other source — a `#main` param identity-return, a
        // field load, a call — produces a non-contiguous, whole-input-
        // buffer-relative block the rigid-delta return copy cannot
        // relocate; it would segfault / return corrupt data. Reject it
        // loudly here so the silent-miscompile path is unreachable.
        //
        // In-place region-walk return ABI (S1/S2 cranelift+llvm): a
        // `List<List<scalar>>` value, (S3) a `List<String>` value, or
        // (S4) a `List<Schema>` value — each sourced **directly from a
        // `#main` parameter identity** (`xss` / `ss` / `items`) — is
        // *not* copied at all. The lowering emits the
        // trailing `StoreField { ty, inplace: true }`, and both AOT
        // backends lower that store to the in-place return sentinel
        // (`-(root_abs + 1)`): they report the arena-absolute offset of
        // the root list header (which the `Load*Ptr` param load already
        // produced) to the host instead of relocating the block. The host
        // selects the region the root lands in, runs the bounds verifier
        // over the whole reachable graph, and only then decodes in place.
        // Because the value is self-contained in the input region (the
        // single-region invariant: no const-pool / element-construction
        // producer for these param-sourced shapes), the walk never crosses
        // a region.
        //
        // A parameter-**field** path (`o.field`) is intentionally NOT an
        // identity walk: a `List<List>` / `List<String>` reached through a
        // schema field is re-encoded by the field-load path into a
        // different inner form (truncated i32 row handles / re-laid
        // records) the in-place reader would decode wrong, so it stays a
        // loud cap. Const-pool `List<String>` literals keep the existing
        // copy path (`inplace: false`).
        // For `List<Schema>` (S4) the in-place sub-record reader only
        // decodes scalar / String / List<scalar> / List<String> fields.
        // A sub-record carrying a *deeper* pointer-array element field
        // (`List<Schema>` / `List<List<…>>` inside the element) is out of
        // S4 scope: leaving it `inplace` would emit the sentinel but the
        // host decode would error at runtime. Gate it here so it stays a
        // loud cap at lowering (S5 territory) instead.
        // F4: a parameter **field** walk (`o.tags` / `o.items` / `o.grid`,
        // where `o` is a schema-typed `#main` param and the field is a
        // pointer-array list) is now an in-place region-walk return too.
        // Post-F1 the input marshaller bakes `in_ptr` into *every* pointer
        // slot (`finish_arena_absolute` relocates recursively), so the
        // field-load (`LoadFieldAtAbsolute`) pushes the field list root's
        // arena-absolute offset directly — exactly the value the single-root
        // sentinel + multi-region verifier + reader consume. No re-encode
        // happens on the field-load path, so the historical S3/S4 rebase
        // cap is resolved by the F1 flip (proven byte-equal to tree-walk).
        let is_inplace_param_walk = (pointer_array_param_identity_walk(&root.expr)
            || pointer_array_param_field_walk(&root.expr, &main_schema).is_some())
            && match ret_ir_ty {
                IrType::ListList | IrType::ListString => true,
                IrType::ListSchema => {
                    list_schema_subrecord_in_s4_scope(&return_schema.fields[0].ty)
                }
                _ => false,
            };
        // Wave R3c: a String-result list HOF (`map` / `filter`) building a
        // self-contained scratch `List<String>` pointer-array also returns
        // in place (same arena-absolute-slot invariant as a param walk —
        // see [`string_result_list_hof_call`]). The numeric-result list
        // HOFs (`List<Int>` / `List<Float>`) are inline-fixed, not
        // pointer-array, so they keep the rigid-copy path and never reach
        // this branch.
        let is_inplace_string_hof =
            ret_ir_ty == IrType::ListString && string_result_list_hof_call(&root.expr);
        let is_inplace_param_walk = is_inplace_param_walk || is_inplace_string_hof;
        // Wave R15: `s.split(sep)` builds a self-contained scratch
        // `List<String>` pointer-array (per-segment String records, each
        // independently arena-allocated — same single-arena invariant as the
        // R3c String-result HOF results), so it returns in place through the
        // same `inplace_return` decoder rather than the rigid-block copy.
        let is_inplace_split = ret_ir_ty == IrType::ListString && string_split_call(&root.expr);
        let is_inplace_constructed_variant_list = ret_ir_ty == IrType::ListList
            && variant_record_list_inplace_expr_for_type(&return_schema.fields[0].ty, &root.expr);
        let is_inplace_param_walk =
            is_inplace_param_walk || is_inplace_split || is_inplace_constructed_variant_list;
        if pointer_array_list_ir_type(ret_ir_ty)
            && !pointer_array_list_source_is_const_pool(&root.expr)
            && !is_inplace_param_walk
        {
            return Err(cap!(
                "lower_entry_with_resolver.unsupported_type_in_main",
                LoweringError::UnsupportedTypeInMain {
                    type_name: format!(
                        "{:?} return sourced from `{}` — pointer-array list returns are only \
                     marshalled from in-source list literals, not parameters / loads / calls",
                        return_schema.fields[0].ty,
                        root.expr.kind()
                    ),
                    range: sig.range,
                }
            ));
        }
        lower_value_as_type(&return_schema.fields[0].ty, root, &mut ctx)?;

        // Trailing StoreField for the single root return value. Pops
        // the top stack entry — codegen will translate this to
        // `local.get $out_ptr; <value>; <store>.offset=N`.
        let ret_offset = return_layout
            .fields
            .first()
            .map(|f| f.offset as u32)
            .unwrap_or(0);
        ctx.out.push(TaggedOp {
            op: Op::StoreField {
                offset: ret_offset,
                ty: ret_ir_ty,
                inplace: is_inplace_param_walk,
            },
            range: sig.range,
        });
        ctx.tstack.pop();
    }

    // `Op::Return` keeps its v1.beta meaning: end of function. The
    // codegen pass synthesises the actual wasm `return` (it pushes
    // `bytes_written` and emits the implicit `end`).
    ctx.out.push(TaggedOp {
        op: Op::Return,
        range: sig.range,
    });
    // Hoist the lambda funcs emitted by the entry body's lowering pass
    // from the module-wide shared slot table. Each slot was reserved in
    // global `fn_table_idx` order (a parent lambda before any lambda
    // created inside its body), so the table is already in
    // closure-table order. Every slot must be filled by now — a `None`
    // would mean a reserved slot whose Func was never built (a lowering
    // bug), so surface it loudly rather than emit a broken module.
    let entry_lambda_funcs: Vec<Func> = ctx
        .lambda_table
        .borrow_mut()
        .drain(..)
        .enumerate()
        .map(|(i, slot)| {
            slot.ok_or_else(|| {
                cap!(
                    "lower_entry_with_resolver.unsupported_expr.3",
                    LoweringError::UnsupportedExpr {
                        kind: format!("closure-table slot {i} reserved but never filled"),
                        range: sig.range,
                    }
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let body = ctx.out;

    let func = Func {
        name: "run_main".to_string(),
        // Wasm-level binary handshake signature: four i32 slots
        // (in_ptr, in_len, out_ptr, out_cap) plus the Phase-11
        // capability bitmap (`caps_arg: i64`). User-declared params
        // reach the body through `LoadField`. `caps_arg` is the
        // reserved seat for the wasm capability handshake (see
        // `WASM_LOCAL_CAPS_ARG`); the bitmap-fed prologue is not yet
        // emitted.
        params: vec![
            IrType::I32,
            IrType::I32,
            IrType::I32,
            IrType::I32,
            IrType::I64,
        ],
        // `bytes_written` returned as i32. Phase 2.b never returns
        // anything else from the wasm function itself; user-side
        // return values live in `out_buf`.
        ret: IrType::I32,
        body,
        range: sig.range,
    };

    let mut funcs = method_funcs;
    funcs.push(func);

    // Phase 10-a: stitch the closure table together. The lowering
    // pass attached each emitted lambda to the body-walking context;
    // we lift them out here and translate the per-lambda local idx
    // (relative to the lambdas emitted **inside** the entry body)
    // into the combined IR-func-index space. Lambdas appear after the
    // entry function in the final `funcs` vec, so the closure table
    // entries point at `funcs.len() - lambda_count + i`.
    let lambda_count = entry_lambda_funcs.len();
    let entry_funcs_len = funcs.len(); // method_funcs + entry, no lambdas yet
    funcs.extend(entry_lambda_funcs);
    let closure_table: Vec<u32> = (0..lambda_count as u32)
        .map(|i| (entry_funcs_len as u32) + i)
        .collect();

    // Native imports interned by `lower_fn_call`'s native path across
    // every body sharing `native_imports`. Empty when the source never
    // calls a host-registered fn. Extracted before the struct literal
    // so the `Ref` borrow doesn't outlive the temporary.
    let imports = native_imports.borrow().imports.clone();

    Ok(LoweredEntry {
        module: Module {
            imports,
            funcs,
            entry_func_index: Some(entry_ir_idx),
            closure_table,
        },
        main_schema,
        return_schema,
    })
}

/// Synthesise the [`MAIN_PARAMS_SCHEMA_NAME`] canonical schema from
/// the `#main` parameter list. Phase 5 widens the surface so a
/// user-schema-typed param (`#main(User u) -> ...`) builds a
/// pointer-indirect field whose payload is the canonical shape of
/// the named schema; scalar / `String` / `List<Int>` params keep
/// their existing canonical form.
fn build_main_params_schema(
    sig: &MainSignature,
    resolver: &SchemaResolver<'_>,
) -> Result<Schema, LoweringError> {
    let mut fields = Vec::with_capacity(sig.params.len());
    for p in &sig.params {
        let ty = type_node_to_canonical_with_schemas(&p.type_node, resolver).ok_or_else(|| {
            cap!(
                "build_main_params_schema.unsupported_type_in_main",
                LoweringError::UnsupportedTypeInMain {
                    type_name: type_head_for_display(&p.type_node),
                    range: p.type_node.range,
                }
            )
        })?;
        fields.push(Field {
            name: p.name.clone(),
            ty,
            default: None,
        });
    }
    Ok(Schema {
        name: MAIN_PARAMS_SCHEMA_NAME.to_string(),
        generics: vec![],
        fields,
        is_tuple: false,
    })
}

/// Convert a parsed type into the canonical boundary representation, while
/// allowing references to user-declared schemas. This is used for `#main`
/// parameters and return shapes that must cross the host boundary.
fn type_node_to_canonical_with_schemas(
    t: &TypeNode,
    resolver: &SchemaResolver<'_>,
) -> Option<TypeRepr> {
    if t.path.len() != 1 || t.variant_fields.is_some() {
        return None;
    }
    let head = t.path[0].as_str();
    if is_removed_unit_null_type_name(head) {
        return None;
    }

    let base = match (head, t.generics.as_slice()) {
        ("Int", []) => TypeRepr::Int,
        ("Float", []) => TypeRepr::Float,
        ("Bool", []) => TypeRepr::Bool,
        ("String", []) => TypeRepr::String,
        ("List", [elem]) => TypeRepr::List {
            element: Box::new(type_node_to_canonical_with_schemas(elem, resolver)?),
        },
        ("Option", [inner]) => TypeRepr::Option {
            inner: Box::new(type_node_to_canonical_with_schemas(inner, resolver)?),
        },
        ("Result", [ok, err]) => TypeRepr::Result {
            ok: Box::new(type_node_to_canonical_with_schemas(ok, resolver)?),
            err: Box::new(type_node_to_canonical_with_schemas(err, resolver)?),
        },
        ("Tuple", _) => TypeRepr::Schema {
            schema: Box::new(tuple_type_node_to_schema(t, Some(resolver))?),
        },
        _ => {
            if matches!(
                head,
                "Int" | "Float" | "Bool" | "String" | "List" | "Option" | "Result" | "Tuple"
            ) {
                return None;
            }
            let def = resolver.resolve(head)?;
            let subst = generic_subst_for_def(def, t)?;
            let mut stack: Vec<&str> = Vec::new();
            if !def.variants.is_empty() {
                canonical_enum_from_def_with_subst(def, resolver, &mut stack, t.range, &subst)
                    .ok()?
            } else {
                TypeRepr::Schema {
                    schema: Box::new(
                        canonical_schema_from_def_with_subst(
                            def, resolver, &mut stack, t.range, &subst,
                        )
                        .ok()?,
                    ),
                }
            }
        }
    };

    Some(maybe_optional(t, base))
}

/// Synthesise the [`MAIN_RETURN_SCHEMA_NAME`] canonical schema with a
/// single `value` field carrying the declared return type.
///
/// Phase 3.a widens the return surface to `String` / `List<Int>`
/// alongside the v1 scalars. The codegen pass copies the tail-area
/// record bytes into `out_buf` at a `$tail_cursor` past the fixed
/// area; the fixed-area pointer slot stores a buffer-relative
/// offset so the host's `BufferReader` can decode it uniformly.
///
/// Phase F.2 (W7 closure-as-value boundary, design doc
/// `docs/internal/w7-closure-as-value-design.md`): an unbound `-> Dict`
/// head (no named schema, no generics) reaches this helper today
/// because no canonical schema exists. The diagnostic stays
/// `UnsupportedTypeInMain { type_name: "Dict" }` so the two boundary
/// tests (`run_main_w7_recursive_closure_dict_field` /
/// `w7_production_source_pins_unsupported_dict_return`) continue to
/// pin the failure shape — but the error string now points users at
/// the Phase C lifting work so they don't grep blindly through the
/// lowering pass when they meet it.
fn build_main_return_schema(
    sig: &MainSignature,
    resolver: &SchemaResolver<'_>,
) -> Result<Schema, LoweringError> {
    let rt = sig.return_type.as_ref().ok_or_else(|| {
        cap!(
            "build_main_return_schema.unsupported_type_in_main.1",
            LoweringError::UnsupportedTypeInMain {
                type_name: "<missing>".to_string(),
                range: sig.range,
            }
        )
    })?;
    // S4: a `-> List<Schema>` return resolves through the schema-aware
    // canonicaliser (the narrow `type_node_to_canonical` has no resolver
    // and so can't name a user schema). F5: `List<List<String>>` /
    // `List<List<Schema>>` (and deeper nested lists) resolve through the
    // schema-aware nested-list canonicaliser; the layout pass is the final
    // arbiter of materialisable inner element types.
    let ty = type_node_to_canonical_with_schemas(rt, resolver)
        .or_else(|| type_node_to_canonical(rt))
        .or_else(|| return_nested_list_canonical(rt, Some(resolver)))
        .or_else(|| return_list_schema_canonical(rt, resolver))
        .ok_or_else(|| {
            cap!(
                "build_main_return_schema.unsupported_type_in_main.2",
                LoweringError::UnsupportedTypeInMain {
                    type_name: type_head_for_display(rt),
                    range: rt.range,
                }
            )
        })?;
    Ok(Schema {
        name: MAIN_RETURN_SCHEMA_NAME.to_string(),
        generics: vec![],
        is_tuple: false,
        fields: vec![Field {
            name: RETURN_VALUE_FIELD_NAME.to_string(),
            ty,
            default: None,
        }],
    })
}

/// Name reserved for the anonymous positional-record schema that
/// represents a compiled `Tuple<...>` boundary. Distinct from `MainParams` / `Ret` so
/// the backends and the host decode never confuse a tuple record with a
/// scalar-wrapper or branded return.
pub const TUPLE_RETURN_SCHEMA_NAME: &str = "Tuple";

/// One entry in the `#main` parameter index: `(name, ir_type,
/// field_offset)`. The body walk uses this to lower `Variable(x)`
/// into a typed [`Op::LoadField`].
#[derive(Debug, Clone)]
struct LocalBinding {
    name: String,
    ty: IrType,
    offset: u32,
    /// Schema name when the param is a schema-typed instance carried
    /// as a pointer-indirect slot in the `MainParams` fixed area.
    /// `None` for scalar / String / List<Int> params. Used so a
    /// `Variable(x)` reference can lift the pointer to an absolute
    /// address and so `x.method()` resolves through the schema's
    /// method table.
    schema_brand: Option<String>,
    /// Canonical type of this parameter. Kept alongside the IR type so
    /// runtime enum match lowering can read the variant table after the
    /// boundary type has collapsed to an `I32` pointer.
    type_repr: TypeRepr,
    /// When `schema_brand` is set this carries the canonical schema
    /// shape so multi-segment field walks (`x.a.b`) can find the
    /// nested field layouts without re-running the type resolver.
    /// `None` for non-schema bindings.
    schema: Option<Schema>,
}

fn build_local_index(
    sig: &MainSignature,
    main_schema: &Schema,
    layout: &OffsetTable,
) -> Result<Vec<LocalBinding>, LoweringError> {
    // The schema and layout are built side-by-side from the same
    // `MainSignature`, so their `fields` vectors line up index-for-
    // index. We zip them once here so the body walk does a O(N) lookup
    // by name without re-walking the offset table per reference.
    debug_assert_eq!(main_schema.fields.len(), layout.fields.len());
    debug_assert_eq!(main_schema.fields.len(), sig.params.len());
    let mut out = Vec::with_capacity(sig.params.len());
    for (field, slot) in main_schema.fields.iter().zip(layout.fields.iter()) {
        let (ir_ty, schema_brand, schema_shape) = match &field.ty {
            TypeRepr::Schema { schema } => (
                // Schema-typed params ride a pointer slot in the
                // fixed area; the IR-level brand stays I32 but the
                // binding remembers the schema name for method
                // dispatch + nested field walks.
                IrType::I32,
                Some(schema.name.clone()),
                Some((**schema).clone()),
            ),
            other => (type_repr_to_ir_type(other)?, None, None),
        };
        out.push(LocalBinding {
            name: field.name.clone(),
            ty: ir_ty,
            offset: slot.offset as u32,
            schema_brand,
            type_repr: field.ty.clone(),
            schema: schema_shape,
        });
    }
    Ok(out)
}

/// Phase 10-a: shallow predicate over a `TypeNode` head.
/// Returns `true` when the head names a closure type (`Closure<...>`
/// or `Fn<...>`); used by the entry-signature validator to reject
/// closure-typed `#main` params / returns before the schema-build
/// pass even runs.
fn type_node_names_closure(t: &TypeNode) -> bool {
    t.path.len() == 1 && (t.path[0] == "Closure" || t.path[0] == "Fn")
}

/// Format a `TypeNode`'s head + generics for the error message
/// without dragging the analyzer's full `format_type` in.
fn type_head_for_display(t: &TypeNode) -> String {
    if t.path.is_empty() {
        return "<empty>".to_string();
    }
    let mut s = t.path.join(".");
    if !t.generics.is_empty() {
        s.push('<');
        for (i, g) in t.generics.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&type_head_for_display(g));
        }
        s.push('>');
    }
    s
}

fn token_key_display(key: &TokenKey) -> String {
    match key {
        TokenKey::Dummy => "_".to_string(),
        TokenKey::Index(i, optional) => {
            if *optional {
                format!("{i}?")
            } else {
                i.to_string()
            }
        }
        TokenKey::String(s, _, optional) => {
            if *optional {
                format!("{s}?")
            } else {
                s.clone()
            }
        }
        TokenKey::Dynamic(_, optional) => {
            if *optional {
                "<dynamic>?".to_string()
            } else {
                "<dynamic>".to_string()
            }
        }
        TokenKey::Spread(_) => "...".to_string(),
    }
}

/// Recursive expression lowering. Appends ops to `ctx.out` in postfix /
/// stack order.
fn lower_expr(expr: &Expr, range: TokenRange, ctx: &mut LowerCtx<'_>) -> Result<(), LoweringError> {
    match expr {
        Expr::Bool(b) => {
            ctx.out.push(TaggedOp {
                op: Op::ConstBool(*b),
                range,
            });
            ctx.tstack.push(IrType::Bool);
            Ok(())
        }
        Expr::Int(i) => {
            ctx.out.push(TaggedOp {
                op: Op::ConstI64(*i),
                range,
            });
            ctx.tstack.push(IrType::I64);
            Ok(())
        }
        Expr::Float(f) => {
            ctx.out.push(TaggedOp {
                op: Op::ConstF64(OrderedFloat::from(f.into_inner())),
                range,
            });
            ctx.tstack.push(IrType::F64);
            Ok(())
        }
        Expr::String(s) => {
            // #151 — Intern through the module-wide
            // `ConstInternTables`. Two `Op::ConstString` with the same
            // bytes (across any func in the module — entry / schema
            // method / lambda body) resolve to the same idx so the
            // const-pool walker stores one `[len][payload]` record and
            // every reference materialises the same arena offset.
            // The bytes still ride along on the op so the const-pool
            // pass (which walks `Op::ConstString` by visitor dispatch)
            // can populate `string_offsets[idx]` on the first sighting.
            let idx = ctx.const_intern.borrow_mut().strings.intern(s);
            ctx.out.push(TaggedOp {
                op: Op::ConstString {
                    idx,
                    value: s.clone(),
                },
                range,
            });
            ctx.tstack.push(IrType::String);
            Ok(())
        }
        Expr::List(items) => {
            // Phase 10-c: list literals dispatch on the first element's
            // shape — `[1, 2, 3]` → `Op::ConstListInt`; `[1.0, 2.0]` →
            // `Op::ConstListFloat`; `[true, false]` → `Op::ConstListBool`;
            // `["a", "b"]` → `Op::ConstListString`. Empty list is
            // ambiguous; reject for now (callers that need an empty
            // typed list build it via the buffer writer instead).
            //
            // Mixed-shape literals (e.g. `[1, 2.0]`) reject with a
            // lowering error so a hand-written program surfaces the
            // mistake clearly rather than crashing at codegen.
            if items.is_empty() {
                return Err(cap!(
                    "lower_expr.unsupported_expr.1",
                    LoweringError::UnsupportedExpr {
                        kind: "List(empty literal)".to_string(),
                        range,
                    }
                ));
            }
            // Wave R12-lower: list spread `[...a, b, ...c]`. Statically
            // flatten each `...source` (a list literal) into its elements
            // in order — matching the tree-walk `Expr::List` spread branch
            // (source elements concatenated in place, then the trailing
            // elements) — then route the flat sequence through the same
            // runtime scalar-list materialiser the computed-element path
            // uses. A spread source that is not a list literal (a param /
            // call / non-list) is not statically resolvable and caps loudly
            // (`flatten_list_spread`). The flat element list is homogeneous
            // (the analyzer enforced it), so the Int / Float materialiser
            // accepts it byte-for-byte like a plain `[1, 2, n]`.
            if list_has_spread(items) {
                // Runtime (non-list-literal) spread sources —
                // `[a, ...xs, b, ...ys, c]` with each source a
                // `List<Int>` / `List<Float>` parameter or computed
                // handle — are not statically flattenable (the source
                // lengths are only known at runtime). Materialise them
                // directly: alloc one scratch record sized from the
                // summed source headers, then walk the scalar/source
                // segments left to right (`memory.copy` per source,
                // inline store per scalar), leaving a scratch list
                // handle on the same path the literal-source spread +
                // map output use. Non-scalar surrounding elements /
                // list-literal sources mixed with runtime sources fall
                // through to the static flatten below (and its loud
                // caps).
                if let Some(shape) = classify_runtime_spread(items) {
                    return emit_list_spread_runtime_materialize(&shape, range, ctx);
                }
                let flat = flatten_list_spread(items, range)?;
                if flat.is_empty() {
                    return Err(cap!(
                        "lower_expr.unsupported_expr.spread_empty",
                        LoweringError::UnsupportedExpr {
                            kind:
                                "List(spread flattened to empty — element type cannot be inferred)"
                                    .to_string(),
                            range,
                        }
                    ));
                }
                let elem_ty = probe_expr_ir_ty(&flat[0], ctx)?;
                match elem_ty {
                    IrType::F64 => return emit_list_float_literal_materialize(&flat, range, ctx),
                    IrType::I64 => return emit_list_int_literal_materialize(&flat, range, ctx),
                    other => {
                        return Err(cap!(
                            "lower_expr.unsupported_expr.spread_elem_ty",
                            LoweringError::UnsupportedExpr {
                                kind: format!(
                                    "List(spread element of type {other:?} — only Float / Int \
                                     spread list literals are materialised in the AOT envelope)"
                                ),
                                range,
                            }
                        ))
                    }
                }
            }
            // #359 (W20): a list literal with at least one *computed*
            // element (the per-step `step(s)` body `[s[0] + s[4]*dt, ..]`)
            // cannot be interned as a `ConstList*` — materialise it into a
            // scratch arena and leave a runtime list handle. The element
            // shape (Float vs Int) is determined by speculatively lowering
            // the first element and inspecting its IR type (rolled back so
            // the real materialiser re-lowers from a clean state). W20's
            // computed list is `List<Float>`; the Int path is supported
            // symmetrically for completeness. An all-literal list still
            // flows through the `ConstList*` arm below (consumed by the
            // wasm / bytecode / cranelift const-pool path); only the
            // previously-rejected computed shape is diverted here, so no
            // other backend's behaviour changes.
            if list_has_computed_element(items) {
                let elem_ty = probe_expr_ir_ty(&items[0], ctx)?;
                match elem_ty {
                    IrType::F64 => return emit_list_float_literal_materialize(items, range, ctx),
                    IrType::I64 => return emit_list_int_literal_materialize(items, range, ctx),
                    other => {
                        return Err(cap!(
                            "lower_expr.unsupported_expr.2",
                            LoweringError::UnsupportedExpr {
                                kind: format!(
                                    "List(computed element of type {other:?} — only Float / Int \
                                 computed list literals are materialised in the AOT envelope)"
                                ),
                                range,
                            }
                        ))
                    }
                }
            }
            // Detect the shape from the first element.
            let kind = match &*items[0].expr {
                Expr::Int(_) => ConstListKind::Int,
                Expr::Float(_) => ConstListKind::Float,
                Expr::Bool(_) => ConstListKind::Bool,
                Expr::String(_) => ConstListKind::String,
                other => {
                    return Err(cap!(
                        "lower_expr.unsupported_expr.3",
                        LoweringError::UnsupportedExpr {
                            kind: format!("List(non-literal element `{}`)", other.kind()),
                            range: items[0].range,
                        }
                    ));
                }
            };
            match kind {
                ConstListKind::Int => {
                    let mut elements: Vec<i64> = Vec::with_capacity(items.len());
                    for node in items {
                        match &*node.expr {
                            Expr::Int(v) => elements.push(*v),
                            _ => {
                                return Err(cap!(
                                    "lower_expr.unsupported_expr.4",
                                    LoweringError::UnsupportedExpr {
                                        kind: format!(
                                            "List<Int>(non-Int element `{}`)",
                                            node.expr.kind()
                                        ),
                                        range: node.range,
                                    }
                                ));
                            }
                        }
                    }
                    let idx = ctx.const_intern.borrow_mut().alloc_list_int_idx();
                    ctx.out.push(TaggedOp {
                        op: Op::ConstListInt { idx, elements },
                        range,
                    });
                    ctx.tstack.push(IrType::ListInt);
                }
                ConstListKind::Float => {
                    let mut elements: Vec<u64> = Vec::with_capacity(items.len());
                    for node in items {
                        match &*node.expr {
                            Expr::Float(v) => elements.push(v.into_inner().to_bits()),
                            // Accept Int promotion so `[1, 2.0, 3]` lowers
                            // through this arm without forcing the
                            // caller to spell every literal with a
                            // decimal point.
                            Expr::Int(v) => elements.push((*v as f64).to_bits()),
                            _ => {
                                return Err(cap!(
                                    "lower_expr.unsupported_expr.5",
                                    LoweringError::UnsupportedExpr {
                                        kind: format!(
                                            "List<Float>(non-Float element `{}`)",
                                            node.expr.kind()
                                        ),
                                        range: node.range,
                                    }
                                ));
                            }
                        }
                    }
                    let idx = ctx.const_intern.borrow_mut().alloc_list_float_idx();
                    ctx.out.push(TaggedOp {
                        op: Op::ConstListFloat { idx, elements },
                        range,
                    });
                    ctx.tstack.push(IrType::ListFloat);
                }
                ConstListKind::Bool => {
                    let mut elements: Vec<bool> = Vec::with_capacity(items.len());
                    for node in items {
                        match &*node.expr {
                            Expr::Bool(v) => elements.push(*v),
                            _ => {
                                return Err(cap!(
                                    "lower_expr.unsupported_expr.6",
                                    LoweringError::UnsupportedExpr {
                                        kind: format!(
                                            "List<Bool>(non-Bool element `{}`)",
                                            node.expr.kind()
                                        ),
                                        range: node.range,
                                    }
                                ));
                            }
                        }
                    }
                    let idx = ctx.const_intern.borrow_mut().alloc_list_bool_idx();
                    ctx.out.push(TaggedOp {
                        op: Op::ConstListBool { idx, elements },
                        range,
                    });
                    ctx.tstack.push(IrType::ListBool);
                }
                ConstListKind::String => {
                    let mut elements: Vec<String> = Vec::with_capacity(items.len());
                    for node in items {
                        match &*node.expr {
                            Expr::String(v) => elements.push(v.clone()),
                            _ => {
                                return Err(cap!(
                                    "lower_expr.unsupported_expr.7",
                                    LoweringError::UnsupportedExpr {
                                        kind: format!(
                                            "List<String>(non-String element `{}`)",
                                            node.expr.kind()
                                        ),
                                        range: node.range,
                                    }
                                ));
                            }
                        }
                    }
                    let idx = ctx.const_intern.borrow_mut().alloc_list_string_idx();
                    ctx.out.push(TaggedOp {
                        op: Op::ConstListString { idx, elements },
                        range,
                    });
                    ctx.tstack.push(IrType::ListString);
                }
            }
            Ok(())
        }
        Expr::Variable(path) => lower_variable(path, range, ctx),
        Expr::Reference { base, path } => lower_reference(*base, path, range, ctx),
        Expr::Match {
            expr: scrutinee,
            arms,
        } => lower_match(scrutinee, arms, range, ctx),
        Expr::FString(parts) => lower_fstring(parts, range, ctx),
        Expr::Binary(op, lhs, rhs) => lower_binary(*op, lhs, rhs, range, ctx),
        Expr::Ternary { cond, then, els } => lower_ternary(cond, then, els, range, ctx),
        Expr::Where { expr, bindings } => lower_where(expr, bindings, range, ctx),
        Expr::Comprehension {
            element,
            id,
            iterable,
            condition,
        } => lower_comprehension(element, id, iterable, condition.as_ref(), range, ctx),
        Expr::FnCall { path, args } => lower_fn_call(path, args, range, ctx),
        Expr::Closure { .. } => Err(cap!(
            "lower_expr.closure_across_boundary",
            LoweringError::ClosureAcrossBoundary {
                context: "closure used in a non-higher-order position".to_string(),
                range,
            }
        )),
        _ => Err(cap!(
            "lower_expr.unsupported_expr.8",
            LoweringError::UnsupportedExpr {
                kind: expr.kind().to_string(),
                range,
            }
        )),
    }
}
