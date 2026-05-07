//! `@input(name=SchemaRef)` collection pass.
//!
//! Walks the root document's decorators and gathers every `@input(...)`
//! declaration into [`AnalyzedTree::input_decls`]. Each decoration adds
//! a named slot to the program's input contract; the runtime later
//! merges the slots into a virtual wrapper schema and validates the
//! host-pushed value against it before evaluation begins.
//!
//! Validation of the actual schema referenced by each `SchemaRef` is
//! deferred to runtime — the analyzer only checks the *shape* of the
//! decoration (named arg present, no duplicate slot name).

use crate::decorator_names::INPUT;
use crate::diagnostic::{span_of, Diagnostic};
use crate::tree::AnalyzedTree;
use relon_parser::{Decorator, Node, TokenKey, TokenRange};
use std::collections::HashMap;
use std::sync::Arc;

/// One `@input(name=SchemaRef)` declaration.
#[derive(Debug, Clone)]
pub struct InputDecl {
    /// Slot name within the merged `input` wrapper. Resolves to
    /// `input.<name>` in the script.
    pub name: String,
    /// Source range of the decoration's name token (for diagnostics).
    pub name_range: TokenRange,
    /// AST node of the schema-reference expression. The runtime
    /// evaluates this node against the root scope to obtain the
    /// `Value::Schema` used to validate the slot.
    pub schema_ref: Arc<Node>,
    /// Source range of the entire decorator (for diagnostics).
    pub decorator_range: TokenRange,
}

/// Walk the root node's decorators and append every well-formed
/// `@input(...)` to `tree.input_decls`. Malformed decorations and
/// duplicate slot names emit diagnostics; well-formed ones land in the
/// table even when the file has other errors so downstream passes see
/// a consistent picture.
pub fn collect_inputs(root: &Node, tree: &mut AnalyzedTree) {
    let mut seen: HashMap<String, TokenRange> = HashMap::new();
    for dec in &root.decorators {
        if !is_input_decorator(dec) {
            continue;
        }
        let mut had_arg = false;
        for arg in &dec.args {
            had_arg = true;
            let Some(name) = &arg.name else {
                tree.diagnostics
                    .push(Diagnostic::InputDecoratorMissingName {
                        range: span_of(arg.value.range),
                    });
                continue;
            };
            if let Some(prev_range) = seen.get(name) {
                tree.diagnostics.push(Diagnostic::DuplicateInputName {
                    name: name.clone(),
                    first: span_of(*prev_range),
                    second: span_of(dec.range),
                });
                continue;
            }
            // Use the value-node range as a stand-in for the named-arg
            // span — the parser doesn't carry a separate range for the
            // `name=` portion today.
            seen.insert(name.clone(), arg.value.range);
            tree.input_decls.push(InputDecl {
                name: name.clone(),
                name_range: arg.value.range,
                schema_ref: Arc::new(arg.value.clone()),
                decorator_range: dec.range,
            });
            track_node(&arg.value, tree);
        }
        if !had_arg {
            tree.diagnostics.push(Diagnostic::InputDecoratorEmpty {
                range: span_of(dec.range),
            });
        }
    }
}

fn is_input_decorator(dec: &Decorator) -> bool {
    dec.path.len() == 1 && matches!(&dec.path[0], TokenKey::String(s, _, _) if s == INPUT)
}

/// Mirror the schema-ref node into `tree.node_index` so the evaluator
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
