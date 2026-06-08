//! Schema desugar pass.
//!
//! Walks the root AST and, for every dict entry annotated with `#schema`,
//! lowers the right-hand side to a [`SchemaDef`] keyed by the value
//! node's [`NodeId`]. The evaluator can then skip its own schema
//! extraction for these nodes and just look up the pre-computed result.
//!
//! This pass is deliberately conservative: anything dynamic
//! (Schema-as-value composition that depends on `&sibling` / `&root`
//! lookups, or schemas built via expressions) is left for the evaluator
//! to resolve at runtime. Only the "obvious" static cases are handled
//! here:
//!
//! * `#schema Name { Type field: predicate, ... }`
//! * `#schema Name Base + { Type field: predicate, ... }` where `Base`
//!   is a sibling identifier we can record by name (the evaluator still
//!   composes the predicates at runtime).
//!
//! Fields whose type or predicate cannot be statically classified are
//! recorded with placeholders; this is a structural skeleton meant to
//! support diagnostics + future passes, not full type-checking.

use crate::decorator_names::VALUE;
use crate::diagnostic::{span_of, Diagnostic};
use crate::directive_names::{BRAND, DEFAULT, ERROR, EXPECT, MSG, SCHEMA};
use crate::tree::AnalyzedTree;
use crate::typecheck::format_type;
use relon_parser::{
    type_node_from_brand_arg, Directive, DirectiveBody, Expr, Node, NodeId, Operator, TokenKey,
    TokenRange, TypeNode,
};
use std::sync::Arc;

/// Static skeleton of a `#schema` definition. The evaluator owns the
/// authoritative runtime form (`Value::Schema` with closure predicates);
/// this is the AST-level shape that LSP and lint passes can reason
/// about without running the program.
#[derive(Debug, Clone)]
pub struct SchemaDef {
    /// Identifier the schema was bound to (`#schema User {...}` →
    /// `"User"`). `None` for anonymous `#schema` annotations on data.
    pub name: Option<String>,
    /// Generic type parameters declared by this schema (e.g. `Page<T>`).
    pub generics: Vec<String>,
    /// Field declarations in source order.
    pub fields: Vec<SchemaFieldDef>,
    /// Positional element declarations for tuple schemas
    /// (`#schema Pair (Int, String)`). `None` means this is not a tuple
    /// schema; `Some(vec![])` is the unit tuple schema.
    pub tuple_elements: Option<Vec<TypeNode>>,
    /// Base schemas this one extends (left operands of `Base + { ... }`).
    /// Each entry carries both the human-readable name (for diagnostics
    /// and LSP hover) and an `Arc<Node>` pointing back to the original
    /// reference expression. The evaluator re-evaluates that node at
    /// validation time to fetch the base's runtime `Value::Schema`.
    pub bases: Vec<BaseRef>,
    /// Source range of the schema body (for diagnostics / LSP hover).
    pub range: TokenRange,
    /// Tagged-enum variants, populated for sum-type schemas
    /// (`#schema X Enum<A { ... }, B>`). When non-empty, `fields` and
    /// `bases` are unused — the schema is consumed via variant
    /// construction and pattern matching instead of dict validation.
    pub variants: Vec<EnumVariant>,
    /// Schema-rooted Phase B: methods declared inside the schema's
    /// `with { ... }` block (decisions 1, 4, 10, 12 of
    /// `schema-rooted-model-2026-05-11.md`). Source-order; analyzer
    /// later resolves `Self` and merges `#extend` contributions per
    /// import-chain visibility (decision 9).
    pub methods: Vec<SchemaMethodInfo>,
    /// Schema-level `#no_auto_derive <Constraint>` opt-outs collected
    /// from the schema's `with { ... }` block (decision 15). Constraint
    /// names as bare strings; analyzer cross-references against the
    /// built-in constraint set in Phase C.
    pub schema_no_auto_derives: Vec<String>,
    /// Documentation extracted from leading comments.
    pub doc_comment: Option<String>,
}

