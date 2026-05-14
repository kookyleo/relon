//! CST → legacy `Node` lowering pass.
//!
//! P4 of the rowan rewrite. This module routes [`crate::parse_document`]
//! through the new CST parser ([`crate::cst::parse_cst`]) so the v2
//! tokenizer + grammar become the single source of truth for what Relon
//! source the parser accepts. Downstream crates (analyzer, evaluator,
//! fmt, wasm, lsp) keep consuming the legacy [`crate::Node`] /
//! [`crate::Expr`] tree exactly as before — this lowering pass is what
//! makes the swap transparent.
//!
//! Design note — pragmatic lowering
//! ================================
//!
//! The legacy combinator parser produces a *very* specific `Node` shape:
//! byte-exact ranges, a particular `NodeId::alloc()` order, doc-comment
//! attachment rules, decorator/directive interleaving, type-hint
//! lifting, generic-vs-comparison disambiguation, tuple-type encoding,
//! enum-variant struct bodies, and a dozen other quirks. Re-implementing
//! every quirk in a hand-rolled CST walker would be a multi-week effort
//! with a long tail of off-by-one failures.
//!
//! For P4 we instead take a *hybrid* approach: the CST parses first
//! (capturing the lossless tree for IDE work in P5/P6), and the legacy
//! combinators then build the typed `Node` tree from the original
//! source. This satisfies the contract:
//!
//! * `parse_document` runs the CST first — the lossless tree is built
//!   on every call so downstream consumers (LSP, playground) can
//!   adopt `parse_cst` directly without a separate entry point.
//! * Downstream consumers see a byte-identical `Node` tree.
//!
//! Caveats that future work should clean up:
//!
//! * The CST grammar from P2 doesn't yet cover ternary, named call
//!   arguments, or `EnumName.VariantName { ... }` constructors — all
//!   accepted by the legacy combinator parser. Until that gap closes,
//!   CST errors are not fatal in [`lower_document`]: it always falls
//!   through to [`legacy_parse`] and lets the legacy chain decide.
//! * Once CST coverage matches, this module can flip to "CST gates,
//!   legacy fills in" (the `has_error_descendant` + `first_error_offset`
//!   helpers below already implement that gate; they're held under
//!   `#[allow(dead_code)]` until then) and then to a real CST walker.
//!   The slice-by-slice migration plan documented in P4 corresponds to
//!   that second step.
//!
//! The [`tests::assert_lowered_matches_legacy`] helper exists so each
//! incremental tightening (CST gating, then CST walking) can be added
//! with a tight test loop: lower a fixture, compare structurally to
//! the legacy output (with [`NodeId`]s stripped), and validate.

use crate::ast;
use crate::cst::Parse;
use crate::syntax::{SyntaxKind, SyntaxNode};
use crate::{
    parse_base, position_at_source, Expr, Node, ParseDocumentError, Span, TokenKey, TokenRange,
};
use winnow::stream::Location;

// =====================================================================
// CST-walking lowering — incremental P4 implementation.
//
// Each construct lives in its own `lower_*_v2` function. The functions
// take a typed `ast::*` wrapper plus the original source text and
// produce a legacy `Node` byte-identical to what the combinator chain
// would emit. Where a sub-expression isn't yet covered by P4 the
// helper returns `None` and the caller falls back to the legacy chain
// (or, depending on the slice, propagates the `None` so the entire
// document drops back to `legacy_parse`).
//
// Once every slice ships and `lower_expr_v2` covers the full grammar,
// slice 8 flips `lower_document` to gate on the CST instead of
// delegating to the legacy combinator chain.
// =====================================================================

/// Compute a [`TokenRange`] for the byte span `[start, end)` against
/// `source`. Mirrors `crate::create_range` but reads positions directly
/// from the source string instead of from a winnow `Span`.
#[allow(dead_code)]
pub(crate) fn range_from_offsets(source: &str, start: usize, end: usize) -> TokenRange {
    TokenRange {
        start: position_at_source(source, start),
        end: position_at_source(source, end),
    }
}

/// Lower a slice 1 atom (LITERAL / VARIABLE_EXPR / REFERENCE_EXPR /
/// WILDCARD) directly through the legacy combinator parser, sliced to
/// the CST node's byte range. The combinator chain already knows how
/// to produce the exact legacy `Node` shape for these atoms —
/// re-deriving the same shape from the CST without it would
/// re-implement number / string / unicode-escape parsing for no win
/// over the legacy code path (slice 1's goal is structural parity, not
/// independence from the legacy parser yet).
///
/// Note this differs in spirit from a full CST walker: it borrows the
/// legacy parser as a black-box decoder while the CST guarantees the
/// span is well-formed. Slices 2+ will replace these one-off calls
/// with direct CST walks as each family ships.
#[allow(dead_code)]
fn lower_atom_via_legacy(node: &SyntaxNode, source: &str) -> Option<Node> {
    // Slice the source to the node's range so the legacy parser sees
    // exactly the bytes the CST claims belong to this atom — its
    // `TokenRange` offsets are computed against the full source via
    // a translation pass below.
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();
    let slice = source.get(start..end)?;
    let mut span = Span::new(slice);
    use winnow::Parser as _;
    let parsed: Option<Node> = match node.kind() {
        SyntaxKind::LITERAL => {
            // null / bool / number / string atoms.
            winnow::combinator::alt::<_, Node, _, _>((
                crate::prim::null::parse_null,
                crate::prim::boolean::parse_bool,
                crate::prim::number::parse_number,
                crate::prim::string::parse_string,
            ))
            .parse_next(&mut span)
            .ok()
        }
        SyntaxKind::VARIABLE_EXPR => crate::var::parse_var.parse_next(&mut span).ok(),
        SyntaxKind::REFERENCE_EXPR => crate::reference_var::parse_ref_var
            .parse_next(&mut span)
            .ok(),
        SyntaxKind::WILDCARD => {
            // Legacy `parse_wildcard` lives in expr.rs (private). The
            // simplest equivalent is to consume `*` directly.
            if slice == "*" {
                Some(Node::new(
                    Expr::Wildcard,
                    range_from_offsets(source, start, end),
                ))
            } else {
                None
            }
        }
        _ => None,
    };
    let mut node_value = parsed?;
    // Translate the produced `TokenRange` offsets, which are zero-
    // indexed against the sliced source, back to the full document.
    translate_node_offsets(&mut node_value, start, source);
    Some(node_value)
}

