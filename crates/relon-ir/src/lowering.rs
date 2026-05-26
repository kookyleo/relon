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
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
use relon_parser::{ClosureParam, Expr, Node, Operator, TokenKey, TokenRange, TypeNode};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::error::LoweringError;
use crate::intern::ConstInternTables;
use crate::ir::{ClosureCapture, Func, IrType, Module, Op, TaggedOp};
use crate::stdlib::{
    builtin_stdlib, stdlib_closure_arg_signature, stdlib_function_count, stdlib_function_index,
    stdlib_method_index,
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
    /// Phase 10-a: lambda functions emitted in this lowering pass.
    /// Each entry is a fully-lowered closure body with the implicit
    /// `captures_ptr: i32` as its first parameter; the closure-table
    /// emit step picks the entries up in declaration order. Shared
    /// across nested closure sites so the table assignment stays
    /// stable.
    lambda_funcs: Vec<Func>,
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
}

impl<'a> LowerCtx<'a> {
    fn new(
        params: &'a [LocalBinding],
        schema_resolver: SchemaResolver<'a>,
        method_registry: SchemaMethodRegistry,
        const_intern: Rc<RefCell<ConstInternTables>>,
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
            lambda_funcs: Vec::new(),
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
            lambda_funcs: Vec::new(),
        }
    }

    /// Clone the shared intern handle for spawning a nested
    /// `LowerCtx` (lambda body / schema-method body) that must
    /// participate in the same module-wide idx space.
    fn intern_handle(&self) -> Rc<RefCell<ConstInternTables>> {
        Rc::clone(&self.const_intern)
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
/// is local 4 (i64). The entry prologue forwards it into the module-
/// internal mutable `relon_caps_avail` global so the codegen-emitted
/// `check_cap` prologue can keep reading through `global.get`. Moving
/// the bitmap from an imported global to a run_main argument is what
/// lets the host SDK build a single `wasmtime::InstancePre` and reuse
/// it across stores — the prior import made the linker store-bound.
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
    let entry_tree =
        ws.modules
            .get(entry_module)
            .ok_or_else(|| LoweringError::EntryModuleNotFound {
                module: entry_module.to_string(),
            })?;
    let entry_root =
        ws.nodes
            .get(entry_module)
            .ok_or_else(|| LoweringError::EntryModuleNotFound {
                module: entry_module.to_string(),
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
            return Err(LoweringError::MultipleMainDirectives {
                entry_module: entry_module.to_string(),
                other_module: (*id).to_string(),
            });
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
    use std::collections::{HashSet, VecDeque};
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
                    return Err(LoweringError::DuplicateSchemaAcrossFiles {
                        name,
                        first_module: other_id.clone(),
                        second_module: (*id).to_string(),
                    });
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
    let sig = tree
        .main_signature
        .as_ref()
        .ok_or_else(|| LoweringError::MissingMain {
            module: module_id.to_string(),
        })?;

    // Phase 10-a: reject closure-typed `#main` params + return type
    // up front. Wasm-side closure values are scratch-heap pointers
    // whose lifetime ends at `run_main` return — carrying one
    // through the binary handshake would dangle. Detected here so the
    // diagnostic message points at the directive declaration rather
    // than at a downstream schema-build failure.
    for p in &sig.params {
        if type_node_names_closure(&p.type_node) {
            return Err(LoweringError::ClosureAcrossBoundary {
                context: format!("`#main` parameter `{}`", p.name),
                range: p.type_node.range,
            });
        }
    }
    if let Some(rt) = sig.return_type.as_ref() {
        if type_node_names_closure(rt) {
            return Err(LoweringError::ClosureAcrossBoundary {
                context: "`#main` return type".to_string(),
                range: rt.range,
            });
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
    let user_return_schema = resolve_return_user_schema(sig.return_type.as_ref(), &resolver)?;

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
    } else {
        build_main_return_schema(sig)?
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

    // Phase 5: enumerate every user-declared schema method, assign
    // IR-side indices (and through them combined wasm-level
    // function indices), then lower each method body into a `Func`.
    // The entry body is appended last so it can resolve
    // `obj.method()` calls against the populated registry.
    let (method_funcs, method_registry) =
        lower_schema_methods(tree, &resolver, Rc::clone(&const_intern))?;
    let entry_ir_idx = method_funcs.len();

    // Walk the body into a single op stream + virtual stack via the
    // per-function lowering context. Phase 3.a's let-bindings + const
    // literals piggy-back on `LowerCtx` for their counters.
    let mut ctx = LowerCtx::new(&locals, resolver, method_registry, const_intern);

    if let Some(ref user_schema) = user_return_schema {
        // Branded dict-return path: emit `AllocRootRecord` + the
        // per-field stores into the root record, then `Return`.
        // Top-level dict expression must be a `Expr::Dict(...)` (the
        // brand is contributed by the return type).
        let dict_pairs = match &*root.expr {
            Expr::Dict(pairs) => pairs.as_slice(),
            _ => {
                return Err(LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Body-of-branded-#main must be a dict literal, got `{}`",
                        root.expr.kind()
                    ),
                    range: root.range,
                });
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
    } else {
        // Scalar-return path: existing v1 shape.
        lower_expr(&root.expr, root.range, &mut ctx)?;

        // Trailing StoreField for the single root return value. Pops
        // the top stack entry — codegen will translate this to
        // `local.get $out_ptr; <value>; <store>.offset=N`.
        let ret_offset = return_layout
            .fields
            .first()
            .map(|f| f.offset as u32)
            .unwrap_or(0);
        let ret_ir_ty = type_repr_to_ir_type(&return_schema.fields[0].ty)?;
        ctx.out.push(TaggedOp {
            op: Op::StoreField {
                offset: ret_offset,
                ty: ret_ir_ty,
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
    let body = ctx.out;
    // Hoist the lambda funcs emitted by the entry body's lowering
    // pass; the entry context is consumed by the move below.
    let entry_lambda_funcs = ctx.lambda_funcs;

    let func = Func {
        name: "run_main".to_string(),
        // Wasm-level binary handshake signature: four i32 slots
        // (in_ptr, in_len, out_ptr, out_cap) plus the Phase-11
        // capability bitmap (`caps_arg: i64`). User-declared params
        // reach the body through `LoadField`. `caps_arg` is forwarded
        // into the module-internal mutable `relon_caps_avail` global
        // by the entry prologue so `check_cap` keeps reading through
        // `global.get`.
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

    Ok(LoweredEntry {
        module: Module {
            imports: Vec::new(),
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
            LoweringError::UnsupportedTypeInMain {
                type_name: type_head_for_display(&p.type_node),
                range: p.type_node.range,
            }
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
    })
}

/// Widened [`type_node_to_canonical`] that also accepts single-segment
/// references to user-declared schemas. Used by [`build_main_params_schema`]
/// so `#main(User u)` lowers the `u` param into a pointer-indirect
/// schema slot. Scalar / String / List<Int> heads still resolve via
/// the narrower [`type_node_to_canonical`] helper — keeping that path
/// dependency-free lets the rest of the lowering pass reach for it
/// without threading the resolver through.
fn type_node_to_canonical_with_schemas(
    t: &TypeNode,
    resolver: &SchemaResolver<'_>,
) -> Option<TypeRepr> {
    if let Some(scalar) = type_node_to_canonical(t) {
        return Some(scalar);
    }
    // Phase 10-c: `List<User>` — the head is `List` with a single
    // generic naming a user schema. Recurse into the generic via
    // `with_schemas` so the inner schema lookup succeeds, then wrap
    // it in a List variant.
    if t.path.len() == 1
        && t.path[0].as_str() == "List"
        && t.generics.len() == 1
        && t.variant_fields.is_none()
    {
        let inner = type_node_to_canonical_with_schemas(&t.generics[0], resolver)?;
        if matches!(inner, TypeRepr::Schema { .. }) {
            return Some(TypeRepr::List {
                element: Box::new(inner),
            });
        }
        return None;
    }
    // Only a single-segment, non-variant, non-generic head can name a
    // user schema. Anything else falls through.
    if t.path.len() != 1 || !t.generics.is_empty() || t.variant_fields.is_some() {
        return None;
    }
    let head = t.path[0].as_str();
    if matches!(
        head,
        "Int" | "Float" | "Bool" | "Null" | "String" | "List" | "Option" | "Result"
    ) {
        return None;
    }
    let def = resolver.resolve(head)?;
    let mut stack: Vec<&str> = Vec::new();
    let schema = canonical_schema_from_def(def, resolver, &mut stack, t.range).ok()?;
    Some(TypeRepr::Schema {
        schema: Box::new(schema),
    })
}

/// Synthesise the [`MAIN_RETURN_SCHEMA_NAME`] canonical schema with a
/// single `value` field carrying the declared return type.
///
/// Phase 3.a widens the return surface to `String` / `List<Int>`
/// alongside the v1 scalars. The codegen pass copies the tail-area
/// record bytes into `out_buf` at a `$tail_cursor` past the fixed
/// area; the fixed-area pointer slot stores a buffer-relative
/// offset so the host's `BufferReader` can decode it uniformly.
fn build_main_return_schema(sig: &MainSignature) -> Result<Schema, LoweringError> {
    let rt = sig
        .return_type
        .as_ref()
        .ok_or_else(|| LoweringError::UnsupportedTypeInMain {
            type_name: "<missing>".to_string(),
            range: sig.range,
        })?;
    let ty = type_node_to_canonical(rt).ok_or_else(|| LoweringError::UnsupportedTypeInMain {
        type_name: type_head_for_display(rt),
        range: rt.range,
    })?;
    Ok(Schema {
        name: MAIN_RETURN_SCHEMA_NAME.to_string(),
        generics: vec![],
        fields: vec![Field {
            name: RETURN_VALUE_FIELD_NAME.to_string(),
            ty,
            default: None,
        }],
    })
}

/// Map a parser [`TypeNode`] to a canonical [`TypeRepr`].
///
/// Phase 2.c surface:
///   * `Int` / `Float` / `Bool` / `Null` — the v1 scalar leaves.
///   * `String` — pointer-indirect leaf.
///   * `List<Int>` — pointer-indirect leaf with i64 elements. Other
///     list element types still return `None` so the schema build
///     rejects them with `UnsupportedTypeInMain`.
fn type_node_to_canonical(t: &TypeNode) -> Option<TypeRepr> {
    if t.path.len() != 1 || t.variant_fields.is_some() {
        return None;
    }
    let head = t.path[0].as_str();
    match (head, t.generics.as_slice()) {
        ("Int", []) => Some(TypeRepr::Int),
        ("Float", []) => Some(TypeRepr::Float),
        ("Bool", []) => Some(TypeRepr::Bool),
        ("Null", []) => Some(TypeRepr::Null),
        ("String", []) => Some(TypeRepr::String),
        ("List", [elem]) => {
            // Phase 10-c: `List<T>` with T in `{Int, Float, Bool,
            // String}` is supported via the matching IR / layout
            // paths. Nested lists / Option / Result still reject.
            let inner = type_node_to_canonical(elem)?;
            if matches!(
                inner,
                TypeRepr::Int | TypeRepr::Float | TypeRepr::Bool | TypeRepr::String
            ) {
                Some(TypeRepr::List {
                    element: Box::new(inner),
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Map a canonical [`TypeRepr`] to the matching [`IrType`]. Used both
/// when building the local index (so `Variable` references know their
/// type) and when synthesising the trailing `StoreField`.
fn type_repr_to_ir_type(t: &TypeRepr) -> Result<IrType, LoweringError> {
    match t {
        TypeRepr::Int => Ok(IrType::I64),
        TypeRepr::Float => Ok(IrType::F64),
        TypeRepr::Bool => Ok(IrType::Bool),
        TypeRepr::Null => Ok(IrType::Null),
        TypeRepr::String => Ok(IrType::String),
        TypeRepr::List { element } => match element.as_ref() {
            TypeRepr::Int => Ok(IrType::ListInt),
            TypeRepr::Float => Ok(IrType::ListFloat),
            TypeRepr::Bool => Ok(IrType::ListBool),
            TypeRepr::String => Ok(IrType::ListString),
            TypeRepr::Schema { .. } => Ok(IrType::ListSchema),
            _ => Err(LoweringError::UnsupportedTypeInMain {
                type_name: format!("{t:?}"),
                range: TokenRange::default(),
            }),
        },
        // Composite / list-of-other types are rejected upstream
        // during schema build; this branch fires only for a directly
        // hand-crafted IR.
        _ => Err(LoweringError::UnsupportedTypeInMain {
            type_name: format!("{t:?}"),
            range: TokenRange::default(),
        }),
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
                return Err(LoweringError::UnsupportedExpr {
                    kind: "List(empty literal)".to_string(),
                    range,
                });
            }
            // Detect the shape from the first element.
            let kind = match &*items[0].expr {
                Expr::Int(_) => ConstListKind::Int,
                Expr::Float(_) => ConstListKind::Float,
                Expr::Bool(_) => ConstListKind::Bool,
                Expr::String(_) => ConstListKind::String,
                other => {
                    return Err(LoweringError::UnsupportedExpr {
                        kind: format!("List(non-literal element `{}`)", other.kind()),
                        range: items[0].range,
                    });
                }
            };
            match kind {
                ConstListKind::Int => {
                    let mut elements: Vec<i64> = Vec::with_capacity(items.len());
                    for node in items {
                        match &*node.expr {
                            Expr::Int(v) => elements.push(*v),
                            _ => {
                                return Err(LoweringError::UnsupportedExpr {
                                    kind: format!(
                                        "List<Int>(non-Int element `{}`)",
                                        node.expr.kind()
                                    ),
                                    range: node.range,
                                });
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
                                return Err(LoweringError::UnsupportedExpr {
                                    kind: format!(
                                        "List<Float>(non-Float element `{}`)",
                                        node.expr.kind()
                                    ),
                                    range: node.range,
                                });
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
                                return Err(LoweringError::UnsupportedExpr {
                                    kind: format!(
                                        "List<Bool>(non-Bool element `{}`)",
                                        node.expr.kind()
                                    ),
                                    range: node.range,
                                });
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
                                return Err(LoweringError::UnsupportedExpr {
                                    kind: format!(
                                        "List<String>(non-String element `{}`)",
                                        node.expr.kind()
                                    ),
                                    range: node.range,
                                });
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
        Expr::Binary(op, lhs, rhs) => lower_binary(*op, lhs, rhs, range, ctx),
        Expr::Ternary { cond, then, els } => lower_ternary(cond, then, els, range, ctx),
        Expr::Where { expr, bindings } => lower_where(expr, bindings, range, ctx),
        Expr::FnCall { path, args } => lower_fn_call(path, args, range, ctx),
        Expr::Closure { .. } => Err(LoweringError::ClosureAcrossBoundary {
            context: "closure used in a non-higher-order position".to_string(),
            range,
        }),
        _ => Err(LoweringError::UnsupportedExpr {
            kind: expr.kind().to_string(),
            range,
        }),
    }
}

// =====================================================================
// Phase 10-a: closure-conversion helpers.
// =====================================================================

/// Walk a lambda's body expression and collect identifiers that
/// reference a name not bound by the lambda's own param list. The
/// scan is heuristic — it counts every bare-identifier head segment
/// once and treats it as a potential free variable; spurious entries
/// (names that don't actually resolve in the enclosing scope) are
/// filtered out later by [`resolve_capture`]. The lambda's own params
/// are excluded so they don't pollute the captures list.
fn collect_free_vars(expr: &Expr, lambda_params: &[ClosureParam]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut visit = |s: &str| {
        if lambda_params.iter().any(|p| p.name == s) {
            return;
        }
        if seen.insert(s.to_string()) {
            found.push(s.to_string());
        }
    };
    fn walk_expr(expr: &Expr, lambda_params: &[ClosureParam], visit: &mut dyn FnMut(&str)) {
        match expr {
            Expr::Variable(path) | Expr::Reference { path, .. } => {
                if let Some(TokenKey::String(name, _, _)) = path.first() {
                    visit(name);
                }
            }
            Expr::Binary(_, a, b) => {
                walk_expr(&a.expr, lambda_params, visit);
                walk_expr(&b.expr, lambda_params, visit);
            }
            Expr::Unary(_, n) => walk_expr(&n.expr, lambda_params, visit),
            Expr::Ternary { cond, then, els } => {
                walk_expr(&cond.expr, lambda_params, visit);
                walk_expr(&then.expr, lambda_params, visit);
                walk_expr(&els.expr, lambda_params, visit);
            }
            Expr::List(items) => {
                for n in items {
                    walk_expr(&n.expr, lambda_params, visit);
                }
            }
            Expr::Dict(pairs) => {
                for (_, v) in pairs {
                    walk_expr(&v.expr, lambda_params, visit);
                }
            }
            Expr::Where { expr, bindings } => {
                walk_expr(&bindings.expr, lambda_params, visit);
                walk_expr(&expr.expr, lambda_params, visit);
            }
            Expr::FnCall { path, args } => {
                // Method-call form (`xs.length()`) carries the
                // receiver in the path's leading segments; treat the
                // head segment as a potential free var.
                if let Some(TokenKey::String(name, _, _)) = path.first() {
                    if path.len() > 1 {
                        visit(name);
                    }
                }
                for a in args {
                    walk_expr(&a.value.expr, lambda_params, visit);
                }
            }
            Expr::Closure { body, params, .. } => {
                // Nested lambda: the inner lambda's own params shadow
                // outer ones, but anything else escapes.
                let mut combined: Vec<ClosureParam> = lambda_params.to_vec();
                combined.extend(params.iter().cloned());
                walk_expr(&body.expr, &combined, visit);
            }
            _ => {}
        }
    }
    walk_expr(expr, lambda_params, &mut visit);
    found
}

/// Find a name in the enclosing scope and return its `(IrType,
/// outer_let_idx)`. If the binding is currently a `#main` /
/// method-param (i.e. not yet in a let-local), the helper materialises
/// a fresh let-local, emits a `LetSet` that captures the value, and
/// returns the new index — so a captured `#main` param participates
/// in the closure capture protocol just like any user-let.
fn resolve_capture(
    name: &str,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<(IrType, u32)>, LoweringError> {
    // Innermost-first: let-bindings shadow params.
    if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == name).cloned() {
        return Ok(Some((b.ty, b.idx)));
    }
    // Method params / `#main` params — lift the value into a fresh
    // let-local so the capture protocol has a uniform source.
    if let Some(p) = ctx.method_params.iter().find(|p| p.name == name).cloned() {
        ctx.out.push(TaggedOp {
            op: Op::LocalGet(p.wasm_local_idx),
            range,
        });
        let idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetSet { idx, ty: p.ty },
            range,
        });
        ctx.lets.push(LetBinding {
            name: name.to_string(),
            idx,
            ty: p.ty,
            schema_brand: None,
        });
        return Ok(Some((p.ty, idx)));
    }
    if let Some(p) = ctx.params.iter().find(|p| p.name == name).cloned() {
        // For scalar / pointer params, emit a `LoadField` + `LetSet`.
        // Schema-typed `#main` params are intentionally NOT captureable
        // by Phase 10-a — closure values cannot carry the analyzer's
        // brand machinery yet.
        if p.schema_brand.is_some() {
            return Err(LoweringError::UnsupportedClosureCapture {
                name: name.to_string(),
                ty: p.ty,
                range,
            });
        }
        // Use the matching load shape for the param's IR type.
        let load_op = match p.ty {
            IrType::String => Op::LoadStringPtr { offset: p.offset },
            IrType::ListInt => Op::LoadListIntPtr { offset: p.offset },
            IrType::ListFloat => Op::LoadListFloatPtr { offset: p.offset },
            IrType::ListBool => Op::LoadListBoolPtr { offset: p.offset },
            IrType::ListString => Op::LoadListStringPtr { offset: p.offset },
            IrType::ListSchema => Op::LoadListSchemaPtr { offset: p.offset },
            other => Op::LoadField {
                offset: p.offset,
                ty: other,
            },
        };
        ctx.out.push(TaggedOp { op: load_op, range });
        let idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetSet { idx, ty: p.ty },
            range,
        });
        ctx.lets.push(LetBinding {
            name: name.to_string(),
            idx,
            ty: p.ty,
            schema_brand: None,
        });
        return Ok(Some((p.ty, idx)));
    }
    // Not in scope — the name might refer to a parser-level construct
    // (e.g. a stdlib free-call head, a schema name) that doesn't
    // contribute a capture. Let the lambda body's own
    // `lower_variable` handle the diagnostic later.
    Ok(None)
}

/// Lay out a captures struct: place 8-aligned fields first, then 4-/
/// 1-byte fields. Returns the per-field byte offsets in the same
/// order as the input plus the total struct size (rounded up to 8).
fn layout_captures(captures: &[(String, IrType, u32)]) -> (Vec<u32>, u32) {
    // Two passes: 8-byte slots, then everything else. Keeps the
    // total size aligned at 8 without complex packing logic.
    let mut offsets = vec![0u32; captures.len()];
    let mut cursor: u32 = 0;
    for (i, (_, ty, _)) in captures.iter().enumerate() {
        if matches!(ty.wasm_slot(), IrType::I64 | IrType::F64)
            || matches!(ty, IrType::I64 | IrType::F64)
        {
            offsets[i] = cursor;
            cursor += 8;
        }
    }
    for (i, (_, ty, _)) in captures.iter().enumerate() {
        if !matches!(ty, IrType::I64 | IrType::F64) {
            offsets[i] = cursor;
            cursor += 4;
        }
    }
    // Round up the total size to 8 so the next scratch alloc starts
    // at an 8-aligned boundary.
    let total = (cursor + 7) & !7u32;
    (offsets, total)
}

/// Phase 10-a: lower one [`Expr::Closure`] argument and emit a
/// `MakeClosure` op leaving an `IrType::Closure` value on top of the
/// vstack. The lambda's body becomes a fresh `Func` appended to
/// `ctx.lambda_funcs`; its wasm-side function index is communicated
/// to `MakeClosure` via the closure-table slot `lambda_funcs.len() - 1`.
///
/// `expected_param_tys` and `expected_ret_ty` describe the surface
/// the higher-order caller (stdlib `list_int_map` / `filter` /
/// `fold`) expects from the closure body. Mismatches between these
/// and the inferred body type surface as
/// [`LoweringError::UnsupportedExpr`] — the lambda surface in this
/// phase is closed to user-defined higher-order shapes, so we keep
/// the diagnostics terse.
fn lower_closure_arg(
    closure_expr: &Expr,
    closure_range: TokenRange,
    expected_param_tys: &[IrType],
    expected_ret_ty: IrType,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let Expr::Closure {
        params: lambda_params,
        body: lambda_body,
        ..
    } = closure_expr
    else {
        return Err(LoweringError::UnsupportedExpr {
            kind: format!("lower_closure_arg(non-closure `{}`)", closure_expr.kind()),
            range: closure_range,
        });
    };
    if lambda_params.len() != expected_param_tys.len() {
        return Err(LoweringError::UnsupportedExpr {
            kind: format!(
                "Closure(arity-mismatch: expected {}, got {})",
                expected_param_tys.len(),
                lambda_params.len()
            ),
            range: closure_range,
        });
    }

    // -----------------------------------------------------------------
    // Free-var analysis + capture resolution.
    // -----------------------------------------------------------------
    let free_vars = collect_free_vars(&lambda_body.expr, lambda_params);
    let mut resolved: Vec<(String, IrType, u32)> = Vec::new();
    for name in free_vars {
        if let Some((ty, outer_idx)) = resolve_capture(&name, lambda_body.range, ctx)? {
            resolved.push((name, ty, outer_idx));
        }
    }
    let (offsets, captures_size) = layout_captures(&resolved);
    let captures: Vec<ClosureCapture> = resolved
        .iter()
        .zip(offsets.iter())
        .map(|((_, ty, let_idx), offset)| ClosureCapture {
            let_idx: *let_idx,
            ty: *ty,
            offset: *offset,
        })
        .collect();

    // -----------------------------------------------------------------
    // Build the lambda Func.
    //
    // Signature: `(captures_ptr: i32, ...lambda_params) -> ret_ty`.
    // Body prologue: for each capture, emit `LocalGet(0);
    // LoadXxxAtAbsolute { offset }; LetSet { fresh_idx, ty }`. The
    // lambda body's lowering ctx sees each capture as a let-binding
    // under its source-level name; the body lowers normally.
    // -----------------------------------------------------------------
    let mut lambda_param_tys: Vec<IrType> = Vec::with_capacity(1 + expected_param_tys.len());
    lambda_param_tys.push(IrType::I32);
    lambda_param_tys.extend(expected_param_tys.iter().copied());

    // Use a fresh LowerCtx — captures + lambda params become its let
    // bindings. Cloning the schema resolver / method registry is a
    // cheap re-use of the outer-side shared maps; the inner walk
    // never mutates them. #151 — share the outer ctx's intern handle
    // so any literal lowered inside the lambda body participates in
    // the module-wide dedup table.
    const EMPTY_PARAMS: &[LocalBinding] = &[];
    let mut inner = LowerCtx::new(
        EMPTY_PARAMS,
        ctx.schema_resolver.clone(),
        ctx.method_registry.clone(),
        ctx.intern_handle(),
    );

    // Prologue: load each capture into a fresh inner let-local.
    let mut inner_let_idx: u32 = 0;
    for ((name, ty, _outer_idx), offset) in resolved.iter().zip(offsets.iter()) {
        // Push captures_ptr (local 0), then emit the type-driven load
        // followed by a LetSet under the source-level name.
        inner.out.push(TaggedOp {
            op: Op::LocalGet(0),
            range: lambda_body.range,
        });
        match ty {
            IrType::I64 => inner.out.push(TaggedOp {
                op: Op::LoadI64AtAbsolute { offset: *offset },
                range: lambda_body.range,
            }),
            IrType::F64 => inner.out.push(TaggedOp {
                op: Op::LoadF64AtAbsolute { offset: *offset },
                range: lambda_body.range,
            }),
            IrType::Bool => inner.out.push(TaggedOp {
                op: Op::LoadI8UAtAbsolute { offset: *offset },
                range: lambda_body.range,
            }),
            IrType::I32
            | IrType::Null
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::Closure => inner.out.push(TaggedOp {
                op: Op::LoadI32AtAbsolute { offset: *offset },
                range: lambda_body.range,
            }),
        }
        inner.out.push(TaggedOp {
            op: Op::LetSet {
                idx: inner_let_idx,
                ty: *ty,
            },
            range: lambda_body.range,
        });
        inner.lets.push(LetBinding {
            name: name.clone(),
            idx: inner_let_idx,
            ty: *ty,
            schema_brand: None,
        });
        inner_let_idx += 1;
    }
    // Lambda's own params: stash each wasm local into a let-local
    // bound under the source-level name so the body's
    // `lower_variable` lookup just works.
    for (i, lp) in lambda_params.iter().enumerate() {
        let wasm_local_idx = (i + 1) as u32;
        let ty = expected_param_tys[i];
        inner.out.push(TaggedOp {
            op: Op::LocalGet(wasm_local_idx),
            range: lambda_body.range,
        });
        inner.out.push(TaggedOp {
            op: Op::LetSet {
                idx: inner_let_idx,
                ty,
            },
            range: lambda_body.range,
        });
        inner.lets.push(LetBinding {
            name: lp.name.clone(),
            idx: inner_let_idx,
            ty,
            schema_brand: None,
        });
        inner_let_idx += 1;
    }
    inner.next_let_idx = inner_let_idx;

    // Body lowering.
    lower_expr(&lambda_body.expr, lambda_body.range, &mut inner)?;
    let body_ty = inner
        .tstack
        .last()
        .copied()
        .ok_or_else(|| LoweringError::UnsupportedExpr {
            kind: "Closure(empty-body-stack)".to_string(),
            range: lambda_body.range,
        })?;
    if body_ty.wasm_slot() != expected_ret_ty.wasm_slot() {
        return Err(LoweringError::StdlibArgTypeMismatch {
            name: "closure-return".to_string(),
            arg_idx: 0,
            got: body_ty,
            expected: expected_ret_ty,
            range: lambda_body.range,
        });
    }
    inner.out.push(TaggedOp {
        op: Op::Return,
        range: lambda_body.range,
    });

    // Nested lambdas inside `inner` would land in `inner.lambda_funcs`;
    // append them after the outer body so the closure table stays
    // contiguous. (Phase 10-a doesn't surface nested lambdas through
    // the user-facing stdlib calls, but the recursion stays sound.)
    let nested_lambdas = std::mem::take(&mut inner.lambda_funcs);

    // -----------------------------------------------------------------
    // Outer-side: emit the MakeClosure op. The closure-table slot is
    // determined by the lambda's position in `ctx.lambda_funcs` —
    // appending below makes the slot a deterministic index from
    // source order.
    // -----------------------------------------------------------------
    let fn_table_idx = ctx.lambda_funcs.len() as u32;
    let lambda_func = Func {
        name: format!("__closure_{}", fn_table_idx),
        params: lambda_param_tys,
        ret: expected_ret_ty,
        body: inner.out,
        range: closure_range,
    };
    ctx.lambda_funcs.push(lambda_func);
    // Append nested lambdas (if any) immediately after — they'll get
    // table slots `fn_table_idx + 1..N`.
    ctx.lambda_funcs.extend(nested_lambdas);

    ctx.out.push(TaggedOp {
        op: Op::MakeClosure {
            fn_table_idx,
            captures,
            captures_size,
        },
        range: closure_range,
    });
    ctx.tstack.push(IrType::Closure);
    Ok(())
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
fn lower_fn_call(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if path.is_empty() {
        return Err(LoweringError::UnsupportedExpr {
            kind: "FnCall(empty-path)".to_string(),
            range,
        });
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
    if let Some(()) = try_lower_range_chain_reduce(path, args, range, ctx)? {
        return Ok(());
    }
    // Final path segment is the method / function name. Earlier
    // segments either form the receiver (method-call form) or are
    // unused (free-call form has exactly one path segment).
    let method_name = match path.last().unwrap() {
        TokenKey::String(name, _, _) => name.as_str(),
        _ => {
            return Err(LoweringError::UnsupportedExpr {
                kind: "FnCall(non-string-tail-segment)".to_string(),
                range,
            });
        }
    };
    let receiver_segments = &path[..path.len() - 1];
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
            return Err(LoweringError::UnknownStdlibMethod {
                name: method_name.to_string(),
                arity,
                range,
            });
        };
        let stdlib_meta = builtin_stdlib().get(fn_index as usize).ok_or_else(|| {
            LoweringError::UnknownStdlibMethod {
                name: method_name.to_string(),
                arity,
                range,
            }
        })?;
        if (stdlib_meta.params.len() as u32) != arity {
            return Err(LoweringError::UnknownStdlibMethod {
                name: method_name.to_string(),
                arity,
                range,
            });
        }
        for (i, call_arg) in args.iter().enumerate() {
            if call_arg.name.is_some() {
                return Err(LoweringError::UnsupportedExpr {
                    kind: format!("FnCall(named-arg `{}` for stdlib)", method_name),
                    range,
                });
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
    let receiver_ty = *ctx.tstack.last().ok_or(LoweringError::UnsupportedExpr {
        kind: format!("FnCall(receiver-stack-empty for `{}`)", method_name),
        range,
    })?;
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
    let Some(fn_index) = stdlib_method_index(receiver_ty, method_name) else {
        return Err(LoweringError::UnknownStdlibMethod {
            name: method_name.to_string(),
            arity,
            range,
        });
    };
    let stdlib_meta = builtin_stdlib().get(fn_index as usize).ok_or_else(|| {
        LoweringError::UnknownStdlibMethod {
            name: method_name.to_string(),
            arity,
            range,
        }
    })?;
    if (stdlib_meta.params.len() as u32) != arity {
        return Err(LoweringError::UnknownStdlibMethod {
            name: method_name.to_string(),
            arity,
            range,
        });
    }
    // The receiver already sits on the vstack — re-check that its
    // slot matches the callee's declared param[0] so a future
    // dispatch entry that mistypes its receiver surfaces at lowering
    // rather than at codegen.
    let pushed_receiver = ctx.tstack.pop().ok_or(LoweringError::UnsupportedExpr {
        kind: format!("FnCall(receiver-stack-empty for `{}`)", method_name),
        range,
    })?;
    check_stdlib_arg(method_name, 0, pushed_receiver, &stdlib_meta.params, range)?;
    ctx.tstack.push(pushed_receiver);
    for (i, call_arg) in args.iter().enumerate() {
        if call_arg.name.is_some() {
            return Err(LoweringError::UnsupportedExpr {
                kind: format!("FnCall(named-arg `{}` for stdlib)", method_name),
                range,
            });
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

/// review-improvement-160 bytecode M3 phase 2: recognise the
/// `list.sum(range(...))` peephole at the receiver/method/inner-call
/// level and emit an explicit `Op::Loop` accumulator.  Returns
/// `Ok(Some(()))` when the desugar fired (leaves a single `I64` on the
/// vstack), `Ok(None)` when the pattern did not match (caller falls
/// through to the normal lowering path), or `Err` when the inner
/// argument expressions themselves fail to lower.
///
/// Pattern shapes accepted (Open follow-up #2 — IR lowering surface
/// expansion):
///   * `list.sum(range(end))` — `start = 0`.
///   * `list.sum(range(start, end))`.
///   * `list.sum(range(...).map((p) => <body_i64>))` — inlines the
///     closure body per iteration; the body's i64 result is added to
///     the running accumulator. Captures into the body are limited to
///     in-scope let-bindings / `#main` params already reachable from
///     the outer ctx (no heap-style capture frame; the body is
///     emitted directly into the outer ctx's op stream so its
///     captures resolve through the same let-table walk the top-level
///     body uses).
///
/// All inner args must be plain positional (no keyword form); the
/// inner `range` call must use the bare identifier head (no dotted
/// receiver).  Anything else falls through to the default path so a
/// future `list.sum(my_user_fn())` keeps the existing diagnostic.
fn try_lower_list_sum_range(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    // Outer head must be `list.sum(<single positional arg>)`.
    if path.len() != 2 {
        return Ok(None);
    }
    let outer_head = matches!(&path[0], TokenKey::String(s, _, _) if s == "list");
    let outer_method = matches!(&path[1], TokenKey::String(s, _, _) if s == "sum");
    if !(outer_head && outer_method) {
        return Ok(None);
    }
    if args.len() != 1 || args[0].name.is_some() {
        return Ok(None);
    }

    // Walk the chain rooted at `range(...)`, peeling off
    // recognized `.map((p) => <body>)` / `.filter((p) => <body>)`
    // invocations.  Returns the base range_args plus the per-stage
    // closure adaptor list; otherwise falls through.
    let Some(chain) = match_range_chain(&args[0].value.expr) else {
        return Ok(None);
    };

    emit_range_pipeline_loop(&chain, RangeConsumer::SumI64, range, ctx)?;
    Ok(Some(()))
}

/// Open follow-up #2 companion to `try_lower_list_sum_range`: recognise
/// `range(...)[ . map(c) | . filter(c) ]*.len()` and emit a pure i64
/// count accumulator. Returns `Ok(Some(()))` on a successful desugar
/// (vstack carries one `I64` after return), `Ok(None)` when the
/// pattern didn't match (caller falls through to default dispatch),
/// `Err` when an inner expression failed to lower.
///
/// The W4 cmp_lua workload (`range(n).map((i) => "axb")
/// .filter((s) => s.contains("x")).len()`) is the canonical caller.
/// Without this peephole the call would resolve `range` as an unknown
/// stdlib method (`range` is a tree-walker host-fn, not an IR stdlib
/// entry), or — if range were promoted — the filter / map pipeline
/// would materialise a transient `List<String>` the bytecode scalar
/// envelope rejects. The peephole side-steps both by emitting the
/// equivalent scalar loop directly.
fn try_lower_range_chain_len(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    // Outer call must be `<receiver>.len()` with no args.
    if path.len() != 2 || !args.is_empty() {
        return Ok(None);
    }
    let TokenKey::Dynamic(receiver_node, _) = &path[0] else {
        return Ok(None);
    };
    let TokenKey::String(method_name, _, _) = &path[1] else {
        return Ok(None);
    };
    if method_name.as_str() != "len" {
        return Ok(None);
    }
    let Some(chain) = match_range_chain(&receiver_node.expr) else {
        return Ok(None);
    };
    emit_range_pipeline_loop(&chain, RangeConsumer::Len, range, ctx)?;
    Ok(Some(()))
}

/// Open follow-up #2 companion to `try_lower_list_sum_range` /
/// `try_lower_range_chain_len`: recognise `<chain>.reduce(<init>,
/// (acc, elem) => body)` and emit a per-iteration accumulator update
/// driven by the user's body. Returns `Ok(Some(()))` on a successful
/// desugar (vstack carries the accumulator's type after return),
/// `Ok(None)` when the pattern didn't match, `Err` when an inner
/// expression failed to lower.
///
/// The cmp_lua W3 workload (`range(n).map((i) => "a").reduce("",
/// (acc, s) => acc + s)`) is the canonical caller — its string-concat
/// reduce returns a `String` accumulator the bytecode VM accepts via
/// the B-1 / B-2 string-arena infrastructure.
fn try_lower_range_chain_reduce(
    path: &[TokenKey],
    args: &[relon_parser::CallArg],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<Option<()>, LoweringError> {
    // Outer call shape: `<receiver>.reduce(<init>, <closure>)` — two
    // positional args, second is the closure literal.
    if path.len() != 2 || args.len() != 2 {
        return Ok(None);
    }
    if args.iter().any(|a| a.name.is_some()) {
        return Ok(None);
    }
    let TokenKey::Dynamic(receiver_node, _) = &path[0] else {
        return Ok(None);
    };
    let TokenKey::String(method_name, _, _) = &path[1] else {
        return Ok(None);
    };
    if method_name.as_str() != "reduce" {
        return Ok(None);
    }
    let init_node = &args[0].value;
    let Expr::Closure {
        params,
        body,
        return_type: _,
    } = &*args[1].value.expr
    else {
        return Ok(None);
    };
    if params.len() != 2 {
        return Ok(None);
    }
    let Some(chain) = match_range_chain(&receiver_node.expr) else {
        return Ok(None);
    };
    emit_range_pipeline_loop(
        &chain,
        RangeConsumer::Reduce {
            init: init_node,
            params: params.as_slice(),
            body,
        },
        range,
        ctx,
    )?;
    Ok(Some(()))
}

/// One stage in a `range(...).chain(...)` pipeline. Each stage takes
/// the running per-iteration value (initially the loop counter `i`)
/// and produces a new value of the recorded `result_ty`.
#[derive(Debug, Clone)]
struct ChainStage<'a> {
    /// Surface method name (`map` / `filter` for now). Pinned so
    /// downstream loop emitters know what control-flow shape to
    /// produce.
    method: &'static str,
    /// Closure literal supplied as the method's single positional arg.
    /// Borrowed straight off the parser AST so the loop emitter can
    /// inline it into the outer ctx's op stream.
    closure_params: &'a [ClosureParam],
    closure_body: &'a Node,
}

/// Decomposition of a `range(...)[. <method>(<closure>)]*` chain. The
/// `stages` vec walks innermost-to-outermost — `range(...).map(f).map(g)`
/// produces stages `[(map, f), (map, g)]` so the loop emitter can
/// pipeline them in source order.
#[derive(Debug)]
struct RangeChain<'a> {
    range_args: &'a [relon_parser::CallArg],
    stages: Vec<ChainStage<'a>>,
}

/// Recognise a `range(...)[ . map((p) => body) ]*` pipeline.
///
/// Parser shape (cf. `relon-parser` token AST):
///   `range(n).map(f).filter(g)` →
///     `FnCall {
///        path: [Dynamic(FnCall {
///                 path: [Dynamic(FnCall { path: [String("range")], args: [n] }),
///                        String("map")],
///                 args: [f] }),
///               String("filter")],
///        args: [g] }`
///
/// Each method call wraps its receiver as a `TokenKey::Dynamic(Node)`
/// at the head of the `path` slice, with the remaining segment being
/// the method name. This function walks that nesting innermost-out,
/// peeling each recognised method off until it hits the bare
/// `range(...)` call (terminal). Anything outside the recognised
/// shape returns `None` so the caller falls through to the regular
/// lowering path.
fn match_range_chain(expr: &Expr) -> Option<RangeChain<'_>> {
    let mut stages: Vec<ChainStage<'_>> = Vec::new();
    let mut current: &Expr = expr;
    loop {
        let Expr::FnCall { path, args } = current else {
            return None;
        };
        // Bare `range(...)` — terminal case. Validate arity + reject
        // keyword args; the parent `try_lower_list_sum_range` does the
        // same for the 0-stage form, but we repeat the check here so
        // a multi-stage chain bottoms out cleanly.
        if path.len() == 1
            && matches!(&path[0], TokenKey::String(s, _, _) if s == "range")
        {
            if args.is_empty() || args.len() > 2 {
                return None;
            }
            if args.iter().any(|a| a.name.is_some()) {
                return None;
            }
            // Stages were pushed outermost-first; reverse so the
            // emitter walks innermost-first (source order).
            stages.reverse();
            return Some(RangeChain {
                range_args: args,
                stages,
            });
        }
        // Recognised chain step has exactly 2 path segments:
        //   [Dynamic(receiver_call), String("<method_name>")]
        // and exactly one positional closure arg.
        if path.len() != 2 {
            return None;
        }
        let TokenKey::Dynamic(receiver_node, _) = &path[0] else {
            return None;
        };
        let TokenKey::String(method_name, _, _) = &path[1] else {
            return None;
        };
        let method: &'static str = match method_name.as_str() {
            "map" => "map",
            // Open follow-up #2: `filter` lets the W4-shape
            // (`range(n).map(c1).filter(c2).len()`) collapse onto the
            // same accumulator-loop skeleton. The filter's predicate
            // body lowers to a Bool atop the vstack; the emitter walks
            // an inner `block { ... }` so a false predicate jumps past
            // the consumer's accumulator update.
            "filter" => "filter",
            _ => return None,
        };
        if args.len() != 1 || args[0].name.is_some() {
            return None;
        }
        let Expr::Closure {
            params,
            body,
            return_type: _,
        } = &*args[0].value.expr
        else {
            return None;
        };
        if params.len() != 1 {
            return None;
        }
        stages.push(ChainStage {
            method,
            closure_params: params.as_slice(),
            closure_body: body,
        });
        // Descend into the receiver (the inner FnCall wrapped in
        // Dynamic) and continue peeling.
        current = &receiver_node.expr;
    }
}

/// Consumer terminating a `range(...)` pipeline. Selects how the
/// per-iteration value (after the final map/filter stage) folds into
/// the loop's accumulator.
#[derive(Debug, Clone, Copy)]
enum RangeConsumer<'a> {
    /// `list.sum(<chain>)` — accumulator += element (element must be
    /// `I64`).
    SumI64,
    /// `<chain>.len()` — accumulator += 1 per surviving iteration
    /// (element type is irrelevant; only the filter outcome matters).
    Len,
    /// `<chain>.reduce(<init>, (acc, elem) => body)` — init the
    /// accumulator from a lowered expression, then update it per
    /// iteration via the supplied 2-arg closure body.
    Reduce {
        /// Init expression's AST node — lowered against the outer
        /// ctx before the loop opens so its captures resolve through
        /// the normal walker.
        init: &'a Node,
        /// Closure params: [acc_name, elem_name].
        params: &'a [ClosureParam],
        /// Closure body expression.
        body: &'a Node,
    },
}

/// Emit the pure-i64 accumulator loop that implements one of the
/// recognised `range(start, end)[. map(...) | . filter(...)]*` chain
/// consumers. Pre-condition: the caller has already matched the
/// chain via [`match_range_chain`]. The emitter walks the stages
/// in source order, lowering each closure body inline against the
/// outer ctx so captures resolve through the normal walker.
///
/// Control-flow shape (the inner block lets `filter` short-circuit
/// the consumer update without breaking out of the loop):
///
/// ```text
/// block (loop-exit) {
///   loop {
///     if start >= end { br 1 }      // exit the loop-exit block
///     block (next-iter) {
///       <stage 0 emit>
///       <stage 1 emit>              // filter stages emit `br 0` on
///       ...                         // false to skip the consumer
///       <consumer update>
///     }
///     start += 1
///     br 0                          // back to loop header
///   }
/// }
/// push acc
/// ```
///
/// Label depths (counted from innermost out):
///   * inside `next-iter` block: 0 → next-iter, 1 → loop, 2 → loop-exit
///   * inside `loop` body but outside `next-iter`: 0 → loop, 1 → loop-exit
fn emit_range_pipeline_loop(
    chain: &RangeChain<'_>,
    consumer: RangeConsumer<'_>,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let range_args = chain.range_args;
    // The accumulator type depends on the consumer: SumI64 / Len use
    // i64; Reduce derives it from the init expression's lowered type.
    // Stash the determined type so we can re-emit `LetGet` ops later
    // and also so the post-loop push matches.
    let start_i = ctx.next_let_idx;
    let end_i = ctx.next_let_idx + 1;
    let acc_i = ctx.next_let_idx + 2;
    ctx.next_let_idx += 3;

    // start
    if range_args.len() == 2 {
        lower_expr(&range_args[0].value.expr, range_args[0].value.range, ctx)?;
        expect_int_top(ctx, range)?;
    } else {
        ctx.out.push(TaggedOp {
            op: Op::ConstI64(0),
            range,
        });
        ctx.tstack.push(IrType::I64);
    }
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();
    // end
    let end_arg = &range_args[range_args.len() - 1];
    lower_expr(&end_arg.value.expr, end_arg.value.range, ctx)?;
    expect_int_top(ctx, range)?;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: end_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.pop();
    // acc = <init>. SumI64 / Len init to i64 zero; Reduce lowers the
    // user-supplied init expression and inherits its type.
    let acc_ty = match consumer {
        RangeConsumer::SumI64 | RangeConsumer::Len => {
            ctx.out.push(TaggedOp {
                op: Op::ConstI64(0),
                range,
            });
            ctx.tstack.push(IrType::I64);
            IrType::I64
        }
        RangeConsumer::Reduce { init, .. } => {
            lower_expr(&init.expr, init.range, ctx)?;
            ctx.tstack
                .last()
                .copied()
                .ok_or_else(|| LoweringError::UnsupportedExpr {
                    kind: "range-chain reduce: init expression produced no value".to_string(),
                    range: init.range,
                })?
        }
    };
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: acc_i,
            ty: acc_ty,
        },
        range,
    });
    ctx.tstack.pop();

    // -----------------------------------------------------------------
    // Build the inner loop body.
    //
    // Strategy: temporarily redirect `ctx.out` into a sub-buffer per
    // nested control-flow region so we can splice the resulting vec
    // under the matching Block/Loop op. The op-stream redirect
    // preserves all other ctx fields (let-table, vstack,
    // next_let_idx, intern handle); only `out` is swapped so the
    // closure body's lowering goes into the right region.
    //
    // The sub-buffer dance is needed because the body lowering walks
    // arbitrary user IR — we don't want to drop a hand-rolled
    // Vec<TaggedOp> in the middle of `ctx.out` only to re-shuffle it.
    // -----------------------------------------------------------------

    // Outer loop body sub-buffer.
    let saved_outer = std::mem::take(&mut ctx.out);

    // br_if (start_i >= end_i) -> label_depth 1 (outer loop-exit block)
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: end_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Ge(IrType::I64),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::BrIf { label_depth: 1 },
        range,
    });

    // ----- "next-iter" block (filter short-circuit target) ----------
    let saved_iter = std::mem::take(&mut ctx.out);

    // Walk the stages. `current_value` flows from the loop counter
    // into each successive map's output; filter stages don't change
    // it (they only branch out on false).
    let mut current_value_idx = start_i;
    let mut current_value_ty = IrType::I64;
    for stage in chain.stages.iter() {
        let param = &stage.closure_params[0];
        // Allocate a fresh let-binding under the closure
        // parameter's name so `Variable(p)` inside the body
        // resolves to current_value via the normal walker.
        let param_let_idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: current_value_idx,
                ty: current_value_ty,
            },
            range,
        });
        ctx.out.push(TaggedOp {
            op: Op::LetSet {
                idx: param_let_idx,
                ty: current_value_ty,
            },
            range,
        });
        ctx.lets.push(LetBinding {
            name: param.name.clone(),
            idx: param_let_idx,
            ty: current_value_ty,
            schema_brand: None,
        });
        let body_node = stage.closure_body;
        lower_expr(&body_node.expr, body_node.range, ctx)?;
        let produced_ty = ctx.tstack.last().copied().ok_or_else(|| {
            LoweringError::UnsupportedExpr {
                kind: format!(
                    "range-chain {}: closure body produced no value",
                    stage.method
                ),
                range: body_node.range,
            }
        })?;
        ctx.lets.pop();
        match stage.method {
            "map" => {
                // Stash the body result into a fresh let so further
                // stages (and the consumer) can pick it up.
                let result_let_idx = ctx.next_let_idx;
                ctx.next_let_idx += 1;
                ctx.out.push(TaggedOp {
                    op: Op::LetSet {
                        idx: result_let_idx,
                        ty: produced_ty,
                    },
                    range: body_node.range,
                });
                ctx.tstack.pop();
                current_value_idx = result_let_idx;
                current_value_ty = produced_ty;
            }
            "filter" => {
                if produced_ty != IrType::Bool {
                    // Restore ctx.out to keep the caller's diagnostic
                    // surface from seeing a corrupt op stream.
                    let _ = std::mem::take(&mut ctx.out);
                    ctx.out = saved_iter;
                    let _ = std::mem::take(&mut ctx.out);
                    ctx.out = saved_outer;
                    return Err(LoweringError::UnsupportedExpr {
                        kind: format!(
                            "range-chain filter predicate must return Bool, got {:?}",
                            produced_ty
                        ),
                        range,
                    });
                }
                // The predicate left a Bool on top. Branch to the
                // "next-iter" block on FALSE so the consumer update
                // is skipped. Wasm's `br_if` branches on non-zero —
                // so we invert first via Op::Sub or Op::Eq with 0.
                // Simpler: push 0 and `Eq` to get "predicate==0", then
                // br_if.
                ctx.out.push(TaggedOp {
                    op: Op::ConstBool(false),
                    range,
                });
                ctx.tstack.push(IrType::Bool);
                ctx.out.push(TaggedOp {
                    op: Op::Eq(IrType::Bool),
                    range,
                });
                ctx.tstack.pop();
                ctx.tstack.pop();
                ctx.tstack.push(IrType::Bool);
                ctx.out.push(TaggedOp {
                    op: Op::BrIf { label_depth: 0 },
                    range,
                });
                ctx.tstack.pop();
                // current_value passes through unchanged.
            }
            other => {
                return Err(LoweringError::UnsupportedExpr {
                    kind: format!("range-chain: unsupported method `{}`", other),
                    range,
                });
            }
        }
    }

    // Consumer update.
    match consumer {
        RangeConsumer::SumI64 => {
            // Push the current element and require it i64.
            ctx.out.push(TaggedOp {
                op: Op::LetGet {
                    idx: current_value_idx,
                    ty: current_value_ty,
                },
                range,
            });
            ctx.tstack.push(current_value_ty);
            if current_value_ty != IrType::I64 {
                let _ = std::mem::take(&mut ctx.out);
                ctx.out = saved_iter;
                let _ = std::mem::take(&mut ctx.out);
                ctx.out = saved_outer;
                return Err(LoweringError::UnsupportedExpr {
                    kind: format!(
                        "list.sum(range(...).map(...)) requires Int-valued element; got {:?}",
                        current_value_ty
                    ),
                    range,
                });
            }
            // acc_i += element
            ctx.out.push(TaggedOp {
                op: Op::LetGet {
                    idx: acc_i,
                    ty: IrType::I64,
                },
                range,
            });
            ctx.tstack.push(IrType::I64);
            ctx.out.push(TaggedOp {
                op: Op::Add(IrType::I64),
                range,
            });
            ctx.tstack.pop();
            ctx.tstack.pop();
            ctx.tstack.push(IrType::I64);
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx: acc_i,
                    ty: IrType::I64,
                },
                range,
            });
            ctx.tstack.pop();
        }
        RangeConsumer::Len => {
            // acc_i += 1
            ctx.out.push(TaggedOp {
                op: Op::LetGet {
                    idx: acc_i,
                    ty: IrType::I64,
                },
                range,
            });
            ctx.tstack.push(IrType::I64);
            ctx.out.push(TaggedOp {
                op: Op::ConstI64(1),
                range,
            });
            ctx.tstack.push(IrType::I64);
            ctx.out.push(TaggedOp {
                op: Op::Add(IrType::I64),
                range,
            });
            ctx.tstack.pop();
            ctx.tstack.pop();
            ctx.tstack.push(IrType::I64);
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx: acc_i,
                    ty: IrType::I64,
                },
                range,
            });
            ctx.tstack.pop();
            // Silence the unused-let warning when no map stage flows.
            let _ = current_value_idx;
        }
        RangeConsumer::Reduce { params, body, .. } => {
            // The reduce closure takes (acc, elem). Bind both as
            // transient let-bindings under the closure's parameter
            // names so the body's `Variable(...)` lookups resolve
            // through the normal walker.
            if params.len() != 2 {
                let _ = std::mem::take(&mut ctx.out);
                ctx.out = saved_iter;
                let _ = std::mem::take(&mut ctx.out);
                ctx.out = saved_outer;
                return Err(LoweringError::UnsupportedExpr {
                    kind: format!(
                        "range-chain reduce requires 2-arg closure (acc, elem); got {}",
                        params.len()
                    ),
                    range,
                });
            }
            // Bind acc into a fresh let under params[0].name. Source
            // value: current acc_i contents.
            let acc_param_let = ctx.next_let_idx;
            ctx.next_let_idx += 1;
            ctx.out.push(TaggedOp {
                op: Op::LetGet {
                    idx: acc_i,
                    ty: acc_ty,
                },
                range,
            });
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx: acc_param_let,
                    ty: acc_ty,
                },
                range,
            });
            ctx.lets.push(LetBinding {
                name: params[0].name.clone(),
                idx: acc_param_let,
                ty: acc_ty,
                schema_brand: None,
            });
            // Bind elem into a fresh let under params[1].name. Source
            // value: current_value_idx.
            let elem_param_let = ctx.next_let_idx;
            ctx.next_let_idx += 1;
            ctx.out.push(TaggedOp {
                op: Op::LetGet {
                    idx: current_value_idx,
                    ty: current_value_ty,
                },
                range,
            });
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx: elem_param_let,
                    ty: current_value_ty,
                },
                range,
            });
            ctx.lets.push(LetBinding {
                name: params[1].name.clone(),
                idx: elem_param_let,
                ty: current_value_ty,
                schema_brand: None,
            });
            // Lower the closure body — leaves the new acc value on
            // top of the vstack.
            lower_expr(&body.expr, body.range, ctx)?;
            let produced = ctx.tstack.last().copied().ok_or_else(|| {
                LoweringError::UnsupportedExpr {
                    kind: "range-chain reduce: body produced no value".to_string(),
                    range: body.range,
                }
            })?;
            if produced.wasm_slot() != acc_ty.wasm_slot() {
                let _ = std::mem::take(&mut ctx.out);
                ctx.out = saved_iter;
                let _ = std::mem::take(&mut ctx.out);
                ctx.out = saved_outer;
                return Err(LoweringError::UnsupportedExpr {
                    kind: format!(
                        "range-chain reduce: body returned {:?}, expected init type {:?}",
                        produced, acc_ty
                    ),
                    range: body.range,
                });
            }
            ctx.lets.pop(); // elem
            ctx.lets.pop(); // acc
            ctx.out.push(TaggedOp {
                op: Op::LetSet {
                    idx: acc_i,
                    ty: acc_ty,
                },
                range,
            });
            ctx.tstack.pop();
        }
    }

    // Pop the "next-iter" sub-buffer and splice it under Op::Block.
    let iter_body = std::mem::replace(&mut ctx.out, saved_iter);
    ctx.out.push(TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: iter_body,
        },
        range,
    });

    // start_i += 1; br 0 (back to loop)
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::ConstI64(1),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I64),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: start_i,
            ty: IrType::I64,
        },
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::Br { label_depth: 0 },
        range,
    });

    // Pop the outer loop body and wrap under Block { Loop { ... } }.
    let outer_body = std::mem::replace(&mut ctx.out, saved_outer);
    ctx.out.push(TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: vec![TaggedOp {
                op: Op::Loop {
                    result_ty: None,
                    body: outer_body,
                },
                range,
            }],
        },
        range,
    });
    // Push the accumulator so the consumer sees its final value on
    // top — matches the corresponding `list_int_sum` / `list_int_length`
    // / `list_int_fold` return shape depending on the consumer.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: acc_i,
            ty: acc_ty,
        },
        range,
    });
    ctx.tstack.push(acc_ty);
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
        Some(other) => Err(LoweringError::UnsupportedExpr {
            kind: format!(
                "list.sum(range(...)) desugar requires Int args, got {:?}",
                other
            ),
            range,
        }),
        None => Err(LoweringError::UnsupportedExpr {
            kind: "list.sum(range(...)) desugar saw empty vstack".to_string(),
            range,
        }),
    }
}

