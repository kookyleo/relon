//! `#main(<type> <ident>, ...) [-> <type>]` collection pass.
//!
//! The `#main(...)` directive declares the file as an **entry program**
//! whose host-pushed arguments must validate against the listed
//! parameters. Every parameter becomes a root-scope local available
//! directly by name (no `input.` prefix). A file without `#main` is a
//! library / static config — importable, evaluable as a `Value`, but
//! not a host-entry. The optional `-> Type` clause declares the
//! expected return type; when absent the entry's return value is left
//! unchecked.
//!
//! This pass walks the root document's directives, picks up at most
//! one `#main(...)` declaration, and stores it in
//! [`AnalyzedTree::main_signature`]. Multiple declarations and
//! parameters missing types are surfaced as analyzer diagnostics.

use crate::diagnostic::{span_of, Diagnostic};
use crate::directive_names::MAIN;
use crate::tree::AnalyzedTree;
use relon_parser::{DirectiveBody, Node, TokenRange, TypeNode};

/// One `<type> <ident>` parameter declared on `#main(...)`.
#[derive(Debug, Clone)]
pub struct MainParam {
    /// Parameter name as used in the body (e.g. `${u.name}`).
    pub name: String,
    /// Declared type. Validated against the host-pushed value at
    /// `Evaluator::run_main` time.
    pub type_node: TypeNode,
    /// Source range of the parameter (for diagnostics).
    pub range: TokenRange,
}

/// Parsed `#main(...)` signature attached to the root node.
#[derive(Debug, Clone)]
pub struct MainSignature {
    /// Parameters in declaration order; the host may push them in any
    /// order (lookup is by name).
    pub params: Vec<MainParam>,
    /// Optional return type declared via `-> Type` after the parameter
    /// list. `None` means the entry's return value is left unchecked.
    pub return_type: Option<TypeNode>,
    /// Source range of the entire `#main(...)` directive.
    pub range: TokenRange,
}

/// Walk the root node's directives and pick up the `#main(...)`
/// signature, if any. At most one declaration is allowed; subsequent
/// ones produce [`Diagnostic::DuplicateMainDirective`]. Each parameter
/// must declare a type — the directive parser already enforces the
/// `<ident> : <type>` shape, so this pass primarily handles the "more
/// than one #main" case.
pub fn collect_main(root: &Node, tree: &mut AnalyzedTree) {
    let mut first: Option<TokenRange> = None;
    for dir in &root.directives {
        if dir.name != MAIN {
            continue;
        }
        let DirectiveBody::Main {
            params: dir_params,
            return_type,
        } = &dir.body
        else {
            continue;
        };
        if let Some(first_range) = first {
            tree.diagnostics.push(Diagnostic::DuplicateMainDirective {
                first: span_of(first_range),
                second: span_of(dir.range),
            });
            continue;
        }
        first = Some(dir.range);

        let params: Vec<MainParam> = dir_params
            .iter()
            .map(|p| MainParam {
                name: p.name.clone(),
                type_node: p.type_node.clone(),
                range: p.name_range,
            })
            .collect();
        tree.main_signature = Some(MainSignature {
            params,
            return_type: return_type.clone(),
            range: dir.range,
        });
    }
}
