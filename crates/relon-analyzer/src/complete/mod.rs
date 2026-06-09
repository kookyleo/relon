//! Cursor-position autocomplete resolver.
//!
//! Mirrors [`crate::goto_def::resolve`] in shape: same inputs (source,
//! parsed root, analyzed tree, optional workspace), same position
//! semantics (UTF-16 line/character), but returns a list of candidate
//! [`CompletionItem`]s instead of a single jump target.
//!
//! What we suggest depends on the cursor's *immediate prefix*:
//!
//!   - `#│` — top-level directive names (`schema`, `extend`, `main`,
//!     `import`, …) plus pair-level pragmas (`private`, `expect`, …).
//!   - `&│` — reference vars (`&root`, `&sibling`, `&uncle`, `&this`).
//!     Iteration-only refs are gated to inside-list contexts.
//!   - `@│` — decorator names (currently just emits the user-defined
//!     methods from sibling closures + host names; v1.0 doesn't have
//!     a host-registered decorator registry).
//!   - `ident.│` (member access) — exported names of the module
//!     bound to `ident`, when it's an `#import lib from "..."` alias.
//!     Cross-module destructure imports aren't member-accessed.
//!   - bare identifier — scope-based: closure params, sibling /
//!     ancestor pair keys, destructured/spread import bindings,
//!     `#schema` names, stdlib fns.
//!
//! The caller (LSP / WASM) filters by prefix; this module always
//! returns the full set so the client can re-rank or augment.
//!
//! ## Sub-module split
//!
//! Free functions, grouped by what they answer:
//!
//! - **`cursor`** — `classify_cursor` + the byte-level helpers that
//!   detect type-slot contexts (`preceded_by_type_head`,
//!   `inside_generic_args`, `at_field_start`, `after_arrow`).
//! - **`scope`** — bare / identifier scope walking
//!   (`walk_scope`, `push_scope_candidates*`, `is_inside_list`,
//!   `collect_callable_pairs_in_scope`, `find_named_in_scope`).
//! - **`member`** — `ident.X` member-access candidates
//!   (`push_member_candidates*`).
//! - **`keywords`** — fixed / kind-driven candidate lists
//!   (directives, references, decorators, stdlib, schemas, imports,
//!   generic vars, type primitives), plus the snippet builders
//!   (`call_snippet`, `decorator_snippet`).

mod cursor;
mod keywords;
mod member;
mod scope;

#[cfg(test)]
mod tests;

use crate::tree::AnalyzedTree;
use crate::workspace::WorkspaceTree;
use relon_parser::{Node, ParsedDocument};

/// One entry in the completion candidate list.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompletionItem {
    pub label: String,
    pub kind: CompletionKind,
    /// Short label shown to the right of the suggestion (e.g.
    /// `"method"`, `"stdlib"`, `"import"`). Optional — clients fall
    /// back to a generic "Identifier" label when absent.
    pub detail: Option<String>,
    /// Snippet text inserted when the user accepts the suggestion,
    /// using `${N:placeholder}` tab-stop syntax. `None` means insert
    /// the bare label. Callable kinds (decorators, methods, stdlib
    /// functions) populate this so a Tab landing on `@currency`
    /// expands to `@currency(${1:symbol})` instead of leaving the
    /// user to type the parens.
    pub apply_snippet: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionKind {
    /// `name(params): body` — a closure-valued pair.
    Method,
    /// `name: value` — a non-closure pair.
    Field,
    /// Closure parameter binding.
    Parameter,
    /// `#schema X { ... }` — a schema-name.
    Schema,
    /// stdlib builtin (`len`, `_list_reduce`, …).
    Stdlib,
    /// `#import lib from "..."` — module alias.
    Module,
    /// Visible binding from a destructure or spread import.
    Import,
    /// `&root` / `&sibling` / `&uncle` / `&this` / `&prev` / `&next` /
    /// `&index`.
    Reference,
    /// Top-level `#` directive (`schema`, `extend`, `main`, `import`).
    Directive,
    /// Pair-level `#` pragma (`private`, `expect`, `default`, `brand`,
    /// `derive`, `native`, …).
    Pragma,
    /// Decorator (just `@name`).
    Decorator,
    /// Reserved word (`for`, `in`, `if`, `else`, `true`, `false`).
    Keyword,
}

/// What's immediately to the left of the cursor — drives which
/// candidate categories make sense. Computed by scanning the source
/// bytes; doesn't require a re-parse so it survives unfinished input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CursorContext {
    /// User just typed `#` (or `#partial-name`). Suggest directive /
    /// pragma names.
    Directive { prefix: String },
    /// User just typed `@` (or `@partial-name`). Suggest decorator names.
    Decorator { prefix: String },
    /// User just typed `&` (or `&partial-name`). Suggest reference
    /// vars. `in_list` controls whether iteration-only refs are
    /// included.
    Reference { prefix: String },
    /// User typed `ident.` (a single segment) and is now completing
    /// the part after the dot. `head` is the identifier before the
    /// dot. The cursor may be mid-suffix (`lib.fo│`); we ignore the
    /// suffix for membership lookup and let the client filter.
    Member { head: String, suffix: String },
    /// Bare identifier completion — the default. `prefix` is the
    /// in-progress word, used only as the human-visible filter hint.
    Bare { prefix: String },
    /// Cursor sits in a type-expression slot: inside generic args
    /// (`Foo<│>`), after a closure return arrow (`(p) -> │`), or
    /// after a typed-spread star (`*│`). Suggest primitive type
    /// names, in-scope schema names, and visible generic type vars.
    Type { prefix: String },
}

