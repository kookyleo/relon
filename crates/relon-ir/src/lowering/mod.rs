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
mod closure;
mod peephole;
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

/// Phase F.2 (W7 anon-Dict-return): plan emitted by
/// [`anon_dict_return_plan`] when `#main(...) -> Dict { ... }` is
/// being lifted from "rejected as UnsupportedTypeInMain" to "lower as
/// an anonymous schema with closure-typed fields lifted to internal
/// let-bindings".
#[derive(Debug, Clone)]
struct AnonDictPlan {
    /// Synthesised return schema. Only carries the **scalar** fields
    /// — closure-typed source-level fields are lifted to internal
    /// let-bindings and do not appear here (they would be rejected
    /// by [`SchemaLayout::offsets_for`] anyway per the Phase B guard).
    schema: Schema,
    /// Per-source-field classification in declaration order. The body
    /// walker iterates these to decide whether to emit a closure
    /// let-binding (no host-visible field) or a normal record store.
    fields: Vec<AnonDictField>,
    /// R13: indices into `fields` giving the topological order the body
    /// walker must emit them in, so a `&sibling` / `&root` reference (to
    /// an earlier *or* later declared sibling) sees its target field's
    /// let already bound. For backward-only / reference-free bodies this
    /// is `0..fields.len()` (declaration order), preserving the
    /// pre-existing byte-for-byte compiled output.
    emit_order: Vec<usize>,
}

/// One classified entry from [`AnonDictPlan::fields`]. The walker
/// pairs the source-level `name` with either a closure signature (to
/// emit `MakeClosure` + `LetSet`) or the canonical scalar type the
/// matching schema field will store.
#[derive(Debug, Clone)]
enum AnonDictField {
    /// Source-level field whose value is an `Expr::Closure` literal.
    /// Lifted to an internal let-binding; its surface signature is
    /// memoised in `LowerCtx::closure_let_signatures` so a recursive
    /// self-call inside the body resolves to `Op::CallClosure`.
    Closure {
        name: String,
        param_tys: Vec<IrType>,
        ret_ty: IrType,
        /// Per-param mask: `true` when the param's `String` type came
        /// from the concat-body inference (not an annotation), so a
        /// call site may render a scalar argument to `String` first —
        /// see [`plan_anon_dict_closure_sig`].
        concat_coercible: Vec<bool>,
    },
    /// Source-level field whose value is a normal expression (the
    /// "host-visible" surface). Stored into the root record at the
    /// matching offset.
    Scalar { name: String, ty: TypeRepr },
    /// W5-P1: source-level field whose value is a `{str: int}` dict
    /// literal. Lifted to an internal let-binding (an `IrType::Dict`
    /// captured local materialised via `Op::ConstDict`); like a
    /// closure field it contributes no host-visible record slot.
    /// `entries` are in source declaration order.
    DictStrInt {
        name: String,
        entries: Vec<(String, i64)>,
    },
    /// W5-P4: source-level field whose value is a `["a", "b", ...]`
    /// list-of-string literal. Lifted to an internal let-binding (an
    /// `IrType::ListString` captured local materialised via
    /// `Op::ConstListString`); like a Dict field it contributes no
    /// host-visible record slot. `elements` are in source order.
    ListString { name: String, elements: Vec<String> },
    /// F1b: a host-visible field whose value is a `#main` **parameter
    /// identity** of pointer-array list type `List<Schema>` /
    /// `List<List<scalar>>`. The parameter's data lives in the *input*
    /// region while the object head sits in the *output* region — a
    /// cross-region link. Under the F1 arena-absolute slot convention
    /// the field slot stores the parameter list root's arena-absolute
    /// offset directly (no tail copy); the host's multi-region verifier
    /// classifies that offset into the input region, bounds-checks the
    /// whole reachable graph, and the reader follows it cross-region.
    /// `ty` is the canonical `List<Schema>` / `List<List<scalar>>` type
    /// (carrying the element schema). The source parameter is reached via
    /// the field's value node (the `Variable(param)` expr) in
    /// `lower_anon_dict_body`, so the name need not be stored separately.
    CrossRegionParamList { name: String, ty: TypeRepr },
}

/// Try to build an [`AnonDictPlan`] for the entry's body when the
/// return type is a bare `Dict` and the body is a dict literal.
/// Returns `Ok(None)` when the source does not match the anon-Dict
/// surface (preserving the existing
/// `build_main_return_schema → UnsupportedTypeInMain` path), and
/// `Ok(Some(_))` once the surface is recognised.
///
/// Per-field type classification today is **heuristic**: a closure
/// literal lifts to a `[I64] → I64` (or user-annotated) signature
/// matching the W7 production source shape (`fib: (Int k) -> Int => ...`); a
/// scalar field's type is taken from a small set of statically
/// derivable expressions (literal, arithmetic between literals,
/// `Variable(name)` against a `#main` Int param, and free-call
/// against a previously-classified closure field). Anything else
/// surfaces as a `LoweringError::UnsupportedExpr` — the broader
/// inference work stays Phase D scope. The shape is deliberately
/// minimal so the W7 cmp_lua workload passes the IR pass without
/// dragging analyzer-side per-field Dict inference into the picture.
/// Builtin `@`-decorator names. These resolve to host-registered
/// [`DecoratorPlugin`] semantics, not a user callable, so they have no
/// "desugar to `deco(value, args)`" form on the compiled path. `@value`
/// substitutes its first arg; `@expect`/`@msg`/`@error`/`@default` are
/// schema-field-meta hooks that are identity on ordinary values. A
/// future wave can lower the ones with a compiled-meaningful form; until
/// then they cap loudly rather than silently dropping the transform.
const BUILTIN_DECORATOR_NAMES: &[&str] = &["value", "expect", "msg", "error", "default"];

/// Wave R11: desugar field decorators in a `#main(...) -> Dict { ... }`
/// body before the anon-Dict-return plan / body lowering run.
///
/// A decorated field `@deco(a, b) k: v` desugars to the call
/// `deco(v, a, b)` — the decorated value is the **first** positional
/// argument, the decorator's own args follow. This matches the
/// tree-walk contract exactly: `TreeWalkEvaluator::fallback_decorator`
/// prepends `value` ahead of the evaluated decorator args (closure call
/// `[value, ..args]`; native call `positional.insert(0, value)`). The
/// `examples/pricing.relon` doc-comment that reads "value appended last"
/// describes a `currency(symbol, val)` whose parameter order happens to
/// place `val` last; the evaluator's actual convention — confirmed by
/// running `@currency("USD") x: 12.3` → `"12.3 USD"` — is value-first.
///
/// Stacked decorators apply bottom-up (`@a @b v ≡ a(b(v))`): the
/// decorator nearest the value wraps first, the outermost wraps last.
/// The tree-walk iterates `node.decorators.iter().rev()`; this builds
/// the nested call in the same order (innermost `Vec::last` first).
///
/// Returns `Ok(None)` when no field carries a decorator (the rewritten
/// AST is then byte-identical to the original, so the existing lowering
/// path — and the codegen bytes it produces — are untouched). Returns
/// `Ok(Some(rewritten_root))` when at least one field was desugared.
/// Caps loudly (never silently wrong) on a decorator shape that cannot
/// become a plain `deco(value, args)` call: a builtin `@`-decorator
/// (no compiled callable), a multi-segment / dynamic decorator path, or
/// a named decorator argument (the local-closure / native call lowering
/// admits positional args only).
fn desugar_anon_dict_decorators(root: &Node) -> Result<Option<Node>, LoweringError> {
    let Expr::Dict(pairs) = &*root.expr else {
        return Ok(None);
    };
    if pairs.iter().all(|(_, v)| v.decorators.is_empty()) {
        return Ok(None);
    }
    let mut new_pairs: Vec<(TokenKey, Node)> = Vec::with_capacity(pairs.len());
    for (key, value) in pairs {
        if value.decorators.is_empty() {
            new_pairs.push((key.clone(), value.clone()));
            continue;
        }
        new_pairs.push((key.clone(), desugar_field_decorators(value)?));
    }
    let mut new_root = root.clone();
    new_root.expr = std::sync::Arc::new(Expr::Dict(new_pairs));
    Ok(Some(new_root))
}

/// Desugar one decorated field value into a nested decorator-call node.
/// See [`desugar_anon_dict_decorators`] for the arg-order / stack-order
/// contract. The returned node carries the original field's directives
/// (so a `#internal` decorated field stays internal) and `type_hint`,
/// but has its decorators stripped — the transform is now expressed as
/// the call chain in `expr`.
fn desugar_field_decorators(value: &Node) -> Result<Node, LoweringError> {
    // Start from the bare value with decorators removed; fold each
    // decorator (innermost first) into a call wrapping the running node.
    let mut inner = value.clone();
    let decorators = std::mem::take(&mut inner.decorators);
    // Strip directives off the running call node — directives belong to
    // the field, not to the synthetic intermediate calls. They are
    // re-attached to the outermost node at the end.
    let directives = std::mem::take(&mut inner.directives);
    let type_hint = inner.type_hint.take();

    let mut current = inner;
    // Bottom-up: the decorator nearest the value (`Vec::last` — source
    // order stacks outermost-first into the vec) wraps first.
    for dec in decorators.iter().rev() {
        // Decorator path must be a single plain identifier resolving to
        // a user callable; multi-segment / dynamic paths have no
        // compiled-call form here.
        let path_ok =
            dec.path.len() == 1 && matches!(dec.path.first(), Some(TokenKey::String(_, _, _)));
        if !path_ok {
            return Err(cap!(
                "desugar_field_decorators.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: "field decorator with multi-segment / dynamic path".to_string(),
                    range: dec.range,
                }
            ));
        }
        let TokenKey::String(name, _, _) = &dec.path[0] else {
            unreachable!("guarded by path_ok");
        };
        if BUILTIN_DECORATOR_NAMES.contains(&name.as_str()) {
            return Err(cap!(
                "desugar_field_decorators.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!("builtin `@{name}` decorator has no compiled call form"),
                    range: dec.range,
                }
            ));
        }
        // Named decorator args can't be threaded through the positional-
        // only local-closure / native call lowering; cap loudly.
        if dec.args.iter().any(|a| a.name.is_some()) {
            return Err(cap!(
                "desugar_field_decorators.unsupported_expr.3",
                LoweringError::UnsupportedExpr {
                    kind: format!("field decorator `@{name}` with a named argument"),
                    range: dec.range,
                }
            ));
        }
        // Build `deco(current, ..dec.args)` — value first, then the
        // decorator's own positional args.
        let mut call_args: Vec<relon_parser::CallArg> = Vec::with_capacity(dec.args.len() + 1);
        call_args.push(relon_parser::CallArg {
            name: None,
            value: current,
        });
        call_args.extend(dec.args.iter().cloned());
        current = Node::new(
            Expr::FnCall {
                path: dec.path.clone(),
                args: call_args,
            },
            dec.range,
        );
    }

    // Re-attach the field's directives + type hint to the outermost call.
    current.directives = directives;
    current.type_hint = type_hint;
    Ok(current)
}

fn anon_dict_return_plan(
    sig: &MainSignature,
    root: &Node,
    resolver: &SchemaResolver<'_>,
) -> Result<Option<AnonDictPlan>, LoweringError> {
    let Some(rt) = sig.return_type.as_ref() else {
        return Ok(None);
    };
    if !type_node_is_bare_dict(rt) {
        return Ok(None);
    }
    let Expr::Dict(pairs) = &*root.expr else {
        return Ok(None);
    };

    // Build a quick scalar-type index for the `#main` parameters so
    // a `Variable(n)` on the RHS of a scalar field classifies cleanly.
    let mut param_tys: HashMap<&str, IrType> = HashMap::new();
    for p in &sig.params {
        if let Some(canonical) = type_node_to_canonical(&p.type_node) {
            if let Ok(irt) = type_repr_to_ir_type(&canonical) {
                param_tys.insert(p.name.as_str(), irt);
            }
        }
    }
    // F1b: full canonical types (carrying element schemas) for every
    // `#main` parameter, so a host-visible field whose value is a
    // parameter identity of `List<Schema>` / `List<List<scalar>>` type
    // can be classified as a cross-region field rather than rejected.
    let mut param_canonicals: HashMap<&str, TypeRepr> = HashMap::new();
    for p in &sig.params {
        if let Some(canonical) = type_node_to_canonical_with_schemas(&p.type_node, resolver) {
            param_canonicals.insert(p.name.as_str(), canonical);
        }
    }

    // R13: reference-aware emit order. Fields are classified (and later
    // lowered) in topological order over their `&sibling` / `&root`
    // reference edges so a forward reference sees its target already
    // bound; a reference cycle surfaces here as a loud error aligned
    // with the tree-walk oracle's `CircularReference`. Backward-only /
    // reference-free bodies reproduce declaration order exactly, so the
    // pre-existing compiled output stays byte-for-byte identical. The
    // `#main` param name set drives the forward-reference oracle-
    // agreement gate (a forward ref into a reference-bearing field whose
    // component reads a param diverges from the tree-walk oracle).
    let main_param_names: HashSet<&str> = sig.params.iter().map(|p| p.name.as_str()).collect();
    let emit_order = anon_dict_emit_order(pairs, &main_param_names, root.range)?;

    // Classified entries indexed by *declaration* position so the
    // synthesised return schema (and its layout) keeps declaration
    // order regardless of the classification order. `None` marks a
    // dropped `#internal` scalar field.
    let mut fields_by_decl: Vec<Option<AnonDictField>> = vec![None; pairs.len()];
    let mut closure_field_sigs: HashMap<&str, (Vec<IrType>, IrType)> = HashMap::new();
    // W5-P3: `{String -> Int}` dict fields seen so far, so a later
    // sibling field's `d[k]` index classifies to the dict's `Int`
    // value type. Source order makes `d` visible before `result`.
    let mut dict_field_names: HashSet<&str> = HashSet::new();
    // R10/R13: host-visible scalar / list fields classified so far,
    // name -> IR type. A `&sibling.<name>` (or entry-level
    // `&root.<name>`, which is the same — the entry dict IS the root)
    // classifies to the target field's type. Topological classification
    // order guarantees the target is in this map before the reference is
    // classified, for both backward and forward references.
    let mut scalar_field_irts: HashMap<&str, IrType> = HashMap::new();

    for &decl_idx in &emit_order {
        let (key, value) = &pairs[decl_idx];
        let TokenKey::String(name, _, _) = key else {
            return Err(cap!(
                "anon_dict_return_plan.unsupported_expr",
                LoweringError::UnsupportedExpr {
                    kind: "Dict(non-string-key in anon-Dict-return body)".to_string(),
                    range: root.range,
                }
            ));
        };
        // A field carrying a `#internal` pragma is hidden from the
        // host-visible return surface (the tree-walk oracle drops it
        // too — see the W7 dict-probe `#internal keys` workload).
        // Non-`#internal` collection fields, by contrast, MUST be
        // marshalled into the return buffer to match the oracle; a
        // List literal that is *not* internal becomes a host-visible
        // `List<elem>` field rather than a silently-dropped internal
        // let-binding.
        let is_internal = node_marked_internal(value);
        match &*value.expr {
            Expr::Closure {
                params,
                return_type,
                body,
            } => {
                // A closure value can never cross the host boundary, so
                // it is only legal as an internal helper binding. A
                // non-`#internal` closure field would otherwise be
                // silently dropped from the host output (the tree-walk
                // oracle errors on returning a closure), so reject it.
                if !is_internal {
                    return Err(cap!(
                        "anon_dict_return_plan.closure_across_boundary",
                        LoweringError::ClosureAcrossBoundary {
                            context: format!(
                                "anon-Dict-return field `{name}` is a closure but not `#internal`"
                            ),
                            range: value.range,
                        }
                    ));
                }
                // Read the real `(param_tys, ret_ty)` from the type
                // system: explicit param / return annotations first,
                // then a conservative String-concat body inference, then
                // the historical I64 default (W7 fib). See
                // `plan_anon_dict_closure_sig`.
                let (param_irts, ret_ty, concat_coercible) =
                    plan_anon_dict_closure_sig(params, return_type.as_ref(), &body.expr);
                closure_field_sigs.insert(name.as_str(), (param_irts.clone(), ret_ty));
                fields_by_decl[decl_idx] = Some(AnonDictField::Closure {
                    name: name.clone(),
                    param_tys: param_irts,
                    ret_ty,
                    concat_coercible,
                });
            }
            Expr::Dict(inner_pairs) => {
                // W5-P1: a `{str: int}` dict literal becomes a
                // dict-value internal let-binding. Only the
                // `{String -> Int}` shape is accepted in P1 — any
                // other entry shape surfaces UnsupportedExpr so the
                // edge stays honest (P2/P3 widen value/key types).
                //
                // A non-`#internal` dict-valued field has no compiled-
                // backend marshalling today (Dict is not a return type
                // on the buffer protocol), and the tree-walk oracle
                // *would* surface it — so leaving it as an internal
                // binding silently drops host-visible data. Reject it
                // loudly instead.
                if !is_internal {
                    return Err(cap!("anon_dict_return_plan.unsupported_field_type", LoweringError::UnsupportedFieldType {
                        schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                        field: name.clone(),
                        ty: "Dict-valued anon-Dict-return field is only supported as `#internal`"
                            .to_string(),
                        range: value.range,
                    }));
                }
                let entries = classify_anon_dict_str_int_field(inner_pairs, value.range, name)?;
                dict_field_names.insert(name.as_str());
                fields_by_decl[decl_idx] = Some(AnonDictField::DictStrInt {
                    name: name.clone(),
                    entries,
                });
            }
            Expr::List(items) => {
                if is_internal {
                    // W5-P4: a `#internal ["a", "b", ...]` list-of-string
                    // literal becomes a `ListString` internal let-binding
                    // (the `#internal keys` field of the dict-probe
                    // workload). Only the all-String-literal shape is
                    // accepted; any other element surfaces UnsupportedExpr
                    // so the edge stays honest.
                    let elements = classify_anon_dict_list_string_field(items, value.range, name)?;
                    fields_by_decl[decl_idx] = Some(AnonDictField::ListString {
                        name: name.clone(),
                        elements,
                    });
                } else {
                    // Host-visible list field: classify the element type
                    // (`List<Int/Float/Bool/String>`) and emit a real
                    // record field. The body walker lowers the list
                    // literal to a const-pool record and marshals it into
                    // the return buffer's tail (pointer-indirect). Any
                    // shape the marshaller cannot handle (mixed / empty /
                    // nested element lists / schema elements) surfaces a
                    // loud error rather than silently dropping the field.
                    let list_ty = classify_anon_dict_list_field(
                        items,
                        value.range,
                        name,
                        resolver,
                        &param_tys,
                    )?;
                    // R13: register the list field's IR type so a sibling
                    // `&sibling.<name>` / `&root.<name>` reference (forward
                    // or backward) resolves to the same `List<...>` type.
                    if let Ok(irt) = type_repr_to_ir_type(&list_ty) {
                        scalar_field_irts.insert(name.as_str(), irt);
                    }
                    fields_by_decl[decl_idx] = Some(AnonDictField::Scalar {
                        name: name.clone(),
                        ty: list_ty,
                    });
                }
            }
            Expr::Variable(path)
                if !is_internal
                    && path
                        .iter()
                        .all(|seg| matches!(seg, TokenKey::String(_, _, _)))
                    && anon_dict_cross_region_param_list(path, &param_canonicals).is_some() =>
            {
                // F1b: a host-visible field whose value is a parameter
                // identity of `List<Schema>` / `List<List<scalar>>` type.
                // The object head is built in out_buf but the parameter's
                // list data lives in in_buf — a cross-region link. Under
                // the F1 arena-absolute slot convention the field slot
                // stores the parameter list root's arena-absolute offset
                // (the value `LoadListSchemaPtr` / `LoadListListPtr` pushes
                // post-F1) directly, with no tail copy; the host's
                // multi-region verifier classifies the offset into in_buf,
                // bounds-checks the reachable graph, then the reader
                // follows it cross-region. Only the in-place reader's
                // decode envelope is admitted (`List<Schema>` element
                // sub-records confined to S4-scope field shapes); anything
                // deeper stays a loud cap.
                let ty = anon_dict_cross_region_param_list(path, &param_canonicals)
                    .expect("guarded by the match arm guard")
                    .clone();
                fields_by_decl[decl_idx] = Some(AnonDictField::CrossRegionParamList {
                    name: name.clone(),
                    ty,
                });
            }
            _ => {
                // An `#internal` scalar field is hidden from the host
                // (the tree-walk oracle drops it). Scalar internals are
                // not referenceable by siblings on this surface — a
                // `Variable(name)` against one already loud-errors in
                // `classify_anon_dict_scalar_field_irt` — so there is no
                // let-binding to keep; just drop it. Without this skip
                // the field would surface to the host while tree-walk
                // omits it (a silent field-set divergence). The value is
                // pure, so dropping it changes no observable behaviour.
                if is_internal {
                    continue;
                }
                let ty = classify_anon_dict_scalar_field(
                    &value.expr,
                    value.range,
                    &param_tys,
                    &closure_field_sigs,
                    &dict_field_names,
                    &scalar_field_irts,
                    name,
                )?;
                if let Ok(irt) = type_repr_to_ir_type(&ty) {
                    scalar_field_irts.insert(name.as_str(), irt);
                }
                fields_by_decl[decl_idx] = Some(AnonDictField::Scalar {
                    name: name.clone(),
                    ty,
                });
            }
        }
    }

    // Collapse the declaration-indexed slots into the declaration-order
    // field list (dropped `#internal` scalar slots stay `None`). The
    // schema / layout below is built from this declaration-ordered list,
    // so record offsets are independent of the topological emit order.
    // `decl_to_field` maps each surviving declaration index to its
    // position in `fields` so the topological `emit_order` (in
    // declaration indices) can be re-expressed in `fields` indices for
    // the body walker.
    let mut fields: Vec<AnonDictField> = Vec::with_capacity(pairs.len());
    let mut decl_to_field: Vec<Option<usize>> = vec![None; pairs.len()];
    for (decl_idx, slot) in fields_by_decl.into_iter().enumerate() {
        if let Some(field) = slot {
            decl_to_field[decl_idx] = Some(fields.len());
            fields.push(field);
        }
    }
    // Body-walker emit order over `fields` indices: walk the declaration
    // indices in topological order, keeping only those that survived as
    // host-visible / let-bound fields.
    let field_emit_order: Vec<usize> = emit_order
        .iter()
        .filter_map(|&decl_idx| decl_to_field[decl_idx])
        .collect();

    // Build the host-visible schema from the scalar entries only.
    let schema_fields: Vec<Field> = fields
        .iter()
        .filter_map(|f| match f {
            AnonDictField::Scalar { name, ty } => Some(Field {
                name: name.clone(),
                ty: ty.clone(),
                default: None,
            }),
            // F1b: a cross-region parameter-list field is host-visible —
            // it contributes a real `List<Schema>` / `List<List<scalar>>`
            // record slot (carrying the same canonical element type the
            // host reader / verifier rebuild their layouts from).
            AnonDictField::CrossRegionParamList { name, ty, .. } => Some(Field {
                name: name.clone(),
                ty: ty.clone(),
                default: None,
            }),
            AnonDictField::Closure { .. }
            | AnonDictField::DictStrInt { .. }
            | AnonDictField::ListString { .. } => None,
        })
        .collect();
    let schema = Schema {
        name: MAIN_RETURN_SCHEMA_NAME.to_string(),
        generics: vec![],
        fields: schema_fields,
        is_tuple: false,
    };
    Ok(Some(AnonDictPlan {
        schema,
        fields,
        emit_order: field_emit_order,
    }))
}

/// Collect the host-visible sibling fields a value expression
/// references through a single-segment `&sibling.<name>` / `&root.<name>`
/// reference, restricted to names present in `field_names`. Used to
/// build the anon-Dict-return field dependency graph so forward
/// references can be emitted after their targets and reference cycles
/// surface as a loud `CircularReference`-aligned error.
///
/// Only the reference shape the compiled path lowers contributes an
/// edge: positional/runtime bases, dynamic keys, multi-segment paths
/// and bare `Variable` heads (which name `#main` params, not fields) are
/// deliberately ignored here — they are handled (or capped) elsewhere.
fn collect_anon_dict_ref_edges<'a>(
    expr: &'a Expr,
    field_names: &HashSet<&'a str>,
    out: &mut Vec<&'a str>,
) {
    match expr {
        Expr::Reference {
            base: RefBase::Sibling | RefBase::Root,
            path,
        } => {
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                let n = name.as_str();
                if field_names.contains(n) && !out.contains(&n) {
                    out.push(n);
                }
            }
        }
        Expr::Binary(_, a, b) => {
            collect_anon_dict_ref_edges(&a.expr, field_names, out);
            collect_anon_dict_ref_edges(&b.expr, field_names, out);
        }
        Expr::Unary(_, inner) => collect_anon_dict_ref_edges(&inner.expr, field_names, out),
        Expr::Ternary { cond, then, els } => {
            collect_anon_dict_ref_edges(&cond.expr, field_names, out);
            collect_anon_dict_ref_edges(&then.expr, field_names, out);
            collect_anon_dict_ref_edges(&els.expr, field_names, out);
        }
        Expr::List(items) => {
            for n in items {
                collect_anon_dict_ref_edges(&n.expr, field_names, out);
            }
        }
        Expr::FnCall { args, .. } => {
            for a in args {
                collect_anon_dict_ref_edges(&a.value.expr, field_names, out);
            }
        }
        _ => {}
    }
}

