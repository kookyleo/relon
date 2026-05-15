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

use crate::stdlib_signatures::stdlib_fn_names;
use crate::tree::AnalyzedTree;
use crate::workspace::WorkspaceTree;
use relon_parser::{Expr, Node, ParsedDocument, TokenKey};

/// One entry in the completion candidate list.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompletionItem {
    pub label: String,
    pub kind: CompletionKind,
    /// Short label shown to the right of the suggestion (e.g.
    /// `"method"`, `"stdlib"`, `"import"`). Optional — clients fall
    /// back to a generic "Identifier" label when absent.
    pub detail: Option<String>,
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
    /// `derive`, `strict`).
    Pragma,
    /// Decorator (just `@name`).
    Decorator,
    /// Reserved word (`for`, `in`, `if`, `else`, `true`, `false`, `null`).
    Keyword,
}

/// What's immediately to the left of the cursor — drives which
/// candidate categories make sense. Computed by scanning the source
/// bytes; doesn't require a re-parse so it survives unfinished input.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CursorContext {
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
    let context = classify_cursor(source, offset);
    let partial_root = parsed.nodes.first();
    let in_list = partial_root
        .map(|root| is_inside_list(root, offset))
        .unwrap_or(false);

    let mut items: Vec<CompletionItem> = Vec::new();
    match &context {
        CursorContext::Directive { .. } => push_directive_candidates(&mut items),
        CursorContext::Reference { .. } => push_reference_candidates(&mut items, in_list),
        CursorContext::Decorator { .. } => {
            // Best-effort scope walk for decorator candidates. With a
            // partial root we can still surface sibling closures.
            if let Some(root) = partial_root {
                push_decorator_candidates(&mut items, root, offset);
            }
        }
        CursorContext::Member { head, .. } => {
            if let Some(root) = partial_root {
                push_member_candidates_partial(&mut items, root, head, offset);
            }
        }
        CursorContext::Bare { .. } => {
            if let Some(root) = partial_root {
                push_scope_candidates_partial(&mut items, root, offset);
                push_schema_candidates_partial(&mut items, root);
            }
            push_stdlib_candidates(&mut items);
        }
        CursorContext::Type { .. } => {
            push_type_primitive_candidates(&mut items);
            if let Some(root) = partial_root {
                push_schema_candidates_partial(&mut items, root);
                push_generic_var_candidates_partial(&mut items, root, offset);
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

/// Mirror of [`push_scope_candidates`] for the partial-AST path.
/// Same scope walk, just without the unused `AnalyzedTree` argument
/// that the workspace-aware `resolve` threads through. Kept separate
/// so the recovering entry never reaches into analyzer-internal
/// machinery a partial parse couldn't populate.
fn push_scope_candidates_partial(items: &mut Vec<CompletionItem>, root: &Node, offset: usize) {
    walk_scope(root, offset, items);
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
    let context = classify_cursor(entry_source, offset);
    let in_list = is_inside_list(entry_root, offset);

    let mut items: Vec<CompletionItem> = Vec::new();

    match &context {
        CursorContext::Directive { .. } => push_directive_candidates(&mut items),
        CursorContext::Decorator { .. } => {
            push_decorator_candidates(&mut items, entry_root, offset)
        }
        CursorContext::Reference { .. } => push_reference_candidates(&mut items, in_list),
        CursorContext::Member { head, .. } => {
            push_member_candidates(&mut items, head, entry_tree, workspace);
        }
        CursorContext::Bare { .. } => {
            push_scope_candidates(&mut items, entry_root, entry_tree, offset);
            push_stdlib_candidates(&mut items);
            push_schema_candidates(&mut items, entry_tree);
            push_import_binding_candidates(&mut items, entry_tree);
        }
        CursorContext::Type { .. } => {
            push_type_primitive_candidates(&mut items);
            push_schema_candidates(&mut items, entry_tree);
            push_generic_var_candidates_partial(&mut items, entry_root, offset);
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
// Cursor context classification.
// =====================================================================

fn classify_cursor(source: &str, offset: usize) -> CursorContext {
    let bytes = source.as_bytes();
    // Anchor: walk back through identifier chars to find the start of
    // the word the user is currently typing.
    let mut word_start = offset.min(bytes.len());
    while word_start > 0 && is_ident_byte(bytes[word_start - 1]) {
        word_start -= 1;
    }
    let suffix = source[word_start..offset.min(source.len())].to_string();

    // Look at the byte immediately before the word.
    let prev = word_start.checked_sub(1).map(|i| bytes[i]);

    match prev {
        Some(b'#') => CursorContext::Directive { prefix: suffix },
        Some(b'@') => CursorContext::Decorator { prefix: suffix },
        Some(b'&') => CursorContext::Reference { prefix: suffix },
        Some(b'<') if preceded_by_type_head(bytes, word_start - 1) => {
            CursorContext::Type { prefix: suffix }
        }
        Some(b',') if inside_generic_args(bytes, word_start - 1) => {
            CursorContext::Type { prefix: suffix }
        }
        Some(b'*') if at_field_start(bytes, word_start - 1) => {
            CursorContext::Type { prefix: suffix }
        }
        Some(b'.') => {
            // Walk back past the dot to grab the head identifier.
            let dot_pos = word_start - 1;
            let mut head_end = dot_pos;
            // Skip whitespace between head and dot (rare but possible).
            while head_end > 0 && bytes[head_end - 1].is_ascii_whitespace() {
                head_end -= 1;
            }
            let mut head_start = head_end;
            while head_start > 0 && is_ident_byte(bytes[head_start - 1]) {
                head_start -= 1;
            }
            // A bare-dot context (no head, e.g. mid-string) falls back
            // to plain bare completion.
            if head_start == head_end {
                CursorContext::Bare { prefix: suffix }
            } else {
                CursorContext::Member {
                    head: source[head_start..head_end].to_string(),
                    suffix,
                }
            }
        }
        _ if after_arrow(bytes, word_start) => CursorContext::Type { prefix: suffix },
        _ => CursorContext::Bare { prefix: suffix },
    }
}

/// `<` is a generic-args opener when the byte just before it is an
/// identifier byte. Differentiates `Foo<│>` from `<` as a less-than
/// operator in arithmetic context — the latter has a number / closing
/// paren / space + identifier just before, not the bare identifier
/// tail required for a type head.
fn preceded_by_type_head(bytes: &[u8], lt_pos: usize) -> bool {
    if lt_pos == 0 {
        return false;
    }
    is_ident_byte(bytes[lt_pos - 1])
}

/// Track whether the cursor sits inside an unbalanced `<...>` opened
/// by a type head. Walks backward balancing `<` / `>` and giving up
/// when an unrelated delimiter (newline, `{`, `}`, `;`) appears
/// before finding the opener — those mark a non-generic context.
fn inside_generic_args(bytes: &[u8], comma_pos: usize) -> bool {
    let mut depth: i32 = 0;
    let mut i = comma_pos;
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b'>' => depth += 1,
            b'<' => {
                if depth == 0 {
                    return preceded_by_type_head(bytes, i);
                }
                depth -= 1;
            }
            b'\n' | b'{' | b'}' | b';' => return false,
            _ => {}
        }
    }
    false
}

/// `*` is a typed-spread marker when it sits at the head of a dict
/// or list field — i.e. the preceding non-whitespace byte is `,`,
/// `{`, `[`, or the start of the file. Inside an expression `*`
/// would be a binary operator and gets routed to Bare context.
fn at_field_start(bytes: &[u8], star_pos: usize) -> bool {
    let mut i = star_pos;
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            b',' | b'{' | b'[' | b'(' => return true,
            _ => return false,
        }
    }
    true
}

/// Detect the `->` arrow position (closure return type). The cursor
/// has just passed any whitespace following the `->`; we walk back
/// over that whitespace and look for the two-byte arrow.
fn after_arrow(bytes: &[u8], word_start: usize) -> bool {
    let mut i = word_start;
    while i > 0 && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
        i -= 1;
    }
    i >= 2 && &bytes[i - 2..i] == b"->"
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Walks the AST and returns `true` when the cursor sits inside a
/// `List(...)` or `Comprehension(...)` expression. Drives the gating
/// of iteration-only reference vars (`&prev`, `&next`, `&index`).
fn is_inside_list(root: &Node, offset: usize) -> bool {
    fn visit(node: &Node, offset: usize, in_list: bool) -> bool {
        if !covers(node, offset) {
            return false;
        }
        let here = matches!(&*node.expr, Expr::List(_) | Expr::Comprehension { .. });
        let nested = in_list || here;
        for c in crate::goto_def::smallest_node_at(node, offset).into_iter() {
            // unused — we just need a deep walk; do recursion manually.
            let _ = c;
        }
        // Manual recursion via the child-walker that handles directive
        // bodies + decorator args, mirroring resolve.rs's scope walker.
        for child in children_of(node) {
            if visit(child, offset, nested) {
                return true;
            }
        }
        nested && covers(node, offset)
    }
    fn covers(node: &Node, offset: usize) -> bool {
        node.range.start.offset <= offset && offset <= node.range.end.offset
    }
    visit(root, offset, false)
}

fn children_of(node: &Node) -> Vec<&Node> {
    use relon_parser::child_nodes;
    child_nodes(node)
}

// =====================================================================
// Scope-based candidates (Bare context).
// =====================================================================

/// Walks the AST from the root toward `offset`, accumulating in-scope
/// names. Innermost names land last; the dedupe pass keeps the first
/// insertion of each `(label, kind)` so outer names are preferred for
/// the visible kind label, but both still appear once.
fn push_scope_candidates(
    items: &mut Vec<CompletionItem>,
    root: &Node,
    tree: &AnalyzedTree,
    offset: usize,
) {
    let _ = tree;
    walk_scope(root, offset, items);
}

fn walk_scope(node: &Node, offset: usize, items: &mut Vec<CompletionItem>) {
    if !contains_offset(node, offset) {
        return;
    }

    // Each enclosing Closure contributes its parameter bindings.
    if let Expr::Closure { params, body, .. } = &*node.expr {
        for p in params {
            items.push(CompletionItem {
                label: p.name.clone(),
                kind: CompletionKind::Parameter,
                detail: Some("param".to_string()),
            });
        }
        // Closure body might itself contain a Dict / Closure / etc.
        walk_scope(body, offset, items);
        return;
    }

    // Each enclosing Dict contributes its pair keys.
    if let Expr::Dict(pairs) = &*node.expr {
        for (key, value) in pairs {
            if let TokenKey::String(name, _, _) = key {
                let kind = if matches!(&*value.expr, Expr::Closure { .. }) {
                    CompletionKind::Method
                } else {
                    CompletionKind::Field
                };
                let detail = if matches!(kind, CompletionKind::Method) {
                    Some("method".to_string())
                } else {
                    Some("field".to_string())
                };
                items.push(CompletionItem {
                    label: name.clone(),
                    kind,
                    detail,
                });
            }
        }
        // Recurse into whichever pair's value contains the cursor.
        for (_, value) in pairs {
            if contains_offset(value, offset) {
                walk_scope(value, offset, items);
            }
        }
        return;
    }

    // Comprehension introduces the iteration variable into scope.
    if let Expr::Comprehension {
        id,
        element,
        iterable,
        condition,
    } = &*node.expr
    {
        items.push(CompletionItem {
            label: id.clone(),
            kind: CompletionKind::Parameter,
            detail: Some("for-binding".to_string()),
        });
        let candidates: [Option<&Node>; 3] = [Some(element), Some(iterable), condition.as_ref()];
        for child in candidates.into_iter().flatten() {
            if contains_offset(child, offset) {
                walk_scope(child, offset, items);
            }
        }
        return;
    }

    // Default: descend into every child whose range covers the cursor.
    // This handles Binary / Ternary / FnCall / FString / etc.
    for child in children_of(node) {
        if contains_offset(child, offset) {
            walk_scope(child, offset, items);
        }
    }
}

fn contains_offset(node: &Node, offset: usize) -> bool {
    node.range.start.offset <= offset && offset <= node.range.end.offset
}

// =====================================================================
// Other category sources.
// =====================================================================

/// Primitive type names available everywhere. Surfaced in Type
/// context and as supplementary candidates in Bare so the user gets
/// type completion regardless of whether the byte-level classifier
/// caught the slot — capital-letter prefix filtering on the client
/// keeps the list focused.
fn push_type_primitive_candidates(items: &mut Vec<CompletionItem>) {
    for name in &[
        "Null", "Bool", "Int", "Float", "String", "List", "Dict",
    ] {
        items.push(CompletionItem {
            label: (*name).into(),
            kind: CompletionKind::Schema,
            detail: Some("primitive".into()),
        });
    }
}

/// Schema names visible in a partial AST. Walks `node.directives`
/// for `#schema X[<T, ...>]` declarations and emits each as a Schema
/// candidate. The strict path uses `push_schema_candidates` against
/// the workspace-analyzed tree instead.
fn push_schema_candidates_partial(items: &mut Vec<CompletionItem>, root: &Node) {
    use relon_parser::DirectiveBody;
    fn visit(node: &Node, items: &mut Vec<CompletionItem>) {
        for dir in &node.directives {
            if dir.name == "schema" {
                if let DirectiveBody::NameBody { name, .. } = &dir.body {
                    items.push(CompletionItem {
                        label: name.clone(),
                        kind: CompletionKind::Schema,
                        detail: Some("schema".into()),
                    });
                }
            }
        }
        for child in children_of(node) {
            visit(child, items);
        }
    }
    visit(root, items);
}

/// Generic type variables visible at the cursor. A `#schema X<T, U>`
/// puts `T` and `U` in scope inside the schema body; the partial
/// walker harvests them whenever the cursor sits inside the schema's
/// range. Helps complete things like `Result<│>`.
fn push_generic_var_candidates_partial(
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
                    });
                }
            }
        }
        for child in children_of(node) {
            visit(child, offset, items);
        }
    }
    visit(root, offset, items);
}

