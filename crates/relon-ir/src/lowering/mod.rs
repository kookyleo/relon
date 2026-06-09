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

use closure::{lower_closure_as_value, lower_closure_as_value_with_expected_type};
use peephole::{
    emit_list_float_literal_materialize, emit_list_int_literal_materialize,
    emit_list_value_materialize, flatten_list_spread, list_has_computed_element, list_has_spread,
    list_is_float_shaped, match_bare_range, match_materializable_outer_map, probe_expr_ir_ty,
    try_lower_len_filter_range, try_lower_list_filter, try_lower_list_len, try_lower_list_map,
    try_lower_list_reduce, try_lower_list_sum_range, try_lower_list_sum_value,
    try_lower_materialized_list_reduce, try_lower_nested_range_map_reduce,
    try_lower_range_chain_len, try_lower_range_chain_reduce, try_lower_range_value,
    try_lower_type_const,
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
/// matching the W7 production source shape (`fib: (k) => ...`); a
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

    let mut fields: Vec<AnonDictField> = Vec::with_capacity(pairs.len());
    let mut closure_field_sigs: HashMap<&str, (Vec<IrType>, IrType)> = HashMap::new();
    // W5-P3: `{String -> Int}` dict fields seen so far, so a later
    // sibling field's `d[k]` index classifies to the dict's `Int`
    // value type. Source order makes `d` visible before `result`.
    let mut dict_field_names: HashSet<&str> = HashSet::new();
    // R10: host-visible scalar fields classified so far, name -> IR
    // type, in source declaration order. A backward `&sibling.<name>`
    // (or entry-level `&root.<name>`, which is the same — the entry
    // dict IS the root) classifies to the earlier field's scalar type.
    // Source order is what makes "backward" well-defined; a forward
    // reference simply isn't in the map yet and caps cleanly.
    let mut scalar_field_irts: HashMap<&str, IrType> = HashMap::new();

    for (key, value) in pairs {
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
            Expr::Closure { params, .. } => {
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
                // Default each unannotated param to I64; honor a
                // `(k: Bool) =>` style annotation when the user
                // supplied one.
                let mut param_irts: Vec<IrType> = Vec::with_capacity(params.len());
                for p in params {
                    let irt = p
                        .type_hint
                        .as_ref()
                        .and_then(type_node_to_canonical)
                        .and_then(|r| type_repr_to_ir_type(&r).ok())
                        .unwrap_or(IrType::I64);
                    param_irts.push(irt);
                }
                // Ret type today defaults to I64 (W7 fib returns Int).
                // Future Phase D scope: derive from body inference or
                // user annotation. Acceptable for the production W7
                // surface (`(k) => k < 2 ? k : fib(k-1) + fib(k-2)`).
                let ret_ty = IrType::I64;
                closure_field_sigs.insert(name.as_str(), (param_irts.clone(), ret_ty));
                fields.push(AnonDictField::Closure {
                    name: name.clone(),
                    param_tys: param_irts,
                    ret_ty,
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
                fields.push(AnonDictField::DictStrInt {
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
                    fields.push(AnonDictField::ListString {
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
                    let list_ty =
                        classify_anon_dict_list_field(items, value.range, name, resolver)?;
                    fields.push(AnonDictField::Scalar {
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
                fields.push(AnonDictField::CrossRegionParamList {
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
                fields.push(AnonDictField::Scalar {
                    name: name.clone(),
                    ty,
                });
            }
        }
    }

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
    Ok(Some(AnonDictPlan { schema, fields }))
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
        // R10: a backward static sibling/root reference to an earlier
        // scalar field. At the entry-level dict (which IS the document
        // root) `&sibling.<name>` and `&root.<name>` resolve to the
        // same field, so both bases classify here. Only a single static
        // `String` trailing segment naming an already-classified
        // host-visible scalar field is accepted; positional bases
        // (Uncle/Prev/Next/Index/This), forward names, dynamic keys and
        // multi-segment paths fall through to the loud cap below.
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
                        "AnonDictReturn(field `{}`: backward sibling/root reference {:?} \
                         does not name an earlier host-visible scalar field)",
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

/// Reverse of `type_repr_to_ir_type` for the scalar / String cases
/// needed by [`anon_dict_return_plan`]. Returns `None` for IR types
/// that have no anon-Dict-return canonical form (lists, schemas,
/// unit, closure).
fn ir_type_to_type_repr(t: IrType) -> Option<TypeRepr> {
    match t {
        IrType::I64 => Some(TypeRepr::Int),
        IrType::F64 => Some(TypeRepr::Float),
        IrType::Bool => Some(TypeRepr::Bool),
        IrType::String => Some(TypeRepr::String),
        IrType::Unit => Some(TypeRepr::Unit),
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

    // Index plan-scalar fields against the layout (which only sees
    // those scalar entries). Layout walks `schema.fields` in
    // declaration order so the i-th scalar plan field maps to the
    // i-th layout field.
    let mut scalar_layout_idx: usize = 0;

    for plan_field in &plan.fields {
        match plan_field {
            AnonDictField::Closure {
                name,
                param_tys,
                ret_ty,
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
                let layout_field = layout.fields.get(scalar_layout_idx).ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.5",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "AnonDictReturn(scalar field `{}`: layout index out of range)",
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
                scalar_layout_idx += 1;
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
                let layout_field = layout.fields.get(scalar_layout_idx).ok_or_else(|| {
                    cap!(
                        "lower_anon_dict_body.unsupported_expr.8",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                            "AnonDictReturn(cross-region field `{}`: layout index out of range)",
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
                scalar_layout_idx += 1;
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
    if let Some(()) = try_lower_list_filter(path, args, range, ctx)? {
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
    // hit the bytecode VM's scalar-envelope rejection at backend
    // build time, so cmp_lua W4 stays at `n/a`. The desugar fires
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
/// (if condition)? ]` by desugaring onto the bundled `list_int_filter`
/// then `list_int_map` higher-order bodies — the same machinery the
/// `xs.filter(...)` / `xs.map(...)` method forms use.
///
/// Semantics (matched byte-exactly to the tree-walk `Expr::Comprehension`
/// driver in `relon-evaluator::eval`): iterate `iterable`'s elements in
/// order; when a `condition` is present, keep only the elements for which
/// `condition` (evaluated with `id` bound to the element) is truthy; emit
/// `element` (evaluated with `id` bound to the element) for each surviving
/// element. Filter-then-map composes to exactly this: `list_int_filter`
/// retains the passing source elements (unchanged), then `list_int_map`
/// computes `element` from each survivor.
///
/// The loop variable `id` becomes the synthesised closure parameter, so
/// any outer reference inside `condition` / `element` (a `#main` param, a
/// where-bound value) resolves through the closure's free-variable
/// capture path exactly as a hand-written `iterable.filter((id) =>
/// condition).map((id) => element)` would. Only the `List<Int>` element
/// shape is supported in the AOT envelope today (the bundled HOF bodies
/// are i64-typed); other element shapes stay capped.
fn lower_comprehension(
    element: &Node,
    id: &str,
    iterable: &Node,
    condition: Option<&Node>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // 1. Lower the iterable to a `List<Int>` handle. `range(n)`, a
    //    where-bound list, a `#main` `List<Int>` param, and nested
    //    comprehensions / map / filter results all land here as
    //    `IrType::ListInt`.
    lower_expr(&iterable.expr, iterable.range, ctx)?;
    let src_ty = ctx.tstack.last().copied();
    if src_ty != Some(IrType::ListInt) {
        return Err(cap!(
            "lower_comprehension.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "comprehension iterable must be a List<Int> in the AOT envelope, got {:?}",
                    src_ty
                ),
                range: iterable.range,
            }
        ));
    }

    // Helper: synthesise a single-param closure `(id) => body` over the
    // loop variable and emit `Op::Call(<builtin>)` against a list source
    // already sitting on top of the vstack.
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

    // 2. Optional filter pass `(id) => condition`.
    if let Some(cond) = condition {
        emit_hof_with_synthetic_closure("list_int_filter", id, cond, range, ctx)?;
    }
    // 3. Map pass `(id) => element`.
    emit_hof_with_synthetic_closure("list_int_map", id, element, range, ctx)?;
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
/// (`stdlib::defs::list_int_filter_body`): `[len: u32 LE][pad: u32]
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

/// Pop the current vstack head and require it to be `I64`.  Used by
/// the `list.sum(range(...))` desugar to defend against the inner
/// argument exprs lowering to a non-i64 slot — analyzer typing should
/// have caught this earlier, but the desugar emits raw arithmetic so a
/// drift would silently corrupt subsequent ops.
fn expect_int_top(ctx: &mut LowerCtx<'_>, range: TokenRange) -> Result<(), LoweringError> {
    match ctx.tstack.last().copied() {
        Some(IrType::I64) => Ok(()),
        Some(other) => Err(cap!(
            "expect_int_top.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "list.sum(range(...)) desugar requires Int args, got {:?}",
                    other
                ),
                range,
            }
        )),
        None => Err(cap!(
            "expect_int_top.unsupported_expr.2",
            LoweringError::UnsupportedExpr {
                kind: "list.sum(range(...)) desugar saw empty vstack".to_string(),
                range,
            }
        )),
    }
}

/// Phase 10-a: lower one argument to a stdlib call, routing closure
/// expressions through [`lower_closure_as_value`] when the matching
/// param slot is `IrType::Closure`. Validates the resulting IR slot
/// against the callee's declared param type and surfaces a
/// `StdlibArgTypeMismatch` when the slots disagree.
fn lower_stdlib_arg(
    name: &str,
    arg_idx: u32,
    value: &Node,
    param_tys: &[IrType],
    ctx: &mut LowerCtx<'_>,
    call_range: TokenRange,
) -> Result<(), LoweringError> {
    let expected = *param_tys.get(arg_idx as usize).ok_or_else(|| {
        cap!(
            "lower_stdlib_arg.unknown_stdlib_method",
            LoweringError::UnknownStdlibMethod {
                name: name.to_string(),
                arity: param_tys.len() as u32,
                range: call_range,
            }
        )
    })?;
    if expected == IrType::Closure {
        // Closure surface: the value expression must be a literal
        // lambda. Any other shape (a Variable referencing a closure,
        // a stdlib-returned closure) is out of scope for Phase 10-a.
        if let Expr::Closure { .. } = &*value.expr {
            let (param_tys_c, ret_ty_c) =
                stdlib_closure_arg_signature(name, arg_idx).ok_or_else(|| {
                    cap!(
                        "lower_stdlib_arg.unsupported_expr.1",
                        LoweringError::UnsupportedExpr {
                            kind: format!(
                                "FnCall(`{}`) arg {} is Closure but no signature side-table entry",
                                name, arg_idx
                            ),
                            range: call_range,
                        }
                    )
                })?;
            lower_closure_as_value(&value.expr, value.range, &param_tys_c, ret_ty_c, ctx)?;
        } else {
            return Err(cap!(
                "lower_stdlib_arg.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "FnCall(`{}`) arg {} expected Closure literal, got `{}`",
                        name,
                        arg_idx,
                        value.expr.kind()
                    ),
                    range: value.range,
                }
            ));
        }
    } else {
        lower_expr(&value.expr, value.range, ctx)?;
    }
    let pushed = ctx.tstack.pop().ok_or_else(|| {
        cap!(
            "lower_stdlib_arg.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: format!("FnCall(arg{}-stack-empty for `{}`)", arg_idx, name),
                range: call_range,
            }
        )
    })?;
    check_stdlib_arg(name, arg_idx, pushed, param_tys, call_range)?;
    ctx.tstack.push(pushed);
    Ok(())
}

/// Lower the receiver of a method-call into the top of the virtual
/// stack. Returns the schema brand of the receiver (the schema name
/// the analyzer would dispatch against) when one is statically
/// resolvable; `None` for scalar / String / List<Int> / sub-expression
/// receivers. The caller uses the brand to decide whether to route
/// through the schema-method registry or the stdlib method table.
fn lower_method_receiver(
    receiver_segments: &[TokenKey],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<String>, LoweringError> {
    // Single-segment receivers: bare identifier (`s.length()`) or
    // parenthesised sub-expression (`("hi").length()`).
    if receiver_segments.len() == 1 {
        match &receiver_segments[0] {
            TokenKey::String(name, _, _) => {
                let brand = resolve_receiver_schema_brand(receiver_segments, ctx);
                lower_variable(receiver_segments, range, ctx)?;
                // Source-form check covers static `Schema.method(...)`
                // — when the head names a schema directly, the brand
                // returned above is the schema name; no extra work.
                let _ = name;
                return Ok(brand);
            }
            TokenKey::Dynamic(node, _) => {
                lower_expr(&node.expr, node.range, ctx)?;
                return Ok(None);
            }
            _ => {
                return Err(cap!(
                    "lower_method_receiver.unsupported_expr.1",
                    LoweringError::UnsupportedExpr {
                        kind: "FnCall(unsupported-receiver-key)".to_string(),
                        range,
                    }
                ));
            }
        }
    }
    // Multi-segment receivers (`obj.sub.method()` → segments=[obj, sub]).
    // We route through `lower_variable` so the chained field walk +
    // schema-brand inheritance kick in. The brand of the final field
    // segment is the receiver brand; resolving it stays a static walk
    // so the caller can re-key the registry without extra runtime
    // information.
    if receiver_segments
        .iter()
        .all(|seg| matches!(seg, TokenKey::String(_, _, _)))
    {
        let brand = resolve_receiver_schema_brand(receiver_segments, ctx);
        lower_variable(receiver_segments, range, ctx)?;
        return Ok(brand);
    }
    Err(cap!(
        "lower_method_receiver.unsupported_expr.2",
        LoweringError::UnsupportedExpr {
            kind: format!(
                "FnCall(multi-segment-receiver, segments={})",
                receiver_segments.len()
            ),
            range,
        }
    ))
}

/// Statically inspect the receiver path against the current lowering
/// context and return the schema brand that the receiver's tail
/// segment would carry. Used by [`lower_method_receiver`] to decide
/// whether `obj.method()` should hit the schema-method registry.
/// Returns `None` when the receiver is not statically schema-typed.
fn resolve_receiver_schema_brand(
    receiver_segments: &[TokenKey],
    ctx: &LowerCtx<'_>,
) -> Option<String> {
    let head = match receiver_segments.first()? {
        TokenKey::String(s, _, _) => s.as_str(),
        _ => return None,
    };
    // Determine the head's schema shape (if any) from the in-scope
    // bindings: `self`, then let-bindings (innermost first), then
    // method params, then `#main` params, then a static schema name.
    let mut current_schema: Option<Schema> = if let Some(self_b) = ctx.self_binding.as_ref() {
        if head == "self" {
            Some(self_b.schema.clone())
        } else if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head) {
            b.schema_brand
                .as_deref()
                .and_then(|n| ctx.schema_resolver.resolve(n))
                .and_then(|def| {
                    let mut stack: Vec<&str> = Vec::new();
                    canonical_schema_from_def(def, &ctx.schema_resolver, &mut stack, def.range).ok()
                })
        } else if let Some(p) = ctx.method_params.iter().find(|p| p.name == head) {
            p.schema.clone()
        } else {
            None
        }
    } else if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head) {
        b.schema_brand
            .as_deref()
            .and_then(|n| ctx.schema_resolver.resolve(n))
            .and_then(|def| {
                let mut stack: Vec<&str> = Vec::new();
                canonical_schema_from_def(def, &ctx.schema_resolver, &mut stack, def.range).ok()
            })
    } else if let Some(p) = ctx.params.iter().find(|b| b.name == head) {
        p.schema.clone()
    } else {
        // Static `Schema.method(...)` form: the head names a schema
        // directly. The walker treats the schema name itself as the
        // brand; no `self` instance is on the stack so emitting the
        // call from this form would still need an instance — for
        // Phase 5 we leave this surface unsupported and return
        // `None`, falling through to stdlib dispatch.
        return None;
    };
    // Walk any chained field segments to find the brand of the final
    // segment.
    for seg in receiver_segments[1..].iter() {
        let TokenKey::String(name, _, _) = seg else {
            return None;
        };
        let schema = current_schema.take()?;
        let field = schema.fields.iter().find(|f| &f.name == name)?;
        current_schema = match &field.ty {
            TypeRepr::Schema { schema } => Some((**schema).clone()),
            _ => None,
        };
    }
    current_schema.map(|s| s.name)
}

/// Finish a schema-method `Op::Call` emit: validate / lower
/// non-receiver args, type-check each against the method's
/// signature, then emit the call op with the resolved fn_index.
/// Assumes the receiver is already on top of the vstack.
fn finish_schema_method_call(
    fn_index: u32,
    param_tys: Vec<IrType>,
    ret_ty: IrType,
    args: &[relon_parser::CallArg],
    method_name: &str,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let arity = param_tys.len() as u32;
    let expected_args = arity.saturating_sub(1) as usize;
    if args.len() != expected_args {
        return Err(cap!(
            "finish_schema_method_call.unknown_stdlib_method",
            LoweringError::UnknownStdlibMethod {
                name: method_name.to_string(),
                arity,
                range,
            }
        ));
    }
    // Validate the receiver slot against param[0].
    let pushed_receiver = ctx.tstack.pop().ok_or(cap!(
        "finish_schema_method_call.unsupported_expr.1",
        LoweringError::UnsupportedExpr {
            kind: format!("FnCall(receiver-stack-empty for `{}`)", method_name),
            range,
        }
    ))?;
    if pushed_receiver.wasm_slot() != param_tys[0].wasm_slot() {
        return Err(cap!(
            "finish_schema_method_call.stdlib_arg_type_mismatch.1",
            LoweringError::StdlibArgTypeMismatch {
                name: method_name.to_string(),
                arg_idx: 0,
                got: pushed_receiver,
                expected: param_tys[0],
                range,
            }
        ));
    }
    ctx.tstack.push(pushed_receiver);
    for (i, call_arg) in args.iter().enumerate() {
        if call_arg.name.is_some() {
            return Err(cap!(
                "finish_schema_method_call.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!("FnCall(named-arg `{}` for schema method)", method_name),
                    range,
                }
            ));
        }
        lower_expr(&call_arg.value.expr, call_arg.value.range, ctx)?;
        let pushed = ctx.tstack.pop().ok_or(cap!(
            "finish_schema_method_call.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: format!("FnCall(arg{}-stack-empty for `{}`)", i + 1, method_name),
                range,
            }
        ))?;
        let expected = param_tys[i + 1];
        if pushed.wasm_slot() != expected.wasm_slot() {
            return Err(cap!(
                "finish_schema_method_call.stdlib_arg_type_mismatch.2",
                LoweringError::StdlibArgTypeMismatch {
                    name: method_name.to_string(),
                    arg_idx: (i + 1) as u32,
                    got: pushed,
                    expected,
                    range,
                }
            ));
        }
        ctx.tstack.push(pushed);
    }
    for _ in 0..arity {
        ctx.tstack.pop();
    }
    ctx.out.push(TaggedOp {
        op: Op::Call {
            fn_index,
            arg_count: arity,
            param_tys: param_tys.clone(),
            ret_ty,
        },
        range,
    });
    ctx.tstack.push(ret_ty);
    Ok(())
}

/// Validate a single argument's IR type against the stdlib
/// function's declared signature. We compare the **wasm slot** rather
/// than the exact IR type so a `String` argument (i32 slot) doesn't
/// require the caller to disambiguate from other i32-shaped types
/// upstream — the analyzer already enforces stronger typing, and the
/// codegen layer treats slot equivalence as the bytecode-correctness
/// invariant.
fn check_stdlib_arg(
    name: &str,
    arg_idx: u32,
    got: IrType,
    param_tys: &[IrType],
    range: TokenRange,
) -> Result<(), LoweringError> {
    let expected = *param_tys.get(arg_idx as usize).ok_or_else(|| {
        cap!(
            "check_stdlib_arg.unknown_stdlib_method",
            LoweringError::UnknownStdlibMethod {
                name: name.to_string(),
                arity: param_tys.len() as u32,
                range,
            }
        )
    })?;
    if got.wasm_slot() != expected.wasm_slot() {
        return Err(cap!(
            "check_stdlib_arg.stdlib_arg_type_mismatch",
            LoweringError::StdlibArgTypeMismatch {
                name: name.to_string(),
                arg_idx,
                got,
                expected,
                range,
            }
        ));
    }
    Ok(())
}

/// AOT-4: infer the IR return type of a where-bound helper closure
/// from its body, used when no explicit `: Type` annotation is
/// present. The inference is intentionally narrow — it only needs to
/// distinguish `Bool` (W18 `is_prime`) from the `Int`/I64 default
/// (W17 `bs`):
///
///   * a `Bool` / comparison / logical expression -> `Bool`;
///   * a ternary -> the type of whichever branch is determinable
///     without recursing into the helper itself (so the recursive arm,
///     which has no fixed type yet, doesn't dominate the inference);
///   * everything else -> the `Int` (I64) default.
///
/// This keeps the self-recursive call's return type in agreement with
/// the sibling literal branches, which is what `lower_ternary`'s
/// `IfBranchTypeMismatch` check enforces.
fn infer_closure_body_ret_ty(expr: &Expr) -> IrType {
    match expr {
        Expr::Bool(_) => IrType::Bool,
        Expr::Binary(op, _, _) if operator_yields_bool(*op) => IrType::Bool,
        Expr::Unary(Operator::Not, _) => IrType::Bool,
        Expr::Ternary { then, els, .. } => {
            // Prefer the branch that resolves to a concrete scalar
            // type — the recursive arm typically falls through to the
            // I64 default, so a definite Bool on either side wins.
            let then_ty = infer_closure_body_ret_ty(&then.expr);
            let else_ty = infer_closure_body_ret_ty(&els.expr);
            if then_ty == IrType::Bool || else_ty == IrType::Bool {
                IrType::Bool
            } else {
                then_ty
            }
        }
        _ => IrType::I64,
    }
}

/// #359 (W20): ctx-aware closure body return-type inference. Extends
/// [`infer_closure_body_ret_ty`] (Bool / Int) to also resolve `F64`
/// and `ListFloat`:
///
///   * a `[...]` list literal whose first element is Float-valued
///     (a Float literal, a list-typed param index, or Float arith)
///     -> `ListFloat` (W20 `step` returns an 8-element `List<Float>`);
///   * a Float literal / Float-typed where-binding / index into a
///     `ListFloat` param / a call into a sibling closure that returns
///     `F64` / arithmetic over any of those -> `F64` (W20 `pair_force`
///     and `accel`);
///   * everything else falls back to the structural Bool / Int walk.
///
/// `param_irts` / `params` describe THIS closure's own params (so an
/// indexed `s[k]` on a `ListFloat` param resolves to a Float element);
/// `ctx` supplies sibling closure signatures + outer where-bound let
/// types.
fn infer_closure_body_ret_ty_ctx(
    expr: &Expr,
    param_irts: &[IrType],
    params: &[ClosureParam],
    ctx: &LowerCtx<'_>,
) -> IrType {
    // List literal -> ListInt / ListFloat based on the first element.
    if let Expr::List(items) = expr {
        if let Some(first) = items.first() {
            return match infer_scalar_expr_ir_ty(&first.expr, param_irts, params, ctx) {
                Some(IrType::F64) => IrType::ListFloat,
                Some(IrType::I64) => IrType::ListInt,
                _ => IrType::ListInt,
            };
        }
        return IrType::ListInt;
    }
    // Scalar / Float resolution first (so a Float ternary like
    // `i == j ? 0.0 : <float arith>` reports F64 rather than the
    // structural Int default).
    if let Some(t) = infer_scalar_expr_ir_ty(expr, param_irts, params, ctx) {
        if t == IrType::F64 {
            return IrType::F64;
        }
    }
    // Fall back to the original structural Bool / Int walk.
    infer_closure_body_ret_ty(expr)
}

/// #359 (W20): best-effort scalar IR-type inference for a closure body
/// sub-expression, resolving `F64` vs `I64` (other shapes return
/// `None`). Used by the ctx-aware return-type / list-literal inference.
/// Conservative: only commits when the shape is unambiguous.
fn infer_scalar_expr_ir_ty(
    expr: &Expr,
    param_irts: &[IrType],
    params: &[ClosureParam],
    ctx: &LowerCtx<'_>,
) -> Option<IrType> {
    match expr {
        Expr::Float(_) => Some(IrType::F64),
        Expr::Int(_) => Some(IrType::I64),
        Expr::Variable(path) | Expr::Reference { path, .. } => {
            // Bare name: a where-bound let or a sibling param.
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                if let Some(b) = ctx.lets.iter().rev().find(|b| &b.name == name) {
                    return scalar_of(b.ty);
                }
                if let Some(pos) = params.iter().position(|p| &p.name == name) {
                    return scalar_of(param_irts[pos]);
                }
                return None;
            }
            // `s[k]` index on a list-typed param -> element scalar type.
            if path.len() == 2 {
                if let (TokenKey::String(name, _, _), TokenKey::Dynamic(_, _)) =
                    (&path[0], &path[1])
                {
                    if let Some(pos) = params.iter().position(|p| &p.name == name) {
                        return match param_irts[pos] {
                            IrType::ListFloat => Some(IrType::F64),
                            IrType::ListInt => Some(IrType::I64),
                            _ => None,
                        };
                    }
                }
            }
            None
        }
        Expr::FnCall { path, .. } => {
            // A call into a sibling where-bound closure adopts its
            // declared return type.
            if let [TokenKey::String(name, _, _)] = path.as_slice() {
                if let Some(b) = ctx.lets.iter().rev().find(|b| &b.name == name) {
                    if b.ty == IrType::Closure {
                        if let Some((_, ret)) = ctx.closure_let_signatures.get(&b.idx) {
                            return scalar_of(*ret);
                        }
                    }
                }
            }
            None
        }
        Expr::Binary(op, l, r) => {
            if operator_yields_bool(*op) {
                return Some(IrType::Bool);
            }
            let lt = infer_scalar_expr_ir_ty(&l.expr, param_irts, params, ctx);
            let rt = infer_scalar_expr_ir_ty(&r.expr, param_irts, params, ctx);
            match (lt, rt) {
                // Float dominates (mirrors the runtime Int->Float
                // promotion the Part A `ConvertI64ToF64` op implements).
                (Some(IrType::F64), _) | (_, Some(IrType::F64)) => Some(IrType::F64),
                (Some(IrType::I64), Some(IrType::I64)) => Some(IrType::I64),
                _ => None,
            }
        }
        Expr::Ternary { then, els, .. } => {
            let tt = infer_scalar_expr_ir_ty(&then.expr, param_irts, params, ctx);
            let et = infer_scalar_expr_ir_ty(&els.expr, param_irts, params, ctx);
            match (tt, et) {
                (Some(IrType::F64), _) | (_, Some(IrType::F64)) => Some(IrType::F64),
                (Some(IrType::I64), Some(IrType::I64)) => Some(IrType::I64),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Narrow an [`IrType`] to its scalar `F64` / `I64` form for the
/// inference helpers; other shapes are `None` (not scalar-relevant).
fn scalar_of(t: IrType) -> Option<IrType> {
    match t {
        IrType::F64 => Some(IrType::F64),
        IrType::I64 => Some(IrType::I64),
        _ => None,
    }
}

/// #359 (W20): infer a closure param's IR type from how a sibling
/// where-bound closure call uses it. When the body contains a call
/// `f(.., name, ..)` where `f` is a previously-bound closure whose
/// declared param at that position is a List* / scalar type, the
/// param `name` adopts that type. Drives `accel(s, i)`'s `s` to
/// `ListFloat` (it passes `s` as `pair_force`'s first arg without ever
/// indexing it directly). Returns `None` when no such call pins it.
fn infer_param_from_sibling_call(name: &str, body: &Expr, ctx: &LowerCtx<'_>) -> Option<IrType> {
    fn walk(name: &str, expr: &Expr, ctx: &LowerCtx<'_>) -> Option<IrType> {
        match expr {
            Expr::FnCall { path, args } => {
                if let [TokenKey::String(callee, _, _)] = path.as_slice() {
                    if let Some(b) = ctx.lets.iter().rev().find(|b| &b.name == callee) {
                        if b.ty == IrType::Closure {
                            if let Some((param_tys, _)) = ctx.closure_let_signatures.get(&b.idx) {
                                for (i, a) in args.iter().enumerate() {
                                    if a.name.is_none() && expr_is_bare_named(&a.value.expr, name) {
                                        if let Some(t) = param_tys.get(i) {
                                            return Some(*t);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Recurse into args for nested calls.
                args.iter().find_map(|a| walk(name, &a.value.expr, ctx))
            }
            Expr::Binary(_, l, r) => walk(name, &l.expr, ctx).or_else(|| walk(name, &r.expr, ctx)),
            Expr::Unary(_, n) => walk(name, &n.expr, ctx),
            Expr::Ternary { cond, then, els } => walk(name, &cond.expr, ctx)
                .or_else(|| walk(name, &then.expr, ctx))
                .or_else(|| walk(name, &els.expr, ctx)),
            Expr::List(items) => items.iter().find_map(|n| walk(name, &n.expr, ctx)),
            _ => None,
        }
    }
    walk(name, body, ctx)
}

/// `true` when `expr` is the bare variable `name` (a single-segment
/// `Variable([String(name)])`).
fn expr_is_bare_named(expr: &Expr, name: &str) -> bool {
    matches!(expr, Expr::Variable(path)
        if path.len() == 1
            && matches!(&path[0], TokenKey::String(s, _, _) if s == name))
}

/// #359 (W20): infer that a closure param named `name` is a
/// `List<Float>` from how the body uses it: an index access
/// `name[...]` whose result flows into Float arithmetic, OR the body
/// otherwise pairs the index with a Float literal / Float where-
/// binding. Conservative — only fires when the body contains a Float
/// literal alongside an index on `name`, so a `List<Int>` param (whose
/// body is pure-Int) is never mis-inferred. This runs only after
/// [`infer_param_from_sibling_call`] declines, so the precise
/// propagation always wins.
fn closure_param_used_as_list_float(name: &str, expr: &Expr) -> bool {
    closure_param_used_as_list_int(name, expr) && expr_contains_float_literal(expr)
}

/// #359 (W20): infer that a scalar closure param named `name` is a
/// `Float` from its use as a bare operand in a Float-shaped arithmetic
/// expression. Drives `pair_force`'s `mj` mass param (used in
/// `(s[j] - s[i]) * mj * (1.0 / ..)`) to `F64`. Conservative: fires only
/// when `name` appears as a direct (non-bool) Binary operand AND the
/// closure body somewhere carries a Float literal — a pure-Int scalar
/// param (e.g. `pair_force`'s `i` / `j` index args, which only appear
/// inside `s[..]` brackets and `i == j` comparisons, never as a bare
/// arithmetic operand) is never mis-tagged.
fn closure_param_used_as_float(name: &str, body: &Expr) -> bool {
    expr_contains_float_literal(body) && param_is_bare_arith_operand(name, body)
}

/// `true` when `name` appears as a direct operand of a non-bool Binary
/// (or Unary) arithmetic op anywhere in `expr`.
fn param_is_bare_arith_operand(name: &str, expr: &Expr) -> bool {
    match expr {
        Expr::Binary(op, l, r) => {
            if !operator_yields_bool(*op)
                && (expr_is_bare_named(&l.expr, name) || expr_is_bare_named(&r.expr, name))
            {
                return true;
            }
            param_is_bare_arith_operand(name, &l.expr) || param_is_bare_arith_operand(name, &r.expr)
        }
        Expr::Unary(op, n) => {
            (!matches!(op, Operator::Not) && expr_is_bare_named(&n.expr, name))
                || param_is_bare_arith_operand(name, &n.expr)
        }
        Expr::Ternary { cond, then, els } => {
            param_is_bare_arith_operand(name, &cond.expr)
                || param_is_bare_arith_operand(name, &then.expr)
                || param_is_bare_arith_operand(name, &els.expr)
        }
        Expr::List(items) => items
            .iter()
            .any(|n| param_is_bare_arith_operand(name, &n.expr)),
        Expr::FnCall { args, .. } => args
            .iter()
            .any(|a| param_is_bare_arith_operand(name, &a.value.expr)),
        _ => false,
    }
}

/// `true` when `expr` contains a Float literal anywhere in its tree.
fn expr_contains_float_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Float(_) => true,
        Expr::Binary(_, l, r) => {
            expr_contains_float_literal(&l.expr) || expr_contains_float_literal(&r.expr)
        }
        Expr::Unary(_, n) => expr_contains_float_literal(&n.expr),
        Expr::Ternary { cond, then, els } => {
            expr_contains_float_literal(&cond.expr)
                || expr_contains_float_literal(&then.expr)
                || expr_contains_float_literal(&els.expr)
        }
        Expr::List(items) => items.iter().any(|n| expr_contains_float_literal(&n.expr)),
        Expr::FnCall { args, .. } => args
            .iter()
            .any(|a| expr_contains_float_literal(&a.value.expr)),
        _ => false,
    }
}

/// AOT-4 (W16 slice): infer that a closure param named `name` is a
/// `List<Int>` from how the body uses it. Returns `true` when the body
/// contains an index access `name[...]`, a `_len(name)` / `len(name)`,
/// or a `_list_filter(name, ...)` whose first argument is the bare
/// param. The walk is purely structural (no type-checking) and
/// conservative — it only fires on uses that are unambiguously
/// list-shaped, so a scalar param is never mis-inferred.
fn closure_param_used_as_list_int(name: &str, expr: &Expr) -> bool {
    fn expr_is_bare_param(expr: &Expr, name: &str) -> bool {
        matches!(expr, Expr::Variable(path)
            if path.len() == 1
                && matches!(&path[0], TokenKey::String(s, _, _) if s == name))
    }
    match expr {
        // `name[...]` -> `Variable([String(name), Dynamic(idx)])`.
        Expr::Variable(path) | Expr::Reference { path, .. } => {
            path.len() == 2
                && matches!(&path[0], TokenKey::String(s, _, _) if s == name)
                && matches!(&path[1], TokenKey::Dynamic(_, _))
        }
        Expr::FnCall { path, args } => {
            // `_len(name)` / `len(name)` / `_list_filter(name, ...)`.
            let head_list_intrinsic = path.len() == 1
                && matches!(&path[0], TokenKey::String(s, _, _)
                    if s == "_len" || s == "len" || s == "_list_filter");
            if head_list_intrinsic
                && args
                    .first()
                    .is_some_and(|a| expr_is_bare_param(&a.value.expr, name))
            {
                return true;
            }
            // AOT-4 (W19 slice): a method call whose receiver is the bare
            // param — `name.reduce(...)` / `name.sum()` / `name.map(...)`
            // / `name.fold(...)` / `name.length()` — is a list use. A
            // bare-variable receiver parses as `[String(name),
            // String(method)]`; a complex-expression receiver parses as
            // `[Dynamic(<receiver>), String(method)]`. Cover both.
            if path.len() == 2 {
                if let TokenKey::String(m, _, _) = &path[1] {
                    let is_list_method = matches!(
                        m.as_str(),
                        "reduce" | "sum" | "map" | "fold" | "filter" | "length" | "max"
                    );
                    let recv_is_param = match &path[0] {
                        TokenKey::String(s, _, _) => s == name,
                        TokenKey::Dynamic(recv, _) => expr_is_bare_param(&recv.expr, name),
                        _ => false,
                    };
                    if is_list_method && recv_is_param {
                        return true;
                    }
                }
            }
            args.iter()
                .any(|a| closure_param_used_as_list_int(name, &a.value.expr))
        }
        Expr::Binary(_, l, r) => {
            closure_param_used_as_list_int(name, &l.expr)
                || closure_param_used_as_list_int(name, &r.expr)
        }
        Expr::Unary(_, n) => closure_param_used_as_list_int(name, &n.expr),
        Expr::Ternary { cond, then, els } => {
            closure_param_used_as_list_int(name, &cond.expr)
                || closure_param_used_as_list_int(name, &then.expr)
                || closure_param_used_as_list_int(name, &els.expr)
        }
        _ => false,
    }
}

/// `true` for the comparison + logical operators whose IR lowering
/// leaves a `Bool` on the operand stack.
fn operator_yields_bool(op: Operator) -> bool {
    matches!(
        op,
        Operator::Eq
            | Operator::Ne
            | Operator::Lt
            | Operator::Gt
            | Operator::Le
            | Operator::Ge
            | Operator::And
            | Operator::Or
    )
}

/// Lower a `<expr> where { name: value, ... }` form by emitting one
/// `LetSet` per binding (in declaration order) and then lowering
/// `expr` with the names in scope.
///
/// Each binding picks up a fresh per-function let-local index;
/// shadowing is supported (a re-declared name uses a new index, and
/// the outer binding becomes unreachable inside the inner expression
/// but stays valid after). We restore the outer scope after the
/// inner body lowers so the trailing `StoreField` sees a clean
/// virtual stack.
fn lower_where(
    expr: &Node,
    bindings: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let pairs = match &*bindings.expr {
        Expr::Dict(pairs) => pairs,
        _ => {
            return Err(cap!(
                "lower_where.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!("Where(bindings={})", bindings.expr.kind()),
                    range,
                }
            ));
        }
    };
    let saved_lets_len = ctx.lets.len();
    for (key, value) in pairs {
        let name = match key {
            TokenKey::String(s, _, _) => s.clone(),
            _ => {
                return Err(cap!(
                    "lower_where.unsupported_expr.2",
                    LoweringError::UnsupportedExpr {
                        kind: "Where(non-string-binding-key)".to_string(),
                        range,
                    }
                ));
            }
        };
        // AOT-3: a where-binding whose value is a closure (the
        // method-shorthand `name(params): body` desugars to an
        // `Expr::Closure`) is lifted to a closure-typed let-binding,
        // exactly the way the W7 anon-Dict-return path lifts its
        // `#internal fib: (k) => ...` field (see `lower_anon_dict_body`).
        // The closure let-idx + its signature are registered in
        // `ctx.closure_let_signatures` BEFORE the body lowers so a
        // recursive self-call inside the body (W17's `bs((lo+hi)/2, hi,
        // t)`) resolves through `try_lower_local_closure_call` ->
        // `Op::CallClosure`. Pre-Phase-AOT-3 this hit the
        // `Expr::Closure { .. } => ClosureAcrossBoundary` arm of
        // `lower_expr` and the whole W17 source was rejected at lowering.
        if let Expr::Closure {
            params,
            body: closure_body,
            return_type,
        } = &*value.expr
        {
            // Unannotated params / return default to `Int` (I64) — the
            // same convention `anon_dict_return_plan` uses for W7. A
            // `(k: Bool) =>` style annotation is honoured when present.
            //
            // AOT-4 (W16 slice): an unannotated param defaults to
            // `List<Int>` when the body uses it as a list — indexed
            // (`p[i]`) or handed to `_len(p)` / `_list_filter(p, ...)`.
            // The W16 quicksort helper `sum_qs(xs)` takes a `List<Int>`
            // handle with NO annotation (the closure-param grammar does
            // not accept `List<Int>`), so the recursive lift must infer
            // it; an I64 default would mis-tag the recursive list arg
            // and reject lowering. The inference is a cheap structural
            // walk that only fires on the list-shaped uses above.
            let mut param_irts: Vec<IrType> = Vec::with_capacity(params.len());
            for p in params {
                let annotated = p
                    .type_hint
                    .as_ref()
                    .and_then(type_node_to_canonical)
                    .and_then(|r| type_repr_to_ir_type(&r).ok());
                let irt = annotated.unwrap_or_else(|| {
                    // #359 (W20): a param passed positionally into a
                    // sibling where-bound closure adopts that closure's
                    // declared param type (so `accel(s, i)`'s `s` picks up
                    // `ListFloat` from `pair_force`'s first param even
                    // though `accel` never indexes `s` directly). This
                    // runs first because it is a precise propagation, not
                    // a structural guess.
                    if let Some(t) = infer_param_from_sibling_call(&p.name, &closure_body.expr, ctx)
                    {
                        t
                    } else if closure_param_used_as_list_float(&p.name, &closure_body.expr) {
                        // Indexed param whose element flows into Float
                        // arithmetic (W20 `step` / `pair_force` read
                        // `s[k]` then combine with `dt` / `soft` / a
                        // mass) is a `List<Float>`.
                        IrType::ListFloat
                    } else if closure_param_used_as_list_int(&p.name, &closure_body.expr) {
                        IrType::ListInt
                    } else if closure_param_used_as_float(&p.name, &closure_body.expr) {
                        // Scalar param used as a bare operand in Float
                        // arithmetic (W20 `pair_force`'s `mj` mass:
                        // `(s[j] - s[i]) * mj * (1.0 / ..)`) is a `Float`.
                        IrType::F64
                    } else {
                        IrType::I64
                    }
                });
                param_irts.push(irt);
            }
            // Return type: honour an explicit annotation, else infer
            // from the body. AOT-3 hardcoded I64 (W17's `bs` returns
            // `Int`); AOT-4's W18 `is_prime` returns `Bool`, so the
            // self-recursive call inside the ternary must agree with
            // the sibling `true`/`false` literal branches — a fixed
            // I64 default surfaces an `IfBranchTypeMismatch`. The
            // inference is a cheap structural walk (ternary branches /
            // comparison + logical ops -> Bool, otherwise Int).
            //
            // #359 (W20): the ctx-aware variant additionally resolves
            // `F64` (Float literal / Float arith / a call into a sibling
            // closure that returns `F64`) and `ListFloat` (a `[...]`
            // list literal of Float-valued elements) — `pair_force` /
            // `accel` return `F64`, `step` returns a `List<Float>`.
            let ret_ty = return_type
                .as_ref()
                .and_then(type_node_to_canonical)
                .and_then(|r| type_repr_to_ir_type(&r).ok())
                .unwrap_or_else(|| {
                    infer_closure_body_ret_ty_ctx(&closure_body.expr, &param_irts, params, ctx)
                });

            // Pre-allocate the let-idx the closure handle lands in and
            // register both the binding and its signature before the
            // body lowers, so a self-recursive call resolves to this
            // slot.
            let idx = ctx.next_let_idx;
            ctx.next_let_idx += 1;
            ctx.lets.push(LetBinding {
                name,
                idx,
                ty: IrType::Closure,
                schema_brand: None,
                type_repr: None,
            });
            ctx.closure_let_signatures
                .insert(idx, (param_irts.clone(), ret_ty));

            // Lower the closure body — pushes `IrType::Closure` and
            // appends the lambda Func to `ctx.lambda_funcs`.
            lower_closure_as_value(&value.expr, value.range, &param_irts, ret_ty, ctx)?;
            let popped = ctx.tstack.pop().ok_or(cap!(
                "lower_where.unsupported_expr.3",
                LoweringError::UnsupportedExpr {
                    kind: "Where(closure-binding-empty-stack)".to_string(),
                    range: value.range,
                }
            ))?;
            debug_assert_eq!(popped, IrType::Closure);
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx,
                    ty: IrType::Closure,
                },
                range: value.range,
            });
            continue;
        }
        // AOT-4 (W16 slice): a where-binding whose value is a bare
        // `range(a, b)` (or `range(b)`) MUST materialise into a
        // `List<Int>` arena record so a downstream consumer can index
        // it (`xs[0]`) or recurse on a filtered sub-list. The eliding
        // range peepholes only fire for the fusable `.sum` / `.len` /
        // `.reduce` terminals — a where-bound range has no such
        // terminal, so without this it would fall through to the
        // generic FnCall dispatch and reject as
        // `UnknownStdlibMethod { name: "range" }`.
        // AOT-4 (W19 slice): also materialise a where-bound nested
        // `range(a, b).map((p) => <inner>)` — a `List<List<Int>>` (when
        // `<inner>` is itself a materialisable row) or a `List<Int>`
        // (when `<inner>` is `Int`-valued). `emit_list_value_materialize`
        // subsumes the bare-range case, so route both shapes through it.
        if match_bare_range(&value.expr).is_some()
            || match_materializable_outer_map(&value.expr).is_some()
        {
            emit_list_value_materialize(&value.expr, value.range, ctx)?;
            let value_ty = ctx.tstack.pop().ok_or(cap!(
                "lower_where.unsupported_expr.4",
                LoweringError::UnsupportedExpr {
                    kind: "Where(range-materialize-empty-stack)".to_string(),
                    range: value.range,
                }
            ))?;
            debug_assert_eq!(value_ty, IrType::ListInt);
            let idx = ctx.next_let_idx;
            ctx.next_let_idx += 1;
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx,
                    ty: IrType::ListInt,
                },
                range: value.range,
            });
            ctx.lets.push(LetBinding {
                name,
                idx,
                ty: IrType::ListInt,
                schema_brand: None,
                type_repr: None,
            });
            continue;
        }
        // #359 (W20): a where-binding whose value is a Float-shaped list
        // literal (the n-body `init: [0.0, 1.0, ..]`) materialises into a
        // scratch `List<Float>` arena record so a downstream consumer (the
        // `range(n).reduce(init, ..)` accumulator) carries a runtime
        // handle, and `init[k]` / `s[k]` index it as `f64`. The bare
        // `Expr::List` arm would otherwise intern a `ConstListFloat` the
        // LLVM AOT envelope cannot materialise; routing the where-bound
        // literal through the scratch materialiser keeps the accumulator
        // a live arena handle (only the LLVM AOT path reaches this — the
        // tree-walker resolves `init` directly).
        if let Expr::List(items) = &*value.expr {
            if !items.is_empty() && list_is_float_shaped(items) {
                emit_list_float_literal_materialize(items, value.range, ctx)?;
                let value_ty = ctx.tstack.pop().ok_or(cap!(
                    "lower_where.unsupported_expr.5",
                    LoweringError::UnsupportedExpr {
                        kind: "Where(float-list-materialize-empty-stack)".to_string(),
                        range: value.range,
                    }
                ))?;
                debug_assert_eq!(value_ty, IrType::ListFloat);
                let idx = ctx.next_let_idx;
                ctx.next_let_idx += 1;
                ctx.out.push(TaggedOp {
                    op: Op::LetSet {
                        idx,
                        ty: IrType::ListFloat,
                    },
                    range: value.range,
                });
                ctx.lets.push(LetBinding {
                    name,
                    idx,
                    ty: IrType::ListFloat,
                    schema_brand: None,
                    type_repr: None,
                });
                continue;
            }
        }
        // #359 (W20 div-trap unlock): record a where-bound *scalar
        // literal* (`soft: 0.1`, `dt: 0.01`, `m0: 1.0`, ...) as a
        // compile-time constant keyed by its let-idx, so a closure that
        // captures it can inline the constant instead of loading it from
        // the captures struct (see `LowerCtx::const_let_values`). Only
        // bare `Int` / `Float` / `Bool` literals qualify — anything
        // computed still flows through the normal capture path.
        let scalar_const = match &*value.expr {
            Expr::Int(i) => Some(ScalarConst::I64(*i)),
            Expr::Float(f) => Some(ScalarConst::F64(f.into_inner())),
            Expr::Bool(b) => Some(ScalarConst::Bool(*b)),
            _ => None,
        };
        lower_expr(&value.expr, value.range, ctx)?;
        let value_ty = ctx.tstack.pop().ok_or(cap!(
            "lower_where.unsupported_expr.6",
            LoweringError::UnsupportedExpr {
                kind: "Where(binding-empty-stack)".to_string(),
                range: value.range,
            }
        ))?;
        let idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetSet { idx, ty: value_ty },
            range: value.range,
        });
        if let Some(sc) = scalar_const {
            ctx.const_let_values.insert(idx, sc);
        }
        ctx.lets.push(LetBinding {
            name,
            idx,
            ty: value_ty,
            schema_brand: None,
            type_repr: None,
        });
    }
    lower_expr(&expr.expr, expr.range, ctx)?;
    // Pop only the bindings we pushed in this frame — preserves
    // outer-scope lets for sibling expressions.
    ctx.lets.truncate(saved_lets_len);
    Ok(())
}

/// Lower one binary expression. Splits the arithmetic + comparison
/// paths so each surface keeps its rejection rules explicit.
fn lower_binary(
    op: Operator,
    lhs: &Node,
    rhs: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // #165 — collapse a left-leaning `String + String + ... + String`
    // chain into a single `Op::StrConcatN { operand_count: N }` so
    // every IR-consuming backend (bytecode VM / cranelift AOT /
    // trace-JIT) routes through a single allocation instead of N - 1
    // pairwise `Op::Add(String)` allocs. The fold is gated on AST
    // shape (the outer node is `Add` and the LHS is itself an `Add`),
    // matches the tree-walker's `try_eval_string_concat_chain` filter,
    // and bails to standard pair-wise lowering when the chain mixes
    // non-String operand types.
    if matches!(op, Operator::Add)
        && matches!(lhs.expr.as_ref(), Expr::Binary(Operator::Add, _, _))
        && try_lower_str_concat_chain(lhs, rhs, range, ctx)?
    {
        return Ok(());
    }
    // Short-circuit Boolean operators. Lowered as a guard-style `Op::If`:
    //
    // * `a && b` → `a ? b : false`
    // * `a || b` → `a ? true  : b`
    //
    // Each branch lowers in its own sub-stream / tstack via
    // [`lower_branch`] so a non-Bool side bails cleanly. Short-circuit
    // semantics fall out of `Op::If`: the unselected branch's ops never
    // execute, matching the tree-walker's eager-eval-of-LHS-then-RHS
    // discipline. Open follow-up #264-cont: this unblocks predicate
    // chains in cmp_lua W10's `allow`-style closures and any future
    // boolean-heavy workload, without disturbing the arithmetic /
    // comparison paths below.
    if matches!(op, Operator::And | Operator::Or) {
        lower_expr(&lhs.expr, lhs.range, ctx)?;
        let lhs_ty = ctx.tstack.pop().ok_or(cap!(
            "lower_binary.unsupported_operator.1",
            LoweringError::UnsupportedOperator { op, range }
        ))?;
        if lhs_ty != IrType::Bool {
            return Err(cap!(
                "lower_binary.unsupported_operator.2",
                LoweringError::UnsupportedOperator { op, range }
            ));
        }
        let (then_body, then_ty) = if matches!(op, Operator::And) {
            // a && b: then-branch is `b`, else-branch is `false`.
            lower_branch(rhs, range, ctx)?
        } else {
            // a || b: then-branch is `true`, else-branch is `b`.
            let saved_out = std::mem::take(&mut ctx.out);
            let saved_stack = std::mem::take(&mut ctx.tstack);
            ctx.out.push(TaggedOp {
                op: Op::ConstBool(true),
                range,
            });
            ctx.tstack.push(IrType::Bool);
            let body = std::mem::replace(&mut ctx.out, saved_out);
            let stack = std::mem::replace(&mut ctx.tstack, saved_stack);
            (body, stack[0])
        };
        let (else_body, else_ty) = if matches!(op, Operator::And) {
            // a && b: else-branch is `false`.
            let saved_out = std::mem::take(&mut ctx.out);
            let saved_stack = std::mem::take(&mut ctx.tstack);
            ctx.out.push(TaggedOp {
                op: Op::ConstBool(false),
                range,
            });
            ctx.tstack.push(IrType::Bool);
            let body = std::mem::replace(&mut ctx.out, saved_out);
            let stack = std::mem::replace(&mut ctx.tstack, saved_stack);
            (body, stack[0])
        } else {
            // a || b: else-branch is `b`.
            lower_branch(rhs, range, ctx)?
        };
        if then_ty != IrType::Bool || else_ty != IrType::Bool {
            return Err(cap!(
                "lower_binary.unsupported_operator.3",
                LoweringError::UnsupportedOperator { op, range }
            ));
        }
        ctx.out.push(TaggedOp {
            op: Op::If {
                result_ty: IrType::Bool,
                then_body,
                else_body,
            },
            range,
        });
        ctx.tstack.push(IrType::Bool);
        return Ok(());
    }
    if let Some(ir_op_ctor) = arithmetic_op_ctor(op) {
        // Lower the LHS into `ctx.out`, then capture the RHS into a
        // detached stream so a mixed `Int`/`Float` pair can splice an
        // `Op::ConvertI64ToF64` promotion onto whichever operand is the
        // `I64` one — the LHS sits buried under the RHS once both are
        // emitted, so we cannot append to it after the fact.
        lower_expr(&lhs.expr, lhs.range, ctx)?;
        let lhs_ty = *ctx.tstack.last().ok_or(cap!(
            "lower_binary.unsupported_operator.4",
            LoweringError::UnsupportedOperator { op, range }
        ))?;
        let saved_out = std::mem::take(&mut ctx.out);
        lower_expr(&rhs.expr, rhs.range, ctx)?;
        let rhs_ops = std::mem::replace(&mut ctx.out, saved_out);
        let rhs_ty = ctx.tstack.pop().ok_or(cap!(
            "lower_binary.unsupported_operator.5",
            LoweringError::UnsupportedOperator { op, range }
        ))?;
        // Pop the LHS too — the result type tag is recomputed below.
        ctx.tstack.pop().ok_or(cap!(
            "lower_binary.unsupported_operator.6",
            LoweringError::UnsupportedOperator { op, range }
        ))?;
        // Int↔Float promotion (#359 / #362): mirror the tree-walker's
        // `NumericValue::as_f64()` — when one operand is `Int` and the
        // other `Float`, the `Int` operand is widened to `f64` and the
        // binop runs as `F64`, result `Float`. `Add` / `Sub` / `Mul` /
        // `Div` / `Mod` all promote: the tree-walker's
        // `eval_numeric_division` computes `a.as_f64() % b.as_f64()`
        // (Rust f64 `%` = `fmod`, truncated remainder, sign of the
        // dividend) for any non-`Int`/`Int` `%`, so mixed `Int`/`Float`
        // `%` widens here and lowers to `Op::Mod(F64)` below.
        let mixed_promote = matches!(
            (lhs_ty, rhs_ty),
            (IrType::I64, IrType::F64) | (IrType::F64, IrType::I64)
        ) && matches!(
            op,
            Operator::Add | Operator::Sub | Operator::Mul | Operator::Div | Operator::Mod
        );
        if mixed_promote {
            if lhs_ty == IrType::I64 {
                // Promote the LHS (currently on top of `ctx.out`) before
                // emitting the RHS stream.
                ctx.out.push(TaggedOp {
                    op: Op::ConvertI64ToF64,
                    range: lhs.range,
                });
                ctx.out.extend(rhs_ops);
            } else {
                // LHS is already `F64`; emit the RHS then promote it.
                ctx.out.extend(rhs_ops);
                ctx.out.push(TaggedOp {
                    op: Op::ConvertI64ToF64,
                    range: rhs.range,
                });
            }
            ctx.out.push(TaggedOp {
                op: ir_op_ctor(IrType::F64),
                range,
            });
            ctx.tstack.push(IrType::F64);
            return Ok(());
        }
        // Homogeneous path: re-attach the RHS stream verbatim and fall
        // through to the same-type checks below.
        ctx.out.extend(rhs_ops);
        if lhs_ty != rhs_ty {
            return Err(cap!(
                "lower_binary.unsupported_operator.7",
                LoweringError::UnsupportedOperator { op, range }
            ));
        }
        // F-D7-D: `String + String` lowers to `Op::Add(IrType::String)`.
        // The trace recorder short-circuits this onto `TraceOp::StrConcat`
        // (see `relon_trace_recorder::lowering::lower_str_add`); the
        // tree-walk / cranelift-AOT backends route through their generic
        // string-concat dispatch (Value-level concat in the evaluator,
        // and a host-shim call in cranelift). Only `Operator::Add` is
        // accepted for strings — `String - String` / `String * String`
        // would have been rejected upstream by the analyzer
        // (`infer_binary` returns `None`) so the lhs/rhs types would
        // not both be `String` here for any non-Add arith op.
        if lhs_ty == IrType::String {
            if !matches!(op, Operator::Add) {
                return Err(cap!(
                    "lower_binary.unsupported_operator.8",
                    LoweringError::UnsupportedOperator { op, range }
                ));
            }
            ctx.out.push(TaggedOp {
                op: Op::Add(IrType::String),
                range,
            });
            ctx.tstack.push(IrType::String);
            return Ok(());
        }
        // Only Int / Float pairs support arithmetic.
        if !matches!(lhs_ty, IrType::I64 | IrType::F64) {
            return Err(cap!(
                "lower_binary.unsupported_operator.9",
                LoweringError::UnsupportedOperator { op, range }
            ));
        }
        // #362: `F64 % F64` lowers to `Op::Mod(F64)` (and so does the
        // promoted-mixed `%` above) to match the tree-walker, which
        // computes `a.as_f64() % b.as_f64()`. Backends that lack a
        // native float remainder (wasm has no `f64.rem`; cranelift has
        // no `frem` and no fmod libcall wired) gracefully reject
        // `Op::Mod(F64)` at codegen — never a panic, never a wrong
        // answer. The LLVM AOT lowers it to `frem` (= `fmod`).
        ctx.out.push(TaggedOp {
            op: ir_op_ctor(lhs_ty),
            range,
        });
        ctx.tstack.push(lhs_ty);
        return Ok(());
    }
    if let Some(cmp_ctor) = comparison_op_ctor(op) {
        lower_expr(&lhs.expr, lhs.range, ctx)?;
        lower_expr(&rhs.expr, rhs.range, ctx)?;
        let rhs_ty = ctx.tstack.pop().ok_or(cap!(
            "lower_binary.unsupported_operator.10",
            LoweringError::UnsupportedOperator { op, range }
        ))?;
        let lhs_ty = ctx.tstack.pop().ok_or(cap!(
            "lower_binary.unsupported_operator.11",
            LoweringError::UnsupportedOperator { op, range }
        ))?;
        if lhs_ty != rhs_ty {
            return Err(cap!(
                "lower_binary.unsupported_operator.12",
                LoweringError::UnsupportedOperator { op, range }
            ));
        }
        // Phase 2.c supports comparisons on Int / Float / Bool /
        // Unit. Bool / Unit only support `==` / `!=`; ordering
        // (`<`, `<=`, `>`, `>=`) is rejected at the comparison
        // codegen layer too, but we surface it here as a lowering
        // error so the message stays user-facing.
        match (lhs_ty, op) {
            (IrType::I64 | IrType::F64, _) => {}
            (IrType::Bool, Operator::Eq | Operator::Ne) => {}
            (IrType::Unit, Operator::Eq | Operator::Ne) => {}
            _ => {
                return Err(cap!(
                    "lower_binary.unsupported_operator.13",
                    LoweringError::UnsupportedOperator { op, range }
                ))
            }
        }
        ctx.out.push(TaggedOp {
            op: cmp_ctor(lhs_ty),
            range,
        });
        ctx.tstack.push(IrType::Bool);
        return Ok(());
    }
    // Wave R2 — `a | f(...)` pipe operator. A pure static desugar: the
    // tree-walker (eval.rs `Expr::Binary(Operator::Pipe, ..)`) prepends
    // the LHS value as the FIRST positional argument of the right-hand
    // call and dispatches the call. We reproduce that here by building a
    // synthetic argument list `[positional(lhs), ...rhs_args]` and
    // routing it through `lower_fn_call`, so `xs | list.sum`
    // (zero extra args) lowers identically to `list.sum(xs)`, and
    // `xs | _list_reduce(0, f)` lowers identically to
    // `_list_reduce(xs, 0, f)`. No new IR op is needed — the desugar is
    // entirely at the call-shape level, and every downstream peephole
    // (`list.sum(range(..))`, `_len(_list_filter(..))`, ...) fires
    // exactly as it would for the spelled-out call. The tree-walker uses
    // `right.range` for the dispatched call; we mirror that by using the
    // RHS node's range.
    if matches!(op, Operator::Pipe) {
        match rhs.expr.as_ref() {
            // `a | f(extra...)` ≡ `f(a, extra...)`.
            Expr::FnCall { path, args } => {
                let mut piped_args: Vec<relon_parser::CallArg> = Vec::with_capacity(args.len() + 1);
                piped_args.push(relon_parser::CallArg {
                    name: None,
                    value: lhs.clone(),
                });
                piped_args.extend(args.iter().cloned());
                return lower_fn_call(path, &piped_args, rhs.range, ctx);
            }
            // `a | path` (bare, no parens) — `path` parses as a
            // `Variable` naming a function/stdlib entry. The tree-walker
            // resolves it to a closure value and applies `a` as the sole
            // argument, which is byte-equal to the zero-extra-arg call
            // `path(a)`. Route it through the same call lowering.
            Expr::Variable(path) => {
                let piped_args = vec![relon_parser::CallArg {
                    name: None,
                    value: lhs.clone(),
                }];
                return lower_fn_call(path, &piped_args, rhs.range, ctx);
            }
            // `a | <closure-literal>` / `a | <other>` — the tree-walker
            // applies a closure *value* on the RHS. Closures cannot
            // cross the AOT boundary as first-class values (already
            // capped as `lower_expr.closure_across_boundary`), so this
            // residual pipe shape stays capped via the catch-all below.
            _ => {}
        }
    }
    Err(cap!(
        "lower_binary.unsupported_operator.14",
        LoweringError::UnsupportedOperator { op, range }
    ))
}

/// Wave R2 — lower an f-string `f"...${e}..."` into a single `String`
/// IR value. Pure static desugar of the tree-walker's `Expr::FString`
/// arm (eval.rs): the parts are concatenated left-to-right, each
/// interpolated value coerced to a `String` via the same rendering the
/// tree-walker uses (`write!(result, "{}", val)`, i.e. `Display`):
///
///   * a `String`-typed interpolation is used verbatim,
///   * an `Int`-typed interpolation is rendered as its base-10 decimal
///     (`Op::IntToStr`, byte-exact with `i64` `Display`),
///   * literal parts become `Op::ConstString`.
///
/// Each part is reduced to exactly one `String` operand; the operands
/// are then joined with `Op::StrConcatN { operand_count: parts }` (which
/// matches the chained `String + String` lowering byte-for-byte). The
/// 0-part (`f""`) and 1-part degenerate shapes skip the concat. Float /
/// Bool / other interpolation types are not yet byte-proven and stay
/// capped via the catch-all `lower_expr.unsupported_expr.8`.
fn lower_fstring(
    parts: &[FStringPart],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // `f""` → the empty string literal (interned like any other).
    if parts.is_empty() {
        let idx = ctx.const_intern.borrow_mut().strings.intern("");
        ctx.out.push(TaggedOp {
            op: Op::ConstString {
                idx,
                value: String::new(),
            },
            range,
        });
        ctx.tstack.push(IrType::String);
        return Ok(());
    }
    // Emit one `String` operand per part, in source order.
    for part in parts {
        lower_fstring_part(part, range, ctx)?;
    }
    // Single part: the lone operand is already the result.
    if parts.len() == 1 {
        return Ok(());
    }
    // Join the N operands with a single concat allocation.
    let operand_count = parts.len() as u32;
    // The N String operands were just pushed; collapse their stack tags
    // into the single String result.
    for _ in 0..operand_count {
        ctx.tstack.pop().ok_or(cap!(
            "lower_expr.unsupported_expr.8",
            LoweringError::UnsupportedExpr {
                kind: "FString(operand stack underflow)".to_string(),
                range,
            }
        ))?;
    }
    ctx.out.push(TaggedOp {
        op: Op::StrConcatN { operand_count },
        range,
    });
    ctx.tstack.push(IrType::String);
    Ok(())
}

/// Lower one f-string part to exactly one `String` operand on the stack.
fn lower_fstring_part(
    part: &FStringPart,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    match part {
        FStringPart::Literal(s) => {
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
        FStringPart::Interpolation(node) => {
            lower_expr(&node.expr, node.range, ctx)?;
            let ty = ctx.tstack.pop().ok_or(cap!(
                "lower_expr.unsupported_expr.8",
                LoweringError::UnsupportedExpr {
                    kind: "FString(interpolation operand underflow)".to_string(),
                    range: node.range,
                }
            ))?;
            match ty {
                IrType::String => {
                    // Already a String — coercion is identity.
                    ctx.tstack.push(IrType::String);
                    Ok(())
                }
                IrType::I64 => {
                    // Int → base-10 decimal, byte-exact with `Display`.
                    ctx.out.push(TaggedOp {
                        op: Op::IntToStr,
                        range: node.range,
                    });
                    ctx.tstack.push(IrType::String);
                    Ok(())
                }
                other => Err(cap!(
                    "lower_expr.unsupported_expr.8",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "FString(interpolation of type {other:?} — only String / Int \
                             interpolations have a byte-exact AOT coercion)"
                        ),
                        range: node.range,
                    }
                )),
            }
        }
    }
}

/// #165 — fold a left-leaning `String + String + ... + String` chain
/// into a single `Op::StrConcatN { operand_count: N }` so every
/// IR-consuming backend (bytecode VM / cranelift AOT / trace-JIT)
/// allocates once instead of N - 1 times.
///
/// Returns `Ok(true)` when the fold fired (caller skips its standard
/// pair-wise path), `Ok(false)` when the chain mixes non-String
/// operand types (caller falls back). The function side-effects
/// `ctx.out` / `ctx.tstack` only when it commits — a mismatch is
/// detected before any append, so the caller's fall-back path
/// re-lowers from the original `lhs` / `rhs` nodes without seeing
/// stale ops.
///
/// AST shape preconditions (checked by the caller): the outer op is
/// `Operator::Add` and `lhs` is itself an `Expr::Binary(Add, _, _)` —
/// i.e. the chain has at least three leaves.
fn try_lower_str_concat_chain(
    lhs: &Node,
    rhs: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<bool, LoweringError> {
    // Walk the LHS spine, peeling off each Add's RHS onto a stack so
    // the deepest leaf becomes `cursor` and `rhs_stack` holds the
    // outer-to-inner right operands. Pop order then yields source
    // order: deepest leaf first, then each RHS in chain order, then
    // the original outer `rhs` last.
    let mut rhs_stack: Vec<&Node> = Vec::with_capacity(4);
    let mut cursor: &Node = lhs;
    while let Expr::Binary(Operator::Add, inner_l, inner_r) = cursor.expr.as_ref() {
        rhs_stack.push(inner_r);
        cursor = inner_l;
    }
    // `cursor` is now the deepest non-Add leaf. Build the leaf list in
    // source order. `leaf_count >= 3` is guaranteed by the caller's
    // shape gate (`lhs` itself is a Binary(Add)).
    let mut leaves: Vec<&Node> = Vec::with_capacity(rhs_stack.len() + 2);
    leaves.push(cursor);
    while let Some(node) = rhs_stack.pop() {
        leaves.push(node);
    }
    leaves.push(rhs);
    debug_assert!(leaves.len() >= 3);
    let leaf_count = leaves.len();
    // Snapshot the emit cursor / type stack so we can roll back if any
    // leaf turns out non-String. We restore both on miss so the
    // caller's standard `lower_arith` path re-runs from the same
    // starting state without observing partial ops.
    let saved_out_len = ctx.out.len();
    let saved_tstack_len = ctx.tstack.len();
    let saved_next_let = ctx.next_let_idx;
    let saved_lambda_len = ctx.lambda_table.borrow().len();
    // Lower each leaf, type-checking after each so we abort early on
    // the first non-String operand (the common rejection — e.g. an
    // outer-Add was actually Schema-merge from a non-Add LHS, which
    // the caller's shape gate already filters).
    for leaf in &leaves {
        lower_expr(&leaf.expr, leaf.range, ctx)?;
        let leaf_ty = ctx.tstack.last().copied();
        if leaf_ty != Some(IrType::String) {
            // Mismatch — restore the snapshot and let the caller fall
            // back to pair-wise lowering. `const_intern` interning is
            // idempotent so any literals we pushed into the intern
            // table are still correct. AOT-4: a leaf may now lower a
            // closure (the W16 `+` chain has `qs(_list_filter(xs, (x)
            // => ...))` operands whose `_list_filter` builds a predicate
            // lambda), so restore `next_let_idx` AND truncate any
            // closure-table slots reserved during the discarded
            // speculative leaf lowering — leaking a slot would offset
            // every later `fn_table_idx` and dispatch the predicate to
            // the wrong lambda at runtime.
            ctx.out.truncate(saved_out_len);
            ctx.tstack.truncate(saved_tstack_len);
            ctx.next_let_idx = saved_next_let;
            ctx.lambda_table.borrow_mut().truncate(saved_lambda_len);
            return Ok(false);
        }
    }
    // All N leaves are String — commit the StrConcatN op. Pop the N
    // type-stack entries and push the single result.
    ctx.tstack.truncate(saved_tstack_len);
    ctx.tstack.push(IrType::String);
    ctx.out.push(TaggedOp {
        op: Op::StrConcatN {
            operand_count: leaf_count as u32,
        },
        range,
    });
    Ok(true)
}

/// Lower a ternary `cond ? then : els` into `Op::If`. The branches
/// must agree on the IR type they push; the condition must lower to
/// `IrType::Bool`.
fn lower_ternary(
    cond: &Node,
    then: &Node,
    els: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Lower the condition in the outer tstack so a body like
    // `(a > 0) ? ... : ...` accurately reports its Bool result.
    lower_expr(&cond.expr, cond.range, ctx)?;
    let cond_ty = ctx.tstack.pop().ok_or(cap!(
        "lower_ternary.unsupported_expr",
        LoweringError::UnsupportedExpr {
            kind: "Ternary(cond)".to_string(),
            range,
        }
    ))?;
    if cond_ty != IrType::Bool {
        return Err(cap!(
            "lower_ternary.if_condition_not_bool",
            LoweringError::IfConditionNotBool {
                got: cond_ty,
                range,
            }
        ));
    }
    // Lower each branch into its own sub-stream + isolated tstack so
    // an inner expression spilling extra values onto the stack is
    // caught here rather than leaking into the outer body. The
    // branch sub-ctx inherits the outer `lets` scope + counters so
    // a `let`-bound name remains visible inside the arm and any
    // const-literal index issued by the arm doesn't collide with
    // the outer module.
    let then_ops = lower_branch(then, range, ctx)?;
    let then_ty = then_ops.1;
    let then_body = then_ops.0;

    let else_ops = lower_branch(els, range, ctx)?;
    let else_ty = else_ops.1;
    let else_body = else_ops.0;

    if then_ty != else_ty {
        return Err(cap!(
            "lower_ternary.if_branch_type_mismatch",
            LoweringError::IfBranchTypeMismatch {
                then_ty,
                else_ty,
                range,
            }
        ));
    }
    let result_ty = then_ty;
    ctx.out.push(TaggedOp {
        op: Op::If {
            result_ty,
            then_body,
            else_body,
        },
        range,
    });
    ctx.tstack.push(result_ty);
    Ok(())
}

/// Lower a ternary when the surrounding expression already declares the
/// result type. This is required for enum variants such as
/// `flag ? Stat.Up : Stat.Down`, where `Stat.Up` only has meaning once
/// the expected enum type is known.
fn lower_ternary_as_type(
    expected: &TypeRepr,
    cond: &Node,
    then: &Node,
    els: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    lower_expr(&cond.expr, cond.range, ctx)?;
    let cond_ty = ctx.tstack.pop().ok_or(cap!(
        "lower_ternary_as_type.unsupported_expr",
        LoweringError::UnsupportedExpr {
            kind: "Ternary(cond)".to_string(),
            range,
        }
    ))?;
    if cond_ty != IrType::Bool {
        return Err(cap!(
            "lower_ternary_as_type.if_condition_not_bool",
            LoweringError::IfConditionNotBool {
                got: cond_ty,
                range,
            }
        ));
    }

    let then_ops = lower_branch_as_type(expected, then, range, ctx)?;
    let then_ty = then_ops.1;
    let then_body = then_ops.0;

    let else_ops = lower_branch_as_type(expected, els, range, ctx)?;
    let else_ty = else_ops.1;
    let else_body = else_ops.0;

    if then_ty != else_ty {
        return Err(cap!(
            "lower_ternary_as_type.if_branch_type_mismatch",
            LoweringError::IfBranchTypeMismatch {
                then_ty,
                else_ty,
                range,
            }
        ));
    }

    let expected_ty = type_repr_to_ir_type_dict(expected);
    if then_ty != expected_ty {
        return Err(cap!(
            "lower_ternary_as_type.unsupported_expr.branch_type",
            LoweringError::UnsupportedExpr {
                kind: format!("Ternary(branch produced {then_ty:?}, expected {expected_ty:?})"),
                range,
            }
        ));
    }

    ctx.out.push(TaggedOp {
        op: Op::If {
            result_ty: then_ty,
            then_body,
            else_body,
        },
        range,
    });
    ctx.tstack.push(then_ty);
    Ok(())
}

/// Lower one branch of a ternary into a self-contained op stream +
/// the type it leaves on top of its private virtual stack.
///
/// The branch shares the outer `LowerCtx`'s let-local counter / const
/// indices so a const literal inside the branch picks up a unique
/// per-module index. Only the `out` stream is rerouted; the `tstack`
/// is replaced with a fresh one so the branch can be checked in
/// isolation.
fn lower_branch(
    node: &Node,
    range: TokenRange,
    parent: &mut LowerCtx<'_>,
) -> Result<(Vec<TaggedOp>, IrType), LoweringError> {
    let saved_out = std::mem::take(&mut parent.out);
    let saved_stack = std::mem::take(&mut parent.tstack);
    lower_expr(&node.expr, node.range, parent)?;
    let branch_ops = std::mem::replace(&mut parent.out, saved_out);
    let branch_stack = std::mem::replace(&mut parent.tstack, saved_stack);
    if branch_stack.len() != 1 {
        return Err(cap!(
            "lower_branch.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: format!("Ternary(branch-stack={})", branch_stack.len()),
                range,
            }
        ));
    }
    Ok((branch_ops, branch_stack[0]))
}

fn lower_branch_as_type(
    expected: &TypeRepr,
    node: &Node,
    range: TokenRange,
    parent: &mut LowerCtx<'_>,
) -> Result<(Vec<TaggedOp>, IrType), LoweringError> {
    let saved_out = std::mem::take(&mut parent.out);
    let saved_stack = std::mem::take(&mut parent.tstack);
    lower_value_as_type(expected, node, parent)?;
    let branch_ops = std::mem::replace(&mut parent.out, saved_out);
    let branch_stack = std::mem::replace(&mut parent.tstack, saved_stack);
    if branch_stack.len() != 1 {
        return Err(cap!(
            "lower_branch_as_type.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: format!("Ternary(branch-stack={})", branch_stack.len()),
                range,
            }
        ));
    }
    Ok((branch_ops, branch_stack[0]))
}

/// Map a parser comparison `Operator` to the matching IR op
/// constructor. Returns `None` for non-comparison ops.
fn comparison_op_ctor(op: Operator) -> Option<fn(IrType) -> Op> {
    match op {
        Operator::Eq => Some(Op::Eq),
        Operator::Ne => Some(Op::Ne),
        Operator::Lt => Some(Op::Lt),
        Operator::Le => Some(Op::Le),
        Operator::Gt => Some(Op::Gt),
        Operator::Ge => Some(Op::Ge),
        _ => None,
    }
}

/// Map a parser `Operator` to the matching IR op constructor.
fn arithmetic_op_ctor(op: Operator) -> Option<fn(IrType) -> Op> {
    match op {
        Operator::Add => Some(Op::Add),
        Operator::Sub => Some(Op::Sub),
        Operator::Mul => Some(Op::Mul),
        Operator::Div => Some(Op::Div),
        Operator::Mod => Some(Op::Mod),
        _ => None,
    }
}

// =====================================================================
// Phase 3.b: dict-literal lowering helpers.
// =====================================================================

type GenericSubst = HashMap<String, TypeNode>;

fn generic_subst_for_def(def: &SchemaDef, ty: &TypeNode) -> Option<GenericSubst> {
    if def.generics.len() != ty.generics.len() {
        return None;
    }
    Some(
        def.generics
            .iter()
            .cloned()
            .zip(ty.generics.iter().cloned())
            .collect(),
    )
}

fn apply_generic_subst(ty: &TypeNode, subst: &GenericSubst) -> TypeNode {
    if subst.is_empty() {
        ty.clone()
    } else {
        relon_analyzer::substitute_generics_in_typenode(ty, subst)
    }
}

/// If `return_type` names a user-declared record schema (single-segment
/// TypeNode with no generics), return its canonical-form `Schema`
/// recursively flattened. Returns `Ok(None)` for custom `#enum` so the
/// normal single-field return path can carry `TypeRepr::Enum`.
fn resolve_return_user_schema(
    return_type: Option<&TypeNode>,
    resolver: &SchemaResolver<'_>,
) -> Result<Option<Schema>, LoweringError> {
    let Some(t) = return_type else {
        return Ok(None);
    };
    if t.path.len() != 1 || !t.generics.is_empty() || t.variant_fields.is_some() {
        return Ok(None);
    }
    let name = &t.path[0];
    // Built-in scalar / wrapper heads stay on the scalar path even
    // though they would also fail the user-schema lookup below.
    if matches!(
        name.as_str(),
        "Int"
            | "Float"
            | "Bool"
            | "String"
            | "List"
            | "Option"
            | "Result"
            | "Tuple"
            | "Null"
            | "Unit"
    ) {
        return Ok(None);
    }
    let Some(def) = resolver.resolve(name) else {
        return Ok(None);
    };
    if !def.variants.is_empty() {
        return Ok(None);
    }
    let mut stack: Vec<&str> = Vec::new();
    let schema = canonical_schema_from_def(def, resolver, &mut stack, t.range)?;
    Ok(Some(schema))
}

fn canonical_enum_from_def<'a>(
    def: &'a SchemaDef,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
) -> Result<TypeRepr, LoweringError> {
    canonical_enum_from_def_with_subst(def, resolver, stack, range, &GenericSubst::new())
}

fn canonical_enum_from_def_with_subst<'a>(
    def: &'a SchemaDef,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
    subst: &GenericSubst,
) -> Result<TypeRepr, LoweringError> {
    let name = def.name.as_deref().ok_or_else(|| {
        cap!(
            "canonical_schema_from_def.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: "anonymous-enum-schema".to_string(),
                range,
            }
        )
    })?;
    if stack.contains(&name) {
        let mut cycle: Vec<String> = stack.iter().map(|s| s.to_string()).collect();
        cycle.push(name.to_string());
        return Err(cap!(
            "canonical_schema_from_def.cyclic_field_dependency",
            LoweringError::CyclicFieldDependency {
                schema: name.to_string(),
                cycle,
                range,
            }
        ));
    }
    if def.variants.len() > u8::MAX as usize + 1 {
        return Err(cap!(
            "canonical_schema_from_def.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: format!("enum `{name}` has more than 256 variants"),
                range,
            }
        ));
    }

    stack.push(name);
    let mut variants = Vec::with_capacity(def.variants.len());
    for (tag, variant) in def.variants.iter().enumerate() {
        let mut fields = Vec::with_capacity(variant.fields.len());
        for field in &variant.fields {
            let ty_node = field.type_hint.as_ref().ok_or_else(|| {
                cap!(
                    "canonical_schema_from_def.unsupported_field_type",
                    LoweringError::UnsupportedFieldType {
                        schema: name.to_string(),
                        field: format!("{}.{}", variant.name, field.name),
                        ty: "<untyped>".to_string(),
                        range: field.value_range,
                    }
                )
            })?;
            fields.push(Field {
                name: field.name.clone(),
                ty: canonical_type_repr_with_subst(
                    ty_node,
                    resolver,
                    stack,
                    field.value_range,
                    subst,
                )?,
                default: None,
            });
        }
        variants.push(CanonicalEnumVariant {
            name: variant.name.clone(),
            tag: tag as u8,
            is_tuple: fields_are_tuple_payload(&fields),
            fields,
        });
    }
    stack.pop();
    Ok(TypeRepr::Enum {
        name: name.to_string(),
        variants,
    })
}

fn fields_are_tuple_payload(fields: &[Field]) -> bool {
    !fields.is_empty()
        && fields
            .iter()
            .enumerate()
            .all(|(idx, field)| field.name == idx.to_string())
}

/// Recursively build a canonical [`Schema`] from a [`SchemaDef`].
///
/// `stack` carries the in-progress schema names so a cycle in nested
/// declarations (`#schema A { B b: * }`, `#schema B { A a: * }`)
/// surfaces as [`LoweringError::CyclicFieldDependency`] rather than
/// infinite recursion. Cycles in nested-schema *types* are
/// independent of the per-schema field-default cycle the topological
/// emit pass detects later — both surface the same error variant so
/// users get a uniform diagnostic for either shape.
fn canonical_schema_from_def<'a>(
    def: &'a SchemaDef,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
) -> Result<Schema, LoweringError> {
    canonical_schema_from_def_with_subst(def, resolver, stack, range, &GenericSubst::new())
}

fn canonical_schema_from_def_with_subst<'a>(
    def: &'a SchemaDef,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
    subst: &GenericSubst,
) -> Result<Schema, LoweringError> {
    let name = def.name.as_deref().ok_or_else(|| {
        cap!(
            "canonical_schema_from_def.unsupported_expr",
            LoweringError::UnsupportedExpr {
                kind: "anonymous-nested-schema".to_string(),
                range,
            }
        )
    })?;
    if stack.contains(&name) {
        let mut cycle: Vec<String> = stack.iter().map(|s| s.to_string()).collect();
        cycle.push(name.to_string());
        return Err(cap!(
            "canonical_schema_from_def.cyclic_field_dependency",
            LoweringError::CyclicFieldDependency {
                schema: name.to_string(),
                cycle,
                range,
            }
        ));
    }
    stack.push(name);
    if let Some(elements) = &def.tuple_elements {
        let mut tys = Vec::with_capacity(elements.len());
        for (idx, ty_node) in elements.iter().enumerate() {
            let ty = canonical_type_repr_with_subst(ty_node, resolver, stack, range, subst)
                .map_err(|_| {
                    cap!(
                        "canonical_schema_from_def.unsupported_tuple_element_type",
                        LoweringError::UnsupportedFieldType {
                            schema: name.to_string(),
                            field: idx.to_string(),
                            ty: type_head_for_display(ty_node),
                            range: ty_node.range,
                        }
                    )
                })?;
            tys.push(ty);
        }
        stack.pop();
        let mut schema = Schema::tuple(name.to_string(), tys);
        schema.generics = def.generics.clone();
        return Ok(schema);
    }
    let mut fields = Vec::with_capacity(def.fields.len());
    for f in &def.fields {
        let ty_node = f.type_hint.as_ref().ok_or_else(|| {
            cap!(
                "canonical_schema_from_def.unsupported_field_type",
                LoweringError::UnsupportedFieldType {
                    schema: name.to_string(),
                    field: f.name.clone(),
                    ty: "<untyped>".to_string(),
                    range: f.value_range,
                }
            )
        })?;
        let ty = canonical_type_repr_with_subst(ty_node, resolver, stack, f.value_range, subst)?;
        fields.push(Field {
            name: f.name.clone(),
            ty,
            default: None,
        });
    }
    stack.pop();
    Ok(Schema {
        name: name.to_string(),
        generics: def.generics.clone(),
        fields,
        is_tuple: false,
    })
}

/// Convert a schema field type into the canonical [`TypeRepr`]. This is the
/// resolver-aware form used for named schemas, including tuple schemas.
fn canonical_type_repr<'a>(
    ty: &TypeNode,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
) -> Result<TypeRepr, LoweringError> {
    canonical_type_repr_with_subst(ty, resolver, stack, range, &GenericSubst::new())
}