/// True when `expr` reads a `#main` parameter through a bare
/// single-segment `Variable([param])`. Walks the same expression shapes
/// the anon-Dict-return scalar / list classifier understands. Used by
/// the forward-reference oracle-agreement gate in [`anon_dict_emit_order`].
fn expr_reads_main_param(expr: &Expr, main_param_names: &HashSet<&str>) -> bool {
    match expr {
        Expr::Variable(path) => {
            matches!(path.as_slice(), [TokenKey::String(name, _, _)]
                if main_param_names.contains(name.as_str()))
                // A `d[k]` style index still reads its head identifier;
                // treat any leading param identifier as a param read.
                || matches!(path.first(), Some(TokenKey::String(name, _, _))
                    if main_param_names.contains(name.as_str()))
        }
        Expr::Binary(_, a, b) => {
            expr_reads_main_param(&a.expr, main_param_names)
                || expr_reads_main_param(&b.expr, main_param_names)
        }
        Expr::Unary(_, inner) => expr_reads_main_param(&inner.expr, main_param_names),
        Expr::Ternary { cond, then, els } => {
            expr_reads_main_param(&cond.expr, main_param_names)
                || expr_reads_main_param(&then.expr, main_param_names)
                || expr_reads_main_param(&els.expr, main_param_names)
        }
        Expr::List(items) => items
            .iter()
            .any(|n| expr_reads_main_param(&n.expr, main_param_names)),
        Expr::FnCall { args, .. } => args
            .iter()
            .any(|a| expr_reads_main_param(&a.value.expr, main_param_names)),
        _ => false,
    }
}

/// Connected-component labelling of the undirected anon-Dict reference
/// graph (`field_refs[i]` = the sibling fields field `i` references).
/// Returns a component id per field. Used by the forward-reference
/// oracle-agreement gate so a reference whose component reads a `#main`
/// parameter can be distinguished from a fully param-free one.
fn anon_dict_ref_components(n: usize, field_refs: &[Vec<usize>]) -> Vec<usize> {
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], x: usize) -> usize {
        let mut root = x;
        while parent[root] != root {
            root = parent[root];
        }
        // Path compression.
        let mut cur = x;
        while parent[cur] != root {
            let next = parent[cur];
            parent[cur] = root;
            cur = next;
        }
        root
    }
    for (i, refs) in field_refs.iter().enumerate() {
        for &j in refs {
            let ri = find(&mut parent, i);
            let rj = find(&mut parent, j);
            if ri != rj {
                parent[ri] = rj;
            }
        }
    }
    (0..n).map(|i| find(&mut parent, i)).collect()
}

/// Decide the order in which the anon-Dict-return body fields must be
/// classified / emitted so that a `&sibling.<name>` / `&root.<name>`
/// reference always sees its target field already bound — regardless of
/// whether the target is declared earlier (backward) or later (forward)
/// in source.
///
/// `pairs` are the source dict entries in declaration order. The
/// returned vector is a permutation of `0..pairs.len()` (the topological
/// order over the reference-edge graph). A field `i` that references a
/// sibling field `j` produces edge `j → i` (j must be ready first), so
/// Kahn's algorithm emits `j` before `i`.
///
/// The ready queue is drained in ascending declaration index so that a
/// graph with **only backward edges** (every reference targets an
/// earlier field) yields the identity order `0,1,2,…` — preserving the
/// byte-for-byte output of the pre-existing source-ordered lowering. A
/// forward reference is the only thing that perturbs the order.
///
/// A reference cycle (`x: &sibling.y, y: &sibling.x`, or a self
/// reference `x: &sibling.x`) leaves Kahn unable to drain the graph and
/// surfaces as [`LoweringError::CyclicFieldDependency`] — the compiled
/// path's loud analogue of the tree-walk oracle's `CircularReference`.
fn anon_dict_emit_order(
    pairs: &[(TokenKey, Node)],
    main_param_names: &HashSet<&str>,
    range: TokenRange,
) -> Result<Vec<usize>, LoweringError> {
    let n = pairs.len();
    let mut name_to_idx: HashMap<&str, usize> = HashMap::with_capacity(n);
    for (i, (key, _)) in pairs.iter().enumerate() {
        if let TokenKey::String(name, _, _) = key {
            // First declaration wins for duplicate keys; the dict
            // builder rejects genuine duplicates elsewhere, and using
            // the first keeps edge resolution deterministic.
            name_to_idx.entry(name.as_str()).or_insert(i);
        }
    }
    let field_names: HashSet<&str> = name_to_idx.keys().copied().collect();

    let mut incoming = vec![0usize; n];
    let mut outgoing: Vec<Vec<usize>> = vec![Vec::new(); n];
    // Per-field: reference-bearing (references some sibling), reads a
    // `#main` param directly, and the sibling fields it references (by
    // declaration index). Used by the forward-reference oracle-agreement
    // gate below.
    let mut is_ref_bearing = vec![false; n];
    let mut reads_param = vec![false; n];
    let mut field_refs: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, (_, value)) in pairs.iter().enumerate() {
        reads_param[i] = expr_reads_main_param(&value.expr, main_param_names);
        let mut refs: Vec<&str> = Vec::new();
        collect_anon_dict_ref_edges(&value.expr, &field_names, &mut refs);
        is_ref_bearing[i] = !refs.is_empty();
        for r in refs {
            // edge target → this field; skip a self edge so a field that
            // references its own name still surfaces as a cycle (Kahn
            // counts the incoming edge and never drains it).
            if let Some(&j) = name_to_idx.get(r) {
                outgoing[j].push(i);
                incoming[i] += 1;
                field_refs[i].push(j);
            }
        }
    }

    // Forward-reference oracle-agreement gate.
    //
    // The tree-walk oracle resolves anon-Dict field references lazily.
    // A *forward* reference (a field referencing a later-declared
    // sibling) forces the target field's thunk; when that target is
    // itself reference-bearing and its connected reference component
    // reaches a `#main` parameter, the oracle forces it under a scope
    // that has lost the `#main` parameter frame and raises
    // `variable_not_found`. The compiled path *can* evaluate it, but
    // emitting a value where the reference oracle errors would be a
    // silent divergence — so we cap that exact shape loudly. Forward
    // references whose target is a non-reference leaf (`x: a + b`), and
    // reference chains whose whole connected component is `#main`-param-
    // free, both resolve consistently four-way and are admitted.
    let component = anon_dict_ref_components(n, &field_refs);
    let mut component_reads_param: HashSet<usize> = HashSet::new();
    for (i, &reads) in reads_param.iter().enumerate() {
        if reads {
            component_reads_param.insert(component[i]);
        }
    }
    for (i, refs) in field_refs.iter().enumerate() {
        for &j in refs {
            // forward reference: the target is declared after the
            // referencing field.
            if j > i && is_ref_bearing[j] && component_reads_param.contains(&component[i]) {
                let (fname, frange) = match &pairs[i].0 {
                    TokenKey::String(s, r, _) => (s.clone(), *r),
                    other => (format!("{other:?}"), range),
                };
                return Err(cap!(
                    "anon_dict_emit_order.forward_ref_through_param",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "AnonDictReturn(field `{}`: forward reference into a reference-bearing \
                             field whose component reads a `#main` parameter — the tree-walk \
                             oracle cannot resolve this shape consistently)",
                            fname
                        ),
                        range: frange,
                    }
                ));
            }
        }
    }

    // Kahn's algorithm with an ascending-index ready set so backward-only
    // graphs reproduce declaration order exactly.
    let mut ready: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for (i, &deg) in incoming.iter().enumerate() {
        if deg == 0 {
            ready.insert(i);
        }
    }
    let mut order: Vec<usize> = Vec::with_capacity(n);
    while let Some(&i) = ready.iter().next() {
        ready.remove(&i);
        order.push(i);
        for &j in &outgoing[i] {
            incoming[j] -= 1;
            if incoming[j] == 0 {
                ready.insert(j);
            }
        }
    }
    if order.len() != n {
        let cycle = find_anon_dict_ref_cycle(pairs, &outgoing, &incoming);
        return Err(cap!(
            "anon_dict_emit_order.cyclic_field_dependency",
            LoweringError::CyclicFieldDependency {
                schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                cycle,
                range,
            }
        ));
    }
    Ok(order)
}

/// Build a representative reference-cycle path (field names, first name
/// repeated at the end) for the anon-Dict-return diagnostic. The caller
/// has already proven a cycle exists (Kahn could not drain the graph).
fn find_anon_dict_ref_cycle(
    pairs: &[(TokenKey, Node)],
    outgoing: &[Vec<usize>],
    incoming: &[usize],
) -> Vec<String> {
    let n = outgoing.len();
    let field_name = |i: usize| match &pairs[i].0 {
        TokenKey::String(name, _, _) => name.clone(),
        other => format!("{other:?}"),
    };
    let mut visited = vec![false; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..n {
        if visited[start] || incoming[start] == 0 {
            continue;
        }
        if let Some(cycle) =
            dfs_find_cycle(start, outgoing, &mut visited, &mut on_stack, &mut stack)
        {
            return cycle.into_iter().map(field_name).collect();
        }
    }
    Vec::new()
}

/// True when `t` is a single-segment `Dict` with no generic
/// arguments — the surface [`anon_dict_return_plan`] hangs the W7
/// anon-Dict-return lifting off. Multi-segment paths (`pkg.Dict`),
/// `Dict<K, V>` with explicit generics, and variant-style nodes are
/// out of scope.
fn type_node_is_bare_dict(t: &TypeNode) -> bool {
    t.path.len() == 1 && t.path[0] == "Dict" && t.generics.is_empty() && t.variant_fields.is_none()
}

/// Statically derive a [`TypeRepr`] for a scalar dict field in the
/// W7 anon-Dict-return path. Today's surface intentionally stays
/// minimal — anything beyond the supported shapes surfaces as
/// `UnsupportedExpr` so the future inference work has a clear edge
/// rather than a half-implemented fallback.
///
/// Supported value shapes:
/// * `Expr::Int` / `Expr::Float` / `Expr::Bool` / `Expr::String`.
/// * `Expr::Variable([name])` where `name` resolves to a `#main`
///   parameter with a known scalar IR type.
/// * `Expr::FnCall { path: [name], args }` where `name` was already
///   classified as a closure field — the field type is the closure's
///   declared return type.
/// * `Expr::Binary(Add|Sub|Mul|Div|Mod, lhs, rhs)` over integers /
///   floats — propagates the operand type (Int + Int → Int).
fn classify_anon_dict_scalar_field(
    expr: &Expr,
    range: TokenRange,
    main_param_tys: &HashMap<&str, IrType>,
    closure_field_sigs: &HashMap<&str, (Vec<IrType>, IrType)>,
    dict_field_names: &HashSet<&str>,
    scalar_field_irts: &HashMap<&str, IrType>,
    field_name: &str,
) -> Result<TypeRepr, LoweringError> {
    let irt = classify_anon_dict_scalar_field_irt(
        expr,
        range,
        main_param_tys,
        closure_field_sigs,
        dict_field_names,
        scalar_field_irts,
        field_name,
    )?;
    ir_type_to_type_repr(irt).ok_or_else(|| {
        cap!(
            "classify_anon_dict_scalar_field.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "AnonDictReturn(field `{}`: non-scalar inferred IR type {:?})",
                    field_name, irt,
                ),
                range,
            }
        )
    })
}

/// W5-P1: classify a `{str: int}` dict literal sitting on the RHS of
/// an anon-Dict-return `#internal` field. Returns the `(key, value)`
/// entry set in source declaration order when every entry is a
/// string-key / integer-literal pair; any other entry shape (non-string
/// key, spread, non-Int value, nested dict) surfaces `UnsupportedExpr`
/// so the P1 surface stays honest — value/key-type widening is P2/P3.
fn classify_anon_dict_str_int_field(
    pairs: &[(TokenKey, Node)],
    range: TokenRange,
    field_name: &str,
) -> Result<Vec<(String, i64)>, LoweringError> {
    let mut entries: Vec<(String, i64)> = Vec::with_capacity(pairs.len());
    for (key, value) in pairs {
        let TokenKey::String(key_name, _, _) = key else {
            return Err(cap!(
                "classify_anon_dict_str_int_field.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "AnonDictReturn(dict field `{}`: non-string dict key)",
                        field_name
                    ),
                    range,
                }
            ));
        };
        let Expr::Int(v) = &*value.expr else {
            return Err(cap!("classify_anon_dict_str_int_field.unsupported_expr.2", LoweringError::UnsupportedExpr {
                kind: format!(
                    "AnonDictReturn(dict field `{}`: value for key `{}` is `{}`, only Int literals supported in P1)",
                    field_name,
                    key_name,
                    value.expr.kind()
                ),
                range: value.range,
            }));
        };
        entries.push((key_name.clone(), *v));
    }
    Ok(entries)
}

/// W5-P4: classify a `["a", "b", ...]` list-of-string literal sitting on
/// the RHS of an anon-Dict-return `#internal` field (the `keys` field of
/// the dict-probe workload). Returns the element set in source order when
/// every element is a String literal; any other element shape (non-String
/// literal, nested list, spread) surfaces `UnsupportedExpr` so the
/// surface stays honest — non-String list fields are out of scope here.
fn classify_anon_dict_list_string_field(
    items: &[Node],
    range: TokenRange,
    field_name: &str,
) -> Result<Vec<String>, LoweringError> {
    let mut elements: Vec<String> = Vec::with_capacity(items.len());
    for node in items {
        let Expr::String(s) = &*node.expr else {
            return Err(cap!(
                "classify_anon_dict_list_string_field.unsupported_expr",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                    "AnonDictReturn(list field `{}`: element `{}`, only String literals supported)",
                    field_name,
                    node.expr.kind()
                ),
                    range,
                }
            ));
        };
        elements.push(s.clone());
    }
    Ok(elements)
}

/// True when a dict-field value node carries a `#internal` pragma. The
/// anon-Dict-return path uses this to keep `#internal` collection /
/// closure fields off the host-visible return surface (matching the
/// tree-walk oracle, which also drops them) while still marshalling
/// every non-`#internal` field.
fn node_marked_internal(node: &Node) -> bool {
    node.directives
        .iter()
        .any(|d| d.name == relon_parser::directive::INTERNAL)
}

/// Classify a host-visible list-literal anon-Dict-return field into a
/// `List<elem>` [`TypeRepr`] by sniffing the element shape. Mirrors the
/// `Expr::List` arm of [`lower_expr`] (which picks `ConstListInt` /
/// `ConstListFloat` / `ConstListBool` / `ConstListString` from the same
/// first-element type). Only homogeneous scalar / String element lists
/// are accepted; empty lists (no element to type), mixed-type lists, and
/// lists of lists / schemas surface a loud error so an unmarshallable
/// field never silently disappears from the host output.
fn classify_anon_dict_list_field(
    items: &[Node],
    range: TokenRange,
    field_name: &str,
    resolver: &SchemaResolver<'_>,
    main_param_tys: &HashMap<&str, IrType>,
) -> Result<TypeRepr, LoweringError> {
    let Some(first) = items.first() else {
        return Err(cap!(
            "classify_anon_dict_list_field.unsupported_field_type.1",
            LoweringError::UnsupportedFieldType {
                schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                field: field_name.to_string(),
                ty: "empty list field — element type cannot be inferred for the return marshaller"
                    .to_string(),
                range,
            }
        ));
    };
    if let Some(variant_list_ty) =
        classify_anon_dict_variant_list_field(items, range, field_name, main_param_tys)?
    {
        return Ok(variant_list_ty);
    }
    if let Some(enum_list_ty) =
        classify_anon_dict_enum_list_field(items, range, field_name, resolver)?
    {
        return Ok(enum_list_ty);
    }
    let element = match &*first.expr {
        Expr::Int(_) => TypeRepr::Int,
        Expr::Float(_) => TypeRepr::Float,
        Expr::Bool(_) => TypeRepr::Bool,
        Expr::String(_) => TypeRepr::String,
        other => {
            return Err(cap!(
                "classify_anon_dict_list_field.unsupported_field_type.2",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!(
                    "list element `{}` — only homogeneous List<Int/Float/Bool/String> fields are \
                     marshalled in anon-Dict returns",
                    other.kind()
                ),
                    range,
                }
            ));
        }
    };
    // Enforce homogeneity up front so a mixed list (which `lower_expr`
    // would reject deeper, or worse mis-type) fails here with a precise
    // field name rather than a generic codegen error.
    for node in &items[1..] {
        let ok = matches!(
            (&element, &*node.expr),
            (TypeRepr::Int, Expr::Int(_))
                | (TypeRepr::Float, Expr::Float(_))
                | (TypeRepr::Bool, Expr::Bool(_))
                | (TypeRepr::String, Expr::String(_))
        );
        if !ok {
            return Err(cap!(
                "classify_anon_dict_list_field.unsupported_field_type.3",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!(
                        "heterogeneous list field (expected all {element:?} elements, found `{}`)",
                        node.expr.kind()
                    ),
                    range,
                }
            ));
        }
    }
    Ok(TypeRepr::List {
        element: Box::new(element),
    })
}

fn classify_anon_dict_enum_list_field(
    items: &[Node],
    range: TokenRange,
    field_name: &str,
    resolver: &SchemaResolver<'_>,
) -> Result<Option<TypeRepr>, LoweringError> {
    let Some(first) = items.first() else {
        return Ok(None);
    };
    let Some((enum_name, first_variant)) = enum_variant_literal_path(first.expr.as_ref()) else {
        return Ok(None);
    };
    let Some(def) = resolver.resolve(&enum_name) else {
        return Ok(None);
    };
    if def.variants.is_empty() {
        return Ok(None);
    }
    if !def.variants.iter().any(|v| v.name == first_variant) {
        return Err(cap!(
            "classify_anon_dict_enum_list_field.unsupported_field_type.1",
            LoweringError::UnsupportedFieldType {
                schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                field: field_name.to_string(),
                ty: format!("enum `{enum_name}` has no variant `{first_variant}`"),
                range: first.range,
            }
        ));
    }

    for node in &items[1..] {
        let Some((item_enum, item_variant)) = enum_variant_literal_path(node.expr.as_ref()) else {
            return Err(cap!(
                "classify_anon_dict_enum_list_field.unsupported_field_type.2",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!(
                        "heterogeneous list field: expected `{enum_name}` enum variants, found `{}`",
                        node.expr.kind()
                    ),
                    range: node.range,
                }
            ));
        };
        if item_enum != enum_name {
            return Err(cap!(
                "classify_anon_dict_enum_list_field.unsupported_field_type.3",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!(
                        "heterogeneous enum list field: expected `{enum_name}`, found `{item_enum}`"
                    ),
                    range: node.range,
                }
            ));
        }
        if !def.variants.iter().any(|v| v.name == item_variant) {
            return Err(cap!(
                "classify_anon_dict_enum_list_field.unsupported_field_type.4",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!("enum `{enum_name}` has no variant `{item_variant}`"),
                    range: node.range,
                }
            ));
        }
    }

    let mut stack: Vec<&str> = Vec::new();
    let enum_ty = canonical_enum_from_def(def, resolver, &mut stack, range)?;
    Ok(Some(TypeRepr::List {
        element: Box::new(enum_ty),
    }))
}

/// Classify a host-visible anon-Dict-return list-literal field whose
/// elements are built-in `Option` / `Result` variant constructors into a
/// `List<Option<T>>` / `List<Result<T, E>>` [`TypeRepr`]. The named-enum
/// counterpart ([`classify_anon_dict_enum_list_field`]) cannot reach these
/// because `Option` / `Result` are prelude sum types — `resolver.resolve`
/// returns `None` for them — so they fell through to the homogeneous-scalar
/// classifier and capped (`classify_anon_dict_list_field.unsupported_field_type.2`).
///
/// Once a concrete element type is recovered the field becomes a normal
/// `List<variant>` whose lowering already exists: the body walker routes the
/// list literal through the `variant_list_literal_for_type` pointer-array of
/// tagged variant records in `lower_dict_field_value`, returned via the
/// in-place region-walk ABI (verifier-gated). So the only missing piece was
/// recovering the payload type the declared-schema path gets for free from the
/// annotation.
///
/// The inner type is inferred by sniffing the scalar payload of a
/// payload-bearing variant (`Some { value }` / `Ok { value }` / `Err { error }`),
/// requiring homogeneity across the list. Shapes whose inner type cannot be
/// proven from the literal alone are left capped (returned as `Ok(None)` so the
/// caller's own loud cap fires, or a precise `Err` for an outright malformed
/// list):
///   * an all-`None` `Option` list (no `Some` payload to type the inner),
///   * a `Result` list missing either the `Ok` or the `Err` arm,
///   * a non-scalar / non-param payload expression,
///   * a heterogeneous payload type.
fn classify_anon_dict_variant_list_field(
    items: &[Node],
    range: TokenRange,
    field_name: &str,
    main_param_tys: &HashMap<&str, IrType>,
) -> Result<Option<TypeRepr>, LoweringError> {
    let Some(first) = items.first() else {
        return Ok(None);
    };
    let Some((enum_name, _)) = enum_variant_literal_path(first.expr.as_ref()) else {
        return Ok(None);
    };
    // Only the two built-in sum types are handled here; named user enums
    // stay on the resolver-backed path.
    let kind = match enum_name.as_str() {
        "Option" => VariantListKind::Option,
        "Result" => VariantListKind::Result,
        _ => return Ok(None),
    };

    // Accumulated payload scalar types per arm. `None` until a
    // payload-bearing element pins it down.
    let mut some_ty: Option<TypeRepr> = None; // Option.Some / Result.Ok
    let mut err_ty: Option<TypeRepr> = None; // Result.Err

    for node in items {
        let Expr::VariantCtor {
            enum_path,
            variant,
            body,
        } = &*node.expr
        else {
            return Err(cap!(
                "classify_anon_dict_variant_list_field.unsupported_field_type.1",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!(
                        "list element `{}` — expected a `{enum_name}` variant constructor",
                        node.expr.kind()
                    ),
                    range: node.range,
                }
            ));
        };
        if enum_path.join(".") != enum_name {
            return Err(cap!(
                "classify_anon_dict_variant_list_field.unsupported_field_type.2",
                LoweringError::UnsupportedFieldType {
                    schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                    field: field_name.to_string(),
                    ty: format!(
                        "heterogeneous variant list field: expected `{enum_name}`, found `{}`",
                        enum_path.join(".")
                    ),
                    range: node.range,
                }
            ));
        }
        // Determine which payload slot this variant feeds and its key.
        let payload_slot = match (kind, variant.as_str()) {
            (VariantListKind::Option, "None") => None,
            (VariantListKind::Option, "Some") => Some(("value", false)),
            (VariantListKind::Result, "Ok") => Some(("value", false)),
            (VariantListKind::Result, "Err") => Some(("error", true)),
            (_, other) => {
                return Err(cap!(
                    "classify_anon_dict_variant_list_field.unsupported_field_type.3",
                    LoweringError::UnsupportedFieldType {
                        schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                        field: field_name.to_string(),
                        ty: format!("`{enum_name}` has no variant `{other}`"),
                        range: node.range,
                    }
                ));
            }
        };
        let Some((key, is_err)) = payload_slot else {
            // Payload-free variant (`None`) — nothing to type.
            continue;
        };
        let payload_node =
            variant_payload_node(variant_body_pairs(body, node.range)?, key, node.range)?;
        let payload_ty =
            variant_payload_scalar_ty(&payload_node.expr, main_param_tys).ok_or_else(|| {
                cap!(
                    "classify_anon_dict_variant_list_field.unsupported_field_type.4",
                    LoweringError::UnsupportedFieldType {
                        schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                        field: field_name.to_string(),
                        ty: format!(
                            "`{enum_name}.{variant}` payload `{}` is not a scalar literal or \
                             scalar `#main` parameter — cannot type the variant list element",
                            payload_node.expr.kind()
                        ),
                        range: payload_node.range,
                    }
                )
            })?;
        let slot = if is_err { &mut err_ty } else { &mut some_ty };
        match slot {
            Some(existing) if *existing != payload_ty => {
                return Err(cap!(
                    "classify_anon_dict_variant_list_field.unsupported_field_type.5",
                    LoweringError::UnsupportedFieldType {
                        schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                        field: field_name.to_string(),
                        ty: format!(
                            "heterogeneous `{enum_name}` payload type: {existing:?} vs {payload_ty:?}"
                        ),
                        range: payload_node.range,
                    }
                ));
            }
            Some(_) => {}
            None => *slot = Some(payload_ty),
        }
    }

    match kind {
        VariantListKind::Option => {
            // All-`None` cannot pin the inner type from the literal alone.
            let Some(inner) = some_ty else {
                return Ok(None);
            };
            Ok(Some(TypeRepr::List {
                element: Box::new(TypeRepr::Option {
                    inner: Box::new(inner),
                }),
            }))
        }
        VariantListKind::Result => {
            // Need both arms present to type `Result<T, E>` fully.
            let (Some(ok), Some(err)) = (some_ty, err_ty) else {
                return Ok(None);
            };
            let _ = range;
            Ok(Some(TypeRepr::List {
                element: Box::new(TypeRepr::Result {
                    ok: Box::new(ok),
                    err: Box::new(err),
                }),
            }))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VariantListKind {
    Option,
    Result,
}

/// Recover the scalar [`TypeRepr`] of a variant payload expression. Only
/// shapes whose type is provable at classify time are accepted: scalar
/// literals and a bare `#main` scalar parameter reference. Anything else
/// (computed expressions, nested collections, schemas) returns `None` so the
/// caller caps loudly rather than guessing a layout.
fn variant_payload_scalar_ty(
    expr: &Expr,
    main_param_tys: &HashMap<&str, IrType>,
) -> Option<TypeRepr> {
    match expr {
        Expr::Int(_) => Some(TypeRepr::Int),
        Expr::Float(_) => Some(TypeRepr::Float),
        Expr::Bool(_) => Some(TypeRepr::Bool),
        Expr::String(_) => Some(TypeRepr::String),
        Expr::Variable(path) => {
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                return match main_param_tys.get(name.as_str())? {
                    IrType::I64 => Some(TypeRepr::Int),
                    IrType::F64 => Some(TypeRepr::Float),
                    IrType::Bool => Some(TypeRepr::Bool),
                    IrType::String => Some(TypeRepr::String),
                    _ => None,
                };
            }
            None
        }
        _ => None,
    }
}

fn enum_variant_literal_path(expr: &Expr) -> Option<(String, String)> {
    match expr {
        Expr::Variable(path) | Expr::FnCall { path, .. } => enum_variant_literal_token_path(path),
        Expr::VariantCtor {
            enum_path, variant, ..
        } => {
            if enum_path.is_empty() {
                None
            } else {
                Some((enum_path.join("."), variant.clone()))
            }
        }
        _ => None,
    }
}

fn enum_variant_literal_token_path(path: &[TokenKey]) -> Option<(String, String)> {
    let mut parts = Vec::with_capacity(path.len());
    for seg in path {
        match seg {
            TokenKey::String(s, _, _) => parts.push(s.clone()),
            _ => return None,
        }
    }
    if parts.len() < 2 {
        return None;
    }
    let variant = parts.pop()?;
    Some((parts.join("."), variant))
}

fn classify_anon_dict_scalar_field_irt(
    expr: &Expr,
    range: TokenRange,
    main_param_tys: &HashMap<&str, IrType>,
    closure_field_sigs: &HashMap<&str, (Vec<IrType>, IrType)>,
    dict_field_names: &HashSet<&str>,
    scalar_field_irts: &HashMap<&str, IrType>,
    field_name: &str,
) -> Result<IrType, LoweringError> {
    match expr {
        Expr::Int(_) => Ok(IrType::I64),
        Expr::Float(_) => Ok(IrType::F64),
        Expr::Bool(_) => Ok(IrType::Bool),
        Expr::String(_) => Ok(IrType::String),
        // R10/R13: a static sibling/root reference to another
        // host-visible field. At the entry-level dict (which IS the
        // document root) `&sibling.<name>` and `&root.<name>` resolve to
        // the same field, so both bases classify here. Classification
        // runs in topological order over the reference edges, so the
        // target field's type is in `scalar_field_irts` whether it is
        // declared earlier (backward) or later (forward). Only a single
        // static `String` trailing segment naming a host-visible scalar
        // *or list* field is accepted; positional bases
        // (Uncle/Prev/Next/Index/This), dynamic keys and multi-segment
        // paths fall through to the loud cap below.
        Expr::Reference {
            base: RefBase::Sibling | RefBase::Root,
            path,
        } => {
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                if let Some(t) = scalar_field_irts.get(name.as_str()) {
                    return Ok(*t);
                }
            }
            Err(cap!(
                "classify_anon_dict_scalar_field_irt.reference_unresolved",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "AnonDictReturn(field `{}`: sibling/root reference {:?} \
                         does not name a host-visible field)",
                        field_name, path
                    ),
                    range,
                }
            ))
        }
        Expr::Variable(path) => {
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                if let Some(t) = main_param_tys.get(name.as_str()) {
                    return Ok(*t);
                }
            }
            // W5-P3: `d[k]` — a sibling `{String -> Int}` dict field
            // indexed by a String key — classifies to the dict's `Int`
            // value type. The head must name a known dict field and the
            // single trailing segment must be a `Dynamic` (bracket)
            // index; `lower_dict_string_index` emits the actual probe.
            if let [TokenKey::String(name, _, _), TokenKey::Dynamic(_, optional)] = path.as_slice()
            {
                if !optional && dict_field_names.contains(name.as_str()) {
                    return Ok(IrType::I64);
                }
            }
            Err(cap!(
                "classify_anon_dict_scalar_field_irt.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "AnonDictReturn(field `{}`: cannot classify Variable({:?}))",
                        field_name, path
                    ),
                    range,
                }
            ))
        }
        Expr::FnCall { path, .. } => {
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                if let Some((_, ret_ty)) = closure_field_sigs.get(name.as_str()) {
                    return Ok(*ret_ty);
                }
            }
            // W5-P4: `result: list.sum(range(...)[.map|.filter]*)` — the
            // dict-probe workload's host-visible field. `list.sum` over a
            // range pipeline always yields an `Int` accumulator (the
            // peephole `emit_range_pipeline_loop` enforces an Int-valued
            // element and rejects otherwise), so classify the field as
            // I64; the actual loop + capture is lowered in `lower_expr`.
            if let [TokenKey::String(head, _, _), TokenKey::String(method, _, _)] = path.as_slice()
            {
                if head == "list" && method == "sum" {
                    return Ok(IrType::I64);
                }
            }
            Err(cap!(
                "classify_anon_dict_scalar_field_irt.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "AnonDictReturn(field `{}`: cannot classify FnCall({:?}) — \
                     only calls into previously-classified closure fields are \
                     supported at this surface)",
                        field_name, path
                    ),
                    range,
                }
            ))
        }
        Expr::Binary(_, lhs, rhs) => {
            // Conservative arithmetic propagation: both sides must
            // resolve to the same scalar IR type. Mixed Int/Float
            // promotes to Float (mirroring the runtime). String
            // concat (`+`) is recognised when both sides are String.
            let lt = classify_anon_dict_scalar_field_irt(
                &lhs.expr,
                lhs.range,
                main_param_tys,
                closure_field_sigs,
                dict_field_names,
                scalar_field_irts,
                field_name,
            )?;
            let rt = classify_anon_dict_scalar_field_irt(
                &rhs.expr,
                rhs.range,
                main_param_tys,
                closure_field_sigs,
                dict_field_names,
                scalar_field_irts,
                field_name,
            )?;
            match (lt, rt) {
                (IrType::I64, IrType::I64) => Ok(IrType::I64),
                (IrType::F64, IrType::F64)
                | (IrType::F64, IrType::I64)
                | (IrType::I64, IrType::F64) => Ok(IrType::F64),
                (IrType::Bool, IrType::Bool) => Ok(IrType::Bool),
                (IrType::String, IrType::String) => Ok(IrType::String),
                _ => Err(cap!(
                    "classify_anon_dict_scalar_field_irt.unsupported_expr.3",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                        "AnonDictReturn(field `{}`: binary with mixed scalar types {:?} / {:?})",
                        field_name, lt, rt
                    ),
                        range,
                    }
                )),
            }
        }
        _ => Err(cap!(
            "classify_anon_dict_scalar_field_irt.unsupported_expr.4",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "AnonDictReturn(field `{}`: unsupported value shape `{}`)",
                    field_name,
                    expr.kind()
                ),
                range,
            }
        )),
    }
}