/// Partial-AST completion entry point. Drives IDE completion when
/// the workspace analyzer couldn't produce an `AnalyzedTree` for the
/// file — typically because the user is mid-edit (`{ a:│`, `&│`,
/// `f"hi ${│`, etc.). Routes through the recovering parser so a
/// partial root expression is still available where the CST could
/// recover; falls back to source-byte classification only when
/// neither the partial AST nor the cursor context yields useful
/// candidates.
///
/// Caller responsibility: pass [`relon_parser::parse_document_recovering`]
/// output. Don't call [`relon_parser::parse_document`] in front of
/// this — recovery is the whole point.
pub fn resolve_recovering(
    source: &str,
    parsed: &ParsedDocument,
    line: u32,
    character: u32,
) -> Vec<CompletionItem> {
    let offset = crate::goto_def::position_to_offset(source, line, character);
    let context = cursor::classify_cursor(source, offset);
    let partial_root = parsed.nodes.first();
    let in_list = partial_root
        .map(|root| scope::is_inside_list(root, offset))
        .unwrap_or(false);

    let mut items: Vec<CompletionItem> = Vec::new();
    match &context {
        CursorContext::Directive { .. } => keywords::push_directive_candidates(&mut items),
        CursorContext::Reference { .. } => keywords::push_reference_candidates(&mut items, in_list),
        CursorContext::Decorator { .. } => {
            // Best-effort scope walk for decorator candidates. With a
            // partial root we can still surface sibling closures.
            if let Some(root) = partial_root {
                keywords::push_decorator_candidates(&mut items, root, offset);
            }
        }
        CursorContext::Member { head, .. } => {
            if let Some(root) = partial_root {
                member::push_member_candidates_partial(&mut items, root, head, offset);
            }
        }
        CursorContext::Bare { .. } => {
            if let Some(root) = partial_root {
                scope::push_scope_candidates_partial(&mut items, root, offset);
                keywords::push_schema_candidates_partial(&mut items, root);
            }
            keywords::push_stdlib_candidates(&mut items);
        }
        CursorContext::Type { .. } => {
            keywords::push_type_primitive_candidates(&mut items);
            if let Some(root) = partial_root {
                keywords::push_schema_candidates_partial(&mut items, root);
                keywords::push_generic_var_candidates_partial(&mut items, root, offset);
            }
        }
    }

    // Dedupe — same logic as `resolve`.
    let mut seen: std::collections::HashSet<(String, CompletionKind)> =
        std::collections::HashSet::new();
    items.retain(|item| seen.insert((item.label.clone(), item.kind)));
    items
}

/// Legacy parse-free fallback retained for callers that don't yet
/// route through [`resolve_recovering`]. Forwards to the recovering
/// entry with an empty partial AST so the result matches the old
/// behaviour: directive / reference keyword lists for `#` / `&`,
/// empty for bare / member contexts (no AST means no scope).
///
/// New callers should use [`resolve_recovering`] directly with a
/// real [`ParsedDocument`] — it offers context-aware completion
/// where the legacy fallback could only offer the static keyword
/// lists.
pub fn keywords_for_cursor(source: &str, line: u32, character: u32) -> Vec<CompletionItem> {
    let empty = ParsedDocument {
        nodes: Vec::new(),
        diagnostics: Vec::new(),
    };
    resolve_recovering(source, &empty, line, character)
}

/// Public entry point — mirrors [`crate::goto_def::resolve`] in
/// signature. Returns every candidate the resolver thinks is
/// reasonable for the cursor position; the LSP / WASM adapter wraps
/// these into protocol-shaped items.
pub fn resolve(
    entry_source: &str,
    entry_root: &Node,
    entry_tree: &AnalyzedTree,
    workspace: Option<&WorkspaceTree>,
    line: u32,
    character: u32,
) -> Vec<CompletionItem> {
    let offset = crate::goto_def::position_to_offset(entry_source, line, character);
    let context = cursor::classify_cursor(entry_source, offset);
    let in_list = scope::is_inside_list(entry_root, offset);

    let mut items: Vec<CompletionItem> = Vec::new();

    match &context {
        CursorContext::Directive { .. } => keywords::push_directive_candidates(&mut items),
        CursorContext::Decorator { .. } => {
            keywords::push_decorator_candidates(&mut items, entry_root, offset)
        }
        CursorContext::Reference { .. } => keywords::push_reference_candidates(&mut items, in_list),
        CursorContext::Member { head, .. } => {
            member::push_member_candidates(&mut items, head, entry_tree, workspace);
        }
        CursorContext::Bare { .. } => {
            scope::push_scope_candidates(&mut items, entry_root, entry_tree, offset);
            keywords::push_stdlib_candidates(&mut items);
            keywords::push_schema_candidates(&mut items, entry_tree);
            keywords::push_import_binding_candidates(&mut items, entry_tree);
        }
        CursorContext::Type { .. } => {
            keywords::push_type_primitive_candidates(&mut items);
            keywords::push_schema_candidates(&mut items, entry_tree);
            keywords::push_generic_var_candidates_partial(&mut items, entry_root, offset);
        }
    }

    // Dedupe while preserving order — keys may be inserted by multiple
    // collectors (e.g. a sibling pair name + a schema name).
    let mut seen: std::collections::HashSet<(String, CompletionKind)> =
        std::collections::HashSet::new();
    items.retain(|item| seen.insert((item.label.clone(), item.kind)));
    items
}

// =====================================================================
// Shared low-level helpers used across multiple sub-modules.
// =====================================================================

pub(crate) fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

pub(crate) fn children_of(node: &Node) -> Vec<&Node> {
    use relon_parser::child_nodes;
    child_nodes(node)
}

pub(crate) fn contains_offset(node: &Node, offset: usize) -> bool {
    node.range.start.offset <= offset && offset <= node.range.end.offset
}