/// Static skeleton of a single method declared in a `with { ... }`
/// block (either on a `#schema X { ... } with { ... }` or on a
/// `#extend X with { ... }`). Mirrors `relon_parser::SchemaMethod`
/// shape but lives in the analyzer crate so passes can attach derived
/// data without depending on the parser AST.
///
/// `body_node` carries the AST `Node` of the method body when present
/// (`None` when `is_native` — host-implemented). Analyzer passes treat
/// it as a closure-like body; evaluator dispatches by binding `self`
/// and evaluating the body in the schema's scope.
#[derive(Debug, Clone)]
pub struct SchemaMethodInfo {
    /// Method name as declared in source.
    pub name: String,
    /// Range of the method-name token (for LSP hover / diagnostics).
    pub name_range: TokenRange,
    /// Method-level generic type parameter names (e.g. `["U"]` for
    /// `map<U>(...)`). Empty for monomorphic methods. These get spliced
    /// into the synthesized `FnSignature.generics` so the existing
    /// `instantiate` machinery in `sig.rs` can bind them at the call
    /// site — schema-level generics already in scope come from the
    /// `SchemaDef.generics` field on the owning schema.
    pub generics: Vec<String>,
    /// Method parameters as declared (excluding the implicit `self`).
    pub params: Vec<SchemaMethodParamInfo>,
    /// Declared return type (`-> R` slot).
    pub return_type: TypeNode,
    /// Body expression node when the method has one. `None` for
    /// `#native` methods.
    pub body_node: Option<Arc<Node>>,
    /// Constraint names from method-level `#derive <C>` pragmas
    /// (decision 18: only constraints not already derived in the
    /// owning schema's initial declaration).
    pub derives: Vec<String>,
    /// True when an `#native` pragma precedes this method — the body
    /// lives in host Rust (registered via `register_method`, Phase D).
    pub is_native: bool,
    /// True when an `#internal` pragma precedes this method (decision
    /// 16: schema-internal visibility, only callable from other method
    /// bodies on the same schema).
    pub is_private: bool,
    /// Range of the entire method declaration (signature + body).
    pub range: TokenRange,
    /// Source module path / hint of where this method was declared.
    /// `None` when declared in the schema's initial `#schema X` site;
    /// `Some(canonical_id)` when contributed by an `#extend X` in
    /// another module (used for import-chain visibility checks in
    /// Phase B.2).
    pub source_module: Option<String>,
    /// Documentation extracted from leading comments.
    pub doc_comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SchemaMethodParamInfo {
    pub name: String,
    pub name_range: TokenRange,
    pub type_node: TypeNode,
}

/// One alternative inside a sum-type Enum schema. `fields` is empty for
/// unit variants like `Push`.
#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub name: String,
    pub fields: Vec<SchemaFieldDef>,
    pub range: TokenRange,
    pub doc_comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SchemaFieldDef {
    pub name: String,
    /// `None` means the field had no static type prefix. The schema pass
    /// emits a `SchemaFieldUntyped` diagnostic for this case but still
    /// records the field so downstream passes can reason about its
    /// presence.
    pub type_hint: Option<TypeNode>,
    /// Range of the field's value expression (predicate, default, etc.).
    pub value_range: TokenRange,
    /// `true` if the value position is the `*` wildcard. Useful for
    /// hover docs and "predicate vs. wildcard" lint rules.
    pub is_wildcard: bool,
    /// Cheap pointer back to the original AST value node. The evaluator
    /// uses this to instantiate predicate closures and run `#expect /
    /// #default` decorator hooks without re-walking the body. Stored as
    /// `Arc<Node>` so `SchemaDef` can be shared cheaply between analyzer
    /// passes, evaluator, and LSP consumers.
    pub value_node: Arc<Node>,
    /// Names of decorators attached to the field (`#expect`, `#default`,
    /// `#msg`, ...) in source order, paired with `Arc<Node>` references
    /// to each decorator's argument list. The evaluator dispatches them
    /// by name through `schema_field_meta`, so the analyzer only needs
    /// to record the dispatch shape — not run the hooks itself.
    pub meta_decorators: Vec<MetaDecoratorRef>,
    /// Documentation extracted from leading comments.
    pub doc_comment: Option<String>,
}

/// Static reference to a `#meta ...` directive attached to a schema
/// field. The evaluator looks up the matching directive plugin by `name`
/// and re-evaluates the body at validation time.
#[derive(Debug, Clone)]
pub struct MetaDecoratorRef {
    pub name: String,
    pub range: TokenRange,
    pub directive: Arc<Directive>,
}

/// Static reference to a base schema in `Base + { ... }` composition.
#[derive(Debug, Clone)]
pub struct BaseRef {
    /// Last identifier in the reference path (`&sibling.foo.Base` →
    /// `"Base"`). Used for diagnostics and LSP hover.
    pub name: String,
    /// Original reference expression node. Evaluator re-runs this with
    /// the live scope to obtain the base `Value::Schema`.
    pub node: Arc<Node>,
}

/// Stage 2.8: walk every collected schema's field type hints and emit
/// `UnknownTypeName` for heads that aren't builtins, prelude names, or
/// declared schemas. Multi-segment heads (`pkg.Type`) are handled by the
/// workspace-level `re_check_unknown_types` post-pass.
///
/// v1.6 piggyback: same walk also enforces the "no `Any` in user code"
/// policy on schema field types. The recursive
/// `crate::ban_unsafe_types::scan_typenode_for_any` helper covers nested generics
/// like `List<Any>` / `Dict<String, Any>` — those used to be a sneaky
/// way to launder `Any` past the surface check.
pub fn check_schema_field_types(tree: &mut AnalyzedTree) {
    use crate::diagnostic::Diagnostic;
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    // Snapshot the set of declared schema names so we don't borrow
    // `tree.schemas` while iterating.
    let known_names: std::collections::HashSet<String> = tree
        .schemas
        .values()
        .filter_map(|d| d.name.clone())
        .chain(tree.root_schemas.iter().map(|d| d.name.clone()))
        .collect();
    for def in tree.schemas.values() {
        // v1.8+ fix (issue 4): a generic schema like `#schema Box<T> {
        // T value: * }` legitimately uses `T` as a field type. Pre-fix
        // `T` wasn't in `known_names`, so the field-type walker
        // reported `unknown type name T`. The generic parameters are
        // schema-local, so we extend the known set per schema instead
        // of polluting the global set.
        let mut local_known = known_names.clone();
        for g in &def.generics {
            local_known.insert(g.clone());
        }
        for element in def.tuple_elements.iter().flatten() {
            crate::ban_unsafe_types::scan_typenode_for_any(
                element,
                &format!(
                    "schema tuple element `{}`",
                    def.name.as_deref().unwrap_or("<anonymous>")
                ),
                &mut diagnostics,
            );
            check_schema_type_name(element, &local_known, &mut diagnostics);
        }
        for field in &def.fields {
            let Some(t) = &field.type_hint else { continue };
            // v1.6: ban `Any` (anywhere in the type tree).
            crate::ban_unsafe_types::scan_typenode_for_any(
                t,
                &format!("schema field `{}`", field.name),
                &mut diagnostics,
            );
            check_schema_type_name(t, &local_known, &mut diagnostics);
        }
    }
    tree.diagnostics.extend(diagnostics);
}

fn check_schema_type_name(
    t: &TypeNode,
    local_known: &std::collections::HashSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if t.path.len() == 1 {
        let head = &t.path[0];
        if relon_parser::is_builtin_type_name(head) {
            return;
        }
        if matches!(head.as_str(), "Result" | "Option") {
            return;
        }
        if local_known.contains(head) {
            return;
        }
        diagnostics.push(Diagnostic::UnknownTypeName {
            name: head.clone(),
            range: span_of(t.range),
        });
    } else if t.path.len() == 2 {
        // v1.8+: tentative `pkg.Tail` diagnostic; cleared by
        // `re_check_unknown_types` iff the entry's import index
        // resolves `head` to an alias whose exports include
        // `tail`. Otherwise the user sees the diagnostic.
        diagnostics.push(Diagnostic::UnknownTypeName {
            name: format!("{}.{}", t.path[0], t.path[1]),
            range: span_of(t.range),
        });
    }
}

pub(crate) fn tuple_elements_for_schema_type(
    tree: &AnalyzedTree,
    expected: &TypeNode,
) -> Option<(String, Vec<TypeNode>)> {
    let schema_name = match expected.path.as_slice() {
        [name] => name.clone(),
        [alias, name] => {
            let idx = tree.workspace_import_index.as_ref()?;
            if idx
                .aliased
                .get(alias)
                .is_some_and(|exports| exports.contains(name))
            {
                format!("{alias}.{name}")
            } else {
                return None;
            }
        }
        _ => return None,
    };
    for def in tree.schemas.values() {
        if def.name.as_deref() == Some(schema_name.as_str()) {
            return def
                .tuple_elements
                .as_ref()
                .map(|elements| (schema_name.clone(), elements.clone()));
        }
    }
    tree.workspace_import_index
        .as_ref()
        .and_then(|idx| idx.imported_tuple_schemas.get(&schema_name))
        .cloned()
        .map(|elements| (schema_name, elements))
}

/// Walk `root` and populate `tree.schemas` with every statically-classifiable
/// `#schema` definition. Root-level name-body directives are owned by
/// [`crate::root_schemas::collect_root_schemas`] (which keeps a separate
/// list and reports its own diagnostics); this pass walks dict fields
/// looking for the bare `#schema` directive on a `key: value` field —
/// that's the dict-field form, equivalent to old batch-2 `#schema X: ...`.
pub fn collect_schemas(root: &Node, tree: &mut AnalyzedTree) {
    if let Expr::Dict(pairs) = &*root.expr {
        for (key, value) in pairs {
            visit_field(key, value, tree);
            visit_for_schemas(value, tree);
        }
    }
}

fn visit_field(key: &TokenKey, value: &Node, tree: &mut AnalyzedTree) {
    let has_schema_directive = value
        .directives
        .iter()
        .any(|dir| dir.name == SCHEMA && matches!(dir.body, DirectiveBody::Bare));
    if !has_schema_directive {
        return;
    }

    let mut name = None;
    let mut generics = Vec::new();
    match key {
        TokenKey::String(s, _, _) => name = Some(s.clone()),
        TokenKey::Dynamic(node, _) => {
            if let Expr::Type(t) = &*node.expr {
                name = t.path.first().cloned();
                generics = t
                    .generics
                    .iter()
                    .filter_map(|g| g.path.first().cloned())
                    .collect();
            }
        }
        _ => {}
    }
    if let Some(def) = lower_schema(name, generics, value, tree) {
        record_schema_methods(&def, tree);
        tree.schemas.insert(value.id, def);
    }
}

/// Recursively walk nested dict bodies looking for dict-field `#schema`
/// directives. Each match is lowered into a [`SchemaDef`] keyed at the
/// value node's id (analogous to batch 2's `#schema`-decorated fields).
fn visit_for_schemas(node: &Node, tree: &mut AnalyzedTree) {
    if let Expr::Dict(pairs) = &*node.expr {
        for (key, value) in pairs {
            visit_field(key, value, tree);
            visit_for_schemas(value, tree);
        }
    }
}

fn lower_schema(
    name: Option<String>,
    generics: Vec<String>,
    value: &Node,
    tree: &mut AnalyzedTree,
) -> Option<SchemaDef> {
    let (def, diags) = lower_schema_pure(name, generics, value);
    tree.diagnostics.extend(diags);
    def
}

pub fn lower_schema_pure(
    name: Option<String>,
    generics: Vec<String>,
    value: &Node,
) -> (Option<SchemaDef>, Vec<Diagnostic>) {
    lower_schema_pure_with(name, generics, value, &[], &[])
}

/// Schema-rooted Phase B variant: also accepts the `with { ... }` block
/// contributions parsed off the directive (`methods` and
/// `schema_no_auto_derives`). Used by `collect_root_schemas` when the
/// surrounding directive carries this metadata; the legacy
/// `lower_schema_pure` keeps the empty-`with` shape for callers that
/// don't see the directive (dict-field form).
pub fn lower_schema_pure_with(
    name: Option<String>,
    generics: Vec<String>,
    value: &Node,
    methods: &[relon_parser::SchemaMethod],
    schema_no_auto_derives: &[String],
) -> (Option<SchemaDef>, Vec<Diagnostic>) {
    let mut tmp = AnalyzedTree::new();
    let mut def = SchemaDef {
        name,
        generics,
        fields: Vec::new(),
        tuple_elements: None,
        bases: Vec::new(),
        range: value.range,
        variants: Vec::new(),
        methods: methods.iter().map(method_info_from_parser).collect(),
        schema_no_auto_derives: schema_no_auto_derives.to_vec(),
        doc_comment: value.doc_comment.clone(),
    };
    let ok = walk_schema_body(value, &mut def, &mut tmp);
    let diags = std::mem::take(&mut tmp.diagnostics);
    if ok {
        (Some(def), diags)
    } else {
        (None, diags)
    }
}

/// Mirror a freshly-lowered `SchemaDef.methods` list into the
/// `tree.schema_methods` index. Schema-rooted dispatch is keyed by
/// schema *name*, so anonymous schemas (no `name`) and method-less
/// definitions are skipped — they cannot contribute callable methods.
/// Append-only: when the same schema name appears in both root-level
/// `#schema` and an `#extend` block, both contributions accumulate
/// here in source order; conflict detection runs in a later pass.
pub fn record_schema_methods(def: &SchemaDef, tree: &mut AnalyzedTree) {
    let Some(name) = def.name.as_ref() else {
        return;
    };
    if def.methods.is_empty() {
        return;
    }
    tree.schema_methods
        .entry(name.clone())
        .or_default()
        .extend(def.methods.iter().cloned());
}

/// Convert a parser-produced `SchemaMethod` into the analyzer's
/// `SchemaMethodInfo`. `source_module` is left `None` here — the
/// workspace pass fills it for `#extend`-contributed methods when
/// merging across modules (Phase B.2).
pub fn method_info_from_parser(m: &relon_parser::SchemaMethod) -> SchemaMethodInfo {
    SchemaMethodInfo {
        name: m.name.clone(),
        name_range: m.name_range,
        generics: m.generics.clone(),
        params: m
            .params
            .iter()
            .map(|p| SchemaMethodParamInfo {
                name: p.name.clone(),
                name_range: p.name_range,
                type_node: p.type_node.clone(),
            })
            .collect(),
        return_type: m.return_type.clone(),
        body_node: m.body.as_ref().map(|b| Arc::new((**b).clone())),
        derives: m.derives.clone(),
        is_native: m.is_native,
        is_private: m.is_private,
        range: m.range,
        source_module: None,
        doc_comment: m.doc_comment.clone(),
    }
}

/// Recurse through the schema body. `Dict` adds fields directly; `Binary
/// Add` walks both sides so `Base + { ... } + { ... }` flattens cleanly.
/// Returns `false` if the body's top-level shape is something the static
/// pass refuses to interpret (and a diagnostic was emitted).
fn walk_schema_body(node: &Node, def: &mut SchemaDef, tree: &mut AnalyzedTree) -> bool {
    match &*node.expr {
        // `#schema X Enum<...>` body — a Type whose head is `Enum`. Detect
        // tagged-enum form (alternatives carrying `variant_fields`) here so
        // the analyzer can expose `def.variants` to downstream passes.
        Expr::Type(t) if t.path.len() == 1 && t.path[0] == "Enum" => lower_enum_body(t, def, tree),
        // Tuple schema body: `#schema IPv4 (Int, Int, Int, Int)`.
        // This is fixed-arity positional data, distinct from both
        // struct schemas (`{ ... }`) and homogeneous list values (`[...]`).
        Expr::Tuple(items) => collect_tuple_elements(items, def, tree),
        Expr::Dict(pairs) => {
            collect_fields(pairs, def, tree);
            true
        }
        Expr::Binary(Operator::Add, lhs, rhs) => {
            // Try to record the LHS as a base reference, then continue
            // into the RHS as more fields. If LHS isn't a recognizable
            // identifier we keep walking — runtime will handle it.
            if let Some(base) = base_ref(lhs) {
                def.bases.push(base);
            } else {
                walk_schema_body(lhs, def, tree);
            }
            walk_schema_body(rhs, def, tree);
            true
        }
        Expr::Reference { .. } | Expr::Variable(_) => {
            if let Some(base) = base_ref(node) {
                def.bases.push(base);
                return true;
            }
            // Reference shape we don't recognize — leave it for runtime
            // and don't emit a diagnostic; this is a "soft skip".
            true
        }
        _ => {
            tree.diagnostics.push(Diagnostic::SchemaBodyNotDict {
                found: node.expr.kind().to_string(),
                range: span_of(node.range),
            });
            false
        }
    }
}

fn collect_tuple_elements(items: &[Node], def: &mut SchemaDef, tree: &mut AnalyzedTree) -> bool {
    if !def.fields.is_empty() || !def.variants.is_empty() || !def.bases.is_empty() {
        tree.diagnostics.push(Diagnostic::SchemaBodyNotDict {
            found: "Tuple composition".to_string(),
            range: span_of(def.range),
        });
        return false;
    }
    let mut elements = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let Some(t) = type_node_from_tuple_schema_item(item) else {
            tree.diagnostics.push(Diagnostic::SchemaFieldUntyped {
                field: i.to_string(),
                range: span_of(item.range),
            });
            return false;
        };
        elements.push(t);
    }
    def.tuple_elements = Some(elements);
    true
}