/// Reverse of `type_repr_to_ir_type` for the host-visible anon-Dict
/// field types. Covers the scalar / String leaves plus the marshalled
/// scalar-element list types — the latter so a `&sibling.<list>` /
/// `&root.<list>` reference field classifies to the same `List<...>`
/// type as the field it aliases. Returns `None` for IR types that have
/// no anon-Dict-return canonical form (schemas, cross-region pointer
/// lists, closures, dicts).
fn ir_type_to_type_repr(t: IrType) -> Option<TypeRepr> {
    let list = |element: TypeRepr| {
        Some(TypeRepr::List {
            element: Box::new(element),
        })
    };
    match t {
        IrType::I64 => Some(TypeRepr::Int),
        IrType::F64 => Some(TypeRepr::Float),
        IrType::Bool => Some(TypeRepr::Bool),
        IrType::String => Some(TypeRepr::String),
        IrType::Unit => Some(TypeRepr::Unit),
        IrType::ListInt => list(TypeRepr::Int),
        IrType::ListFloat => list(TypeRepr::Float),
        IrType::ListBool => list(TypeRepr::Bool),
        IrType::ListString => list(TypeRepr::String),
        _ => None,
    }
}

/// Phase F.2 (W7): body walker for the anon-Dict-return path. Walks
/// the dict literal in declaration order; each entry is either a
/// closure-field let-binding (no host-visible store) or a scalar
/// field store into the root record.
///
/// Closure fields are pre-registered as `IrType::Closure` let-locals
/// **before** their body lowers — this gives recursive self-calls
/// (W7's `fib(k - 1)` inside `fib`'s body) a stable let slot to
/// `LetGet` off and consume via `Op::CallClosure`.
fn lower_anon_dict_body(
    plan: &AnonDictPlan,
    layout: &OffsetTable,
    dict_pairs: &[(TokenKey, Node)],
    record_local: u32,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Build a name → user-supplied Node map so we can pull each
    // classified plan field's value back out of the source dict.
    let mut user_values: HashMap<&str, &Node> = HashMap::new();
    for (key, value) in dict_pairs {
        if let TokenKey::String(name, _, _) = key {
            user_values.insert(name.as_str(), value);
        }
    }

    // Resolve each host-visible scalar / cross-region field's layout
    // slot by name. The layout walks `schema.fields` in declaration
    // order; looking the slot up by name (rather than a running index)
    // lets the body walker emit fields in topological order — needed so
    // a forward `&sibling` / `&root` reference's target field is already
    // bound — without disturbing the record offset each value stores to.
    let layout_field_by_name = |name: &str| layout.fields.iter().find(|f| f.name == name);

    // R13: emit fields in topological order over their reference edges
    // (see `AnonDictPlan::emit_order`). Backward-only / reference-free
    // bodies keep declaration order, so the pre-existing byte output is
    // unchanged.
    for &field_idx in &plan.emit_order {
        let plan_field = &plan.fields[field_idx];
        match plan_field {
            AnonDictField::Closure {
                name,
                param_tys,
                ret_ty,
                concat_coercible,
            } => {
                let value = user_values.get(name.as_str()).copied().ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.1",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(missing source value for closure field `{}`)",
                                name
                            ),
                            range: TokenRange::default(),
                        }
                    )
                })?;
                // Pre-allocate the let-idx the closure handle will
                // land in. Registered before the body lowers so a
                // recursive `Variable(name)` inside the body resolves
                // to `LetGet { idx, Closure }`.
                let let_idx = ctx.next_let_idx;
                ctx.next_let_idx += 1;
                ctx.lets.push(LetBinding {
                    name: name.clone(),
                    idx: let_idx,
                    ty: IrType::Closure,
                    schema_brand: None,
                    type_repr: None,
                });
                ctx.closure_let_signatures
                    .insert(let_idx, (param_tys.clone(), *ret_ty));
                if concat_coercible.iter().any(|&c| c) {
                    ctx.closure_concat_coercible
                        .insert(let_idx, concat_coercible.clone());
                }

                // Lower the closure body — pushes `IrType::Closure` on
                // top of the vstack and appends the lambda to
                // `ctx.lambda_funcs`.
                lower_closure_as_value(&value.expr, value.range, param_tys, *ret_ty, ctx)?;

                // Stash the handle into the pre-allocated let-local.
                let popped = ctx.tstack.pop().ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.2",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(closure field `{}` produced no value)",
                                name
                            ),
                            range: value.range,
                        }
                    )
                })?;
                debug_assert_eq!(popped, IrType::Closure);
                ctx.out.push(TaggedOp {
                    op: Op::LetSet {
                        idx: let_idx,
                        ty: IrType::Closure,
                    },
                    range: value.range,
                });
            }
            AnonDictField::DictStrInt { name, entries } => {
                // W5-P1: materialise the `{str:int}` dict into the const
                // pool via `Op::ConstDict` and stash the arena pointer
                // into a fresh `IrType::Dict` internal let-local. This
                // is the construction + capture half; the read half
                // (`DictGetByStringKey`) is a P3 follow-up, so the
                // let-local is value-only for now.
                let let_idx = ctx.next_let_idx;
                ctx.next_let_idx += 1;
                ctx.lets.push(LetBinding {
                    name: name.clone(),
                    idx: let_idx,
                    ty: IrType::Dict,
                    schema_brand: None,
                    type_repr: None,
                });
                let dict_idx = ctx.const_intern.borrow_mut().alloc_dict_idx();
                ctx.out.push(TaggedOp {
                    op: Op::ConstDict {
                        idx: dict_idx,
                        entries: entries.clone(),
                    },
                    range: TokenRange::default(),
                });
                ctx.out.push(TaggedOp {
                    op: Op::LetSet {
                        idx: let_idx,
                        ty: IrType::Dict,
                    },
                    range: TokenRange::default(),
                });
            }
            AnonDictField::ListString { name, elements } => {
                // W5-P4: materialise the `["a", ...]` list into the const
                // pool via `Op::ConstListString` and stash the arena
                // pointer into a fresh `IrType::ListString` internal
                // let-local. Captured by a later sibling `result` field
                // (the map-loop body `keys[i % 10]`); the let-binding is
                // registered before `result` lowers so `Variable(keys)`
                // resolves to `LetGet { idx, ListString }`.
                let let_idx = ctx.next_let_idx;
                ctx.next_let_idx += 1;
                ctx.lets.push(LetBinding {
                    name: name.clone(),
                    idx: let_idx,
                    ty: IrType::ListString,
                    schema_brand: None,
                    type_repr: None,
                });
                let list_idx = ctx.const_intern.borrow_mut().alloc_list_string_idx();
                ctx.out.push(TaggedOp {
                    op: Op::ConstListString {
                        idx: list_idx,
                        elements: elements.clone(),
                    },
                    range: TokenRange::default(),
                });
                ctx.out.push(TaggedOp {
                    op: Op::LetSet {
                        idx: let_idx,
                        ty: IrType::ListString,
                    },
                    range: TokenRange::default(),
                });
            }
            AnonDictField::Scalar { name, ty } => {
                let value = user_values.get(name.as_str()).copied().ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.3",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(missing source value for scalar field `{}`)",
                                name
                            ),
                            range: TokenRange::default(),
                        }
                    )
                })?;
                let expected_ir = type_repr_to_ir_type(ty)?;
                let is_variant_list_literal = variant_list_literal_for_type(ty, &value.expr);
                // Same pointer-array-list provenance guard as the
                // top-level and branded-struct return paths. `List<String>`
                // still needs the const-pool path; `List<Enum>` source
                // literals are constructed directly in the output tail by
                // `BuildPointerList`, so they are also safe here.
                if pointer_array_list_ir_type(expected_ir)
                    && !pointer_array_list_source_is_const_pool(&value.expr)
                    && !is_variant_list_literal
                {
                    return Err(cap!(
                        "lower_anon_dict_body.unsupported_field_type.1",
                        LoweringError::UnsupportedFieldType {
                            schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                            field: name.clone(),
                            ty: format!(
                            "{expected_ir:?} sourced from `{}` — pointer-array list fields are                              only marshalled from in-source list literals",
                            value.expr.kind()
                        ),
                            range: value.range,
                        }
                    ));
                }
                if is_variant_list_literal {
                    lower_value_as_type(ty, value, ctx)?;
                } else {
                    lower_expr(&value.expr, value.range, ctx)?;
                }
                let top = ctx.tstack.pop().ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.4",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(scalar field `{}` produced no value)",
                                name
                            ),
                            range: value.range,
                        }
                    )
                })?;
                if top.wasm_slot() != expected_ir.wasm_slot() {
                    return Err(cap!(
                        "lower_anon_dict_body.unsupported_field_type.2",
                        LoweringError::UnsupportedFieldType {
                            schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                            field: name.clone(),
                            ty: format!("expected {:?}, got {:?}", expected_ir, top),
                            range: value.range,
                        }
                    ));
                }
                // R10: stash this scalar field's value in an internal
                // let-local so a *later* sibling field can read it back
                // with `&sibling.<name>` / `&root.<name>` (the entry dict
                // IS the document root, so both bases resolve to the same
                // field). The let holds the field's natural scalar value
                // (the same value `lower_variable` would `LetGet`); the
                // pointer-indirect tail-emit, if any, happens below on the
                // value we re-load — so the reference and the stored field
                // observe a bit-identical value. Source order makes the
                // binding visible only to fields declared after it, which
                // is exactly the backward-only contract the reference arm
                // in `lower_expr` enforces. Registered for every
                // host-visible scalar field (including String); a forward
                // or positional reference simply never finds it and caps.
                let field_let_idx = ctx.next_let_idx;
                ctx.next_let_idx += 1;
                ctx.lets.push(LetBinding {
                    name: name.clone(),
                    idx: field_let_idx,
                    ty: expected_ir,
                    schema_brand: None,
                    type_repr: None,
                });
                ctx.out.push(TaggedOp {
                    op: Op::LetSet {
                        idx: field_let_idx,
                        ty: expected_ir,
                    },
                    range: value.range,
                });
                ctx.out.push(TaggedOp {
                    op: Op::LetGet {
                        idx: field_let_idx,
                        ty: expected_ir,
                    },
                    range: value.range,
                });
                let layout_field = layout_field_by_name(name).ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.5",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(scalar field `{}`: no matching layout slot)",
                                name
                            ),
                            range: value.range,
                        }
                    )
                })?;
                debug_assert_eq!(&layout_field.name, name);
                // Pointer-indirect fields (String / List<scalar> /
                // List<String>) push an *absolute* arena address from
                // `lower_expr` (a `ConstString` / `ConstList*` record or
                // a `Load*Ptr` param). They must be copied into the
                // return buffer's tail area first — the fixed-area slot
                // stores a *buffer-relative* offset, not the absolute
                // address. This mirrors the branded-dict path
                // (`lower_dict_field_value`): emit
                // `EmitTailRecordFromAbsoluteAddr { ty }` to perform the
                // copy, then store the resulting i32 offset. Without
                // this the slot held the raw arena pointer and the host
                // reader dereferenced garbage → a silent empty
                // String / List (the W7-anon-dict mis-compile).
                let store_ty = if is_variant_list_literal {
                    expected_ir
                } else if pointer_indirect_ir_type(expected_ir) {
                    ctx.out.push(TaggedOp {
                        op: Op::EmitTailRecordFromAbsoluteAddr { ty: expected_ir },
                        range: value.range,
                    });
                    IrType::I32
                } else {
                    expected_ir
                };
                ctx.out.push(TaggedOp {
                    op: Op::StoreFieldAtRecord {
                        record_local_idx: record_local,
                        offset: layout_field.offset as u32,
                        ty: store_ty,
                    },
                    range: value.range,
                });
            }
            AnonDictField::CrossRegionParamList { name, ty, .. } => {
                // F1b: cross-region object field. The value is a `#main`
                // parameter identity of `List<Schema>` / `List<List<scalar>>`
                // type whose data lives in the *input* region; the object
                // head is in the *output* region. We do NOT copy the data
                // into the output tail (that is exactly what the old
                // `EmitTailRecordFromAbsoluteAddr` cap rejected as
                // unsupported for these pointer-array element lists, and a
                // copy would also lose the cross-region link).
                //
                // Under the F1 arena-absolute slot convention every pointer
                // slot stores an arena-absolute u32 offset. `lower_expr`
                // over the parameter identity emits `LoadListSchemaPtr` /
                // `LoadListListPtr`, which post-F1 push the parameter list
                // root header's arena-absolute offset (the input marshaller
                // baked `in_ptr` into the slot). We store that offset
                // directly into the object's field slot via
                // `StoreFieldAtRecord { ty: ListSchema / ListList }`. The
                // host's multi-region verifier (which the object positive-
                // `bytes_written` path now always runs) reads the slot,
                // classifies the offset into the input region, bounds-checks
                // the whole reachable graph, and only then does the reader
                // follow it cross-region — bit-equal to the tree-walk
                // oracle. An offset that classifies to no region / runs off
                // its region is a loud verifier error; the decode never runs.
                let value = user_values.get(name.as_str()).copied().ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.6",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(missing source value for cross-region field `{}`)",
                                name
                            ),
                            range: TokenRange::default(),
                        }
                    )
                })?;
                let expected_ir = type_repr_to_ir_type(ty)?;
                // F1b admitted `ListSchema` / `ListList`; F3 widens to the
                // pointer-array `ListString` and the inline-fixed scalar
                // lists (`ListInt` / `ListFloat` / `ListBool`). Every one is
                // a cross-region list whose param root slot stores an
                // arena-absolute offset (the value `lower_expr` pushes via
                // the matching `LoadList*Ptr`).
                debug_assert!(matches!(
                    expected_ir,
                    IrType::ListSchema
                        | IrType::ListList
                        | IrType::ListString
                        | IrType::ListInt
                        | IrType::ListFloat
                        | IrType::ListBool
                ));
                lower_expr(&value.expr, value.range, ctx)?;
                let top = ctx.tstack.pop().ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.7",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(cross-region field `{}` produced no value)",
                                name
                            ),
                            range: value.range,
                        }
                    )
                })?;
                if top != expected_ir {
                    return Err(cap!(
                        "lower_anon_dict_body.unsupported_field_type.3",
                        LoweringError::UnsupportedFieldType {
                            schema: MAIN_RETURN_SCHEMA_NAME.to_string(),
                            field: name.clone(),
                            ty: format!(
                                "cross-region field expected {:?}, got {:?}",
                                expected_ir, top
                            ),
                            range: value.range,
                        }
                    ));
                }
                let layout_field = layout_field_by_name(name).ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.8",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(cross-region field `{}`: no matching layout slot)",
                                name
                            ),
                            range: value.range,
                        }
                    )
                })?;
                debug_assert_eq!(&layout_field.name, name);
                // Store the parameter list root's arena-absolute offset
                // straight into the slot — no tail copy, the cross-region
                // link is the point.
                ctx.out.push(TaggedOp {
                    op: Op::StoreFieldAtRecord {
                        record_local_idx: record_local,
                        offset: layout_field.offset as u32,
                        ty: expected_ir,
                    },
                    range: value.range,
                });
            }
        }
    }

    Ok(())
}

/// Name reserved for the anonymous positional-record schema that
/// represents a compiled `Tuple<...>` boundary. Distinct from `MainParams` / `Ret` so
/// the backends and the host decode never confuse a tuple record with a
/// scalar-wrapper or branded return.
pub const TUPLE_RETURN_SCHEMA_NAME: &str = "Tuple";

fn is_removed_unit_null_type_name(name: &str) -> bool {
    matches!(name, "Null" | "Unit")
}

fn maybe_optional(t: &TypeNode, base: TypeRepr) -> TypeRepr {
    if t.is_optional {
        TypeRepr::Option {
            inner: Box::new(base),
        }
    } else {
        base
    }
}

/// Convert a `Tuple<...>` type node into the anonymous positional schema used
/// by compiled host boundaries. Element types are converted recursively, so
/// tuples can contain tuples, lists, options, results, and named schemas; the
/// layout pass remains responsible for rejecting shapes a backend cannot yet
/// materialise.
fn tuple_type_node_to_schema(
    t: &TypeNode,
    resolver: Option<&SchemaResolver<'_>>,
) -> Option<Schema> {
    if t.path.len() != 1 || t.path[0].as_str() != "Tuple" || t.variant_fields.is_some() {
        return None;
    }
    let mut elements = Vec::with_capacity(t.generics.len());
    for g in &t.generics {
        let elem = match resolver {
            Some(r) => type_node_to_canonical_with_schemas(g, r)?,
            None => type_node_to_canonical(g)?,
        };
        elements.push(elem);
    }
    Some(Schema::tuple(TUPLE_RETURN_SCHEMA_NAME, elements))
}

/// Convert a `Tuple<...>` return type into the same positional schema used for
/// tuple parameters. A non-tuple head returns `None`; a tuple with an element
/// type that cannot be represented canonically returns an explicit lowering
/// error at the element span.
fn return_tuple_canonical(
    t: &TypeNode,
    resolver: &SchemaResolver<'_>,
) -> Option<Result<Schema, LoweringError>> {
    if t.path.len() != 1 || t.path[0].as_str() != "Tuple" || t.variant_fields.is_some() {
        return None;
    }
    let mut elements = Vec::with_capacity(t.generics.len());
    for g in &t.generics {
        let Some(elem) = type_node_to_canonical_with_schemas(g, resolver) else {
            return Some(Err(cap!(
                "return_tuple_canonical.unsupported_type_in_main",
                LoweringError::UnsupportedTypeInMain {
                    type_name: format!("Tuple element `{}`", type_head_for_display(g)),
                    range: g.range,
                }
            )));
        };
        elements.push(elem);
    }
    Some(Ok(Schema::tuple(TUPLE_RETURN_SCHEMA_NAME, elements)))
}

/// Map a parsed builtin type to a canonical [`TypeRepr`] without resolving
/// user schemas. This keeps the scalar/list paths usable in places that do not
/// have a schema resolver, while still allowing normal recursive builtin
/// nesting such as `List<Tuple<Int, String>>` and `Option<List<Int>>`.
fn type_node_to_canonical(t: &TypeNode) -> Option<TypeRepr> {
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
            element: Box::new(type_node_to_canonical(elem)?),
        },
        ("Option", [inner]) => TypeRepr::Option {
            inner: Box::new(type_node_to_canonical(inner)?),
        },
        ("Result", [ok, err]) => TypeRepr::Result {
            ok: Box::new(type_node_to_canonical(ok)?),
            err: Box::new(type_node_to_canonical(err)?),
        },
        ("Tuple", _) => TypeRepr::Schema {
            schema: Box::new(tuple_type_node_to_schema(t, None)?),
        },
        _ => return None,
    };

    Some(maybe_optional(t, base))
}

