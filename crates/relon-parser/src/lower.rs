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
//! Design — hybrid CST-walking lowering
//! ====================================
//!
//! The legacy combinator parser produces a *very* specific `Node` shape:
//! byte-exact ranges, a particular `NodeId::alloc()` order, doc-comment
//! attachment rules, decorator/directive interleaving, type-hint
//! lifting, generic-vs-comparison disambiguation, tuple-type encoding,
//! enum-variant struct bodies, and a dozen other quirks. Re-implementing
//! every quirk in a from-scratch CST walker would be a multi-week effort
//! with a long tail of off-by-one failures.
//!
//! P4 takes a pragmatic *byte-slice* approach: each typed `ast::Expr`
//! (or `ast::Directive` / `ast::Decorator`) holds a CST node whose
//! `text_range()` is byte-exact. The lowering slices the original
//! source to that range, runs the relevant legacy combinator
//! (`parse_expr` / `parse_directive` / `parse_decorator`) on the
//! sliced bytes, and translates the produced `TokenRange`s back onto
//! the full source via [`translate_node_offsets`] (+ the directive /
//! decorator / type-node specializations below). The result is a
//! byte-identical `Node` tree without re-implementing dict / list /
//! comprehension / binary-precedence / call-arg / closure / match /
//! variant-ctor / f-string / typed-spread / typed-dynamic-key /
//! with-block-method / schema-method-param / generic / optional /
//! variant-fields / etc. machinery here.
//!
//! Dispatch
//! --------
//!
//! [`lower_document`] is the entry point invoked by
//! [`crate::parse_document`]. P5 retired the top-level legacy
//! combinator fallback — the v2 path is now the single source of
//! truth for what `parse_document` accepts. When the CST cleanly
//! accepts the input AND [`lower_document_node_v2`] succeeds, the
//! new path produces the `Node` tree; any failure (CST errors, or
//! v2-lowering rejecting a byte-slice) surfaces as a typed
//! [`ParseDocumentError`] without re-running [`parse_base`].
//!
//! The previously-known CST grammar gaps — `#schema X: { ... }`
//! colon-separated body, optional-chain `?.` / `?[`, and `#import
//! { a, b as c } from ...` destructure — are closed in `cst.rs`.
//! Inputs that historically required the legacy fallback now reach
//! the v2 path directly.
//!
//! [`lower_expr_v2`] dispatches per `ast::Expr` variant. Each variant
//! routes to either [`lower_atom_via_legacy`] (atoms) or
//! [`lower_expr_via_legacy`] (every composite construct). The
//! "via_legacy" naming is historical — these helpers re-run the
//! per-construct winnow combinator on the CST node's byte slice to
//! produce the byte-identical legacy `Node` shape. They remain
//! load-bearing because re-implementing typed-Node construction
//! directly from rowan walks is P6 territory; the [`legacy_parse`]
//! retired in P5 was specifically the *top-level* `parse_base`
//! fallback in [`lower_document`], not the per-construct byte-slice
//! routes used by `lower_*_v2`. Slice-level comparison tests in
//! [`tests`] assert per-construct byte-identical parity with the
//! cfg(test)-only [`legacy_parse`] for every construct family.

use crate::ast;
use crate::cst::Parse;
use crate::syntax::{SyntaxKind, SyntaxNode};
use crate::{position_at_source, Expr, Node, ParseDocumentError, RefBase, Span, TokenKey, TokenRange};
#[cfg(test)]
use {crate::parse_base, winnow::stream::Location};