fn push_stdlib_candidates(items: &mut Vec<CompletionItem>) {
    for name in stdlib_fn_names() {
        items.push(CompletionItem {
            label: name.to_string(),
            kind: CompletionKind::Stdlib,
            detail: Some("stdlib".to_string()),
        });
    }
}

fn push_schema_candidates(items: &mut Vec<CompletionItem>, tree: &AnalyzedTree) {
    for def in tree.schemas.values() {
        if let Some(name) = &def.name {
            items.push(CompletionItem {
                label: name.clone(),
                kind: CompletionKind::Schema,
                detail: Some("schema".to_string()),
            });
        }
    }
    for decl in &tree.root_schemas {
        items.push(CompletionItem {
            label: decl.name.clone(),
            kind: CompletionKind::Schema,
            detail: Some("schema".to_string()),
        });
    }
}

fn push_import_binding_candidates(items: &mut Vec<CompletionItem>, tree: &AnalyzedTree) {
    for imp in &tree.imports {
        if let Some(alias) = &imp.alias {
            items.push(CompletionItem {
                label: alias.clone(),
                kind: CompletionKind::Module,
                detail: imp.path.clone(),
            });
        }
        for (name, local) in &imp.destructure {
            let label = local.clone().unwrap_or_else(|| name.clone());
            items.push(CompletionItem {
                label,
                kind: CompletionKind::Import,
                detail: imp.path.clone(),
            });
        }
        // Spread imports are visible by their downstream name; we
        // don't know the names without the module's analyzed tree.
        // Member-access completion handles those via push_member.
    }
}