/// Phase 10-a: lower one argument to a stdlib call, routing closure
/// expressions through `lower_closure_arg` when the matching param
/// slot is `IrType::Closure`. Validates the resulting IR slot against
/// the callee's declared param type and surfaces a
/// `StdlibArgTypeMismatch` when the slots disagree.
fn lower_stdlib_arg(
    name: &str,
    arg_idx: u32,
    value: &Node,
    param_tys: &[IrType],
    ctx: &mut LowerCtx<'_>,
    call_range: TokenRange,
) -> Result<(), LoweringError> {
    let expected =
        *param_tys
            .get(arg_idx as usize)
            .ok_or_else(|| LoweringError::UnknownStdlibMethod {
                name: name.to_string(),
                arity: param_tys.len() as u32,
                range: call_range,
            })?;
    if expected == IrType::Closure {
        // Closure surface: the value expression must be a literal
        // lambda. Any other shape (a Variable referencing a closure,
        // a stdlib-returned closure) is out of scope for Phase 10-a.
        if let Expr::Closure { .. } = &*value.expr {
            let (param_tys_c, ret_ty_c) =
                stdlib_closure_arg_signature(name, arg_idx).ok_or_else(|| {
                    LoweringError::UnsupportedExpr {
                        kind: format!(
                            "FnCall(`{}`) arg {} is Closure but no signature side-table entry",
                            name, arg_idx
                        ),
                        range: call_range,
                    }
                })?;
            lower_closure_arg(&value.expr, value.range, &param_tys_c, ret_ty_c, ctx)?;
        } else {
            return Err(LoweringError::UnsupportedExpr {
                kind: format!(
                    "FnCall(`{}`) arg {} expected Closure literal, got `{}`",
                    name,
                    arg_idx,
                    value.expr.kind()
                ),
                range: value.range,
            });
        }
    } else {
        lower_expr(&value.expr, value.range, ctx)?;
    }
    let pushed = ctx
        .tstack
        .pop()
        .ok_or_else(|| LoweringError::UnsupportedExpr {
            kind: format!("FnCall(arg{}-stack-empty for `{}`)", arg_idx, name),
            range: call_range,
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
                return Err(LoweringError::UnsupportedExpr {
                    kind: "FnCall(unsupported-receiver-key)".to_string(),
                    range,
                });
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
    Err(LoweringError::UnsupportedExpr {
        kind: format!(
            "FnCall(multi-segment-receiver, segments={})",
            receiver_segments.len()
        ),
        range,
    })
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
        return Err(LoweringError::UnknownStdlibMethod {
            name: method_name.to_string(),
            arity,
            range,
        });
    }
    // Validate the receiver slot against param[0].
    let pushed_receiver = ctx.tstack.pop().ok_or(LoweringError::UnsupportedExpr {
        kind: format!("FnCall(receiver-stack-empty for `{}`)", method_name),
        range,
    })?;
    if pushed_receiver.wasm_slot() != param_tys[0].wasm_slot() {
        return Err(LoweringError::StdlibArgTypeMismatch {
            name: method_name.to_string(),
            arg_idx: 0,
            got: pushed_receiver,
            expected: param_tys[0],
            range,
        });
    }
    ctx.tstack.push(pushed_receiver);
    for (i, call_arg) in args.iter().enumerate() {
        if call_arg.name.is_some() {
            return Err(LoweringError::UnsupportedExpr {
                kind: format!("FnCall(named-arg `{}` for schema method)", method_name),
                range,
            });
        }
        lower_expr(&call_arg.value.expr, call_arg.value.range, ctx)?;
        let pushed = ctx.tstack.pop().ok_or(LoweringError::UnsupportedExpr {
            kind: format!("FnCall(arg{}-stack-empty for `{}`)", i + 1, method_name),
            range,
        })?;
        let expected = param_tys[i + 1];
        if pushed.wasm_slot() != expected.wasm_slot() {
            return Err(LoweringError::StdlibArgTypeMismatch {
                name: method_name.to_string(),
                arg_idx: (i + 1) as u32,
                got: pushed,
                expected,
                range,
            });
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
    let expected =
        *param_tys
            .get(arg_idx as usize)
            .ok_or_else(|| LoweringError::UnknownStdlibMethod {
                name: name.to_string(),
                arity: param_tys.len() as u32,
                range,
            })?;
    if got.wasm_slot() != expected.wasm_slot() {
        return Err(LoweringError::StdlibArgTypeMismatch {
            name: name.to_string(),
            arg_idx,
            got,
            expected,
            range,
        });
    }
    Ok(())
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
            return Err(LoweringError::UnsupportedExpr {
                kind: format!("Where(bindings={})", bindings.expr.kind()),
                range,
            });
        }
    };
    let saved_lets_len = ctx.lets.len();
    for (key, value) in pairs {
        let name = match key {
            TokenKey::String(s, _, _) => s.clone(),
            _ => {
                return Err(LoweringError::UnsupportedExpr {
                    kind: "Where(non-string-binding-key)".to_string(),
                    range,
                });
            }
        };
        lower_expr(&value.expr, value.range, ctx)?;
        let value_ty = ctx.tstack.pop().ok_or(LoweringError::UnsupportedExpr {
            kind: "Where(binding-empty-stack)".to_string(),
            range: value.range,
        })?;
        let idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetSet { idx, ty: value_ty },
            range: value.range,
        });
        ctx.lets.push(LetBinding {
            name,
            idx,
            ty: value_ty,
            schema_brand: None,
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
        let lhs_ty = ctx.tstack.pop().ok_or(LoweringError::UnsupportedOperator { op, range })?;
        if lhs_ty != IrType::Bool {
            return Err(LoweringError::UnsupportedOperator { op, range });
        }
        let (then_body, then_ty) = if matches!(op, Operator::And) {
            // a && b: then-branch is `b`, else-branch is `false`.
            lower_branch(rhs, range, ctx)?
        } else {
            // a || b: then-branch is `true`, else-branch is `b`.
            let saved_out = std::mem::take(&mut ctx.out);
            let saved_stack = std::mem::take(&mut ctx.tstack);
            ctx.out.push(TaggedOp { op: Op::ConstBool(true), range });
            ctx.tstack.push(IrType::Bool);
            let body = std::mem::replace(&mut ctx.out, saved_out);
            let stack = std::mem::replace(&mut ctx.tstack, saved_stack);
            (body, stack[0])
        };
        let (else_body, else_ty) = if matches!(op, Operator::And) {
            // a && b: else-branch is `false`.
            let saved_out = std::mem::take(&mut ctx.out);
            let saved_stack = std::mem::take(&mut ctx.tstack);
            ctx.out.push(TaggedOp { op: Op::ConstBool(false), range });
            ctx.tstack.push(IrType::Bool);
            let body = std::mem::replace(&mut ctx.out, saved_out);
            let stack = std::mem::replace(&mut ctx.tstack, saved_stack);
            (body, stack[0])
        } else {
            // a || b: else-branch is `b`.
            lower_branch(rhs, range, ctx)?
        };
        if then_ty != IrType::Bool || else_ty != IrType::Bool {
            return Err(LoweringError::UnsupportedOperator { op, range });
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
        lower_expr(&lhs.expr, lhs.range, ctx)?;
        lower_expr(&rhs.expr, rhs.range, ctx)?;
        let rhs_ty = ctx
            .tstack
            .pop()
            .ok_or(LoweringError::UnsupportedOperator { op, range })?;
        let lhs_ty = ctx
            .tstack
            .pop()
            .ok_or(LoweringError::UnsupportedOperator { op, range })?;
        if lhs_ty != rhs_ty {
            return Err(LoweringError::UnsupportedOperator { op, range });
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
                return Err(LoweringError::UnsupportedOperator { op, range });
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
            return Err(LoweringError::UnsupportedOperator { op, range });
        }
        // Mod on F64 is unsupported (wasm has no `f64.rem`).
        if lhs_ty == IrType::F64 && matches!(op, Operator::Mod) {
            return Err(LoweringError::UnsupportedOperator { op, range });
        }
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
        let rhs_ty = ctx
            .tstack
            .pop()
            .ok_or(LoweringError::UnsupportedOperator { op, range })?;
        let lhs_ty = ctx
            .tstack
            .pop()
            .ok_or(LoweringError::UnsupportedOperator { op, range })?;
        if lhs_ty != rhs_ty {
            return Err(LoweringError::UnsupportedOperator { op, range });
        }
        // Phase 2.c supports comparisons on Int / Float / Bool /
        // Null. Bool / Null only support `==` / `!=`; ordering
        // (`<`, `<=`, `>`, `>=`) is rejected at the comparison
        // codegen layer too, but we surface it here as a lowering
        // error so the message stays user-facing.
        match (lhs_ty, op) {
            (IrType::I64 | IrType::F64, _) => {}
            (IrType::Bool, Operator::Eq | Operator::Ne) => {}
            (IrType::Null, Operator::Eq | Operator::Ne) => {}
            _ => return Err(LoweringError::UnsupportedOperator { op, range }),
        }
        ctx.out.push(TaggedOp {
            op: cmp_ctor(lhs_ty),
            range,
        });
        ctx.tstack.push(IrType::Bool);
        return Ok(());
    }
    Err(LoweringError::UnsupportedOperator { op, range })
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
            // table are still correct; the leaf-lowering ops never
            // touch `next_let_idx` / `lambda_funcs` for non-Where /
            // non-Lambda leaves, and those shapes aren't legal inside
            // an `Add` chain anyway.
            ctx.out.truncate(saved_out_len);
            ctx.tstack.truncate(saved_tstack_len);
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
    let cond_ty = ctx.tstack.pop().ok_or(LoweringError::UnsupportedExpr {
        kind: "Ternary(cond)".to_string(),
        range,
    })?;
    if cond_ty != IrType::Bool {
        return Err(LoweringError::IfConditionNotBool {
            got: cond_ty,
            range,
        });
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
        return Err(LoweringError::IfBranchTypeMismatch {
            then_ty,
            else_ty,
            range,
        });
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
        return Err(LoweringError::UnsupportedExpr {
            kind: format!("Ternary(branch-stack={})", branch_stack.len()),
            range,
        });
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

/// If `return_type` names a user-declared schema (single-segment
/// TypeNode with no generics), return its canonical-form `Schema`
/// recursively flattened. Returns `Ok(None)` when the return type is
/// not a user schema (the v1 scalar-return path stays in effect).
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
        "Int" | "Float" | "Bool" | "Null" | "String" | "List" | "Option" | "Result"
    ) {
        return Ok(None);
    }
    let Some(def) = resolver.resolve(name) else {
        return Ok(None);
    };
    let mut stack: Vec<&str> = Vec::new();
    let schema = canonical_schema_from_def(def, resolver, &mut stack, t.range)?;
    Ok(Some(schema))
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
    let name = def
        .name
        .as_deref()
        .ok_or_else(|| LoweringError::UnsupportedExpr {
            kind: "anonymous-nested-schema".to_string(),
            range,
        })?;
    if stack.contains(&name) {
        let mut cycle: Vec<String> = stack.iter().map(|s| s.to_string()).collect();
        cycle.push(name.to_string());
        return Err(LoweringError::CyclicFieldDependency {
            schema: name.to_string(),
            cycle,
            range,
        });
    }
    stack.push(name);
    let mut fields = Vec::with_capacity(def.fields.len());
    for f in &def.fields {
        let ty_node = f
            .type_hint
            .as_ref()
            .ok_or_else(|| LoweringError::UnsupportedFieldType {
                schema: name.to_string(),
                field: f.name.clone(),
                ty: "<untyped>".to_string(),
                range: f.value_range,
            })?;
        let ty = canonical_type_repr(ty_node, resolver, stack, f.value_range)?;
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
    })
}

/// Convert a `TypeNode` into the canonical `TypeRepr`. Supports the
/// Phase 3.b surface (scalar leaves, `String`, `List<Int>`, nested
/// user schemas); everything else surfaces
/// [`LoweringError::UnsupportedFieldType`] at the call site that
/// owns the field name.
fn canonical_type_repr<'a>(
    ty: &TypeNode,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
) -> Result<TypeRepr, LoweringError> {
    if ty.path.len() != 1 || ty.variant_fields.is_some() {
        return Err(LoweringError::UnsupportedFieldType {
            schema: stack.last().copied().unwrap_or("?").to_string(),
            field: "?".to_string(),
            ty: type_head_for_display(ty),
            range,
        });
    }
    let head = ty.path[0].as_str();
    match (head, ty.generics.as_slice()) {
        ("Int", []) => Ok(TypeRepr::Int),
        ("Float", []) => Ok(TypeRepr::Float),
        ("Bool", []) => Ok(TypeRepr::Bool),
        ("Null", []) => Ok(TypeRepr::Null),
        ("String", []) => Ok(TypeRepr::String),
        ("List", [elem]) => {
            let inner = canonical_type_repr(elem, resolver, stack, range)?;
            if matches!(inner, TypeRepr::Int) {
                Ok(TypeRepr::List {
                    element: Box::new(inner),
                })
            } else {
                Err(LoweringError::UnsupportedFieldType {
                    schema: stack.last().copied().unwrap_or("?").to_string(),
                    field: "?".to_string(),
                    ty: type_head_for_display(ty),
                    range,
                })
            }
        }
        _ => {
            // Treat any single-segment head as a user-schema reference.
            let Some(def) = resolver.resolve(head) else {
                return Err(LoweringError::UnsupportedFieldType {
                    schema: stack.last().copied().unwrap_or("?").to_string(),
                    field: "?".to_string(),
                    ty: head.to_string(),
                    range,
                });
            };
            let sub = canonical_schema_from_def(def, resolver, stack, range)?;
            Ok(TypeRepr::Schema {
                schema: Box::new(sub),
            })
        }
    }
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
            return Err(LoweringError::MissingFieldNoDefault {
                schema: schema_name.to_string(),
                field: field.name.clone(),
                range,
            });
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
        return Err(LoweringError::CyclicFieldDependency {
            schema: schema_name.to_string(),
            cycle,
            range,
        });
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
                        return Err(LoweringError::UnknownFieldReferenceInDefault {
                            schema: schema.to_string(),
                            field: field.to_string(),
                            referenced: name.clone(),
                            range: *range,
                        });
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
        TypeRepr::Schema { .. } | TypeRepr::Option { .. } | TypeRepr::Result { .. } => IrType::I32,
        TypeRepr::List { .. } => IrType::ListInt,
        _ => IrType::I32,
    }
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
        LoweringError::UnknownSchemaBrand {
            name: schema.name.clone(),
            range,
        }
    })?;

    // Build name → user-expr map. Reject duplicate keys.
    let mut user_values: HashMap<String, &Node> = HashMap::new();
    for (key, value) in dict_pairs {
        let TokenKey::String(name, _, _) = key else {
            return Err(LoweringError::UnsupportedExpr {
                kind: format!("Dict(non-string-key in branded dict for `{}`)", schema.name),
                range,
            });
        };
        // Schema must declare this field.
        if !schema.fields.iter().any(|f| &f.name == name) {
            return Err(LoweringError::UnsupportedFieldType {
                schema: schema.name.clone(),
                field: name.clone(),
                ty: format!("(unknown field, not declared on `{}`)", schema.name),
                range,
            });
        }
        user_values.insert(name.clone(), value);
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
            lower_dict_field_value(schema, idx, user_value, user_value.range, ctx)?;
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
            lower_dict_default(&schema.name, idx, def, ctx, field_range)?;
        }
        // Stack now holds the field's value (with type matching the
        // canonical Field). Emit the StoreFieldAtRecord.
        let ir_ty = type_repr_to_ir_type_dict(&canonical_field.ty);
        let store_ty = match layout_field.kind {
            FieldKind::Inline { .. } => ir_ty,
            // Pointer-indirect fields all store as an i32 pointer.
            FieldKind::PointerIndirect { .. } => IrType::I32,
        };
        let top = ctx
            .tstack
            .pop()
            .ok_or_else(|| LoweringError::UnsupportedExpr {
                kind: format!(
                    "Dict field `{}` of `{}` produced no value",
                    canonical_field.name, schema.name
                ),
                range,
            })?;
        if top.wasm_slot() != store_ty.wasm_slot() {
            return Err(LoweringError::UnsupportedFieldType {
                schema: schema.name.clone(),
                field: canonical_field.name.clone(),
                ty: format!("got {:?}, expected {:?}", top, store_ty),
                range,
            });
        }
        ctx.out.push(TaggedOp {
            op: Op::StoreFieldAtRecord {
                record_local_idx: record_local,
                offset: layout_field.offset as u32,
                ty: store_ty,
            },
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
        });
    }

    // Pop the field-name let bindings we pushed so the surrounding
    // scope sees its original let stack.
    let drop_count = schema.fields.len();
    let new_len = ctx.lets.len().saturating_sub(drop_count);
    ctx.lets.truncate(new_len);

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
        (TypeRepr::Schema { schema: sub_schema }, Expr::Dict(pairs)) => {
            // Nested branded dict. Allocate a sub-record, recurse,
            // then push the sub-record's base offset for the parent's
            // pointer slot.
            let sub_layout = SchemaLayout::offsets_for(sub_schema)?;
            let record_local = ctx.alloc_record_local();
            ctx.out.push(TaggedOp {
                op: Op::AllocSubRecord {
                    record_local_idx: record_local,
                    root_size: sub_layout.root_size as u32,
                    root_align: sub_layout.root_align as u32,
                },
                range,
            });
            lower_dict_into_record(sub_schema, &sub_layout, pairs, range, record_local, ctx)?;
            // Push the sub-record base so the parent's pointer-slot
            // store can consume it.
            ctx.out.push(TaggedOp {
                op: Op::PushRecordBase {
                    record_local_idx: record_local,
                },
                range,
            });
            ctx.tstack.push(IrType::I32);
            Ok(())
        }
        (TypeRepr::String, _) | (TypeRepr::List { .. }, _) => {
            // Recursively lower the value to produce an absolute
            // pointer (ConstString / ConstListInt / LoadStringPtr /
            // ...). Then copy the record into the parent's tail
            // area and push the buffer-relative offset.
            lower_expr(&value.expr, range, ctx)?;
            // Top of stack is an absolute address. Emit the tail-
            // record memcpy.
            let popped = ctx.tstack.pop().ok_or(LoweringError::UnsupportedExpr {
                kind: "Dict(field-value-stack-empty)".to_string(),
                range,
            })?;
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
                    _ => IrType::ListInt,
                },
                _ => unreachable!(),
            };
            if popped != expected_ir {
                return Err(LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: canonical.name.clone(),
                    ty: format!("expected {expected_ir:?}, got {popped:?}"),
                    range,
                });
            }
            ctx.out.push(TaggedOp {
                op: Op::EmitTailRecordFromAbsoluteAddr { ty: expected_ir },
                range,
            });
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
    def: &SchemaDef,
    ctx: &mut LowerCtx<'_>,
    range: TokenRange,
) -> Result<(), LoweringError> {
    let field = &def.fields[field_idx];
    if field.is_wildcard {
        return Err(LoweringError::MissingFieldNoDefault {
            schema: schema_name.to_string(),
            field: field.name.clone(),
            range,
        });
    }
    // Lower the default expression with the surrounding lets in
    // scope. The let-stack already carries `<prior-field-name> →
    // value` bindings because the topological order placed
    // dependencies first.
    let value_node = &field.value_node;
    lower_expr(&value_node.expr, value_node.range, ctx)?;
    Ok(())
}