// =====================================================================
// P6 progress notes
// =====================================================================
//
// Inventory of `lower_*_v2` helpers and the legacy combinator they
// still bridge to (as of mid-P6). Complexity reflects the work needed
// to rewrite each helper as a direct rowan walk that constructs the
// legacy `Node` shape without re-entering the winnow combinator.
//
//   helper                                | legacy fn                      | file                         | complexity
//   --------------------------------------|--------------------------------|------------------------------|-----------
//   lower_atom_via_legacy / WILDCARD      | (inlined: `*` → Expr::Wildcard)| —                            | done
//   lower_atom_via_legacy / LITERAL       | parse_null/bool/number/string  | prim/{null,boolean,number,   | small (null, bool)
//                                         |                                | string}.rs                   | medium (number, string escapes)
//   lower_atom_via_legacy / VARIABLE_EXPR | var::parse_var                 | var.rs                       | small (CST nodes hold tokens)
//   lower_atom_via_legacy / REFERENCE_EXPR| reference_var::parse_ref_var   | reference_var.rs             | small
//   lower_decorator_v2                    | decorator::parse_decorator     | decorator.rs                 | small
//   lower_directive_v2                    | directive::parse_directive     | directive.rs                 | large (5 shapes, schema body,
//                                         |                                |                              |   with-block, main params)
//   lower_expr_via_legacy / Dict/List/    | expr::parse_expr               | expr.rs                      | large (operator precedence,
//     Spread/Comprehension/Binary/Unary/  |                                |                              |   call args, closures, match,
//     Ternary/Call/Closure/Match/Where/   |                                |                              |   variant ctors, type expr,
//     VariantCtor/FString/Type            |                                |                              |   f-strings)
//
// Coupling: `decorator.rs` / `directive.rs` recursively call
// `expr::parse_expr`; `var.rs` / `reference_var.rs` recursively call
// `parse_expr` for dynamic-key (`[expr]`) accesses. `expr.rs` calls
// all the prim modules. `parse_base` + `parse_attributes` (in
// `lib.rs`) and `structure/{dict,list}.rs` are only reachable from
// `parse_expr` and from test-only `legacy_parse`. As a result the
// legacy modules form a single connected web — deletion is gated on
// the `parse_expr` / `parse_directive` retirement. The lower_*_v2
// bridges, however, can be replaced one at a time provided each
// rewrite preserves byte-identical `Node` output (the
// `corpus_lower_matches_legacy` test in this module is the gate).
//
// Strategy: rewrite the smallest bridges first (WILDCARD ✓, then
// VARIABLE / REFERENCE / LITERAL atoms, then `lower_decorator_v2`),
// and only then attack the large ones. Once every `lower_*_v2`
// stops calling its legacy counterpart, the whole legacy module web
// can be deleted in one commit because nothing else (outside their
// own tests + `lib.rs::parse_base`) depends on them.

// =====================================================================
// CST-walking lowering — P4 implementation.
//
// Each construct lives in its own `lower_*_v2` function. The functions
// take a typed `ast::*` wrapper plus the original source text and
// produce a legacy `Node` byte-identical to what the combinator chain
// would emit. The CST gives us the exact byte range; the legacy
// combinator produces the typed Node shape on the slice; the
// `translate_*_offsets` helpers below lift slice-local ranges onto
// the full source.
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
    match node.kind() {
        // CST walker — no byte-slice re-parse needed. The CST already
        // carries the typed tokens; we read them off in order and
        // build the legacy `Vec<TokenKey>` directly. Dynamic-key
        // (`[expr]`) accesses recurse through `lower_expr_v2`, which
        // is itself the lowering dispatch — keeping a single source
        // of truth for nested expressions.
        SyntaxKind::VARIABLE_EXPR => lower_variable_expr_v2(node, source),
        SyntaxKind::REFERENCE_EXPR => lower_reference_expr_v2(node, source),
        SyntaxKind::WILDCARD => {
            let r = node.text_range();
            let start: usize = r.start().into();
            let end: usize = r.end().into();
            let slice = source.get(start..end)?;
            if slice == "*" {
                Some(Node::new(
                    Expr::Wildcard,
                    range_from_offsets(source, start, end),
                ))
            } else {
                None
            }
        }
        SyntaxKind::LITERAL => lower_literal_v2(node, source),
        _ => None,
    }
}