fn push_reference_candidates(items: &mut Vec<CompletionItem>, in_list: bool) {
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
            });
        }
    }
}

fn push_directive_candidates(items: &mut Vec<CompletionItem>) {
    // Top-level block directives.
    for name in &["schema", "extend", "main", "import", "strict"] {
        items.push(CompletionItem {
            label: (*name).into(),
            kind: CompletionKind::Directive,
            detail: Some("directive".into()),
        });
    }
    // Pair-level pragmas — same `#` prefix, different positions.
    for name in &[
        "private",
        "expect",
        "default",
        "brand",
        "derive",
        "native",
        "no_auto_derive",
    ] {
        items.push(CompletionItem {
            label: (*name).into(),
            kind: CompletionKind::Pragma,
            detail: Some("pragma".into()),
        });
    }
}

fn push_decorator_candidates(items: &mut Vec<CompletionItem>, root: &Node, offset: usize) {
    // No host decorator registry in v1, so we surface every visible
    // closure-valued pair (the user-defined hook shape — `pricing` uses
    // `@currency(...)` where `currency` is a sibling method). Same
    // scope walk as the bare path but filtered to Methods only.
    let mut scope: Vec<CompletionItem> = Vec::new();
    walk_scope(root, offset, &mut scope);
    for item in scope {
        if matches!(item.kind, CompletionKind::Method) {
            items.push(CompletionItem {
                label: item.label,
                kind: CompletionKind::Decorator,
                detail: Some("decorator".to_string()),
            });
        }
    }
}

