//! Complete sub-module: kind-driven and fixed-list candidate sources.
//!
//! These collectors don't walk lexical scope. Instead each one emits
//! a fixed family of completions: directive / pragma keywords,
//! reference base names, decorator candidates from sibling closures,
//! stdlib function names, schema names (either from a workspace-
//! analyzed tree or from `#schema` declarations in a partial AST),
//! `#import` aliases / destructured bindings, generic type variables
//! visible at the cursor, and the primitive type names.
//!
//! Also hosts the two snippet builders ([`call_snippet`] for normal
//! `name(args)` calls and [`decorator_snippet`] for `@name(args)`
//! where the closure's first param is implicitly bound to the
//! decorated value).

use super::scope::collect_callable_pairs_in_scope;
use super::{CompletionItem, CompletionKind};
use crate::stdlib_signatures::stdlib_fn_names;
use crate::tree::AnalyzedTree;
use relon_parser::Node;

/// Primitive type names available everywhere. Surfaced in Type
/// context and as supplementary candidates in Bare so the user gets
/// type completion regardless of whether the byte-level classifier
/// caught the slot — capital-letter prefix filtering on the client
/// keeps the list focused.
pub(super) fn push_type_primitive_candidates(items: &mut Vec<CompletionItem>) {
    for name in &["Null", "Bool", "Int", "Float", "String", "List", "Dict"] {
        items.push(CompletionItem {
            label: (*name).into(),
            kind: CompletionKind::Schema,
            detail: Some("primitive".into()),
            apply_snippet: None,
        });
    }
}

/// Schema names visible in a partial AST. Walks `node.directives`
/// for `#schema X[<T, ...>]` declarations and emits each as a Schema
/// candidate. The strict path uses `push_schema_candidates` against
/// the workspace-analyzed tree instead.
pub(super) fn push_schema_candidates_partial(items: &mut Vec<CompletionItem>, root: &Node) {
    use relon_parser::DirectiveBody;
    fn visit(node: &Node, items: &mut Vec<CompletionItem>) {
        for dir in &node.directives {
            if dir.name == "schema" {
                if let DirectiveBody::NameBody { name, .. } = &dir.body {
                    items.push(CompletionItem {
                        label: name.clone(),
                        kind: CompletionKind::Schema,
                        detail: Some("schema".into()),
                        apply_snippet: None,
                    });
                }
            }
        }
        for child in super::children_of(node) {
            visit(child, items);
        }
    }
    visit(root, items);
}

/// Generic type variables visible at the cursor. A `#schema X<T, U>`
/// puts `T` and `U` in scope inside the schema body; the partial
/// walker harvests them whenever the cursor sits inside the schema's
/// range. Helps complete things like `Result<│>`.
pub(super) fn push_generic_var_candidates_partial(
    items: &mut Vec<CompletionItem>,
    root: &Node,
    offset: usize,
) {
    use relon_parser::DirectiveBody;
    fn visit(node: &Node, offset: usize, items: &mut Vec<CompletionItem>) {
        if node.range.start.offset > offset || offset > node.range.end.offset {
            return;
        }
        for dir in &node.directives {
            if let DirectiveBody::NameBody { generics, .. } = &dir.body {
                for g in generics {
                    items.push(CompletionItem {
                        label: g.clone(),
                        kind: CompletionKind::Schema,
                        detail: Some("type var".into()),
                        apply_snippet: None,
                    });
                }
            }
        }
        for child in super::children_of(node) {
            visit(child, offset, items);
        }
    }
    visit(root, offset, items);
}

pub(super) fn push_stdlib_candidates(items: &mut Vec<CompletionItem>) {
    use crate::stdlib_signatures::stdlib_signatures;
    let sigs = stdlib_signatures();
    for name in stdlib_fn_names() {
        let apply_snippet = sigs.get(name).map(|sig| {
            let param_names: Vec<String> = sig.params.iter().map(|p| p.name.clone()).collect();
            call_snippet(name, &param_names)
        });
        items.push(CompletionItem {
            label: name.to_string(),
            kind: CompletionKind::Stdlib,
            detail: Some("stdlib".to_string()),
            apply_snippet,
        });
    }
}

pub(super) fn push_schema_candidates(items: &mut Vec<CompletionItem>, tree: &AnalyzedTree) {
    for def in tree.schemas.values() {
        if let Some(name) = &def.name {
            items.push(CompletionItem {
                label: name.clone(),
                kind: CompletionKind::Schema,
                detail: Some("schema".to_string()),
                apply_snippet: None,
            });
        }
    }
    for decl in &tree.root_schemas {
        items.push(CompletionItem {
            label: decl.name.clone(),
            kind: CompletionKind::Schema,
            detail: Some("schema".to_string()),
            apply_snippet: None,
        });
    }
}

pub(super) fn push_import_binding_candidates(items: &mut Vec<CompletionItem>, tree: &AnalyzedTree) {
    for imp in &tree.imports {
        if let Some(alias) = &imp.alias {
            items.push(CompletionItem {
                label: alias.clone(),
                kind: CompletionKind::Module,
                detail: imp.path.clone(),
                apply_snippet: None,
            });
        }
        for (name, local) in &imp.destructure {
            let label = local.clone().unwrap_or_else(|| name.clone());
            items.push(CompletionItem {
                label,
                kind: CompletionKind::Import,
                detail: imp.path.clone(),
                apply_snippet: None,
            });
        }
        // Spread imports are visible by their downstream name; we
        // don't know the names without the module's analyzed tree.
        // Member-access completion handles those via push_member.
    }
}