/// Lower a `LITERAL` CST node to the corresponding `Node`. The
/// CST groups null / true / false / NUMBER / STRING under a single
/// LITERAL kind; we dispatch by inspecting the inner token. Bool
/// and null are inlined; number and string keep delegating to the
/// prim combinators (escape decoding / overflow handling lives
/// there). Either way the leaf parser runs on the exact token
/// bytes — no recursion through `parse_expr` happens here.
fn lower_literal_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let token = node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| {
            matches!(
                t.kind(),
                SyntaxKind::IDENT | SyntaxKind::NUMBER | SyntaxKind::STRING
            )
        })?;
    let tr = token.text_range();
    let start: usize = tr.start().into();
    let end: usize = tr.end().into();
    let range = range_from_offsets(source, start, end);
    match token.kind() {
        SyntaxKind::IDENT => match token.text() {
            "null" => Some(Node::new(Expr::Null, range)),
            "true" => Some(Node::new(Expr::Bool(true), range)),
            "false" => Some(Node::new(Expr::Bool(false), range)),
            _ => None,
        },
        SyntaxKind::NUMBER => {
            // Numbers carry hex / oct / bin / scientific / Infinity /
            // NaN parsing inside `prim::number::parse_number`. The
            // token text is exactly the number literal — no
            // surrounding bytes — so we run the combinator on it
            // directly and translate the slice-local offsets back.
            let slice = source.get(start..end)?;
            let mut span = Span::new(slice);
            use winnow::Parser as _;
            let mut node_value = crate::prim::number::parse_number
                .parse_next(&mut span)
                .ok()?;
            translate_node_offsets(&mut node_value, start, source);
            Some(node_value)
        }
        SyntaxKind::STRING => {
            // Strings own escape decoding (`\n`, `\u{...}`, raw
            // `r#"..."#`). Same drill — leaf combinator on the
            // token text, then translate.
            let slice = source.get(start..end)?;
            let mut span = Span::new(slice);
            use winnow::Parser as _;
            let mut node_value = crate::prim::string::parse_string
                .parse_next(&mut span)
                .ok()?;
            translate_node_offsets(&mut node_value, start, source);
            Some(node_value)
        }
        _ => None,
    }
}

/// Walk a `VARIABLE_EXPR` CST node and rebuild the legacy
/// `Expr::Variable(Vec<TokenKey>)` shape directly. The CST tracks
/// every path token (head IDENT, `.` / `?.` access, `[expr]` /
/// `?[expr]` dynamic, numeric `.N` index) as siblings under the
/// VARIABLE_EXPR node. We walk the `children_with_tokens()` iter
/// once, threading a "pending optional" flag for the `?.` / `?[`
/// prefix, and emit one `TokenKey` per segment.
///
/// Dynamic-key (`[expr]`) accesses recurse through [`lower_expr_v2`]
/// — the same dispatch the outer lowering uses — so the bracket-
/// expression shape matches whatever the rest of the pipeline
/// produces (e.g. inner BINARY_EXPR with operator precedence).
fn lower_variable_expr_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();
    let path = walk_path_tokens(node, source, /*is_reference=*/ false)?;
    Some(Node::new(
        Expr::Variable(path),
        range_from_offsets(source, start, end),
    ))
}

/// Walk a `REFERENCE_EXPR` CST node and rebuild the legacy
/// `Expr::Reference { base, path }` shape. The CST emits `&` as a
/// leaf token, then a single IDENT for the base name, then the same
/// path-token alternation as `VARIABLE_EXPR`.
fn lower_reference_expr_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();
    // First IDENT after the `&` AMP token names the reference base.
    // Locate it without consuming any other tokens — `walk_path_tokens`
    // will re-walk from the start and skip both `&` + base IDENT.
    let base_text = node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| t.kind() == SyntaxKind::IDENT)
        .map(|t| t.text().to_string())?;
    let base = match base_text.as_str() {
        "root" => RefBase::Root,
        "sibling" => RefBase::Sibling,
        "uncle" => RefBase::Uncle,
        "prev" => RefBase::Prev,
        "next" => RefBase::Next,
        "index" => RefBase::Index,
        "this" => RefBase::This,
        _ => return None,
    };
    let path = walk_path_tokens(node, source, /*is_reference=*/ true)?;
    Some(Node::new(
        Expr::Reference { base, path },
        range_from_offsets(source, start, end),
    ))
}