fn canonical_type_repr_with_subst<'a>(
    ty: &TypeNode,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
    subst: &GenericSubst,
) -> Result<TypeRepr, LoweringError> {
    let concrete_ty;
    let ty = if subst.is_empty() {
        ty
    } else {
        concrete_ty = apply_generic_subst(ty, subst);
        &concrete_ty
    };
    if ty.path.len() != 1 || ty.variant_fields.is_some() {
        return Err(cap!(
            "canonical_type_repr.unsupported_field_type.1",
            LoweringError::UnsupportedFieldType {
                schema: stack.last().copied().unwrap_or("?").to_string(),
                field: "?".to_string(),
                ty: type_head_for_display(ty),
                range,
            }
        ));
    }

    let head = ty.path[0].as_str();
    if is_removed_unit_null_type_name(head) {
        return Err(cap!(
            "canonical_type_repr.unsupported_field_type.reserved",
            LoweringError::UnsupportedFieldType {
                schema: stack.last().copied().unwrap_or("?").to_string(),
                field: "?".to_string(),
                ty: head.to_string(),
                range,
            }
        ));
    }

    let base = match (head, ty.generics.as_slice()) {
        ("Int", []) => TypeRepr::Int,
        ("Float", []) => TypeRepr::Float,
        ("Bool", []) => TypeRepr::Bool,
        ("String", []) => TypeRepr::String,
        ("List", [elem]) => TypeRepr::List {
            element: Box::new(canonical_type_repr(elem, resolver, stack, range)?),
        },
        ("Option", [inner]) => TypeRepr::Option {
            inner: Box::new(canonical_type_repr(inner, resolver, stack, range)?),
        },
        ("Result", [ok, err]) => TypeRepr::Result {
            ok: Box::new(canonical_type_repr(ok, resolver, stack, range)?),
            err: Box::new(canonical_type_repr(err, resolver, stack, range)?),
        },
        ("Tuple", _) => TypeRepr::Schema {
            schema: Box::new(
                tuple_type_node_to_schema(ty, Some(resolver)).ok_or_else(|| {
                    cap!(
                        "canonical_type_repr.unsupported_field_type.tuple",
                        LoweringError::UnsupportedFieldType {
                            schema: stack.last().copied().unwrap_or("?").to_string(),
                            field: "?".to_string(),
                            ty: type_head_for_display(ty),
                            range,
                        }
                    )
                })?,
            ),
        },
        _ => {
            if matches!(
                head,
                "Int" | "Float" | "Bool" | "String" | "List" | "Option" | "Result" | "Tuple"
            ) {
                return Err(cap!(
                    "canonical_type_repr.unsupported_field_type.2",
                    LoweringError::UnsupportedFieldType {
                        schema: stack.last().copied().unwrap_or("?").to_string(),
                        field: "?".to_string(),
                        ty: type_head_for_display(ty),
                        range,
                    }
                ));
            }
            let Some(def) = resolver.resolve(head) else {
                return Err(cap!(
                    "canonical_type_repr.unsupported_field_type.3",
                    LoweringError::UnsupportedFieldType {
                        schema: stack.last().copied().unwrap_or("?").to_string(),
                        field: "?".to_string(),
                        ty: head.to_string(),
                        range,
                    }
                ));
            };
            let Some(schema_subst) = generic_subst_for_def(def, ty) else {
                return Err(cap!(
                    "canonical_type_repr.unsupported_field_type.generics",
                    LoweringError::UnsupportedFieldType {
                        schema: stack.last().copied().unwrap_or("?").to_string(),
                        field: "?".to_string(),
                        ty: type_head_for_display(ty),
                        range,
                    }
                ));
            };
            if !def.variants.is_empty() {
                canonical_enum_from_def_with_subst(def, resolver, stack, range, &schema_subst)?
            } else {
                TypeRepr::Schema {
                    schema: Box::new(canonical_schema_from_def_with_subst(
                        def,
                        resolver,
                        stack,
                        range,
                        &schema_subst,
                    )?),
                }
            }
        }
    };

    Ok(maybe_optional(ty, base))
}