// =====================================================================
// Member access (Member context).
// =====================================================================

/// `lib.X` completion. `head` is the segment before the dot; we look
/// it up in `tree.imports` and, when it's an alias for another module,
/// pull that module's top-level dict pair keys out of `workspace`.
/// Partial-AST member-access completion. When the user types
/// `name.│`, walk the AST from the root toward the cursor looking
/// for a Dict pair whose key is `head`. If the matching value is
/// itself a Dict, surface every key as a Field / Method candidate.
/// Falls back silently when `head` doesn't resolve — the caller
/// won't see noise from speculative siblings.
fn push_member_candidates_partial(
    items: &mut Vec<CompletionItem>,
    root: &Node,
    head: &str,
    offset: usize,
) {
    if let Some(target) = find_named_in_scope(root, head, offset) {
        if let Expr::Dict(pairs) = &*target.expr {
            for (key, value) in pairs {
                if let TokenKey::String(name, _, _) = key {
                    let kind = if matches!(&*value.expr, Expr::Closure { .. }) {
                        CompletionKind::Method
                    } else {
                        CompletionKind::Field
                    };
                    let detail = if matches!(kind, CompletionKind::Method) {
                        Some("method".to_string())
                    } else {
                        Some("field".to_string())
                    };
                    items.push(CompletionItem {
                        label: name.clone(),
                        kind,
                        detail,
                    });
                }
            }
        }
    }
}