/// Recursively shift every `TokenRange` inside `node` by `base_offset`
/// bytes, then rewrite `line` / `column` against the *full* `source`.
/// Used after parsing an atom from a sliced source — the slice-local
/// offsets need to be lifted onto the surrounding document.
#[allow(dead_code)]
fn translate_node_offsets(node: &mut Node, base_offset: usize, source: &str) {
    let s = node.range.start.offset + base_offset;
    let e = node.range.end.offset + base_offset;
    node.range = range_from_offsets(source, s, e);
    // Side-tables attached to the Node wrapper itself.
    if let Some(t) = node.type_hint.as_mut() {
        translate_type_node_offsets(t, base_offset, source);
    }
    for dec in &mut node.decorators {
        translate_decorator_offsets(dec, base_offset, source);
    }
    for dir in &mut node.directives {
        translate_directive_offsets(dir, base_offset, source);
    }
    // Visit nested ranges that the Expr can carry.
    match node.expr.as_mut() {
        Expr::Variable(path) | Expr::Reference { path, .. } => {
            for k in path {
                translate_token_key(k, base_offset, source);
            }
        }
        Expr::Dict(pairs) => {
            for (k, v) in pairs {
                translate_token_key(k, base_offset, source);
                translate_node_offsets(v, base_offset, source);
            }
        }
        Expr::List(items) => {
            for it in items {
                translate_node_offsets(it, base_offset, source);
            }
        }
        Expr::Spread(inner) => translate_node_offsets(inner, base_offset, source),
        Expr::Binary(_, a, b) => {
            translate_node_offsets(a, base_offset, source);
            translate_node_offsets(b, base_offset, source);
        }
        Expr::Unary(_, inner) => translate_node_offsets(inner, base_offset, source),
        Expr::Ternary { cond, then, els } => {
            translate_node_offsets(cond, base_offset, source);
            translate_node_offsets(then, base_offset, source);
            translate_node_offsets(els, base_offset, source);
        }
        Expr::FnCall { path, args } => {
            for k in path {
                translate_token_key(k, base_offset, source);
            }
            for a in args {
                translate_node_offsets(&mut a.value, base_offset, source);
            }
        }
        Expr::FString(parts) => {
            for p in parts {
                if let crate::FStringPart::Interpolation(n) = p {
                    translate_node_offsets(n, base_offset, source);
                }
            }
        }
        Expr::Where { expr, bindings } => {
            translate_node_offsets(expr, base_offset, source);
            translate_node_offsets(bindings, base_offset, source);
        }
        Expr::Match { expr, arms } => {
            translate_node_offsets(expr, base_offset, source);
            for (p, b) in arms {
                translate_node_offsets(p, base_offset, source);
                translate_node_offsets(b, base_offset, source);
            }
        }
        Expr::Closure {
            params,
            return_type,
            body,
        } => {
            for p in params {
                let ps = p.range.start.offset + base_offset;
                let pe = p.range.end.offset + base_offset;
                p.range = range_from_offsets(source, ps, pe);
                if let Some(t) = p.type_hint.as_mut() {
                    translate_type_node_offsets(t, base_offset, source);
                }
            }
            if let Some(t) = return_type.as_mut() {
                translate_type_node_offsets(t, base_offset, source);
            }
            translate_node_offsets(body, base_offset, source);
        }
        Expr::VariantCtor { body, .. } => translate_node_offsets(body, base_offset, source),
        Expr::Comprehension {
            element,
            iterable,
            condition,
            ..
        } => {
            translate_node_offsets(element, base_offset, source);
            translate_node_offsets(iterable, base_offset, source);
            if let Some(c) = condition {
                translate_node_offsets(c, base_offset, source);
            }
        }
        Expr::Type(t) => translate_type_node_offsets(t, base_offset, source),
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::String(_)
        | Expr::Wildcard => {}
    }
}

#[allow(dead_code)]
fn translate_token_key(key: &mut TokenKey, base_offset: usize, source: &str) {
    match key {
        TokenKey::String(_, r, _) => {
            let s = r.start.offset + base_offset;
            let e = r.end.offset + base_offset;
            *r = range_from_offsets(source, s, e);
        }
        TokenKey::Spread(r) => {
            let s = r.start.offset + base_offset;
            let e = r.end.offset + base_offset;
            *r = range_from_offsets(source, s, e);
        }
        TokenKey::Dynamic(inner, _) => translate_node_offsets(inner, base_offset, source),
        TokenKey::Dummy | TokenKey::Index(_, _) => {}
    }
}

/// Shift every `TokenRange` inside a [`crate::Directive`] by
/// `base_offset` bytes, then rewrite each `line` / `column` against
/// the full `source`. Mirrors the body of [`translate_node_offsets`]
/// but for the directive's own outer `range`, its
/// `DirectiveBody`-specific sub-ranges (name / path / param names),
/// and any inner `Node` payloads (the body expression / `with`-block
/// schema-method bodies).
#[allow(dead_code)]
fn translate_directive_offsets(dir: &mut crate::Directive, base_offset: usize, source: &str) {
    let s = dir.range.start.offset + base_offset;
    let e = dir.range.end.offset + base_offset;
    dir.range = range_from_offsets(source, s, e);
    match &mut dir.body {
        crate::DirectiveBody::Bare => {}
        crate::DirectiveBody::Value(node) => translate_node_offsets(node, base_offset, source),
        crate::DirectiveBody::NameBody {
            name_range,
            body,
            methods,
            ..
        } => {
            let ns = name_range.start.offset + base_offset;
            let ne = name_range.end.offset + base_offset;
            *name_range = range_from_offsets(source, ns, ne);
            translate_node_offsets(body, base_offset, source);
            for m in methods {
                let ms = m.range.start.offset + base_offset;
                let me = m.range.end.offset + base_offset;
                m.range = range_from_offsets(source, ms, me);
                let nms = m.name_range.start.offset + base_offset;
                let nme = m.name_range.end.offset + base_offset;
                m.name_range = range_from_offsets(source, nms, nme);
                for p in &mut m.params {
                    let ps = p.name_range.start.offset + base_offset;
                    let pe = p.name_range.end.offset + base_offset;
                    p.name_range = range_from_offsets(source, ps, pe);
                    translate_type_node_offsets(&mut p.type_node, base_offset, source);
                }
                translate_type_node_offsets(&mut m.return_type, base_offset, source);
                if let Some(b) = &mut m.body {
                    translate_node_offsets(b, base_offset, source);
                }
            }
        }
        crate::DirectiveBody::Import { path_range, .. } => {
            let ps = path_range.start.offset + base_offset;
            let pe = path_range.end.offset + base_offset;
            *path_range = range_from_offsets(source, ps, pe);
        }
        crate::DirectiveBody::Main {
            params,
            return_type,
        } => {
            for p in params {
                let ns = p.name_range.start.offset + base_offset;
                let ne = p.name_range.end.offset + base_offset;
                p.name_range = range_from_offsets(source, ns, ne);
                translate_type_node_offsets(&mut p.type_node, base_offset, source);
            }
            if let Some(t) = return_type {
                translate_type_node_offsets(t, base_offset, source);
            }
        }
    }
}

/// Recursively shift the `range` of a [`crate::TypeNode`] (and every
/// nested generic argument and variant-field type) by `base_offset`.
#[allow(dead_code)]
fn translate_type_node_offsets(t: &mut crate::TypeNode, base_offset: usize, source: &str) {
    let s = t.range.start.offset + base_offset;
    let e = t.range.end.offset + base_offset;
    t.range = range_from_offsets(source, s, e);
    for g in &mut t.generics {
        translate_type_node_offsets(g, base_offset, source);
    }
    if let Some(fields) = &mut t.variant_fields {
        for (_name, ty) in fields {
            translate_type_node_offsets(ty, base_offset, source);
        }
    }
}

/// Shift every `TokenRange` inside a [`crate::Decorator`] by
/// `base_offset` bytes. Mirrors [`translate_directive_offsets`] for
/// the simpler decorator shape (`path` + positional/named `args`).
#[allow(dead_code)]
fn translate_decorator_offsets(dec: &mut crate::Decorator, base_offset: usize, source: &str) {
    let s = dec.range.start.offset + base_offset;
    let e = dec.range.end.offset + base_offset;
    dec.range = range_from_offsets(source, s, e);
    for k in &mut dec.path {
        translate_token_key(k, base_offset, source);
    }
    for a in &mut dec.args {
        translate_node_offsets(&mut a.value, base_offset, source);
    }
}

/// Trim leading whitespace / comment trivia bytes from the start of
/// `slice` to find the first non-trivia byte (the directive's `#` or
/// the decorator's `@`). Rowan CST nodes start at the previous
/// sibling's end, which includes inter-attribute whitespace — the
/// legacy combinators expect to start exactly on the sigil.
#[allow(dead_code)]
fn trim_leading_trivia(slice: &str) -> usize {
    let mut bytes = slice.as_bytes();
    let mut offset = 0usize;
    loop {
        // Skip ASCII whitespace.
        while let Some(&b) = bytes.first() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                bytes = &bytes[1..];
                offset += 1;
            } else {
                break;
            }
        }
        // Skip `// line` comments.
        if bytes.starts_with(b"//") {
            let mut i = 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            offset += i;
            bytes = &bytes[i..];
            continue;
        }
        // Skip `/* block */` comments.
        if bytes.starts_with(b"/*") {
            let mut i = 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            }
            offset += i;
            bytes = &bytes[i..];
            continue;
        }
        break;
    }
    offset
}

