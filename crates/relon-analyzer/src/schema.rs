//! Schema desugar pass.
//!
//! Walks the root AST and, for every dict entry annotated with `@schema`,
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
//! * `@schema Name: { Type field: predicate, ... }`
//! * `@schema Name: Base + { Type field: predicate, ... }` where `Base`
//!   is a sibling identifier we can record by name (the evaluator still
//!   composes the predicates at runtime).
//!
//! Fields whose type or predicate cannot be statically classified are
//! recorded with placeholders; this is a structural skeleton meant to
//! support diagnostics + future passes, not full type-checking.

use crate::diagnostic::{span_of, Diagnostic};
use crate::tree::AnalyzedTree;
use relon_parser::{Decorator, Expr, Node, NodeId, Operator, TokenKey, TokenRange, TypeNode};
use std::sync::Arc;

/// Static skeleton of a `@schema` definition. The evaluator owns the
/// authoritative runtime form (`Value::Schema` with closure predicates);
/// this is the AST-level shape that LSP and lint passes can reason
/// about without running the program.
#[derive(Debug, Clone)]
pub struct SchemaDef {
    /// Identifier the schema was bound to (`@schema User: {...}` →
    /// `"User"`). `None` for anonymous `@schema` annotations on data.
    pub name: Option<String>,
    /// Field declarations in source order.
    pub fields: Vec<SchemaFieldDef>,
    /// Base schemas this one extends (left operands of `Base + { ... }`).
    /// Each entry carries both the human-readable name (for diagnostics
    /// and LSP hover) and an `Arc<Node>` pointing back to the original
    /// reference expression. The evaluator re-evaluates that node at
    /// validation time to fetch the base's runtime `Value::Schema`.
    pub bases: Vec<BaseRef>,
    /// Source range of the schema body (for diagnostics / LSP hover).
    pub range: TokenRange,
    /// Tagged-enum variants, populated for sum-type schemas
    /// (`@schema X: Enum<A { ... }, B>`). When non-empty, `fields` and
    /// `bases` are unused — the schema is consumed via variant
    /// construction and pattern matching instead of dict validation.
    pub variants: Vec<EnumVariant>,
}

/// One alternative inside a sum-type Enum schema. `fields` is empty for
/// unit variants like `Push`.
#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub name: String,
    pub fields: Vec<SchemaFieldDef>,
    pub range: TokenRange,
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
    /// uses this to instantiate predicate closures and run `@expect /
    /// @default` decorator hooks without re-walking the body. Stored as
    /// `Arc<Node>` so `SchemaDef` can be shared cheaply between analyzer
    /// passes, evaluator, and LSP consumers.
    pub value_node: Arc<Node>,
    /// Names of decorators attached to the field (`@expect`, `@default`,
    /// `@msg`, ...) in source order, paired with `Arc<Node>` references
    /// to each decorator's argument list. The evaluator dispatches them
    /// by name through `schema_field_meta`, so the analyzer only needs
    /// to record the dispatch shape — not run the hooks itself.
    pub meta_decorators: Vec<MetaDecoratorRef>,
}