fn type_node_from_tuple_schema_item(item: &Node) -> Option<TypeNode> {
    match &*item.expr {
        Expr::Type(t) => Some(t.clone()),
        Expr::Variable(path) => {
            let [TokenKey::String(name, _, false)] = path.as_slice() else {
                return None;
            };
            Some(TypeNode {
                path: vec![name.clone()],
                generics: Vec::new(),
                is_optional: false,
                range: item.range,
                variant_fields: None,
                doc_comment: item.doc_comment.clone(),
            })
        }
        _ => None,
    }
}

/// Lower an `Enum<...>` schema body. If any alternative carries
/// `variant_fields`, the schema is treated as a tagged sum type and every
/// alternative must be a named variant — otherwise we emit
/// `HeterogeneousEnum`. Untagged enums (no `variant_fields` anywhere) are
/// left intact for runtime check (`def.variants` stays empty).
fn lower_enum_body(t: &TypeNode, def: &mut SchemaDef, tree: &mut AnalyzedTree) -> bool {
    let any_struct = t.generics.iter().any(|g| g.variant_fields.is_some());
    if !any_struct {
        // Plain untagged enum — runtime owns it. We still mark the schema
        // valid so the host has a `SchemaDef` keyed at this node id.
        return true;
    }
    let all_struct = t.generics.iter().all(|g| g.variant_fields.is_some());
    if !all_struct {
        tree.diagnostics.push(Diagnostic::HeterogeneousEnum {
            range: span_of(t.range),
        });
        // Don't lower a half-tagged enum into `tree.schemas` — partial
        // variants would shadow the whole-Enum check at runtime.
        return false;
    }

    for alt in &t.generics {
        let variant_name = alt.path.first().cloned().unwrap_or_default();
        let mut fields = Vec::new();
        if let Some(fields_spec) = &alt.variant_fields {
            for (fname, ftype) in fields_spec {
                fields.push(SchemaFieldDef {
                    name: fname.clone(),
                    type_hint: Some(ftype.clone()),
                    value_range: ftype.range,
                    is_wildcard: true,
                    value_node: Arc::new(Node::with_id(
                        NodeId::SYNTHETIC,
                        Expr::Wildcard,
                        ftype.range,
                    )),
                    meta_decorators: Vec::new(),
                    doc_comment: ftype.doc_comment.clone(),
                });
            }
        }
        def.variants.push(EnumVariant {
            name: variant_name,
            fields,
            range: alt.range,
            doc_comment: alt.doc_comment.clone(),
        });
    }
    true
}