/// Return-side canonicalizer for the `List<List<…>>` return type
/// (`#main(...) -> List<List<Int|Float|Bool|String|Schema|List<…>>>`). The
/// scalar [`type_node_to_canonical`] only accepts a single level of list,
/// so a nested-list **return** head falls through to here.
///
/// S1/S2 admitted the inline-fixed scalar inner (`List<List<Int>>`); F5
/// widens this to the doubly-nested **pointer-array** shapes
/// (`List<List<String>>` / `List<List<Schema>>`, and deeper
/// `List<List<List<…>>>`): the recursive input marshaller, relocation
/// walker, multi-region verifier, and in-place reader all decode them
/// bit-equal. The outer head must be `List<…>` whose generic is itself a
/// `List<…>`; the inner is resolved through the schema-aware
/// canonicaliser so a `List<List<Schema>>` inner schema lookup succeeds.
/// The layout pass (`inner_list_record_alignment`) is the final arbiter of
/// which inner element types are materialisable — an unsupported leaf
/// (Option / Result / Closure) is still a loud cap there.
///
/// Return-only: parameter canonicalisation already handles nested lists
/// via [`type_node_to_canonical_with_schemas`], and widening the shared
/// scalar canonicalizer would mis-accept nested lists in unrelated
/// surfaces (native-fn signatures, etc.).
///
/// A [`SchemaResolver`] (`Some`) lets a `List<List<Schema>>` return type
/// resolve its inner user schema; `None` resolves only scalar / String
/// inner elements via the resolver-free [`type_node_to_canonical`].
fn return_nested_list_canonical(
    t: &TypeNode,
    resolver: Option<&SchemaResolver<'_>>,
) -> Option<TypeRepr> {
    if t.path.len() != 1
        || t.path[0].as_str() != "List"
        || t.generics.len() != 1
        || t.variant_fields.is_some()
    {
        return None;
    }
    // Outer is `List<…>`; the generic must itself be a `List<…>`.
    let inner = match resolver {
        Some(r) => type_node_to_canonical_with_schemas(&t.generics[0], r)?,
        None => type_node_to_canonical(&t.generics[0])?,
    };
    match &inner {
        TypeRepr::List { .. } => Some(TypeRepr::List {
            element: Box::new(inner),
        }),
        _ => None,
    }
}

/// Canonicalise a `-> List<Schema>` return type (S4). The outer head must
/// be `List<…>` with exactly one generic that names a user `#schema`; the
/// inner schema is resolved through the schema-aware canonicaliser. A
/// `List<List<…>>` or `List<scalar>` / `List<String>` generic returns
/// `None` here so those keep their own dedicated paths (the scalar/string
/// list canonicalisers, or the loud cap for nested schema lists).
///
/// Scoped narrowly to the single `List<Schema>` shape: the inner element
/// must be a `TypeRepr::Schema`, never itself a `List`. That keeps
/// `List<List<Schema>>` (a pointer-array-of-pointer-array the in-place
/// reader does not decode) rejected as `UnsupportedTypeInMain`.
fn return_list_schema_canonical(t: &TypeNode, resolver: &SchemaResolver<'_>) -> Option<TypeRepr> {
    if t.path.len() != 1
        || t.path[0].as_str() != "List"
        || t.generics.len() != 1
        || t.variant_fields.is_some()
    {
        return None;
    }
    let inner = type_node_to_canonical_with_schemas(&t.generics[0], resolver)?;
    match &inner {
        TypeRepr::Schema { .. } => Some(TypeRepr::List {
            element: Box::new(inner),
        }),
        _ => None,
    }
}

/// True when an [`IrType`] is materialised through a buffer-relative
/// pointer slot rather than stored inline in a record's fixed area —
/// String and every `List<_>` variant. Such fields require an
/// `EmitTailRecordFromAbsoluteAddr` copy into the return buffer's tail
/// before the fixed-area slot can receive the (buffer-relative) offset.
fn pointer_indirect_ir_type(t: IrType) -> bool {
    matches!(
        t,
        IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::ListList
    )
}

/// True when `t` is a **pointer-array** list type (`List<String>` /
/// `List<Schema>` / `List<List<_>>`) — i.e. its tail record is a
/// `[len][off_0]…[off_{N-1}]` header whose entries point at *further*
/// records carrying their own inner pointers.
///
/// These are the shapes the return marshaller cannot relocate
/// correctly for an arbitrary source: the compiled return path copies
/// the source block with a single rigid `delta` (see the cranelift
/// `copy_list_string_block` / `emit_tail_record_from_absolute`), which
/// is only sound when the whole reachable block is **contiguous** and
/// its inner offsets share one base — the layout the const-pool
/// `Op::ConstListString` emits. A pointer-array list sourced from a
/// `#main` parameter (or any non-const-pool producer) lives in the
/// input buffer with whole-buffer-relative, non-contiguous offsets;
/// feeding it through the rigid-block copy reads `off_0` as the block
/// start and computes a bogus span, segfaulting or returning corrupt
/// data. `List<Int/Float/Bool>` are *not* pointer-array (their payload
/// is one inline-fixed `[len][payload]` record), so identity-return of
/// a scalar-list param stays correct and is excluded here.
fn pointer_array_list_ir_type(t: IrType) -> bool {
    matches!(
        t,
        IrType::ListString | IrType::ListSchema | IrType::ListList
    )
}

/// True when `expr` deterministically lowers to a **const-pool**
/// pointer-array list record — today only a list literal whose every
/// element is a String literal (`["a", "b", …]` → `Op::ConstListString`).
/// That is the one provenance whose tail block is contiguous and
/// single-base, so the rigid-block return copy is provably correct.
///
/// Everything else that can yield a `ListString` value at runtime — a
/// `#main` parameter reference, a field/index load, a function call, a
/// comprehension — produces a block the rigid copy cannot relocate
/// (see [`pointer_array_list_ir_type`]). The return marshaller must
/// reject those loudly rather than emit the silent-miscompile / segfault
/// path. `List<Schema>` / `List<List<_>>` have *no* const-pool producer
/// at all, so this returns `false` for every source carrying them — they
/// stay a loud cap unconditionally.
fn pointer_array_list_source_is_const_pool(expr: &Expr) -> bool {
    let Expr::List(items) = expr else {
        return false;
    };
    // An all-String-literal list lowers to `Op::ConstListString`. A
    // mixed / empty / nested list does not reach a pointer-array
    // `ListString` return (it types as a scalar list or fails the
    // element classifier earlier), so requiring String literals here is
    // exactly the const-`ListString` provenance.
    !items.is_empty() && items.iter().all(|n| matches!(&*n.expr, Expr::String(_)))
}

/// True when `expr` is a bare `#main` parameter **identity** reference
/// (`xss` / `ss`, a single-segment `Expr::Variable`) — the value lowers
/// to a single `Load*Ptr` (`LoadListListPtr` / `LoadListStringPtr`) that
/// pushes the input-region root header's arena-relative offset.
///
/// This is the trigger for the in-place region-walk return ABI (S1/S2
/// `List<List<scalar>>`, S3 `List<String>`, S4 `List<Schema>`): the
/// pointer-array value is self-contained in the input buffer and its
/// outer/inner layout is exactly what the host writer emitted, so both
/// AOT backends report the root's arena-absolute offset and the host
/// verifies + decodes it in place — bit-equal to the tree-walk oracle,
/// including every string's bytes and every sub-record field.
///
/// A parameter-**field** walk (`o.tags` / `o.items` / `o.grid`) is the
/// F4 sibling, handled by [`pointer_array_param_field_walk`]. Post-F1 the
/// field-load no longer re-encodes the inner form: every pointer slot is
/// arena-absolute (the input marshaller bakes `in_ptr` recursively), so
/// `LoadFieldAtAbsolute` pushes the field list root's arena-absolute
/// offset directly — bit-equal to tree-walk under the same single-root
/// sentinel + verifier + reader the identity walk uses. Everything else
/// (a list literal, comprehension, call, binary expression) is not a
/// param walk and keeps the existing const-pool / loud-cap paths.
fn pointer_array_param_identity_walk(expr: &Expr) -> bool {
    matches!(expr, Expr::Variable(path) if path.len() == 1)
}

/// Wave R3c: true when `expr` is a list higher-order call (`map` /
/// `filter`, in method form `xs.map(f)` or free form `_list_map(xs, f)` /
/// `_list_filter(xs, f)` — the latter is also what a list comprehension
/// desugars to) that, with a `List<String>` declared `#main` return,
/// lowers to one of the R3c String-result bundled bodies
/// (`list_string_map` / `list_int_map_to_string` /
/// `list_float_map_to_string` / `list_string_filter`).
///
/// These bodies build the result `List<String>` pointer-array record in
/// the **scratch** region, and every `off_i` slot they write is an
/// arena-absolute String handle the closure already produced (a const-pool
/// literal or a scratch-built `StrConcatN` / `IntToStr` record — all in the
/// same flat arena). The record is therefore self-contained under the
/// single global arena-relative pointer convention, exactly like a
/// param-sourced pointer array, so it qualifies for the **in-place
/// region-walk return** ABI: the backend reports the root header's
/// arena-absolute offset via the negative sentinel and the host
/// verifies + decodes it in place (over the scratch region) — no rigid
/// block copy / relocation, which is what made the old `List<String>`
/// computed-return path unsound.
///
/// Caller gates this on `ret_ir_ty == IrType::ListString`; we only need to
/// confirm the source is one of the String-result HOF surfaces (the
/// closure-return-type probe in `emit_list_hof_call` is what actually
/// selects a String-result body, so a `map` returning a numeric list never
/// reaches here with a `ListString` return type).
fn string_result_list_hof_call(expr: &Expr) -> bool {
    // A list comprehension `[ element for id in src (if cond)? ]` desugars
    // in `lower_comprehension` onto the same `list_*_map` bundled body the
    // method / free forms use, so a `List<String>` comprehension result is
    // the same self-contained scratch pointer-array record (every `off_i`
    // an arena-absolute String handle). The caller gates on
    // `ret_ir_ty == IrType::ListString`, which a comprehension only reaches
    // by lowering through a String-result map body — the numeric / inline
    // shapes never produce a `ListString` return — so the in-place
    // region-walk return ABI applies unchanged.
    if matches!(expr, Expr::Comprehension { .. }) {
        return true;
    }
    let Expr::FnCall { path, .. } = expr else {
        return false;
    };
    // Free form / comprehension desugaring: `_list_map(...)` /
    // `_list_filter(...)` — the leading segment names the builtin.
    if let [TokenKey::String(name, _, _), ..] = path.as_slice() {
        if name == "_list_map" || name == "_list_filter" {
            return true;
        }
    }
    // Method form: `xs.map(f)` / `xs.filter(f)` — the trailing segment
    // names the method.
    matches!(
        path.last(),
        Some(TokenKey::String(m, _, _)) if m == "map" || m == "filter"
    )
}

/// Wave R15: true when `expr` is a `split` call on a String receiver
/// (`s.split(sep)`) that, with a `List<String>` declared `#main` return,
/// lowers to the `split` bundled body. That body builds the result
/// `List<String>` pointer-array record (header + per-segment String
/// records) entirely in the **scratch** region; every `off_i` slot is an
/// arena-absolute handle to a self-contained segment record, so the
/// result qualifies for the in-place region-walk return ABI exactly like
/// the R3c String-result HOF results (see [`string_result_list_hof_call`]).
///
/// The free-call wrapper `_string_split(s, sep)` is a tree-walk-only
/// surface (it routes through the evaluator's `std/string` module, not a
/// lowered free-call body), so only the method form reaches here. Caller
/// gates this on `ret_ir_ty == IrType::ListString`, which a `String`
/// receiver's `split` always produces.
fn string_split_call(expr: &Expr) -> bool {
    let Expr::FnCall { path, .. } = expr else {
        return false;
    };
    matches!(
        path.last(),
        Some(TokenKey::String(m, _, _)) if m == "split"
    )
}

/// F4: detect a two-segment `#main` parameter **field** walk (`o.tags`
/// where `o: Outer` is a schema-typed param and `tags: List<…>` is a
/// pointer-array list field), returning the field's canonical `TypeRepr`
/// when matched. The field type is resolved through `main_schema` (the
/// param schema set, element schemas inlined).
///
/// Admission mirrors the identity walk envelope ([`anon_dict_cross_region_param_list`]):
/// `List<String>`, `List<Int|Float|Bool>` (inline-fixed scalar list),
/// `List<List<scalar>>` (nested-scalar pointer array), and `List<Schema>`
/// confined to the S4 sub-record decode scope. A deeper nesting
/// (`List<List<String|Schema>>`) or a non-list / non-pointer-array field
/// returns `None`, so the caller keeps it a loud cap.
///
/// Why this is safe (the F1 flip resolved the S3/S4 rebase cap): the
/// field-load path no longer materialises a re-encoded inner form. The
/// slot at `param_root + field_offset` holds an arena-absolute u32 (the
/// input marshaller's recursive `finish_arena_absolute` relocated it),
/// and `LoadFieldAtAbsolute` loads it verbatim. That offset IS the field
/// list root the verifier classifies (into the input region) and the
/// reader follows cross-region — proven byte-equal to the tree-walk
/// oracle on cranelift / llvm / wasm. F6 generalises this from a single
/// field segment to an arbitrary-depth chain (`o.inner.tags`,
/// `o.a.b.tags`) via [`cross_region_param_field_chain`]: every
/// intermediate segment must be a nested-schema field.
fn pointer_array_param_field_walk<'s>(
    expr: &Expr,
    main_schema: &'s Schema,
) -> Option<&'s TypeRepr> {
    let Expr::Variable(path) = expr else {
        return None;
    };
    let [TokenKey::String(p, _, _), rest @ ..] = path.as_slice() else {
        return None;
    };
    if rest.is_empty() {
        return None;
    }
    let param = main_schema.fields.iter().find(|fl| &fl.name == p)?;
    cross_region_param_field_chain(&param.ty, rest)
}

/// Resolve a `#main` parameter **field chain** (`o.inner.tags`,
/// `o.a.b.tags`, or the single-segment `o.tags`) down to its leaf
/// field's canonical [`TypeRepr`], returning it only when every
/// intermediate segment is a nested-schema field and the leaf field is a
/// pointer-array list inside the cross-region admission envelope
/// ([`cross_region_list_envelope`]).
///
/// `base` is the canonical type the head identifier resolves to (the
/// param's own type, with element schemas inlined); `segs` are the
/// remaining `.field` segments after the head. Each non-leaf segment must
/// name a `TypeRepr::Schema` field so the walk can descend into the
/// sub-record's field set; the leaf segment's type must satisfy
/// `cross_region_list_envelope`.
///
/// Why a deep chain is as safe as the single-segment F4 walk: the
/// `lower_variable` walker already emits one `LoadFieldAtAbsolute` per
/// segment, and post-F1 *every* pointer slot in the input region is
/// arena-absolute. An intermediate nested-schema field load therefore
/// pushes the sub-record's arena-absolute base, and the leaf list-field
/// load reads the list root's arena-absolute offset off that base —
/// exactly the single-root sentinel + multi-region verifier + reader
/// value the F4 / identity walks consume. No re-encode happens at any
/// link, so the host decode is unchanged and the result is byte-equal to
/// the tree-walk oracle at any depth. A non-schema intermediate segment
/// or an out-of-envelope leaf returns `None`, keeping it a loud cap.
fn cross_region_param_field_chain<'s>(
    base: &'s TypeRepr,
    segs: &[TokenKey],
) -> Option<&'s TypeRepr> {
    let mut current = base;
    let last = segs.len().checked_sub(1)?;
    for (i, seg) in segs.iter().enumerate() {
        let TokenKey::String(name, _, _) = seg else {
            return None;
        };
        let TypeRepr::Schema { schema } = current else {
            return None;
        };
        let field = schema.fields.iter().find(|fl| &fl.name == name)?;
        if i == last {
            return cross_region_list_envelope(&field.ty).then_some(&field.ty);
        }
        current = &field.ty;
    }
    None
}

/// The cross-region list-field admission envelope, shared by every
/// classifier that decides whether a parameter-sourced `List<…>` value
/// (identity `servers` or field walk `o.tags`) is one the host's
/// multi-region verifier + reader can follow cross-region:
///   * `List<String>` — pointer array of String records,
///   * `List<Int|Float|Bool>` — inline-fixed scalar list,
///   * `List<List<scalar>>` — nested-scalar pointer array (inner element
///     must be an inline-fixed scalar; a deeper nesting or String / Schema
///     inner element is out of scope),
///   * `List<Schema{…}>` confined to the S4 sub-record decode scope, or
///   * `List<Option|Result|Enum>` variant-record elements.
///     (`list_schema_subrecord_in_s4_scope`).
///
/// F5 widens this to the double pointer array `List<List<String>>` /
/// `List<List<Schema>>` (and deeper nested lists): the recursive input
/// marshaller, relocation walker, multi-region verifier, and in-place
/// reader all follow them cross-region bit-equal. Variant-record elements
/// (`Option` / `Result` / custom `#enum`) use the same pointer-array path.
/// A leaf type the layout pass cannot materialise (Closure) returns `false`,
/// keeping it a loud cap.
fn cross_region_list_envelope(ty: &TypeRepr) -> bool {
    let TypeRepr::List { element } = ty else {
        return false;
    };
    cross_region_list_element_ok(element.as_ref())
}

/// `true` when a `List<element>` element type is one the cross-region
/// host marshaller / verifier / reader handle. Recurses for nested lists
/// so `List<List<String>>` / `List<List<List<…>>>` are admitted to the
/// depth the layout pass materialises; a `List<Schema>` element still goes
/// through the S4 sub-record envelope.
fn cross_region_list_element_ok(element: &TypeRepr) -> bool {
    match element {
        TypeRepr::String | TypeRepr::Int | TypeRepr::Float | TypeRepr::Bool => true,
        TypeRepr::Schema { .. } => list_schema_subrecord_in_s4_scope(&TypeRepr::List {
            element: Box::new(element.clone()),
        }),
        TypeRepr::List { element: inner } => cross_region_list_element_ok(inner.as_ref()),
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => true,
        _ => false,
    }
}

/// F1b / F3: classify a host-visible anon-Dict-return field whose value is
/// a bare `#main` parameter identity (`servers` / `tags` / `xs`) of a list
/// type that the cross-region object path can marshal. Returns the
/// parameter's canonical type when it is:
///   * `List<Schema{…}>` whose element sub-record stays inside the
///     in-place reader's decode envelope (`list_schema_subrecord_in_s4_scope`),
///   * `List<List<scalar>>` (the nested-scalar pointer array),
///   * `List<String>` (the pointer-array-of-string), or
///   * `List<Int>` / `List<Float>` / `List<Bool>` (the inline-fixed scalar
///     list — F3).
///
/// All of these live in the *input* region when sourced from a parameter
/// identity while the object head sits in the *output* region, so the field
/// slot must store the parameter list root's **arena-absolute** offset (no
/// tail copy) and the host's multi-region verifier + reader follow it
/// cross-region. The host decode side already handles every one of these
/// element types: the object positive-`bytes_written` path runs
/// `verify_object_return_multi` over the whole arena, and the `BufferReader`
/// field readers (`read_list_string` / `read_list_int` / `read_list_record`
/// / `read_list_list`) follow arena-absolute slot pointers cross-region, so
/// widening the lowering classifier is all that is needed.
///
/// Anything else — a scalar param, a deeper nested element list, a
/// `List<List<String|Schema>>`, or a non-parameter expression — returns
/// `None`, so the field falls through to the existing scalar classifier and
/// stays a loud cap. Mirrors the single-value return gate
/// (`pointer_array_param_identity_walk` + `list_schema_subrecord_in_s4_scope`).
/// F4 widens this to also accept a two-segment parameter **field** walk
/// (`o.tags` where `o` is a schema-typed param and `tags` is a
/// pointer-array list field), resolved through the param's element schema.
/// Both the identity and field walks land the same arena-absolute slot
/// offset on the object field (the field-load no longer re-encodes
/// post-F1), so the host decode is unchanged.
fn anon_dict_cross_region_param_list<'p>(
    path: &[TokenKey],
    param_canonicals: &'p HashMap<&str, TypeRepr>,
) -> Option<&'p TypeRepr> {
    match path {
        // Identity walk (`servers` / `tags`): the param's own list type.
        [TokenKey::String(name, _, _)] => {
            let ty = param_canonicals.get(name.as_str())?;
            cross_region_list_envelope(ty).then_some(ty)
        }
        // F4/F6 field walk (`o.tags`, `o.inner.tags`, `o.a.b.tags`): the
        // head must be a schema param and the chain must descend through
        // nested-schema fields to a pointer-array leaf inside the
        // envelope. Resolved through the shared deep-chain walker.
        [TokenKey::String(p, _, _), rest @ ..] if !rest.is_empty() => {
            cross_region_param_field_chain(param_canonicals.get(p.as_str())?, rest)
        }
        _ => None,
    }
}

/// F3: classify a **branded-struct** return field (`#schema Wrapper {
/// servers: List<Server>, … }` returned via `#main(…) -> Wrapper { servers:
/// servers, … }`) as a cross-region parameter-list field. Returns `true`
/// when the field's declared `List<…>` type is one the cross-region object
/// path can marshal AND the value is a bare `#main` parameter identity of a
/// list type whose IR shape matches the field.
///
/// This is the branded-struct sibling of [`anon_dict_cross_region_param_list`]
/// (which works the anon-Dict lowering path); the difference is only how the
/// two paths reach the field — the field type / value-shape admission rules
/// and the resulting arena-absolute slot store are identical. The element
/// type envelope matches: `List<Schema>` confined to S4 sub-record scope,
/// `List<List<scalar>>`, `List<String>`, `List<Option|Result|Enum>`, and `List<Int|Float|Bool>`.
///
/// The value may be either a single-segment `Variable` resolving to a
/// `#main` param whose `IrType` matches the field (the F3 identity walk),
/// or — F4 — a two-segment param **field** walk (`w.items`) where the
/// param is a schema and the named field's IR list type matches. Both
/// land the same arena-absolute slot offset on the struct field post-F1.
/// A literal / load / call is **not** a cross-region walk and keeps the
/// existing const-pool / loud-cap paths.
fn branded_field_cross_region_param_list(
    field_ty: &TypeRepr,
    value: &Node,
    ctx: &LowerCtx<'_>,
) -> bool {
    let TypeRepr::List { element } = field_ty else {
        return false;
    };
    if !cross_region_list_envelope(field_ty) {
        return false;
    }
    // Field element-type envelope (mirrors `anon_dict_cross_region_param_list`).
    let field_ir = match element.as_ref() {
        TypeRepr::Schema { .. } => IrType::ListSchema,
        TypeRepr::List { .. }
        | TypeRepr::Option { .. }
        | TypeRepr::Result { .. }
        | TypeRepr::Enum { .. } => IrType::ListList,
        TypeRepr::String => IrType::ListString,
        TypeRepr::Int => IrType::ListInt,
        TypeRepr::Float => IrType::ListFloat,
        TypeRepr::Bool => IrType::ListBool,
        _ => return false,
    };
    let Expr::Variable(path) = &*value.expr else {
        return false;
    };
    match path.as_slice() {
        // F3 identity walk: a bare `#main` param whose IR list type matches.
        [TokenKey::String(name, _, _)] => ctx
            .params
            .iter()
            .any(|b| b.name == *name && b.ty == field_ir),
        // F4/F6 field walk (`w.items`, `w.inner.items`, `w.a.b.items`):
        // the head must be a schema param and the chain must descend
        // through nested-schema fields to a pointer-array leaf whose IR
        // type matches the struct field. The field-load chain pushes the
        // leaf list root's arena-absolute offset post-F1, identical to
        // the identity walk's slot value.
        [TokenKey::String(p, _, _), rest @ ..] if !rest.is_empty() => {
            let Some(binding) = ctx.params.iter().find(|b| b.name == *p) else {
                return false;
            };
            let Some(schema) = binding.schema.as_ref() else {
                return false;
            };
            let base = TypeRepr::Schema {
                schema: Box::new(schema.clone()),
            };
            cross_region_param_field_chain(&base, rest)
                .and_then(|leaf| type_repr_to_ir_type(leaf).ok())
                .map(|ir| ir == field_ir)
                .unwrap_or(false)
        }
        _ => false,
    }
}

/// True when a `List<Schema>` return type's per-element sub-record carries
/// only fields the in-place sub-record reader can decode — **recursively,
/// to any depth** (F7). The admission is type-driven: a field is in scope
/// when its type is a scalar leaf (`Int` / `Float` / `Bool` /
/// `String`) or a `List<element>` whose element is itself in scope
/// ([`cross_region_list_element_ok`]). The list arm reaches back into this
/// predicate for a `List<Schema>` element, so a sub-record field that is
/// itself an object array (`members: List<Person>`) or a nested list
/// (`tags: List<List<Int>>`) — and whose own element schemas again carry
/// such fields — is accepted to whatever depth the element schemas nest.
///
/// The verifier ([`crate::verifier`] in `relon-eval-api`) walks the same
/// graph recursively with a `MAX_DEPTH` guard, and the in-place reader
/// decodes it type-driven, so admitting these here is sound: the host
/// verifies the whole reachable graph before any decode. A field type the
/// layout pass cannot materialise (`Option` / `Result` / `Closure`, or a
/// bare nested `Schema`) returns `false`, keeping that shape a loud cap.
/// `ty` is the full `List<Schema{…}>` return type (the element schema is
/// canonicalised inline, so the field set is available here).
fn list_schema_subrecord_in_s4_scope(ty: &TypeRepr) -> bool {
    let TypeRepr::List { element } = ty else {
        return false;
    };
    let TypeRepr::Schema { schema } = element.as_ref() else {
        return false;
    };
    schema.fields.iter().all(|f| match &f.ty {
        TypeRepr::Int | TypeRepr::Float | TypeRepr::Bool | TypeRepr::Unit | TypeRepr::String => {
            true
        }
        // F7: a list field recurses through the shared element predicate,
        // which re-enters this function for a `List<Schema>` element — so
        // `List<Person>` / `List<List<Int>>` / deeper nest are admitted to
        // the depth the element schemas materialise.
        TypeRepr::List { element } => cross_region_list_element_ok(element.as_ref()),
        // Bare nested Schema field, Option / Result / Closure — out of
        // scope (the bare-Schema sub-field reader path is not on the
        // return-side in-place surface yet).
        _ => false,
    })
}