/// Static reference to a `@meta(...)` decorator attached to a schema
/// field. The evaluator looks up the matching `DecoratorPlugin` by
/// `name` and re-evaluates `args` at validation time (host-supplied
/// plugins may want fresh arg values per call).
#[derive(Debug, Clone)]
pub struct MetaDecoratorRef {
    pub name: String,
    pub range: TokenRange,
    pub decorator: Arc<Decorator>,
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

/// Walk `root` and populate `tree.schemas` with every statically-classifiable
/// `@schema` definition.
pub fn collect_schemas(root: &Node, tree: &mut AnalyzedTree) {
    let Expr::Dict(pairs) = &*root.expr else {
        // Top-level Lists can't host `@schema` definitions — only Dicts
        // carry decorated fields. Nothing to collect.
        return;
    };

    for (key, value) in pairs {
        let Some(schema_decorator) = find_schema_decorator(&value.decorators) else {
            continue;
        };
        let name = match key {
            TokenKey::String(s, _, _) => Some(s.clone()),
            _ => None,
        };
        let _ = schema_decorator; // reserved for future per-decorator args
        if let Some(def) = lower_schema(name, value, tree) {
            tree.schemas.insert(value.id, def);
        }
    }
}

fn find_schema_decorator(decorators: &[Decorator]) -> Option<&Decorator> {
    decorators.iter().find(|dec| {
        dec.path.len() == 1 && matches!(&dec.path[0], TokenKey::String(s, _, _) if s == "schema")
    })
}

fn lower_schema(name: Option<String>, value: &Node, tree: &mut AnalyzedTree) -> Option<SchemaDef> {
    let (def, diags) = lower_schema_pure(name, value);
    tree.diagnostics.extend(diags);
    def
}

/// Pure (no-tree-mutation) version of [`lower_schema`]. Used by hosts
/// that need to lower a schema body on-demand — typically the
/// evaluator's `SchemaDecorator` when `Context::analyzed` wasn't
/// attached. Returns the desugar'd [`SchemaDef`] together with any
/// diagnostics that would have been emitted.
pub fn lower_schema_pure(
    name: Option<String>,
    value: &Node,
) -> (Option<SchemaDef>, Vec<Diagnostic>) {
    let mut tmp = AnalyzedTree::new();
    let mut def = SchemaDef {
        name,
        fields: Vec::new(),
        bases: Vec::new(),
        range: value.range,
        variants: Vec::new(),
    };
    let ok = walk_schema_body(value, &mut def, &mut tmp);
    let diags = std::mem::take(&mut tmp.diagnostics);
    if ok {
        (Some(def), diags)
    } else {
        (None, diags)
    }
}

/// Recurse through the schema body. `Dict` adds fields directly; `Binary
/// Add` walks both sides so `Base + { ... } + { ... }` flattens cleanly.
/// Returns `false` if the body's top-level shape is something the static
/// pass refuses to interpret (and a diagnostic was emitted).
fn walk_schema_body(node: &Node, def: &mut SchemaDef, tree: &mut AnalyzedTree) -> bool {
    match &*node.expr {
        // `@schema X: Enum<...>` body — a Type whose head is `Enum`. Detect
        // tagged-enum form (alternatives carrying `variant_fields`) here so
        // the analyzer can expose `def.variants` to downstream passes.
        Expr::Type(t) if t.path.len() == 1 && t.path[0] == "Enum" => {
            lower_enum_body(t, def, tree)
        }
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
                found: expr_kind(&node.expr),
                range: span_of(node.range),
            });
            false
        }
    }
}

/// Lower an `Enum<...>` schema body. If any alternative carries
/// `variant_fields`, the schema is treated as a tagged sum type and every
/// alternative must be a named variant — otherwise we emit
/// `HeterogeneousEnum`. Untagged enums (no `variant_fields` anywhere) are
/// left intact for runtime check (`def.variants` stays empty).
fn lower_enum_body(t: &TypeNode, def: &mut SchemaDef, tree: &mut AnalyzedTree) -> bool {
    let any_variant = t.generics.iter().any(|g| g.variant_fields.is_some());
    if !any_variant {
        // Plain untagged enum — runtime owns it. We still mark the schema
        // valid so the host has a `SchemaDef` keyed at this node id.
        return true;
    }
    let all_variants = t.generics.iter().all(|g| g.variant_fields.is_some());
    if !all_variants {
        tree.diagnostics.push(Diagnostic::HeterogeneousEnum {
            range: span_of(t.range),
        });
        return false;
    }
    for alt in &t.generics {
        let Some(fields_spec) = &alt.variant_fields else {
            continue;
        };
        let variant_name = alt.path.first().cloned().unwrap_or_default();
        let mut fields = Vec::new();
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
            });
        }
        def.variants.push(EnumVariant {
            name: variant_name,
            fields,
            range: alt.range,
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
        let is_wildcard = matches!(&*value.expr, Expr::Wildcard);
        // A field is "typed" if either:
        //   1. It carries a static prefix (`String name: *`) — then
        //      `value.type_hint` is `Some(_)`.
        //   2. The value position itself is a `Type` expression
        //      (`name: String`) — equivalent to `String name: *`.
        let value_as_type = if let Expr::Type(t) = &*value.expr {
            Some(t.clone())
        } else {
            None
        };
        let type_hint = value.type_hint.clone().or_else(|| value_as_type.clone());
        if type_hint.is_none() && !is_field_skippable(value) {
            tree.diagnostics.push(Diagnostic::SchemaFieldUntyped {
                field: field_name.clone(),
                range: span_of(value.range),
            });
        }
        let meta_decorators = value
            .decorators
            .iter()
            .filter_map(|dec| {
                let name = match dec.path.first()? {
                    TokenKey::String(s, _, _) => s.clone(),
                    _ => return None,
                };
                Some(MetaDecoratorRef {
                    name,
                    range: dec.range,
                    decorator: Arc::new(dec.clone()),
                })
            })
            .collect();
        def.fields.push(SchemaFieldDef {
            name: field_name.clone(),
            type_hint,
            value_range: value.range,
            is_wildcard,
            value_node: Arc::new(value.clone()),
            meta_decorators,
        });
    }
}