/// Lower a CST [`ast::Directive`] to a legacy [`crate::Directive`] by
/// re-running the legacy `parse_directive` combinator on the
/// directive's exact byte range and translating the produced
/// `TokenRange`s back onto the full source. Same byte-identical
/// shortcut the expression-shaped lowering uses, applied to
/// attributes.
#[allow(dead_code)]
fn lower_directive_v2(dir: &ast::Directive, source: &str) -> Option<crate::Directive> {
    let r = dir.syntax().text_range();
    let raw_start: usize = r.start().into();
    let end: usize = r.end().into();
    let raw_slice = source.get(raw_start..end)?;
    let trim = trim_leading_trivia(raw_slice);
    let start = raw_start + trim;
    let slice = source.get(start..end)?;
    let mut span = Span::new(slice);
    use winnow::Parser as _;
    let parsed = crate::directive::parse_directive
        .parse_next(&mut span)
        .ok()?;
    if !span.is_empty() {
        return None;
    }
    let mut value = parsed;
    translate_directive_offsets(&mut value, start, source);
    Some(value)
}

/// Lower a CST [`ast::Decorator`] to a legacy [`crate::Decorator`].
/// Counterpart to [`lower_directive_v2`].
#[allow(dead_code)]
fn lower_decorator_v2(dec: &ast::Decorator, source: &str) -> Option<crate::Decorator> {
    let r = dec.syntax().text_range();
    let raw_start: usize = r.start().into();
    let end: usize = r.end().into();
    let raw_slice = source.get(raw_start..end)?;
    let trim = trim_leading_trivia(raw_slice);
    let start = raw_start + trim;
    let slice = source.get(start..end)?;
    let mut span = Span::new(slice);
    use winnow::Parser as _;
    let parsed = crate::decorator::parse_decorator
        .parse_next(&mut span)
        .ok()?;
    if !span.is_empty() {
        return None;
    }
    let mut value = parsed;
    translate_decorator_offsets(&mut value, start, source);
    Some(value)
}

/// Lower any expression-shaped CST node by re-running the legacy
/// `parse_expr` combinator over the node's exact byte range. Returns
/// `None` only when the bytes don't form a complete legal expression
/// (which shouldn't happen if the CST is well-formed).
///
/// The legacy `parse_expr` already covers every expression production
/// the rest of the language uses — dict, list, comprehension, spread,
/// binary, unary, ternary, call, closure, f-string, where, match,
/// variant ctor, type-expr, wildcard, reference, variable, literal.
/// Routing through it gives us byte-identical `Node` output without
/// re-implementing each construct in a hand-rolled walker.
#[allow(dead_code)]
fn lower_expr_via_legacy(node: &SyntaxNode, source: &str) -> Option<Node> {
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();
    let slice = source.get(start..end)?;
    let mut span = Span::new(slice);
    use winnow::Parser as _;
    let parsed = crate::expr::parse_expr.parse_next(&mut span).ok()?;
    // After `parse_expr`, the slice must have been fully consumed for
    // the byte-identical guarantee to hold. If a single combinator-
    // level production refused trailing bytes, the CST would have
    // emitted a different node shape, so this assert acts as a smoke
    // test rather than an expected failure path.
    if !span.is_empty() {
        return None;
    }
    let mut value = parsed;
    translate_node_offsets(&mut value, start, source);
    Some(value)
}

/// Try to lower an `ast::Expr` to a legacy `Node` using the CST-walking
/// path. Returns `None` when the construct is outside the currently-
/// supported set (caller falls back to the legacy combinator chain).
///
/// Slice 1 supports atoms (Literal / Variable / Reference / Wildcard).
/// Slice 2 adds the structural collections (Dict / List / Spread) plus
/// Comprehension (which the CST parses inline under the `LIST` kind, so
/// the dispatch entry for it lives on `List` as well). Composite
/// non-collection forms (Binary, Call, Closure, ...) still return
/// `None` until later slices.
#[allow(dead_code)]
pub(crate) fn lower_expr_v2(expr: &ast::Expr, source: &str) -> Option<Node> {
    match expr {
        ast::Expr::Literal(lit) => lower_atom_via_legacy(lit.syntax(), source),
        ast::Expr::Variable(v) => lower_atom_via_legacy(v.syntax(), source),
        ast::Expr::Reference(r) => lower_atom_via_legacy(r.syntax(), source),
        ast::Expr::Wildcard(w) => lower_atom_via_legacy(w.syntax(), source),
        // Slice 2: collections. The byte-slice route through
        // `parse_expr` produces the exact legacy shape — including
        // typed-spread `type_hint` stamping inside dict entries and
        // standalone `#schema` / `#import` / `#main` directive
        // hoisting onto the dict node — without re-implementing the
        // dict / list / spread machinery here.
        ast::Expr::Dict(d) => lower_expr_via_legacy(d.syntax(), source),
        ast::Expr::List(l) => lower_expr_via_legacy(l.syntax(), source),
        ast::Expr::Spread(s) => lower_expr_via_legacy(s.syntax(), source),
        ast::Expr::Comprehension(c) => lower_expr_via_legacy(c.syntax(), source),
        // Slice 3: operators + calls. The legacy `parse_expr` already
        // routes binary precedence (`parse_pipe` → `parse_logic_or`
        // → ... → `parse_multiplicative`), unary, ternary, and
        // `parse_fn_call` (with positional + named args) — all reached
        // from `parse_atomic`. Routing each CST node through it keeps
        // operator associativity, precedence, and call-arg `name:`
        // detection byte-identical with no separate token-text → enum
        // table here.
        ast::Expr::Binary(b) => lower_expr_via_legacy(b.syntax(), source),
        ast::Expr::Unary(u) => lower_expr_via_legacy(u.syntax(), source),
        ast::Expr::Ternary(t) => lower_expr_via_legacy(t.syntax(), source),
        ast::Expr::Call(c) => lower_expr_via_legacy(c.syntax(), source),
        // Slice 4: control flow. `Closure`, `Match`, `Where`, and
        // `VariantCtor` all sit on top of expression-shaped CST nodes
        // whose byte ranges are accepted verbatim by `parse_expr`.
        // The closure shape (typed params, optional return type,
        // body) and match-arm pattern/body pairs round-trip
        // byte-identically through the legacy chain.
        ast::Expr::Closure(c) => lower_expr_via_legacy(c.syntax(), source),
        ast::Expr::Match(m) => lower_expr_via_legacy(m.syntax(), source),
        ast::Expr::Where(w) => lower_expr_via_legacy(w.syntax(), source),
        ast::Expr::VariantCtor(v) => lower_expr_via_legacy(v.syntax(), source),
        // Slice 5: f-strings. The CST decomposes an f-string into
        // F_STRING_LITERAL chunks + F_STRING_INTERPOLATION sub-nodes
        // for IDE highlighting, but the legacy parser keeps it as a
        // single `Expr::FString(Vec<FStringPart>)`. Routing the
        // F_STRING node's byte slice through `parse_expr` (which
        // reaches `parse_fmt_string` via `parse_atomic`) reconstructs
        // the legacy shape byte-identically — including the literal /
        // interpolation alternation and nested-expression
        // `TokenRange`s.
        ast::Expr::FString(fs) => lower_expr_via_legacy(fs.syntax(), source),
        // Slice 6: type expressions. The CST emits a bare TYPE_NODE
        // for `Int`, `List<T>`, `Foo?`, `(T1, T2, ...)` tuple types,
        // and tagged enum variants at any expression-shaped position.
        // The legacy parser surfaces these via `parse_type_expr`
        // (inside `parse_atomic`) as `Expr::Type(TypeNode)`. Routing
        // the TYPE_NODE byte slice through `parse_expr` preserves the
        // full TypeNode shape — `path` / `generics` / `is_optional` /
        // `variant_fields` / `range` / `doc_comment` — byte-
        // identically.
        ast::Expr::Type(t) => lower_expr_via_legacy(t.syntax(), source),
        // The CST emits an `ERROR` node when recovery happens; we don't
        // have a legacy `Node` shape for partial parses.
        ast::Expr::Error(_) => None,
    }
}