/// Map a canonical [`TypeRepr`] to the matching [`IrType`]. Used both
/// when building the local index (so `Variable` references know their
/// type) and when synthesising the trailing `StoreField`.
fn type_repr_to_ir_type(t: &TypeRepr) -> Result<IrType, LoweringError> {
    match t {
        TypeRepr::Int => Ok(IrType::I64),
        TypeRepr::Float => Ok(IrType::F64),
        TypeRepr::Bool => Ok(IrType::Bool),
        TypeRepr::Unit => Ok(IrType::Unit),
        TypeRepr::String => Ok(IrType::String),
        TypeRepr::List { element } => match element.as_ref() {
            TypeRepr::Int => Ok(IrType::ListInt),
            TypeRepr::Float => Ok(IrType::ListFloat),
            TypeRepr::Bool => Ok(IrType::ListBool),
            TypeRepr::String => Ok(IrType::ListString),
            TypeRepr::Schema { .. } => Ok(IrType::ListSchema),
            TypeRepr::List { .. }
            | TypeRepr::Option { .. }
            | TypeRepr::Result { .. }
            | TypeRepr::Enum { .. } => Ok(IrType::ListList),
            _ => Err(cap!(
                "type_repr_to_ir_type.unsupported_type_in_main.1",
                LoweringError::UnsupportedTypeInMain {
                    type_name: format!("{t:?}"),
                    range: TokenRange::default(),
                }
            )),
        },
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => {
            Ok(IrType::I32)
        }
        // Composite types rejected upstream reach this branch only from a
        // hand-crafted IR.
        _ => Err(cap!(
            "type_repr_to_ir_type.unsupported_type_in_main.2",
            LoweringError::UnsupportedTypeInMain {
                type_name: format!("{t:?}"),
                range: TokenRange::default(),
            }
        )),
    }
}

/// Map a host-fn signature's [`TypeNode`] onto the IR scalar/list
/// type lattice. Returns `None` for any shape outside the native-call
/// envelope (nested schemas, dicts, closures, enums) — the caller
/// treats `None` as "not lowerable as a native import" and lets the
/// name fall through to the stdlib-unknown error, so an unsupported
/// host-fn signature never silently mis-types a call.
fn type_node_to_ir_type(t: &TypeNode) -> Option<IrType> {
    let name = t.path.last()?.as_str();
    Some(match name {
        "Int" => IrType::I64,
        "Float" => IrType::F64,
        "Bool" => IrType::Bool,
        "String" => IrType::String,
        "List" => {
            let elem = t.generics.first()?;
            match elem.path.last()?.as_str() {
                "Int" => IrType::ListInt,
                "Float" => IrType::ListFloat,
                "Bool" => IrType::ListBool,
                "String" => IrType::ListString,
                _ => return None,
            }
        }
        _ => return None,
    })
}

/// A host-registered native fn resolved into IR-ready shape: the
/// param/return IR types plus the capability bit indices its gate
/// requires (declaration order). Built once per module from the
/// analyzer's `host_fn_signatures` + `host_fn_gates`.
#[derive(Debug, Clone)]
struct HostFnEntry {
    param_tys: Vec<IrType>,
    ret_ty: IrType,
    required_bits: Vec<u32>,
}

/// Module-wide accumulator for `#native` imports. Shared (via
/// `Rc<RefCell<…>>`) across the entry body, every schema-method body,
/// and every lambda body so a native call anywhere in the module
/// interns into one [`Module::imports`] table with stable
/// `import_idx`es. Mirrors the sharing discipline of
/// [`ConstInternTables`] and the lambda slot table.
#[derive(Debug, Default)]
struct NativeImportBuilder {
    /// Name → resolved signature/gate. Immutable after construction;
    /// populated only when the analyzer supplied host-fn metadata
    /// (the legacy single-file `analyze` path leaves it empty, so
    /// no source ever resolves a native call there).
    resolved: HashMap<String, HostFnEntry>,
    /// Emitted imports in `import_idx` order.
    imports: Vec<NativeImport>,
    /// Name → already-assigned `import_idx` (dedup across call sites).
    index_of: HashMap<String, u32>,
}

impl NativeImportBuilder {
    /// Resolve every host-fn signature the analyzer attached to `tree`
    /// into IR-ready form. Signatures outside the native-call type
    /// envelope (see [`type_node_to_ir_type`]) are dropped so a call
    /// to such a name still surfaces the stdlib-unknown error rather
    /// than a mis-typed import.
    fn from_tree(tree: &AnalyzedTree) -> Self {
        let mut resolved = HashMap::new();
        for (name, sig) in &tree.host_fn_signatures {
            let mut param_tys = Vec::with_capacity(sig.params.len());
            let mut ok = true;
            for p in &sig.params {
                match type_node_to_ir_type(&p.ty) {
                    Some(ty) => param_tys.push(ty),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            let Some(ret_ty) = type_node_to_ir_type(&sig.return_type) else {
                continue;
            };
            let required_bits = tree
                .host_fn_gates
                .get(name)
                .map(|g| g.required_bit_indices())
                .unwrap_or_default();
            resolved.insert(
                name.clone(),
                HostFnEntry {
                    param_tys,
                    ret_ty,
                    required_bits,
                },
            );
        }
        // Fail-open guard (observability). A native whose *type signature*
        // the host declared but whose capability *gate* it never declared
        // lowers with an empty `required_bits` above → no `Op::CheckCap`
        // is emitted → the compiled call runs with no capability
        // requirement. That is correct for a genuinely pure fn and also
        // exactly what a forgotten gate looks like — but the two are now
        // distinguishable: `register_pure_fn`'s "this is pure" intent is
        // preserved through `Context::pure_fn_names` →
        // `AnalyzeOptions::host_fn_pure` → `tree.host_fn_pure`. So the
        // warning fires only for names that carry neither a gate nor a
        // purity declaration, no longer false-triggering on legitimately
        // pure fns. We still do NOT change lowering (no behavior change);
        // this stays a `warn!` for operators who want the fail-open
        // surfaced without opting into the hard
        // `require_declared_native_gates` gate (which the analyzer
        // enforces as an Error before lowering is ever reached). Emitting
        // is inert unless a subscriber is installed, so it adds no stderr
        // noise by default.
        for name in undeclared_gate_imports(&resolved, &tree.host_fn_gates, &tree.host_fn_pure) {
            tracing::warn!(
                native_fn = %name,
                "host declared a signature for native `{name}` but no capability gate; \
                 the compiled call will run with no capability requirement. If it is pure, \
                 register an empty `NativeFnGate` to make that explicit; if it touches \
                 files / network / clock / env / rng, register the matching gate so the \
                 runtime can enforce it."
            );
        }
        Self {
            resolved,
            imports: Vec::new(),
            index_of: HashMap::new(),
        }
    }

    /// Get-or-assign the `import_idx` for `name`. The import carries
    /// [`NO_CAPABILITY_BIT`]: the capability guard is emitted as
    /// dedicated `Op::CheckCap` ops (one per required bit) ahead of the
    /// call, so the cranelift backend keys the host-fn slot off
    /// `import_idx` and a multi-bit gate needs no single-bit encoding.
    fn intern(&mut self, name: &str, entry: &HostFnEntry) -> u32 {
        if let Some(idx) = self.index_of.get(name) {
            return *idx;
        }
        let idx = self.imports.len() as u32;
        self.imports.push(NativeImport {
            name: name.to_string(),
            param_tys: entry.param_tys.clone(),
            ret_ty: entry.ret_ty,
            cap_bit: NO_CAPABILITY_BIT,
        });
        self.index_of.insert(name.to_string(), idx);
        idx
    }
}

/// Names of resolved native imports the host declared a *signature* for
/// but declared neither a capability *gate* (`host_fn_gates`) nor an
/// explicit *purity* marker (`host_fn_pure`). These lower with empty
/// `required_bits` and so run ungated in the compiled backends — the
/// fail-open the caller warns about. Sorted for deterministic
/// diagnostics/tests.
///
/// The rule is presence-of-key in either table:
///
/// * A present `host_fn_gates` entry (including an explicitly-empty
///   `NativeFnGate` — the `register_fn(name, NativeFnGate::default(), …)`
///   shape) means "declared, needs exactly these caps" → NOT reported.
/// * A present `host_fn_pure` entry — the `register_pure_fn` intent
///   mirrored through `AnalyzeOptions::host_fn_pure` — means "declared
///   pure, needs no cap" → NOT reported. This is what eliminates the
///   prior false-positive on legitimately pure fns.
///
/// Only a name absent from *both* tables — the shape of "host filled
/// `host_fn_signatures` but forgot to declare a gate" — is reported.
/// Generic over the gate value type so this leaf helper needs no
/// capability-type import.
fn undeclared_gate_imports<G>(
    resolved: &HashMap<String, HostFnEntry>,
    gates: &HashMap<String, G>,
    pure: &HashSet<String>,
) -> Vec<String> {
    let mut out: Vec<String> = resolved
        .keys()
        .filter(|name| !gates.contains_key(*name) && !pure.contains(*name))
        .cloned()
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod undeclared_gate_import_tests {
    use super::*;

    fn entry() -> HostFnEntry {
        HostFnEntry {
            param_tys: Vec::new(),
            ret_ty: IrType::I64,
            required_bits: Vec::new(),
        }
    }

    #[test]
    fn signature_without_gate_or_purity_is_reported() {
        // Host declared a signature but neither a gate nor a purity
        // marker → under-declared (a forgotten gate looks exactly like
        // this shape).
        let mut resolved = HashMap::new();
        resolved.insert("read_net".to_string(), entry());
        let gates: HashMap<String, ()> = HashMap::new();
        let pure: HashSet<String> = HashSet::new();
        assert_eq!(
            undeclared_gate_imports(&resolved, &gates, &pure),
            vec!["read_net".to_string()]
        );
    }

    #[test]
    fn empty_gate_entry_declares_intent_and_is_silent() {
        // A present (even empty) gate entry records intent → NOT
        // reported.
        let mut resolved = HashMap::new();
        resolved.insert("pure_add".to_string(), entry());
        let mut gates: HashMap<String, ()> = HashMap::new();
        gates.insert("pure_add".to_string(), ());
        let pure: HashSet<String> = HashSet::new();
        assert!(undeclared_gate_imports(&resolved, &gates, &pure).is_empty());
    }

    #[test]
    fn declared_pure_via_purity_set_is_silent() {
        // The `register_pure_fn` intent, mirrored through
        // `host_fn_pure`: no gate entry at all, but the name is in the
        // purity set → NOT reported. This is the false-positive the
        // refinement eliminates.
        let mut resolved = HashMap::new();
        resolved.insert("pure_add".to_string(), entry());
        let gates: HashMap<String, ()> = HashMap::new();
        let mut pure: HashSet<String> = HashSet::new();
        pure.insert("pure_add".to_string());
        assert!(undeclared_gate_imports(&resolved, &gates, &pure).is_empty());
    }

    #[test]
    fn gated_effectful_fn_is_silent() {
        // A properly gated effectful fn has a present entry → silent.
        let mut resolved = HashMap::new();
        resolved.insert("clock_add".to_string(), entry());
        let mut gates: HashMap<String, ()> = HashMap::new();
        gates.insert("clock_add".to_string(), ());
        let pure: HashSet<String> = HashSet::new();
        assert!(undeclared_gate_imports(&resolved, &gates, &pure).is_empty());
    }

    #[test]
    fn report_is_sorted_and_only_covers_undeclared() {
        let mut resolved = HashMap::new();
        resolved.insert("zeta".to_string(), entry());
        resolved.insert("alpha".to_string(), entry());
        resolved.insert("declared".to_string(), entry());
        resolved.insert("pure_one".to_string(), entry());
        let mut gates: HashMap<String, ()> = HashMap::new();
        gates.insert("declared".to_string(), ());
        let mut pure: HashSet<String> = HashSet::new();
        pure.insert("pure_one".to_string());
        assert_eq!(
            undeclared_gate_imports(&resolved, &gates, &pure),
            vec!["alpha".to_string(), "zeta".to_string()]
        );
    }
}

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

/// Static result of testing one match arm's pattern against the
/// scrutinee's statically-known shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaticArmDecision {
    /// The scrutinee's static type provably satisfies this arm's
    /// pattern — `check_type` / brand-equality would return `Ok` for
    /// EVERY runtime value of this static type. Select this arm.
    Matches,
    /// The pattern provably never matches a value of this static type —
    /// `check_type` would always fail (or the eval brand-shortcut would
    /// `continue`). Skip this arm.
    Never,
    /// The static type does NOT pin the arm decision (a coarsening
    /// builtin like `Number` / `List` / `Dict`, a generic pattern, a
    /// dotted/optional pattern, or a builtin pattern against a branded
    /// dict). Keep the whole `match` capped and defer — never force.
    Undecidable,
}

/// Decide, purely statically, whether a value whose IR type is `ty`
/// (carrying optional schema `brand`) satisfies the arm `pattern`.
///
/// This MUST agree with the tree-walk `Expr::Match` arm semantics in
/// `relon-evaluator`'s `eval.rs` (the `check_type` / brand-equality
/// path) for the static type in question:
///
/// * `Wildcard` always matches.
/// * `Type(tn)` with a single-segment, non-generic, non-optional path:
///   - branded scrutinee (`brand == Some(b)`, i.e. a branded `Dict`):
///     `name == b` ⇒ match; a non-builtin `name != b` ⇒ the eval
///     brand-shortcut `continue`s (never matches); a builtin `name`
///     falls through to `check_type` against a branded dict, which the
///     static layer does not model ⇒ `Undecidable`.
///   - unbranded scrutinee: a builtin `name` matches iff it is the
///     EXACT scalar tag for `ty` (no coarsening / multi-type builtin);
///     a non-builtin (schema) `name` against a definite scalar value
///     can never satisfy `apply_schema` (which requires a `Dict`) ⇒
///     `Never`.
///   - any optional / dotted / generic pattern ⇒ `Undecidable`.
fn static_arm_decision(ty: IrType, brand: Option<&str>, pattern: &Expr) -> StaticArmDecision {
    match pattern {
        Expr::Wildcard => StaticArmDecision::Matches,
        Expr::Type(tn) => static_type_pattern_decision(ty, brand, tn),
        // Any other pattern shape (literal patterns, etc.) is not part of
        // the strict-mode static surface — defer.
        _ => StaticArmDecision::Undecidable,
    }
}

fn static_type_pattern_decision(
    ty: IrType,
    brand: Option<&str>,
    tn: &TypeNode,
) -> StaticArmDecision {
    // Optional (`W?`), dotted (`geo.Location`), or generic (`List<Int>`)
    // patterns engage check_type branches the static layer does not
    // model — defer rather than guess.
    if tn.is_optional || tn.path.len() != 1 || !tn.generics.is_empty() {
        return StaticArmDecision::Undecidable;
    }
    let name = tn.path[0].as_str();

    if let Some(b) = brand {
        // Branded scrutinee (a branded `Dict`). Mirror the eval
        // brand-shortcut block exactly.
        if name == b {
            // `type_node.path.len() == 1 && path[0] == brand` ⇒ match.
            return StaticArmDecision::Matches;
        }
        if !is_builtin_type_name(name) {
            // Non-builtin brand mismatch ⇒ eval `continue`s (never).
            return StaticArmDecision::Never;
        }
        // Builtin pattern against a branded dict falls through to
        // check_type (e.g. `Dict` / `Any` matching a branded dict).
        // Not modelled statically — defer.
        return StaticArmDecision::Undecidable;
    }

    // Unbranded scrutinee.
    if is_builtin_type_name(name) {
        // Only the EXACT scalar tags are decided here. Coarsening /
        // multi-type builtins (`Any`, `Number`, `List`, `Dict`,
        // `Closure`, `Fn`, `Enum`, `Tuple`) are deferred.
        match (name, ty) {
            ("Int", IrType::I64) => StaticArmDecision::Matches,
            ("Float", IrType::F64) => StaticArmDecision::Matches,
            ("Bool", IrType::Bool) => StaticArmDecision::Matches,
            ("String", IrType::String) => StaticArmDecision::Matches,
            // A scalar builtin pattern naming a DIFFERENT scalar than the
            // (definite-scalar) static type can never match.
            ("Int" | "Float" | "Bool" | "String", t) if is_definite_scalar(t) => {
                StaticArmDecision::Never
            }
            _ => StaticArmDecision::Undecidable,
        }
    } else {
        // Non-builtin pattern (a schema/brand name) against an unbranded
        // value. The eval path runs `check_type`, whose `apply_schema`
        // requires a `Dict`; a definite scalar can never satisfy it, so
        // the arm provably never matches. For non-scalar shapes (lists /
        // plain dicts) defer to stay honest.
        if is_definite_scalar(ty) {
            StaticArmDecision::Never
        } else {
            StaticArmDecision::Undecidable
        }
    }
}

/// `true` when the IR type pins the runtime value to a single concrete
/// scalar Relon shape (`Int` / `Float` / `Bool` / `String`).
/// These are the only types for which a scalar / schema pattern decision
/// can be made with certainty.
fn is_definite_scalar(ty: IrType) -> bool {
    matches!(
        ty,
        IrType::I64 | IrType::F64 | IrType::Bool | IrType::String | IrType::Unit
    )
}

/// One lowered source arm in a runtime `#enum` match.
struct RuntimeEnumMatchArm {
    /// `Some(tag)` for a concrete variant arm, `None` for wildcard.
    tag: Option<u8>,
    body_ops: Vec<TaggedOp>,
    body_ty: IrType,
    range: TokenRange,
}

/// Build the body ops for a guaranteed no-match trap of result type
/// `result_ty`: an `Op::Trap { NoMatch }` followed by a typed placeholder
/// const. The trap makes the placeholder unreachable; it exists only so
/// the type stack / wasm verifier see a value of the right type (mirrors
/// the stdlib bounds-trap shape `Op::Trap` + typed const). Returns `None`
/// for a result type with no scalar placeholder const, so the caller caps
/// cleanly rather than miscompiling.
fn no_match_trap_body_ops(
    result_ty: IrType,
    range: TokenRange,
    ctx: &LowerCtx<'_>,
) -> Option<Vec<TaggedOp>> {
    let placeholder = match result_ty {
        IrType::I64 => Op::ConstI64(0),
        IrType::I32 => Op::ConstI32(0),
        IrType::F64 => Op::ConstF64(OrderedFloat(0.0)),
        IrType::Bool => Op::ConstBool(false),
        IrType::String => {
            let idx = ctx.const_intern.borrow_mut().strings.intern("");
            Op::ConstString {
                idx,
                value: String::new(),
            }
        }
        _ => return None,
    };
    Some(vec![
        TaggedOp {
            op: Op::Trap {
                kind: TrapKind::NoMatch,
            },
            range,
        },
        TaggedOp {
            op: placeholder,
            range,
        },
    ])
}

fn enum_scrutinee_binding(scrutinee: &Node, ctx: &LowerCtx<'_>) -> Option<(String, TypeRepr)> {
    let Expr::Variable(path) = &*scrutinee.expr else {
        return None;
    };
    if path.len() != 1 {
        return None;
    }
    let TokenKey::String(name, _, _) = &path[0] else {
        return None;
    };
    if let Some(binding) = ctx
        .params
        .iter()
        .find(|binding| binding.name == name.as_str())
    {
        return enum_like_type(&binding.type_repr)
            .then(|| (binding.name.clone(), binding.type_repr.clone()));
    }
    let binding = ctx
        .lets
        .iter()
        .rev()
        .find(|binding| binding.name == name.as_str())?;
    let type_repr = binding.type_repr.as_ref()?;
    enum_like_type(type_repr).then(|| (binding.name.clone(), type_repr.clone()))
}

fn enum_like_type(ty: &TypeRepr) -> bool {
    matches!(
        ty,
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. }
    )
}

fn enum_like_name(ty: &TypeRepr) -> Option<&str> {
    match ty {
        TypeRepr::Option { .. } => Some("Option"),
        TypeRepr::Result { .. } => Some("Result"),
        TypeRepr::Enum { name, .. } => Some(name.as_str()),
        _ => None,
    }
}

fn enum_like_tags(ty: &TypeRepr) -> Option<Vec<u8>> {
    match ty {
        TypeRepr::Option { .. } | TypeRepr::Result { .. } => Some(vec![0, 1]),
        TypeRepr::Enum { variants, .. } => {
            Some(variants.iter().map(|variant| variant.tag).collect())
        }
        _ => None,
    }
}

fn type_pattern_variant_name(enum_ty: &TypeRepr, pattern: &TypeNode) -> Option<String> {
    if pattern.is_optional || !pattern.generics.is_empty() || pattern.variant_fields.is_some() {
        return None;
    }
    let parts: Vec<&str> = pattern.path.iter().map(String::as_str).collect();
    match enum_ty {
        TypeRepr::Option { .. } => match parts.as_slice() {
            ["None"] | ["Option", "None"] => Some("None".to_string()),
            ["Some"] | ["Option", "Some"] => Some("Some".to_string()),
            _ => None,
        },
        TypeRepr::Result { .. } => match parts.as_slice() {
            ["Ok"] | ["Result", "Ok"] => Some("Ok".to_string()),
            ["Err"] | ["Result", "Err"] => Some("Err".to_string()),
            _ => None,
        },
        TypeRepr::Enum { name, variants } => {
            let variant_name = match parts.as_slice() {
                [variant] => *variant,
                [enum_name, variant] if enum_name == name => *variant,
                _ => return None,
            };
            variants
                .iter()
                .find(|variant| variant.name == variant_name)
                .map(|variant| variant.name.clone())
        }
        _ => None,
    }
}

fn matched_enum_variant(
    enum_ty: &TypeRepr,
    enum_path: Option<&[String]>,
    variant: &str,
    range: TokenRange,
) -> Option<EnumVariantNarrowing> {
    let enum_name = enum_like_name(enum_ty)?.to_string();
    match enum_ty {
        TypeRepr::Option { inner } => {
            if !enum_path_matches("Option", enum_path) {
                return None;
            }
            let (tag, fields, direct_payload) = match variant {
                "None" => (0, Vec::new(), None),
                "Some" => {
                    let field = Field {
                        name: "value".to_string(),
                        ty: inner.as_ref().clone(),
                        default: None,
                    };
                    let direct_payload = DirectEnumPayload {
                        field_name: field.name.clone(),
                        ty: field.ty.clone(),
                    };
                    (1, vec![field], Some(direct_payload))
                }
                _ => return None,
            };
            Some(EnumVariantNarrowing {
                enum_name,
                variant: CanonicalEnumVariant {
                    name: variant.to_string(),
                    tag,
                    fields,
                    is_tuple: false,
                },
                direct_payload,
            })
        }
        TypeRepr::Result { ok, err } => {
            if !enum_path_matches("Result", enum_path) {
                return None;
            }
            let (tag, field_name, ty) = match variant {
                "Ok" => (0, "value", ok.as_ref().clone()),
                "Err" => (1, "error", err.as_ref().clone()),
                _ => return None,
            };
            let field = Field {
                name: field_name.to_string(),
                ty,
                default: None,
            };
            let direct_payload = DirectEnumPayload {
                field_name: field.name.clone(),
                ty: field.ty.clone(),
            };
            Some(EnumVariantNarrowing {
                enum_name,
                variant: CanonicalEnumVariant {
                    name: variant.to_string(),
                    tag,
                    fields: vec![field],
                    is_tuple: false,
                },
                direct_payload: Some(direct_payload),
            })
        }
        TypeRepr::Enum { name, variants } => {
            if !enum_path_matches(name, enum_path) {
                return None;
            }
            let variant = variants
                .iter()
                .find(|candidate| candidate.name == variant)?;
            Some(EnumVariantNarrowing {
                enum_name,
                variant: variant.clone(),
                direct_payload: None,
            })
        }
        _ => {
            let _ = range;
            None
        }
    }
}

fn enum_pattern_variant(
    enum_ty: &TypeRepr,
    pattern: &Expr,
    range: TokenRange,
) -> Option<(EnumVariantNarrowing, Vec<PatternBinding>)> {
    match pattern {
        Expr::Type(tn) => {
            let variant_name = type_pattern_variant_name(enum_ty, tn)?;
            matched_enum_variant(enum_ty, None, &variant_name, range).map(|n| (n, Vec::new()))
        }
        Expr::VariantPattern {
            enum_path,
            variant,
            bindings,
        } => matched_enum_variant(enum_ty, Some(enum_path), variant, range)
            .map(|n| (n, bindings.clone())),
        _ => None,
    }
}

fn enum_payload_field_type(
    narrowing: &EnumVariantNarrowing,
    field_name: &str,
    range: TokenRange,
) -> Result<TypeRepr, LoweringError> {
    if let Some(payload) = &narrowing.direct_payload {
        if field_name == payload.field_name {
            return Ok(payload.ty.clone());
        }
        return Err(cap!(
            "lower_match.enum_pattern_unknown_payload",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "Enum(variant `{}` has no payload field `{field_name}`)",
                    narrowing.variant.name
                ),
                range,
            }
        ));
    }

    let payload_schema = narrowing
        .variant
        .payload_schema(&narrowing.enum_name)
        .ok_or_else(|| {
            cap!(
                "lower_match.enum_pattern_unit_payload",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Enum(unit variant `{}` has no payload field `{field_name}`)",
                        narrowing.variant.name
                    ),
                    range,
                }
            )
        })?;
    payload_schema
        .fields
        .iter()
        .find(|field| field.name == field_name)
        .map(|field| field.ty.clone())
        .ok_or_else(|| {
            cap!(
                "lower_match.enum_pattern_unknown_payload",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Enum(variant `{}` has no payload field `{field_name}`)",
                        narrowing.variant.name
                    ),
                    range,
                }
            )
        })
}

fn enum_pattern_binding_field_name(
    narrowing: &EnumVariantNarrowing,
    binding: &PatternBinding,
    idx: usize,
) -> String {
    if let Some(field) = binding.field.clone() {
        return field;
    }
    if let Some(payload) = &narrowing.direct_payload {
        return payload.field_name.clone();
    }
    idx.to_string()
}