/// Read the path tokens off a `VARIABLE_EXPR` / `REFERENCE_EXPR` node.
/// For variables (`is_reference = false`) the first IDENT is the
/// head segment (`a` in `a.b[0]`) and is emitted as a non-optional
/// `TokenKey::String`. For references (`is_reference = true`) the
/// first IDENT (after the `&` AMP token) is the base name; the
/// caller consumed it already, so we skip it here and start the
/// path with whatever follows.
///
/// The token loop mirrors the legacy `parse_path` / `parse_ref_var`
/// shape exactly: a `?` is consumed as an optional prefix only when
/// followed by `.` or `[`; bare `?` would have been emitted as a
/// ternary operator at a different position in the grammar.
fn walk_path_tokens(
    node: &SyntaxNode,
    source: &str,
    is_reference: bool,
) -> Option<Vec<TokenKey>> {
    let mut path: Vec<TokenKey> = Vec::new();
    // Tracks whether we've already consumed the head — either the
    // first IDENT (variable) or the `&` + base IDENT pair
    // (reference). For variables, the parser also folds nested
    // VARIABLE_EXPR children at the head position (the postfix
    // re-wrapper at `parse_postfix`), so a head VARIABLE_EXPR's
    // path gets flattened into ours.
    let mut head_done = false;
    let mut pending_optional = false;
    let mut expect_segment_after_dot = false;

    for el in node.children_with_tokens() {
        match el {
            rowan::NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::WHITESPACE
                | SyntaxKind::LINE_COMMENT
                | SyntaxKind::BLOCK_COMMENT => continue,
                SyntaxKind::AMP => continue,
                SyntaxKind::IDENT => {
                    if !head_done {
                        head_done = true;
                        if is_reference {
                            // Base name — caller already captured it
                            // as `RefBase`. We do NOT push a path
                            // segment for the base.
                            continue;
                        }
                        // Head segment of a VARIABLE_EXPR.
                        let tr = t.text_range();
                        let s: usize = tr.start().into();
                        let e: usize = tr.end().into();
                        path.push(TokenKey::String(
                            t.text().to_string(),
                            range_from_offsets(source, s, e),
                            false,
                        ));
                        continue;
                    }
                    if expect_segment_after_dot {
                        let tr = t.text_range();
                        let s: usize = tr.start().into();
                        let e: usize = tr.end().into();
                        path.push(TokenKey::String(
                            t.text().to_string(),
                            range_from_offsets(source, s, e),
                            pending_optional,
                        ));
                        pending_optional = false;
                        expect_segment_after_dot = false;
                        continue;
                    }
                    // Unexpected bare IDENT — should never happen in a
                    // well-formed VARIABLE_EXPR / REFERENCE_EXPR.
                    return None;
                }
                SyntaxKind::NUMBER => {
                    if expect_segment_after_dot {
                        let idx: usize = t.text().parse().ok()?;
                        path.push(TokenKey::Index(idx, pending_optional));
                        pending_optional = false;
                        expect_segment_after_dot = false;
                        continue;
                    }
                    return None;
                }
                SyntaxKind::DOT => {
                    expect_segment_after_dot = true;
                    continue;
                }
                SyntaxKind::QUESTION => {
                    // Legacy grammar: `?` only stands in the path when
                    // followed by `.` or `[`. The CST grammar enforces
                    // the same lookahead before emitting the QUESTION
                    // as a path token; if we encounter one here, it's
                    // always the optional-chain prefix.
                    pending_optional = true;
                    continue;
                }
                SyntaxKind::L_BRACK | SyntaxKind::R_BRACK => continue,
                _ => continue,
            },
            rowan::NodeOrToken::Node(n) => {
                // Two shapes nest a Node inside a VARIABLE_EXPR /
                // REFERENCE_EXPR:
                //
                //   1. A nested VARIABLE_EXPR at the head position.
                //      `parse_postfix` reopens VARIABLE_EXPR at a
                //      checkpoint covering the *atom* it just parsed,
                //      so for `foo.bar` the CST has the inner `foo`
                //      VARIABLE_EXPR as a child of the outer
                //      VARIABLE_EXPR. Flatten its path into ours.
                //
                //   2. A bracketed expression: `a[expr]` or `a?[expr]`.
                //      The inner Expr recurses through `lower_expr_v2`
                //      so its shape matches the rest of the lowering.
                if !head_done && !is_reference && n.kind() == SyntaxKind::VARIABLE_EXPR {
                    // Flatten the inner VARIABLE_EXPR's path. The
                    // inner path's `is_optional` flags stay as-is
                    // because the inner expression had no leading
                    // `?` (a `?.foo` head would have been parsed as
                    // a ternary, not as a path).
                    let inner_path = walk_path_tokens(&n, source, /*is_reference=*/ false)?;
                    path.extend(inner_path);
                    head_done = true;
                    continue;
                }
                if let Some(inner) = ast::Expr::cast(n.clone()) {
                    let inner_node = lower_expr_v2(&inner, source)?;
                    path.push(TokenKey::Dynamic(inner_node, pending_optional));
                    pending_optional = false;
                }
            }
        }
    }
    Some(path)
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