fn collect_fields(pairs: &[(TokenKey, Node)], def: &mut SchemaDef, tree: &mut AnalyzedTree) {
    for (key, value) in pairs {
        let TokenKey::String(field_name, _, _) = key else {
            // Dynamic keys / spreads in a schema body aren't statically
            // analyzable; runtime owns them.
            continue;
        };
        // A field is "typed" if either:
        //   1. It carries a static prefix (`String name: *`) — then
        //      `value.type_hint` is `Some(_)`.
        //   2. The value position itself is a `Type` expression
        //      (`name: String`) — equivalent to `String name: *`.
        //   3. The value position is a bare schema-name reference
        //      (`inner: Inner`). Built-in type names parse straight into
        //      `Expr::Type`, but a user `#schema Inner` is only an
        //      identifier to the parser, so `inner: Inner` lands as an
        //      `Expr::Variable(["Inner"])`. Lift the type-name form into a
        //      `TypeNode` so the value-position spelling desugars to the
        //      same `Inner inner: *` the prefix form produces — this is
        //      what unblocks nested-schema field walks (`o.inner.x`).
        let value_as_type = match &*value.expr {
            Expr::Type(t) => Some(t.clone()),
            Expr::Variable(_) => type_name_from_value_variable(value),
            _ => None,
        };
        // A lifted type-name field carries the same "no inline predicate"
        // shape as the canonical `Type field: *` wildcard form.
        let is_wildcard = matches!(&*value.expr, Expr::Wildcard) || value_as_type.is_some();
        let mut type_hint = value.type_hint.clone().or_else(|| value_as_type.clone());

        // Schema-field-position `#brand X y: *` is the directive-form
        // mirror of `X y: *`: lift the brand argument into the field's
        // type hint when no explicit prefix is present, and emit a
        // conflict diagnostic when both are.
        if let Some((dir, brand_type)) = brand_directive_type(value, field_name, tree) {
            match type_hint.as_ref() {
                None => {
                    type_hint = Some(brand_type);
                }
                Some(existing) => {
                    tree.diagnostics.push(Diagnostic::SchemaFieldBrandConflict {
                        field: field_name.clone(),
                        type_prefix: format_type(existing),
                        range: span_of(dir.range),
                    });
                }
            }
        }

        if type_hint.is_none() && !is_field_skippable(value) {
            tree.diagnostics.push(Diagnostic::SchemaFieldUntyped {
                field: field_name.clone(),
                range: span_of(value.range),
            });
        }
        let meta_decorators = value
            .directives
            .iter()
            .map(|dir| MetaDecoratorRef {
                name: dir.name.clone(),
                range: dir.range,
                directive: Arc::new(dir.clone()),
            })
            .collect();
        def.fields.push(SchemaFieldDef {
            name: field_name.clone(),
            type_hint,
            value_range: value.range,
            is_wildcard,
            value_node: Arc::new(value.clone()),
            meta_decorators,
            doc_comment: value.doc_comment.clone(),
        });
    }
}