fn emit_enum_pattern_bindings(
    scrutinee_let_idx: u32,
    narrowing: &EnumVariantNarrowing,
    bindings: &[PatternBinding],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Vec<String>, LoweringError> {
    let mut added = Vec::new();
    let mut seen_bindings: HashSet<&str> = HashSet::new();
    for (idx, binding) in bindings.iter().enumerate() {
        let Some(name) = binding.binding.as_ref() else {
            continue;
        };
        if !seen_bindings.insert(name.as_str()) {
            return Err(cap!(
                "lower_match.enum_pattern_duplicate_binding",
                LoweringError::UnsupportedExpr {
                    kind: format!("duplicate enum pattern binding `{name}`"),
                    range,
                }
            ));
        }
        let field_name = enum_pattern_binding_field_name(narrowing, binding, idx);
        let field_ty = enum_payload_field_type(narrowing, &field_name, range)?;
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: scrutinee_let_idx,
                ty: IrType::I32,
            },
            range,
        });
        ctx.tstack.push(IrType::I32);
        let key = TokenKey::String(field_name, range, false);
        lower_enum_payload_path(&[key], narrowing, range, ctx)?;
        let ty = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_match.enum_pattern_binding_stack",
                LoweringError::UnsupportedExpr {
                    kind: format!("enum pattern binding `{name}` produced no value"),
                    range,
                }
            )
        })?;
        let let_idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetSet { idx: let_idx, ty },
            range,
        });
        let schema_brand = match &field_ty {
            TypeRepr::Schema { schema } => Some(schema.name.clone()),
            _ => None,
        };
        ctx.lets.push(LetBinding {
            name: name.clone(),
            idx: let_idx,
            ty,
            schema_brand,
            type_repr: Some(field_ty.clone()),
        });
        added.push(name.clone());
    }
    Ok(added)
}

fn enum_tag_test_ops(scrutinee_let_idx: u32, tag: u8, range: TokenRange) -> Vec<TaggedOp> {
    vec![
        TaggedOp {
            op: Op::LetGet {
                idx: scrutinee_let_idx,
                ty: IrType::I32,
            },
            range,
        },
        TaggedOp {
            op: Op::LoadI8UAtAbsolute { offset: 0 },
            range,
        },
        TaggedOp {
            op: Op::ConstI32(i32::from(tag)),
            range,
        },
        TaggedOp {
            op: Op::Eq(IrType::I32),
            range,
        },
    ]
}

fn runtime_enum_match_chain(
    arms: &[RuntimeEnumMatchArm],
    scrutinee_let_idx: u32,
    result_ty: IrType,
    range: TokenRange,
) -> Vec<TaggedOp> {
    let mut else_body = arms
        .last()
        .expect("runtime enum match must have at least one arm")
        .body_ops
        .clone();
    for arm in arms[..arms.len().saturating_sub(1)].iter().rev() {
        let Some(tag) = arm.tag else {
            else_body = arm.body_ops.clone();
            continue;
        };
        let mut seq = enum_tag_test_ops(scrutinee_let_idx, tag, arm.range);
        seq.push(TaggedOp {
            op: Op::If {
                result_ty,
                then_body: arm.body_ops.clone(),
                else_body,
            },
            range,
        });
        else_body = seq;
    }
    else_body
}

fn lower_branch_with_enum_narrowing(
    node: &Node,
    range: TokenRange,
    parent: &mut LowerCtx<'_>,
    scrutinee_name: &str,
    scrutinee_let_idx: u32,
    narrowing: Option<EnumVariantNarrowing>,
    pattern_bindings: &[PatternBinding],
) -> Result<(Vec<TaggedOp>, IrType), LoweringError> {
    let previous = narrowing.clone().map(|narrowing| {
        parent
            .enum_variant_narrowing
            .insert(scrutinee_name.to_string(), narrowing)
    });

    let saved_out = std::mem::take(&mut parent.out);
    let saved_stack = std::mem::take(&mut parent.tstack);
    let let_len_before = parent.lets.len();

    if let Some(narrowing) = narrowing.as_ref() {
        emit_enum_pattern_bindings(
            scrutinee_let_idx,
            narrowing,
            pattern_bindings,
            range,
            parent,
        )?;
    }
    lower_expr(&node.expr, node.range, parent)?;
    let branch_ops = std::mem::replace(&mut parent.out, saved_out);
    let branch_stack = std::mem::replace(&mut parent.tstack, saved_stack);
    parent.lets.truncate(let_len_before);

    if let Some(previous) = previous {
        match previous {
            Some(old) => {
                parent
                    .enum_variant_narrowing
                    .insert(scrutinee_name.to_string(), old);
            }
            None => {
                parent.enum_variant_narrowing.remove(scrutinee_name);
            }
        }
    }

    if branch_stack.len() != 1 {
        return Err(cap!(
            "lower_branch.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: format!("Match(branch-stack={})", branch_stack.len()),
                range,
            }
        ));
    }
    Ok((branch_ops, branch_stack[0]))
}

fn try_lower_runtime_enum_match(
    scrutinee: &Node,
    arms: &[(Node, Node)],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<bool, LoweringError> {
    let Some((scrutinee_name, enum_ty)) = enum_scrutinee_binding(scrutinee, ctx) else {
        return Ok(false);
    };
    if enum_like_name(&enum_ty).is_none() {
        return Ok(false);
    }
    if arms.is_empty() {
        return Err(cap!(
            "lower_match.empty_enum_match",
            LoweringError::UnsupportedExpr {
                kind: "Match(enum with no arms)".to_string(),
                range,
            }
        ));
    }

    let scrutinee_let_idx = ctx.next_let_idx;
    ctx.next_let_idx += 1;
    let all_tags: HashSet<u8> = enum_like_tags(&enum_ty)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let mut covered_tags: HashSet<u8> = HashSet::new();
    let mut lowered: Vec<RuntimeEnumMatchArm> = Vec::new();
    let mut has_wildcard = false;

    for (pattern, body) in arms {
        match &*pattern.expr {
            Expr::Wildcard => {
                let (body_ops, body_ty) = lower_branch(body, range, ctx)?;
                lowered.push(RuntimeEnumMatchArm {
                    tag: None,
                    body_ops,
                    body_ty,
                    range: pattern.range,
                });
                has_wildcard = true;
                break;
            }
            Expr::Type(_) | Expr::VariantPattern { .. } => {
                let Some((narrowing, pattern_bindings)) =
                    enum_pattern_variant(&enum_ty, pattern.expr.as_ref(), pattern.range)
                else {
                    return Err(cap!(
                        "lower_match.unsupported_expr.1",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "Match(enum pattern `{}` is not a variant of the scrutinee enum)",
                                pattern.expr.kind()
                            ),
                            range: pattern.range,
                        }
                    ));
                };
                let tag = narrowing.variant.tag;
                let narrowing = Some(narrowing);
                let (body_ops, body_ty) = lower_branch_with_enum_narrowing(
                    body,
                    range,
                    ctx,
                    &scrutinee_name,
                    scrutinee_let_idx,
                    narrowing,
                    &pattern_bindings,
                )?;
                covered_tags.insert(tag);
                lowered.push(RuntimeEnumMatchArm {
                    tag: Some(tag),
                    body_ops,
                    body_ty,
                    range: pattern.range,
                });
            }
            other => {
                return Err(cap!(
                    "lower_match.unsupported_expr.1",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "Match(enum pattern `{}` is not supported by compiled runtime dispatch)",
                            other.kind()
                        ),
                        range: pattern.range,
                    }
                ));
            }
        }
    }

    if lowered.is_empty() {
        return Err(cap!(
            "lower_match.empty_enum_match",
            LoweringError::UnsupportedExpr {
                kind: "Match(enum with no lowerable arms)".to_string(),
                range,
            }
        ));
    }

    let result_ty = lowered[0].body_ty;
    for arm in lowered.iter().skip(1) {
        if arm.body_ty != result_ty {
            return Err(cap!(
                "lower_ternary.if_branch_type_mismatch",
                LoweringError::IfBranchTypeMismatch {
                    then_ty: result_ty,
                    else_ty: arm.body_ty,
                    range,
                }
            ));
        }
    }

    // Non-exhaustive enum match with no `_` catch-all: every uncovered
    // tag must trap at runtime exactly as the tree-walk oracle does
    // (`TypeMismatch { expected: "a matching arm" }`). Append a synthetic
    // trailing wildcard arm whose body is the `TrapKind::NoMatch` trap +
    // a typed placeholder, so the dispatch chain's innermost `else`
    // traps instead of silently returning a wrong arm's body.
    if !has_wildcard && !all_tags.is_subset(&covered_tags) {
        let Some(body_ops) = no_match_trap_body_ops(result_ty, range, ctx) else {
            return Err(cap!(
                "lower_match.no_match_trap_result_ty",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Match(non-exhaustive enum, no-match trap placeholder unavailable for \
                         result type {result_ty:?})"
                    ),
                    range,
                }
            ));
        };
        lowered.push(RuntimeEnumMatchArm {
            tag: None,
            body_ops,
            body_ty: result_ty,
            range,
        });
        has_wildcard = true;
    }
    let _ = has_wildcard;

    lower_expr(&scrutinee.expr, scrutinee.range, ctx)?;
    let scrut_ty = ctx.tstack.pop().ok_or_else(|| {
        cap!(
            "lower_match.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: "Match(enum scrutinee produced no value)".to_string(),
                range: scrutinee.range,
            }
        )
    })?;
    if scrut_ty != IrType::I32 {
        return Err(cap!(
            "lower_match.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "Match(enum scrutinee lowered to {scrut_ty:?}, expected I32 pointer)"
                ),
                range: scrutinee.range,
            }
        ));
    }
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: scrutinee_let_idx,
            ty: IrType::I32,
        },
        range: scrutinee.range,
    });

    let chain = runtime_enum_match_chain(&lowered, scrutinee_let_idx, result_ty, range);
    ctx.out.extend(chain);
    ctx.tstack.push(result_ty);
    Ok(true)
}

/// Wave R5 — static lowering of a strict-mode `match` whose scrutinee's
/// type is statically known, so the winning arm is selected at compile
/// time (no runtime brand dispatch).
///
/// Semantics matched byte-for-byte against the tree-walk `Expr::Match`
/// arm (see `relon-evaluator`'s `eval.rs`):
///
/// 1. The scrutinee is evaluated exactly once (and any trap fires).
/// 2. Arms are tried in SOURCE ORDER; the first arm whose pattern the
///    scrutinee's static type satisfies wins.
/// 3. The winning arm's body is the result.
///
/// The static decision per arm is made by [`static_arm_decision`], which
/// is proven to agree with the runtime `check_type` / brand-equality for
/// the scrutinee's static type. If ANY arm before the winner is
/// undecidable, or no arm statically matches (the eval would trap with
/// `TypeMismatch { expected: "a matching arm" }`, a cross-backend trap
/// shape the static layer cannot yet surface), the whole construct is
/// kept `cap!`'d and deferred (R6 — the `#relaxed` / dynamic
/// brand-dispatch form lives there too).
fn lower_match(
    scrutinee: &Node,
    arms: &[(Node, Node)],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if try_lower_runtime_enum_match(scrutinee, arms, range, ctx)? {
        return Ok(());
    }

    // 1. Determine the scrutinee's static IR type by speculatively
    //    lowering it (rolled back — the real evaluation is re-emitted
    //    below for trap / side-effect parity).
    let ty = probe_expr_ir_ty(scrutinee, ctx)?;

    // 2. Determine the scrutinee's static schema brand, if it is a plain
    //    variable path rooted at a schema-typed binding. Any other
    //    scrutinee shape carries no brand (brand == None).
    let brand: Option<String> = match &*scrutinee.expr {
        Expr::Variable(path) => resolve_receiver_schema_brand(path, ctx),
        _ => None,
    };

    // 3. Walk arms in source order. Select the first arm that statically
    //    matches. Bail (cap) the moment an earlier arm is undecidable —
    //    its runtime match could pre-empt our chosen arm.
    let mut selected: Option<usize> = None;
    for (idx, (pattern, _body)) in arms.iter().enumerate() {
        match static_arm_decision(ty, brand.as_deref(), &pattern.expr) {
            StaticArmDecision::Matches => {
                selected = Some(idx);
                break;
            }
            StaticArmDecision::Never => continue,
            StaticArmDecision::Undecidable => {
                // Defensive cap: an undecidable arm means the scrutinee is
                // not pinned to a single type, i.e. this is dynamic
                // runtime-`#brand` dispatch. The analyzer now rejects that
                // shape up-front (`Diagnostic::DynamicBrandDispatchMatch`),
                // so a well-analyzed program never reaches here. We keep
                // the cap rather than `unreachable!` so a caller that
                // lowers un-analyzed IR fails honestly instead of
                // miscompiling.
                return Err(cap!(
                    "lower_match.unsupported_expr.1",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "Match(arm pattern not statically decidable for scrutinee type \
                             {ty:?} brand {brand:?} — dynamic brand-dispatch is rejected by \
                             the analyzer; declare an `#enum`)"
                        ),
                        range: pattern.range,
                    }
                ));
            }
        }
    }

    let Some(selected) = selected else {
        // No arm statically matches: the construct ALWAYS traps at
        // runtime, exactly as the tree-walk oracle's `Expr::Match`
        // no-match path does (`TypeMismatch { expected: "a matching
        // arm" }`). `TrapKind::NoMatch` lifts to that same typed
        // `RuntimeError::TypeMismatch` on every backend (cranelift /
        // llvm / wasm), so we lower a guaranteed trap rather than cap.
        //
        // We need a typed value on the stack for the verifier's
        // both-arms-typed / result-type contract even though the trap
        // makes everything after it unreachable. The first arm's body
        // type is the match's nominal result type; re-lowering that body
        // after the trap yields a correctly-typed (dead) placeholder.
        // Probe it first so a body we cannot lower caps cleanly instead
        // of half-emitting.
        let result_ty = probe_expr_ir_ty(&arms[0].1, ctx)?;

        // Evaluate the scrutinee for value / trap / side-effect-ordering
        // parity (its own traps fire here), then discard it.
        lower_expr(&scrutinee.expr, scrutinee.range, ctx)?;
        let scrut_ty = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_match.unsupported_expr.3",
                LoweringError::UnsupportedExpr {
                    kind: "Match(scrutinee produced no value)".to_string(),
                    range: scrutinee.range,
                }
            )
        })?;
        let discard_idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetSet {
                idx: discard_idx,
                ty: scrut_ty,
            },
            range: scrutinee.range,
        });

        // The guaranteed no-match trap.
        ctx.out.push(TaggedOp {
            op: Op::Trap {
                kind: TrapKind::NoMatch,
            },
            range,
        });

        // Dead placeholder of the result type so the type stack / wasm
        // verifier stay satisfied (mirrors the stdlib bounds-trap which
        // emits `Op::Trap` followed by a typed const). Re-lower the first
        // arm body; everything here is unreachable past the trap.
        let body = &arms[0].1;
        lower_expr(&body.expr, body.range, ctx)?;
        let placeholder_ty = ctx.tstack.last().copied();
        debug_assert_eq!(
            placeholder_ty,
            Some(result_ty),
            "no-match placeholder type drifted from probe"
        );
        return Ok(());
    };

    // 4. Evaluate the scrutinee for value / trap / side-effect-ordering
    //    parity, then discard its value into a fresh internal let-local
    //    that is never read (the R4 `type(v)` discard pattern). The
    //    scrutinee's traps already fired in its op stream.
    lower_expr(&scrutinee.expr, scrutinee.range, ctx)?;
    let scrut_ty = ctx.tstack.pop().ok_or_else(|| {
        cap!(
            "lower_match.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: "Match(scrutinee produced no value)".to_string(),
                range: scrutinee.range,
            }
        )
    })?;
    let discard_idx = ctx.next_let_idx;
    ctx.next_let_idx += 1;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: discard_idx,
            ty: scrut_ty,
        },
        range: scrutinee.range,
    });

    // 5. Lower the selected arm's body as the result of the whole match.
    let body = &arms[selected].1;
    lower_expr(&body.expr, body.range, ctx)
}

/// Phase F.2 (W7 anon-Dict-return): when a free-call's head names a
/// closure-typed let-binding (a `(name) => ...` value lifted into an
/// internal let by [`lower_anon_dict_body`]), emit the call as
/// `LetGet { idx, Closure }` + per-arg lowering + `Op::CallClosure`.
///
/// Returns `Ok(Some(()))` when the call was lowered, `Ok(None)` when
/// the head doesn't match a closure let (so the caller falls back to
/// the stdlib dispatch / schema-method path). Errors propagate when
/// the arg arity / types don't match the recorded signature.
fn try_lower_local_closure_call(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    let [TokenKey::String(name, _, _)] = path else {
        return Ok(None);
    };
    let Some(binding) = ctx.lets.iter().rev().find(|b| b.name == *name).cloned() else {
        return Ok(None);
    };
    if binding.ty != IrType::Closure {
        return Ok(None);
    }
    let (param_tys, ret_ty) = ctx
        .closure_let_signatures
        .get(&binding.idx)
        .cloned()
        .ok_or_else(|| {
            cap!(
                "try_lower_local_closure_call.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "FnCall(local-closure `{}`: signature side-table missing for let_idx {})",
                        name, binding.idx,
                    ),
                    range,
                }
            )
        })?;
    if args.len() != param_tys.len() {
        return Err(cap!(
            "try_lower_local_closure_call.unknown_stdlib_method",
            LoweringError::UnknownStdlibMethod {
                name: name.clone(),
                arity: args.len() as u32,
                range,
            }
        ));
    }
    // Push the closure handle first; `Op::CallClosure` consumes
    // `[handle, arg0, arg1, ...]`.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: binding.idx,
            ty: IrType::Closure,
        },
        range,
    });
    ctx.tstack.push(IrType::Closure);
    for (i, call_arg) in args.iter().enumerate() {
        if call_arg.name.is_some() {
            return Err(cap!(
                "try_lower_local_closure_call.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!("FnCall(local-closure `{}`: named-arg unsupported)", name),
                    range,
                }
            ));
        }
        lower_expr(&call_arg.value.expr, call_arg.value.range, ctx)?;
        let expected = param_tys[i];
        let got = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "try_lower_local_closure_call.unsupported_expr.3",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "FnCall(local-closure `{}`: arg {} produced no value)",
                        name, i
                    ),
                    range,
                }
            )
        })?;
        if got.wasm_slot() != expected.wasm_slot() {
            // Wave B: a param typed `String` by the concat-body
            // inference (never by an explicit annotation) is only ever
            // used as a concat leaf inside the body, so rendering a
            // scalar argument to `String` *before* the call is
            // byte-identical to the tree-walk oracle, which renders
            // the value at the `+` inside the body via the same
            // `Display`. This is what admits the
            // `examples/pricing.relon` Float-valued `@currency` shape
            // (`@currency("USD") display: price` ⇒
            // `currency(price, "USD")` with a String-concat body).
            let coercible = expected == IrType::String
                && matches!(got, IrType::I64 | IrType::F64 | IrType::Bool)
                && ctx
                    .closure_concat_coercible
                    .get(&binding.idx)
                    .and_then(|mask| mask.get(i).copied())
                    .unwrap_or(false);
            if coercible {
                // `lower_value_to_string` expects the operand tag
                // already popped (it is — `got` above) and pushes the
                // result `String` tag itself.
                lower_value_to_string(got, call_arg.value.range, ctx)?;
                continue;
            }
            return Err(cap!(
                "try_lower_local_closure_call.stdlib_arg_type_mismatch",
                LoweringError::StdlibArgTypeMismatch {
                    name: name.clone(),
                    arg_idx: i as u32,
                    got,
                    expected,
                    range,
                }
            ));
        }
        ctx.tstack.push(got);
    }
    // Pop args + handle from the virtual stack to match the op's
    // stack effect.
    for _ in 0..param_tys.len() {
        ctx.tstack.pop();
    }
    ctx.tstack.pop(); // the handle
    ctx.out.push(TaggedOp {
        op: Op::CallClosure {
            param_tys: param_tys.clone(),
            ret_ty,
        },
        range,
    });
    ctx.tstack.push(ret_ty);
    Ok(Some(()))
}

/// Phase 4.a: lower an [`Expr::FnCall`] into a stdlib dispatch.
///
/// Accepts two forms:
///
/// * **Free call**: `path = ["length"]`, `args = [single_value]`. The
///   value is treated as the lone argument; codegen pops it before
///   emitting the wasm `call`.
/// * **Method call**: `path = [<receiver_segments..>, "length"]`,
///   `args = []`. The leading path segments name the receiver — a
///   bare identifier (`s.length()`) carries the segments as
///   `TokenKey::String(name)`, while a parenthesised expression
///   (`("a").length()`) wraps the receiver in a `TokenKey::Dynamic`.
///   Both shapes lower the receiver onto the stack as the call's
///   single argument.
///
/// Any other name / arity surfaces as
/// [`LoweringError::UnknownStdlibMethod`]; argument-type mismatches
/// surface as [`LoweringError::StdlibArgTypeMismatch`].
/// Free-call native-fn lowering. Returns `Ok(true)` when `name`
/// resolved to a host-registered native fn and the guarded call was
/// emitted; `Ok(false)` when the name is not a native fn (so the
/// caller falls through to its stdlib-unknown error).
///
/// Emitted shape, in order:
///   1. one `Op::CheckCap { cap_bit }` per capability bit the fn's
///      gate requires (declaration order) — fires the runtime consult
///      before any argument is evaluated, so a denial traps before the
///      host fn observes state;
///   2. each argument expression, left-to-right;
///   3. `Op::CallNative { import_idx, param_tys, ret_ty, cap_bit:
///      NO_CAPABILITY_BIT }` — the cap guard lives in the CheckCap
///      prologue, so the call itself carries the no-cap sentinel and
///      both backends key the host-fn slot off `import_idx`.
///
/// The analyzer has already arity- and type-checked the call (and the
/// capability reachability check has run against the static grant), so
/// an arity mismatch here means the host registered a signature the
/// analyzer never saw — bail to `Ok(false)` and let the unknown-method
/// error surface rather than emit a malformed call.
fn try_lower_native_call(
    name: &str,
    args: &[relon_parser::CallArg],
    arity: u32,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<bool, LoweringError> {
    let entry = match ctx.native_imports.borrow().resolved.get(name) {
        Some(e) => e.clone(),
        None => return Ok(false),
    };
    if entry.param_tys.len() as u32 != arity {
        return Ok(false);
    }
    // Native fns take positional args only; a keyword arg means the
    // call doesn't match the host signature.
    for call_arg in args {
        if call_arg.name.is_some() {
            return Err(cap!(
                "try_lower_native_call.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!("FnCall(named-arg `{name}` for native fn)"),
                    range,
                }
            ));
        }
    }

    // 1. Capability prologue: one CheckCap per required bit, before
    //    any argument side effect.
    for bit in &entry.required_bits {
        ctx.out.push(TaggedOp {
            op: Op::CheckCap { cap_bit: *bit },
            range,
        });
    }

    // 2. Arguments, left-to-right. Each must land on the operand stack
    //    with the IR type the import declares.
    for (i, call_arg) in args.iter().enumerate() {
        lower_expr(&call_arg.value.expr, call_arg.value.range, ctx)?;
        let pushed = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "try_lower_native_call.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!("FnCall(native arg{i}-stack-empty for `{name}`)"),
                    range,
                }
            )
        })?;
        let expected = entry.param_tys[i];
        if pushed != expected {
            return Err(cap!("try_lower_native_call.unsupported_expr.3", LoweringError::UnsupportedExpr {
                kind: format!(
                    "FnCall(`{name}`) native arg {i} type mismatch: expected {expected:?}, got {pushed:?}"
                ),
                range,
            }));
        }
    }

    // 3. Intern the import + emit the call. Sentinel cap_bit: the
    //    guard already fired via the CheckCap prologue.
    let import_idx = ctx.native_imports.borrow_mut().intern(name, &entry);
    ctx.out.push(TaggedOp {
        op: Op::CallNative {
            import_idx,
            param_tys: entry.param_tys.clone(),
            ret_ty: entry.ret_ty,
            cap_bit: NO_CAPABILITY_BIT,
        },
        range,
    });
    ctx.tstack.push(entry.ret_ty);
    Ok(true)
}