pub(super) fn push_reference_candidates(items: &mut Vec<CompletionItem>, in_list: bool) {
    // Always-available refs.
    for (name, detail) in &[
        ("root", "document root"),
        ("sibling", "enclosing dict"),
        ("uncle", "enclosing-enclosing dict"),
        ("this", "current value (inside list)"),
    ] {
        items.push(CompletionItem {
            label: (*name).into(),
            kind: CompletionKind::Reference,
            detail: Some((*detail).into()),
            apply_snippet: None,
        });
    }
    // Iteration-only refs — only meaningful inside a List or
    // Comprehension. Outside, they always emit an `IterationRefOutside
    // List` diagnostic.
    if in_list {
        for (name, detail) in &[
            ("prev", "previous element (inside list)"),
            ("next", "next element (inside list)"),
            ("index", "element index (inside list)"),
        ] {
            items.push(CompletionItem {
                label: (*name).into(),
                kind: CompletionKind::Reference,
                detail: Some((*detail).into()),
                apply_snippet: None,
            });
        }
    }
}

pub(super) fn push_directive_candidates(items: &mut Vec<CompletionItem>) {
    // Top-level block directives. Each shape is a fixed grammar so we
    // can emit canonical snippets with tab stops — Tab on `#schema`
    // lands the user inside the body, not just on the bare name.
    for (name, snippet) in [
        ("schema", "schema ${1:Name} { ${0} }"),
        ("extend", "extend ${1:Target} { ${0} }"),
        ("main", "main(${1:Type param}) -> ${0:Return}"),
        ("import", "import ${1:bindings} from \"${0:path}\""),
        ("relaxed", "relaxed"),
        ("unstrict", "unstrict"),
    ] {
        items.push(CompletionItem {
            label: name.into(),
            kind: CompletionKind::Directive,
            detail: Some("directive".into()),
            apply_snippet: Some(snippet.into()),
        });
    }
    // Pair-level pragmas — same `#` prefix, different positions.
    // `Bare` shapes don't carry a tab stop; `Value` shapes leave the
    // cursor at the argument so the user just types the payload.
    for (name, snippet) in [
        ("internal", "internal"),
        ("expect", "expect ${0:\"message\"}"),
        ("default", "default ${0:value}"),
        ("brand", "brand ${0:TypeName}"),
        ("derive", "derive ${0:Constraint}"),
        ("native", "native"),
        ("no_auto_derive", "no_auto_derive"),
    ] {
        items.push(CompletionItem {
            label: name.into(),
            kind: CompletionKind::Pragma,
            detail: Some("pragma".into()),
            apply_snippet: Some(snippet.into()),
        });
    }
}

pub(super) fn push_decorator_candidates(
    items: &mut Vec<CompletionItem>,
    root: &Node,
    offset: usize,
) {
    // No host decorator registry in v1, so we surface every visible
    // closure-valued pair (the user-defined hook shape — `pricing` uses
    // `@currency(...)` where `currency` is a sibling method).
    //
    // Decorators auto-receive the decorated field's value as their
    // first argument, so the snippet skips the closure's first param
    // and only exposes the remaining ones as tab stops.
    let candidates = collect_callable_pairs_in_scope(root, offset);
    for (name, params) in candidates {
        let snippet = decorator_snippet(&name, &params);
        items.push(CompletionItem {
            label: name,
            kind: CompletionKind::Decorator,
            detail: Some("decorator".to_string()),
            apply_snippet: Some(snippet),
        });
    }
}

/// Build a CodeMirror-compatible snippet for a decorator invocation.
/// Skips the closure's first param (the auto-passed field value);
/// the remaining params become `${N:name}` tab stops. Zero remaining
/// params produces `name()` with the final cursor inside the parens.
fn decorator_snippet(name: &str, params: &[String]) -> String {
    // Skip the first param (auto-bound to the decorated value).
    let exposed: &[String] = if params.is_empty() { &[] } else { &params[1..] };
    if exposed.is_empty() {
        // No user-facing params — leave cursor between the parens.
        format!("{}(${{0}})", name)
    } else {
        let body: Vec<String> = exposed
            .iter()
            .enumerate()
            .map(|(i, p)| format!("${{{}:{}}}", i + 1, p))
            .collect();
        format!("{}({})", name, body.join(", "))
    }
}

/// Build a snippet for a regular function/method call (member access,
/// stdlib). Every param becomes a tab stop with its name as default
/// placeholder text; the final cursor lands after the closing paren.
pub(super) fn call_snippet(name: &str, params: &[String]) -> String {
    if params.is_empty() {
        return format!("{}()", name);
    }
    let body: Vec<String> = params
        .iter()
        .enumerate()
        .map(|(i, p)| format!("${{{}:{}}}", i + 1, p))
        .collect();
    format!("{}({})", name, body.join(", "))
}
