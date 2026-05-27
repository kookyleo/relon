//! Root-level `#schema Name Body, ...` directive collection pass.
//!
//! Layout sugar that lets users co-locate schema declarations with the
//! root entry directive instead of stuffing them inside the root dict
//! body. Semantically equivalent to declaring each `Name` as a `#internal
//! #schema` field of the root dict — once the pass has registered the
//! schemas, root-level references to them resolve the same way as
//! dict-field `#schema X {...}` (renamed: `#schema X {...}` at field
//! level is the same form as a dict field).
//!
//! ```relon
//! #schema Req { String name: *, Int retries: * }
//! #main(Req req)
//! { greeting: f"hello ${req.name}" }
//! ```
//!
//! Validation here is structural only: shape of the directive (parser
//! has already enforced `<ident> : <expr>`), no duplicates within the
//! root-directive stack, and no collision with a dict-field `#schema`
//! of the same name.

use crate::diagnostic::{span_of, Diagnostic};
use crate::directive_names::SCHEMA;
use crate::schema::{lower_schema_pure_with, record_schema_methods};
use crate::tree::AnalyzedTree;
use relon_parser::{DirectiveBody, Expr, Node, Operator, TokenRange};
use std::collections::HashMap;
use std::sync::Arc;

/// One `#schema Name Body` entry on the root-directive stack.
#[derive(Debug, Clone)]
pub struct RootSchemaDecl {
    /// Schema identifier introduced into the root scope.
    pub name: String,
    /// Source range of the name token (for diagnostics).
    pub name_range: TokenRange,
    /// v1.8+ fix (issue 4): generic parameter names declared on the
    /// directive header (`#schema Box<T, U> { ... }` → `["T", "U"]`).
    /// Pre-fix this was dropped here and the runtime fell back to
    /// `Vec::new()` when seeding the schema, so `Box<Int>` couldn't
    /// substitute `T → Int` and the analyzer reported field types
    /// like `T` as `UnknownTypeName`.
    pub generics: Vec<String>,
    /// AST node of the schema body — a `Dict` literal or an `Enum<...>`
    /// type expression. The evaluator builds a `Value::Schema` from this
    /// node the same way it does for dict-field `#schema X : {...}`.
    pub schema_node: Arc<Node>,
    /// Source range of the entire directive (for diagnostics).
    pub directive_range: TokenRange,
}

/// Walk the root node's directives and append every well-formed
/// `#schema Name Body` to `tree.root_schemas`. Malformed entries and
/// duplicate names emit diagnostics; collisions with dict-field
/// `#schema X: { ... }` are also reported here so the user gets one
/// clean error per name.
pub fn collect_root_schemas(root: &Node, tree: &mut AnalyzedTree) {
    let mut seen: HashMap<String, TokenRange> = HashMap::new();
    for dir in &root.directives {
        if dir.name != SCHEMA {
            continue;
        }
        let DirectiveBody::NameBody {
            name,
            name_range,
            generics,
            body,
            methods,
            schema_no_auto_derives,
        } = &dir.body
        else {
            continue;
        };
        // Validate the value's shape: only `Dict { ... }` or an
        // `Enum<...>` type expression are accepted as a schema body.
        // Anything else (literals, numbers, references) would not
        // produce a schema at runtime, so reject it eagerly.
        // Accepted body shapes:
        //   * `Dict { ... }` literal — a struct schema body.
        //   * `Enum<...>` type expression — a sum-type schema body.
        //   * `Base + { ... }` (or chained) — base-schema composition.
        //   * Plain reference / variable — an alias for an existing
        //     schema (validated at runtime by `lower_schema_pure`).
        let valid = matches!(body.expr.as_ref(), Expr::Dict(_))
            || matches!(body.expr.as_ref(),
                Expr::Type(t) if t.path.len() == 1 && t.path[0] == "Enum")
            || matches!(body.expr.as_ref(), Expr::Binary(Operator::Add, _, _))
            || matches!(
                body.expr.as_ref(),
                Expr::Variable(_) | Expr::Reference { .. }
            );
        if !valid {
            tree.diagnostics.push(Diagnostic::RootSchemaInvalidValue {
                name: name.clone(),
                found_type: body.expr.kind().to_string(),
                range: span_of(body.range),
            });
            continue;
        }
        if let Some(prev_range) = seen.get(name) {
            tree.diagnostics.push(Diagnostic::DuplicateRootSchemaName {
                name: name.clone(),
                first: span_of(*prev_range),
                second: span_of(dir.range),
            });
            continue;
        }
        seen.insert(name.clone(), body.range);
        tree.root_schemas.push(RootSchemaDecl {
            name: name.clone(),
            name_range: *name_range,
            generics: generics.clone(),
            schema_node: Arc::new((**body).clone()),
            directive_range: dir.range,
        });
        // Lower the body so per-field diagnostics
        // (`SchemaFieldUntyped`, `HeterogeneousEnum`, ...) surface
        // for root-level schemas the same way they do for nested
        // ones. The lowering result also lands in `tree.schemas` —
        // keyed by the body node id — so downstream consumers can
        // treat root-form and dict-field-form uniformly.
        let (def, diags) = lower_schema_pure_with(
            Some(name.clone()),
            generics.clone(),
            body,
            methods,
            schema_no_auto_derives,
        );
        tree.diagnostics.extend(diags);
        if let Some(def) = def {
            record_schema_methods(&def, tree);
            tree.schemas.insert(body.id, def);
        }
        track_node(body, tree);
    }

    // Collision check against nested-dict `#schema X {...}`. The
    // root pass also seeds entries into `tree.schemas` (so callers can
    // look up by node id uniformly); skip those by remembering the
    // body-node ranges the root pass owns.
    if !tree.root_schemas.is_empty() {
        let mut root_names: HashMap<String, (TokenRange, TokenRange)> = HashMap::new();
        for decl in &tree.root_schemas {
            root_names.insert(
                decl.name.clone(),
                (decl.directive_range, decl.schema_node.range),
            );
        }
        let collisions: Vec<(String, TokenRange, TokenRange)> = tree
            .schemas
            .values()
            .filter_map(|def| {
                let name = def.name.clone()?;
                let (root_range, root_body_range) = root_names.get(&name).copied()?;
                if def.range == root_body_range {
                    return None;
                }
                Some((name, root_range, def.range))
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

/// Mirror the schema-body node into `tree.node_index` so the evaluator
/// can recover it through `analyzer_target` once eval starts.
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