/// Lift a value-position schema-field type-name reference (`inner: Inner`)
/// into a [`TypeNode`]. Built-in type names (`String`, `List<…>`, …) are
/// committed to `Expr::Type` by the parser, so the only `Expr::Variable`
/// shapes that reach here are user identifiers. We accept a single bare
/// segment whose initial is uppercase — the lexical convention for a
/// type/schema name — and reject everything else (multi-segment paths,
/// dynamic segments, lowercase value/predicate references like
/// `port: someSibling`). Conservative on purpose: a false lift would turn
/// a predicate field into a phantom typed field.
fn type_name_from_value_variable(value: &Node) -> Option<TypeNode> {
    let Expr::Variable(path) = &*value.expr else {
        return None;
    };
    let [TokenKey::String(name, _, false)] = path.as_slice() else {
        return None;
    };
    if !name.chars().next().is_some_and(char::is_uppercase) {
        return None;
    }
    Some(TypeNode {
        path: vec![name.clone()],
        generics: Vec::new(),
        is_optional: false,
        range: value.range,
        variant_fields: None,
        doc_comment: None,
    })
}

/// `#expect ...` / `#brand X`-directive-marked entries inside a schema
/// body don't need their own type prefix. `#expect` & friends are pure
/// meta directives consumed by the evaluator; `#brand X` doubles as an
/// implicit type prefix (lifted into `type_hint` by `collect_fields`).
/// `@value` (the only surviving `@`-decorator) is also accepted for
/// historical parity. Skip the untyped-field diagnostic for any of these.
fn is_field_skippable(value: &Node) -> bool {
    let any_meta_directive = value
        .directives
        .iter()
        .any(|dir| matches!(dir.name.as_str(), EXPECT | DEFAULT | MSG | ERROR | BRAND));
    if any_meta_directive {
        return true;
    }
    value.decorators.iter().any(|dec| {
        dec.path
            .first()
            .and_then(|seg| match seg {
                TokenKey::String(name, _, _) => Some(name.as_str()),
                _ => None,
            })
            .map(|name| name == VALUE)
            .unwrap_or(false)
    })
}

