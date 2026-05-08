//! `@schema(Name={...})` root-decorator collection pass.
//!
//! Layout sugar that lets users co-locate schema declarations with
//! `@input(...)` in the root-decorator stack instead of stuffing them
//! inside the root dict body. Semantically equivalent to declaring each
//! `Name` as a `@private @schema` field of the root dict — once the pass
//! has registered the schemas, `@input(req=Name)` resolves them the same
//! way it resolves dict-field `@schema X: {...}`.
//!
//! ```relon
//! @schema(Req={ String name: *, Int retries: * })
//! @input(req=Req)
//! { greeting: f"hello ${input.req.name}" }
//! ```
//!
//! Validation here is structural only: shape of the decoration (named
//! arg present, body is a `Dict` or `Enum<...>` type), no duplicates
//! within the root-decorator stack, and no collision with a dict-field
//! `@schema X: {...}` of the same name.

use crate::decorator_names::SCHEMA;
use crate::diagnostic::{span_of, Diagnostic};
use crate::tree::AnalyzedTree;
use relon_parser::{Decorator, Expr, Node, TokenKey, TokenRange};
use std::collections::HashMap;
use std::sync::Arc;

/// One `@schema(Name={...})` declaration on the root-decorator stack.
#[derive(Debug, Clone)]
pub struct RootSchemaDecl {
    /// Schema identifier introduced into the root scope (`@schema(Req=...)`
    /// → `"Req"`).
    pub name: String,
    /// Source range of the name token (for diagnostics).
    pub name_range: TokenRange,
    /// AST node of the schema body — a `Dict` literal or an `Enum<...>`
    /// type expression. The evaluator builds a `Value::Schema` from this
    /// node the same way it does for dict-field `@schema X: {...}`.
    pub schema_node: Arc<Node>,
    /// Source range of the entire decorator (for diagnostics).
    pub decorator_range: TokenRange,
}

/// Walk the root node's decorators and append every well-formed
/// `@schema(Name=...)` to `tree.root_schemas`. Malformed entries and
/// duplicate names emit diagnostics; collisions with field-form
/// `@schema X: {...}` are also reported here so the user gets one clean
/// error per name.
pub fn collect_root_schemas(root: &Node, tree: &mut AnalyzedTree) {
    let mut seen: HashMap<String, TokenRange> = HashMap::new();
    for dec in &root.decorators {
        if !is_schema_decorator(dec) {
            continue;
        }
        // Field-form `@schema` (no args) is the regular dict-field
        // marker; the root-decorator pass only fires on the
        // *args-bearing* form.
        if dec.args.is_empty() {
            tree.diagnostics.push(Diagnostic::RootSchemaDecoratorEmpty {
                range: span_of(dec.range),
            });
            continue;
        }
        for arg in &dec.args {
            let Some(name) = &arg.name else {
                tree.diagnostics
                    .push(Diagnostic::RootSchemaDecoratorMissingName {
                        range: span_of(arg.value.range),
                    });
                continue;
            };
            // Validate the value's shape: only `Dict { ... }` or an
            // `Enum<...>` type expression are accepted as a schema body.
            // Anything else (literals, numbers, references) would not
            // produce a schema at runtime, so reject it eagerly.
            let valid = matches!(arg.value.expr.as_ref(), Expr::Dict(_))
                || matches!(arg.value.expr.as_ref(),
                    Expr::Type(t) if t.path.len() == 1 && t.path[0] == "Enum");
            if !valid {
                tree.diagnostics.push(Diagnostic::RootSchemaInvalidValue {
                    name: name.clone(),
                    found_type: arg.value.expr.kind().to_string(),
                    range: span_of(arg.value.range),
                });
                continue;
            }
            if let Some(prev_range) = seen.get(name) {
                tree.diagnostics.push(Diagnostic::DuplicateRootSchemaName {
                    name: name.clone(),
                    first: span_of(*prev_range),
                    second: span_of(dec.range),
                });
                continue;
            }
            seen.insert(name.clone(), arg.value.range);
            tree.root_schemas.push(RootSchemaDecl {
                name: name.clone(),
                name_range: arg.value.range,
                schema_node: Arc::new(arg.value.clone()),
                decorator_range: dec.range,
            });
            track_node(&arg.value, tree);
        }
    }

    // Collision check against dict-field `@schema X: {...}`. We do this
    // here (rather than as a separate pass) because both side-tables are
    // already populated by the time this fires: `collect_schemas` runs
    // before us in the analyzer pipeline. For each root-form decl, look
    // up a same-named field schema; if one exists, emit the dual-form
    // error and let the rest of the pipeline keep running.
    if !tree.root_schemas.is_empty() && !tree.schemas.is_empty() {
        // Build a name→range map for field-form schemas so the error
        // points at both declaration sites.
        let field_names: HashMap<String, TokenRange> = tree
            .schemas
            .values()
            .filter_map(|def| def.name.clone().map(|n| (n, def.range)))
            .collect();
        let collisions: Vec<(String, TokenRange, TokenRange)> = tree
            .root_schemas
            .iter()
            .filter_map(|decl| {
                field_names
                    .get(&decl.name)
                    .map(|fr| (decl.name.clone(), decl.decorator_range, *fr))
            })
            .collect();
        for (name, root_range, field_range) in collisions {
            tree.diagnostics
                .push(Diagnostic::RootSchemaCollidesWithField {
                    name,
                    root_range: span_of(root_range),
                    field_range: span_of(field_range),
                });
        }
    }
}

fn is_schema_decorator(dec: &Decorator) -> bool {
    dec.path.len() == 1 && matches!(&dec.path[0], TokenKey::String(s, _, _) if s == SCHEMA)
}

/// Mirror the schema-body node into `tree.node_index` so the evaluator
/// can recover it through `analyzer_target` once eval starts. Same shape
/// as `inputs::collect_inputs::track_node`.
fn track_node(node: &Node, tree: &mut AnalyzedTree) {
    insert_into_index(node, tree);
}

fn insert_into_index(node: &Node, tree: &mut AnalyzedTree) {
    tree.node_index
        .entry(node.id)
        .or_insert_with(|| Arc::new(node.clone()));
    for child in relon_parser::child_nodes(node) {
        insert_into_index(child, tree);
    }
}