fn lower_fn_call(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if path.is_empty() {
        return Err(cap!(
            "lower_fn_call.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: "FnCall(empty-path)".to_string(),
                range,
            }
        ));
    }
    // Phase F.2 (W7 anon-Dict-return): a free-call whose head is a
    // single-segment identifier may resolve to a closure-typed
    // let-binding (the anon-Dict-return path stashes each closure
    // field there). When it does, emit `LetGet { idx, Closure }` +
    // typed arg loads + `Op::CallClosure`. Falls through to the
    // stdlib dispatch when the head doesn't match a let.
    if let Some(()) = try_lower_local_closure_call(path, args, range, ctx)? {
        return Ok(());
    }
    // AOT-4 (W18 slice): materialize a runtime `range(a, b)` into a
    // `List<Int>` scratch record, run it through the bundled
    // `list_int_filter` body, then read the survivor count via
    // `ReadStringLen`. The canonical caller is the W18 prime-count
    // kernel `_len(_list_filter(range(2, n), (k) => is_prime(k, 2)))`.
    //
    // Unlike the eliding `range(...).len()` / `list.sum(range(...))`
    // peepholes below, the filtered list MUST be materialised: the
    // predicate decides survivor membership per element, and the
    // count cannot be expressed as a closed-form fold over the loop
    // counter (the survivors are a data-dependent subset). The
    // materialise + filter + length path mirrors the bundled stdlib
    // bodies (`list_int_filter` / `list_int_length`) so the record
    // layout flows through unchanged.
    if let Some(()) = try_lower_len_filter_range(path, args, range, ctx)? {
        return Ok(());
    }
    // AOT-4 (W16 slice): general `_len(xs)` / `_list_filter(xs, f)` on
    // an arbitrary `List<Int>`-typed argument (a where-bound
    // materialised range, a `#main` `List<Int>` param, or a nested
    // `_list_filter(...)` result). These run AFTER the fused
    // `_len(_list_filter(range(...)))` peephole above so that specific
    // shape keeps its single-pass lowering; here the argument is
    // lowered first and the handler only commits when it actually
    // produced a `List<Int>` handle (otherwise the speculative
    // lowering is discarded and dispatch falls through).
    if let Some(()) = try_lower_list_len(path, args, range, ctx)? {
        return Ok(());
    }
    // Stdlib tail wave: `count(xs)` — the oracle's plain element count
    // (`xs.len() as i64`) over ANY list shape; same `ReadStringLen`
    // lowering as `_len` but accepting every list IR type (all records
    // share the `[len: u32 LE]` count prefix). Non-list arguments roll
    // back and cap loudly through the generic dispatch.
    if let Some(()) = try_lower_list_count(path, args, range, ctx)? {
        return Ok(());
    }
    if let Some(()) = try_lower_list_filter(path, args, range, ctx)? {
        return Ok(());
    }
    // Stdlib tail wave: short-circuiting quantifiers `every(xs, p)` /
    // `some(xs, p)` and the `uniqueItems` scan `unique(xs)` over
    // `List<Int>` / `List<Float>` sources. Unsupported sources roll
    // back and cap loudly through the generic dispatch (no bundled
    // body is registered under the surface names).
    if let Some(()) = try_lower_list_pred(path, args, range, ctx)? {
        return Ok(());
    }
    if let Some(()) = try_lower_list_unique(path, args, range, ctx)? {
        return Ok(());
    }
    // Wave R3: general `_list_map(xs, f)` — materialise / resolve the
    // list source, then dispatch the transform closure per element
    // through the bundled `list_int_map` body (`Op::CallClosure` over the
    // proven four-way closure substrate). Sits beside `_list_filter`
    // here; the method forms `xs.map(f)` route through `stdlib_method_index`
    // at the bottom of the dispatch (both land on `list_int_map`).
    if let Some(()) = try_lower_list_map(path, args, range, ctx)? {
        return Ok(());
    }
    // review-improvement-160 bytecode M3 phase 2: peephole-desugar
    // `list.sum(range(end))` / `list.sum(range(start, end))` into an
    // explicit `Op::Loop` accumulator before normal lowering kicks in.
    //
    // Why not go through the stdlib path?  `list` is a Relon-source
    // module alias (`#import list from "std/list"`) — the tree-walker
    // resolves it dynamically through the host module loader, but IR
    // lowering has no notion of "user-imported namespace", so the
    // receiver `list` always falls through as `UnresolvedVariable`.
    // Similarly `range` is a tree-walker-only host fn with no IR stdlib
    // entry.  Together this makes the canonical hot-loop shape
    // `list.sum(range(n))` (cmp_lua W1) unreachable to the bytecode +
    // cranelift backends today.
    //
    // The desugar emits the same arithmetic the hand-written
    // `list_int_sum` body computes (i64 accumulator over `start..end`)
    // without materialising the intermediate List<Int> — bytecode does
    // not have list arenas big enough for a 10k element transient
    // anyway, and even the cranelift backend benefits from skipping the
    // record allocation.  Behaviour matches the tree-walker exactly:
    // `range(start, end)` produces the integers `[start, end)` (empty
    // when `start >= end`) and `list.sum([])` returns `0`.
    if let Some(()) = try_lower_list_sum_range(path, args, range, ctx)? {
        return Ok(());
    }
    // AOT-4 (W16 slice): general `list.sum(<List<Int> handle>)` — the
    // equal-partition fold in the W16 quicksort kernel
    // (`list.sum(_list_filter(xs, (x) => x == xs[0]))`). Runs after the
    // `list.sum(range(...))` eliding peephole so a raw range still folds
    // without materialising; here the argument is a materialised handle.
    if let Some(()) = try_lower_list_sum_value(path, args, range, ctx)? {
        return Ok(());
    }
    // Open follow-up #2: `range(start, end)[. map(...) | . filter(...)]*.len()`
    // desugars to a pure i64 count accumulator. Without this the
    // chain would lower into the buffer-protocol list pipeline and
    // miss the scalar accumulator shape the native backends need, so
    // cmp_lua W4 stays at `n/a`. The desugar fires
    // before regular method dispatch because the receiver path
    // (`range(...)` etc.) is not statically reachable as a known
    // schema in the analyzer's IR-side table — without the peephole
    // `lower_method_receiver` would emit an `UnresolvedVariable`.
    if let Some(()) = try_lower_range_chain_len(path, args, range, ctx)? {
        return Ok(());
    }
    // Open follow-up #2: `<chain>.reduce(<init>, (acc, elem) => body)`.
    // The reduce consumer is structurally similar to sum + len but
    // takes a user-supplied init expression and a 2-arg closure that
    // updates the accumulator per iteration. Covers cmp_lua W3
    // (`range(n).map((i) => "a").reduce("", (acc, s) => acc + s)`)
    // by emitting the same scalar pipeline loop with a string
    // accumulator stashed in a let-binding.
    // AOT-2: doubly-nested `range.map(range.map(...))` reduced
    // cell-by-cell. The canonical caller is the W19 matmul kernel
    // shape — `range(size).map((i) => range(size).map((j) => <cell>))
    // .reduce(0, (row_acc, row) => row_acc + <fold-of-row>)`. The
    // outer map's body is itself a *bare* inner `range(...).map(...)`
    // producing a `List<Int>` row; the outer reduce folds each row
    // cell-by-cell. This MUST run before `try_lower_range_chain_reduce`
    // — that single-level peephole would otherwise match the outer
    // `range(...).map(...).reduce(...)` and then choke lowering the
    // list-valued map body (the inner bare `range(...)` surfaces as an
    // `UnknownStdlibMethod`; `range` is a tree-walker host fn, not an
    // IR stdlib entry). This peephole emits a doubly-nested i64
    // accumulator loop with NO intermediate list materialised.
    if let Some(()) = try_lower_nested_range_map_reduce(path, args, range, ctx)? {
        return Ok(());
    }
    if let Some(()) = try_lower_range_chain_reduce(path, args, range, ctx)? {
        return Ok(());
    }
    // AOT-4 (W19 slice): `<materialised-list>.reduce(init, (acc, elem)
    // => body)` over a where-bound `List<Int>` / `List<List<Int>>`
    // handle — the W19 `#main` outer fold `c.reduce(...)` (with the
    // nested `row.reduce(...)` re-reducing each row handle). Runs after
    // the range-chain reduce so a `range(...)` receiver still elides; it
    // only fires when the receiver is a bare list-typed let.
    if let Some(()) = try_lower_materialized_list_reduce(path, args, range, ctx)? {
        return Ok(());
    }
    // Wave R3: general `_list_reduce(xs, init, f)` — fold an arbitrary
    // `List<Int>` source to an `Int` accumulator via the bundled
    // `list_int_fold` body (matches the tree-walk `ListReduce` order +
    // checked-add overflow). Runs after the `.reduce` method peepholes so
    // the eliding range-chain / nested-matmul folds keep their scalar
    // loops; this only fires for the underscore free-call form.
    if let Some(()) = try_lower_list_reduce(path, args, range, ctx)? {
        return Ok(());
    }
    // Wave R3: general `range(a, b)` as a materialised `List<Int>` value
    // (not folded inside an eliding consumer). Runs LAST among the
    // free-call peepholes so the `list.sum(range(...))` /
    // `_len(_list_filter(range(...)))` / `range(...).len()` elisions above
    // keep their allocation-free loops; only a bare `range(...)` whose
    // result is actually used as a list (returned, indexed, `.map`/
    // `.filter`/`reduce`'d) reaches here and gets a real record.
    if let Some(()) = try_lower_range_value(path, args, range, ctx)? {
        return Ok(());
    }
    // Wave R4: static const-fold of `type(v)`. In strict mode the
    // argument's type is statically known, so this lowers the argument
    // (for trap parity), discards the value, and pushes the constant
    // canonical type-name String via the shared `IrType::type_name`
    // mapping. Falls through to the cap path when the argument type is
    // not statically nameable (Wave R6 dynamic / boxed `type()`).
    if let Some(()) = try_lower_type_const(path, args, range, ctx)? {
        return Ok(());
    }
    // Final path segment is the method / function name. Earlier
    // segments either form the receiver (method-call form) or are
    // unused (free-call form has exactly one path segment).
    let method_name = match path.last().unwrap() {
        TokenKey::String(name, _, _) => name.as_str(),
        _ => {
            return Err(cap!(
                "lower_fn_call.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: "FnCall(non-string-tail-segment)".to_string(),
                    range,
                }
            ));
        }
    };
    let receiver_segments = &path[..path.len() - 1];
    // Wave R7: scalar-returning Float math (`abs`/`floor`/`ceil`/`round`/
    // `sqrt`) — operand-type-aware dispatch over the new float-intrinsic
    // bundled bodies. Runs before the generic name-keyed dispatch (which
    // caps these for a Float operand because the bundled `abs` is
    // `Int->Int` and the others had no body pre-R7). Falls through on a
    // non-numeric operand / unmatched name, which then caps loudly.
    if let Some(()) =
        peephole::try_lower_scalar_math(method_name, receiver_segments, args, range, ctx)?
    {
        return Ok(());
    }
    // JSON-Schema numeric predicates `multiple_of` / `in_range` (polymorphic
    // numeric domain — same speculative-then-dispatch discipline as the
    // scalar-math peephole). Float `multiple_of` rolls back here and caps
    // loudly through the generic dispatch.
    if let Some(()) =
        peephole::try_lower_predicate_math(method_name, receiver_segments, args, range, ctx)?
    {
        return Ok(());
    }
    // JSON-Schema `size_in_range` over a List / Dict receiver (the element /
    // entry count comes from the shared `[len: u32 LE]` header). A String
    // receiver rolls back and caps loudly (Unicode code-point count needs the
    // UTF-8 decode seam LLVM-native / wasm do not lower).
    if let Some(()) =
        peephole::try_lower_size_in_range(method_name, receiver_segments, args, range, ctx)?
    {
        return Ok(());
    }
    let arity = if receiver_segments.is_empty() {
        // Free-call form. Each `CallArg` value contributes one
        // argument; named args are rejected here (Phase 4.a/4.b
        // stdlib doesn't take keyword args).
        u32::try_from(args.len()).unwrap_or(u32::MAX)
    } else {
        // Method-call form: the receiver becomes argument 0, and any
        // additional `args` slots become the remaining arguments.
        // Phase 4.b method dispatch is limited to single-arg surface
        // methods (`length` / `is_empty`); later phases (concat /
        // substring / ...) will exercise the `1 + args.len()` path.
        1u32 + u32::try_from(args.len()).unwrap_or(u32::MAX)
    };

    // Method-call form (`s.length()`) and free-call form (`length(s)`)
    // take different dispatch paths:
    //
    //   * Method-call: lower the receiver first so we know its IR type,
    //     then resolve `(receiver_ty, method_name)` via
    //     `stdlib_method_index`. This lets the same surface name
    //     (`length`) dispatch to different bodies based on receiver
    //     (`String` -> `length`, `List<Int>` -> `list_int_length`).
    //   * Free-call: resolve directly through `stdlib_function_index`
    //     on the bare name. Free-call form deliberately doesn't go
    //     through `stdlib_method_index` so e.g. `length(xs)` for a
    //     `List<Int>` xs surfaces an `StdlibArgTypeMismatch` rather
    //     than silently routing to `list_int_length` — callers who
    //     want list length must use `xs.length()` or call
    //     `list_int_length(xs)` explicitly.
    if receiver_segments.is_empty() {
        // Free-call form.
        let Some(fn_index) = stdlib_function_index(method_name) else {
            // Not a stdlib builtin — try a host-registered native fn.
            // Stdlib wins on a name clash (builtins stay first-class);
            // a name the host registered that shadows no builtin
            // resolves here into an `Op::CheckCap`-guarded
            // `Op::CallNative`.
            if try_lower_native_call(method_name, args, arity, range, ctx)? {
                return Ok(());
            }
            return Err(cap!(
                "lower_fn_call.unknown_stdlib_method.1",
                LoweringError::UnknownStdlibMethod {
                    name: method_name.to_string(),
                    arity,
                    range,
                }
            ));
        };
        let stdlib_meta = builtin_stdlib().get(fn_index as usize).ok_or_else(|| {
            cap!(
                "lower_fn_call.unknown_stdlib_method.2",
                LoweringError::UnknownStdlibMethod {
                    name: method_name.to_string(),
                    arity,
                    range,
                }
            )
        })?;
        if (stdlib_meta.params.len() as u32) != arity {
            return Err(cap!(
                "lower_fn_call.unknown_stdlib_method.3",
                LoweringError::UnknownStdlibMethod {
                    name: method_name.to_string(),
                    arity,
                    range,
                }
            ));
        }
        for (i, call_arg) in args.iter().enumerate() {
            if call_arg.name.is_some() {
                return Err(cap!(
                    "lower_fn_call.unsupported_expr.3",
                    LoweringError::UnsupportedExpr {
                        kind: format!("FnCall(named-arg `{}` for stdlib)", method_name),
                        range,
                    }
                ));
            }
            lower_stdlib_arg(
                stdlib_meta.name,
                i as u32,
                &call_arg.value,
                &stdlib_meta.params,
                ctx,
                range,
            )?;
        }
        for _ in 0..arity {
            ctx.tstack.pop();
        }
        ctx.out.push(TaggedOp {
            op: Op::Call {
                fn_index,
                arg_count: arity,
                param_tys: stdlib_meta.params.clone(),
                ret_ty: stdlib_meta.ret,
            },
            range,
        });
        ctx.tstack.push(stdlib_meta.ret);
        return Ok(());
    }

    // Method-call form. Lower the receiver first to surface its IR
    // type and schema brand, then route to either schema-rooted
    // dispatch (when the receiver carries a brand) or the stdlib
    // method table.
    let receiver_brand = lower_method_receiver(receiver_segments, range, ctx)?;
    let receiver_ty = *ctx.tstack.last().ok_or(cap!(
        "lower_fn_call.unsupported_expr.4",
        LoweringError::UnsupportedExpr {
            kind: format!("FnCall(receiver-stack-empty for `{}`)", method_name),
            range,
        }
    ))?;
    // Schema-rooted dispatch beats the stdlib table when both could
    // resolve the call — Phase 5 keeps user methods first-class. The
    // stdlib path stays available for receivers without a brand
    // (`String::length`, `List<Int>::length`).
    if let Some(schema_name) = receiver_brand.as_deref() {
        let key = (schema_name.to_string(), method_name.to_string());
        if let Some((fn_index, param_tys, ret_ty)) = ctx
            .method_registry
            .methods
            .get(&key)
            .map(|(idx, p, r)| (*idx, p.clone(), *r))
        {
            return finish_schema_method_call(
                fn_index,
                param_tys,
                ret_ty,
                args,
                method_name,
                range,
                ctx,
            );
        }
    }
    // Wave R3b/R3c: route method-form `xs.map(f)` / `xs.filter(f)` over a
    // list receiver through the closure-return-type candidate selector
    // (the receiver is already lowered onto the vstack). This resolves the
    // element-type-changing numeric maps and the R3c String-result maps
    // that the fixed `stdlib_method_index` (`(elem -> elem)`) signature
    // cannot. `Ok(None)` falls through to the regular dispatch below (e.g.
    // a non-list receiver, or no candidate return matched), which caps it
    // loudly.
    if matches!(
        receiver_ty,
        IrType::ListInt | IrType::ListFloat | IrType::ListString
    ) && (method_name == "map" || method_name == "filter")
        && args.len() == 1
        && args[0].name.is_none()
    {
        let kind = if method_name == "map" {
            peephole::ListHofKind::Map
        } else {
            peephole::ListHofKind::Filter
        };
        if let Some(()) =
            peephole::emit_list_hof_method(kind, receiver_ty, &args[0].value, range, ctx)?
        {
            return Ok(());
        }
    }
    let Some(fn_index) = stdlib_method_index(receiver_ty, method_name) else {
        return Err(cap!(
            "lower_fn_call.unknown_stdlib_method.4",
            LoweringError::UnknownStdlibMethod {
                name: method_name.to_string(),
                arity,
                range,
            }
        ));
    };
    let stdlib_meta = builtin_stdlib().get(fn_index as usize).ok_or_else(|| {
        cap!(
            "lower_fn_call.unknown_stdlib_method.5",
            LoweringError::UnknownStdlibMethod {
                name: method_name.to_string(),
                arity,
                range,
            }
        )
    })?;
    if (stdlib_meta.params.len() as u32) != arity {
        return Err(cap!(
            "lower_fn_call.unknown_stdlib_method.6",
            LoweringError::UnknownStdlibMethod {
                name: method_name.to_string(),
                arity,
                range,
            }
        ));
    }
    // Wave R15: `s.split(sep)` is only sound to lower when the separator
    // is a provably non-empty String literal. The tree-walk oracle
    // *errors* on an empty separator (`UnsupportedOperator`, never a
    // value), and the byte-level `split` body cannot portably reproduce
    // that error (an empty separator would match at every byte position,
    // diverging from the oracle). Rather than emit a non-portable
    // `Op::Trap`, cap every case we cannot prove non-empty at lowering: a
    // non-literal separator (parameter / load / call) and the empty
    // literal both stay capped. The dominant `","` / `"--"` literal form
    // lowers four-way.
    if method_name == "split"
        && receiver_ty == IrType::String
        && !matches!(
            args.first().map(|a| &*a.value.expr),
            Some(Expr::String(s)) if !s.is_empty()
        )
    {
        return Err(cap!(
            "lower_fn_call.split_empty_separator",
            LoweringError::UnsupportedExpr {
                kind: "String.split with a separator that is not a non-empty string literal — the \
                     tree-walk oracle errors on an empty separator, so only a provably non-empty \
                     literal separator lowers byte-equal four-way"
                    .to_string(),
                range,
            }
        ));
    }
    // The receiver already sits on the vstack — re-check that its
    // slot matches the callee's declared param[0] so a future
    // dispatch entry that mistypes its receiver surfaces at lowering
    // rather than at codegen.
    let pushed_receiver = ctx.tstack.pop().ok_or(cap!(
        "lower_fn_call.unsupported_expr.5",
        LoweringError::UnsupportedExpr {
            kind: format!("FnCall(receiver-stack-empty for `{}`)", method_name),
            range,
        }
    ))?;
    check_stdlib_arg(method_name, 0, pushed_receiver, &stdlib_meta.params, range)?;
    ctx.tstack.push(pushed_receiver);
    for (i, call_arg) in args.iter().enumerate() {
        if call_arg.name.is_some() {
            return Err(cap!(
                "lower_fn_call.unsupported_expr.6",
                LoweringError::UnsupportedExpr {
                    kind: format!("FnCall(named-arg `{}` for stdlib)", method_name),
                    range,
                }
            ));
        }
        lower_stdlib_arg(
            stdlib_meta.name,
            (i + 1) as u32,
            &call_arg.value,
            &stdlib_meta.params,
            ctx,
            range,
        )?;
    }
    for _ in 0..arity {
        ctx.tstack.pop();
    }
    ctx.out.push(TaggedOp {
        op: Op::Call {
            fn_index,
            arg_count: arity,
            param_tys: stdlib_meta.params.clone(),
            ret_ty: stdlib_meta.ret,
        },
        range,
    });
    ctx.tstack.push(stdlib_meta.ret);
    Ok(())
}

/// Wave R3: lower a list comprehension `[ element for id in iterable
/// (if condition)? ]` by desugaring onto the bundled `list_*_filter`
/// then `list_*_map` higher-order bodies — the same machinery the
/// `xs.filter(...)` / `xs.map(...)` method forms use.
///
/// Semantics (matched byte-exactly to the tree-walk `Expr::Comprehension`
/// driver in `relon-evaluator::eval`): iterate `iterable`'s elements in
/// order; when a `condition` is present, keep only the elements for which
/// `condition` (evaluated with `id` bound to the element) is truthy; emit
/// `element` (evaluated with `id` bound to the element) for each surviving
/// element. Filter-then-map composes to exactly this: the filter body
/// retains the passing source elements (unchanged), then the map body
/// computes `element` from each survivor.
///
/// The loop variable `id` becomes the synthesised closure parameter, so
/// any outer reference inside `condition` / `element` (a `#main` param, a
/// where-bound value) resolves through the closure's free-variable
/// capture path exactly as a hand-written `iterable.filter((id) =>
/// condition).map((id) => element)` would.
///
/// Source-element coverage mirrors the method-form HOF emitter
/// ([`peephole::emit_list_hof_call`]): `List<Int>` and `List<Float>`
/// sources ride the 8-byte-slot numeric bodies; `List<String>` rides the
/// 4-byte pointer-array bodies. The map body is selected from the
/// closure's inferred return type so an element-type-changing map (e.g.
/// `[float(x) for x in ints]` -> `list_int_map_to_float`) lowers four-way
/// when a bundled cross-type body exists. A `List<String>` filter caps
/// (no four-way `String -> Bool` predicate body, matching the method
/// form), as does any source whose element type lacks bundled HOF bodies
/// (e.g. `List<Bool>`).
fn lower_comprehension(
    element: &Node,
    id: &str,
    iterable: &Node,
    condition: Option<&Node>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // 1. Lower the iterable to a list handle. `range(n)`, a where-bound
    //    list, a `#main` list param, and nested comprehensions / map /
    //    filter results all land here. The element type drives body
    //    selection: `ListInt`/`ListFloat` ride the 8-byte numeric bodies,
    //    `ListString` rides the 4-byte pointer-array bodies.
    lower_expr(&iterable.expr, iterable.range, ctx)?;
    let src_ty = ctx.tstack.last().copied();
    let src_elem = match src_ty {
        Some(IrType::ListInt) => IrType::I64,
        Some(IrType::ListFloat) => IrType::F64,
        Some(IrType::ListString) => IrType::String,
        _ => {
            return Err(cap!(
                "lower_comprehension.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "comprehension iterable must be a List<Int>, List<Float>, or List<String> in the AOT envelope, got {:?}",
                        src_ty
                    ),
                    range: iterable.range,
                }
            ));
        }
    };

    // Helper: synthesise a single-param closure `(id) => body` over the
    // loop variable and emit `Op::Call(<builtin>)` against a list source
    // already sitting on top of the vstack. The bundled body's fixed
    // closure signature is used directly — a body that doesn't match it is
    // a loud error, not a fall-through (used for the filter pass, whose
    // predicate signature is fixed `elem -> Bool`).
    fn emit_hof_with_synthetic_closure(
        builtin: &'static str,
        id: &str,
        body: &Node,
        range: TokenRange,
        ctx: &mut LowerCtx<'_>,
    ) -> Result<(), LoweringError> {
        let fn_idx = stdlib_function_index(builtin).ok_or_else(|| {
            cap!(
                "lower_comprehension.unknown_stdlib_method.1",
                LoweringError::UnknownStdlibMethod {
                    name: builtin.to_string(),
                    arity: 2,
                    range,
                }
            )
        })?;
        let meta = builtin_stdlib().get(fn_idx as usize).ok_or_else(|| {
            cap!(
                "lower_comprehension.unknown_stdlib_method.2",
                LoweringError::UnknownStdlibMethod {
                    name: builtin.to_string(),
                    arity: 2,
                    range,
                }
            )
        })?;
        let params = meta.params.clone();
        let ret = meta.ret;
        let (param_tys_c, ret_ty_c) =
            stdlib_closure_arg_signature(builtin, 1).ok_or_else(|| {
                cap!(
                    "lower_comprehension.unsupported_expr.2",
                    LoweringError::UnsupportedExpr {
                        kind: format!("{builtin} closure signature missing"),
                        range,
                    }
                )
            })?;
        let closure_expr = Expr::Closure {
            params: vec![relon_parser::ClosureParam {
                name: id.to_string(),
                type_hint: None,
                range: body.range,
            }],
            return_type: None,
            body: body.clone(),
        };
        lower_closure_as_value(&closure_expr, body.range, &param_tys_c, ret_ty_c, ctx)?;
        ctx.tstack.pop(); // closure
        ctx.tstack.pop(); // source list handle
        ctx.out.push(TaggedOp {
            op: Op::Call {
                fn_index: fn_idx,
                arg_count: 2,
                param_tys: params,
                ret_ty: ret,
            },
            range,
        });
        ctx.tstack.push(ret);
        Ok(())
    }

    // 2. Optional filter pass `(id) => condition`. The filter retains the
    //    passing source elements unchanged, so the body stays at the
    //    source element type. `List<String>` filter caps (no four-way
    //    `String -> Bool` predicate body), matching the method form.
    if let Some(cond) = condition {
        let filter_builtin = match src_elem {
            IrType::I64 => "list_int_filter",
            IrType::F64 => "list_float_filter",
            _ => {
                return Err(cap!(
                    "lower_comprehension.unsupported_expr.2",
                    LoweringError::UnsupportedExpr {
                        kind: "filtered List<String> comprehension is not compiled yet".to_string(),
                        range,
                    }
                ));
            }
        };
        emit_hof_with_synthetic_closure(filter_builtin, id, cond, range, ctx)?;
    }

    // 3. Map pass `(id) => element`. Probe the closure body against each
    //    candidate result type for this source — homogeneous (src -> src)
    //    first, then the cross-type widths — and select the matching
    //    bundled body, exactly as `peephole::emit_list_hof_call`'s map arm
    //    does. The candidates are mutually exclusive (the body yields one
    //    result slot), so the first probe that accepts wins. A body that
    //    matches no candidate caps loudly.
    let map_candidates: &[(IrType, &'static str)] = match src_elem {
        IrType::I64 => &[
            (IrType::I64, "list_int_map"),
            (IrType::F64, "list_int_map_to_float"),
            (IrType::String, "list_int_map_to_string"),
        ],
        IrType::F64 => &[
            (IrType::F64, "list_float_map"),
            (IrType::I64, "list_float_map_to_int"),
            (IrType::String, "list_float_map_to_string"),
        ],
        IrType::String => &[(IrType::String, "list_string_map")],
        _ => unreachable!("src_elem is I64, F64, or String"),
    };
    let mut last_err: Option<LoweringError> = None;
    for &(ret_ty, builtin) in map_candidates {
        // Roll-back discipline mirrors `peephole::try_lower_closure_with_ret`:
        // a body that produces a different result slot than `ret_ty` rolls
        // back (out / tstack / next_let_idx / lambda_table) so the next
        // candidate width is tried; any other error propagates.
        let saved_out_len = ctx.out.len();
        let saved_stack_len = ctx.tstack.len();
        let saved_next_let = ctx.next_let_idx;
        let saved_lambda_len = ctx.lambda_table.borrow().len();
        match emit_map_with_ret(builtin, id, element, ret_ty, range, ctx) {
            Ok(()) => return Ok(()),
            Err(LoweringError::StdlibArgTypeMismatch { .. }) => {
                ctx.out.truncate(saved_out_len);
                ctx.tstack.truncate(saved_stack_len);
                ctx.next_let_idx = saved_next_let;
                ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
            }
            Err(e) => {
                last_err = Some(e);
                ctx.out.truncate(saved_out_len);
                ctx.tstack.truncate(saved_stack_len);
                ctx.next_let_idx = saved_next_let;
                ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
            }
        }
    }
    // No candidate matched — cap loudly with the last concrete error (or a
    // generic unsupported-element error when every candidate rolled back).
    Err(last_err.unwrap_or_else(|| {
        cap!(
            "lower_comprehension.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "comprehension element type is not compiled yet for {:?} source",
                    src_ty
                ),
                range,
            }
        )
    }))
}