/// Search the AST from `root` for a Dict pair whose key matches
/// `name`, visible from the cursor at `offset`. Walks outward from
/// the innermost enclosing scope so a closer sibling shadows a
/// farther one — same scoping rules as `walk_scope` reads.
fn find_named_in_scope<'a>(root: &'a Node, name: &str, offset: usize) -> Option<&'a Node> {
    // Inner: visit `node` if it covers the cursor, descending into
    // whichever child contains the offset; on the way back up, check
    // each enclosing Dict's pairs. Returns the closest matching pair
    // value.
    fn visit<'a>(node: &'a Node, name: &str, offset: usize) -> Option<&'a Node> {
        if !node_covers(node, offset) {
            return None;
        }
        // Descend first so inner scopes return their match before the
        // outer scope is consulted.
        if let Expr::Dict(pairs) = &*node.expr {
            for (_, value) in pairs {
                if let Some(inner) = visit(value, name, offset) {
                    return Some(inner);
                }
            }
            for (key, value) in pairs {
                if let TokenKey::String(k, _, _) = key {
                    if k == name {
                        return Some(value);
                    }
                }
            }
            return None;
        }
        if let Expr::Closure { body, .. } = &*node.expr {
            if let Some(inner) = visit(body, name, offset) {
                return Some(inner);
            }
            return None;
        }
        // Generic descent for everything else.
        for child in children_of(node) {
            if let Some(inner) = visit(child, name, offset) {
                return Some(inner);
            }
        }
        None
    }
    fn node_covers(node: &Node, offset: usize) -> bool {
        node.range.start.offset <= offset && offset <= node.range.end.offset
    }
    visit(root, name, offset)
}