/// Decide topological order in which a schema's fields must be
/// emitted, given the set of user-provided field names. A field
/// that's user-provided stops dependency tracking for itself (the
/// user value wins and is independent of the schema default).
/// Otherwise the default expression's referenced sibling fields
/// become incoming edges.
///
/// Returns `Err(CyclicFieldDependency)` when the dependency graph on
/// the **needs-defaults** subset has a cycle. User-provided values
/// can break a cycle: a schema `{ Int a: b, Int b: a }` constructed
/// as `{ a: 1 }` is fine — only `b` needs defaulting and its
/// reference to `a` is satisfied by the user value.
fn topo_order_fields(
    schema_name: &str,
    def: &SchemaDef,
    user_provided: &std::collections::HashSet<&str>,
    range: TokenRange,
) -> Result<Vec<usize>, LoweringError> {
    // Collect per-field referenced sibling field names. Only fields
    // we'll evaluate via their default expression need this — others
    // get the user-supplied value and contribute no edges.
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); def.fields.len()];
    let name_to_idx: HashMap<&str, usize> = def
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), i))
        .collect();
    for (i, field) in def.fields.iter().enumerate() {
        if user_provided.contains(field.name.as_str()) {
            // User-supplied: ignore its default expression.
            continue;
        }
        if field.is_wildcard {
            // `Int x: *` declares the field with no default value.
            // The dict literal must provide it.
            return Err(cap!(
                "topo_order_fields.missing_field_no_default",
                LoweringError::MissingFieldNoDefault {
                    schema: schema_name.to_string(),
                    field: field.name.clone(),
                    range,
                }
            ));
        }
        collect_field_refs(&field.value_node.expr, &name_to_idx, &mut deps[i]);
        // Sanity: every reference must resolve to a sibling field.
        // We can't know yet which references are unresolved at this
        // step — `collect_field_refs` only walks bare-identifier
        // references; an unresolved one was already a diagnostic at
        // analyzer time. We still surface the case where a default
        // expression names a sibling that doesn't exist as
        // `UnknownFieldReferenceInDefault`. The walk runs the same
        // resolution again and reports the first miss.
        check_field_default_refs_resolvable(
            schema_name,
            &field.name,
            &field.value_node.expr,
            &name_to_idx,
        )?;
    }
    // Kahn-style topological sort. `incoming[i]` = number of edges
    // pointing into i. A field `j` evaluated from a default that
    // references `i` requires `i` ready first → edge `i → j`. We
    // build the graph from `deps[i] = list of i's prerequisite
    // field indices` ⇒ for every `r ∈ deps[i]` add edge `r → i`,
    // i.e. incoming[i] += 1 for each ref.
    let n = def.fields.len();
    let mut incoming = vec![0usize; n];
    let mut outgoing: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, refs) in deps.iter().enumerate() {
        for &r in refs {
            outgoing[r].push(i);
            incoming[i] += 1;
        }
    }
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut ready: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    for (i, &incoming_count) in incoming.iter().enumerate().take(n) {
        if incoming_count == 0 {
            ready.push_back(i);
        }
    }
    while let Some(i) = ready.pop_front() {
        order.push(i);
        for &j in &outgoing[i] {
            incoming[j] -= 1;
            if incoming[j] == 0 {
                ready.push_back(j);
            }
        }
    }
    if order.len() != n {
        // Find one cycle path for the error message via DFS.
        let cycle = find_cycle_path(&outgoing, def, &incoming);
        return Err(cap!(
            "topo_order_fields.cyclic_field_dependency",
            LoweringError::CyclicFieldDependency {
                schema: schema_name.to_string(),
                cycle,
                range,
            }
        ));
    }
    Ok(order)
}

/// DFS through the remaining (non-zero-incoming) field-default graph
/// looking for a cycle path. The caller has already established at
/// least one cycle exists (Kahn's algorithm couldn't drain the
/// graph); we build a representative path so the user sees the field
/// chain that participates.
fn find_cycle_path(outgoing: &[Vec<usize>], def: &SchemaDef, incoming: &[usize]) -> Vec<String> {
    let n = outgoing.len();
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
            return cycle.iter().map(|&i| def.fields[i].name.clone()).collect();
        }
    }
    // Fallback: should never happen given the caller's invariant.
    Vec::new()
}

fn dfs_find_cycle(
    start: usize,
    outgoing: &[Vec<usize>],
    visited: &mut [bool],
    on_stack: &mut [bool],
    stack: &mut Vec<usize>,
) -> Option<Vec<usize>> {
    visited[start] = true;
    on_stack[start] = true;
    stack.push(start);
    for &next in &outgoing[start] {
        if on_stack[next] {
            // Cycle: emit the portion of the stack from `next` to the
            // current node, plus `next` repeated at the end for a
            // readable arrow chain.
            let entry = stack.iter().position(|&i| i == next).unwrap_or(0);
            let mut cycle = stack[entry..].to_vec();
            cycle.push(next);
            on_stack[start] = false;
            stack.pop();
            return Some(cycle);
        }
        if !visited[next] {
            if let Some(c) = dfs_find_cycle(next, outgoing, visited, on_stack, stack) {
                on_stack[start] = false;
                stack.pop();
                return Some(c);
            }
        }
    }
    on_stack[start] = false;
    stack.pop();
    None
}