/// Look for a `#brand X` directive on a schema field. Returns the first
/// hit (directive metadata + extracted [`TypeNode`]); pushes a
/// diagnostic and returns `None` when the body isn't a type expression.
/// Multiple `#brand` on one field doesn't compose, so we only honor the
/// first.
fn brand_directive_type<'a>(
    value: &'a Node,
    field_name: &str,
    tree: &mut AnalyzedTree,
) -> Option<(&'a Directive, TypeNode)> {
    for dir in &value.directives {
        if dir.name != BRAND {
            continue;
        }
        let DirectiveBody::Value(body) = &dir.body else {
            tree.diagnostics
                .push(Diagnostic::SchemaFieldBrandInvalidArg {
                    field: field_name.to_string(),
                    range: span_of(dir.range),
                });
            return None;
        };
        match type_node_from_brand_arg(&body.expr, dir.range) {
            Some(t) => return Some((dir, t)),
            None => {
                tree.diagnostics
                    .push(Diagnostic::SchemaFieldBrandInvalidArg {
                        field: field_name.to_string(),
                        range: span_of(dir.range),
                    });
                return None;
            }
        }
    }
    None
}

fn base_ref(node: &Node) -> Option<BaseRef> {
    let name = match &*node.expr {
        Expr::Reference { path, .. } | Expr::Variable(path) => {
            path.last().and_then(|seg| match seg {
                TokenKey::String(s, _, _) => Some(s.clone()),
                _ => None,
            })?
        }
        _ => return None,
    };
    Some(BaseRef {
        name,
        node: Arc::new(node.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_parser::parse_document;

    fn analyze_str(src: &str) -> AnalyzedTree {
        let node = parse_document(src).expect("parse");
        crate::analyze(&node)
    }

    #[test]
    fn collects_simple_schema() {
        let tree = analyze_str(
            r#"{
                #schema User {
                    String name: *,
                    Int age: *
                },
                User alice: { name: "A", age: 1 }
            }"#,
        );
        assert!(!tree.has_errors());
        assert_eq!(tree.schemas.len(), 1);
        let def = tree.schemas.values().next().unwrap();
        assert_eq!(def.name.as_deref(), Some("User"));
        assert_eq!(def.fields.len(), 2);
        assert_eq!(def.fields[0].name, "name");
        assert!(def.fields[0].is_wildcard);
        assert!(def.fields[0].type_hint.is_some());
    }

    #[test]
    fn records_base_for_composition() {
        let tree = analyze_str(
            r#"{
                #schema Base { String name: * },
                #schema Derived &sibling.Base + { Int age: * }
            }"#,
        );
        assert!(!tree.has_errors(), "{:?}", tree.diagnostics);
        let derived = tree
            .schemas
            .values()
            .find(|d| d.name.as_deref() == Some("Derived"))
            .expect("Derived schema present");
        let base_names: Vec<&str> = derived.bases.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(base_names, vec!["Base"]);
        assert_eq!(derived.fields.len(), 1);
        assert_eq!(derived.fields[0].name, "age");
    }

    #[test]
    fn diagnoses_non_dict_schema_body() {
        // Root-level `#schema Bad 42` body is `42`, not a Dict / Enum
        // / composition / alias. The root-schemas pass surfaces this
        // as `RootSchemaInvalidValue` instead of `SchemaBodyNotDict`
        // (the latter only fires for nested dict-field schemas).
        let tree = analyze_str(r#"{ #schema Bad 42 }"#);
        assert!(tree.has_errors());
        assert!(tree.diagnostics.iter().any(
            |d| matches!(d, Diagnostic::RootSchemaInvalidValue { name, .. } if name == "Bad")
        ));
    }

    #[test]
    fn diagnoses_untyped_schema_field() {
        let tree = analyze_str(
            r#"{
                #schema Bad {
                    name: *
                }
            }"#,
        );
        assert!(tree.has_errors());
        assert!(matches!(
            tree.diagnostics.first(),
            Some(Diagnostic::SchemaFieldUntyped { field, .. }) if field == "name"
        ));
    }

    #[test]
    fn skips_decorated_meta_fields_for_untyped_diagnostic() {
        let tree = analyze_str(
            r#"{
                #schema OK {
                    #expect "required" String name: *
                }
            }"#,
        );
        assert!(!tree.has_errors(), "{:?}", tree.diagnostics);
    }

    #[test]
    fn lowers_sum_type_enum_schema() {
        let tree = analyze_str(
            r#"{
                #schema Notification Enum<
                    Email { address: String, subject: String },
                    SMS { phone: String },
                    Push,
                >
            }"#,
        );
        assert!(!tree.has_errors(), "{:?}", tree.diagnostics);
        let def = tree
            .schemas
            .values()
            .find(|d| d.name.as_deref() == Some("Notification"))
            .expect("schema present");
        assert_eq!(def.variants.len(), 3);
        assert_eq!(def.variants[0].name, "Email");
        assert_eq!(def.variants[0].fields.len(), 2);
        assert_eq!(def.variants[2].name, "Push");
        assert_eq!(def.variants[2].fields.len(), 0);
    }

    #[test]
    fn lowers_single_variant_enum_schema() {
        let tree = analyze_str(
            r#"{
                #schema Wrap Enum<Only { v: Int }>
            }"#,
        );
        assert!(!tree.has_errors(), "{:?}", tree.diagnostics);
        let def = tree
            .schemas
            .values()
            .find(|d| d.name.as_deref() == Some("Wrap"))
            .expect("schema present");
        assert_eq!(def.variants.len(), 1);
        assert_eq!(def.variants[0].name, "Only");
    }

    #[test]
    fn diagnoses_heterogeneous_enum() {
        // Mixing a literal `"hot"` and a struct variant `Email { ... }`
        // is the classic heterogeneous-enum mistake.
        let tree = analyze_str(
            r#"{
                #schema Mixed Enum<"hot", Email { address: String }>
            }"#,
        );
        assert!(tree.has_errors(), "{:?}", tree.diagnostics);
        assert!(tree
            .diagnostics
            .iter()
            .any(|d| matches!(d, Diagnostic::HeterogeneousEnum { .. })));
    }
}