/// Lower a bare-identifier reference. Phase 3.a checks the user-let
/// scope first (innermost shadow wins) and falls back to the `#main`
/// parameter index. The let-binding hit emits an [`Op::LetGet`]; the
/// param hit emits a typed [`Op::LoadField`] reading from the `in_buf`.
///
/// Phase 5 extends the surface in two ways:
///
/// * `self` (when the lowering context owns a `self_binding`) lowers
///   to the wasm-local that holds the schema instance's absolute
///   address.
/// * Multi-segment paths whose head resolves to a schema-typed
///   binding chase field offsets through the schema's layout chain,
///   emitting [`Op::LoadFieldAtAbsolute`] per segment.
fn lower_variable(
    path: &[TokenKey],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if path.is_empty() {
        return Err(LoweringError::UnsupportedExpr {
            kind: "Variable(empty-path)".to_string(),
            range,
        });
    }
    let head = match &path[0] {
        TokenKey::String(s, _, _) => s.as_str(),
        TokenKey::Index(_, _) | TokenKey::Dummy | TokenKey::Spread(_) | TokenKey::Dynamic(_, _) => {
            return Err(LoweringError::UnsupportedExpr {
                kind: "Variable(non-string-key)".to_string(),
                range,
            });
        }
    };
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
            return Err(LoweringError::UnresolvedVariable {
                name: head.to_string(),
                range,
            });
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
            .ok_or_else(|| LoweringError::UnresolvedVariable {
                name: head.to_string(),
                range,
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
    for seg in &path[1..] {
        let field_name = match seg {
            TokenKey::String(s, _, _) => s.as_str(),
            _ => {
                return Err(LoweringError::UnsupportedExpr {
                    kind: "Variable(non-string-segment)".to_string(),
                    range,
                });
            }
        };
        let Some(schema) = current_schema.clone() else {
            return Err(LoweringError::UnsupportedExpr {
                kind: format!(
                    "Variable(field-on-non-schema-base, segment=`{}`)",
                    field_name
                ),
                range,
            });
        };
        // Recompute the layout for the current schema shape. Cached
        // canonical schemas are reused across calls so the resolver
        // doesn't repeatedly re-walk the analyzer tree.
        let layout = SchemaLayout::offsets_for(&schema)?;
        let field_idx = schema
            .fields
            .iter()
            .position(|f| f.name == field_name)
            .ok_or_else(|| LoweringError::UnsupportedFieldType {
                schema: schema.name.clone(),
                field: field_name.to_string(),
                ty: "(unknown field)".to_string(),
                range,
            })?;
        let field_meta = &schema.fields[field_idx];
        let layout_field = &layout.fields[field_idx];
        // Pop the base address.
        let popped = ctx.tstack.pop().ok_or(LoweringError::UnsupportedExpr {
            kind: "Variable(field-load-stack-empty)".to_string(),
            range,
        })?;
        if popped.wasm_slot() != IrType::I32 {
            return Err(LoweringError::UnsupportedExpr {
                kind: format!("Variable(field-base-not-i32, got={:?})", popped),
                range,
            });
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
        let func = lower_one_method(m, &sig, resolver, &registry, Rc::clone(&const_intern))?;
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
/// `Bool` / `Null` types — variable-length return values (`String` /
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
                LoweringError::UnsupportedTypeInMain {
                    type_name: type_head_for_display(&p.type_node),
                    range: p.type_node.range,
                }
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
            LoweringError::UnsupportedTypeInMain {
                type_name: type_head_for_display(&info.return_type),
                range: info.return_type.range,
            }
        })?;
    // Phase 5 scope: only scalar / `Bool` / `Null` returns ride the
    // wasm function's single-value return slot. Variable-length
    // returns are deferred — they need a tail-cursor handshake the
    // non-entry signature doesn't carry yet.
    let ret_ty = match ret_repr {
        TypeRepr::Int => IrType::I64,
        TypeRepr::Float => IrType::F64,
        TypeRepr::Bool => IrType::Bool,
        TypeRepr::Null => IrType::Null,
        _ => {
            return Err(LoweringError::UnsupportedTypeInMain {
                type_name: type_head_for_display(&info.return_type),
                range: info.return_type.range,
            });
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
) -> Result<Func, LoweringError> {
    let MethodSig {
        param_tys,
        ret_ty,
        param_schemas,
    } = sig;
    let ret_ty = *ret_ty;
    let body_node = m
        .info
        .body_node
        .as_ref()
        .ok_or_else(|| LoweringError::UnsupportedExpr {
            kind: format!("SchemaMethod(no-body for `{}`)", m.info.name),
            range: m.info.range,
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
    );
    lower_expr(&body_node.expr, body_node.range, &mut ctx)?;
    // Validate the body left exactly one value of the declared
    // return type on the virtual stack.
    let top = ctx
        .tstack
        .last()
        .copied()
        .ok_or_else(|| LoweringError::UnsupportedExpr {
            kind: format!(
                "SchemaMethod(`{}::{}`) body produced no value",
                m.schema_name, m.info.name
            ),
            range: body_node.range,
        })?;
    if top.wasm_slot() != ret_ty.wasm_slot() {
        return Err(LoweringError::UnsupportedTypeInMain {
            type_name: format!(
                "method `{}::{}` returns `{:?}` but body produced `{:?}`",
                m.schema_name, m.info.name, ret_ty, top
            ),
            range: body_node.range,
        });
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
}