/// Walk a default expression and record any bare-identifier
/// references whose head matches a sibling field. Multi-segment
/// references (`a.b.c`) only contribute the head segment — if the
/// head resolves to a sibling, the rest of the path is treated as a
/// post-access we don't track.
fn collect_field_refs(expr: &Expr, name_to_idx: &HashMap<&str, usize>, out: &mut Vec<usize>) {
    match expr {
        Expr::Variable(path) | Expr::Reference { path, .. } => {
            if let Some(TokenKey::String(name, _, _)) = path.first() {
                if let Some(&idx) = name_to_idx.get(name.as_str()) {
                    if !out.contains(&idx) {
                        out.push(idx);
                    }
                }
            }
        }
        Expr::Binary(_, a, b) => {
            collect_field_refs(&a.expr, name_to_idx, out);
            collect_field_refs(&b.expr, name_to_idx, out);
        }
        Expr::Unary(_, inner) => collect_field_refs(&inner.expr, name_to_idx, out),
        Expr::Ternary { cond, then, els } => {
            collect_field_refs(&cond.expr, name_to_idx, out);
            collect_field_refs(&then.expr, name_to_idx, out);
            collect_field_refs(&els.expr, name_to_idx, out);
        }
        Expr::List(items) => {
            for n in items {
                collect_field_refs(&n.expr, name_to_idx, out);
            }
        }
        Expr::Dict(pairs) => {
            for (_, v) in pairs {
                collect_field_refs(&v.expr, name_to_idx, out);
            }
        }
        Expr::Where { expr, bindings } => {
            collect_field_refs(&bindings.expr, name_to_idx, out);
            collect_field_refs(&expr.expr, name_to_idx, out);
        }
        Expr::FnCall { args, .. } => {
            for a in args {
                collect_field_refs(&a.value.expr, name_to_idx, out);
            }
        }
        // Other shapes don't matter for the Phase 3.b surface (they
        // either fail to lower upstream or don't reference siblings).
        _ => {}
    }
}

/// Recursive walker mirroring [`collect_field_refs`] that reports the
/// first bare-identifier reference whose head doesn't resolve to a
/// sibling field. Lowering uses this to surface
/// `UnknownFieldReferenceInDefault` instead of letting the inner
/// `lower_expr` fall through with an `UnresolvedVariable` (which the
/// user would see as a confusing diagnostic about `#main` params).
fn check_field_default_refs_resolvable(
    schema: &str,
    field: &str,
    expr: &Expr,
    name_to_idx: &HashMap<&str, usize>,
) -> Result<(), LoweringError> {
    let mut stack: Vec<&Expr> = vec![expr];
    while let Some(e) = stack.pop() {
        match e {
            Expr::Variable(path) | Expr::Reference { path, .. } => {
                if let Some(TokenKey::String(name, range, _)) = path.first() {
                    if !name_to_idx.contains_key(name.as_str()) {
                        return Err(cap!("check_field_default_refs_resolvable.unknown_field_reference_in_default", LoweringError::UnknownFieldReferenceInDefault {
                            schema: schema.to_string(),
                            field: field.to_string(),
                            referenced: name.clone(),
                            range: *range,
                        }));
                    }
                }
            }
            Expr::Binary(_, a, b) => {
                stack.push(&a.expr);
                stack.push(&b.expr);
            }
            Expr::Unary(_, inner) => stack.push(&inner.expr),
            Expr::Ternary { cond, then, els } => {
                stack.push(&cond.expr);
                stack.push(&then.expr);
                stack.push(&els.expr);
            }
            Expr::List(items) => {
                for n in items {
                    stack.push(&n.expr);
                }
            }
            Expr::Dict(pairs) => {
                for (_, v) in pairs {
                    stack.push(&v.expr);
                }
            }
            Expr::Where { expr, bindings } => {
                stack.push(&expr.expr);
                stack.push(&bindings.expr);
            }
            Expr::FnCall { args, .. } => {
                for a in args {
                    stack.push(&a.value.expr);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Map a Phase 3.b `TypeRepr` to its corresponding `IrType` in dict
/// field context. Reuses [`type_repr_to_ir_type`] for the strict
/// subset (base types + `List<base>`) and extends with the cases
/// only dict fields can carry: nested branded `Schema { .. }` rides a
/// pointer slot, and `Option` / `Result` fold to i32 too. Hand-
/// crafted ill-formed schemas (where the layout pass would normally
/// reject) get a silent `IrType::ListInt` fallback to keep the
/// lowering total.
fn type_repr_to_ir_type_dict(t: &TypeRepr) -> IrType {
    if let Ok(ty) = type_repr_to_ir_type(t) {
        return ty;
    }
    match t {
        TypeRepr::Schema { .. }
        | TypeRepr::Option { .. }
        | TypeRepr::Result { .. }
        | TypeRepr::Enum { .. } => IrType::I32,
        TypeRepr::List { .. } => IrType::ListInt,
        _ => IrType::I32,
    }
}

fn align_up_for_lowering(value: usize, align: usize) -> usize {
    if align <= 1 {
        value
    } else {
        (value + (align - 1)) & !(align - 1)
    }
}

fn payload_slot_layout_for_lowering(ty: &TypeRepr) -> Result<(usize, usize), LoweringError> {
    match ty {
        TypeRepr::Bool | TypeRepr::Unit => Ok((1, 1)),
        TypeRepr::Int | TypeRepr::Float => Ok((8, 8)),
        TypeRepr::String
        | TypeRepr::List { .. }
        | TypeRepr::Schema { .. }
        | TypeRepr::Option { .. }
        | TypeRepr::Result { .. }
        | TypeRepr::Enum { .. } => Ok((4, 4)),
        other => Err(cap!(
            "payload_slot_layout_for_lowering.unsupported_type",
            LoweringError::UnsupportedTypeInMain {
                type_name: format!("{other:?}"),
                range: TokenRange::default(),
            }
        )),
    }
}

fn type_graph_alignment_for_lowering(ty: &TypeRepr) -> Result<usize, LoweringError> {
    match ty {
        TypeRepr::Bool | TypeRepr::Unit => Ok(1),
        TypeRepr::Int | TypeRepr::Float => Ok(8),
        TypeRepr::String | TypeRepr::List { .. } | TypeRepr::Schema { .. } => Ok(4),
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => {
            variant_record_alignment_for_lowering(ty)
        }
        other => Err(cap!(
            "type_graph_alignment_for_lowering.unsupported_type",
            LoweringError::UnsupportedTypeInMain {
                type_name: format!("{other:?}"),
                range: TokenRange::default(),
            }
        )),
    }
}

fn variant_record_alignment_for_lowering(ty: &TypeRepr) -> Result<usize, LoweringError> {
    let payloads: Vec<TypeRepr> = match ty {
        TypeRepr::Option { inner } => vec![inner.as_ref().clone()],
        TypeRepr::Result { ok, err } => vec![ok.as_ref().clone(), err.as_ref().clone()],
        TypeRepr::Enum { name, variants } => variants
            .iter()
            .filter_map(|variant| {
                variant.payload_schema(name).map(|schema| TypeRepr::Schema {
                    schema: Box::new(schema),
                })
            })
            .collect(),
        other => {
            return Err(cap!(
                "variant_record_alignment_for_lowering.unsupported_type",
                LoweringError::UnsupportedTypeInMain {
                    type_name: format!("{other:?}"),
                    range: TokenRange::default(),
                }
            ))
        }
    };
    let mut align = 4usize;
    for payload in &payloads {
        let (_, slot_align) = payload_slot_layout_for_lowering(payload)?;
        align = align
            .max(slot_align)
            .max(type_graph_alignment_for_lowering(payload)?);
    }
    Ok(align)
}

fn variant_payload_offset_for_lowering(payload_ty: &TypeRepr) -> Result<usize, LoweringError> {
    let (_, payload_align) = payload_slot_layout_for_lowering(payload_ty)?;
    Ok(align_up_for_lowering(1, payload_align))
}

fn variant_body_pairs(
    body: &Node,
    range: TokenRange,
) -> Result<&[(TokenKey, Node)], LoweringError> {
    match &*body.expr {
        Expr::Dict(pairs) => Ok(pairs.as_slice()),
        other => Err(cap!(
            "lower_variant_ctor_as_type.body_not_dict",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "variant constructor body must be a dict literal, got `{}`",
                    other.kind()
                ),
                range,
            }
        )),
    }
}

fn variant_payload_node<'a>(
    pairs: &'a [(TokenKey, Node)],
    key_name: &str,
    range: TokenRange,
) -> Result<&'a Node, LoweringError> {
    let mut found: Option<&Node> = None;
    for (key, value) in pairs {
        let TokenKey::String(name, _, _) = key else {
            return Err(cap!(
                "lower_variant_ctor_as_type.non_string_key",
                LoweringError::UnsupportedExpr {
                    kind: "variant constructor field key must be a string identifier".to_string(),
                    range,
                }
            ));
        };
        if name == key_name {
            if found.is_some() {
                return Err(cap!(
                    "lower_variant_ctor_as_type.duplicate_payload",
                    LoweringError::UnsupportedExpr {
                        kind: format!("duplicate variant payload field `{key_name}`"),
                        range,
                    }
                ));
            }
            found = Some(value);
        } else {
            return Err(cap!(
                "lower_variant_ctor_as_type.unexpected_field",
                LoweringError::UnsupportedExpr {
                    kind: format!("unexpected variant payload field `{name}`"),
                    range,
                }
            ));
        }
    }
    found.ok_or_else(|| {
        cap!(
            "lower_variant_ctor_as_type.missing_payload",
            LoweringError::UnsupportedExpr {
                kind: format!("missing variant payload field `{key_name}`"),
                range,
            }
        )
    })
}

fn lower_schema_value_as_absolute_pointer(
    schema: &Schema,
    value: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    match (&*value.expr, schema.is_tuple) {
        (Expr::Dict(pairs), false) => {
            let layout = SchemaLayout::offsets_for(schema)?;
            let record_local = ctx.alloc_record_local();
            ctx.out.push(TaggedOp {
                op: alloc_record_op(
                    ctx,
                    record_local,
                    layout.root_size as u32,
                    layout.root_align as u32,
                ),
                range,
            });
            if ctx.schema_resolver.resolve(&schema.name).is_some() {
                lower_dict_into_record(schema, &layout, pairs, range, record_local, ctx)?;
            } else {
                lower_plain_dict_into_record(schema, &layout, pairs, range, record_local, ctx)?;
            }
            push_record_base_for_pointer(record_local, range, ctx);
            Ok(())
        }
        (Expr::Tuple(elements), true) => {
            if elements.len() != schema.fields.len() {
                return Err(cap!(
                    "lower_schema_value_as_absolute_pointer.arity_mismatch",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "tuple payload has {} elements but schema declares {}",
                            elements.len(),
                            schema.fields.len()
                        ),
                        range,
                    }
                ));
            }
            let layout = SchemaLayout::offsets_for(schema)?;
            let record_local = ctx.alloc_record_local();
            ctx.out.push(TaggedOp {
                op: alloc_record_op(
                    ctx,
                    record_local,
                    layout.root_size as u32,
                    layout.root_align as u32,
                ),
                range,
            });
            lower_tuple_into_record(schema, &layout, elements, record_local, ctx)?;
            push_record_base_for_pointer(record_local, range, ctx);
            Ok(())
        }
        _ => lower_expr(&value.expr, range, ctx),
    }
}

fn alloc_record_op(ctx: &LowerCtx<'_>, record_local: u32, root_size: u32, root_align: u32) -> Op {
    if ctx.variant_records_in_scratch {
        Op::AllocScratchRecord {
            record_local_idx: record_local,
            root_size,
            root_align,
        }
    } else {
        Op::AllocSubRecord {
            record_local_idx: record_local,
            root_size,
            root_align,
        }
    }
}

fn store_field_at_record_op(ctx: &LowerCtx<'_>, record_local: u32, offset: u32, ty: IrType) -> Op {
    if ctx.variant_records_in_scratch {
        Op::StoreFieldAtRecordAbsolute {
            record_local_idx: record_local,
            offset,
            ty,
        }
    } else {
        Op::StoreFieldAtRecord {
            record_local_idx: record_local,
            offset,
            ty,
        }
    }
}

fn push_record_base_for_pointer(record_local: u32, range: TokenRange, ctx: &mut LowerCtx<'_>) {
    if ctx.variant_records_in_scratch {
        ctx.out.push(TaggedOp {
            op: Op::PushRecordBaseAbsolute {
                record_local_idx: record_local,
            },
            range,
        });
        ctx.tstack.push(IrType::I32);
    } else {
        push_record_base_as_absolute(record_local, range, ctx);
    }
}

fn push_record_base_as_absolute(record_local: u32, range: TokenRange, ctx: &mut LowerCtx<'_>) {
    ctx.out.push(TaggedOp {
        op: Op::PushRecordBase {
            record_local_idx: record_local,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::LocalGet(2),
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
}

fn lower_value_as_type(
    expected: &TypeRepr,
    value: &Node,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    match (expected, &*value.expr) {
        (expected, Expr::Ternary { cond, then, els }) => {
            lower_ternary_as_type(expected, cond, then, els, value.range, ctx)
        }
        (TypeRepr::List { element }, Expr::FnCall { path, args })
            if variant_record_list_element(element) =>
        {
            if let Some(()) = peephole::emit_variant_list_filter_call_as_type(
                element,
                path,
                args,
                value.range,
                ctx,
            )? {
                Ok(())
            } else if let Some(()) = peephole::emit_variant_list_filter_method_as_type(
                element,
                path,
                args,
                value.range,
                ctx,
            )? {
                Ok(())
            } else if let Some(()) =
                peephole::emit_variant_list_map_call_as_type(element, path, args, value.range, ctx)?
            {
                Ok(())
            } else if let Some(()) = peephole::emit_variant_list_map_method_as_type(
                element,
                path,
                args,
                value.range,
                ctx,
            )? {
                Ok(())
            } else {
                lower_expr(&value.expr, value.range, ctx)
            }
        }
        (
            TypeRepr::List {
                element: expected_element,
            },
            Expr::Comprehension {
                element,
                id,
                iterable,
                condition,
            },
        ) if variant_record_list_element(expected_element) => lower_comprehension_as_type(
            expected_element,
            element,
            id,
            iterable,
            condition.as_ref(),
            value.range,
            ctx,
        ),
        (TypeRepr::List { element }, Expr::List(items)) if variant_record_list_element(element) => {
            lower_variant_record_list_literal(element, items, value.range, ctx)
        }
        (
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. },
            Expr::VariantCtor {
                enum_path,
                variant,
                body,
            },
        ) => lower_variant_ctor_as_type(expected, enum_path, variant, body, value.range, ctx),
        (
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. },
            Expr::Variable(path),
        ) => {
            if let Some(variant) = variant_name_from_path(expected, path, false) {
                lower_standard_variant_record(expected, variant.as_str(), None, value.range, ctx)
            } else {
                lower_expr(&value.expr, value.range, ctx)
            }
        }
        (
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. },
            Expr::FnCall { path, args },
        ) => {
            if let Some(variant) = variant_name_from_path(expected, path, true) {
                lower_variant_call_as_type(expected, variant.as_str(), args, value.range, ctx)
            } else {
                lower_expr(&value.expr, value.range, ctx)
            }
        }
        (TypeRepr::Schema { schema }, _) => {
            lower_schema_value_as_absolute_pointer(schema, value, value.range, ctx)
        }
        _ => lower_expr(&value.expr, value.range, ctx),
    }
}

fn variant_record_list_element(element: &TypeRepr) -> bool {
    matches!(
        element,
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. }
    )
}

fn variant_list_literal_for_type(expected: &TypeRepr, expr: &Expr) -> bool {
    matches!(expected, TypeRepr::List { element } if variant_record_list_element(element))
        && matches!(expr, Expr::List(_))
}

fn variant_record_list_inplace_expr_for_type(expected: &TypeRepr, expr: &Expr) -> bool {
    if !matches!(expected, TypeRepr::List { element } if variant_record_list_element(element)) {
        return false;
    }
    match expr {
        Expr::List(_) | Expr::Comprehension { .. } => true,
        Expr::FnCall { path, .. } => {
            if let [TokenKey::String(name, _, _), ..] = path.as_slice() {
                if name == "_list_map" || name == "_list_filter" {
                    return true;
                }
            }
            matches!(path.last(), Some(TokenKey::String(name, _, _)) if name == "map" || name == "filter")
        }
        _ => false,
    }
}

fn lower_variant_record_list_literal(
    element: &TypeRepr,
    items: &[Node],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    for item in items {
        lower_value_as_type(element, item, ctx)?;
        let top = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_variant_record_list_literal.empty_element_stack",
                LoweringError::UnsupportedExpr {
                    kind: "List<Enum>(element produced no value)".to_string(),
                    range: item.range,
                }
            )
        })?;
        if top.wasm_slot() != IrType::I32.wasm_slot() {
            return Err(cap!(
                "lower_variant_record_list_literal.element_type_mismatch",
                LoweringError::UnsupportedExpr {
                    kind: format!("List<Enum>(element produced {top:?}, expected variant pointer)"),
                    range: item.range,
                }
            ));
        }
    }
    let len = u32::try_from(items.len()).map_err(|_| {
        cap!(
            "lower_variant_record_list_literal.length_overflow",
            LoweringError::UnsupportedExpr {
                kind: "List<Enum>(too many elements)".to_string(),
                range,
            }
        )
    })?;
    ctx.out.push(TaggedOp {
        op: Op::BuildPointerList { len },
        range,
    });
    ctx.tstack.push(IrType::ListList);
    Ok(())
}

#[derive(Debug, Clone)]
struct VariantPayloadShape {
    ty: TypeRepr,
    key: Option<&'static str>,
}

#[derive(Debug, Clone)]
struct VariantShape {
    tag: u8,
    payload: Option<VariantPayloadShape>,
}

fn path_strings(path: &[TokenKey]) -> Option<Vec<&str>> {
    path.iter()
        .map(|key| match key {
            TokenKey::String(name, _, _) => Some(name.as_str()),
            _ => None,
        })
        .collect()
}

fn enum_path_matches(expected_name: &str, enum_path: Option<&[String]>) -> bool {
    match enum_path {
        Some(path) => path.is_empty() || path == [expected_name],
        None => true,
    }
}

fn variant_name_from_path(
    expected: &TypeRepr,
    path: &[TokenKey],
    require_payload: bool,
) -> Option<String> {
    let parts = path_strings(path)?;
    match expected {
        TypeRepr::Option { .. } => {
            let name = match parts.as_slice() {
                ["None"] if !require_payload => "None",
                ["Option", "None"] if !require_payload => "None",
                ["Some"] if require_payload => "Some",
                ["Option", "Some"] if require_payload => "Some",
                _ => return None,
            };
            Some(name.to_string())
        }
        TypeRepr::Result { .. } => {
            let name = match parts.as_slice() {
                ["Ok"] if require_payload => "Ok",
                ["Result", "Ok"] if require_payload => "Ok",
                ["Err"] if require_payload => "Err",
                ["Result", "Err"] if require_payload => "Err",
                _ => return None,
            };
            Some(name.to_string())
        }
        TypeRepr::Enum { name, variants } => {
            let variant_name = match parts.as_slice() {
                [variant] => *variant,
                [enum_name, variant] if enum_name == name => *variant,
                _ => return None,
            };
            let variant = variants.iter().find(|v| v.name == variant_name)?;
            if require_payload != variant.fields.is_empty() {
                Some(variant.name.clone())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn standard_variant_shape(
    expected: &TypeRepr,
    enum_path: Option<&[String]>,
    variant: &str,
    range: TokenRange,
) -> Result<VariantShape, LoweringError> {
    match expected {
        TypeRepr::Option { inner } => {
            if !enum_path_matches("Option", enum_path) {
                return Err(cap!(
                    "standard_variant_shape.option_enum_mismatch",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "expected Option variant, got {}.{variant}",
                            enum_path.map(|p| p.join(".")).unwrap_or_default()
                        ),
                        range,
                    }
                ));
            }
            match variant {
                "None" => Ok(VariantShape {
                    tag: 0,
                    payload: None,
                }),
                "Some" => Ok(VariantShape {
                    tag: 1,
                    payload: Some(VariantPayloadShape {
                        ty: inner.as_ref().clone(),
                        key: Some("value"),
                    }),
                }),
                other => Err(cap!(
                    "standard_variant_shape.option_variant_mismatch",
                    LoweringError::UnsupportedExpr {
                        kind: format!("unknown Option variant `{other}`"),
                        range,
                    }
                )),
            }
        }
        TypeRepr::Result { ok, err } => {
            if !enum_path_matches("Result", enum_path) {
                return Err(cap!(
                    "standard_variant_shape.result_enum_mismatch",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "expected Result variant, got {}.{variant}",
                            enum_path.map(|p| p.join(".")).unwrap_or_default()
                        ),
                        range,
                    }
                ));
            }
            match variant {
                "Ok" => Ok(VariantShape {
                    tag: 0,
                    payload: Some(VariantPayloadShape {
                        ty: ok.as_ref().clone(),
                        key: Some("value"),
                    }),
                }),
                "Err" => Ok(VariantShape {
                    tag: 1,
                    payload: Some(VariantPayloadShape {
                        ty: err.as_ref().clone(),
                        key: Some("error"),
                    }),
                }),
                other => Err(cap!(
                    "standard_variant_shape.result_variant_mismatch",
                    LoweringError::UnsupportedExpr {
                        kind: format!("unknown Result variant `{other}`"),
                        range,
                    }
                )),
            }
        }
        TypeRepr::Enum { name, variants } => {
            if !enum_path_matches(name, enum_path) {
                return Err(cap!(
                    "standard_variant_shape.not_variant_type",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "expected {name} variant, got {}.{variant}",
                            enum_path.map(|p| p.join(".")).unwrap_or_default()
                        ),
                        range,
                    }
                ));
            }
            let Some(v) = variants.iter().find(|v| v.name == variant) else {
                return Err(cap!(
                    "standard_variant_shape.not_variant_type",
                    LoweringError::UnsupportedExpr {
                        kind: format!("unknown {name} variant `{variant}`"),
                        range,
                    }
                ));
            };
            Ok(VariantShape {
                tag: v.tag,
                payload: v.payload_schema(name).map(|schema| VariantPayloadShape {
                    ty: TypeRepr::Schema {
                        schema: Box::new(schema),
                    },
                    key: None,
                }),
            })
        }
        other => Err(cap!(
            "standard_variant_shape.not_variant_type",
            LoweringError::UnsupportedExpr {
                kind: format!("variant constructor needs enum target, got `{other:?}`"),
                range,
            }
        )),
    }
}

fn lower_variant_call_as_type(
    expected: &TypeRepr,
    variant: &str,
    args: &[CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let shape = standard_variant_shape(expected, None, variant, range)?;
    let Some(payload) = shape.payload.as_ref() else {
        if args.is_empty() {
            return emit_standard_variant_record(
                expected, variant, shape.tag, None, None, range, ctx,
            );
        }
        return Err(cap!(
            "lower_prelude_variant_call_as_type.arity_mismatch",
            LoweringError::UnsupportedExpr {
                kind: format!("{variant}(...) does not take payload"),
                range,
            }
        ));
    };

    if payload.key.is_some() {
        if args.len() != 1 || args[0].name.is_some() {
            return Err(cap!(
                "lower_prelude_variant_call_as_type.arity_mismatch",
                LoweringError::UnsupportedExpr {
                    kind: format!("{variant}(...) expects exactly one positional payload"),
                    range,
                }
            ));
        }
        return emit_standard_variant_record(
            expected,
            variant,
            shape.tag,
            Some(&payload.ty),
            Some(&args[0].value),
            range,
            ctx,
        );
    }

    let TypeRepr::Schema { schema } = &payload.ty else {
        return Err(cap!(
            "standard_variant_shape.not_variant_type",
            LoweringError::UnsupportedExpr {
                kind: format!("variant `{variant}` payload is not a record"),
                range,
            }
        ));
    };
    if !fields_are_tuple_payload(&schema.fields) {
        return Err(cap!(
            "lower_prelude_variant_call_as_type.arity_mismatch",
            LoweringError::UnsupportedExpr {
                kind: format!("struct variant `{variant}` must be constructed with `{{ ... }}`"),
                range,
            }
        ));
    }
    if args.len() != schema.fields.len() || args.iter().any(|arg| arg.name.is_some()) {
        return Err(cap!(
            "lower_prelude_variant_call_as_type.arity_mismatch",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "{variant}(...) expects {} positional payload values",
                    schema.fields.len()
                ),
                range,
            }
        ));
    }
    let layout = SchemaLayout::offsets_for(schema)?;
    let record_local = ctx.alloc_record_local();
    ctx.out.push(TaggedOp {
        op: alloc_record_op(
            ctx,
            record_local,
            layout.root_size as u32,
            layout.root_align as u32,
        ),
        range,
    });
    for (idx, arg) in args.iter().enumerate() {
        lower_dict_field_value(schema, idx, &arg.value, arg.value.range, ctx)?;
        let canonical_field = &schema.fields[idx];
        let layout_field = &layout.fields[idx];
        let ir_ty = type_repr_to_ir_type_dict(&canonical_field.ty);
        let store_ty = match layout_field.kind {
            FieldKind::Inline { .. } => ir_ty,
            FieldKind::PointerIndirect { .. } => IrType::I32,
        };
        let top = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_tuple_return.unsupported_expr",
                LoweringError::UnsupportedExpr {
                    kind: format!("tuple variant field {idx} produced no value"),
                    range: arg.value.range,
                }
            )
        })?;
        if top.wasm_slot() != store_ty.wasm_slot() {
            return Err(cap!(
                "lower_tuple_return.unsupported_expr",
                LoweringError::UnsupportedExpr {
                    kind: format!("tuple variant field {idx}: got {top:?}, expected {store_ty:?}"),
                    range: arg.value.range,
                }
            ));
        }
        ctx.out.push(TaggedOp {
            op: store_field_at_record_op(ctx, record_local, layout_field.offset as u32, store_ty),
            range: arg.value.range,
        });
    }
    push_record_base_for_pointer(record_local, range, ctx);
    emit_variant_record_from_lowered_payload(
        expected,
        variant,
        shape.tag,
        Some(&payload.ty),
        range,
        ctx,
    )
}

fn lower_variant_ctor_as_type(
    expected: &TypeRepr,
    enum_path: &[String],
    variant: &str,
    body: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let shape = standard_variant_shape(expected, Some(enum_path), variant, range)?;
    let pairs = variant_body_pairs(body, range)?;
    let payload_node = if let Some(payload) = shape.payload.as_ref() {
        if let Some(key_name) = payload.key {
            Some(variant_payload_node(pairs, key_name, range)?)
        } else {
            Some(body)
        }
    } else {
        if !pairs.is_empty() {
            return Err(cap!(
                "lower_variant_ctor_as_type.unit_variant_has_fields",
                LoweringError::UnsupportedExpr {
                    kind: format!("variant `{variant}` does not take payload fields"),
                    range,
                }
            ));
        }
        None
    };
    emit_standard_variant_record(
        expected,
        variant,
        shape.tag,
        shape.payload.as_ref().map(|p| &p.ty),
        payload_node,
        range,
        ctx,
    )
}

fn lower_standard_variant_record(
    expected: &TypeRepr,
    variant: &str,
    payload_node: Option<&Node>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let shape = standard_variant_shape(expected, None, variant, range)?;
    emit_standard_variant_record(
        expected,
        variant,
        shape.tag,
        shape.payload.as_ref().map(|p| &p.ty),
        payload_node,
        range,
        ctx,
    )
}

fn emit_standard_variant_record(
    expected: &TypeRepr,
    variant: &str,
    tag: u8,
    payload_ty: Option<&TypeRepr>,
    payload_node: Option<&Node>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if let Some(payload_ty) = payload_ty {
        let Some(payload_node) = payload_node else {
            return Err(cap!(
                "emit_standard_variant_record.missing_payload",
                LoweringError::UnsupportedExpr {
                    kind: format!("variant `{variant}` requires a payload"),
                    range,
                }
            ));
        };
        lower_value_as_type(payload_ty, payload_node, ctx)?;
        emit_variant_record_from_lowered_payload(
            expected,
            variant,
            tag,
            Some(payload_ty),
            range,
            ctx,
        )
    } else {
        if payload_node.is_some() {
            return Err(cap!(
                "emit_standard_variant_record.unexpected_payload",
                LoweringError::UnsupportedExpr {
                    kind: format!("variant `{variant}` does not take a payload"),
                    range,
                }
            ));
        }
        emit_variant_record_from_lowered_payload(expected, variant, tag, None, range, ctx)
    }
}

fn emit_variant_record_from_lowered_payload(
    expected: &TypeRepr,
    variant: &str,
    tag: u8,
    payload_ty: Option<&TypeRepr>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let record_align = variant_record_alignment_for_lowering(expected)?;
    let (payload_offset, payload_ir_ty, record_size) = if let Some(payload_ty) = payload_ty {
        let expected_ir = type_repr_to_ir_type_dict(payload_ty);
        let top = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "emit_standard_variant_record.empty_payload_stack",
                LoweringError::UnsupportedExpr {
                    kind: format!("variant `{variant}` payload produced no value"),
                    range,
                }
            )
        })?;
        if top.wasm_slot() != expected_ir.wasm_slot() {
            return Err(cap!(
                "emit_standard_variant_record.payload_type_mismatch",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "variant `{variant}` payload produced {top:?}, expected {expected_ir:?}"
                    ),
                    range,
                }
            ));
        }
        let (payload_size, _) = payload_slot_layout_for_lowering(payload_ty)?;
        let offset = variant_payload_offset_for_lowering(payload_ty)?;
        (
            Some(offset as u32),
            Some(expected_ir),
            (offset + payload_size) as u32,
        )
    } else {
        (None, None, 1)
    };

    let op = if ctx.variant_records_in_scratch {
        Op::BuildVariantRecordScratch {
            tag,
            record_size,
            record_align: record_align as u32,
            payload_offset,
            payload_ty: payload_ir_ty,
        }
    } else {
        Op::BuildVariantRecord {
            tag,
            record_size,
            record_align: record_align as u32,
            payload_offset,
            payload_ty: payload_ir_ty,
        }
    };
    ctx.out.push(TaggedOp { op, range });
    ctx.tstack.push(IrType::I32);
    Ok(())
}

/// Lower a dict literal into the in-construction record at
/// `record_local`. The schema describes the record's shape; the
/// `OffsetTable` carries field offsets; `dict_pairs` are the user-
/// supplied fields.
///
/// Steps:
///   1. Resolve user pairs to a (name, expr) map.
///   2. Compute topological emit order from the schema defaults.
///   3. For each field in topo order, lower the value expression
///      (either user-provided or schema default) and emit the
///      matching `StoreFieldAtRecord` op.
///
/// Synthesise a `source.field` field-access node from a dict-spread
/// source expression and a contributed field name. The source is
/// expected to be a `Variable(path)`; the synthesised access appends a
/// `String(field)` segment so it lowers through the existing
/// [`lower_variable`] schema field-walk (`LoadFieldAtAbsolute` chain).
/// The synthesised node carries the source's range so diagnostics point
/// back at the spread site.
fn synthesize_field_access(source: &Node, field: &str) -> Node {
    let mut path = match source.expr.as_ref() {
        Expr::Variable(segs) => segs.clone(),
        // Non-`Variable` sources are rejected before this point by
        // `spread_source_schema`; fall back to an empty path so the
        // synthesised access loud-errors in `lower_variable` rather than
        // silently mis-lowering.
        _ => Vec::new(),
    };
    path.push(TokenKey::String(field.to_string(), source.range, false));
    Node::new(Expr::Variable(path), source.range)
}