/// Lower a CST [`ast::Decorator`] to a legacy [`crate::Decorator`] by
/// walking the rowan node directly. The decorator shape is simple
/// enough — dotted IDENT path under the AT sigil, optional CALL_ARG
/// node containing positional / named arguments — that we can
/// construct the legacy `Decorator` without re-entering the legacy
/// combinator chain. Each arg's `value` is lowered through
/// [`lower_expr_v2`], the shared expression dispatch.
///
/// Counterpart to [`lower_directive_v2`].
#[allow(dead_code)]
fn lower_decorator_v2(dec: &ast::Decorator, source: &str) -> Option<crate::Decorator> {
    let node = dec.syntax();
    let r = node.text_range();
    let raw_start: usize = r.start().into();
    let end: usize = r.end().into();
    // Legacy `parse_decorator` starts on the `@` sigil. The CST node
    // range includes leading inter-attribute whitespace, so trim
    // first to find the `@`.
    let raw_slice = source.get(raw_start..end)?;
    let trim = trim_leading_trivia(raw_slice);
    let start = raw_start + trim;

    // Walk children-with-tokens to assemble the path + args.
    let mut path: Vec<TokenKey> = Vec::new();
    let mut args: Vec<crate::CallArg> = Vec::new();
    let mut head_done = false;
    let mut expect_segment_after_dot = false;

    for el in node.children_with_tokens() {
        match el {
            rowan::NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::WHITESPACE
                | SyntaxKind::LINE_COMMENT
                | SyntaxKind::BLOCK_COMMENT
                | SyntaxKind::AT => continue,
                SyntaxKind::IDENT => {
                    let tr = t.text_range();
                    let s: usize = tr.start().into();
                    let e: usize = tr.end().into();
                    let r = range_from_offsets(source, s, e);
                    if !head_done {
                        path.push(TokenKey::String(t.text().to_string(), r, false));
                        head_done = true;
                    } else if expect_segment_after_dot {
                        path.push(TokenKey::String(t.text().to_string(), r, false));
                        expect_segment_after_dot = false;
                    } else {
                        // Stray IDENT — malformed decorator.
                        return None;
                    }
                }
                SyntaxKind::DOT => {
                    expect_segment_after_dot = true;
                }
                _ => continue,
            },
            rowan::NodeOrToken::Node(n) => {
                if n.kind() == SyntaxKind::CALL_ARG {
                    args.extend(walk_call_arg_node(&n, source)?);
                }
            }
        }
    }

    // Legacy verification: once a named arg appears, all subsequent
    // args must also be named. Mirrors the `.verify(...)` closure in
    // `decorator::decorator`.
    let mut saw_named = false;
    for a in &args {
        if a.name.is_some() {
            saw_named = true;
        } else if saw_named {
            return None;
        }
    }

    Some(crate::Decorator {
        path,
        args,
        range: range_from_offsets(source, start, end),
    })
}