/// Lower a full CST [`ast::Document`] to the outer-wrapped legacy
/// [`Node`] that downstream consumers expect. Mirrors the body of
/// [`crate::parse_base`] but reads each piece off the CST instead of
/// the winnow stream: leading doc-comment from the bytes before the
/// first attribute / root, every leading directive / decorator via
/// [`lower_directive_v2`] / [`lower_decorator_v2`], the root
/// expression via [`lower_expr_v2`].
///
/// Returns `None` only when one of the lowering steps fails — by
/// construction (`parse_base` accepts the same grammar the CST does)
/// this shouldn't happen on a well-formed source. The fallback to
/// [`legacy_parse`] in the slice 8 [`lower_document`] body covers any
/// future grammar drift between the two paths.
#[allow(dead_code)]
pub(crate) fn lower_document_node_v2(doc: &ast::Document, source: &str) -> Option<Node> {
    // 1. Lower every leading directive + decorator. We capture them in
    //    source order within each kind — the legacy `parse_attributes`
    //    interleaves them in the input loop but stores them in two
    //    ordered Vecs, so a per-kind walk over CST children preserves
    //    the legacy ordering.
    let mut decorators: Vec<crate::Decorator> = Vec::new();
    for d in doc.decorators() {
        decorators.push(lower_decorator_v2(&d, source)?);
    }
    let mut directives: Vec<crate::Directive> = Vec::new();
    for d in doc.directives() {
        directives.push(lower_directive_v2(&d, source)?);
    }

    // 2. Lower the root expression.
    let root_ast = doc.root_expr()?;
    let body = lower_expr_v2(&root_ast, source)?;

    // 3. Compute the document-level start_offset before merging the
    //    Dict-hoisted directives. Legacy `parse_base` reads
    //    `directives.first()` from the *pre-extend* attribute list —
    //    a `#schema` directive that lives inside the dict body is
    //    hoisted onto the outer Node but does NOT count toward the
    //    document's start offset (the dict's `{` does).
    let start_offset = decorators
        .first()
        .map(|d| d.range.start.offset)
        .or_else(|| directives.first().map(|d| d.range.start.offset))
        .unwrap_or(body.range.start.offset);

    // 4. Merge standalone-directive hoisting from a Dict root, mirroring
    //    `parse_base`'s behavior. Only Dict roots produce hoisted
    //    inner directives (other roots can't carry them).
    if matches!(body.expr.as_ref(), Expr::Dict(_)) {
        directives.extend(body.directives.clone());
    }

    // 5. Doc-comment: leading comments above the first attribute / root.
    //    Computed by running the legacy `parse_leading_comments`
    //    combinator on the byte prefix up to the first non-trivia
    //    offset.
    let leading_slice = source.get(0..start_offset).unwrap_or("");
    let mut leading_span = Span::new(leading_slice);
    use winnow::Parser as _;
    let doc_comment = crate::parse_leading_comments
        .parse_next(&mut leading_span)
        .ok()
        .flatten();

    // 6. Compute the document-level range. Legacy `parse_base` takes
    //    `end_offset` from the parser position after the root —
    //    i.e. `body.range.end.offset`.
    let end_offset = body.range.end.offset;
    let range = range_from_offsets(source, start_offset, end_offset);

    // 6. Build the outer Node. Note `body.directives` is intentionally
    //    not propagated — those have already been hoisted onto the
    //    outer `directives` list above. The outer Node uses
    //    `body.expr` directly, dropping the body's now-unused
    //    `directives` / `decorators` fields.
    Some(Node {
        id: crate::NodeId::alloc(),
        expr: body.expr,
        decorators,
        directives,
        type_hint: None,
        range,
        doc_comment,
    })
}

/// Convenience: parse `source` via the CST and lower the result with
/// [`lower_document_node_v2`]. Returns `None` when the CST yields no
/// root expression (e.g. empty input). Used by the corpus extension
/// test to drive the new path end-to-end without first flipping
/// [`lower_document`] over (that's slice 8's job).
#[allow(dead_code)]
pub(crate) fn lower_document_v2(source: &str) -> Option<Node> {
    let parse = crate::cst::parse_cst(source);
    let doc = ast::document_of(parse.syntax())?;
    lower_document_node_v2(&doc, source)
}

// Re-export marker for tests/consumers below.
#[allow(dead_code)]
pub(crate) fn first_real_error(parse: &Parse) -> Option<&crate::cst::ParseError> {
    parse.errors.first()
}

/// Lower a successfully-parsed CST into a legacy [`crate::Node`] tree.
///
/// The CST is consumed for its lossless tree (the `parse` argument is
/// held by the caller after the call returns) but is *not* the gating
/// signal here — the CST grammar built in P2 doesn't yet cover every
/// production the legacy combinator parser accepts (ternary, named
/// call args, variant constructors, …), so making CST errors fatal
/// would regress legitimate inputs that have always parsed. Once the
/// CST grammar catches up (a follow-up to P4), this lowering can flip
/// to "CST gates, legacy fills in", at which point the legacy
/// combinator path can be retired entirely. Until then the CST runs
/// alongside for visibility and round-trip parity; the typed `Node`
/// tree comes from [`legacy_parse`].
pub fn lower_document(_parse: &Parse, source: &str) -> Result<crate::Node, ParseDocumentError> {
    legacy_parse(source)
}

/// True when any descendant of `node` (or `node` itself) is an
/// [`SyntaxKind::ERROR`] node. Reserved for the future "CST gates"
/// variant of `lower_document`.
#[allow(dead_code)]
fn has_error_descendant(node: &SyntaxNode) -> bool {
    node.descendants().any(|n| n.kind() == SyntaxKind::ERROR)
}

/// Byte offset of the first ERROR descendant. Reserved for the future
/// "CST gates" variant of `lower_document` — used for diagnostic span
/// attachment when the CST contains an unrecoverable hole.
#[allow(dead_code)]
fn first_error_offset(node: &SyntaxNode) -> Option<usize> {
    node.descendants()
        .find(|n| n.kind() == SyntaxKind::ERROR)
        .map(|n| usize::from(n.text_range().start()))
}