/// Resolve the canonical [`Schema`] of a dict-spread source expression so
/// the fields it contributes are statically known. Only a
/// **statically-resolvable schema value** is admitted: a `Variable(path)`
/// whose root binds a schema-typed `#main` parameter / let / `self`,
/// optionally walking trailing field segments into nested schema fields.
///
/// Anything else (a non-`Variable` source, a non-schema root, a dynamic /
/// index segment, a field whose type is not itself a schema) is not
/// statically flattenable on the compiled path and caps loudly — the
/// silent-miscompile path is unreachable. This is the dict counterpart of
/// the list-spread `flatten_list_spread` static guard.
fn spread_source_schema(
    source: &Node,
    ctx: &LowerCtx<'_>,
    range: TokenRange,
) -> Result<Schema, LoweringError> {
    let Expr::Variable(path) = source.expr.as_ref() else {
        return Err(cap!(
            "spread_source_schema.non_variable",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "Dict(spread source `{}` is not a statically-resolvable schema value — \
                     compiled dict spread needs a schema-typed identifier source)",
                    source.expr.kind()
                ),
                range,
            }
        ));
    };
    let mut segs = path.iter();
    let head = match segs.next() {
        Some(TokenKey::String(s, _, _)) => s.as_str(),
        _ => {
            return Err(cap!(
                "spread_source_schema.non_string_head",
                LoweringError::UnsupportedExpr {
                    kind: "Dict(spread source root is not a bare identifier)".to_string(),
                    range,
                }
            ));
        }
    };

    // Resolve the root binding's canonical schema, mirroring the
    // root-resolution order in `lower_variable` (self → let → method
    // param → entry param).
    let resolve_brand = |brand: Option<&str>| -> Option<Schema> {
        brand
            .and_then(|n| ctx.schema_resolver.resolve(n))
            .and_then(|def| {
                let mut stack: Vec<&str> = Vec::new();
                canonical_schema_from_def(def, &ctx.schema_resolver, &mut stack, range).ok()
            })
    };
    let mut current_schema: Option<Schema> = if let Some(self_b) = ctx.self_binding.as_ref() {
        if head == "self" {
            Some(self_b.schema.clone())
        } else if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head) {
            resolve_brand(b.schema_brand.as_deref())
        } else if let Some(p) = ctx.method_params.iter().find(|p| p.name == head) {
            p.schema.clone()
        } else {
            None
        }
    } else if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head) {
        resolve_brand(b.schema_brand.as_deref())
    } else {
        ctx.params
            .iter()
            .find(|b| b.name == head)
            .and_then(|b| b.schema.clone())
    };

    // Walk any trailing segments into nested schema fields.
    for seg in segs {
        let field_name = match seg {
            TokenKey::String(s, _, _) => s.as_str(),
            _ => {
                return Err(cap!(
                    "spread_source_schema.non_string_segment",
                    LoweringError::UnsupportedExpr {
                        kind: "Dict(spread source path has a non-field segment)".to_string(),
                        range,
                    }
                ));
            }
        };
        let next = current_schema
            .as_ref()
            .and_then(|s| s.fields.iter().find(|f| f.name == field_name))
            .and_then(|f| match &f.ty {
                TypeRepr::Schema { schema } => Some((**schema).clone()),
                _ => None,
            });
        current_schema = next;
    }

    current_schema.ok_or_else(|| {
        cap!(
            "spread_source_schema.not_a_schema",
            LoweringError::UnsupportedExpr {
                kind: "Dict(spread source does not resolve to a statically-known schema value)"
                    .to_string(),
                range,
            }
        )
    })
}

/// Nested branded dicts recurse via the same helper after allocating
/// a fresh sub-record.
fn lower_dict_into_record(
    schema: &Schema,
    layout: &OffsetTable,
    dict_pairs: &[(TokenKey, Node)],
    range: TokenRange,
    record_local: u32,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Resolve the analyzer-side SchemaDef so default expressions can
    // be lowered. The canonical Schema we have here only carries
    // field name + type — defaults live on the SchemaDef.
    let def = ctx.schema_resolver.resolve(&schema.name).ok_or_else(|| {
        cap!(
            "lower_dict_into_record.unknown_schema_brand",
            LoweringError::UnknownSchemaBrand {
                name: schema.name.clone(),
                range,
            }
        )
    })?;

    // Build name → user-expr map. Reject duplicate keys. Values are a
    // `Cow`: an explicit `k: v` field borrows the source node; a field
    // contributed by a `...source` spread is a synthesised
    // `source.field` access (owned).
    let mut user_values: HashMap<String, std::borrow::Cow<'_, Node>> = HashMap::new();
    for (key, value) in dict_pairs {
        // Wave R12-lower: dict spread `{ ...source, k: v } -> Schema`.
        // Each field the spread source contributes (and that the result
        // schema declares) is lowered as a synthesised `source.field`
        // access into the matching schema slot — matching the tree-walk
        // `Expr::Dict` spread branch (the source's keys merge into the
        // result; the analyzer already rejects duplicate keys via
        // `DuplicateField`, so no later-key override is ever silently
        // applied). The source must be a statically-resolvable schema
        // value (a schema-typed param / let / `self`); anything else
        // caps loudly in `spread_source_schema`.
        if let TokenKey::Spread(_) = key {
            let src_schema = spread_source_schema(value, ctx, range)?;
            for src_field in &src_schema.fields {
                // Only fields the result schema declares are merged. A
                // source field absent from the result is dropped exactly
                // as the tree-walk merge would (the result `Value::Dict`
                // only keeps keys the schema validates) — but because the
                // analyzer brands the result to `schema`, every source
                // field the result keeps is one it declares; a source
                // field the result does NOT declare is not a result key.
                if !schema.fields.iter().any(|f| f.name == src_field.name) {
                    continue;
                }
                let access = synthesize_field_access(value, &src_field.name);
                if user_values
                    .insert(src_field.name.clone(), std::borrow::Cow::Owned(access))
                    .is_some()
                {
                    return Err(cap!(
                        "lower_dict_into_record.duplicate_spread_field",
                        LoweringError::UnsupportedFieldType {
                            schema: schema.name.clone(),
                            field: src_field.name.clone(),
                            ty: "duplicate field produced by spread".to_string(),
                            range,
                        }
                    ));
                }
            }
            continue;
        }
        let TokenKey::String(name, _, _) = key else {
            return Err(cap!(
                "lower_dict_into_record.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!("Dict(non-string-key in branded dict for `{}`)", schema.name),
                    range,
                }
            ));
        };
        // Schema must declare this field.
        if !schema.fields.iter().any(|f| &f.name == name) {
            return Err(cap!(
                "lower_dict_into_record.unsupported_field_type.1",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: name.clone(),
                    ty: format!("(unknown field, not declared on `{}`)", schema.name),
                    range,
                }
            ));
        }
        if user_values
            .insert(name.clone(), std::borrow::Cow::Borrowed(value))
            .is_some()
        {
            return Err(cap!(
                "lower_dict_into_record.duplicate_field",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: name.clone(),
                    ty: "duplicate field".to_string(),
                    range,
                }
            ));
        }
    }

    let user_set: std::collections::HashSet<&str> =
        user_values.keys().map(|s| s.as_str()).collect();
    let order = topo_order_fields(&schema.name, def, &user_set, range)?;

    for idx in order {
        let canonical_field = &schema.fields[idx];
        // `SchemaLayout::offsets_for` walks `schema.fields` in
        // declaration order, so `layout.fields[i].name ==
        // schema.fields[i].name` is invariant by construction.
        let layout_field = &layout.fields[idx];
        debug_assert_eq!(layout_field.name, canonical_field.name);
        let field_range = def.fields[idx].value_range;
        // Lower the value expression (user-supplied or schema default).
        if let Some(user_value) = user_values.get(canonical_field.name.as_str()) {
            // Wave R11: a field decorator on a branded `-> Schema` return
            // field is not yet desugared on the compiled path (only the
            // anon-Dict-return surface is). Cap loudly rather than lower
            // the raw value and silently drop the decorator transform —
            // that would diverge from the tree-walk oracle.
            if !user_value.decorators.is_empty() {
                return Err(cap!(
                    "lower_dict_into_record.unsupported_field_type.3",
                    LoweringError::UnsupportedFieldType {
                        schema: schema.name.clone(),
                        field: canonical_field.name.clone(),
                        ty: "field decorator on a branded `-> Schema` return field is not yet \
                             lowered (only anon-Dict-return field decorators desugar today)"
                            .to_string(),
                        range: user_value.range,
                    }
                ));
            }
            lower_dict_field_value(schema, idx, user_value.as_ref(), user_value.range, ctx)?;
        } else {
            // Schema default. Re-bind `#main` params; let-scope is
            // shared with the surrounding body (defaults sit at the
            // schema-instantiation site, not inside an isolated
            // scope, so referenced field names already resolved
            // through the topo-ordered store ops above are reachable
            // via `LetGet` over the per-field default-local — see
            // below for the sibling lookup mechanism).
            //
            // For Phase 3.b sibling field references are resolved
            // through the lowered value expression directly: the
            // default expression `a + 1` lowers to `LetGet { idx:
            // sibling_let_of_a }`. That trick requires us to keep a
            // per-record map from field name → let-local index when
            // a field's value is consumed by a later default. The
            // simpler shape: emit a `LetSet` for every default-
            // evaluated field so the wasm side caches the value and
            // a later default can read it back via `LetGet`.
            lower_dict_default(
                &schema.name,
                idx,
                &canonical_field.ty,
                def,
                ctx,
                field_range,
            )?;
        }
        // Stack now holds the field's value (with type matching the
        // canonical Field). Emit the StoreFieldAtRecord.
        let ir_ty = type_repr_to_ir_type_dict(&canonical_field.ty);
        let store_ty = match layout_field.kind {
            FieldKind::Inline { .. } => ir_ty,
            // Pointer-indirect fields all store as an i32 pointer.
            FieldKind::PointerIndirect { .. } => IrType::I32,
        };
        let top = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_dict_into_record.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Dict field `{}` of `{}` produced no value",
                        canonical_field.name, schema.name
                    ),
                    range,
                }
            )
        })?;
        if top.wasm_slot() != store_ty.wasm_slot() {
            return Err(cap!(
                "lower_dict_into_record.unsupported_field_type.2",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: canonical_field.name.clone(),
                    ty: format!("got {:?}, expected {:?}", top, store_ty),
                    range,
                }
            ));
        }
        ctx.out.push(TaggedOp {
            op: store_field_at_record_op(ctx, record_local, layout_field.offset as u32, store_ty),
            range: field_range,
        });

        // Cache the freshly-stored value into a let-local so later
        // sibling defaults can `LetGet` it. We only do this for
        // fields the schema's defaults actually reference — but
        // computing that subset requires a second pass. For the
        // Phase 3.b surface we cache *every* field, accepting the
        // unused-local overhead in exchange for simpler bookkeeping.
        // The wasm engine drops unused locals at JIT time.
        //
        // The cache mechanism: re-lower the value into a `LetSet` so
        // the value lives in a wasm local, then map the field name
        // to that let-idx. Because the value has already been
        // consumed by `StoreFieldAtRecord`, we re-emit a `LetGet`
        // that pulls the *stored slot* back through `LoadField`-like
        // semantics — but that's expensive. Simpler: stash the
        // value in a let *before* the StoreFieldAtRecord.
        //
        // Reorder: emit value → LetSet (cache) → LetGet (push back)
        // → StoreFieldAtRecord. The implementation does this by
        // splicing the LetSet/Get pair just before the store.
        //
        // We thread the cache via `ctx`'s let-binding stack so the
        // existing `Variable(name)` lookup resolves to the cached
        // value when a later default emits a reference.
        //
        // Performed below.
        let bound_ty = top;
        let let_idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        // Reach into the just-emitted op stream: splice
        // [LetSet, LetGet] right before the trailing
        // StoreFieldAtRecord. The current top-of-`out` is that
        // StoreFieldAtRecord (we pushed it just above) — pop, push
        // the cache pair, push it back. Cheaper than re-walking.
        let store_op = ctx.out.pop().expect("StoreFieldAtRecord just pushed");
        ctx.out.push(TaggedOp {
            op: Op::LetSet {
                idx: let_idx,
                ty: bound_ty,
            },
            range: field_range,
        });
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: let_idx,
                ty: bound_ty,
            },
            range: field_range,
        });
        ctx.out.push(store_op);
        ctx.lets.push(LetBinding {
            name: canonical_field.name.clone(),
            idx: let_idx,
            ty: bound_ty,
            schema_brand: None,
            type_repr: None,
        });
    }

    // Pop the field-name let bindings we pushed so the surrounding
    // scope sees its original let stack.
    let drop_count = schema.fields.len();
    let new_len = ctx.lets.len().saturating_sub(drop_count);
    ctx.lets.truncate(new_len);

    Ok(())
}

/// Lower a record literal whose schema is synthetic, such as a custom enum
/// payload. These records have no `SchemaDef`, so there are no defaults or
/// sibling-default references to resolve; every declared field must be present.
fn lower_plain_dict_into_record(
    schema: &Schema,
    layout: &OffsetTable,
    dict_pairs: &[(TokenKey, Node)],
    range: TokenRange,
    record_local: u32,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let mut user_values: HashMap<String, &Node> = HashMap::new();
    for (key, value) in dict_pairs {
        let TokenKey::String(name, _, _) = key else {
            return Err(cap!(
                "lower_plain_dict_into_record.unsupported_expr.1",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Dict(non-string-key in payload record for `{}`)",
                        schema.name
                    ),
                    range,
                }
            ));
        };
        if !schema.fields.iter().any(|f| &f.name == name) {
            return Err(cap!(
                "lower_plain_dict_into_record.unsupported_field_type.1",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: name.clone(),
                    ty: format!("(unknown field, not declared on `{}`)", schema.name),
                    range,
                }
            ));
        }
        if user_values.insert(name.clone(), value).is_some() {
            return Err(cap!(
                "lower_plain_dict_into_record.unsupported_field_type.2",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: name.clone(),
                    ty: "duplicate field".to_string(),
                    range,
                }
            ));
        }
    }

    for (idx, canonical_field) in schema.fields.iter().enumerate() {
        let layout_field = &layout.fields[idx];
        debug_assert_eq!(layout_field.name, canonical_field.name);
        let Some(user_value) = user_values.get(canonical_field.name.as_str()) else {
            return Err(cap!(
                "lower_plain_dict_into_record.missing_field",
                LoweringError::MissingFieldNoDefault {
                    schema: schema.name.clone(),
                    field: canonical_field.name.clone(),
                    range,
                }
            ));
        };
        if !user_value.decorators.is_empty() {
            return Err(cap!(
                "lower_plain_dict_into_record.unsupported_field_type.3",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: canonical_field.name.clone(),
                    ty: "field decorator on an enum payload field is not lowered".to_string(),
                    range: user_value.range,
                }
            ));
        }
        lower_dict_field_value(schema, idx, user_value, user_value.range, ctx)?;
        let ir_ty = type_repr_to_ir_type_dict(&canonical_field.ty);
        let store_ty = match layout_field.kind {
            FieldKind::Inline { .. } => ir_ty,
            FieldKind::PointerIndirect { .. } => IrType::I32,
        };
        let top = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_plain_dict_into_record.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Dict field `{}` of `{}` produced no value",
                        canonical_field.name, schema.name
                    ),
                    range,
                }
            )
        })?;
        if top.wasm_slot() != store_ty.wasm_slot() {
            return Err(cap!(
                "lower_plain_dict_into_record.unsupported_field_type.4",
                LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: canonical_field.name.clone(),
                    ty: format!("got {:?}, expected {:?}", top, store_ty),
                    range,
                }
            ));
        }
        ctx.out.push(TaggedOp {
            op: store_field_at_record_op(ctx, record_local, layout_field.offset as u32, store_ty),
            range: user_value.range,
        });
    }
    Ok(())
}

/// Lower the elements of a tuple literal into a positional record.
/// `tuple_schema` is the synthesised anonymous positional-record
/// schema (`is_tuple == true`); `layout` is its offset table; `elements`
/// are the source-level element expressions in declaration order (arity
/// already validated by the caller).
///
/// Each element is lowered like a branded-dict field of the matching
/// canonical type: scalars (`Int` / `Float` / `Bool`) land inline; a
/// `String` element is materialised to an absolute address then copied
/// into the tail area via `EmitTailRecordFromAbsoluteAddr`, leaving an
/// i32 buffer-relative pointer for the slot store — byte-identical to the
/// branded-record path, so the host object-return decode + verifier read
/// it back unchanged (only the final container shape forks to an array).
fn lower_tuple_into_record(
    tuple_schema: &Schema,
    layout: &OffsetTable,
    elements: &[Node],
    record_local: u32,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    for (idx, element) in elements.iter().enumerate() {
        let canonical_field = &tuple_schema.fields[idx];
        let layout_field = &layout.fields[idx];
        debug_assert_eq!(layout_field.name, canonical_field.name);

        lower_dict_field_value(tuple_schema, idx, element, element.range, ctx)?;

        let ir_ty = type_repr_to_ir_type_dict(&canonical_field.ty);
        let store_ty = match layout_field.kind {
            FieldKind::Inline { .. } => ir_ty,
            FieldKind::PointerIndirect { .. } => IrType::I32,
        };
        let top = ctx.tstack.pop().ok_or_else(|| {
            cap!(
                "lower_tuple_return.unsupported_expr",
                LoweringError::UnsupportedExpr {
                    kind: format!("tuple element {idx} produced no value"),
                    range: element.range,
                }
            )
        })?;
        if top.wasm_slot() != store_ty.wasm_slot() {
            return Err(cap!(
                "lower_tuple_return.unsupported_expr",
                LoweringError::UnsupportedExpr {
                    kind: format!("tuple element {idx}: got {top:?}, expected {store_ty:?}"),
                    range: element.range,
                }
            ));
        }
        ctx.out.push(TaggedOp {
            op: store_field_at_record_op(ctx, record_local, layout_field.offset as u32, store_ty),
            range: element.range,
        });
    }
    Ok(())
}

/// Lower one user-supplied dict-literal field value. Field `idx`
/// describes the schema-side canonical type; the value's source-side
/// expression decides which lowering arm to take.
fn lower_dict_field_value(
    schema: &Schema,
    field_idx: usize,
    value: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let canonical = &schema.fields[field_idx];
    match (&canonical.ty, &*value.expr) {
        (TypeRepr::Schema { schema: sub_schema }, Expr::Dict(pairs)) if !sub_schema.is_tuple => {
            // Nested branded dict. Allocate a sub-record, recurse,
            // then push the sub-record's base offset for the parent's
            // pointer slot.
            let sub_layout = SchemaLayout::offsets_for(sub_schema)?;
            let record_local = ctx.alloc_record_local();
            ctx.out.push(TaggedOp {
                op: alloc_record_op(
                    ctx,
                    record_local,
                    sub_layout.root_size as u32,
                    sub_layout.root_align as u32,
                ),
                range,
            });
            lower_dict_into_record(sub_schema, &sub_layout, pairs, range, record_local, ctx)?;
            // Store pointer slots use arena-absolute offsets. The record-local
            // itself is out-buffer-relative, so rebase it before the parent
            // field store consumes it.
            push_record_base_for_pointer(record_local, range, ctx);
            Ok(())
        }
        (TypeRepr::Schema { schema: sub_schema }, Expr::Tuple(elements)) if sub_schema.is_tuple => {
            if elements.len() != sub_schema.fields.len() {
                return Err(cap!(
                    "lower_tuple_field.arity_mismatch",
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "tuple field has {} elements but schema declares {}",
                            elements.len(),
                            sub_schema.fields.len()
                        ),
                        range,
                    }
                ));
            }
            let sub_layout = SchemaLayout::offsets_for(sub_schema)?;
            let record_local = ctx.alloc_record_local();
            ctx.out.push(TaggedOp {
                op: alloc_record_op(
                    ctx,
                    record_local,
                    sub_layout.root_size as u32,
                    sub_layout.root_align as u32,
                ),
                range,
            });
            lower_tuple_into_record(sub_schema, &sub_layout, elements, record_local, ctx)?;
            // Store pointer slots use arena-absolute offsets. The record-local
            // itself is out-buffer-relative, so rebase it before the parent
            // field store consumes it.
            push_record_base_for_pointer(record_local, range, ctx);
            Ok(())
        }
        (
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. },
            Expr::VariantCtor { .. },
        )
        | (
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. },
            Expr::Variable(_),
        )
        | (
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. },
            Expr::FnCall { .. },
        ) => lower_value_as_type(&canonical.ty, value, ctx),
        (TypeRepr::String, _) | (TypeRepr::List { .. }, _) => {
            // F3: cross-region branded-struct field. When the field is a
            // `List<…>` and its value is a bare `#main` parameter identity
            // whose data lives in the input region (the object head sits in
            // the output region), store the parameter list root's
            // arena-absolute offset directly into the field slot — exactly
            // like the anon-Dict `CrossRegionParamList` path (F1b/F2). The
            // value `lower_expr` pushes over a `Variable(param)` is the
            // `LoadList*Ptr` arena-absolute offset (F1 slot convention); we
            // store it verbatim with NO tail copy (the copy would lose the
            // cross-region link and, for pointer-array lists, mis-relocate
            // the in-buffer offsets). The host's object positive-path
            // verifier (`verify_object_return_multi`) classifies the offset
            // into the input region, bounds-checks the whole reachable
            // graph, and only then does the `BufferReader` field reader
            // follow it cross-region — bit-equal to the tree-walk oracle.
            if let TypeRepr::List { .. } = &canonical.ty {
                if branded_field_cross_region_param_list(&canonical.ty, value, ctx) {
                    lower_expr(&value.expr, range, ctx)?;
                    let popped = ctx.tstack.pop().ok_or(cap!(
                        "lower_dict_field_value.unsupported_expr.1",
                        LoweringError::UnsupportedExpr {
                            kind: "Dict(cross-region-field-value-stack-empty)".to_string(),
                            range,
                        }
                    ))?;
                    let expected_ir = type_repr_to_ir_type_dict(&canonical.ty);
                    if popped != expected_ir {
                        return Err(cap!(
                            "lower_dict_field_value.unsupported_field_type.1",
                            LoweringError::UnsupportedFieldType {
                                schema: schema.name.clone(),
                                field: canonical.name.clone(),
                                ty: format!(
                                    "cross-region field expected {expected_ir:?}, got {popped:?}"
                                ),
                                range,
                            }
                        ));
                    }
                    // The slot stores the arena-absolute offset directly.
                    // The caller's `StoreFieldAtRecord` writes an i32
                    // pointer-indirect slot; push the i32 offset for it.
                    ctx.tstack.push(IrType::I32);
                    return Ok(());
                }
            }
            if variant_list_literal_for_type(&canonical.ty, &value.expr) {
                lower_value_as_type(&canonical.ty, value, ctx)?;
                let popped = ctx.tstack.pop().ok_or(cap!(
                    "lower_dict_field_value.unsupported_expr.variant_list_stack_empty",
                    LoweringError::UnsupportedExpr {
                        kind: "Dict(variant-list-field-value-stack-empty)".to_string(),
                        range,
                    }
                ))?;
                if popped != IrType::ListList {
                    return Err(cap!(
                        "lower_dict_field_value.unsupported_field_type.variant_list_stack",
                        LoweringError::UnsupportedFieldType {
                            schema: schema.name.clone(),
                            field: canonical.name.clone(),
                            ty: format!("variant list field expected ListList, got {popped:?}"),
                            range,
                        }
                    ));
                }
                ctx.tstack.push(IrType::I32);
                return Ok(());
            }

            // Pointer-array list fields (`List<String>` / `List<Schema>`
            // / `List<List<_>>`) inside a branded-struct return are only
            // marshalled correctly from a const-pool `ConstListString`
            // block. A value sourced from a `#main` parameter / load /
            // call lives in the input buffer with non-contiguous,
            // whole-buffer-relative offsets the rigid-delta tail copy
            // (`EmitTailRecordFromAbsoluteAddr`) cannot relocate — it
            // would segfault / corrupt. Reject loudly before lowering so
            // the silent path is unreachable. (`List<Int/Float/Bool>` is
            // inline-fixed and copies correctly from any source, so the
            // pointer-*array* check excludes it.)
            if let TypeRepr::List { element } = &canonical.ty {
                let field_ir = match element.as_ref() {
                    TypeRepr::String => IrType::ListString,
                    TypeRepr::Schema { .. } => IrType::ListSchema,
                    TypeRepr::List { .. }
                    | TypeRepr::Option { .. }
                    | TypeRepr::Result { .. }
                    | TypeRepr::Enum { .. } => IrType::ListList,
                    _ => IrType::ListInt,
                };
                if pointer_array_list_ir_type(field_ir)
                    && !pointer_array_list_source_is_const_pool(&value.expr)
                {
                    return Err(cap!(
                        "lower_dict_field_value.unsupported_field_type.2",
                        LoweringError::UnsupportedFieldType {
                            schema: schema.name.clone(),
                            field: canonical.name.clone(),
                            ty: format!(
                                "{:?} sourced from `{}` — pointer-array list fields are only \
                             marshalled from in-source list literals, not parameters / loads / \
                             calls",
                                canonical.ty,
                                value.expr.kind()
                            ),
                            range,
                        }
                    ));
                }
            }
            // Recursively lower the value to produce an absolute
            // pointer (ConstString / ConstListInt / LoadStringPtr /
            // ...). Then copy the record into the parent's tail
            // area and push the buffer-relative offset.
            lower_expr(&value.expr, range, ctx)?;
            // Top of stack is an absolute address. Emit the tail-
            // record memcpy.
            let popped = ctx.tstack.pop().ok_or(cap!(
                "lower_dict_field_value.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: "Dict(field-value-stack-empty)".to_string(),
                    range,
                }
            ))?;
            // Cross-check the IR type against the declared field
            // type — saves a confusing codegen-time failure when the
            // dict field expects a String but the value lowered to
            // List<Int>.
            let expected_ir = match &canonical.ty {
                TypeRepr::String => IrType::String,
                TypeRepr::List { element } => match element.as_ref() {
                    TypeRepr::Int => IrType::ListInt,
                    TypeRepr::Float => IrType::ListFloat,
                    TypeRepr::Bool => IrType::ListBool,
                    TypeRepr::String => IrType::ListString,
                    TypeRepr::Schema { .. } => IrType::ListSchema,
                    TypeRepr::List { .. }
                    | TypeRepr::Option { .. }
                    | TypeRepr::Result { .. }
                    | TypeRepr::Enum { .. } => IrType::ListList,
                    _ => IrType::ListInt,
                },
                _ => unreachable!(),
            };
            if popped != expected_ir {
                return Err(cap!(
                    "lower_dict_field_value.unsupported_field_type.3",
                    LoweringError::UnsupportedFieldType {
                        schema: schema.name.clone(),
                        field: canonical.name.clone(),
                        ty: format!("expected {expected_ir:?}, got {popped:?}"),
                        range,
                    }
                ));
            }
            if !ctx.variant_records_in_scratch {
                ctx.out.push(TaggedOp {
                    op: Op::EmitTailRecordFromAbsoluteAddr { ty: expected_ir },
                    range,
                });
            }
            ctx.tstack.push(IrType::I32);
            Ok(())
        }
        // Scalar leaves: just lower the value. The
        // StoreFieldAtRecord ranges already align.
        _ => lower_expr(&value.expr, range, ctx),
    }
}

/// Lower a schema-default expression for field `field_idx`. The
/// default's body lives on the analyzer-side `SchemaFieldDef::value_node`;
/// we re-route the existing `lower_expr` recursion at that body so
/// references to sibling fields hit the just-pushed let-bindings
/// (we cache each evaluated field into a let-local in
/// [`lower_dict_into_record`]).
fn lower_dict_default(
    schema_name: &str,
    field_idx: usize,
    expected_ty: &TypeRepr,
    def: &SchemaDef,
    ctx: &mut LowerCtx<'_>,
    range: TokenRange,
) -> Result<(), LoweringError> {
    let field = &def.fields[field_idx];
    if field.is_wildcard {
        return Err(cap!(
            "lower_dict_default.missing_field_no_default",
            LoweringError::MissingFieldNoDefault {
                schema: schema_name.to_string(),
                field: field.name.clone(),
                range,
            }
        ));
    }
    // Lower the default expression with the surrounding lets in
    // scope. The let-stack already carries `<prior-field-name> →
    // value` bindings because the topological order placed
    // dependencies first.
    let value_node = &field.value_node;
    if matches!(
        expected_ty,
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. }
    ) {
        lower_value_as_type(expected_ty, value_node, ctx)?;
    } else {
        lower_expr(&value_node.expr, value_node.range, ctx)?;
    }
    Ok(())
}

/// R10: lower a *backward static* `&sibling.<name>` / `&root.<name>`
/// reference on the compiled path.
///
/// At the entry-level dict (the `#main -> Dict` anon-Dict-return body)
/// the entry dict IS the document root, so `&sibling.<name>` and
/// `&root.<name>` resolve to the very same field — both bases are
/// handled here. The runtime contract for a `&sibling`/`&root` whose
/// single trailing segment names an earlier field in the *same* dict is
/// identical to a bare let reference, so this reuses the source-ordered
/// field-let graph: each host-visible scalar field is registered as a
/// `LetBinding` (see [`lower_anon_dict_body`]) before later fields
/// lower, exactly as `lower_where` / the closure / dict / list-string
/// fields already do. We resolve `<name>` to that let-idx and emit the
/// same `LetGet` (or scalar-const inline) `lower_variable` would.
///
/// Everything outside that narrow shape is a loud cap, NOT a silent
/// fallback:
///
/// * Positional / runtime / grandparent bases — `&uncle` / `&prev` /
///   `&next` / `&index` / `&this` — need loop-carried or cross-dict
///   state the compiled entry body does not model, so they cap.
/// * A forward reference (the name is not yet bound — declared later in
///   source order) is not in `ctx.lets` and caps via the unresolved
///   path; the backward-only contract is what keeps the value
///   well-defined at lowering time.
/// * Dynamic-key segments and multi-segment paths (`&sibling.x.y`) cap;
///   only a single static `String` segment is lowered. `#internal`
///   sibling fields are dropped from the compiled plan entirely, so a
///   reference to one never resolves and caps — this also sidesteps the
///   `&sibling.<priv>`-allowed vs `&root.<priv>`-blocked privacy split,
///   since neither form can reach a private field here.
fn lower_reference(
    base: RefBase,
    path: &[TokenKey],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Only the entry-level-equivalent bases. Positional/runtime bases
    // are honestly out of the compiled path's reach.
    if !matches!(base, RefBase::Sibling | RefBase::Root) {
        return Err(cap!(
            "lower_reference.positional_base",
            LoweringError::UnsupportedExpr {
                kind: format!("Reference(positional base {base:?} not supported on compiled path)"),
                range,
            }
        ));
    }
    // Exactly one static String segment — no dynamic keys, no chaining.
    let name = match path {
        [TokenKey::String(name, _, _)] => name.as_str(),
        _ => {
            return Err(cap!(
                "lower_reference.unsupported_path_shape",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Reference(only a single static field segment is supported, got {path:?})"
                    ),
                    range,
                }
            ));
        }
    };
    // Backward-only: the named field must already be bound as a let
    // (declared earlier in source order). Inline a scalar-const let to
    // the literal exactly as `lower_variable` does so all backends fold
    // an identical compile-time value; otherwise emit the `LetGet`.
    if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == name).cloned() {
        if let Some(sc) = ctx.const_let_values.get(&b.idx).copied() {
            let (op, ty) = match sc {
                ScalarConst::I64(i) => (Op::ConstI64(i), IrType::I64),
                ScalarConst::F64(f) => (Op::ConstF64(OrderedFloat::from(f)), IrType::F64),
                ScalarConst::Bool(b) => (Op::ConstBool(b), IrType::Bool),
            };
            ctx.out.push(TaggedOp { op, range });
            ctx.tstack.push(ty);
            return Ok(());
        }
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: b.idx,
                ty: b.ty,
            },
            range,
        });
        ctx.tstack.push(b.ty);
        return Ok(());
    }
    Err(cap!(
        "lower_reference.unresolved_backward_field",
        LoweringError::UnresolvedVariable {
            name: name.to_string(),
            range,
        }
    ))
}