fn push_member_candidates(
    items: &mut Vec<CompletionItem>,
    head: &str,
    tree: &AnalyzedTree,
    workspace: Option<&WorkspaceTree>,
) {
    // Reference base prefixes (`&root.X` etc.) flow through here when
    // the cursor's prev byte is `.` but the head starts with `&`. We
    // *could* offer ancestor Dict pair keys, but for v1 we only do
    // module-alias member access.

    let import = tree
        .imports
        .iter()
        .find(|imp| imp.alias.as_deref() == Some(head));
    let Some(import) = import else {
        return;
    };
    let Some(path) = &import.path else {
        return;
    };
    let Some(ws) = workspace else {
        return;
    };

    // Look up the imported module's analyzed tree by trying the path
    // directly first; if that misses, try the import graph for a key
    // that ends with the path string.
    let module_id = if ws.modules.contains_key(path) {
        path.clone()
    } else {
        match ws
            .modules
            .keys()
            .find(|k| k.ends_with(path.trim_start_matches("./")))
        {
            Some(k) => k.clone(),
            None => return,
        }
    };

    let module_root = match ws.nodes.get(&module_id) {
        Some(r) => r,
        None => return,
    };

    if let Expr::Dict(pairs) = &*module_root.expr {
        for (key, value) in pairs {
            if let TokenKey::String(name, _, _) = key {
                let kind = if matches!(&*value.expr, Expr::Closure { .. }) {
                    CompletionKind::Method
                } else {
                    CompletionKind::Field
                };
                items.push(CompletionItem {
                    label: name.clone(),
                    kind,
                    detail: Some(format!("from {}", module_id)),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze;
    use relon_parser::parse_document;

    fn complete_at(source: &str, line: u32, character: u32) -> Vec<CompletionItem> {
        let root = parse_document(source).expect("parse");
        let tree = analyze(&root);
        resolve(source, &root, &tree, None, line, character)
    }

    fn labels_with_kind(items: &[CompletionItem], kind: CompletionKind) -> Vec<String> {
        items
            .iter()
            .filter(|i| i.kind == kind)
            .map(|i| i.label.clone())
            .collect()
    }

    fn labels(items: &[CompletionItem]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn suggests_sibling_pair_keys_inside_dict() {
        // Cursor sits inside `baz`'s value (the `3` literal).
        let src = "{\n    foo: 1,\n    bar: 2,\n    baz: 3\n}\n";
        let items = complete_at(src, 3, 9);
        let names = labels(&items);
        assert!(names.iter().any(|l| l == "foo"), "{names:?}");
        assert!(names.iter().any(|l| l == "bar"), "{names:?}");
        assert!(names.iter().any(|l| l == "baz"), "{names:?}");
    }

    #[test]
    fn suggests_closure_params_inside_body() {
        // Cursor sits on the `b` token of `a + b`.
        let src = "{\n    add(a, b): a + b\n}\n";
        let items = complete_at(src, 1, 21);
        let params = labels_with_kind(&items, CompletionKind::Parameter);
        assert!(params.contains(&"a".to_string()), "{params:?}");
        assert!(params.contains(&"b".to_string()), "{params:?}");
    }

    #[test]
    fn directive_context_suggests_directive_names() {
        // Cursor between `#` and `schema` (offset 1). classify_cursor
        // sees prev byte `#` → Directive context.
        let src = "#schema User { String name: * }\n\n{\n    x: 1\n}\n";
        let items = complete_at(src, 0, 1);
        let names = labels_with_kind(&items, CompletionKind::Directive);
        assert!(names.contains(&"schema".to_string()), "{names:?}");
        assert!(names.contains(&"main".to_string()), "{names:?}");
        let pragmas = labels_with_kind(&items, CompletionKind::Pragma);
        assert!(pragmas.contains(&"private".to_string()), "{pragmas:?}");
        // Should NOT include unrelated stdlib names in `#` context.
        let stdlib = labels_with_kind(&items, CompletionKind::Stdlib);
        assert!(
            stdlib.is_empty(),
            "stdlib should not appear after `#`: {stdlib:?}"
        );
    }

    #[test]
    fn reference_context_suggests_ref_vars() {
        // `&root` reference; cursor right after `&` (offset 8 on line 1).
        let src = "{\n    x: &root\n}\n";
        let items = complete_at(src, 1, 9);
        let refs = labels_with_kind(&items, CompletionKind::Reference);
        assert!(refs.contains(&"root".to_string()), "{refs:?}");
        assert!(refs.contains(&"sibling".to_string()), "{refs:?}");
        // No iteration refs outside a list.
        assert!(!refs.contains(&"prev".to_string()), "{refs:?}");
    }

    #[test]
    fn reference_context_inside_list_includes_iteration_refs() {
        // `&this` inside a list literal; cursor right after the `&`.
        // Source layout:
        //   line 0:  `{`
        //   line 1:  `xs: [&this]`   (no leading indent)
        //   line 2:  `}`
        // Cursor at (1, 6) — byte position right after the `&`.
        let src = "{\nxs: [&this]\n}\n";
        let items = complete_at(src, 1, 6);
        let refs = labels_with_kind(&items, CompletionKind::Reference);
        assert!(refs.contains(&"prev".to_string()), "{refs:?}");
        assert!(refs.contains(&"index".to_string()), "{refs:?}");
    }

    #[test]
    fn bare_context_includes_stdlib() {
        // Cursor on the `1` value.
        let src = "{\n    foo: 1\n}\n";
        let items = complete_at(src, 1, 10);
        let names = labels_with_kind(&items, CompletionKind::Stdlib);
        assert!(names.contains(&"len".to_string()), "{names:?}");
    }

    #[test]
    fn bare_context_includes_schema_names() {
        let src = "#schema User { String name: * }\n\n{\n    x: 1\n}\n";
        // Cursor on `1` in the file body.
        let items = complete_at(src, 3, 7);
        let schemas = labels_with_kind(&items, CompletionKind::Schema);
        assert!(schemas.contains(&"User".to_string()), "{schemas:?}");
    }

    #[test]
    fn bare_context_does_not_offer_directive_names() {
        let src = "{\n    foo: 1\n}\n";
        let items = complete_at(src, 1, 10);
        let dirs = labels_with_kind(&items, CompletionKind::Directive);
        assert!(
            dirs.is_empty(),
            "directives leaked into bare context: {dirs:?}"
        );
    }

    #[test]
    fn import_alias_seeds_module_label() {
        let src = "#import lib from \"./lib.relon\"\n\n{\n    x: 1\n}\n";
        let items = complete_at(src, 3, 7);
        let modules = labels_with_kind(&items, CompletionKind::Module);
        assert!(modules.contains(&"lib".to_string()), "{modules:?}");
    }

    #[test]
    fn destructure_import_seeds_binding_labels() {
        let src = "#import { foo, bar as baz } from \"./lib.relon\"\n\n{\n    x: 1\n}\n";
        let items = complete_at(src, 3, 7);
        let imports = labels_with_kind(&items, CompletionKind::Import);
        assert!(imports.contains(&"foo".to_string()), "{imports:?}");
        assert!(imports.contains(&"baz".to_string()), "{imports:?}");
        // Original `bar` (without alias) shouldn't show — only the
        // visible local binding.
        assert!(!imports.contains(&"bar".to_string()), "{imports:?}");
    }

    #[test]
    fn keywords_for_cursor_directive_works_without_parse() {
        // A bare `#` on its own line doesn't parse — this is the
        // mid-edit state right after the user types `#`. The
        // parse-free fallback still emits directive names.
        let src = "// header\n\n#\n\n{ x: 1 }\n";
        // Cursor right after the `#` on line 2.
        let items = keywords_for_cursor(src, 2, 1);
        let names: Vec<String> = items
            .iter()
            .filter(|i| i.kind == CompletionKind::Directive)
            .map(|i| i.label.clone())
            .collect();
        assert!(names.contains(&"schema".to_string()), "{names:?}");
        assert!(names.contains(&"main".to_string()), "{names:?}");
        assert!(names.contains(&"import".to_string()), "{names:?}");
        let pragmas: Vec<String> = items
            .iter()
            .filter(|i| i.kind == CompletionKind::Pragma)
            .map(|i| i.label.clone())
            .collect();
        assert!(pragmas.contains(&"private".to_string()), "{pragmas:?}");
    }

    #[test]
    fn keywords_for_cursor_reference_works_without_parse() {
        // Mid-edit: bare `&` with no AST yet.
        let src = "&";
        let items = keywords_for_cursor(src, 0, 1);
        let refs: Vec<String> = items
            .iter()
            .filter(|i| i.kind == CompletionKind::Reference)
            .map(|i| i.label.clone())
            .collect();
        assert!(refs.contains(&"root".to_string()), "{refs:?}");
        assert!(refs.contains(&"sibling".to_string()), "{refs:?}");
        // Without AST we can't know if cursor is inside a list →
        // iteration-only refs are suppressed.
        assert!(!refs.contains(&"prev".to_string()), "{refs:?}");
    }

    #[test]
    fn closure_body_sees_sibling_methods() {
        // Inside `multiply`'s body (`a * b`), both `currency` (sibling
        // method) and `multiply`'s own params should be in scope.
        let src = "{\n    currency(s, v): s + v,\n    multiply(a, b): a * b\n}\n";
        let items = complete_at(src, 2, 21);
        let names = labels(&items);
        assert!(names.contains(&"currency".to_string()), "{names:?}");
        assert!(names.contains(&"a".to_string()), "{names:?}");
        assert!(names.contains(&"b".to_string()), "{names:?}");
    }

    fn complete_recovering(source: &str, line: u32, character: u32) -> Vec<CompletionItem> {
        let parsed = relon_parser::parse_document_recovering(source);
        resolve_recovering(source, &parsed, line, character)
    }

    #[test]
    fn recovering_at_decorator_surfaces_sibling_closures() {
        // The original user complaint: typing `@` inside a dict with
        // sibling closures should surface those closures as decorator
        // candidates, not return empty.
        let src = "{\n    fmt(v): v + 1,\n    @\n    name: \"x\"\n}\n";
        // Cursor right after the `@` on line 2 (UTF-16 character index).
        let items = complete_recovering(src, 2, 5);
        let decorators = labels_with_kind(&items, CompletionKind::Decorator);
        assert!(
            decorators.contains(&"fmt".to_string()),
            "expected `fmt` sibling closure as decorator candidate, got {decorators:?}"
        );
    }

    #[test]
    fn recovering_at_hash_surfaces_directive_names() {
        // Standalone `#` mid-edit — should always offer the full
        // directive set even without a partial AST root.
        let src = "#";
        let items = complete_recovering(src, 0, 1);
        let dirs = labels_with_kind(&items, CompletionKind::Directive);
        assert!(dirs.contains(&"schema".to_string()), "{dirs:?}");
        assert!(dirs.contains(&"import".to_string()), "{dirs:?}");
    }

    #[test]
    fn recovering_at_amp_surfaces_reference_bases() {
        let src = "&";
        let items = complete_recovering(src, 0, 1);
        let refs = labels_with_kind(&items, CompletionKind::Reference);
        assert!(refs.contains(&"root".to_string()), "{refs:?}");
        assert!(refs.contains(&"sibling".to_string()), "{refs:?}");
    }

    #[test]
    fn recovering_member_dot_surfaces_dict_keys() {
        // User types `parent.│` where `parent` is a sibling dict.
        // The partial-AST member walker should surface `parent`'s
        // keys (one closure → Method, one literal → Field).
        let src = "{\n    parent: {\n        greet(): \"hi\",\n        nickname: \"jojo\"\n    },\n    child: parent.\n}\n";
        // Cursor immediately after the `parent.` on line 5 (0-indexed),
        // character 18 = end of `    child: parent.`.
        let items = complete_recovering(src, 5, 18);
        let names = labels(&items);
        assert!(
            names.contains(&"greet".to_string()),
            "expected `greet` method via member access, got {names:?}"
        );
        assert!(
            names.contains(&"nickname".to_string()),
            "expected `nickname` field via member access, got {names:?}"
        );
    }

    #[test]
    fn recovering_fstring_interp_sees_scope() {
        // Inside `${...}`, scope candidates (siblings, closure params)
        // should be available — same as bare context.
        let src = "{\n    name: \"world\",\n    greeting: f\"hi ${\n}\n";
        // Cursor right after the `${` on line 2 (UTF-16 index 19).
        let items = complete_recovering(src, 2, 19);
        let names = labels(&items);
        assert!(
            names.contains(&"name".to_string()),
            "expected sibling `name` inside f-string interp, got {names:?}"
        );
    }

    #[test]
    fn recovering_generic_args_surfaces_primitives_and_schemas() {
        // Cursor inside `Foo<│>` — should surface primitives + any
        // visible schema names.
        let src = "#schema Box<T> T\n\n{\n    items: Foo<\n}\n";
        // Cursor right after the `<` on line 3, character 16.
        let items = complete_recovering(src, 3, 16);
        let names = labels(&items);
        assert!(
            names.contains(&"String".to_string()),
            "expected primitive `String` in generic args, got {names:?}"
        );
        assert!(
            names.contains(&"Int".to_string()),
            "{names:?}"
        );
    }

    #[test]
    fn recovering_closure_return_after_arrow_surfaces_types() {
        // `(x) -> │` — closure return type position.
        let src = "{\n    f: (x) -> \n}\n";
        let items = complete_recovering(src, 1, 16);
        let names = labels(&items);
        assert!(
            names.contains(&"String".to_string()),
            "expected `String` after `->`, got {names:?}"
        );
    }

    #[test]
    fn recovering_typed_spread_after_star_surfaces_types() {
        // `{ *│ }` — typed-spread head position.
        let src = "{\n    *\n}\n";
        let items = complete_recovering(src, 1, 5);
        let names = labels(&items);
        assert!(
            names.contains(&"Int".to_string()),
            "expected `Int` after `*`, got {names:?}"
        );
    }

    #[test]
    fn recovering_bare_inside_dict_surfaces_siblings() {
        // Mid-edit dict with a partially typed identifier as a value.
        let src = "{\n    foo: 1,\n    bar: 2,\n    baz: f\n}\n";
        // Cursor right after the `f` on line 3.
        let items = complete_recovering(src, 3, 10);
        let names = labels(&items);
        assert!(
            names.contains(&"foo".to_string()),
            "expected sibling `foo` in bare scope, got {names:?}"
        );
        assert!(names.contains(&"bar".to_string()), "{names:?}");
    }
}