/// `@expect("...")`-decorated entries inside a schema body don't need
/// their own type prefix; they're meta-decorators consumed by the
/// evaluator. Skip the untyped-field diagnostic for them.
fn is_field_skippable(value: &Node) -> bool {
    value.decorators.iter().any(|dec| {
        dec.path
            .first()
            .and_then(|seg| match seg {
                TokenKey::String(name, _, _) => Some(name.as_str()),
                _ => None,
            })
            .map(|name| matches!(name, "expect" | "default" | "msg" | "error" | "value"))
            .unwrap_or(false)
    })
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

fn expr_kind(expr: &Expr) -> String {
    match expr {
        Expr::Null => "Null",
        Expr::Bool(_) => "Bool",
        Expr::Int(_) => "Int",
        Expr::Float(_) => "Float",
        Expr::String(_) => "String",
        Expr::List(_) => "List",
        Expr::Dict(_) => "Dict",
        Expr::Spread(_) => "Spread",
        Expr::Comprehension { .. } => "Comprehension",
        Expr::Variable(_) => "Variable",
        Expr::Reference { .. } => "Reference",
        Expr::Binary(_, _, _) => "Binary",
        Expr::Unary(_, _) => "Unary",
        Expr::Ternary { .. } => "Ternary",
        Expr::FnCall { .. } => "FnCall",
        Expr::FString(_) => "FString",
        Expr::Type(_) => "Type",
        Expr::Wildcard => "Wildcard",
        Expr::Where { .. } => "Where",
        Expr::Match { .. } => "Match",
        Expr::Closure { .. } => "Closure",
        Expr::VariantCtor { .. } => "VariantCtor",
    }
    .to_string()
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
                @schema User: {
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
                @schema Base: { String name: * },
                @schema Derived: &sibling.Base + { Int age: * }
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
        let tree = analyze_str(r#"{ @schema Bad: 42 }"#);
        assert!(tree.has_errors());
        assert!(matches!(
            tree.diagnostics.first(),
            Some(Diagnostic::SchemaBodyNotDict { .. })
        ));
    }

    #[test]
    fn diagnoses_untyped_schema_field() {
        let tree = analyze_str(
            r#"{
                @schema Bad: {
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
                @schema OK: {
                    @expect("required") String name: *
                }
            }"#,
        );
        assert!(!tree.has_errors(), "{:?}", tree.diagnostics);
    }

    #[test]
    fn lowers_sum_type_enum_schema() {
        let tree = analyze_str(
            r#"{
                @schema Notification: Enum<
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
                @schema Wrap: Enum<Only { v: Int }>
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
                @schema Mixed: Enum<"hot", Email { address: String }>
            }"#,
        );
        assert!(tree.has_errors(), "{:?}", tree.diagnostics);
        assert!(tree
            .diagnostics
            .iter()
            .any(|d| matches!(d, Diagnostic::HeterogeneousEnum { .. })));
    }
}