// NOTE (orphaned doc, stranded by a refactor — kept for reference):
// Lower a bare-identifier reference. Phase 3.a checks the user-let
// scope first (innermost shadow wins) and falls back to the `#main`
// parameter index. The let-binding hit emits an `Op::LetGet`; the
// param hit emits a typed `Op::LoadField` reading from the `in_buf`.
// Phase 5 extends the surface in two ways:
// * `self` (when the lowering context owns a `self_binding`) lowers
//   to the wasm-local that holds the schema instance's absolute address.
// * Multi-segment paths whose head resolves to a schema-typed binding
//   chase field offsets through the schema's layout chain, emitting
//   `Op::LoadFieldAtAbsolute` per segment.

fn enum_payload_field_name(segment: &TokenKey, variant: &CanonicalEnumVariant) -> Option<String> {
    match segment {
        TokenKey::String(name, _, optional) if !*optional => Some(name.clone()),
        TokenKey::Index(index, optional) if !*optional && variant.is_tuple => {
            Some(index.to_string())
        }
        _ => None,
    }
}

fn direct_payload_load_op(ty: IrType, offset: u32) -> Op {
    match ty {
        IrType::I64 => Op::LoadI64AtAbsolute { offset },
        IrType::F64 => Op::LoadF64AtAbsolute { offset },
        IrType::Bool => Op::LoadI8UAtAbsolute { offset },
        IrType::I32
        | IrType::Unit
        | IrType::String
        | IrType::ListInt
        | IrType::ListFloat
        | IrType::ListBool
        | IrType::ListString
        | IrType::ListSchema
        | IrType::ListList
        | IrType::Closure
        | IrType::Dict => Op::LoadI32AtAbsolute { offset },
    }
}

fn validate_enum_payload_base(
    ctx: &mut LowerCtx<'_>,
    range: TokenRange,
) -> Result<(), LoweringError> {
    let base_ty = ctx.tstack.pop().ok_or_else(|| {
        cap!(
            "lower_variable.unsupported_expr.enum_payload_stack",
            LoweringError::UnsupportedExpr {
                kind: "Enum(payload access without a variant pointer)".to_string(),
                range,
            }
        )
    })?;
    if base_ty != IrType::I32 {
        return Err(cap!(
            "lower_variable.unsupported_expr.enum_payload_stack_type",
            LoweringError::UnsupportedExpr {
                kind: format!("Enum(payload access expected I32 variant pointer, got {base_ty:?}"),
                range,
            }
        ));
    }
    Ok(())
}

fn lower_enum_payload_path(
    path_tail: &[TokenKey],
    narrowing: &EnumVariantNarrowing,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if path_tail.len() != 1 {
        return Err(cap!(
            "lower_variable.unsupported_expr.enum_payload_path",
            LoweringError::UnsupportedExpr {
                kind: "Enum(payload access with more than one segment)".to_string(),
                range,
            }
        ));
    }
    let Some(field_name) = enum_payload_field_name(&path_tail[0], &narrowing.variant) else {
        return Err(cap!(
            "lower_variable.unsupported_expr.enum_payload_segment",
            LoweringError::UnsupportedExpr {
                kind: "Enum(payload access expects a field name or tuple index)".to_string(),
                range,
            }
        ));
    };

    if let Some(payload) = &narrowing.direct_payload {
        if field_name != payload.field_name {
            return Err(cap!(
                "lower_variable.unsupported_expr.enum_payload_unknown_field",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Enum(variant `{}` has no payload field `{field_name}`)",
                        narrowing.variant.name
                    ),
                    range,
                }
            ));
        }
        let payload_slot_offset = variant_payload_offset_for_lowering(&payload.ty)? as u32;
        let field_ir = type_repr_to_ir_type_dict(&payload.ty);
        ctx.out.push(TaggedOp {
            op: direct_payload_load_op(field_ir, payload_slot_offset),
            range,
        });
        validate_enum_payload_base(ctx, range)?;
        ctx.tstack.push(field_ir);
        return Ok(());
    }

    let payload_schema = narrowing
        .variant
        .payload_schema(&narrowing.enum_name)
        .ok_or_else(|| {
            cap!(
                "lower_variable.unsupported_expr.enum_unit_payload_access",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Enum(unit variant `{}` has no payload field `{field_name}`)",
                        narrowing.variant.name
                    ),
                    range,
                }
            )
        })?;
    let field_meta = payload_schema
        .fields
        .iter()
        .find(|field| field.name == field_name)
        .ok_or_else(|| {
            cap!(
                "lower_variable.unsupported_expr.enum_payload_unknown_field",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Enum(variant `{}` has no payload field `{field_name}`)",
                        narrowing.variant.name
                    ),
                    range,
                }
            )
        })?;
    let payload_ty = TypeRepr::Schema {
        schema: Box::new(payload_schema.clone()),
    };
    let payload_slot_offset = variant_payload_offset_for_lowering(&payload_ty)? as u32;
    let layout = SchemaLayout::offsets_for(&payload_schema)?;
    let field_slot = layout
        .fields
        .iter()
        .find(|slot| slot.name == field_meta.name)
        .ok_or_else(|| {
            cap!(
                "lower_variable.unsupported_expr.enum_payload_missing_layout",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Enum(variant `{}` payload field `{}` has no layout slot)",
                        narrowing.variant.name, field_meta.name
                    ),
                    range,
                }
            )
        })?;
    let field_ir = type_repr_to_ir_type_dict(&field_meta.ty);
    ctx.out.push(TaggedOp {
        op: Op::LoadI32AtAbsolute {
            offset: payload_slot_offset,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LoadFieldAtAbsolute {
            offset: field_slot.offset as u32,
            ty: field_ir,
        },
        range,
    });
    validate_enum_payload_base(ctx, range)?;
    ctx.tstack.push(field_ir);
    Ok(())
}

fn lower_variable(
    path: &[TokenKey],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if path.is_empty() {
        return Err(cap!(
            "lower_variable.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: "Variable(empty-path)".to_string(),
                range,
            }
        ));
    }
    let head = match &path[0] {
        TokenKey::String(s, _, _) => s.as_str(),
        TokenKey::Index(_, _) | TokenKey::Dummy | TokenKey::Spread(_) | TokenKey::Dynamic(_, _) => {
            return Err(cap!(
                "lower_variable.unsupported_expr.2",
                LoweringError::UnsupportedExpr {
                    kind: "Variable(non-string-key)".to_string(),
                    range,
                }
            ));
        }
    };
    let enum_narrowing = ctx.enum_variant_narrowing.get(head).cloned();
    // #359 (W20 container perf): a bare reference to a where-bound
    // SCALAR CONSTANT let (`soft` / `dt` / a mass) lowers to the literal
    // `Op::Const*` directly instead of a `LetGet` (an alloca load
    // pre-mem2reg, or — when captured by a closure — an opaque load from
    // the arena captures struct). Folding it to a compile-time constant
    // lets LLVM's `-O3` value-range / arithmetic simplification see the
    // real value (`dx*dx + 0.1`) instead of an opaque load, recovering
    // the scalar half of the W20 inner-loop overhead (2.14x -> ~1.69x on
    // s90). Restricted to single-segment paths (`path.len() == 1` — no
    // field/index chaining) and to lets recorded in `const_let_values`;
    // the inlined literal is the exact source value, so all backends
    // compute a bit-identical result. Scalar-let shadowing is respected
    // because `const_let_values` is keyed by the same let-idx the
    // innermost binding resolves to.
    if path.len() == 1 {
        if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head) {
            if let Some(sc) = ctx.const_let_values.get(&b.idx).copied() {
                let (op, ty) = match sc {
                    ScalarConst::I64(i) => (Op::ConstI64(i), IrType::I64),
                    ScalarConst::F64(f) => (Op::ConstF64(OrderedFloat::from(f)), IrType::F64),
                    ScalarConst::Bool(b) => (Op::ConstBool(b), IrType::Bool),
                };
                ctx.out.push(TaggedOp { op, range });
                ctx.tstack.push(ty);
                return Ok(());
            }
        }
    }
    // The walker pushes a value onto the vstack representing the
    // root binding's IR type plus, for schema-typed roots, the
    // canonical schema shape and brand so chained field offsets can
    // be resolved deeper down the path.
    let mut current_schema: Option<Schema>;
    if let Some(self_b) = ctx.self_binding.clone() {
        if head == "self" {
            ctx.out.push(TaggedOp {
                op: Op::LocalGet(self_b.wasm_local_idx),
                range,
            });
            ctx.tstack.push(IrType::I32);
            current_schema = Some(self_b.schema.clone());
        } else if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head).cloned() {
            ctx.out.push(TaggedOp {
                op: Op::LetGet {
                    idx: b.idx,
                    ty: b.ty,
                },
                range,
            });
            ctx.tstack.push(b.ty);
            current_schema = b
                .schema_brand
                .as_deref()
                .and_then(|n| ctx.schema_resolver.resolve(n))
                .and_then(|def| {
                    let mut stack: Vec<&str> = Vec::new();
                    canonical_schema_from_def(def, &ctx.schema_resolver, &mut stack, range).ok()
                });
        } else if let Some(p) = ctx.method_params.iter().find(|p| p.name == head).cloned() {
            ctx.out.push(TaggedOp {
                op: Op::LocalGet(p.wasm_local_idx),
                range,
            });
            ctx.tstack.push(p.ty);
            current_schema = p.schema;
        } else {
            return Err(cap!(
                "lower_variable.unresolved_variable.1",
                LoweringError::UnresolvedVariable {
                    name: head.to_string(),
                    range,
                }
            ));
        }
    } else if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == head).cloned() {
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: b.idx,
                ty: b.ty,
            },
            range,
        });
        ctx.tstack.push(b.ty);
        current_schema = b
            .schema_brand
            .as_deref()
            .and_then(|n| ctx.schema_resolver.resolve(n))
            .and_then(|def| {
                let mut stack: Vec<&str> = Vec::new();
                canonical_schema_from_def(def, &ctx.schema_resolver, &mut stack, range).ok()
            });
    } else {
        let binding = ctx
            .params
            .iter()
            .find(|b| b.name == head)
            .cloned()
            .ok_or_else(|| {
                cap!(
                    "lower_variable.unresolved_variable.2",
                    LoweringError::UnresolvedVariable {
                        name: head.to_string(),
                        range,
                    }
                )
            })?;
        // Pointer-indirect leaves (`String` / `ListInt`) get their own
        // op tag so a later phase can hang String / List operations
        // off them without re-deriving the type from the slot. Schema
        // params lift the buffer-relative pointer to an absolute
        // address via `LoadSchemaPtr`.
        let op = match (binding.ty, binding.schema_brand.as_deref()) {
            (IrType::I32, Some(_)) => Op::LoadSchemaPtr {
                offset: binding.offset,
            },
            (IrType::String, _) => Op::LoadStringPtr {
                offset: binding.offset,
            },
            (IrType::ListInt, _) => Op::LoadListIntPtr {
                offset: binding.offset,
            },
            (IrType::ListFloat, _) => Op::LoadListFloatPtr {
                offset: binding.offset,
            },
            (IrType::ListBool, _) => Op::LoadListBoolPtr {
                offset: binding.offset,
            },
            (IrType::ListString, _) => Op::LoadListStringPtr {
                offset: binding.offset,
            },
            (IrType::ListSchema, _) => Op::LoadListSchemaPtr {
                offset: binding.offset,
            },
            (IrType::ListList, _) => Op::LoadListListPtr {
                offset: binding.offset,
            },
            _ => Op::LoadField {
                offset: binding.offset,
                ty: binding.ty,
            },
        };
        ctx.out.push(TaggedOp { op, range });
        ctx.tstack.push(binding.ty);
        current_schema = binding.schema.clone();
    }

    // Walk any remaining segments against the schema layout chain.
    // Each segment pops the i32 absolute address, computes the
    // field's offset + IR type from the canonical schema, and emits a
    // matching `LoadFieldAtAbsolute`. The pushed type adopts the
    // field's IR shape; schema-typed fields preserve the brand for
    // further chained access.
    if path.len() == 1 {
        return Ok(());
    }
    if let Some(narrowing) = enum_narrowing.as_ref() {
        return lower_enum_payload_path(&path[1..], narrowing, range, ctx);
    }
    // AOT-4 (W16 slice): 1D `xs[i]` index on a materialised `List<Int>`
    // receiver. The parser lowers the bracket form to a single trailing
    // `TokenKey::Dynamic(<index Node>)` segment after the root name (a
    // dotted `xs.0` would arrive as `TokenKey::Index`, which we do NOT
    // accept here — the materialised-list index path is bracket-only).
    // The head pushed an `IrType::ListInt` arena handle (i32); the index
    // is read with inline payload addressing that mirrors the record
    // layout the bundled `list_int_*` bodies write
    // (`stdlib::defs::list_int_filter_body`): `[len: u32 LE][pad: u32]
    // [i64 elements...]`, payload at `(base + 4 + 7) & -8`, element `i`
    // at `payload + i*8`. The load is emitted WITHOUT a bounds branch —
    // every caller in scope (the W16 quicksort kernel) guards
    // `_len(xs) <= 1` before reaching `xs[0]`, so the index is provably
    // in-bounds on the hot path. A shape we cannot prove in-bounds is
    // DECLINED (falls through to the generic non-string-segment
    // diagnostic) rather than emitting a possibly-wrong load. We do NOT
    // emit `Op::ListGetByIntIdx` (that op is trace-recorder-only; static
    // codegen rejects it).
    // AOT-4 (W19 slice): generalise to a CHAIN of trailing `Dynamic`
    // index segments so 2D `a[i][k]` (and any N-D `xs[i][j]...`) on a
    // materialised `List<List<Int>>` lowers. A `List<List<Int>>` is the
    // outer `List<Int>` record whose i64 elements are i32 arena offsets
    // of inner `List<Int>` rows (the materialiser writes the handle
    // truncated into the i64 element slot — see
    // `emit_list_value_materialize`). An outer index `a[i]` therefore
    // loads an i64 whose low 32 bits ARE the inner row's arena handle;
    // to index it again the i64 is round-tripped through a ListInt
    // let-slot (`LetSet{ListInt}` truncates i64->i32) so the next
    // `lower_list_int_index` sees a properly tagged `ListInt` receiver.
    // The FINAL segment loads the i64 cell value. Inline payload
    // addressing throughout (NO `Op::ListGetByIntIdx`, NO bounds branch
    // — every W19 index is provably within `range(size)`).
    if path.len() >= 2
        && path[1..]
            .iter()
            .all(|s| matches!(s, TokenKey::Dynamic(_, _)))
    {
        let receiver_ty = ctx.tstack.last().copied();
        if receiver_ty == Some(IrType::ListInt) {
            let last = path.len() - 1;
            for (off, seg) in path[1..].iter().enumerate() {
                let TokenKey::Dynamic(index_node, optional) = seg else {
                    unreachable!("guarded by the all-Dynamic check above");
                };
                // Optional indexing (`xs[i]?`) needs an Option.None-or-value
                // result the i64 element path can't represent; decline.
                if *optional {
                    return Err(cap!(
                        "lower_variable.unsupported_expr.3",
                        LoweringError::UnsupportedExpr {
                            kind: "Variable(optional-list-index unsupported)".to_string(),
                            range,
                        }
                    ));
                }
                // Pops the `ListInt` receiver, pushes the i64 element.
                lower_list_int_index(index_node, range, ctx)?;
                // Not the last segment: the loaded i64 is an inner row
                // handle — retag it as `ListInt` for the next index step.
                if 1 + off != last {
                    let handle_i = ctx.next_let_idx;
                    ctx.next_let_idx += 1;
                    ctx.out.push(TaggedOp {
                        op: Op::LetSet {
                            idx: handle_i,
                            ty: IrType::ListInt,
                        },
                        range,
                    });
                    ctx.tstack.pop(); // i64 element
                    ctx.out.push(TaggedOp {
                        op: Op::LetGet {
                            idx: handle_i,
                            ty: IrType::ListInt,
                        },
                        range,
                    });
                    ctx.tstack.push(IrType::ListInt);
                }
            }
            return Ok(());
        }
        // #359 (W20): 1D `s[k]` index on a `List<Float>` receiver — the
        // n-body state list (`init` / `final_state` / the reducer's `s`
        // param). The record layout is identical to `List<Int>` (8-byte
        // elements); only the element load is `f64` and the result rides
        // as `F64`. A `List<Float>`-of-`List<Float>` does not occur in
        // W20, so only the single trailing-index form is accepted here.
        if receiver_ty == Some(IrType::ListFloat) && path.len() == 2 {
            let TokenKey::Dynamic(index_node, optional) = &path[1] else {
                unreachable!("guarded by the all-Dynamic check above");
            };
            if *optional {
                return Err(cap!(
                    "lower_variable.unsupported_expr.4",
                    LoweringError::UnsupportedExpr {
                        kind: "Variable(optional-list-index unsupported)".to_string(),
                        range,
                    }
                ));
            }
            // Pops the `ListFloat` receiver, pushes the f64 element.
            lower_list_index_typed(index_node, IrType::ListFloat, range, ctx)?;
            return Ok(());
        }
        // W5-P2: 1D `keys[i]` index on a `List<String>` receiver — the
        // dict-probe `keys[i % 10]` workload, plus the standalone
        // `["a", .., "j"][i]` form. A `List<String>` record is a
        // *pointer array*: `[len: u32][off_0: u32]...[off_{N-1}: u32]`
        // header whose `off_i` is the arena-relative byte offset of the
        // i-th String record (`[slen: u32][utf8]`). Indexing it loads
        // the `u32` slot — which IS a `String` handle (the same i32
        // arena offset `ConstString` pushes) — so the result rides on
        // the vstack tagged `String` and any downstream consumer (the
        // String-return tail-record copy) sees a normal String value.
        // Only the single trailing-index form is accepted (no
        // `List<List<String>>` in scope).
        if receiver_ty == Some(IrType::ListString) && path.len() == 2 {
            let TokenKey::Dynamic(index_node, optional) = &path[1] else {
                unreachable!("guarded by the all-Dynamic check above");
            };
            if *optional {
                return Err(cap!(
                    "lower_variable.unsupported_expr.5",
                    LoweringError::UnsupportedExpr {
                        kind: "Variable(optional-list-index unsupported)".to_string(),
                        range,
                    }
                ));
            }
            // Pops the `ListString` receiver, pushes the String handle.
            lower_list_string_index(index_node, range, ctx)?;
            return Ok(());
        }
        // W5-P3: 1D `d[k]` index on a materialised `{String -> Int}`
        // dict receiver — the dict-probe workload. `d` is an
        // `IrType::Dict` arena handle (pushed by `Op::ConstDict` /
        // `LetGet{Dict}`); the bracket index `k` lowers to a runtime
        // `String` handle (a `keys[i]` element or a `ConstString`). The
        // probe is a fully IR-lowered linear scan + byte compare over
        // the arena entry table, so native + wasm32 need no new runtime
        // import. Only the single trailing-index form is accepted (no
        // nested dict-of-dict in scope).
        if receiver_ty == Some(IrType::Dict) && path.len() == 2 {
            let TokenKey::Dynamic(index_node, optional) = &path[1] else {
                unreachable!("guarded by the all-Dynamic check above");
            };
            if *optional {
                return Err(cap!(
                    "lower_variable.unsupported_expr.6",
                    LoweringError::UnsupportedExpr {
                        kind: "Variable(optional-dict-index unsupported)".to_string(),
                        range,
                    }
                ));
            }
            // Pops the `Dict` receiver, pushes the i64 value (Int).
            lower_dict_string_index(index_node, range, ctx)?;
            return Ok(());
        }
        // A `Dynamic` segment on a non-list receiver is not a
        // materialised-list index — fall through to the generic
        // diagnostic below so the rejection message stays precise.
    }
    for seg in &path[1..] {
        let Some(schema) = current_schema.clone() else {
            return Err(cap!(
                "lower_variable.unsupported_expr.8",
                LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Variable(field-on-non-schema-base, segment=`{}`)",
                        token_key_display(seg)
                    ),
                    range,
                }
            ));
        };
        let field_name: std::borrow::Cow<'_, str> = match seg {
            TokenKey::String(s, _, _) => std::borrow::Cow::Borrowed(s.as_str()),
            TokenKey::Index(i, optional) if schema.is_tuple && !*optional => {
                std::borrow::Cow::Owned(i.to_string())
            }
            TokenKey::Index(_, true) if schema.is_tuple => {
                return Err(cap!(
                    "lower_variable.unsupported_expr.7",
                    LoweringError::UnsupportedExpr {
                        kind: "Variable(optional-tuple-index unsupported)".to_string(),
                        range,
                    }
                ));
            }
            _ => {
                return Err(cap!(
                    "lower_variable.unsupported_expr.7",
                    LoweringError::UnsupportedExpr {
                        kind: "Variable(non-string-segment)".to_string(),
                        range,
                    }
                ));
            }
        };
        // Recompute the layout for the current schema shape. Cached
        // canonical schemas are reused across calls so the resolver
        // doesn't repeatedly re-walk the analyzer tree.
        let layout = SchemaLayout::offsets_for(&schema)?;
        let field_idx = schema
            .fields
            .iter()
            .position(|f| f.name == field_name.as_ref())
            .ok_or_else(|| {
                cap!(
                    "lower_variable.unsupported_field_type",
                    LoweringError::UnsupportedFieldType {
                        schema: schema.name.clone(),
                        field: field_name.to_string(),
                        ty: "(unknown field)".to_string(),
                        range,
                    }
                )
            })?;
        let field_meta = &schema.fields[field_idx];
        let layout_field = &layout.fields[field_idx];
        // Pop the base address.
        let popped = ctx.tstack.pop().ok_or(cap!(
            "lower_variable.unsupported_expr.9",
            LoweringError::UnsupportedExpr {
                kind: "Variable(field-load-stack-empty)".to_string(),
                range,
            }
        ))?;
        if popped.wasm_slot() != IrType::I32 {
            return Err(cap!(
                "lower_variable.unsupported_expr.10",
                LoweringError::UnsupportedExpr {
                    kind: format!("Variable(field-base-not-i32, got={:?})", popped),
                    range,
                }
            ));
        }
        let field_ir = type_repr_to_ir_type_dict(&field_meta.ty);
        ctx.out.push(TaggedOp {
            op: Op::LoadFieldAtAbsolute {
                offset: layout_field.offset as u32,
                ty: field_ir,
            },
            range,
        });
        ctx.tstack.push(field_ir);
        // Update walking state for the next segment.
        current_schema = match &field_meta.ty {
            TypeRepr::Schema { schema } => Some((**schema).clone()),
            _ => None,
        };
    }
    Ok(())
}

// =====================================================================
// Phase 5: schema method lowering.
// =====================================================================

/// One enumerated user-declared schema method, paired with the
/// canonical shape of its owning schema. Built by [`enumerate_methods`]
/// before any body lowering so each method's wasm-level function
/// index is decided up front — that's the prerequisite for inter-
/// method calls (`self.other_method()`) and for `obj.method()` calls
/// from the entry body, both of which resolve through
/// [`SchemaMethodRegistry`].
#[derive(Debug, Clone)]
struct EnumeratedMethod {
    /// Owning schema name (key into the registry).
    schema_name: String,
    /// Canonical shape of the owning schema — supplied to the
    /// `SelfBinding` so method-body `self.field` walks reuse it.
    schema_shape: Schema,
    /// Analyzer-side metadata for the method (param types, body
    /// node, return type).
    info: SchemaMethodInfo,
    /// IR-level index this method occupies in `Module::funcs`.
    ir_idx: usize,
}

/// Walk every schema with a non-empty methods list, snapshot the
/// methods in source order, and assign IR-side indices. Methods with
/// `is_native` bodies are skipped — Phase 5 does not yet implement
/// the host-import path; the analyzer would have already accepted
/// `#native` methods as opaque references.
fn enumerate_methods<'a>(
    tree: &'a AnalyzedTree,
    resolver: &SchemaResolver<'a>,
) -> Result<Vec<EnumeratedMethod>, LoweringError> {
    let mut out: Vec<EnumeratedMethod> = Vec::new();
    // Stable iteration order: schemas appear sorted by name. Without
    // sorting, the HashMap's iteration order would shift the wasm
    // function indices across compiles, breaking `relon.srcmap`
    // determinism the harness relies on.
    let mut schema_names: Vec<&String> = tree.schema_methods.keys().collect();
    schema_names.sort();
    for name in schema_names {
        let methods = match tree.schema_methods.get(name) {
            Some(m) if !m.is_empty() => m,
            _ => continue,
        };
        // Resolve the schema definition into a canonical shape so the
        // method body can walk `self.field` against a stable
        // `Schema` value. Schemas not in the resolver (e.g. native
        // carriers, anonymous dict schemas) get skipped — they don't
        // contribute method bodies the IR can emit.
        let Some(def) = resolver.resolve(name.as_str()) else {
            continue;
        };
        let mut stack: Vec<&str> = Vec::new();
        let schema_shape = canonical_schema_from_def(def, resolver, &mut stack, def.range)?;
        for info in methods {
            if info.is_native || info.body_node.is_none() {
                continue;
            }
            let ir_idx = out.len();
            out.push(EnumeratedMethod {
                schema_name: name.clone(),
                schema_shape: schema_shape.clone(),
                info: info.clone(),
                ir_idx,
            });
        }
    }
    Ok(out)
}

/// Lower every enumerated schema method into an IR `Func` and build
/// the dispatch registry mapping `(schema_name, method_name)` to its
/// combined wasm-level function index plus signature. Called once per
/// entry-module lowering, before the entry body walk consumes the
/// registry.
fn lower_schema_methods<'a>(
    tree: &'a AnalyzedTree,
    resolver: &SchemaResolver<'a>,
    const_intern: Rc<RefCell<ConstInternTables>>,
    native_imports: Rc<RefCell<NativeImportBuilder>>,
) -> Result<(Vec<Func>, SchemaMethodRegistry), LoweringError> {
    let enumerated = enumerate_methods(tree, resolver)?;
    let stdlib_offset = stdlib_function_count();
    let mut registry = SchemaMethodRegistry::default();
    // First pass: populate the registry so a method body lowered in
    // the second pass can self-dispatch to a sibling method whose
    // body hasn't been emitted yet (`bar()` from inside `foo()`).
    let mut method_sigs: Vec<MethodSig> = Vec::new();
    for m in &enumerated {
        let sig = method_signature_ir_types(&m.info, resolver)?;
        let wasm_idx = stdlib_offset + m.ir_idx as u32;
        let key = (m.schema_name.clone(), m.info.name.clone());
        registry
            .methods
            .insert(key, (wasm_idx, sig.param_tys.clone(), sig.ret_ty));
        method_sigs.push(sig);
    }
    // Second pass: lower each method's body now that the registry is
    // fully populated. #151 — each method ctx receives a clone of the
    // shared intern handle so its `Op::ConstString` / `Op::ConstList*`
    // ops mint idxs out of the same module-wide allocator as the
    // entry body.
    let mut funcs: Vec<Func> = Vec::with_capacity(enumerated.len());
    for (m, sig) in enumerated.iter().zip(method_sigs) {
        let func = lower_one_method(
            m,
            &sig,
            resolver,
            &registry,
            Rc::clone(&const_intern),
            Rc::clone(&native_imports),
        )?;
        funcs.push(func);
    }
    Ok((funcs, registry))
}

/// Resolved IR-side signature for one schema method. Built once per
/// method during the first pass through [`lower_schema_methods`] and
/// re-used when emitting the body. `param_schemas[i]` is `Some(_)`
/// when the i-th param (including the leading `self` slot) is schema-
/// typed and carries the canonical schema shape so chained-segment
/// reads inside the method body resolve their layouts statically.
#[derive(Debug, Clone)]
struct MethodSig {
    param_tys: Vec<IrType>,
    ret_ty: IrType,
    param_schemas: Vec<Option<Schema>>,
}

/// Translate a `SchemaMethodInfo`'s declared param + return types to
/// IR-side types plus, for schema-typed params, their canonical shape
/// (needed so method-body walks can resolve chained field access on
/// those params). Phase 5 restricts the return surface to scalar /
/// `Bool` / `Unit` types — variable-length return values (`String` /
/// `List<Int>` / nested dict) require a tail-cursor protocol the
/// non-entry wasm signature doesn't carry yet.
fn method_signature_ir_types(
    info: &SchemaMethodInfo,
    resolver: &SchemaResolver<'_>,
) -> Result<MethodSig, LoweringError> {
    // The receiver `self` is implicit at the source level; the IR
    // function carries it as an explicit i32 parameter at slot 0.
    let mut param_tys: Vec<IrType> = vec![IrType::I32];
    let mut param_schemas: Vec<Option<Schema>> = vec![None];
    for p in &info.params {
        let repr =
            type_node_to_canonical_with_schemas(&p.type_node, resolver).ok_or_else(|| {
                cap!(
                    "method_signature_ir_types.unsupported_type_in_main.1",
                    LoweringError::UnsupportedTypeInMain {
                        type_name: type_head_for_display(&p.type_node),
                        range: p.type_node.range,
                    }
                )
            })?;
        match repr {
            TypeRepr::Schema { schema } => {
                param_tys.push(IrType::I32);
                param_schemas.push(Some(*schema));
            }
            other => {
                param_tys.push(type_repr_to_ir_type(&other)?);
                param_schemas.push(None);
            }
        }
    }
    let ret_repr =
        type_node_to_canonical_with_schemas(&info.return_type, resolver).ok_or_else(|| {
            cap!(
                "method_signature_ir_types.unsupported_type_in_main.2",
                LoweringError::UnsupportedTypeInMain {
                    type_name: type_head_for_display(&info.return_type),
                    range: info.return_type.range,
                }
            )
        })?;
    // Phase 5 scope: only scalar / `Bool` / `Unit` returns ride the
    // wasm function's single-value return slot. Variable-length
    // returns are deferred — they need a tail-cursor handshake the
    // non-entry signature doesn't carry yet.
    let ret_ty = match ret_repr {
        TypeRepr::Int => IrType::I64,
        TypeRepr::Float => IrType::F64,
        TypeRepr::Bool => IrType::Bool,
        TypeRepr::Unit => IrType::Unit,
        _ => {
            return Err(cap!(
                "method_signature_ir_types.unsupported_type_in_main.3",
                LoweringError::UnsupportedTypeInMain {
                    type_name: type_head_for_display(&info.return_type),
                    range: info.return_type.range,
                }
            ));
        }
    };
    Ok(MethodSig {
        param_tys,
        ret_ty,
        param_schemas,
    })
}