/// Walk the children of a CALL_ARG CST node and extract the
/// positional / named arguments as a flat list. Named arguments are
/// detected by the `IDENT EQ <expr>` triple emitted by the CST
/// parser (see `cst::parse_call_arg`).
fn walk_call_arg_node(node: &SyntaxNode, source: &str) -> Option<Vec<crate::CallArg>> {
    let mut args: Vec<crate::CallArg> = Vec::new();
    let mut pending_name: Option<String> = None;
    let mut iter = node.children_with_tokens().peekable();
    while let Some(el) = iter.next() {
        match el {
            rowan::NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::WHITESPACE
                | SyntaxKind::LINE_COMMENT
                | SyntaxKind::BLOCK_COMMENT
                | SyntaxKind::L_PAREN
                | SyntaxKind::R_PAREN
                | SyntaxKind::COMMA => continue,
                SyntaxKind::IDENT => {
                    // Named arg: IDENT followed by EQ (skipping
                    // trivia). Use the same peek-and-eat scheme the
                    // CST parser used to emit the triple.
                    let name = t.text().to_string();
                    let mut after = iter.clone();
                    let mut found_eq = false;
                    for nx in after.by_ref() {
                        if let Some(tt) = nx.as_token() {
                            match tt.kind() {
                                SyntaxKind::WHITESPACE
                                | SyntaxKind::LINE_COMMENT
                                | SyntaxKind::BLOCK_COMMENT => continue,
                                SyntaxKind::EQ => {
                                    found_eq = true;
                                    break;
                                }
                                _ => break,
                            }
                        } else {
                            break;
                        }
                    }
                    if found_eq {
                        // Commit: drain trivia + EQ from the real iter.
                        while let Some(peek) = iter.peek() {
                            let is_eq_or_trivia = peek
                                .as_token()
                                .map(|tt| {
                                    matches!(
                                        tt.kind(),
                                        SyntaxKind::WHITESPACE
                                            | SyntaxKind::LINE_COMMENT
                                            | SyntaxKind::BLOCK_COMMENT
                                            | SyntaxKind::EQ
                                    )
                                })
                                .unwrap_or(false);
                            if is_eq_or_trivia {
                                let eaten = iter.next();
                                if eaten
                                    .as_ref()
                                    .and_then(|e| e.as_token())
                                    .map(|tt| tt.kind() == SyntaxKind::EQ)
                                    .unwrap_or(false)
                                {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                        pending_name = Some(name);
                    } else {
                        // Bare IDENT positional arg? That would
                        // actually be a VARIABLE_EXPR Node child in
                        // the CST, not a raw IDENT token. A raw
                        // IDENT token here is a malformed parse.
                        return None;
                    }
                }
                _ => continue,
            },
            rowan::NodeOrToken::Node(n) => {
                if let Some(expr) = ast::Expr::cast(n.clone()) {
                    let value = lower_expr_v2(&expr, source)?;
                    args.push(crate::CallArg {
                        name: pending_name.take(),
                        value,
                    });
                }
            }
        }
    }
    Some(args)
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
/// P5 (final, post-fallback): the CST is the single source of truth
/// for what inputs `parse_document` accepts. When the CST cleanly
/// accepts the input AND [`lower_document_node_v2`] succeeds, this
/// returns `Ok` with a byte-identical legacy `Node` tree. Any other
/// outcome — CST errors, v2 lowering failure, an empty document —
/// surfaces as a typed [`ParseDocumentError`] without re-running
/// the top-level legacy combinator chain.
///
/// Trailing-input errors are surfaced as
/// [`ParseDocumentError::TrailingInput`] (matching the pre-P4 shape)
/// with the offset stepped past inter-token trivia so callers see
/// the legacy span the analyzer / fmt expect.
pub fn lower_document(parse: &Parse, source: &str) -> Result<crate::Node, ParseDocumentError> {
    // Fast path: clean CST + v2 lowering succeeds. The v2 path is the
    // single source of truth post-P5; the top-level legacy combinator
    // fallback is gone and any failure inside this branch surfaces as
    // a typed `ParseDocumentError`.
    if !parse.has_errors() {
        if let Some(doc) = ast::document_of(parse.syntax()) {
            if doc.root_expr().is_none() {
                return Err(ParseDocumentError::Parse {
                    offset: 0,
                    message: "empty document".to_string(),
                });
            }
            if let Some(node) = lower_document_node_v2(&doc, source) {
                return Ok(node);
            }
            // CST accepted the source, but a byte-slice lowering step
            // (one of the `lower_directive_v2` / `lower_decorator_v2`
            // sub-parses, or the legacy `parse_expr` on a CST node)
            // rejected the contents on semantic grounds — e.g. a
            // `with { ... }` block carrying an unknown pragma, a
            // `#native` method with a body, or an expression form
            // the legacy expression combinator stops short of (e.g.
            // a stray leading `+`). Surface as a parse error at
            // offset 0; the analyzer surfaces a friendlier
            // diagnostic downstream.
            return Err(ParseDocumentError::Parse {
                offset: 0,
                message: "could not lower well-formed CST to legacy Node".to_string(),
            });
        }
        // No DOCUMENT node at all — the lexer produced an empty token
        // stream. Treat as a parse error at offset 0.
        return Err(ParseDocumentError::Parse {
            offset: 0,
            message: "empty document".to_string(),
        });
    }
    // CST surfaced one or more errors. Surface the first one in the
    // typed shape callers expect.
    let err = parse
        .errors
        .first()
        .expect("parse.has_errors() implies at least one error");
    if err.message.starts_with("trailing input after root value") {
        // The legacy `parse_base` consumed inter-token trivia before
        // recording the trailing offset; the CST stops at the next-
        // token-start. Step over whitespace + comments so callers
        // see the legacy offset / remaining shape.
        let mut start = err.offset;
        start += trim_leading_trivia(source.get(start..).unwrap_or(""));
        let remaining: String = source.get(start..).unwrap_or("").chars().take(64).collect();
        return Err(ParseDocumentError::TrailingInput {
            offset: start,
            remaining,
        });
    }
    Err(ParseDocumentError::Parse {
        offset: err.offset,
        message: err.message.clone(),
    })
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
///
/// P5: no longer wired into [`lower_document`]; the public entry now
/// runs the v2 path exclusively. Kept around so slice-level parity
/// tests can keep cross-checking per-construct legacy output without
/// reaching for `parse_base` directly.
#[cfg(test)]
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

    /// P5 diagnostic: inspect CST behaviour on broken-input fixtures.
    /// Mirrors the [`p5_scout_invalid_fixtures`] reporter but reads the
    /// `tests/fixtures/broken/` corpus instead.
    #[test]
    #[ignore]
    fn p5_scout_broken_fixtures() {
        use std::fs;
        use std::path::PathBuf;
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let dir = crate_dir.join("tests/fixtures/broken");
        if !dir.is_dir() {
            eprintln!("(no broken fixtures dir yet)");
            return;
        }
        for entry in fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().and_then(|e| e.to_str()) != Some("relon") {
                continue;
            }
            let source = fs::read_to_string(&p).unwrap();
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let parse = cst::parse_cst(&source);
            eprintln!("=== {name} ===");
            eprintln!("source: {source:?}");
            eprintln!("round-trip: {}", parse.syntax().text().to_string() == source);
            eprintln!("CST errors: {}", parse.errors.len());
            for e in &parse.errors {
                eprintln!("  - {} @ {}", e.message, e.offset);
            }
            let result = lower_document(&parse, &source);
            eprintln!(
                "lower_document: {}",
                match &result {
                    Ok(_) => "Ok".to_string(),
                    Err(e) => format!("Err({e:?})"),
                }
            );
            eprintln!();
        }
    }

    /// P5 diagnostic: inspect what the CST + lowered tree look like for
    /// the `with_block_invalid/*` corpus. The legacy fallback currently
    /// rescues semantic-validity diagnostics on these inputs; the v2
    /// path needs to surface the same errors.
    #[test]
    #[ignore]
    fn p5_scout_invalid_fixtures() {
        use std::fs;
        use std::path::PathBuf;
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let invalid = crate_dir.join("tests/fixtures/with_block_invalid");
        for entry in fs::read_dir(&invalid).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().and_then(|e| e.to_str()) != Some("relon") {
                continue;
            }
            let source = fs::read_to_string(&p).unwrap();
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let parse = cst::parse_cst(&source);
            eprintln!("=== {name} ===");
            eprintln!("CST errors: {}", parse.errors.len());
            for e in &parse.errors {
                eprintln!("  - {} @ {}", e.message, e.offset);
            }
            let result = lower_document(&parse, &source);
            eprintln!(
                "lower_document: {}",
                match &result {
                    Ok(_) => "Ok".to_string(),
                    Err(e) => format!("Err({e:?})"),
                }
            );
            eprintln!();
        }
    }

    /// P5 diagnostic: walk all `.relon` fixtures and confirm that
    /// every one either lowers successfully or surfaces a typed
    /// `ParseDocumentError`. Run with `cargo test -- --ignored
    /// p5_scout_corpus_errors --nocapture` to see the per-fixture
    /// outcome breakdown.
    #[test]
    #[ignore]
    fn p5_scout_corpus_errors() {
        use std::collections::BTreeMap;
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

        let mut by_outcome: BTreeMap<&'static str, Vec<PathBuf>> = BTreeMap::new();
        for path in &files {
            let Ok(source) = fs::read_to_string(path) else {
                continue;
            };
            if source.is_empty() {
                continue;
            }
            let parse = cst::parse_cst(&source);
            let label = match lower_document(&parse, &source) {
                Ok(_) => "ok",
                Err(ParseDocumentError::Parse { .. }) => "parse-error",
                Err(ParseDocumentError::TrailingInput { .. }) => "trailing-input",
            };
            by_outcome.entry(label).or_default().push(path.clone());
        }

        eprintln!("\n=== P5 SCOUT: corpus outcome breakdown ===");
        for (label, paths) in &by_outcome {
            eprintln!("\n[{label}] {} fixture(s)", paths.len());
            for p in paths {
                eprintln!("  {}", p.display());
            }
        }
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

    /// Slice 8: `lower_document` prefers the CST-walking v2 path
    /// whenever the CST cleanly accepts the input. Verify by passing
    /// a `Parse` that has no errors and asserting the result is
    /// byte-identical to a direct `lower_document_node_v2` call.
    #[test]
    fn slice8_clean_cst_routes_through_v2() {
        for src in ["{ a: 1 }", "[1, 2, 3]", "42", "1 + 2", "(Int a) => a + 1"] {
            let parse = cst::parse_cst(src);
            assert!(!parse.has_errors(), "expected clean CST for {src:?}");
            let via_lower = lower_document(&parse, src).expect("lower_document");
            let direct = ast::document_of(parse.syntax())
                .and_then(|d| lower_document_node_v2(&d, src))
                .expect("direct v2");
            let mut a = via_lower;
            let mut b = direct;
            strip_node_ids(&mut a);
            strip_node_ids(&mut b);
            assert_eq!(a, b, "slice 8 didn't route through v2 for {src:?}");
        }
    }

    /// Slice 8: trailing input is reported as `TrailingInput` with
    /// the legacy-shaped offset (after whitespace consumption).
    #[test]
    fn slice8_trailing_input_uses_legacy_offset() {
        let err = crate::parse_document("{ a: 1 } true").unwrap_err();
        assert!(matches!(
            err,
            ParseDocumentError::TrailingInput { offset: 9, ref remaining }
                if remaining == "true"
        ));
    }

    /// P5 closed the CST gap for the `#schema X: { ... }` dict-field
    /// shape — the legacy parser accepted a `:` separator between the
    /// schema name and the dict body, and the CST now does too. This
    /// regression-test pins the behaviour so a future grammar
    /// refactor can't silently re-open the gap.
    #[test]
    fn slice8_schema_colon_body_form_parses() {
        let src = r#"{
            #schema Image: { name: String },
            data: { name: "img" }
        }"#;
        let parse = cst::parse_cst(src);
        let node = lower_document(&parse, src).expect("schema-colon shape lowers cleanly");
        assert!(matches!(*node.expr, Expr::Dict(_)));
        assert!(!parse.has_errors(), "no CST errors: {:?}", parse.errors);
    }

    /// Inspect what the legacy parser produced for `#schema X: { ... }`
    /// so we can match its shape in the v2 path.
    #[test]
    fn slice8_schema_colon_body_matches_legacy_shape() {
        let src = r#"{
            #schema Image: { name: String },
            data: { name: "img" }
        }"#;
        let mut legacy = crate::lower::legacy_parse(src).expect("legacy parse");
        let mut lowered = lower_document(&cst::parse_cst(src), src).expect("lower");
        strip_node_ids(&mut legacy);
        strip_node_ids(&mut lowered);
        assert_eq!(legacy, lowered, "shape diverged");
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