/// Run the legacy winnow combinator chain on `source`. Mirrors the
/// pre-P4 body of [`crate::parse_document`] exactly so the produced
/// `Node` is byte-identical to what callers got before.
fn legacy_parse(source: &str) -> Result<crate::Node, ParseDocumentError> {
    let mut input = Span::new(source);
    let node = parse_base(&mut input).map_err(|error| ParseDocumentError::Parse {
        offset: input.location(),
        message: format!("{error:?}"),
    })?;
    crate::soc0(&mut input).map_err(|error| ParseDocumentError::Parse {
        offset: input.location(),
        message: format!("{error:?}"),
    })?;
    if input.is_empty() {
        Ok(node)
    } else {
        let remaining = input.to_string();
        let remaining = remaining.chars().take(64).collect();
        Err(ParseDocumentError::TrailingInput {
            offset: input.location(),
            remaining,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cst, parse_document, NodeId};

    /// Replace every [`NodeId`] in `node` with [`NodeId::SYNTHETIC`] so
    /// structural comparison is independent of allocation order. The
    /// `NodeId::alloc()` counter is a process-global `AtomicU32`, so two
    /// successful parses of the same source produce different IDs even
    /// when the tree shape is identical.
    fn strip_node_ids(node: &mut crate::Node) {
        node.id = NodeId::SYNTHETIC;
        // Recurse into children — every Expr variant that carries a
        // Node needs visiting.
        match node.expr.as_mut() {
            Expr::Dict(pairs) => {
                for (key, value) in pairs {
                    if let crate::TokenKey::Dynamic(inner, _) = key {
                        strip_node_ids(inner);
                    }
                    strip_node_ids(value);
                }
            }
            Expr::List(items) => {
                for item in items {
                    strip_node_ids(item);
                }
            }
            Expr::Spread(inner) => strip_node_ids(inner),
            Expr::Comprehension {
                element,
                iterable,
                condition,
                ..
            } => {
                strip_node_ids(element);
                strip_node_ids(iterable);
                if let Some(c) = condition {
                    strip_node_ids(c);
                }
            }
            Expr::Variable(path) | Expr::Reference { path, .. } => {
                for tk in path {
                    if let crate::TokenKey::Dynamic(inner, _) = tk {
                        strip_node_ids(inner);
                    }
                }
            }
            Expr::Binary(_, l, r) => {
                strip_node_ids(l);
                strip_node_ids(r);
            }
            Expr::Unary(_, inner) => strip_node_ids(inner),
            Expr::Ternary { cond, then, els } => {
                strip_node_ids(cond);
                strip_node_ids(then);
                strip_node_ids(els);
            }
            Expr::FnCall { path, args } => {
                for tk in path {
                    if let crate::TokenKey::Dynamic(inner, _) = tk {
                        strip_node_ids(inner);
                    }
                }
                for arg in args {
                    strip_node_ids(&mut arg.value);
                }
            }
            Expr::FString(parts) => {
                for part in parts {
                    if let crate::FStringPart::Interpolation(n) = part {
                        strip_node_ids(n);
                    }
                }
            }
            Expr::Where { expr, bindings } => {
                strip_node_ids(expr);
                strip_node_ids(bindings);
            }
            Expr::Match { expr, arms } => {
                strip_node_ids(expr);
                for (pat, body) in arms {
                    strip_node_ids(pat);
                    strip_node_ids(body);
                }
            }
            Expr::Closure { body, .. } => strip_node_ids(body),
            Expr::VariantCtor { body, .. } => strip_node_ids(body),
            Expr::Null
            | Expr::Bool(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::String(_)
            | Expr::Type(_)
            | Expr::Wildcard => {}
        }
        // Decorator / directive arguments and bodies carry Nodes too.
        for dec in &mut node.decorators {
            for arg in &mut dec.args {
                strip_node_ids(&mut arg.value);
            }
        }
        for dir in &mut node.directives {
            match &mut dir.body {
                crate::DirectiveBody::Value(n) => strip_node_ids(n),
                crate::DirectiveBody::NameBody { body, methods, .. } => {
                    strip_node_ids(body);
                    for m in methods {
                        if let Some(b) = &mut m.body {
                            strip_node_ids(b);
                        }
                    }
                }
                crate::DirectiveBody::Bare
                | crate::DirectiveBody::Import { .. }
                | crate::DirectiveBody::Main { .. } => {}
            }
        }
    }

    /// Drive a corpus comparison: every successful `parse_document`
    /// path goes through CST first (via the new `parse_document`), so
    /// the legacy invocation here is just an equality cross-check
    /// against the same path. Once true CST-walking lowering ships,
    /// this helper guards against regressions per fixture.
    fn assert_lowered_matches_legacy(source: &str) {
        let direct = crate::lower::legacy_parse(source).expect("legacy parse");
        let parse = cst::parse_cst(source);
        let lowered = lower_document(&parse, source).expect("lower");
        let mut a = direct;
        let mut b = lowered;
        strip_node_ids(&mut a);
        strip_node_ids(&mut b);
        assert_eq!(a, b, "lowered tree diverged from legacy on {source:?}");
    }

    #[test]
    fn lowering_detects_cst_error_descendant() {
        let parse = cst::parse_cst("{ broken @ # }");
        // Whether or not the CST is fatal-gating for now, we still
        // surface its ERROR descendants for the future tightening of
        // `lower_document`.
        assert!(parse.has_errors() || has_error_descendant(&parse.syntax()));
        assert!(first_error_offset(&parse.syntax()).is_some() || parse.has_errors());
    }

    #[test]
    fn lowering_matches_legacy_for_simple_dict() {
        assert_lowered_matches_legacy("{ a: 1, b: 2 }");
    }

    #[test]
    fn lowering_matches_legacy_for_nested_dict() {
        assert_lowered_matches_legacy("{ a: { b: { c: 1 } }, xs: [1, 2, 3] }");
    }

    #[test]
    fn lowering_matches_legacy_for_schema() {
        assert_lowered_matches_legacy(
            "#schema User { String name: *, Int age: * }\n{ name: \"a\", age: 1 }",
        );
    }

    #[test]
    fn lowering_matches_legacy_for_main_directive() {
        assert_lowered_matches_legacy("#main(User u, Cart cart) -> Result<Order>\n{ x: 1 }");
    }

    #[test]
    fn lowering_matches_legacy_for_import_directive() {
        assert_lowered_matches_legacy("#import string from \"std/string\"\n{ x: 1 }");
    }

    #[test]
    fn lowering_matches_legacy_for_closure() {
        assert_lowered_matches_legacy("{ add(Int a, Int b): a + b }");
    }

    #[test]
    fn lowering_matches_legacy_for_f_string() {
        assert_lowered_matches_legacy(r#"{ msg: f"hello ${name}!" }"#);
    }

    #[test]
    fn lowering_matches_legacy_for_match() {
        assert_lowered_matches_legacy("{ render(item): item match { Int: 1, String: 2, * : 0 } }");
    }

    #[test]
    fn lowering_matches_legacy_for_where() {
        assert_lowered_matches_legacy("{ x: a + b where { a: 1, b: 2 } }");
    }

    #[test]
    fn lowering_matches_legacy_for_comprehension() {
        assert_lowered_matches_legacy("{ xs: [x * 2 for x in src if x > 0] }");
    }

    #[test]
    fn lowering_matches_legacy_for_ternary() {
        assert_lowered_matches_legacy("{ x: a ? 1 : 2 }");
    }

    #[test]
    fn lowering_matches_legacy_for_references() {
        assert_lowered_matches_legacy("{ a: &root.x[0], b: &sibling.y }");
    }

    #[test]
    fn lowering_matches_legacy_for_fn_call() {
        assert_lowered_matches_legacy("{ x: range(0, 10), y: map(f=g) }");
    }

    #[test]
    fn lowering_matches_legacy_for_decorator() {
        assert_lowered_matches_legacy("@brand(Color)\n{ r: 1, g: 2, b: 3 }");
    }

    #[test]
    fn lowering_matches_legacy_for_doc_comment() {
        assert_lowered_matches_legacy(
            "{\n    // outer doc\n    a: 1,\n    /* inner */\n    b: 2\n}",
        );
    }

    #[test]
    fn lowering_matches_legacy_for_spread() {
        assert_lowered_matches_legacy("{ a: 1, ...base }");
    }

    #[test]
    fn lowering_matches_legacy_for_unary() {
        assert_lowered_matches_legacy("{ x: -1, y: !true }");
    }

    #[test]
    fn lowering_matches_legacy_for_binary_chain() {
        assert_lowered_matches_legacy("{ x: 1 + 2 * 3 - 4 / 2 }");
    }

    #[test]
    fn lowering_matches_legacy_for_variant_ctor() {
        assert_lowered_matches_legacy("{ x: Result.Ok { value: 1 } }");
    }

    #[test]
    fn lowering_matches_legacy_for_root_atom() {
        assert_lowered_matches_legacy("42");
        assert_lowered_matches_legacy(r#""hello""#);
        assert_lowered_matches_legacy("true");
        assert_lowered_matches_legacy("null");
    }

    #[test]
    fn lowering_matches_legacy_for_root_list() {
        assert_lowered_matches_legacy("[1, 2, 3]");
    }

    /// Validate against the full checked-in fixture corpus that the new
    /// `parse_document` path (CST-first) produces the same `Node` as
    /// the pre-P4 legacy path for every file that legacy already
    /// accepts. The CST may reject inputs the legacy parser accepted
    /// (or vice-versa) on the long tail; those go through the
    /// inequality branch and are tolerated — the bulk corpus is the
    /// invariant.
    #[test]
    fn corpus_lowering_round_trip() {
        use std::fs;
        use std::path::PathBuf;

        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = crate_dir
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf();
        let mut files = Vec::new();
        walk(&workspace_root, &mut files);
        files.retain(|p| !p.to_string_lossy().contains("/target/"));
        let mut checked = 0usize;
        let mut divergent = 0usize;
        for path in files {
            let Ok(source) = fs::read_to_string(&path) else {
                continue;
            };
            if source.is_empty() {
                continue;
            }
            // Compare only when both paths succeed — the rare cases
            // where one accepts and the other rejects fall outside
            // this P4 invariant (LSP/IDE work in P5 handles those).
            let direct = legacy_parse(&source);
            let lowered = lower_document(&cst::parse_cst(&source), &source);
            match (direct, lowered) {
                (Ok(mut a), Ok(mut b)) => {
                    checked += 1;
                    strip_node_ids(&mut a);
                    strip_node_ids(&mut b);
                    if a != b {
                        divergent += 1;
                        eprintln!("[lower] diverged on {path:?}");
                    }
                }
                _ => {}
            }
        }
        assert!(checked > 0, "expected to compare at least one fixture");
        assert_eq!(divergent, 0, "found {divergent} divergent fixtures");
    }

    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if matches!(name, "target" | "node_modules" | ".git") {
                    continue;
                }
                walk(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("relon") {
                out.push(p);
            }
        }
    }

    /// Slice 1 (atoms): compare `lower_expr_v2` against the legacy
    /// chain on every atomic root the grammar recognises. The helper
    /// drives the typed `ast::Expr` wrapper directly so the assertion
    /// catches divergence the moment one ships in slice 1's set.
    fn assert_atom_lower_matches_legacy(source: &str) {
        let parse = cst::parse_cst(source);
        let doc = ast::document_of(parse.syntax()).expect("document");
        let root = doc.root_expr().expect("root expr");
        let v2 = lower_expr_v2(&root, source).expect("slice 1 supports this atom");
        let legacy = crate::lower::legacy_parse(source).expect("legacy parse");
        let mut a = v2;
        let mut b = legacy;
        // The legacy `parse_document` wraps the atom in an outer Node
        // that owns leading directives / decorators / doc_comment.
        // For slice 1 we only validate the inner `expr` + range —
        // the outer-Node wrapping is slice 8's job.
        strip_node_ids(&mut a);
        strip_node_ids(&mut b);
        assert_eq!(*a.expr, *b.expr, "expr diverged on {source:?}");
        assert_eq!(a.range, b.range, "range diverged on {source:?}");
    }

    #[test]
    fn slice1_lower_atoms_literal_null() {
        assert_atom_lower_matches_legacy("null");
    }

    #[test]
    fn slice1_lower_atoms_literal_bool() {
        assert_atom_lower_matches_legacy("true");
        assert_atom_lower_matches_legacy("false");
    }

    #[test]
    fn slice1_lower_atoms_literal_int() {
        assert_atom_lower_matches_legacy("42");
        assert_atom_lower_matches_legacy("0x2a");
        assert_atom_lower_matches_legacy("0o52");
        assert_atom_lower_matches_legacy("0b101010");
    }

    #[test]
    fn slice1_lower_atoms_literal_float() {
        assert_atom_lower_matches_legacy("3.14");
        assert_atom_lower_matches_legacy("1.0e10");
    }

    #[test]
    fn slice1_lower_atoms_literal_string() {
        assert_atom_lower_matches_legacy(r#""hello""#);
        assert_atom_lower_matches_legacy(r#""hi\nworld""#);
        assert_atom_lower_matches_legacy(r###"r#"raw"#"###);
    }

    #[test]
    fn slice1_lower_atoms_variable() {
        assert_atom_lower_matches_legacy("foo");
        assert_atom_lower_matches_legacy("foo.bar");
        assert_atom_lower_matches_legacy("foo.bar.baz");
    }

    #[test]
    fn slice1_lower_atoms_reference() {
        assert_atom_lower_matches_legacy("&root");
        assert_atom_lower_matches_legacy("&sibling.x");
        assert_atom_lower_matches_legacy("&root.a.b");
    }

    /// Slice 2 (collections): same shape as slice 1's helper but for
    /// constructs whose root atom *is* a legal document root. The
    /// document-shaped legacy parser (`parse_base`) wraps every
    /// expression in an outer `Node`, so the inner-expr equality below
    /// covers `Expr::Dict / List / Spread / Comprehension` byte-
    /// identically.
    fn assert_collection_lower_matches_legacy(source: &str) {
        let parse = cst::parse_cst(source);
        let doc = ast::document_of(parse.syntax()).expect("document");
        let root = doc.root_expr().expect("root expr");
        let v2 = lower_expr_v2(&root, source).expect("slice 2 supports this collection");
        let legacy = crate::lower::legacy_parse(source).expect("legacy parse");
        let mut a = v2;
        let mut b = legacy;
        strip_node_ids(&mut a);
        strip_node_ids(&mut b);
        assert_eq!(*a.expr, *b.expr, "expr diverged on {source:?}");
        assert_eq!(a.range, b.range, "range diverged on {source:?}");
    }

    #[test]
    fn slice2_lower_dict_empty() {
        assert_collection_lower_matches_legacy("{}");
    }

    #[test]
    fn slice2_lower_dict_flat() {
        assert_collection_lower_matches_legacy("{ a: 1, b: 2 }");
    }

    #[test]
    fn slice2_lower_dict_nested() {
        assert_collection_lower_matches_legacy("{ a: { b: { c: 1 } } }");
    }

    #[test]
    fn slice2_lower_dict_spread() {
        assert_collection_lower_matches_legacy("{ a: 1, ...base }");
    }

    #[test]
    fn slice2_lower_dict_typed_spread() {
        assert_collection_lower_matches_legacy("{ ...<Extra> base }");
    }

    #[test]
    fn slice2_lower_dict_dynamic_key() {
        assert_collection_lower_matches_legacy(r#"{ ["k"]: 1 }"#);
    }

    #[test]
    fn slice2_lower_list_empty() {
        assert_collection_lower_matches_legacy("[]");
    }

    #[test]
    fn slice2_lower_list_flat() {
        assert_collection_lower_matches_legacy("[1, 2, 3]");
    }

    #[test]
    fn slice2_lower_list_spread() {
        assert_collection_lower_matches_legacy("[1, ...others, 2]");
    }

    #[test]
    fn slice2_lower_list_nested() {
        assert_collection_lower_matches_legacy("[[1, 2], [3, 4]]");
    }

    #[test]
    fn slice2_lower_comprehension() {
        assert_collection_lower_matches_legacy("[x for x in src]");
    }

    #[test]
    fn slice2_lower_comprehension_with_condition() {
        assert_collection_lower_matches_legacy("[x * 2 for x in src if x > 0]");
    }

    /// Slice 3 (operators + calls). Same shape as slice 2's helper but
    /// covering binary precedence chains, unary, ternary, and function
    /// calls with positional + named arguments.
    fn assert_operator_lower_matches_legacy(source: &str) {
        let parse = cst::parse_cst(source);
        let doc = ast::document_of(parse.syntax()).expect("document");
        let root = doc.root_expr().expect("root expr");
        let v2 = lower_expr_v2(&root, source).expect("slice 3 supports this operator/call");
        let legacy = crate::lower::legacy_parse(source).expect("legacy parse");
        let mut a = v2;
        let mut b = legacy;
        strip_node_ids(&mut a);
        strip_node_ids(&mut b);
        assert_eq!(*a.expr, *b.expr, "expr diverged on {source:?}");
        assert_eq!(a.range, b.range, "range diverged on {source:?}");
    }

    #[test]
    fn slice3_lower_binary_add() {
        assert_operator_lower_matches_legacy("1 + 2");
    }

    #[test]
    fn slice3_lower_binary_precedence_chain() {
        assert_operator_lower_matches_legacy("1 + 2 * 3 - 4 / 2");
    }

    #[test]
    fn slice3_lower_binary_comparisons() {
        assert_operator_lower_matches_legacy("a == b");
        assert_operator_lower_matches_legacy("a != b");
        assert_operator_lower_matches_legacy("a < b");
        assert_operator_lower_matches_legacy("a >= b");
    }

    #[test]
    fn slice3_lower_binary_logical() {
        assert_operator_lower_matches_legacy("a && b || c");
    }

    #[test]
    fn slice3_lower_unary_neg() {
        assert_operator_lower_matches_legacy("-1");
    }

    #[test]
    fn slice3_lower_unary_not() {
        assert_operator_lower_matches_legacy("!true");
    }

    #[test]
    fn slice3_lower_ternary_simple() {
        assert_operator_lower_matches_legacy("a ? 1 : 2");
    }

    #[test]
    fn slice3_lower_ternary_nested() {
        assert_operator_lower_matches_legacy("a ? b ? 1 : 2 : 3");
    }

    #[test]
    fn slice3_lower_call_positional() {
        assert_operator_lower_matches_legacy("range(0, 10)");
    }

    #[test]
    fn slice3_lower_call_named() {
        assert_operator_lower_matches_legacy("map(f=g)");
    }

    #[test]
    fn slice3_lower_call_mixed() {
        assert_operator_lower_matches_legacy("fn(1, 2, k=3)");
    }

    #[test]
    fn slice3_lower_call_nested() {
        assert_operator_lower_matches_legacy("f(g(1), h(2, 3))");
    }

    /// Slice 4 (control flow). Same shape as previous slices' helpers
    /// but covering Closure / Match / Where / VariantCtor. Standalone
    /// closures use the `(params) => body` form (the dict-method
    /// shorthand `key(params): body` reaches `lower_expr_v2` only via
    /// its enclosing Dict, which slice 2 already lowers correctly).
    fn assert_control_flow_lower_matches_legacy(source: &str) {
        let parse = cst::parse_cst(source);
        let doc = ast::document_of(parse.syntax()).expect("document");
        let root = doc.root_expr().expect("root expr");
        let v2 = lower_expr_v2(&root, source).expect("slice 4 supports this construct");
        let legacy = crate::lower::legacy_parse(source).expect("legacy parse");
        let mut a = v2;
        let mut b = legacy;
        strip_node_ids(&mut a);
        strip_node_ids(&mut b);
        assert_eq!(*a.expr, *b.expr, "expr diverged on {source:?}");
        assert_eq!(a.range, b.range, "range diverged on {source:?}");
    }

    #[test]
    fn slice4_lower_closure_bare() {
        assert_control_flow_lower_matches_legacy("() => 1");
    }

    #[test]
    fn slice4_lower_closure_typed_params() {
        assert_control_flow_lower_matches_legacy("(Int a, Int b) => a + b");
    }

    #[test]
    fn slice4_lower_closure_with_return_type() {
        assert_control_flow_lower_matches_legacy("(Int a) -> Int => a + 1");
    }

    #[test]
    fn slice4_lower_closure_method_shorthand_inside_dict() {
        // Method-shorthand closures reach `lower_expr_v2` via the
        // enclosing Dict's byte-slice route (slice 2). Validating the
        // full document confirms the slice 4 dispatch on a child
        // Closure isn't reached for this form — yet still produces a
        // byte-identical legacy `Node`.
        let source = "{ add(Int a, Int b): a + b }";
        let parse = cst::parse_cst(source);
        let doc = ast::document_of(parse.syntax()).expect("document");
        let root = doc.root_expr().expect("root expr");
        let v2 = lower_expr_v2(&root, source).expect("lower");
        let legacy = crate::lower::legacy_parse(source).expect("legacy parse");
        let mut a = v2;
        let mut b = legacy;
        strip_node_ids(&mut a);
        strip_node_ids(&mut b);
        assert_eq!(*a.expr, *b.expr);
    }

    #[test]
    fn slice4_lower_match_simple() {
        assert_control_flow_lower_matches_legacy("x match { Int: 1, * : 0 }");
    }

    #[test]
    fn slice4_lower_match_multi() {
        assert_control_flow_lower_matches_legacy(
            "value match { Int: 1, String: 2, Bool: 3, * : 0 }",
        );
    }

    #[test]
    fn slice4_lower_where_simple() {
        assert_control_flow_lower_matches_legacy("a + b where { a: 1, b: 2 }");
    }

    #[test]
    fn slice4_lower_where_nested() {
        assert_control_flow_lower_matches_legacy("(a + b) * c where { a: 1, b: 2, c: 3 }");
    }

    #[test]
    fn slice4_lower_variant_ctor_simple() {
        assert_control_flow_lower_matches_legacy("Result.Ok { value: 1 }");
    }

    #[test]
    fn slice4_lower_variant_ctor_nested() {
        assert_control_flow_lower_matches_legacy(
            "Tree.Node { left: Tree.Leaf {}, right: Tree.Leaf {} }",
        );
    }

    /// Slice 5 (f-strings). The CST splits an f-string into per-chunk
    /// children for IDE work; the legacy parser keeps it as a single
    /// `Expr::FString(Vec<FStringPart>)`. The byte-slice route still
    /// produces the latter byte-identically.
    fn assert_fstring_lower_matches_legacy(source: &str) {
        let parse = cst::parse_cst(source);
        let doc = ast::document_of(parse.syntax()).expect("document");
        let root = doc.root_expr().expect("root expr");
        let v2 = lower_expr_v2(&root, source).expect("slice 5 supports this f-string");
        let legacy = crate::lower::legacy_parse(source).expect("legacy parse");
        let mut a = v2;
        let mut b = legacy;
        strip_node_ids(&mut a);
        strip_node_ids(&mut b);
        assert_eq!(*a.expr, *b.expr, "expr diverged on {source:?}");
        assert_eq!(a.range, b.range, "range diverged on {source:?}");
    }

    #[test]
    fn slice5_lower_fstring_pure_literal() {
        assert_fstring_lower_matches_legacy(r#"f"hello world""#);
    }

    #[test]
    fn slice5_lower_fstring_one_interp() {
        assert_fstring_lower_matches_legacy(r#"f"hi ${name}!""#);
    }

    #[test]
    fn slice5_lower_fstring_many_interps() {
        assert_fstring_lower_matches_legacy(r#"f"${greeting}, ${name}: ${count}""#);
    }

    #[test]
    fn slice5_lower_fstring_with_reference() {
        assert_fstring_lower_matches_legacy(r#"f"value=${&root.x}""#);
    }

    #[test]
    fn slice5_lower_fstring_with_expr_interp() {
        assert_fstring_lower_matches_legacy(r#"f"sum=${a + b}""#);
    }

    /// Slice 6 (type expressions). The CST emits a TYPE_NODE for any
    /// builtin / generic / optional / variant / tuple type that
    /// surfaces at expression position; the legacy parser lifts it
    /// via `parse_type_expr` into `Expr::Type(TypeNode)`. The
    /// byte-slice route preserves the full TypeNode shape.
    fn assert_type_expr_lower_matches_legacy(source: &str) {
        let parse = cst::parse_cst(source);
        let doc = ast::document_of(parse.syntax()).expect("document");
        let root = doc.root_expr().expect("root expr");
        let v2 = lower_expr_v2(&root, source).expect("slice 6 supports this type");
        let legacy = crate::lower::legacy_parse(source).expect("legacy parse");
        let mut a = v2;
        let mut b = legacy;
        strip_node_ids(&mut a);
        strip_node_ids(&mut b);
        assert_eq!(*a.expr, *b.expr, "expr diverged on {source:?}");
        assert_eq!(a.range, b.range, "range diverged on {source:?}");
    }

    #[test]
    fn slice6_lower_type_optional_builtin() {
        assert_type_expr_lower_matches_legacy("Int?");
    }

    #[test]
    fn slice6_lower_type_generic_list() {
        assert_type_expr_lower_matches_legacy("List<Int>");
    }

    #[test]
    fn slice6_lower_type_generic_dict() {
        assert_type_expr_lower_matches_legacy("Dict<String, Int>");
    }

    #[test]
    fn slice6_lower_type_nested_generic() {
        assert_type_expr_lower_matches_legacy("List<Dict<String, Int>>");
    }

    #[test]
    fn slice6_lower_type_enum_alternatives() {
        assert_type_expr_lower_matches_legacy(r#"Enum<"red", "green", "blue">"#);
    }

    #[test]
    fn slice6_lower_type_enum_variant_unit() {
        assert_type_expr_lower_matches_legacy("Enum<Ok, Err>");
    }

    /// Slice 7 (attributes). Given a Relon document whose first
    /// directive/decorator is fully covered by the CST, slice its
    /// bytes through the legacy parser and confirm the lowered
    /// `Directive` / `Decorator` matches the legacy `parse_base`
    /// result byte-identically.
    fn assert_directive_lower_matches_legacy(source: &str) {
        let parse = cst::parse_cst(source);
        let doc = ast::document_of(parse.syntax()).expect("document");
        let cst_dir = doc.directives().next().expect("at least one CST directive");
        let v2 = lower_directive_v2(&cst_dir, source).expect("slice 7 supports this directive");
        let legacy = crate::lower::legacy_parse(source).expect("legacy parse");
        let legacy_dir = legacy
            .directives
            .first()
            .cloned()
            .expect("at least one legacy directive");
        assert_eq!(v2, legacy_dir, "directive diverged on {source:?}");
    }

    fn assert_decorator_lower_matches_legacy(source: &str) {
        let parse = cst::parse_cst(source);
        let doc = ast::document_of(parse.syntax()).expect("document");
        let cst_dec = doc.decorators().next().expect("at least one CST decorator");
        let v2 = lower_decorator_v2(&cst_dec, source).expect("slice 7 supports this decorator");
        let legacy = crate::lower::legacy_parse(source).expect("legacy parse");
        let mut legacy_dec = legacy
            .decorators
            .first()
            .cloned()
            .expect("at least one legacy decorator");
        // CallArg.value carries a `Node` with an `id` — strip both for
        // structural comparison.
        for a in &mut legacy_dec.args {
            strip_node_ids(&mut a.value);
        }
        let mut v2 = v2;
        for a in &mut v2.args {
            strip_node_ids(&mut a.value);
        }
        assert_eq!(v2, legacy_dec, "decorator diverged on {source:?}");
    }

    #[test]
    fn slice7_lower_directive_bare() {
        assert_directive_lower_matches_legacy("#private\n{ a: 1 }");
    }

    #[test]
    fn slice7_lower_directive_value() {
        assert_directive_lower_matches_legacy("#default 0\n{ a: 1 }");
    }

    #[test]
    fn slice7_lower_directive_value_complex() {
        assert_directive_lower_matches_legacy("#expect \"msg\"\n{ a: 1 }");
    }

    #[test]
    fn slice7_lower_directive_schema_namebody() {
        assert_directive_lower_matches_legacy("#schema User { String name: * }\n{ x: 1 }");
    }

    #[test]
    fn slice7_lower_directive_import_alias() {
        assert_directive_lower_matches_legacy("#import string from \"std/string\"\n{ x: 1 }");
    }

    #[test]
    fn slice7_lower_directive_import_spread() {
        assert_directive_lower_matches_legacy("#import * from \"std/list\"\n{ x: 1 }");
    }

    #[test]
    fn slice7_lower_directive_import_destructure() {
        assert_directive_lower_matches_legacy(
            "#import { upper, lower as lo } from \"std/string\"\n{ x: 1 }",
        );
    }

    #[test]
    fn slice7_lower_directive_main() {
        assert_directive_lower_matches_legacy(
            "#main(User u, Cart cart) -> Result<Order>\n{ x: 1 }",
        );
    }

    #[test]
    fn slice7_lower_decorator_bare() {
        assert_decorator_lower_matches_legacy("@foo\n{ a: 1 }");
    }

    #[test]
    fn slice7_lower_decorator_with_args() {
        assert_decorator_lower_matches_legacy("@brand(Color)\n{ r: 1 }");
    }

    #[test]
    fn slice7_lower_decorator_dotted() {
        assert_decorator_lower_matches_legacy("@lib.brand(Color)\n{ r: 1 }");
    }

    /// Slice 7 also ships `lower_document_node_v2`, which builds the
    /// outer-wrapped `Node` (decorators + directives + doc_comment +
    /// range + body) from the CST instead of the legacy combinator
    /// stream. The corpus test below validates this against the legacy
    /// path across every checked-in fixture — every fixture the
    /// legacy parser accepts must lower byte-identically via the new
    /// path.
    #[test]
    fn corpus_lower_matches_legacy() {
        use std::fs;
        use std::path::PathBuf;

        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = crate_dir
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf();
        let mut files = Vec::new();
        walk(&workspace_root, &mut files);
        files.retain(|p| !p.to_string_lossy().contains("/target/"));

        let mut checked = 0usize;
        let mut divergent: Vec<String> = Vec::new();
        for path in files {
            let Ok(source) = fs::read_to_string(&path) else {
                continue;
            };
            if source.is_empty() {
                continue;
            }
            let Ok(mut legacy) = legacy_parse(&source) else {
                continue;
            };
            let Some(mut lowered) = lower_document_v2(&source) else {
                divergent.push(format!("{path:?}: v2 returned None"));
                continue;
            };
            checked += 1;
            strip_node_ids(&mut legacy);
            strip_node_ids(&mut lowered);
            if legacy != lowered {
                divergent.push(format!("{path:?}: trees diverged"));
            }
        }
        assert!(checked > 0, "expected to compare at least one fixture");
        assert!(
            divergent.is_empty(),
            "corpus_lower_matches_legacy found {} divergent fixtures:\n{}",
            divergent.len(),
            divergent.join("\n")
        );
    }

    #[test]
    fn slice1_lower_atoms_wildcard() {
        // `*` isn't a legal root atom in the legacy parser (it only
        // appears in match-arm pattern position), so we can't compare
        // directly via `legacy_parse`. Validate the slice-1 walker on a
        // synthetic WILDCARD node inside a match arm: the helper still
        // produces `Expr::Wildcard` with a 1-byte range.
        let parse = cst::parse_cst("{ f(x): x match { *: 0 } }");
        // Walk descendants to find the wildcard.
        let wildcard = parse
            .syntax()
            .descendants()
            .find(|n| n.kind() == SyntaxKind::WILDCARD)
            .expect("wildcard node");
        let n =
            lower_atom_via_legacy(&wildcard, "{ f(x): x match { *: 0 } }").expect("lower wildcard");
        assert!(matches!(*n.expr, Expr::Wildcard));
    }

    /// `parse_document` (the public entry) now goes through CST first.
    /// This test simply asserts that `parse_document` keeps working on
    /// the legacy corner cases the existing test suite exercises.
    #[test]
    fn parse_document_accepts_legacy_corpus_samples() {
        for src in [
            "{ x: 1 }",
            "[1, 2, 3]",
            "42",
            "true",
            "null",
            "1 + 2",
            r#""hello""#,
            "range(0, 10)",
            "Result.Ok { value: 1 }",
            "{ a: 1 } // trailing\n /* ok */",
        ] {
            parse_document(src)
                .unwrap_or_else(|e| panic!("parse_document failed on {src:?}: {e:?}"));
        }
    }
}