/// Lower one schema method body into a `Func`. Self lives at wasm
/// local `0`; declared parameters fill locals `1..=N`. The body must
/// leave exactly one value of the declared return type on the
/// operand stack — the trailing `Op::Return` marker handles wasm
/// emission.
fn lower_one_method<'a>(
    m: &EnumeratedMethod,
    sig: &MethodSig,
    resolver: &SchemaResolver<'a>,
    registry: &SchemaMethodRegistry,
    const_intern: Rc<RefCell<ConstInternTables>>,
    native_imports: Rc<RefCell<NativeImportBuilder>>,
) -> Result<Func, LoweringError> {
    let MethodSig {
        param_tys,
        ret_ty,
        param_schemas,
    } = sig;
    let ret_ty = *ret_ty;
    let body_node = m.info.body_node.as_ref().ok_or_else(|| {
        cap!(
            "lower_one_method.unsupported_expr.1",
            LoweringError::UnsupportedExpr {
                kind: format!("SchemaMethod(no-body for `{}`)", m.info.name),
                range: m.info.range,
            }
        )
    })?;
    // Build the per-param metadata, skipping the leading `self` slot
    // since the method ctx tracks it separately via `SelfBinding`.
    let mut method_params: Vec<MethodParam> = Vec::with_capacity(m.info.params.len());
    for (i, p) in m.info.params.iter().enumerate() {
        let wasm_local_idx = (i + 1) as u32;
        // `param_tys[0]` is `self`; the user-declared params start at
        // index 1.
        let ty = param_tys[i + 1];
        let schema = param_schemas.get(i + 1).cloned().unwrap_or(None);
        method_params.push(MethodParam {
            name: p.name.clone(),
            ty,
            wasm_local_idx,
            schema,
        });
    }
    let self_binding = SelfBinding {
        wasm_local_idx: 0,
        schema: m.schema_shape.clone(),
    };
    // `params: &[]` — the method body has no `#main` param surface;
    // every reference flows through `self_binding` / `method_params`
    // / `lets`.
    const EMPTY_PARAMS: &[LocalBinding] = &[];
    let mut ctx = LowerCtx::new_method(
        EMPTY_PARAMS,
        resolver.clone(),
        registry.clone(),
        self_binding,
        method_params,
        const_intern,
        native_imports,
    );
    lower_expr(&body_node.expr, body_node.range, &mut ctx)?;
    // Validate the body left exactly one value of the declared
    // return type on the virtual stack.
    let top = ctx.tstack.last().copied().ok_or_else(|| {
        cap!(
            "lower_one_method.unsupported_expr.2",
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "SchemaMethod(`{}::{}`) body produced no value",
                    m.schema_name, m.info.name
                ),
                range: body_node.range,
            }
        )
    })?;
    if top.wasm_slot() != ret_ty.wasm_slot() {
        return Err(cap!(
            "lower_one_method.unsupported_type_in_main",
            LoweringError::UnsupportedTypeInMain {
                type_name: format!(
                    "method `{}::{}` returns `{:?}` but body produced `{:?}`",
                    m.schema_name, m.info.name, ret_ty, top
                ),
                range: body_node.range,
            }
        ));
    }
    ctx.out.push(TaggedOp {
        op: Op::Return,
        range: body_node.range,
    });
    Ok(Func {
        name: format!("__method_{}__{}", m.schema_name, m.info.name),
        params: param_tys.to_vec(),
        ret: ret_ty,
        body: ctx.out,
        range: m.info.range,
    })
}

// =====================================================================
// #151 — Compile-time intern invariants.
//
// End-to-end checks that drive the analyzer + lowering pipeline so the
// invariants exercise the same code path real callers hit (rather
// than synthesising a `LowerCtx` directly, which would bypass the
// schema-method composition step where the latent idx-collision bug
// lived).
// =====================================================================

#[cfg(test)]
mod intern_tests {
    use super::*;

    fn type_node(name: &str, generics: Vec<TypeNode>) -> TypeNode {
        TypeNode {
            path: vec![name.to_string()],
            generics,
            is_optional: false,
            range: TokenRange::default(),
            variant_fields: None,
            doc_comment: None,
        }
    }

    #[test]
    fn tuple_type_canonicalizer_accepts_normal_nested_types() {
        let ty = type_node(
            "Tuple",
            vec![
                type_node("Int", vec![]),
                type_node(
                    "Tuple",
                    vec![type_node("String", vec![]), type_node("Bool", vec![])],
                ),
                type_node("List", vec![type_node("Int", vec![])]),
                type_node("Option", vec![type_node("String", vec![])]),
                type_node(
                    "Result",
                    vec![type_node("Int", vec![]), type_node("String", vec![])],
                ),
            ],
        );

        let TypeRepr::Schema { schema } = type_node_to_canonical(&ty).expect("tuple canonical")
        else {
            panic!("Tuple<...> should canonicalize as a tuple schema");
        };
        assert!(schema.is_tuple);
        assert_eq!(schema.fields.len(), 5);
        assert!(matches!(schema.fields[0].ty, TypeRepr::Int));
        assert!(matches!(&schema.fields[1].ty, TypeRepr::Schema { schema } if schema.is_tuple));
        assert!(
            matches!(&schema.fields[2].ty, TypeRepr::List { element } if matches!(element.as_ref(), TypeRepr::Int))
        );
        assert!(
            matches!(&schema.fields[3].ty, TypeRepr::Option { inner } if matches!(inner.as_ref(), TypeRepr::String))
        );
        assert!(
            matches!(&schema.fields[4].ty, TypeRepr::Result { ok, err } if matches!(ok.as_ref(), TypeRepr::Int) && matches!(err.as_ref(), TypeRepr::String))
        );
    }

    #[test]
    fn null_and_unit_type_names_do_not_canonicalize() {
        assert!(type_node_to_canonical(&type_node("Null", vec![])).is_none());
        assert!(type_node_to_canonical(&type_node("Unit", vec![])).is_none());
    }

    #[test]
    fn tuple_return_lowers_nested_tuple_and_list_elements() {
        let src = r#"
            #main(Int n) -> Tuple<Tuple<Int, String>, List<Int>, String>
            ((n, "x"), [n, n + 1], "done")
        "#;
        let ast = relon_parser::parse_document(src).expect("parse");
        let analyzed = relon_analyzer::analyze(&ast);
        assert!(
            !analyzed.has_errors(),
            "analyze errors: {:?}",
            analyzed.diagnostics
        );
        let lowered = lower_workspace_single(&analyzed, &ast).expect("lower nested tuple return");

        assert!(lowered.return_schema.is_tuple);
        assert_eq!(lowered.return_schema.fields.len(), 3);
        assert!(
            matches!(&lowered.return_schema.fields[0].ty, TypeRepr::Schema { schema } if schema.is_tuple)
        );
        assert!(
            matches!(&lowered.return_schema.fields[1].ty, TypeRepr::List { element } if matches!(element.as_ref(), TypeRepr::Int))
        );
        assert!(matches!(
            lowered.return_schema.fields[2].ty,
            TypeRepr::String
        ));
    }

    /// Recursively flatten a func body's op stream into `out`, descending
    /// into `If` / `Block` / `Loop` arms so assertions see ops wherever
    /// the control-flow places them.
    fn flatten_into(body: &[TaggedOp], out: &mut Vec<Op>) {
        for t in body {
            out.push(t.op.clone());
            match &t.op {
                Op::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    flatten_into(then_body, out);
                    flatten_into(else_body, out);
                }
                Op::Block { body, .. } | Op::Loop { body, .. } => flatten_into(body, out),
                _ => {}
            }
        }
    }

    /// AOT-4 (W16 slice): a 1D `xs[i]` index on a materialised
    /// `List<Int>` receiver lowers to the inline payload addressing —
    /// `(base + 11) & -8` then `+ i*8` then `Op::LoadI64AtAbsolute
    /// { offset: 0 }` — and NEVER to `Op::ListGetByIntIdx` (a
    /// trace-recorder-only op that static codegen rejects) nor to an
    /// eliding peephole that would collapse the index away. Pins the
    /// lowering shape so a regression that swaps the op surfaces here.
    #[test]
    fn list_int_index_emits_inline_payload_load() {
        // `arr: range(0, n)` materialises a `List<Int>`; `arr[1]` indexes
        // it. The `_len <= 1` guard keeps the bench-shape in-bounds; the
        // index lowering itself is what we inspect.
        let src = "#unstrict\n#main(Int n) -> Int\n\
                   (_len(arr) <= 1 ? 0 : arr[1]) where { arr: range(0, n) }";
        let m = lower_source(src);

        // Collect every op (recursing into If / Block / Loop arms) so the
        // assertions see the index ops wherever the ternary places them.
        fn collect(body: &[TaggedOp], out: &mut Vec<Op>) {
            for t in body {
                out.push(t.op.clone());
                match &t.op {
                    Op::If {
                        then_body,
                        else_body,
                        ..
                    } => {
                        collect(then_body, out);
                        collect(else_body, out);
                    }
                    Op::Block { body, .. } | Op::Loop { body, .. } => collect(body, out),
                    _ => {}
                }
            }
        }
        let mut ops = Vec::new();
        for f in &m.funcs {
            collect(&f.body, &mut ops);
        }

        // The index must lower to an inline `LoadI64AtAbsolute { 0 }`.
        let has_inline_load = ops
            .iter()
            .any(|op| matches!(op, Op::LoadI64AtAbsolute { offset: 0 }));
        assert!(
            has_inline_load,
            "xs[i] index must emit an inline `LoadI64AtAbsolute {{ offset: 0 }}`; ops = {ops:?}"
        );

        // It must NOT lower to the trace-recorder-only index op.
        let has_trace_index = ops
            .iter()
            .any(|op| matches!(op, Op::ListGetByIntIdx { .. }));
        assert!(
            !has_trace_index,
            "xs[i] index must NOT emit `Op::ListGetByIntIdx` (trace-recorder-only; static codegen rejects it)"
        );

        // The payload-alignment math must be present: `& -8` (BitAnd I32)
        // plus the `* 8` element stride (Mul I32). A peephole that
        // collapsed the index would drop these.
        let has_align = ops.iter().any(|op| matches!(op, Op::BitAnd(IrType::I32)));
        let has_stride = ops.iter().any(|op| matches!(op, Op::Mul(IrType::I32)));
        assert!(
            has_align && has_stride,
            "xs[i] index must emit payload-align (`BitAnd I32`) + element-stride (`Mul I32`) math; \
             has_align={has_align} has_stride={has_stride}"
        );
    }

    /// AOT-4 (W19 slice): a where-bound `List<List<Int>>` materialises
    /// nested arena records and a 2D index `m[i][k]` composes TWO inline
    /// payload loads — NOT `Op::ListGetByIntIdx`, NOT an eliding peephole
    /// that would collapse the matrix. Pins the materialised 2D path
    /// distinct from the reduce-only fused nested-range shape (which
    /// allocates no list at all).
    #[test]
    fn matmul_2d_materializes_nested_records_and_double_indexes() {
        // `m` is a where-bound `List<List<Int>>` (outer map over an
        // inner map). `m[i][k]` is a cross-row double index — the kernel
        // shape the eliding peephole cannot serve. The `_len` guards keep
        // the read in-bounds without changing the lowering under test.
        let src = "#unstrict\n#main(Int n) -> Int\n\
                   (_len(m) <= 1 ? 0 : m[1][0]) \
                   where { m: range(n).map((i) => range(n).map((j) => i * 10 + j)) }";
        let m = lower_source(src);
        let mut ops = Vec::new();
        for f in &m.funcs {
            flatten_into(&f.body, &mut ops);
        }

        // A 2D materialise allocates the outer record PLUS one inner row
        // record per outer iteration — at least two distinct
        // `AllocScratchDyn` sites in the op stream (outer + inner-in-loop).
        let alloc_dyn = ops
            .iter()
            .filter(|op| matches!(op, Op::AllocScratchDyn))
            .count();
        assert!(
            alloc_dyn >= 2,
            "2D materialise must emit >=2 AllocScratchDyn (outer record + inner rows), got {alloc_dyn}"
        );

        // The double index composes inline payload loads.
        let inline_loads = ops
            .iter()
            .filter(|op| matches!(op, Op::LoadI64AtAbsolute { offset: 0 }))
            .count();
        assert!(
            inline_loads >= 2,
            "2D index m[i][k] must emit >=2 inline LoadI64AtAbsolute (outer handle + inner cell), got {inline_loads}"
        );

        // Never the trace-recorder-only index op.
        assert!(
            !ops.iter()
                .any(|op| matches!(op, Op::ListGetByIntIdx { .. })),
            "2D index must NOT emit Op::ListGetByIntIdx (trace-only; static codegen rejects it)"
        );

        // The materialise path fills the payload with StoreI64AtAbsolute
        // — an eliding peephole would collapse the matrix to a scalar
        // loop and leave none.
        assert!(
            ops.iter()
                .any(|op| matches!(op, Op::StoreI64AtAbsolute { .. })),
            "2D materialise must fill payloads with StoreI64AtAbsolute (no eliding collapse)"
        );

        // Payload-align (`BitAnd I32`) + element-stride (`Mul I32`) math
        // is present (both materialise + index emit it).
        assert!(
            ops.iter().any(|op| matches!(op, Op::BitAnd(IrType::I32)))
                && ops.iter().any(|op| matches!(op, Op::Mul(IrType::I32))),
            "2D path must emit payload-align + element-stride math"
        );
    }

    /// AOT-4 (W19 slice): the production `c.reduce(0, (row_acc, row) =>
    /// row_acc + row.reduce(...))` over a materialised `List<List<Int>>`
    /// lowers through the reduce-over-materialised-list path — it reads
    /// the record headers (`LoadI32AtAbsolute`) for the loop bounds and
    /// the elements (`LoadI64AtAbsolute`) inline, NOT through
    /// `Op::ListGetByIntIdx` or a `list_int_fold` stdlib `Op::Call`.
    #[test]
    fn matmul_reduce_over_materialized_list_lowers_inline() {
        let src = "#unstrict\n\
                   #main(Int n) -> Int\n\
                   c.reduce(0, (row_acc, row) => row_acc + row.reduce(0, (cell_acc, cell) => cell_acc + cell))\n\
                   where {\n\
                     size: n,\n\
                     c: range(size).map((i) => range(size).map((j) => i + j))\n\
                   }";
        let m = lower_source(src);
        let mut ops = Vec::new();
        for f in &m.funcs {
            flatten_into(&f.body, &mut ops);
        }

        // The reduce loops read the `[len]` header with LoadI32AtAbsolute.
        assert!(
            ops.iter()
                .any(|op| matches!(op, Op::LoadI32AtAbsolute { offset: 0 })),
            "reduce-over-list must read the record header via LoadI32AtAbsolute"
        );
        // And the elements with inline LoadI64AtAbsolute.
        assert!(
            ops.iter()
                .any(|op| matches!(op, Op::LoadI64AtAbsolute { offset: 0 })),
            "reduce-over-list must read elements via inline LoadI64AtAbsolute"
        );
        // Never via the trace-only index op.
        assert!(
            !ops.iter()
                .any(|op| matches!(op, Op::ListGetByIntIdx { .. })),
            "reduce-over-list must NOT emit Op::ListGetByIntIdx"
        );
        // The fold must NOT route through the `list_int_fold` stdlib body
        // (that would require a closure conversion the inline reduce
        // avoids). Pins the inline reduce-loop path.
        if let Some(fold_idx) = stdlib_function_index("list_int_fold") {
            assert!(
                !ops.iter()
                    .any(|op| matches!(op, Op::Call { fn_index, .. } if *fn_index == fold_idx)),
                "reduce-over-list must lower inline, not via Op::Call(list_int_fold) (fold_idx={fold_idx})"
            );
        }
    }

    /// AOT-4 (W16 slice): the recursive `sum_qs(xs)` helper's param is
    /// inferred as `List<Int>` (not the I64 default) from the body using
    /// `xs` as a list (`xs[0]`, `_len(xs)`, `_list_filter(xs, ...)`), so
    /// the recursive list arg type-checks. Pins the inference.
    #[test]
    fn recursive_list_helper_param_inferred_list_int() {
        let src = "#unstrict\n#main(Int n) -> Int\n\
                   sum_lt(arr) where { arr: range(0, n), \
                   sum_lt(xs): _len(xs) == 0 ? 0 : (xs[0] + sum_lt(_list_filter(xs, (x) => x > xs[0]))) }";
        let m = lower_source(src);
        // The lifted recursive helper (a lambda) takes `(captures_ptr:
        // I32, xs: ListInt)`. Find a lambda whose user param is ListInt.
        let has_list_param = m.funcs.iter().any(|f| {
            f.params.len() == 2 && f.params[0] == IrType::I32 && f.params[1] == IrType::ListInt
        });
        assert!(
            has_list_param,
            "recursive `sum_lt(xs)` helper must take a `List<Int>` param; funcs = {:?}",
            m.funcs
                .iter()
                .map(|f| (&f.name, &f.params))
                .collect::<Vec<_>>()
        );
    }

    /// Walk `funcs` and collect every `Op::ConstString { idx, value }`
    /// across each func's body (and into any nested `If` / `Block` /
    /// `Loop` arms). Used by the invariant tests below to project the
    /// flat `(idx, value)` ground truth out of the lowered module.
    fn collect_const_strings(funcs: &[Func]) -> Vec<(u32, String)> {
        fn walk(body: &[TaggedOp], out: &mut Vec<(u32, String)>) {
            for t in body {
                match &t.op {
                    Op::ConstString { idx, value } => out.push((*idx, value.clone())),
                    Op::If {
                        then_body,
                        else_body,
                        ..
                    } => {
                        walk(then_body, out);
                        walk(else_body, out);
                    }
                    Op::Block { body, .. } => walk(body, out),
                    Op::Loop { body, .. } => walk(body, out),
                    _ => {}
                }
            }
        }
        let mut acc = Vec::new();
        for f in funcs {
            walk(&f.body, &mut acc);
        }
        acc
    }

    fn lower_source(src: &str) -> Module {
        let ast = relon_parser::parse_document(src).expect("parse");
        let analyzed = relon_analyzer::analyze(&ast);
        assert!(
            !analyzed.has_errors(),
            "analyze errors: {:?}",
            analyzed.diagnostics
        );
        let lowered = lower_workspace_single(&analyzed, &ast).expect("lower");
        lowered.module
    }

    /// Re-export `lower_source` under a stable name so sibling test
    /// modules can drive the same parse + analyze + lower pipeline
    /// without duplicating the boilerplate.
    pub(super) fn test_helpers_lower_source(src: &str) -> Module {
        lower_source(src)
    }

    /// Same-bytes string literals inside one function dedup to a
    /// single idx. Pre-#151 the per-`LowerCtx` counter minted a
    /// fresh idx for each occurrence and the const-pool laid out
    /// three identical `[len][bytes]` records.
    #[test]
    fn intern_dedups_same_literal_in_one_func() {
        // Two `"foo"` literals inside one entry body. Both lower to
        // `Op::ConstString { value: "foo" }` through the same
        // `LowerCtx`.
        let src = "#main() -> String\n\"foo\".concat(\"foo\")";
        let module = lower_source(src);
        let consts = collect_const_strings(&module.funcs);
        let foo_idxs: Vec<u32> = consts
            .iter()
            .filter(|(_, v)| v == "foo")
            .map(|(idx, _)| *idx)
            .collect();
        assert!(
            foo_idxs.len() >= 2,
            "expected at least two `foo` Op::ConstString emissions, got {foo_idxs:?}"
        );
        // Intern contract: every occurrence resolves to the same idx.
        assert!(
            foo_idxs.iter().all(|i| *i == foo_idxs[0]),
            "intern violated — `foo` literals mapped to {foo_idxs:?}"
        );
    }

    /// Distinct literals get distinct idxs (sanity — guards against
    /// a regression that always returns 0).
    #[test]
    fn intern_keeps_distinct_literals_distinct() {
        let src = "#main() -> String\n\"foo\".concat(\"bar\")";
        let module = lower_source(src);
        let consts = collect_const_strings(&module.funcs);
        let foo = consts.iter().find(|(_, v)| v == "foo").map(|(i, _)| *i);
        let bar = consts.iter().find(|(_, v)| v == "bar").map(|(i, _)| *i);
        assert!(foo.is_some(), "missing foo, got {consts:?}");
        assert!(bar.is_some(), "missing bar, got {consts:?}");
        assert_ne!(
            foo, bar,
            "intern collapsed two distinct literals to the same idx"
        );
    }

    /// Module-wide idx-uniqueness across schema-method bodies + the
    /// entry body. Before #151 each func reset `next_string_idx` to
    /// 0, so a method emitting `Op::ConstString { idx: 0, "a" }` and
    /// the entry emitting `Op::ConstString { idx: 0, "b" }` produced
    /// idx collisions the const-pool silently misresolved. The
    /// invariant: every distinct (idx) maps to a single value across
    /// the whole module.
    #[test]
    fn module_wide_idx_uniqueness_across_methods_and_entry() {
        // Schema with a method that returns a string-derived bool
        // (touches a literal), plus an entry body that touches a
        // different literal. The shared intern handle threads through
        // `lower_schema_methods` so both funcs draw idxs from the
        // same allocator.
        let src = "#schema P { String name: * } with {\n\
                     starts_a() -> Bool: self.name.starts_with(\"a\")\n\
                   }\n\
                   #main(P p) -> Bool\n\
                   p.starts_a() ? true : p.name.starts_with(\"b\")";
        let module = lower_source(src);
        let consts = collect_const_strings(&module.funcs);
        // Each idx maps to at most one (value) — collision-free.
        let mut by_idx: HashMap<u32, &String> = HashMap::new();
        for (idx, value) in &consts {
            if let Some(prev) = by_idx.insert(*idx, value) {
                assert_eq!(
                    prev, value,
                    "idx {idx} bound to two values: `{prev}` and `{value}` (module-wide \
                     uniqueness violation)"
                );
            }
        }
        // And we got at least both literals.
        let values: Vec<&String> = consts.iter().map(|(_, v)| v).collect();
        assert!(
            values.iter().any(|v| v.as_str() == "a"),
            "missing `a`, got {values:?}"
        );
        assert!(
            values.iter().any(|v| v.as_str() == "b"),
            "missing `b`, got {values:?}"
        );
    }
}

// =====================================================================
// #165 — `Op::StrConcatN` chain-fold invariants.
//
// End-to-end checks that drive the analyzer + lowering pipeline so the
// fold gate observes the same AST shapes real callers hit. The
// invariants verify both the happy path (a 3+ leaf String chain
// collapses to one `StrConcatN`) and the rejection paths (Dict /
// Schema merge chains and two-operand pair-wise concat keep their
// existing shape).
// =====================================================================

#[cfg(test)]
mod str_concat_chain_tests {
    use super::*;

    /// Walk `funcs` flattening every IR op into a single Vec for
    /// shape-pattern assertions. Recurses into `If` / `Block` / `Loop`
    /// arms so a chain inside a branch still surfaces.
    fn flatten_ops(funcs: &[Func]) -> Vec<Op> {
        fn walk(body: &[TaggedOp], out: &mut Vec<Op>) {
            for t in body {
                out.push(t.op.clone());
                match &t.op {
                    Op::If {
                        then_body,
                        else_body,
                        ..
                    } => {
                        walk(then_body, out);
                        walk(else_body, out);
                    }
                    Op::Block { body, .. } => walk(body, out),
                    Op::Loop { body, .. } => walk(body, out),
                    _ => {}
                }
            }
        }
        let mut acc = Vec::new();
        for f in funcs {
            walk(&f.body, &mut acc);
        }
        acc
    }

    /// Four-leaf left-leaning chain `"a" + "b" + "c" + "d"` folds to
    /// one `Op::StrConcatN { operand_count: 4 }` and emits zero
    /// `Op::Add(IrType::String)` in the same function.
    #[test]
    fn four_way_string_chain_folds_to_str_concat_n() {
        let src = "#main() -> String\n\"a\" + \"b\" + \"c\" + \"d\"";
        let module = super::intern_tests::test_helpers_lower_source(src);
        let ops = flatten_ops(&module.funcs);
        let concat_n_args: Vec<u32> = ops
            .iter()
            .filter_map(|op| match op {
                Op::StrConcatN { operand_count } => Some(*operand_count),
                _ => None,
            })
            .collect();
        assert_eq!(concat_n_args, vec![4], "expected one StrConcatN{{4}}");
        // Pair-wise `Op::Add(IrType::String)` must be elided — every
        // String add was absorbed into the chain fold.
        let leftover_str_adds = ops
            .iter()
            .filter(|op| matches!(op, Op::Add(IrType::String)))
            .count();
        assert_eq!(
            leftover_str_adds, 0,
            "fold left behind {leftover_str_adds} pair-wise Op::Add(String) ops"
        );
    }

    /// Three-leaf chain also fires — the minimal shape the fold gate
    /// requires (LHS itself is an Add).
    #[test]
    fn three_way_string_chain_folds_to_str_concat_n() {
        let src = "#main() -> String\n\"a\" + \"b\" + \"c\"";
        let module = super::intern_tests::test_helpers_lower_source(src);
        let ops = flatten_ops(&module.funcs);
        let concat_n_count = ops
            .iter()
            .filter(|op| matches!(op, Op::StrConcatN { operand_count: 3 }))
            .count();
        assert_eq!(concat_n_count, 1, "expected one StrConcatN{{3}}");
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, Op::Add(IrType::String)))
                .count(),
            0,
        );
    }

    /// Two-leaf concat keeps the existing `Op::Add(IrType::String)`
    /// shape — the fold gate requires `lhs` to be a Binary(Add), which
    /// a single `"a" + "b"` does not satisfy. Backends that don't yet
    /// support the pair-wise variant still bail to the tree-walker via
    /// the existing fallback envelope.
    #[test]
    fn two_way_string_concat_stays_on_add_string() {
        let src = "#main() -> String\n\"a\" + \"b\"";
        let module = super::intern_tests::test_helpers_lower_source(src);
        let ops = flatten_ops(&module.funcs);
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, Op::StrConcatN { .. }))
                .count(),
            0,
            "two-leaf concat should not fold to StrConcatN"
        );
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, Op::Add(IrType::String)))
                .count(),
            1,
            "expected one Op::Add(IrType::String) for the pair-wise concat"
        );
    }
}

// =====================================================================
// Open follow-up #2 — `list.sum(range(...).map(...))` peephole.
//
// Verifies that the extended `try_lower_list_sum_range` recognises the
// `range(...).map((p) => body)` chain and emits a pure-i64 accumulator
// loop with no list allocation. The bytecode VM relies on this shape to
// produce `relon_bytecode` cmp_lua rows for W2 / W6 / W8 / W10 — the
// scalar envelope rejects any IR that materialises a `List<Int>`.
// =====================================================================

#[cfg(test)]
mod range_pipeline_tests {
    use super::*;

    /// Drives the same parse + analyze + lower pipeline `intern_tests`
    /// uses, then returns the lowered entry func's flat op stream so
    /// shape assertions stay focussed on the post-desugar IR.
    fn lower_and_flatten(src: &str) -> Vec<Op> {
        let module = intern_tests::test_helpers_lower_source(src);
        let entry_idx = module.entry_func_index.expect("entry");
        let entry = &module.funcs[entry_idx];
        fn walk(body: &[TaggedOp], out: &mut Vec<Op>) {
            for t in body {
                out.push(t.op.clone());
                match &t.op {
                    Op::If {
                        then_body,
                        else_body,
                        ..
                    } => {
                        walk(then_body, out);
                        walk(else_body, out);
                    }
                    Op::Block { body, .. } => walk(body, out),
                    Op::Loop { body, .. } => walk(body, out),
                    _ => {}
                }
            }
        }
        let mut acc = Vec::new();
        walk(&entry.body, &mut acc);
        acc
    }