/// Map-pass helper for [`lower_comprehension`]: synthesise the
/// single-param closure `(id) => element` with the closure return pinned
/// to `ret_ty` (so the cross-type bundled bodies resolve), then emit
/// `Op::Call(<builtin>)` against the source list already on the vstack.
/// Returns `Err(StdlibArgTypeMismatch)` when the body produces a result
/// slot other than `ret_ty`, which the caller treats as a probe miss.
fn emit_map_with_ret(
    builtin: &'static str,
    id: &str,
    body: &Node,
    ret_ty: IrType,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let fn_idx = stdlib_function_index(builtin).ok_or_else(|| {
        cap!(
            "lower_comprehension.unknown_stdlib_method.1",
            LoweringError::UnknownStdlibMethod {
                name: builtin.to_string(),
                arity: 2,
                range,
            }
        )
    })?;
    let meta = builtin_stdlib().get(fn_idx as usize).ok_or_else(|| {
        cap!(
            "lower_comprehension.unknown_stdlib_method.2",
            LoweringError::UnknownStdlibMethod {
                name: builtin.to_string(),
                arity: 2,
                range,
            }
        )
    })?;
    let params = meta.params.clone();
    let ret = meta.ret;
    let (param_tys_c, _ret_ty_c) = stdlib_closure_arg_signature(builtin, 1).ok_or_else(|| {
        cap!(
            "lower_comprehension.unsupported_expr.2",
            LoweringError::UnsupportedExpr {
                kind: format!("{builtin} closure signature missing"),
                range,
            }
        )
    })?;
    let closure_expr = Expr::Closure {
        params: vec![relon_parser::ClosureParam {
            name: id.to_string(),
            type_hint: None,
            range: body.range,
        }],
        return_type: None,
        body: body.clone(),
    };
    // Pin the closure return to the candidate `ret_ty` so a body whose
    // result slot differs trips `StdlibArgTypeMismatch` (the probe miss).
    lower_closure_as_value(&closure_expr, body.range, &param_tys_c, ret_ty, ctx)?;
    ctx.tstack.pop(); // closure
    ctx.tstack.pop(); // source list handle
    ctx.out.push(TaggedOp {
        op: Op::Call {
            fn_index: fn_idx,
            arg_count: 2,
            param_tys: params,
            ret_ty: ret,
        },
        range,
    });
    ctx.tstack.push(ret);
    Ok(())
}

fn lower_comprehension_as_type(
    expected_element: &TypeRepr,
    element: &Node,
    id: &str,
    iterable: &Node,
    condition: Option<&Node>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    lower_expr(&iterable.expr, iterable.range, ctx)?;
    let src_ty = ctx.tstack.last().copied();
    let (filter_builtin, map_builtin) = match src_ty {
        Some(IrType::ListInt) => (Some("list_int_filter"), "list_int_map_to_variant_list"),
        Some(IrType::ListFloat) => (Some("list_float_filter"), "list_float_map_to_variant_list"),
        Some(IrType::ListString) => (None, "list_string_map_to_variant_list"),
        _ => {
            return Err(cap!(
                "lower_comprehension.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "comprehension iterable must be List<Int>, List<Float>, or List<String> for variant-list output, got {:?}",
                        src_ty
                    ),
                    range: iterable.range,
                }
            ))
        }
    };

    fn emit_hof_with_synthetic_closure_as_type(
        builtin: &'static str,
        id: &str,
        body: &Node,
        expected_body_ty: Option<&TypeRepr>,
        range: TokenRange,
        ctx: &mut LowerCtx<'_>,
    ) -> Result<(), LoweringError> {
        let fn_idx = stdlib_function_index(builtin).ok_or_else(|| {
            cap!(
                "lower_comprehension.unknown_stdlib_method.1",
                LoweringError::UnknownStdlibMethod {
                    name: builtin.to_string(),
                    arity: 2,
                    range,
                }
            )
        })?;
        let meta = builtin_stdlib().get(fn_idx as usize).ok_or_else(|| {
            cap!(
                "lower_comprehension.unknown_stdlib_method.2",
                LoweringError::UnknownStdlibMethod {
                    name: builtin.to_string(),
                    arity: 2,
                    range,
                }
            )
        })?;
        let params = meta.params.clone();
        let ret = meta.ret;
        let (param_tys_c, ret_ty_c) =
            stdlib_closure_arg_signature(builtin, 1).ok_or_else(|| {
                cap!(
                    "lower_comprehension.unsupported_expr.2",
                    LoweringError::UnsupportedExpr {
                        kind: format!("{builtin} closure signature missing"),
                        range,
                    }
                )
            })?;
        let closure_expr = Expr::Closure {
            params: vec![relon_parser::ClosureParam {
                name: id.to_string(),
                type_hint: None,
                range: body.range,
            }],
            return_type: None,
            body: body.clone(),
        };
        if let Some(expected) = expected_body_ty {
            lower_closure_as_value_with_expected_type(
                &closure_expr,
                body.range,
                &param_tys_c,
                ret_ty_c,
                None,
                Some(expected),
                ctx,
            )?;
        } else {
            lower_closure_as_value(&closure_expr, body.range, &param_tys_c, ret_ty_c, ctx)?;
        }
        ctx.tstack.pop();
        ctx.tstack.pop();
        ctx.out.push(TaggedOp {
            op: Op::Call {
                fn_index: fn_idx,
                arg_count: 2,
                param_tys: params,
                ret_ty: ret,
            },
            range,
        });
        ctx.tstack.push(ret);
        Ok(())
    }

    if let Some(cond) = condition {
        let Some(filter_builtin) = filter_builtin else {
            return Err(cap!(
                "lower_comprehension.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: "filtered List<String> comprehension is not compiled yet".to_string(),
                    range,
                }
            ));
        };
        emit_hof_with_synthetic_closure_as_type(filter_builtin, id, cond, None, range, ctx)?;
    }
    emit_hof_with_synthetic_closure_as_type(
        map_builtin,
        id,
        element,
        Some(expected_element),
        range,
        ctx,
    )?;
    Ok(())
}

/// AOT-4 (W16 slice): lower a 1D `xs[i]` index on a `List<Int>`
/// receiver whose arena handle is already on top of the vstack (pushed
/// by [`lower_variable`]'s head load, tagged `IrType::ListInt`).
///
/// Emits the inline payload addressing that mirrors the record layout
/// the bundled `list_int_*` bodies write
/// (`stdlib::defs::list_filter_body_typed`): `[len: u32 LE][pad: u32]
/// [i64 elements...]`, payload aligned at `(base + 4 + 7) & -8`,
/// element `i` at `payload + i*8`:
///
/// ```text
/// base    = <handle on vstack>             ; i32, stashed in a let
/// idx     = <index expr>                   ; i64 -> truncated to i32
/// payload = (base + 11) & -8               ; i32
/// addr    = payload + idx*8                 ; i32
/// push i64.load(addr)                       ; LoadI64AtAbsolute{0}
/// ```
///
/// No bounds branch is emitted — see the caller doc-comment for the
/// in-bounds rationale. The element is left on the vstack tagged
/// `IrType::I64`.
fn lower_list_int_index(
    index_node: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    lower_list_index_typed(index_node, IrType::ListInt, range, ctx)
}

/// #359 (W20): generalised 1D list index — the `List<Int>` body
/// (`elem_ty = I64`, `LoadI64AtAbsolute`) and the `List<Float>` body
/// (`elem_ty = F64`, `LoadF64AtAbsolute`) share the identical record
/// layout (`[len:u32][pad:u32][8-byte elements...]`, payload at
/// `(base + 11) & -8`, element `i` at `payload + i*8`) — only the
/// element-load op and the pushed element type differ. Pops the
/// receiver list handle (tagged `recv_ty`), pushes the element scalar
/// (`I64` for `ListInt`, `F64` for `ListFloat`).
fn lower_list_index_typed(
    index_node: &Node,
    recv_ty: IrType,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    debug_assert!(matches!(recv_ty, IrType::ListInt | IrType::ListFloat));
    let (elem_ty, load_op) = match recv_ty {
        IrType::ListFloat => (IrType::F64, Op::LoadF64AtAbsolute { offset: 0 }),
        _ => (IrType::I64, Op::LoadI64AtAbsolute { offset: 0 }),
    };
    // Reserve let slots: `base` (i32 handle) + `idx` (i32 element
    // index). Each slot is single-typed for its lifetime so the LLVM
    // emitter's `ensure_let_slot` aliasing guard stays happy.
    let base_i = ctx.next_let_idx;
    let idx_i = ctx.next_let_idx + 1;
    ctx.next_let_idx += 2;

    // Stash the receiver handle (already on the vstack as the list ty).
    let top = ctx.tstack.pop().ok_or(cap!(
        "lower_list_index_typed.unsupported_expr",
        LoweringError::UnsupportedExpr {
            kind: "Variable(list-index-empty-stack)".to_string(),
            range,
        }
    ))?;
    debug_assert_eq!(top, recv_ty);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });

    // Lower the index expression; it must be an Int (I64). Truncate
    // into the i32 `idx` slot via the type-narrowing `LetSet` (the
    // emitter's `coerce_to_let_ty` truncates I64 -> I32).
    lower_expr(&index_node.expr, index_node.range, ctx)?;
    expect_int_top(ctx, range)?;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: idx_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // payload = (base + 11) & -8
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(4 + 7),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(-8),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::BitAnd(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);

    // addr = payload + idx*8
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: idx_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(8),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Mul(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);

    // push <i64|f64>.load(addr)
    ctx.out.push(TaggedOp { op: load_op, range });
    ctx.tstack.pop(); // i32 addr
    ctx.tstack.push(elem_ty);
    Ok(())
}

/// W5-P2: lower a 1D `keys[i]` index on a `List<String>` receiver
/// (tagged `IrType::ListString`, pushed by [`lower_variable`]'s head
/// load). A `List<String>` record is a *pointer array*, NOT the inline
/// 8-byte-element shape `lower_list_index_typed` handles:
///
/// ```text
/// [len: u32 LE][off_0: u32 LE][off_1: u32 LE]...[off_{N-1}: u32 LE]
/// ```
///
/// `off_i` is the arena-relative byte offset of the i-th String record
/// (`[slen: u32 LE][utf8]`) — the exact same handle representation
/// `Op::ConstString` pushes. The header has NO 8-byte pad (every slot
/// is u32), so element `i` sits at `base + 4 + i*4`. Indexing therefore
/// loads the `u32` slot and leaves it on the vstack tagged
/// `IrType::String` — a downstream String-return tail-record copy
/// (`EmitTailRecordFromAbsoluteAddr { ty: String }`) then resolves the
/// `[slen][utf8]` record through that handle exactly as it would for a
/// `ConstString`.
///
/// Addressing (mirrors `lower_list_index_typed`'s let-stash discipline
/// but with a 4-byte stride and no align mask):
///
/// ```text
/// base = <ListString handle on vstack>      ; i32, stashed in a let
/// idx  = <index expr>                        ; i64 -> truncated to i32
/// addr = base + 4 + idx*4                     ; i32
/// push i32.load(addr)                         ; LoadI32AtAbsolute{0}
/// ```
///
/// No bounds branch — same in-bounds rationale as the int/float path
/// (`keys[i % 10]` is provably within a 10-element list).
fn lower_list_string_index(
    index_node: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let base_i = ctx.next_let_idx;
    let idx_i = ctx.next_let_idx + 1;
    ctx.next_let_idx += 2;

    // Stash the receiver handle (a `ListString` arena offset, i32).
    let top = ctx.tstack.pop().ok_or(cap!(
        "lower_list_string_index.unsupported_expr",
        LoweringError::UnsupportedExpr {
            kind: "Variable(list-string-index-empty-stack)".to_string(),
            range,
        }
    ))?;
    debug_assert_eq!(top, IrType::ListString);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });

    // Lower the index expression; require an Int (I64), truncate into
    // the i32 `idx` slot.
    lower_expr(&index_node.expr, index_node.range, ctx)?;
    expect_int_top(ctx, range)?;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: idx_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // addr = base + 4 + idx*4
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(4),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);

    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: idx_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(4),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Mul(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);

    // push i32.load(addr) -> the String handle (arena offset).
    ctx.out.push(TaggedOp {
        op: Op::LoadI32AtAbsolute { offset: 0 },
        range,
    });
    ctx.tstack.pop(); // i32 addr
    ctx.tstack.push(IrType::String);
    Ok(())
}

/// W5-P3: lower `d[k]` where `d` is a materialised `{String -> Int}`
/// dict (an `IrType::Dict` arena handle pushed by [`lower_variable`]'s
/// head load via `Op::ConstDict`) and `k` is a runtime `String` handle
/// (e.g. `keys[i]`, the same `[slen: u32][utf8]` record `ConstString`
/// pushes). The lookup runs entirely as IR-lowered primitive ops
/// (`Block`/`Loop`/`BrIf`/`LoadI32AtAbsolute`/`LoadI8UAtAbsolute`/…),
/// so it needs **no new runtime helper / wasm import**: every backend
/// (cranelift AOT, LLVM AOT, wasm32) already lowers these ops, exactly
/// as the bundled `starts_with` stdlib body does its byte compare.
///
/// ## Arena dict record (record-relative offsets — see
/// `relon-codegen-*::const_pool::visit_const_dict`)
///
/// ```text
/// [entry_count: u32 @0][pad: u32 @4][shape_hash: u64 @8]      ; 16-byte header
/// entry_count × [key_off: u32][key_len: u32][value: i64]      ; 16 bytes each,
///                                                             ;   sorted by key
/// concatenated UTF-8 key bytes                                ; key_off is
///                                                             ;   record-relative
/// ```
///
/// ## Probe (linear scan + byte compare)
///
/// ```text
/// dict_base = <Dict handle>          ; i32 arena-relative
/// kh        = <String handle from k> ; i32 arena-relative
/// klen      = load_u32(kh + 0)       ; key byte length
/// n         = load_u32(dict_base+0)  ; entry_count
/// i = 0; found = 0; result = 0
/// loop over entries:
///   if found != 0 || i >= n: break
///   eoff      = dict_base + 16 + i*16
///   ekey_len  = load_u32(eoff + 4)
///   if ekey_len == klen:
///     ekey_addr = dict_base + load_u32(eoff + 0)   ; record-relative key_off
///     j = 0; mismatch = 0
///     inner byte loop until j == klen or a byte differs
///     if mismatch == 0:               ; full match
///       result = load_i64(eoff + 8); found = 1
///   i += 1
/// if found == 0: trap(IndexOutOfBounds)   ; honest miss — no silent wrong value
/// push result (Int)
/// ```
///
/// The scan is linear (the entry table being key-sorted is not
/// required for correctness; a binary search is a future optimisation).
/// No bounds branch is needed beyond `i < n` — the probe stays within
/// the record by construction. The not-found path traps rather than
/// returning a sentinel so a miss is never silently mis-read.
fn lower_dict_string_index(
    index_node: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Pop the `Dict` receiver handle into an i32 let-local.
    let top = ctx.tstack.pop().ok_or(cap!(
        "lower_dict_string_index.unsupported_expr.1",
        LoweringError::UnsupportedExpr {
            kind: "Variable(dict-index-empty-stack)".to_string(),
            range,
        }
    ))?;
    debug_assert_eq!(top, IrType::Dict);
    let dict_base = ctx.next_let_idx;
    ctx.next_let_idx += 1;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: dict_base,
            ty: IrType::I32,
        },
        range,
    });

    // Lower the key expression; it must produce a `String` handle.
    lower_expr(&index_node.expr, index_node.range, ctx)?;
    let key_ty = ctx.tstack.pop().ok_or(cap!(
        "lower_dict_string_index.unsupported_expr.2",
        LoweringError::UnsupportedExpr {
            kind: "Variable(dict-index-key-empty-stack)".to_string(),
            range,
        }
    ))?;
    if key_ty != IrType::String {
        return Err(cap!(
            "lower_dict_string_index.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: format!("Variable(dict-index key must be String, got {key_ty:?})"),
                range,
            }
        ));
    }
    let kh = ctx.next_let_idx;
    ctx.next_let_idx += 1;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: kh,
            ty: IrType::I32,
        },
        range,
    });

    // Fresh scratch let-locals for the probe state. All i32 except the
    // i64 `result` accumulator.
    let klen = ctx.next_let_idx;
    let n = ctx.next_let_idx + 1;
    let i = ctx.next_let_idx + 2;
    let found = ctx.next_let_idx + 3;
    let ekey_len = ctx.next_let_idx + 4;
    let ekey_addr = ctx.next_let_idx + 5;
    let eoff = ctx.next_let_idx + 6;
    let j = ctx.next_let_idx + 7;
    let mismatch = ctx.next_let_idx + 8;
    let result = ctx.next_let_idx + 9;
    ctx.next_let_idx += 10;

    let i32_get = |idx: u32| TaggedOp {
        op: Op::LetGet {
            idx,
            ty: IrType::I32,
        },
        range,
    };
    let i32_set = |idx: u32| TaggedOp {
        op: Op::LetSet {
            idx,
            ty: IrType::I32,
        },
        range,
    };
    let ci32 = |v: i32| TaggedOp {
        op: Op::ConstI32(v),
        range,
    };
    let add = || TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    };
    let load_u32 = |off: u32| TaggedOp {
        op: Op::LoadI32AtAbsolute { offset: off },
        range,
    };

    // klen = load_u32(kh + 0)
    ctx.out.push(i32_get(kh));
    ctx.out.push(load_u32(0));
    ctx.out.push(i32_set(klen));

    // n = load_u32(dict_base + 0)
    ctx.out.push(i32_get(dict_base));
    ctx.out.push(load_u32(0));
    ctx.out.push(i32_set(n));

    // i = 0; found = 0; result = 0
    ctx.out.push(ci32(0));
    ctx.out.push(i32_set(i));
    ctx.out.push(ci32(0));
    ctx.out.push(i32_set(found));
    ctx.out.push(TaggedOp {
        op: Op::ConstI64(0),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: result,
            ty: IrType::I64,
        },
        range,
    });

    // Inner byte-compare loop (mismatch := first differing byte / 0 on
    // full window match). Built once and embedded into the entry body.
    let inner_compare = TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: vec![TaggedOp {
                op: Op::Loop {
                    result_ty: None,
                    body: vec![
                        // j >= klen ? br 1 (fully matched; mismatch stays 0)
                        i32_get(j),
                        i32_get(klen),
                        TaggedOp {
                            op: Op::Ge(IrType::I32),
                            range,
                        },
                        TaggedOp {
                            op: Op::BrIf { label_depth: 1 },
                            range,
                        },
                        // kb = load_i8(kh + 4 + j)
                        i32_get(kh),
                        ci32(4),
                        add(),
                        i32_get(j),
                        add(),
                        TaggedOp {
                            op: Op::LoadI8UAtAbsolute { offset: 0 },
                            range,
                        },
                        // eb = load_i8(ekey_addr + j)
                        i32_get(ekey_addr),
                        i32_get(j),
                        add(),
                        TaggedOp {
                            op: Op::LoadI8UAtAbsolute { offset: 0 },
                            range,
                        },
                        // mismatch = (kb != eb)
                        TaggedOp {
                            op: Op::Ne(IrType::I32),
                            range,
                        },
                        i32_set(mismatch),
                        // mismatch != 0 ? br 1
                        i32_get(mismatch),
                        ci32(0),
                        TaggedOp {
                            op: Op::Ne(IrType::I32),
                            range,
                        },
                        TaggedOp {
                            op: Op::BrIf { label_depth: 1 },
                            range,
                        },
                        // j = j + 1
                        i32_get(j),
                        ci32(1),
                        add(),
                        i32_set(j),
                        TaggedOp {
                            op: Op::Br { label_depth: 0 },
                            range,
                        },
                    ],
                },
                range,
            }],
        },
        range,
    };

    // Per-entry body: compute eoff, compare key length, then bytes.
    let entry_body = vec![
        // found != 0 ? br 1
        i32_get(found),
        ci32(0),
        TaggedOp {
            op: Op::Ne(IrType::I32),
            range,
        },
        TaggedOp {
            op: Op::BrIf { label_depth: 1 },
            range,
        },
        // i >= n ? br 1
        i32_get(i),
        i32_get(n),
        TaggedOp {
            op: Op::Ge(IrType::I32),
            range,
        },
        TaggedOp {
            op: Op::BrIf { label_depth: 1 },
            range,
        },
        // eoff = dict_base + 16 + i*16
        i32_get(dict_base),
        ci32(16),
        add(),
        i32_get(i),
        ci32(16),
        TaggedOp {
            op: Op::Mul(IrType::I32),
            range,
        },
        add(),
        i32_set(eoff),
        // ekey_len = load_u32(eoff + 4)
        i32_get(eoff),
        load_u32(4),
        i32_set(ekey_len),
        // `if ekey_len == klen { compare bytes }`, expressed as a
        // stack-neutral skip-block: branch past the body when the key
        // lengths differ (`Op::If` requires both branches to push a
        // value, so a structured `Block { BrIf 0; body }` is the clean
        // void-conditional form here).
        TaggedOp {
            op: Op::Block {
                result_ty: None,
                body: vec![
                    // ekey_len != klen ? br 0 (skip — length mismatch)
                    i32_get(ekey_len),
                    i32_get(klen),
                    TaggedOp {
                        op: Op::Ne(IrType::I32),
                        range,
                    },
                    TaggedOp {
                        op: Op::BrIf { label_depth: 0 },
                        range,
                    },
                    // ekey_addr = dict_base + load_u32(eoff + 0)
                    i32_get(dict_base),
                    i32_get(eoff),
                    load_u32(0),
                    add(),
                    i32_set(ekey_addr),
                    // j = 0; mismatch = 0
                    ci32(0),
                    i32_set(j),
                    ci32(0),
                    i32_set(mismatch),
                    inner_compare,
                    // mismatch != 0 ? br 0 (skip — bytes differ)
                    i32_get(mismatch),
                    ci32(0),
                    TaggedOp {
                        op: Op::Ne(IrType::I32),
                        range,
                    },
                    TaggedOp {
                        op: Op::BrIf { label_depth: 0 },
                        range,
                    },
                    // full match: result = load_i64(eoff+8); found = 1
                    i32_get(eoff),
                    TaggedOp {
                        op: Op::LoadI64AtAbsolute { offset: 8 },
                        range,
                    },
                    TaggedOp {
                        op: Op::LetSet {
                            idx: result,
                            ty: IrType::I64,
                        },
                        range,
                    },
                    ci32(1),
                    i32_set(found),
                ],
            },
            range,
        },
        // i = i + 1
        i32_get(i),
        ci32(1),
        add(),
        i32_set(i),
        TaggedOp {
            op: Op::Br { label_depth: 0 },
            range,
        },
    ];

    // Outer scan: Block { Loop { entry_body } }. Stack-neutral.
    ctx.out.push(TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: vec![TaggedOp {
                op: Op::Loop {
                    result_ty: None,
                    body: entry_body,
                },
                range,
            }],
        },
        range,
    });

    // Honest miss: trap if no entry matched. w5 always hits, but a
    // missing key must never surface a silent wrong value. Expressed
    // as a skip-block: branch past the trap when `found != 0`.
    ctx.out.push(TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: vec![
                // found != 0 ? br 0 (hit — skip the trap)
                i32_get(found),
                ci32(0),
                TaggedOp {
                    op: Op::Ne(IrType::I32),
                    range,
                },
                TaggedOp {
                    op: Op::BrIf { label_depth: 0 },
                    range,
                },
                TaggedOp {
                    op: Op::Trap {
                        kind: TrapKind::IndexOutOfBounds,
                    },
                    range,
                },
            ],
        },
        range,
    });

    // Push the i64 value result as an Int.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: result,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.push(IrType::I64);
    Ok(())
}