    /// `list.sum(range(n).map((i) => i + 1))` desugars to a pure i64
    /// accumulator loop. No `Op::Call` targeting `list_int_map` or
    /// `list_int_sum` should remain — both would force the bytecode
    /// scalar envelope to bail.
    #[test]
    fn map_sum_chain_desugars_to_pure_loop() {
        let src = "#unstrict\n\
                   #import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   list.sum(range(n).map((i) => i + 1))";
        let ops = lower_and_flatten(src);
        // No buffer-protocol stdlib indirection.
        let stdlib_list_int_map = stdlib_function_index("list_int_map").unwrap();
        let stdlib_list_int_sum = stdlib_function_index("list_int_sum").unwrap();
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                assert_ne!(
                    *fn_index, stdlib_list_int_map,
                    "expected `list_int_map` to be inlined by the peephole"
                );
                assert_ne!(
                    *fn_index, stdlib_list_int_sum,
                    "expected `list_int_sum` to be inlined by the peephole"
                );
            }
        }
        // Block shape: one outer loop-exit block + one inner
        // next-iter block (the latter exists so future `.filter`
        // stages have a short-circuit target). The pipeline emits
        // both unconditionally so the same loop body shape works
        // across all consumer / stage combinations.
        let blocks = ops
            .iter()
            .filter(|op| matches!(op, Op::Block { .. }))
            .count();
        let loops = ops
            .iter()
            .filter(|op| matches!(op, Op::Loop { .. }))
            .count();
        assert_eq!(blocks, 2, "expected outer + inner Block, got {blocks}");
        assert_eq!(loops, 1, "expected one inner Loop, got {loops}");
    }

    /// Chained `.map(...).map(...)` collapses into the same accumulator
    /// loop shape — pipelining stages stay zero-alloc.
    #[test]
    fn chained_map_desugars_to_single_loop() {
        let src = "#unstrict\n\
                   #import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   list.sum(range(n).map((i) => i + 1).map((j) => j * 2))";
        let ops = lower_and_flatten(src);
        let stdlib_list_int_map = stdlib_function_index("list_int_map").unwrap();
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                assert_ne!(*fn_index, stdlib_list_int_map);
            }
        }
        let loops = ops
            .iter()
            .filter(|op| matches!(op, Op::Loop { .. }))
            .count();
        assert_eq!(loops, 1, "expected exactly one fused loop, got {loops}");
    }

    /// Sanity guard: the 0-stage form (`list.sum(range(n))`) still
    /// emits the original loop shape. Regression cover for the
    /// peephole refactor that introduced the chain recogniser.
    #[test]
    fn bare_range_sum_still_desugars() {
        let src = "#import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   list.sum(range(n))";
        let ops = lower_and_flatten(src);
        let stdlib_list_int_sum = stdlib_function_index("list_int_sum").unwrap();
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                assert_ne!(*fn_index, stdlib_list_int_sum);
            }
        }
    }

    /// W4-shape: `range(n).map(c1).filter(c2).len()` desugars to a
    /// pure scalar count accumulator. The buffer-protocol stdlib
    /// `list_int_length` / `list_string_length` / `list_int_filter`
    /// must not show up — every one of them would force the bytecode
    /// scalar envelope to bail.
    #[test]
    fn map_filter_len_chain_desugars_to_count_loop() {
        let src = "#unstrict\n\
                   #import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   range(n)\n\
                     .map((i) => \"axb\")\n\
                     .filter((s) => s.contains(\"x\"))\n\
                     .len()";
        let ops = lower_and_flatten(src);
        let banned = [
            "list_int_length",
            "list_string_length",
            "list_int_filter",
            "list_int_map",
        ];
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                for name in banned.iter() {
                    if let Some(idx) = stdlib_function_index(name) {
                        assert_ne!(
                            *fn_index, idx,
                            "expected `{name}` to be inlined by the peephole"
                        );
                    }
                }
            }
        }
        // Exactly one Loop (the outer counter), two Block ops
        // (the loop-exit + the inner next-iter block where the
        // filter short-circuits).
        let blocks = ops
            .iter()
            .filter(|op| matches!(op, Op::Block { .. }))
            .count();
        let loops = ops
            .iter()
            .filter(|op| matches!(op, Op::Loop { .. }))
            .count();
        assert_eq!(blocks, 2, "expected outer + inner Block, got {blocks}");
        assert_eq!(loops, 1, "expected one Loop, got {loops}");
    }

    /// `range(n).filter(c).sum()` shape uses the same emitter on the
    /// `SumI64` consumer side. The W-sf shape isn't in cmp_lua but
    /// exercises the filter -> sum path independent of the W4 chain.
    #[test]
    fn filter_sum_chain_uses_pipeline_emitter() {
        let src = "#unstrict\n\
                   #import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   list.sum(range(n).filter((i) => i % 2 == 0))";
        let ops = lower_and_flatten(src);
        let stdlib_list_int_filter = stdlib_function_index("list_int_filter").unwrap();
        let stdlib_list_int_sum = stdlib_function_index("list_int_sum").unwrap();
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                assert_ne!(*fn_index, stdlib_list_int_filter);
                assert_ne!(*fn_index, stdlib_list_int_sum);
            }
        }
    }

    /// AOT-2 — the W19 matmul cell-reduction shape lowers to a
    /// doubly-nested integer accumulator loop with NO list
    /// materialised. None of the buffer-protocol list-builder stdlib
    /// bodies (`list_int_map` / `list_int_sum` / `list_int_fold`) may
    /// survive — every one would force the bytecode scalar envelope to
    /// bail and would keep the shape off the LLVM AOT tier.
    #[test]
    fn nested_range_map_reduce_desugars_to_double_loop() {
        let src = "#unstrict\n\
                   #import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   range(n).map((i) => range(n).map((j) => (i * n + j) % 100))\n\
                     .reduce(0, (row_acc, row) => row_acc + row.reduce(0, (cell_acc, cell) => cell_acc + cell))";
        let ops = lower_and_flatten(src);
        let banned = ["list_int_map", "list_int_sum", "list_int_fold"];
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                for name in banned.iter() {
                    if let Some(idx) = stdlib_function_index(name) {
                        assert_ne!(
                            *fn_index, idx,
                            "expected `{name}` to be inlined by the nested peephole"
                        );
                    }
                }
            }
        }
        // Two nested loops (outer `i`, inner `j`) and two Block wrappers
        // (one loop-exit guard per loop). No third loop / list builder.
        let blocks = ops
            .iter()
            .filter(|op| matches!(op, Op::Block { .. }))
            .count();
        let loops = ops
            .iter()
            .filter(|op| matches!(op, Op::Loop { .. }))
            .count();
        assert_eq!(loops, 2, "expected two nested Loops, got {loops}");
        assert_eq!(blocks, 2, "expected one Block per loop, got {blocks}");
    }

    /// The `list.sum(row)` inner-fold form lowers to the same
    /// doubly-nested loop with no list materialised.
    #[test]
    fn nested_range_map_list_sum_desugars_to_double_loop() {
        let src = "#unstrict\n\
                   #import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   range(n).map((i) => range(n).map((j) => (i + j) % 100))\n\
                     .reduce(0, (acc, row) => acc + list.sum(row))";
        let ops = lower_and_flatten(src);
        let banned = ["list_int_map", "list_int_sum", "list_int_fold"];
        for op in &ops {
            if let Op::Call { fn_index, .. } = op {
                for name in banned.iter() {
                    if let Some(idx) = stdlib_function_index(name) {
                        assert_ne!(*fn_index, idx);
                    }
                }
            }
        }
        let loops = ops
            .iter()
            .filter(|op| matches!(op, Op::Loop { .. }))
            .count();
        assert_eq!(loops, 2, "expected two nested Loops, got {loops}");
    }

    /// CODEGEN-QUALITY (W18 slice): `_len(_list_filter(range(2, n),
    /// (k) => ...))` — where the filtered list is dead (only `_len`
    /// consumes it) — FUSES to a pure i64 counting loop that never
    /// materialises the filtered list. The fused shape emits NO
    /// `list_int_filter` `Op::Call` and NO `AllocScratchDyn` for the
    /// filter output; instead the predicate is inlined under an
    /// `Op::Loop` and a counter is incremented per survivor.
    ///
    /// This is dead-list-elimination / stream fusion — the count is
    /// identical to the materialise-then-`_len` path (same predicate,
    /// same range), only the intermediate `List<Int>` is elided. It is
    /// NOT an algorithm substitution.
    #[test]
    fn len_filter_range_fuses_to_counting_loop_no_materialize() {
        let src = "#unstrict\n\
                   #main(Int n) -> Int\n\
                   _len(_list_filter(range(2, n), (k) => k % 2 == 0))";
        let ops = lower_and_flatten(src);

        // No `list_int_filter` `Op::Call` survives — the filter is
        // fused into the counter loop, not dispatched to the bundled
        // stdlib body.
        let filter_idx = stdlib_function_index("list_int_filter").unwrap();
        let filter_calls = ops
            .iter()
            .filter(|op| matches!(op, Op::Call { fn_index, .. } if *fn_index == filter_idx))
            .count();
        assert_eq!(
            filter_calls, 0,
            "fused shape must NOT call list_int_filter, got {filter_calls} calls"
        );

        // No filter-output `AllocScratchDyn` — nothing is materialised.
        // (The eliding range counter loop allocates no scratch record.)
        let alloc_dyn = ops
            .iter()
            .filter(|op| matches!(op, Op::AllocScratchDyn))
            .count();
        assert_eq!(
            alloc_dyn, 0,
            "fused shape must NOT materialise any List<Int> (got {alloc_dyn} AllocScratchDyn)"
        );

        // No per-element arena store fills a materialised payload.
        let store_i64 = ops
            .iter()
            .filter(|op| matches!(op, Op::StoreI64AtAbsolute { .. }))
            .count();
        assert_eq!(
            store_i64, 0,
            "fused shape must NOT store list elements to an arena (got {store_i64})"
        );

        // No `ReadStringLen` survivor-record read — the count comes
        // straight from the loop accumulator.
        let read_len = ops
            .iter()
            .filter(|op| matches!(op, Op::ReadStringLen))
            .count();
        assert_eq!(
            read_len, 0,
            "fused shape reads no length prefix; the counter is the result (got {read_len})"
        );

        // The fusion emits a counting `Op::Loop` with an i64 increment
        // (`Op::Add(I64)` of the accumulator) under the predicate.
        let loops = ops
            .iter()
            .filter(|op| matches!(op, Op::Loop { .. }))
            .count();
        assert!(loops >= 1, "expected a counting Op::Loop, got {loops}");
        assert!(
            ops.iter().any(|op| matches!(op, Op::Add(IrType::I64))),
            "expected an i64 counter increment in the fused loop"
        );
    }

    /// CODEGEN-QUALITY: the full W18 production shape — a `where`-bound
    /// recursive `is_prime` helper called from the filter predicate —
    /// also fuses to the counting loop (no `list_int_filter` call, no
    /// materialised list). The predicate body, including the recursive
    /// `is_prime(k, 2)` call, is inlined under the loop.
    #[test]
    fn w18_prime_count_shape_fuses_no_filter_call() {
        let src = "#unstrict\n\
                   #main(Int n) -> Int\n\
                   _len(_list_filter(range(2, n), (k) => is_prime(k, 2)))\n\
                   where {\n\
                     is_prime(k, d): d * d > k ? true : (k % d == 0 ? false : is_prime(k, d + 1))\n\
                   }";
        let ops = lower_and_flatten(src);
        let filter_idx = stdlib_function_index("list_int_filter").unwrap();
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, Op::Call { fn_index, .. } if *fn_index == filter_idx))
                .count(),
            0,
            "W18 fused shape must NOT route through list_int_filter"
        );
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, Op::AllocScratchDyn))
                .count(),
            0,
            "W18 fused shape must NOT materialise the filtered list"
        );
        // The counting loop is present.
        assert!(
            ops.iter().any(|op| matches!(op, Op::Loop { .. })),
            "expected the fused counting Op::Loop"
        );
    }

    /// #359 (W20): the softened n-body kernel lowers with a `List<Float>`
    /// accumulator. Pins the envelope additions at the IR level: the
    /// list-literal materialiser (`AllocScratchDyn` + `StoreF64AtAbsolute`
    /// element stores), the `List<Float>` 1D index (`LoadF64AtAbsolute`),
    /// the list-valued reduce carry (the accumulator let rides
    /// `ListFloat`), and the closures lifted to `MakeClosure` (no leftover
    /// stdlib indirection). The exact numeric parity is pinned separately
    /// by the LLVM oracle test `llvm_w20_n_body.rs`.
    #[test]
    fn w20_n_body_lowers_with_list_float_reduce_accumulator() {
        let src = "#unstrict\n\
             #main(Int n) -> Float\n\
             final_state[0] * 1.0 + final_state[1] * 2.0 + final_state[2] * 3.0 + final_state[3] * 4.0\n\
               + final_state[4] * 5.0 + final_state[5] * 6.0 + final_state[6] * 7.0 + final_state[7] * 8.0\n\
             where {\n\
               dt: 0.01,\n\
               soft: 0.1,\n\
               m0: 1.0, m1: 2.0, m2: 0.5, m3: 3.0,\n\
               init: [0.0, 1.0, 2.5, 4.0, 0.1, 0.0, 0.0, 0.2],\n\
               pair_force(s, i, j, mj):\n\
                 i == j ? 0.0 :\n\
                   (s[j] - s[i]) * mj * (1.0 / (((s[j] - s[i]) * (s[j] - s[i]) + soft) * ((s[j] - s[i]) * (s[j] - s[i]) + soft))),\n\
               accel(s, i): pair_force(s, i, 0, m0) + pair_force(s, i, 1, m1) + pair_force(s, i, 2, m2) + pair_force(s, i, 3, m3),\n\
               step(s): [\n\
                 s[0] + s[4] * dt,\n\
                 s[1] + s[5] * dt,\n\
                 s[2] + s[6] * dt,\n\
                 s[3] + s[7] * dt,\n\
                 s[4] + accel(s, 0) * dt,\n\
                 s[5] + accel(s, 1) * dt,\n\
                 s[6] + accel(s, 2) * dt,\n\
                 s[7] + accel(s, 3) * dt\n\
               ],\n\
               final_state: range(n).reduce(init, (s, _step) => step(s))\n\
             }";
        let module = intern_tests::test_helpers_lower_source(src);
        let entry = &module.funcs[module.entry_func_index.expect("entry")];

        // The `init` literal materialises into a scratch arena: an
        // `AllocScratchDyn` + 8 `StoreF64AtAbsolute` element stores
        // appear in the entry body (the `step` body's literal stores
        // live in the lambda func, not the entry).
        let entry_f64_stores = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::StoreF64AtAbsolute { .. }))
            .count();
        assert_eq!(
            entry_f64_stores, 8,
            "expected the `init` 8-element List<Float> literal to emit 8 f64 stores, \
             got {entry_f64_stores}"
        );

        // The reduce body carries the `List<Float>` accumulator: a
        // `LetSet { ty: ListFloat }` appears (the accumulator slot).
        let has_listfloat_let = {
            fn walk(body: &[TaggedOp]) -> bool {
                body.iter().any(|t| match &t.op {
                    Op::LetSet {
                        ty: IrType::ListFloat,
                        ..
                    } => true,
                    Op::Block { body, .. } | Op::Loop { body, .. } => walk(body),
                    Op::If {
                        then_body,
                        else_body,
                        ..
                    } => walk(then_body) || walk(else_body),
                    _ => false,
                })
            }
            walk(&entry.body)
        };
        assert!(
            has_listfloat_let,
            "expected a ListFloat-typed let (the reduce accumulator carry)"
        );

        // `final_state[k]` indexes the List<Float> -> `LoadF64AtAbsolute`.
        let has_f64_load = {
            fn walk(body: &[TaggedOp]) -> bool {
                body.iter().any(|t| match &t.op {
                    Op::LoadF64AtAbsolute { .. } => true,
                    Op::Block { body, .. } | Op::Loop { body, .. } => walk(body),
                    Op::If {
                        then_body,
                        else_body,
                        ..
                    } => walk(then_body) || walk(else_body),
                    _ => false,
                })
            }
            walk(&entry.body)
        };
        assert!(
            has_f64_load,
            "expected a LoadF64AtAbsolute for the `final_state[k]` index reads"
        );

        // The where-bound closures `pair_force` / `accel` / `step` lift
        // to lambdas in the closure table (3 entries).
        assert_eq!(
            module.closure_table.len(),
            3,
            "expected pair_force + accel + step lambdas, got {}",
            module.closure_table.len()
        );

        // `step` returns a `List<Float>` handle: its lambda func's
        // declared return type is ListFloat.
        let step_lambda = module
            .funcs
            .iter()
            .find(|f| f.ret == IrType::ListFloat)
            .expect("expected a lambda returning ListFloat (the `step` closure)");
        assert!(
            step_lambda
                .body
                .iter()
                .filter(|t| matches!(t.op, Op::StoreF64AtAbsolute { .. }))
                .count()
                == 8,
            "expected `step`'s body to materialise an 8-element List<Float> via 8 f64 stores"
        );
    }

    /// Symmetric to the W20 Float lowering shape: a COMPUTED `List<Int>`
    /// literal `[n, n+1, n*2, n%3+7]` (each element a non-literal Int
    /// expression over the `#main` arg) must materialise through
    /// `emit_list_int_literal_materialize` — an `AllocScratchDyn` + an
    /// i32 length header (`StoreI32AtAbsolute`) + one
    /// `StoreI64AtAbsolute` PER ELEMENT — and MUST NOT intern as a
    /// `ConstListInt` (which the LLVM AOT envelope cannot materialise).
    /// The where-bound list is passed to a closure so the analyzer's
    /// tuple-index inference doesn't reject it; the exact numeric parity
    /// against the tree-walker is pinned by the LLVM oracle test
    /// `llvm_computed_int_list.rs`.
    #[test]
    fn computed_int_list_literal_lowers_via_scratch_materialize() {
        let src = "#unstrict\n\
             #main(Int n) -> Int\n\
             f(xs) where {\n\
               xs: [n, n + 1, n * 2, n % 3 + 7],\n\
               f(ys): ys[0] + ys[1] + ys[3]\n\
             }";
        let module = intern_tests::test_helpers_lower_source(src);
        let entry = &module.funcs[module.entry_func_index.expect("entry")];

        // The computed `xs` literal materialises in the entry body: at
        // least one `AllocScratchDyn` (the 4-element record) and exactly
        // 4 `StoreI64AtAbsolute` element stores (one per element).
        let alloc_dyn = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::AllocScratchDyn))
            .count();
        assert!(
            alloc_dyn >= 1,
            "expected the computed List<Int> literal to emit an AllocScratchDyn record, \
             got {alloc_dyn}"
        );
        let i64_stores = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::StoreI64AtAbsolute { .. }))
            .count();
        assert_eq!(
            i64_stores, 4,
            "expected the 4-element computed List<Int> literal to emit 4 i64 element stores, \
             got {i64_stores}"
        );
        // The i32 length header is stored.
        let i32_stores = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::StoreI32AtAbsolute { .. }))
            .count();
        assert!(
            i32_stores >= 1,
            "expected an i32 length header store for the materialised record, got {i32_stores}"
        );

        // The whole module must NOT contain a `ConstListInt` for this
        // computed literal — that would mean the const-intern path swallowed
        // it (the AOT envelope cannot materialise an interned const list).
        let has_const_list_int = module.funcs.iter().any(|f| {
            f.body
                .iter()
                .any(|t| matches!(t.op, Op::ConstListInt { .. }))
        });
        assert!(
            !has_const_list_int,
            "computed List<Int> literal must materialise, not intern as ConstListInt"
        );

        // The materialised handle is tagged ListInt: a `LetSet { ty:
        // ListInt }` appears (the where-binding slot for `xs`).
        let has_listint_let = entry.body.iter().any(|t| {
            matches!(
                &t.op,
                Op::LetSet {
                    ty: IrType::ListInt,
                    ..
                }
            )
        });
        assert!(
            has_listint_let,
            "expected a ListInt-typed let (the `xs` where-binding carry)"
        );
    }
}

// =====================================================================
// Phase F.2 — first-class closure value boundary.
//
// The W7 cmp_lua workload (`#main(Int n) -> Dict { #internal fib: (k) =>
// ..., result: fib(n) }`) currently fails `lower_workspace_single` at
// the return-type build step because `-> Dict` has no canonical
// representation. The downstream `Expr::Closure` at a non-higher-order
// site would also reject (see `lower_expr`'s explicit
// `ClosureAcrossBoundary` arm), so even after Phase A's return-type
// work the body would still bail.
//
// These tests pin the *current* diagnostic shape so the Phase B lifting
// surfaces as a test failure (the assertions flip from `Err(...)` to
// `Ok(...)`), giving the future implementer a clean checklist of which
// rejection sites have been lifted. The design doc
// `docs/internal/w7-closure-as-value-design.md` (local-only) captures
// the full plan.
// =====================================================================

#[cfg(test)]
mod w7_closure_boundary_tests {
    use super::*;

    /// Drive parse + analyze + `lower_workspace_single` without the
    /// `.expect("lower")` the `intern_tests::lower_source` helper does
    /// — Phase F.2 needs to observe the failure shape, not panic on it.
    fn try_lower(src: &str) -> Result<Module, LoweringError> {
        let ast = relon_parser::parse_document(src).expect("parse");
        let analyzed = relon_analyzer::analyze(&ast);
        // We intentionally don't `assert!(!analyzed.has_errors())`: the
        // analyzer may surface a soft warning for the closure-typed
        // dict field, but lowering still gets to run. Phase A only
        // cares about the IR-side diagnostic.
        lower_workspace_single(&analyzed, &ast).map(|l| l.module)
    }

    /// Phase C verification: the W7 production source — verbatim copy
    /// of `crates/relon-bench/benches/cmp_lua.rs::w7_relon_src` —
    /// now lowers cleanly through `lower_workspace_single`. The body
    /// produces an anon-Dict-return record with the `result` scalar
    /// field while `fib` is lifted to an internal let-bound closure
    /// handle (it does not appear in the host-visible schema).
    ///
    /// Pre-Phase-C this rejected at the return-schema build step with
    /// `UnsupportedTypeInMain { type_name: "Dict" }`; Phase C lifts
    /// that gap via [`anon_dict_return_plan`] +
    /// [`lower_anon_dict_body`]. Future Phase D scope: backend tier
    /// wiring (`Op::MakeClosure` / `Op::CallClosure`) for bytecode /
    /// trace_jit / LLVM emitters that still reject those ops.
    #[test]
    fn w7_production_source_lowers_via_anon_dict_return_plan() {
        let src = "#main(Int n) -> Dict\n\
                   {\n\
                     #internal\n\
                     fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                     result: fib(n)\n\
                   }";
        let module = try_lower(src).expect("Phase C lowers W7 anon-Dict-return source");

        // The synthesised return schema only carries the `result`
        // scalar — `fib` is internal.
        let entry_idx = module
            .entry_func_index
            .expect("Phase C builds an entry func");
        let entry = &module.funcs[entry_idx];
        // Closure table populated with the W7 `fib` lambda.
        assert_eq!(
            module.closure_table.len(),
            1,
            "expected one entry in closure_table for the `fib` lambda"
        );
        // The lambda Func body exists right after the entry func.
        assert!(
            module.funcs.len() >= 2,
            "expected entry + at least one lambda func, got {} funcs",
            module.funcs.len()
        );
        // Entry body emits `MakeClosure` exactly once (for `fib`).
        let make_count = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::MakeClosure { .. }))
            .count();
        assert_eq!(
            make_count, 1,
            "expected the entry to emit MakeClosure once for the `fib` let, got {make_count}"
        );
        // The `result` field's `fib(n)` lowers to `LetGet { Closure
        // } + CallClosure`.
        let call_count = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::CallClosure { .. }))
            .count();
        assert_eq!(
            call_count, 1,
            "expected the entry to emit CallClosure once for `result: fib(n)`, got {call_count}"
        );
    }

    /// Phase B foundation check: the canonical schema digest treats
    /// the new [`TypeRepr::Closure`] variant as a structural shape.
    ///
    /// Two closure-typed fields with the same `(params, ret)` shape
    /// must collapse to the same digest, and a shape difference (extra
    /// param, different return) must invalidate the digest so a host
    /// SDK refuses to load a module whose declared closure surface
    /// drifted from its compile-time view.
    ///
    /// The test is gated on the digest plumbing alone — no lowering of
    /// W7-shape user source. The closure-as-value lowering itself
    /// stays Phase C scope; this test only confirms the type-system
    /// hook the future implementation will hang behaviour off.
    #[test]
    fn typerepr_closure_digest_distinguishes_signature_shapes() {
        use relon_eval_api::schema_canonical::{schema_hash, Field, Schema, TypeRepr};

        let int_to_int = TypeRepr::Closure {
            params: vec![TypeRepr::Int],
            ret: Box::new(TypeRepr::Int),
        };
        // Same shape, different declaration — must collapse.
        let int_to_int_clone = TypeRepr::Closure {
            params: vec![TypeRepr::Int],
            ret: Box::new(TypeRepr::Int),
        };
        // Extra param — must distinguish.
        let int_int_to_int = TypeRepr::Closure {
            params: vec![TypeRepr::Int, TypeRepr::Int],
            ret: Box::new(TypeRepr::Int),
        };
        // Different return — must distinguish.
        let int_to_float = TypeRepr::Closure {
            params: vec![TypeRepr::Int],
            ret: Box::new(TypeRepr::Float),
        };

        let wrap = |ty: TypeRepr| Schema {
            name: "Probe".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![Field {
                name: "f".into(),
                ty,
                default: None,
            }],
        };

        // Structural equality.
        assert_eq!(
            schema_hash(&wrap(int_to_int.clone())),
            schema_hash(&wrap(int_to_int_clone)),
            "two structurally identical closure-typed fields must hash equal"
        );
        // Shape sensitivity.
        assert_ne!(
            schema_hash(&wrap(int_to_int.clone())),
            schema_hash(&wrap(int_int_to_int)),
            "param-arity change must invalidate the digest"
        );
        assert_ne!(
            schema_hash(&wrap(int_to_int)),
            schema_hash(&wrap(int_to_float)),
            "return-type change must invalidate the digest"
        );
    }

    /// Phase B layout-guard check: closure-typed fields must reject at
    /// [`SchemaLayout::offsets_for`] so the binary-handshake builder
    /// can't accidentally lay a non-portable scratch-heap pointer into
    /// a host-visible record. The canonical schema digest already
    /// distinguishes the shape (see the digest test above); the layout
    /// pass is the second line of defence so a hand-built `Schema`
    /// that bypasses the lowering pass still surfaces a typed error
    /// rather than a silent dangle.
    #[test]
    fn closure_field_rejects_at_schema_layout() {
        use relon_eval_api::layout::{LayoutError, SchemaLayout};
        use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};

        let schema = Schema {
            name: "ProbeWithClosure".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![Field {
                name: "fib".into(),
                ty: TypeRepr::Closure {
                    params: vec![TypeRepr::Int],
                    ret: Box::new(TypeRepr::Int),
                },
                default: None,
            }],
        };
        let err = SchemaLayout::offsets_for(&schema)
            .expect_err("closure fields must reject at layout time");
        match err {
            LayoutError::UnsupportedTypeInLayoutV1 { kind, field } => {
                assert_eq!(kind, "Closure", "expected kind tag `Closure`, got {kind}");
                assert_eq!(field, "fib");
            }
            other => panic!(
                "expected LayoutError::UnsupportedTypeInLayoutV1 {{ kind: \"Closure\" }}, got {other:?}"
            ),
        }
    }

    /// W5-P1 verification: a `{str:int}` dict literal sitting on a
    /// `#internal` field of an anon-Dict-return body lowers to a
    /// dict-value capture — `Op::ConstDict` materialising the entry set
    /// followed by `Op::LetSet { ty: IrType::Dict }` into an internal
    /// let-local. The dict field contributes no host-visible record
    /// slot (it is internal, like a lifted closure), so the synthesised
    /// return schema only carries the `result` scalar.
    ///
    /// This is the construction + capture half of the W5 dict-value
    /// surface. The read half (`DictGetByStringKey`) is a P3 follow-up;
    /// this `result` field is a plain scalar so the body stays inside
    /// the P1 envelope (no DictGet).
    #[test]
    fn w5p1_dict_value_field_lowers_to_const_dict_let() {
        let src = "#main(Int n) -> Dict\n\
                   {\n\
                     #internal\n\
                     d: { a: 1, b: 2, c: 3 },\n\
                     result: n\n\
                   }";
        let ast = relon_parser::parse_document(src).expect("parse");
        let analyzed = relon_analyzer::analyze(&ast);
        let lowered = lower_workspace_single(&analyzed, &ast)
            .expect("W5-P1 lowers anon-Dict-return with dict-value field");
        let module = &lowered.module;

        let entry_idx = module.entry_func_index.expect("W5-P1 builds an entry func");
        let entry = &module.funcs[entry_idx];

        // Exactly one ConstDict carrying the source-order entries.
        let const_dicts: Vec<&Vec<(String, i64)>> = entry
            .body
            .iter()
            .filter_map(|t| match &t.op {
                Op::ConstDict { entries, .. } => Some(entries),
                _ => None,
            })
            .collect();
        assert_eq!(
            const_dicts.len(),
            1,
            "expected exactly one ConstDict for the `d` dict-value field"
        );
        assert_eq!(
            const_dicts[0],
            &vec![
                ("a".to_string(), 1i64),
                ("b".to_string(), 2i64),
                ("c".to_string(), 3i64),
            ],
            "ConstDict must carry the source-declaration-order entries"
        );

        // The dict pointer is stashed into a Dict-typed let-local.
        let dict_let_sets = entry
            .body
            .iter()
            .filter(|t| {
                matches!(
                    t.op,
                    Op::LetSet {
                        ty: IrType::Dict,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            dict_let_sets, 1,
            "expected one `LetSet {{ ty: Dict }}` capturing the dict value"
        );

        // `d` is internal — the host-visible return schema carries only
        // the `result` scalar field.
        let schema = &lowered.return_schema;
        assert_eq!(
            schema.fields.len(),
            1,
            "dict-value field must be internal — schema carries only `result`"
        );
        assert_eq!(schema.fields[0].name, "result");
    }

    /// W5-P1 honesty edge: a dict field whose value is not a plain Int
    /// literal must reject at lowering (value-type widening is P2/P3),
    /// rather than silently lowering a half-supported shape.
    #[test]
    fn w5p1_non_int_dict_value_rejects() {
        let src = "#main(Int n) -> Dict\n\
                   {\n\
                     #internal\n\
                     d: { a: 1, b: \"two\" },\n\
                     result: n\n\
                   }";
        let err = try_lower(src).expect_err("non-Int dict value must reject in P1");
        assert!(
            matches!(err, LoweringError::UnsupportedExpr { .. }),
            "expected UnsupportedExpr for non-Int dict value, got {err:?}"
        );
    }

    /// AOT-3 verification: a W17-shaped where-bound recursive helper
    /// (`bs(lo, hi, t): ...` declared in a `where { ... }` clause and
    /// called from a `reduce` fold) now lowers cleanly. Pre-AOT-3 the
    /// `bs(...)` closure binding hit the `Expr::Closure { .. } =>
    /// ClosureAcrossBoundary` arm of `lower_expr` and the whole source
    /// was rejected at IR lowering, leaving W17 `n/a` on every compiled
    /// backend.
    ///
    /// The lowered entry func must:
    /// * emit `MakeClosure` once for the lifted `bs` let,
    /// * emit `CallClosure` for the recursive self-calls + the fold-site
    ///   call (the W17 body has three `bs(...)` calls: the two recursive
    ///   tails and the `acc + bs(0, n, ...)` fold combine).
    #[test]
    fn w17_where_bound_recursive_helper_lifts_to_closure_let() {
        // W17-shaped binary search: pure recursion over an arithmetic
        // index range, no list materialisation.
        let src = "#unstrict\n\
                   #main(Int n) -> Int\n\
                   range(n).reduce(0, (acc, i) => acc + bs(0, n, (i * 31) % n))\n\
                   where {\n\
                     bs(lo, hi, t): hi - lo <= 1 ? lo : (\n\
                       (lo + hi) / 2 <= t\n\
                         ? bs((lo + hi) / 2, hi, t)\n\
                         : bs(lo, (lo + hi) / 2, t)\n\
                     )\n\
                   }";
        let module = try_lower(src).expect("AOT-3 lowers W17 where-bound recursive helper");

        // The `bs` lambda lands in the closure table.
        assert_eq!(
            module.closure_table.len(),
            1,
            "expected one closure-table entry for the lifted `bs` helper"
        );
        let entry_idx = module.entry_func_index.expect("AOT-3 builds an entry func");
        let entry = &module.funcs[entry_idx];

        // Exactly one MakeClosure for the `bs` let-binding.
        let make_count = entry
            .body
            .iter()
            .filter(|t| matches!(t.op, Op::MakeClosure { .. }))
            .count();
        assert_eq!(
            make_count, 1,
            "expected MakeClosure once for the `bs` where-binding, got {make_count}"
        );

        // Walk the op tree (the reduce fold lowers into an `Op::Loop`,
        // so the fold-site call nests inside the loop body).
        fn count_call_closure(body: &[TaggedOp]) -> usize {
            let mut n = 0;
            for t in body {
                match &t.op {
                    Op::CallClosure { .. } => n += 1,
                    Op::If {
                        then_body,
                        else_body,
                        ..
                    } => {
                        n += count_call_closure(then_body);
                        n += count_call_closure(else_body);
                    }
                    Op::Block { body, .. } | Op::Loop { body, .. } => n += count_call_closure(body),
                    _ => {}
                }
            }
            n
        }
        // The fold-site `bs(0, n, ...)` call lowers to CallClosure in
        // the entry body (nested inside the reduce loop).
        let entry_calls = count_call_closure(&entry.body);
        assert_eq!(
            entry_calls, 1,
            "expected one fold-site CallClosure in the entry body, got {entry_calls}"
        );

        // Walk every non-entry func (the lifted `bs` lambda) and count
        // its recursive CallClosure self-calls — the two bisection
        // tails.
        let lambda_self_calls: usize = module
            .funcs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != entry_idx)
            .map(|(_, f)| count_call_closure(&f.body))
            .sum();
        assert_eq!(
            lambda_self_calls, 2,
            "expected two recursive self-CallClosure inside the `bs` lambda, got {lambda_self_calls}"
        );
    }
}
