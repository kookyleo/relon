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
//! The lowering walks the CST directly: each typed `ast::Expr` /
//! `ast::Directive` / `ast::Decorator` carries a rowan node whose
//! `text_range()` is byte-exact, and the lowering converts those
//! ranges into the legacy `Node` shape (`TokenRange` with line /
//! column resolved against the full source). All dict / list /
//! comprehension / binary-precedence / call-arg / closure / match /
//! variant-ctor / f-string / typed-spread / typed-dynamic-key /
//! with-block-method / schema-method-param / generic / optional /
//! variant-fields machinery lives in this file.
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
use crate::{position_at_source, Expr, Node, ParseDocumentError, RefBase, TokenKey, TokenRange};

// Strict vs recovering lowering: when a sub-tree fails to lower
// (malformed DICT_FIELD, rejected schema-method shape, unknown
// pragma) the strict caller must surface `ParseDocumentError`,
// while the recovering caller must skip the bad piece and keep the
// surrounding tree navigable so the IDE can offer completion /
// hover / goto-def on whatever survived. A thread-local toggle
// keeps the helper signatures stable instead of threading a mode
// argument through every `lower_*_v2`.
thread_local! {
    static RECOVERING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn is_recovering() -> bool {
    RECOVERING.with(|c| c.get())
}

/// RAII guard that flips the recovering flag on for the duration of
/// the wrapping call. Restoring on drop keeps the flag balanced
/// even if a downstream helper panics.
pub(crate) struct RecoveringScope {
    prev: bool,
}

impl RecoveringScope {
    pub(crate) fn enter() -> Self {
        let prev = RECOVERING.with(|c| c.replace(true));
        RecoveringScope { prev }
    }
}

impl Drop for RecoveringScope {
    fn drop(&mut self) {
        RECOVERING.with(|c| c.set(self.prev));
    }
}

// =====================================================================
// P6 progress notes
// =====================================================================
//
// P6 round 2 retired every remaining `lower_*_via_legacy` bridge.
// Each construct now has its own CST-walking lowering function that
// constructs the legacy `Node` shape directly off the rowan tree.
// `lower.rs` no longer calls into `expr::parse_expr`,
// `expr::parse_type_node`, `directive::parse_directive`,
// `prim::number::parse_number`, or `prim::string::parse_string` for
// any production. The directive shape lookup
// (`directive::directive_shape`) and the directive name constants
// (`DERIVE` / `NATIVE` / `INTERNAL` / `NO_AUTO_DERIVE`) are still
// imported because they're stable lookup tables used by both the
// CST builder and the lowering walker; they sit in `directive.rs`
// purely for backwards-compat re-export to downstream crates
// (`relon-analyzer`, `relon-evaluator`) and aren't tied to the
// legacy combinator chain.
//
//   helper / construct           | status              | notes
//   -----------------------------|---------------------|--------------------------------------------
//   atoms                        | done (CST walk)     | null/bool inline, NUMBER/STRING leaf parser
//   variable / reference         | done (CST walk)     | `walk_path_tokens` w/ builtin-name → Type promotion
//   spread (atomic position)     | done (CST walk)     | `lower_spread_expr_v2`
//   list + comprehension         | done (CST walk)     | leading directive/decorator collection
//   dict + dict_field            | done (CST walk)     | typed key, dynamic key, method shorthand,
//                                |                     |   schema-colon rewind, standalone directives
//   binary / unary / ternary     | done (CST walk)     | operator table, lhs/rhs recursion
//   call                         | done (CST walk)     | `walk_call_arg_node` w/ named-args check
//   closure                      | done (CST walk)     | typed params, return type, body
//   variant constructor          | done (CST walk)     | dotted path + DICT body
//   f-string                     | done (CST walk)     | literal-chunk decoder + interpolation walk
//   match / where                | done (CST walk)     | arm pattern/body pairs, where binding dict
//   type expr                    | done (CST walk)     | `lower_type_node_from_cst`
//   directive                    | done (CST walk)     | 5 shapes, schema with-block, name constants
//
// The legacy module web (`expr.rs`, `directive.rs` parse paths,
// `decorator.rs`, `fn_call.rs`, `fmt_string.rs`, `var.rs`,
// `reference_var.rs`, `prim/*`, `structure/*`) is now unreachable
// from `lower.rs`. Outside callers in `relon-evaluator/eval_tests.rs`
// (which uses `parse_expr` directly to drive small-fragment eval
// tests) and `relon-analyzer` / `relon-evaluator` (which import only
// the directive-name constants) keep the legacy parse paths alive
// for now. Full deletion is a separate change once the eval-tests
// migrate to `parse_document`-shaped inputs.

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
pub fn range_from_offsets(source: &str, start: usize, end: usize) -> TokenRange {
    TokenRange {
        start: position_at_source(source, start),
        end: position_at_source(source, end),
    }
}

/// Decode the text of a NUMBER token into the corresponding
/// [`Expr::Int`] / [`Expr::Float`]. Mirrors the byte-identical shape of
/// the legacy `prim::number::parse_number` combinator, but operates on
/// a `&str` so we don't drag the winnow stream / Span machinery into
/// the CST-walking lowering path. Returns `None` when the slice doesn't
/// form a complete numeric literal (hex overflow, malformed exponent,
/// trailing bytes) — the CST grammar is supposed to guarantee a
/// well-formed slice, so this acts as a parity smoke test rather than
/// an expected failure path.
fn parse_number_text(text: &str) -> Option<crate::Expr> {
    use ordered_float::OrderedFloat;
    // Optional leading sign. The CST tokenizer emits the sign as part
    // of the NUMBER token text when it stuck (e.g. `-1`, `+0.5`,
    // `-0x10`), matching the legacy combinator's behaviour.
    let bytes = text.as_bytes();
    let (sign, rest) = match bytes.first() {
        Some(b'+') => (1i64, &text[1..]),
        Some(b'-') => (-1i64, &text[1..]),
        _ => (1i64, text),
    };
    // Hex / oct / bin first — they have explicit prefixes so the
    // dispatch is unambiguous.
    if let Some(hex) = rest.strip_prefix("0x") {
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let v: u64 = u64::from_str_radix(hex, 16).ok()?;
        let signed = if sign >= 0 {
            i64::try_from(v).ok()?
        } else if v > (i64::MAX as u64) + 1 {
            return None;
        } else if v == (i64::MAX as u64) + 1 {
            i64::MIN
        } else {
            -(v as i64)
        };
        return Some(crate::Expr::Int(signed));
    }
    if let Some(oct) = rest.strip_prefix("0o") {
        if oct.is_empty() {
            return None;
        }
        let v: i64 = i64::from_str_radix(oct, 8).ok()?;
        return Some(crate::Expr::Int(v.checked_mul(sign)?));
    }
    if let Some(bin) = rest.strip_prefix("0b") {
        if bin.is_empty() {
            return None;
        }
        let v: i64 = i64::from_str_radix(bin, 2).ok()?;
        return Some(crate::Expr::Int(v.checked_mul(sign)?));
    }
    // Special-named floats — the legacy parser accepted bare
    // `Infinity` / `NaN` as float literals (with optional sign on
    // Infinity). The CST tokenizer surfaces these as NUMBER tokens
    // when they sit in numeric position.
    if rest == "Infinity" {
        return Some(crate::Expr::Float(OrderedFloat(if sign == 1 {
            f64::INFINITY
        } else {
            f64::NEG_INFINITY
        })));
    }
    if rest == "NaN" {
        return Some(crate::Expr::Float(OrderedFloat(f64::NAN)));
    }
    // Decimal integer or float. The presence of `.` / `e` / `E` is
    // the legacy parser's dispatch criterion.
    if rest.contains('.') || rest.contains('e') || rest.contains('E') {
        let f: f64 = rest.parse().ok()?;
        return Some(crate::Expr::Float(OrderedFloat(f * sign as f64)));
    }
    let i: i64 = rest.parse().ok()?;
    Some(crate::Expr::Int(i.checked_mul(sign)?))
}

/// Decode the text of a STRING token (including its surrounding quotes
/// or raw-string `r#"..."#` envelope) into the contained Rust
/// [`String`]. Mirrors the legacy `prim::string::parse_string` /
/// `normal_string` / `raw_string` / `string_content` chain, but operates
/// on a `&str` so the CST-walking lowering path doesn't need to drag
/// in winnow's Span / parser-combinator infrastructure.
///
/// Handles:
/// * Normal double-quoted strings with `\n`, `\r`, `\t`, `\b`, `\f`,
///   `\\`, `\/`, `\"`, `\uXXXX`, `\u{X..}` escapes.
/// * Escaped-whitespace folding: a backslash followed by one or more
///   whitespace chars is consumed silently (the legacy combinator
///   uses this as a line-continuation marker).
/// * Raw strings: `r"..."`, `r#"..."#`, `r##"..."##`, etc. The
///   number of `#`s after `r` is the same number expected before the
///   closing quote; no escapes are processed inside.
///
/// Returns `None` on malformed input — same semantics as the legacy
/// combinator (which would have returned an `Err` from the chain).
fn parse_string_text(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.first().copied() == Some(b'r') {
        return parse_raw_string_text(&text[1..]);
    }
    parse_normal_string_text(text)
}

fn parse_normal_string_text(text: &str) -> Option<String> {
    // Strip opening + closing `"`. The CST grammar guarantees both
    // are present; if not, the token wouldn't have been classified
    // as STRING.
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let escape = chars.next()?;
        match escape {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{08}'),
            'f' => out.push('\u{0C}'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            '"' => out.push('"'),
            'u' => {
                // `\uXXXX` (exactly 4 hex digits) or `\u{X..}` (1..=6).
                let cp = if chars.peek().copied() == Some('{') {
                    chars.next(); // `{`
                    let mut hex = String::new();
                    loop {
                        let h = chars.next()?;
                        if h == '}' {
                            break;
                        }
                        if !h.is_ascii_hexdigit() {
                            return None;
                        }
                        hex.push(h);
                    }
                    if hex.is_empty() || hex.len() > 6 {
                        return None;
                    }
                    u32::from_str_radix(&hex, 16).ok()?
                } else {
                    let mut hex = String::new();
                    for _ in 0..4 {
                        let h = chars.next()?;
                        if !h.is_ascii_hexdigit() {
                            return None;
                        }
                        hex.push(h);
                    }
                    u32::from_str_radix(&hex, 16).ok()?
                };
                out.push(std::char::from_u32(cp)?);
            }
            w if w.is_whitespace() => {
                // Escaped-whitespace continuation: silently consume
                // any additional whitespace chars that follow.
                while let Some(&peek) = chars.peek() {
                    if peek.is_whitespace() {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

fn parse_raw_string_text(after_r: &str) -> Option<String> {
    // Count the leading `#`s before the opening `"`. Same number is
    // expected before the closing `"`.
    let mut hash_count = 0usize;
    let bytes = after_r.as_bytes();
    while bytes.get(hash_count).copied() == Some(b'#') {
        hash_count += 1;
    }
    let rest = &after_r[hash_count..];
    let inner = rest.strip_prefix('"')?;
    let mut closing = String::from("\"");
    for _ in 0..hash_count {
        closing.push('#');
    }
    let content = inner.strip_suffix(closing.as_str())?;
    Some(content.to_string())
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
            "Infinity" => Some(Node::new(
                Expr::Float(ordered_float::OrderedFloat(f64::INFINITY)),
                range,
            )),
            "NaN" => Some(Node::new(
                Expr::Float(ordered_float::OrderedFloat(f64::NAN)),
                range,
            )),
            _ => None,
        },
        SyntaxKind::NUMBER => {
            // Numbers carry hex / oct / bin / scientific / Infinity /
            // NaN parsing. P6 round 2 inlined the leaf parser here as
            // a direct `&str` decoder so the prim/ module web can be
            // deleted later in this round.
            let slice = source.get(start..end)?;
            let expr = parse_number_text(slice)?;
            Some(Node::new(expr, range))
        }
        SyntaxKind::STRING => {
            // Strings own escape decoding (`\n`, `\u{...}`, raw
            // `r#"..."#`). P6 round 2 inlined the leaf parser here.
            let slice = source.get(start..end)?;
            let s = parse_string_text(slice)?;
            Some(Node::new(Expr::String(s), range))
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
    // Legacy `parse_type_expr` upgrades a single-segment builtin-name
    // path (`Int` / `String` / `Bool` / ... / `Enum`) to `Expr::Type`
    // unconditionally — the CST emits VARIABLE_EXPR for the bare-
    // bareword form (no generics, no `?`) but the analyzer / evaluator
    // expect the Type shape so the rest of the pipeline can flow.
    if path.len() == 1 {
        if let TokenKey::String(name, name_range, false) = &path[0] {
            if matches!(
                name.as_str(),
                "Int" | "String" | "Bool" | "Any" | "Null" | "List" | "Dict" | "Enum"
            ) {
                let t = crate::TypeNode {
                    path: vec![name.clone()],
                    generics: Vec::new(),
                    is_optional: false,
                    range: *name_range,
                    variant_fields: None,
                    doc_comment: None,
                };
                return Some(Node::new(
                    Expr::Type(t),
                    range_from_offsets(source, start, end),
                ));
            }
        }
    }
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
fn walk_path_tokens(node: &SyntaxNode, source: &str, is_reference: bool) -> Option<Vec<TokenKey>> {
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
                SyntaxKind::WHITESPACE | SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT => {
                    continue
                }
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
/// walking the rowan node directly. Each shape (Bare / Value /
/// NameBody / Import / Main) reads its structural pieces off the CST
/// children — body expressions go through [`lower_expr_v2`], type
/// nodes go through [`lower_type_node_from_cst`], with-block schema
/// methods walk their own [`SCHEMA_METHOD`] / [`SCHEMA_WITH`] CST
/// children.
///
/// P6 round 2: replaced the byte-slice route into
/// `directive::parse_directive`. The CST now drives every shape
/// directly so the legacy `directive.rs` module web can retire.
#[allow(dead_code)]
fn lower_directive_v2(dir: &ast::Directive, source: &str) -> Option<crate::Directive> {
    let node = dir.syntax();
    let r = node.text_range();
    let raw_start: usize = r.start().into();
    let end: usize = r.end().into();
    let raw_slice = source.get(raw_start..end)?;
    let trim = trim_leading_trivia(raw_slice);
    let start = raw_start + trim;

    // Directive name — the first IDENT after the `#` sigil.
    let name = dir.name()?;
    let shape = crate::directive::directive_shape(&name)?;

    let body = match shape {
        crate::DirectiveShape::Bare => crate::DirectiveBody::Bare,
        crate::DirectiveShape::Value => lower_directive_value_body(node, source)?,
        crate::DirectiveShape::NameBody => lower_directive_name_body(node, source)?,
        crate::DirectiveShape::Import => lower_directive_import_body(node, source)?,
        crate::DirectiveShape::Main => lower_directive_main_body(node, source)?,
    };

    // The directive's outer range matches the legacy combinator's
    // `start_offset..end_offset` shape: from `#` (after leading
    // trivia) to the input position the legacy parser stopped at.
    // For most shapes that's the end of the parsed body; for Bare we
    // stop right after the directive name. The CST node already
    // covers exactly that span when the legacy parser would have,
    // *except* for trailing trivia inside the node — the legacy
    // parser would have left that for the surrounding grammar. We
    // need to trim trailing trivia from the directive's range so the
    // ranges line up byte-for-byte with the legacy `Directive.range`.
    let end_trimmed = directive_end_offset(node, &name, shape, end, source);
    Some(crate::Directive {
        name,
        body,
        range: range_from_offsets(source, start, end_trimmed),
    })
}

/// Walk a DIRECTIVE node for the trailing offset the legacy combinator
/// chain would have reported. The CST node spans every byte of the
/// directive *including* trailing trivia inside its range; the legacy
/// `directive` combinator returns `end_offset = input.location()`
/// *after* its last child production, which excludes trailing
/// whitespace / comments.
///
/// For Bare directives the legacy parser stops right after the name
/// IDENT — we walk to find that IDENT's end.
///
/// For other shapes we find the last "real" child (the body expr, the
/// `with` block, the path STRING, the return TYPE_NODE, or the `)`
/// token closing the main param list) and use its end.
fn directive_end_offset(
    node: &SyntaxNode,
    name: &str,
    shape: crate::DirectiveShape,
    cst_end: usize,
    source: &str,
) -> usize {
    let mut last_significant: Option<usize> = None;
    for el in node.children_with_tokens() {
        let kind = match &el {
            rowan::NodeOrToken::Token(t) => t.kind(),
            rowan::NodeOrToken::Node(n) => n.kind(),
        };
        let range = match &el {
            rowan::NodeOrToken::Token(t) => t.text_range(),
            rowan::NodeOrToken::Node(n) => n.text_range(),
        };
        // Skip pure trivia.
        if matches!(
            kind,
            SyntaxKind::WHITESPACE | SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT
        ) {
            continue;
        }
        let end: usize = range.end().into();
        // Bare directives: stop right after the name IDENT.
        if matches!(shape, crate::DirectiveShape::Bare)
            && kind == SyntaxKind::IDENT
            && last_significant.is_none()
        {
            // Skip the directive's own name IDENT, but no — the
            // `#name` form has `#` token first, then IDENT (the
            // name), and nothing else. We track the IDENT end.
            last_significant = Some(end);
            continue;
        }
        last_significant = Some(end);
    }
    let _ = (name, cst_end, source); // suppress unused-warning lint hints
    last_significant.unwrap_or(cst_end)
}

/// Walk the body expression of a `Value`-shape directive.
fn lower_directive_value_body(node: &SyntaxNode, source: &str) -> Option<crate::DirectiveBody> {
    let body_expr = node.children().find_map(ast::Expr::cast)?;
    let body_node = lower_expr_v2(&body_expr, source)?;
    Some(crate::DirectiveBody::Value(Box::new(body_node)))
}

/// Walk the body of an `Import`-shape directive: spec + `from` + path
/// + optional integrity pin `<algo>:"<hex>"`.
fn lower_directive_import_body(node: &SyntaxNode, source: &str) -> Option<crate::DirectiveBody> {
    // Children we care about (in source order, after the `#import`
    // tokens): STAR / L_BRACE-block / IDENT (spec), then the `from`
    // IDENT, then the path STRING token, then optionally
    // `<algo>:"<hex>"` integrity pin.
    let mut after_hash_name = false; // have we passed the `#import` name?
    let mut spec: Option<crate::DirectiveImportSpec> = None;
    let mut path_string: Option<(String, TokenRange)> = None;
    let mut destructure_open = false;
    let mut destructure_entries: Vec<(String, Option<String>)> = Vec::new();
    let mut pending_name: Option<String> = None;
    let mut expect_as_alias = false;

    // Integrity-pin tracking. The pin's source form is `<ident>:"<hex>"`
    // and may follow the path STRING. We walk through the three pieces
    // (`integrity_algo` IDENT → `:` COLON → STRING) keeping a small
    // state machine so the cluttered overall traversal does not need
    // to grow a second pass.
    let mut integrity_algo: Option<(String, usize)> = None; // (text, start offset)
    let mut integrity_saw_colon = false;
    let mut integrity: Option<crate::IntegrityHash> = None;

    for el in node.children_with_tokens() {
        match el {
            rowan::NodeOrToken::Token(t) => {
                match t.kind() {
                    SyntaxKind::WHITESPACE
                    | SyntaxKind::LINE_COMMENT
                    | SyntaxKind::BLOCK_COMMENT
                    | SyntaxKind::HASH => continue,
                    SyntaxKind::IDENT => {
                        let text = t.text();
                        if !after_hash_name {
                            // The `import` name itself.
                            if text == "import" {
                                after_hash_name = true;
                            }
                            continue;
                        }
                        if destructure_open {
                            if expect_as_alias {
                                if let Some(prev) = pending_name.take() {
                                    destructure_entries.push((prev, Some(text.to_string())));
                                }
                                expect_as_alias = false;
                                continue;
                            }
                            if text == "as" {
                                expect_as_alias = true;
                                continue;
                            }
                            if let Some(prev) = pending_name.take() {
                                destructure_entries.push((prev, None));
                            }
                            pending_name = Some(text.to_string());
                            continue;
                        }
                        if text == "from" {
                            // After `from` we expect the path STRING.
                            continue;
                        }
                        // After the path STRING the only well-formed
                        // IDENT is the integrity-pin algorithm name.
                        if path_string.is_some() && integrity_algo.is_none() {
                            let start: usize = t.text_range().start().into();
                            integrity_algo = Some((text.to_string(), start));
                            continue;
                        }
                        // Alias-shape spec: the bare IDENT is the alias.
                        if spec.is_none() {
                            spec = Some(crate::DirectiveImportSpec::Alias(text.to_string()));
                        }
                    }
                    SyntaxKind::STAR => {
                        if spec.is_none() {
                            spec = Some(crate::DirectiveImportSpec::Spread);
                        }
                    }
                    SyntaxKind::L_BRACE => {
                        destructure_open = true;
                    }
                    SyntaxKind::R_BRACE => {
                        if let Some(prev) = pending_name.take() {
                            destructure_entries.push((prev, None));
                        }
                        if destructure_open {
                            spec = Some(crate::DirectiveImportSpec::Destructure(
                                destructure_entries.clone(),
                            ));
                            destructure_open = false;
                        }
                    }
                    SyntaxKind::COMMA => {
                        if destructure_open {
                            if let Some(prev) = pending_name.take() {
                                destructure_entries.push((prev, None));
                            }
                        }
                    }
                    SyntaxKind::COLON => {
                        if integrity_algo.is_some() && !integrity_saw_colon {
                            integrity_saw_colon = true;
                        }
                    }
                    SyntaxKind::STRING => {
                        let tr = t.text_range();
                        let s: usize = tr.start().into();
                        let e: usize = tr.end().into();
                        let raw = source.get(s..e)?;
                        let decoded = parse_string_text(raw)?;
                        if path_string.is_none() {
                            path_string = Some((decoded, range_from_offsets(source, s, e)));
                        } else if integrity_algo.is_some() && integrity_saw_colon {
                            // Found the hex STRING that completes the
                            // integrity pin. Defer algorithm validation
                            // to the analyzer so the diagnostic carries
                            // a real span; here we only stash what the
                            // source provided.
                            let (algo_text, algo_start) = integrity_algo.take().unwrap();
                            let algo = crate::HashAlgorithm::from_ident(&algo_text);
                            integrity = Some(crate::IntegrityHash {
                                algorithm: algo,
                                algorithm_text: algo_text,
                                hex: decoded,
                                range: range_from_offsets(source, algo_start, e),
                            });
                            integrity_saw_colon = false;
                        }
                    }
                    _ => continue,
                }
            }
            rowan::NodeOrToken::Node(_) => continue,
        }
    }

    let spec = spec?;
    let (path, path_range) = path_string?;
    Some(crate::DirectiveBody::Import {
        spec,
        path,
        path_range,
        integrity,
    })
}

/// Walk the body of a `Main`-shape directive: `(typed-params) [-> Ret]`.
fn lower_directive_main_body(node: &SyntaxNode, source: &str) -> Option<crate::DirectiveBody> {
    let mut params: Vec<crate::DirectiveMainParam> = Vec::new();
    let mut return_type: Option<crate::TypeNode> = None;
    // The CST emits CLOSURE_PARAM children for each `Type ident` pair.
    // The optional return TYPE_NODE follows after `THIN_ARROW`.
    let mut saw_arrow = false;
    for child in node.children_with_tokens() {
        match child {
            rowan::NodeOrToken::Token(t) => {
                if t.kind() == SyntaxKind::THIN_ARROW {
                    saw_arrow = true;
                }
            }
            rowan::NodeOrToken::Node(n) => match n.kind() {
                SyntaxKind::CLOSURE_PARAM => {
                    params.push(lower_main_param(&n, source)?);
                }
                SyntaxKind::TYPE_NODE if saw_arrow && return_type.is_none() => {
                    return_type = Some(lower_type_node_from_cst(&n, source)?);
                }
                _ => continue,
            },
        }
    }
    Some(crate::DirectiveBody::Main {
        params,
        return_type,
    })
}

/// One `#main` parameter — `Type ident` (closure-param shape, but with
/// the type *before* the name like a typed-spread).
fn lower_main_param(node: &SyntaxNode, source: &str) -> Option<crate::DirectiveMainParam> {
    // CLOSURE_PARAM children: TYPE_NODE + IDENT.
    let type_node = node
        .children()
        .find(|c| c.kind() == SyntaxKind::TYPE_NODE)
        .and_then(|n| lower_type_node_from_cst(&n, source))?;
    let ident_tok = node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| t.kind() == SyntaxKind::IDENT)?;
    let tr = ident_tok.text_range();
    let s: usize = tr.start().into();
    let e: usize = tr.end().into();
    Some(crate::DirectiveMainParam {
        name: ident_tok.text().to_string(),
        name_range: range_from_offsets(source, s, e),
        type_node,
    })
}

/// Either a simple-IDENT key or a typed-key (Dynamic(Type)) emitted
/// for the schema-colon directive split.
enum SchemaColonKey {
    /// Simple IDENT key — `#schema Image: { ... }` → key=`Image`.
    SimpleIdent(crate::syntax::SyntaxToken),
    /// Typed key with generics — `#schema Page<T>: { ... }` →
    /// key=Dynamic(Type(TypeNode { path: ["Page"], generics: [T] })).
    TypedDynamic(crate::TypeNode),
}

/// Result of splitting a schema-colon directive into its constituent
/// dict-field pieces.
struct SchemaColonSplit {
    directive: crate::Directive,
    key: SchemaColonKey,
    value: ast::Expr,
}

/// Detect the schema-colon dict-field shape `#schema Image: { ... }`
/// (or `#schema Page<T>: { ... }`) and split it into a Bare directive
/// + a separate `Image: { ... }` dict-field. Returns `Some(...)` when
///   the directive has this shape (a NameBody directive whose CST
///   tokens contain a COLON between the declared name IDENT and the
///   body Expr, with no `with { ... }` block). Returns `None` for any
///   other directive shape — caller proceeds with the regular
///   directive walker.
fn split_schema_colon_directive(node: &SyntaxNode, source: &str) -> Option<SchemaColonSplit> {
    // Quick filter: only `#schema` / `#extend` (the NameBody shapes)
    // can take this form. Read the directive name.
    let dir_name = node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| t.kind() == SyntaxKind::IDENT)
        .map(|t| t.text().to_string())?;
    let shape = crate::directive::directive_shape(&dir_name)?;
    if !matches!(shape, crate::DirectiveShape::NameBody) {
        return None;
    }
    // Walk children to find: directive name IDENT (already seen),
    // declared name IDENT, optional `<T, U>` generic params, COLON
    // token, body Expr.
    let mut after_dir_name = false;
    let mut declared_name_tok: Option<crate::syntax::SyntaxToken> = None;
    let mut saw_colon = false;
    let mut body_expr: Option<ast::Expr> = None;
    let mut saw_schema_with = false;
    let mut in_generics = false;
    let mut generics: Vec<String> = Vec::new();
    let mut generics_lt_offset: Option<usize> = None;
    let mut generics_gt_end: Option<usize> = None;
    for el in node.children_with_tokens() {
        match el {
            rowan::NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::WHITESPACE
                | SyntaxKind::LINE_COMMENT
                | SyntaxKind::BLOCK_COMMENT
                | SyntaxKind::HASH => continue,
                SyntaxKind::IDENT => {
                    if !after_dir_name {
                        after_dir_name = true;
                        continue;
                    }
                    if declared_name_tok.is_none() {
                        declared_name_tok = Some(t);
                        continue;
                    }
                    if in_generics {
                        generics.push(t.text().to_string());
                    }
                }
                SyntaxKind::LT => {
                    in_generics = true;
                    generics_lt_offset = Some(t.text_range().start().into());
                }
                SyntaxKind::GT => {
                    in_generics = false;
                    generics_gt_end = Some(t.text_range().end().into());
                }
                SyntaxKind::COLON if declared_name_tok.is_some() && body_expr.is_none() => {
                    saw_colon = true;
                }
                _ => {}
            },
            rowan::NodeOrToken::Node(n) => {
                if n.kind() == SyntaxKind::SCHEMA_WITH {
                    saw_schema_with = true;
                } else if let Some(e) = ast::Expr::cast(n.clone()) {
                    if body_expr.is_none() {
                        body_expr = Some(e);
                    }
                }
            }
        }
    }
    if !saw_colon || saw_schema_with {
        return None;
    }
    let key_token = declared_name_tok?;
    let body = body_expr?;
    // Build the Bare directive. Its range ends at the *start* of
    // the declared name IDENT — the legacy `parse_name_body` resets
    // to `after_ws` (the input position right after the initial
    // `soc0`, before `Image` is consumed) when it sees the `:`, so
    // the directive's `end_offset = input.location()` lands on the
    // first byte of the name IDENT.
    let raw_start: usize = node.text_range().start().into();
    let key_start: usize = key_token.text_range().start().into();
    let raw_slice = source.get(raw_start..key_start)?;
    let trim = trim_leading_trivia(raw_slice);
    let dir_start = raw_start + trim;
    let dir_end = key_start;
    let bare = crate::Directive {
        name: dir_name,
        body: crate::DirectiveBody::Bare,
        range: range_from_offsets(source, dir_start, dir_end),
    };
    let key = if !generics.is_empty() {
        // Construct a TypeNode mirroring the legacy `parse_type_node`
        // shape: path = [declared_name], generics = each generic name
        // as a single-segment TypeNode. Range covers `Name<T,...>`.
        let key_end = generics_gt_end.unwrap_or_else(|| key_token.text_range().end().into());
        let name_range = range_from_offsets(source, key_start, key_token.text_range().end().into());
        let mut g_nodes: Vec<crate::TypeNode> = Vec::new();
        // Generics' ranges aren't structurally critical for the
        // analyzer (the names match), so we approximate with the
        // declared name range — refining requires walking the inner
        // tokens. The downstream tests don't compare these ranges
        // structurally.
        for g_name in generics {
            g_nodes.push(crate::TypeNode {
                path: vec![g_name],
                generics: Vec::new(),
                is_optional: false,
                range: name_range,
                variant_fields: None,
                doc_comment: None,
            });
        }
        SchemaColonKey::TypedDynamic(crate::TypeNode {
            path: vec![key_token.text().to_string()],
            generics: g_nodes,
            is_optional: false,
            range: range_from_offsets(source, key_start, key_end),
            variant_fields: None,
            doc_comment: None,
        })
    } else {
        SchemaColonKey::SimpleIdent(key_token)
    };
    let _ = generics_lt_offset;
    Some(SchemaColonSplit {
        directive: bare,
        key,
        value: body,
    })
}

/// Walk the body of a `NameBody`-shape directive: `<name>[<T, ...>]
/// <body-expr> [with { methods... }]`.
fn lower_directive_name_body(node: &SyntaxNode, source: &str) -> Option<crate::DirectiveBody> {
    // Children of the DIRECTIVE node:
    //   `#` HASH, name IDENT, optional generics (LT ... GT), optional
    //   body Expr, optional SCHEMA_WITH node.
    //
    // The name IDENT is the second IDENT child (the first being the
    // directive name itself — `schema` / `extend`). We track state:
    //   * `after_dir_name = false` until we've seen the directive name IDENT
    //   * `seen_decl_name = false` until we've recorded the declared name IDENT
    //   * `in_generics = false` while inside `< ... >`
    let mut after_dir_name = false;
    let mut declared_name: Option<(String, TokenRange)> = None;
    let mut in_generics = false;
    let mut generics: Vec<String> = Vec::new();
    let mut body_expr_ast: Option<ast::Expr> = None;
    let mut schema_with: Option<SyntaxNode> = None;
    // Schema-colon shape: `#schema Image: { ... }` inside a dict has
    // the COLON token directly under DIRECTIVE *between* the declared
    // name and the body. Legacy `parse_name_body` detects this and
    // rewinds to Bare so the surrounding dict-field parser sees the
    // `Image: { ... }` form. We replicate by tracking whether a
    // COLON appears between the declared name and the body.
    let mut saw_colon_after_name = false;

    for el in node.children_with_tokens() {
        match el {
            rowan::NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::WHITESPACE
                | SyntaxKind::LINE_COMMENT
                | SyntaxKind::BLOCK_COMMENT
                | SyntaxKind::HASH
                | SyntaxKind::COMMA => continue,
                SyntaxKind::COLON => {
                    if declared_name.is_some() && body_expr_ast.is_none() {
                        saw_colon_after_name = true;
                    }
                }
                SyntaxKind::IDENT => {
                    if !after_dir_name {
                        after_dir_name = true;
                        continue;
                    }
                    if declared_name.is_none() {
                        let tr = t.text_range();
                        let s: usize = tr.start().into();
                        let e: usize = tr.end().into();
                        declared_name =
                            Some((t.text().to_string(), range_from_offsets(source, s, e)));
                        continue;
                    }
                    if in_generics {
                        generics.push(t.text().to_string());
                        continue;
                    }
                    // Trailing `with` keyword — handled when we hit
                    // the SCHEMA_WITH node child below.
                }
                SyntaxKind::LT => in_generics = true,
                SyntaxKind::GT => in_generics = false,
                _ => continue,
            },
            rowan::NodeOrToken::Node(n) => match n.kind() {
                SyntaxKind::SCHEMA_WITH => schema_with = Some(n),
                _ => {
                    if let Some(e) = ast::Expr::cast(n.clone()) {
                        if body_expr_ast.is_none() {
                            body_expr_ast = Some(e);
                        }
                    }
                }
            },
        }
    }

    // Schema-colon shape: rewind to Bare so the surrounding dict-field
    // parser can consume the `<name>: <value>` form.
    if saw_colon_after_name && schema_with.is_none() {
        return Some(crate::DirectiveBody::Bare);
    }

    // No declared name at all (e.g. `#schema` followed by a `{`-led
    // body with no name) — legacy `parse_name_body` rewinds to
    // `pre_body_checkpoint` and returns Bare. The CST may still have
    // a body Expr child if the body-start guard was satisfied, but
    // legacy bails before consuming it.
    let (name, name_range) = match declared_name {
        Some(n) => n,
        None => return Some(crate::DirectiveBody::Bare),
    };

    // Body — when missing (e.g. `#schema X with { ... }` with no body),
    // synthesize an empty dict at the `with` keyword's position to match
    // the legacy parser's behaviour (which captures
    // `body_range = create_range(input, input.location(), input.location())`
    // after consuming the post-name whitespace but before consuming
    // `with`).
    let body = if let Some(expr) = body_expr_ast {
        Box::new(lower_expr_v2(&expr, source)?)
    } else {
        // Position: at the start of the `with` keyword. The CST emits
        // `with` as a bare IDENT token (not wrapped in any node) and
        // SCHEMA_WITH starts at the following `{`. We find the `with`
        // IDENT by looking for the IDENT-token sibling preceding the
        // SCHEMA_WITH child.
        let pos = if let Some(sw) = &schema_with {
            // Find the IDENT("with") right before the SCHEMA_WITH node.
            let mut last_ident_start: Option<usize> = None;
            for el in node.children_with_tokens() {
                match el {
                    rowan::NodeOrToken::Token(t) => {
                        if t.kind() == SyntaxKind::IDENT && t.text() == "with" {
                            last_ident_start = Some(t.text_range().start().into());
                        }
                    }
                    rowan::NodeOrToken::Node(n) => {
                        if n.kind() == SyntaxKind::SCHEMA_WITH {
                            break;
                        }
                    }
                }
            }
            last_ident_start.unwrap_or_else(|| sw.text_range().start().into())
        } else {
            let r = node.text_range();
            let end: usize = r.end().into();
            end
        };
        Box::new(crate::Node {
            id: crate::NodeId::alloc(),
            expr: std::sync::Arc::new(crate::Expr::Dict(Vec::new())),
            decorators: Vec::new(),
            directives: Vec::new(),
            type_hint: None,
            range: range_from_offsets(source, pos, pos),
            doc_comment: None,
        })
    };

    let (methods, schema_no_auto_derives) = if let Some(sw) = schema_with {
        lower_schema_with(&sw, source)?
    } else {
        (Vec::new(), Vec::new())
    };

    Some(crate::DirectiveBody::NameBody {
        name,
        name_range,
        generics,
        body,
        methods,
        schema_no_auto_derives,
    })
}

/// Walk a SCHEMA_WITH CST node into the `(methods, schema_no_auto_derives)`
/// pair the legacy `DirectiveBody::NameBody` carries.
///
/// The CST groups each method (plus its leading pragmas like `#derive`
/// or `#native`) into a SCHEMA_METHOD node; schema-level pragmas like
/// `#no_auto_derive` that don't precede a method sit as DIRECTIVE
/// children of the SCHEMA_WITH node itself.
fn lower_schema_with(
    node: &SyntaxNode,
    source: &str,
) -> Option<(Vec<crate::SchemaMethod>, Vec<String>)> {
    let mut methods: Vec<crate::SchemaMethod> = Vec::new();
    let mut schema_no_auto_derives: Vec<String> = Vec::new();
    for child in node.children() {
        match child.kind() {
            SyntaxKind::SCHEMA_METHOD => {
                let (method, method_no_auto_derives) = lower_schema_method(&child, source)?;
                schema_no_auto_derives.extend(method_no_auto_derives);
                methods.push(method);
            }
            SyntaxKind::DIRECTIVE => {
                // Schema-level pragmas (typically `#no_auto_derive C`)
                // that didn't attach to a method.
                let name = child
                    .children_with_tokens()
                    .filter_map(|el| el.into_token())
                    .find(|t| t.kind() == SyntaxKind::IDENT)
                    .map(|t| t.text().to_string());
                if name.as_deref() == Some(crate::directive::NO_AUTO_DERIVE) {
                    if let Some(constraint) = directive_constraint_name(&child) {
                        schema_no_auto_derives.push(constraint);
                    } else {
                        return None;
                    }
                }
                // Other stray pragmas at the SCHEMA_WITH level are
                // ignored — they would have been bound to a method by
                // the CST grouping rule.
            }
            _ => continue,
        }
    }
    Some((methods, schema_no_auto_derives))
}

/// Walk a SCHEMA_METHOD CST node into the legacy [`crate::SchemaMethod`]
/// shape plus any inline schema-level `#no_auto_derive` pragmas that
/// landed inside the method's pragma stack (rare, but the legacy parser
/// accepted it — schema-level pragmas in mixed pragma stacks).
fn lower_schema_method(
    node: &SyntaxNode,
    source: &str,
) -> Option<(crate::SchemaMethod, Vec<String>)> {
    let method_start = method_name_offset(node);
    let method_end: usize = node.text_range().end().into();

    let mut derives: Vec<String> = Vec::new();
    let mut schema_no_auto_derives: Vec<String> = Vec::new();
    let mut is_native = false;
    let mut is_private = false;
    let mut name: Option<(String, TokenRange)> = None;
    let mut method_generics: Vec<String> = Vec::new();
    let mut params: Vec<crate::SchemaMethodParam> = Vec::new();
    let mut return_type: Option<crate::TypeNode> = None;
    let mut body: Option<Box<crate::Node>> = None;
    let mut saw_arrow = false;
    let mut in_generics = false;
    let mut after_body_colon = false;
    let mut body_after_colon_ast: Option<ast::Expr> = None;

    for el in node.children_with_tokens() {
        match el {
            rowan::NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::WHITESPACE
                | SyntaxKind::LINE_COMMENT
                | SyntaxKind::BLOCK_COMMENT
                | SyntaxKind::L_PAREN
                | SyntaxKind::R_PAREN
                | SyntaxKind::COMMA => continue,
                SyntaxKind::IDENT => {
                    if name.is_none() {
                        let tr = t.text_range();
                        let s: usize = tr.start().into();
                        let e: usize = tr.end().into();
                        name = Some((t.text().to_string(), range_from_offsets(source, s, e)));
                        continue;
                    }
                    if in_generics {
                        method_generics.push(t.text().to_string());
                    }
                }
                SyntaxKind::LT => in_generics = true,
                SyntaxKind::GT => in_generics = false,
                SyntaxKind::THIN_ARROW => saw_arrow = true,
                SyntaxKind::COLON => after_body_colon = true,
                _ => continue,
            },
            rowan::NodeOrToken::Node(n) => match n.kind() {
                SyntaxKind::DIRECTIVE => {
                    // Pragma: `#derive C`, `#native`, `#internal`, or
                    // `#no_auto_derive C` (which is schema-level).
                    let dname = n
                        .children_with_tokens()
                        .filter_map(|el| el.into_token())
                        .find(|t| t.kind() == SyntaxKind::IDENT)
                        .map(|t| t.text().to_string());
                    match dname.as_deref() {
                        Some(crate::directive::DERIVE) => {
                            derives.push(directive_constraint_name(&n)?);
                        }
                        Some(crate::directive::NATIVE) => is_native = true,
                        Some(crate::directive::INTERNAL) => is_private = true,
                        Some(crate::directive::NO_AUTO_DERIVE) => {
                            schema_no_auto_derives.push(directive_constraint_name(&n)?);
                        }
                        _ => return None,
                    }
                }
                SyntaxKind::CLOSURE_PARAM => {
                    params.push(lower_schema_method_param(&n, source)?);
                }
                SyntaxKind::TYPE_NODE if saw_arrow && return_type.is_none() => {
                    return_type = Some(lower_type_node_from_cst(&n, source)?);
                }
                _ => {
                    if after_body_colon {
                        if let Some(e) = ast::Expr::cast(n.clone()) {
                            if body_after_colon_ast.is_none() {
                                body_after_colon_ast = Some(e);
                            }
                        }
                    }
                }
            },
        }
    }

    if let Some(e) = body_after_colon_ast {
        body = Some(Box::new(lower_expr_v2(&e, source)?));
    }

    // Legacy parser enforced two shape rules:
    //   * Non-native methods must have a body (else surface as parse
    //     error).
    //   * `#native` methods must NOT have a body — the host owns the
    //     implementation.
    if is_native && body.is_some() {
        return None;
    }
    if !is_native && body.is_none() {
        return None;
    }

    let (name, name_range) = name?;
    let return_type = return_type?;
    Some((
        crate::SchemaMethod {
            name,
            name_range,
            generics: method_generics,
            params,
            return_type,
            body,
            derives,
            is_native,
            is_private,
            range: range_from_offsets(source, method_start, method_end),
            doc_comment: None,
        },
        schema_no_auto_derives,
    ))
}

/// Locate the byte offset of the method-name IDENT inside a
/// SCHEMA_METHOD node. Mirrors the legacy parser's `method_start`:
/// skip leading pragma directives (`#derive` / `#native` / `#internal`)
/// and their surrounding trivia, then return the first IDENT's start.
/// Falls back to the SCHEMA_METHOD node's own start for shapes that
/// don't carry an IDENT (parser-recovery cases).
fn method_name_offset(node: &SyntaxNode) -> usize {
    for el in node.children_with_tokens() {
        match el {
            rowan::NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::WHITESPACE | SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT => {
                    continue
                }
                SyntaxKind::IDENT => return t.text_range().start().into(),
                _ => continue,
            },
            rowan::NodeOrToken::Node(n) => {
                if n.kind() == SyntaxKind::DIRECTIVE {
                    continue;
                }
                // Non-DIRECTIVE node at the start would be unusual — the
                // CST shape places the method-name IDENT first.
                break;
            }
        }
    }
    node.text_range().start().into()
}

/// One `name: Type` parameter inside a SCHEMA_METHOD's param list.
fn lower_schema_method_param(node: &SyntaxNode, source: &str) -> Option<crate::SchemaMethodParam> {
    let name_tok = node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| t.kind() == SyntaxKind::IDENT)?;
    let tr = name_tok.text_range();
    let s: usize = tr.start().into();
    let e: usize = tr.end().into();
    let type_node = node
        .children()
        .find(|c| c.kind() == SyntaxKind::TYPE_NODE)
        .and_then(|n| lower_type_node_from_cst(&n, source))?;
    Some(crate::SchemaMethodParam {
        name: name_tok.text().to_string(),
        name_range: range_from_offsets(source, s, e),
        type_node,
    })
}

/// Extract the single-segment constraint name from a `#derive` /
/// `#no_auto_derive` directive's body. The body is a `Value` shape
/// holding a `Variable` whose path is one IDENT.
fn directive_constraint_name(node: &SyntaxNode) -> Option<String> {
    // Find the body expression child and read its IDENT text.
    let body_expr = node.children().find_map(ast::Expr::cast)?;
    if let ast::Expr::Variable(v) = body_expr {
        let segs = v.segments();
        if segs.len() == 1 {
            return Some(segs.into_iter().next().unwrap());
        }
    }
    None
}

/// Lower a CST TYPE_NODE (or TUPLE_TYPE) into the legacy
/// [`crate::TypeNode`] shape. Walks the CST directly — no byte-slice
/// re-parse — so we can retire `expr::parse_type_node` along with the
/// rest of the legacy combinator web.
///
/// Handles:
///  * Bare type heads: `Int`, `String`, ...
///  * Dotted paths: `geo.Location`, `"namespaced".Foo`
///  * Generics: `List<Int>`, `Dict<String, Int>`, nested
///    `List<Dict<String, Int>>`
///  * Optional `?` suffix
///  * Tuple types: `()`, `(T,)`, `(T1, T2)`
///  * `Enum<Variant, Other { field: T }>` — emitting variant_fields
///    on the variant alternative TypeNodes.
fn lower_type_node_from_cst(node: &SyntaxNode, source: &str) -> Option<crate::TypeNode> {
    let r = node.text_range();
    // The legacy `parse_type_node` starts after `parse_leading_comments`,
    // so its `start_offset` skips leading whitespace/comments. The CST
    // node may include those leading bytes via rowan's open()
    // flush-trivia rule. Anchor to the first non-trivia child.
    let start: usize = first_non_trivia_offset(node).unwrap_or_else(|| r.start().into());
    let end: usize = r.end().into();

    if node.kind() == SyntaxKind::TUPLE_TYPE {
        // `(T1, T2, ...)`. Children: optional TYPE_NODE elements,
        // optional trailing `?`.
        let mut elems: Vec<crate::TypeNode> = Vec::new();
        for child in node.children() {
            if let Some(t) = lower_type_node_from_cst(&child, source) {
                elems.push(t);
            }
        }
        let is_optional = node
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .any(|t| t.kind() == SyntaxKind::QUESTION);
        return Some(crate::TypeNode {
            path: vec!["Tuple".to_string()],
            generics: elems,
            is_optional,
            range: range_from_offsets(source, start, end),
            variant_fields: None,
            doc_comment: None,
        });
    }
    if node.kind() != SyntaxKind::TYPE_NODE {
        return None;
    }
    // Path: leading IDENT/STRING tokens, separated by DOT, until we
    // hit `<` (generics), `?`, or end.
    let mut path: Vec<String> = Vec::new();
    let mut in_generics = false;
    let mut is_optional = false;
    let mut after_path = false;
    let mut generics: Vec<crate::TypeNode> = Vec::new();
    let mut is_enum_head = false;

    for el in node.children_with_tokens() {
        match el {
            rowan::NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::WHITESPACE | SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT => {
                    continue
                }
                SyntaxKind::DOT => continue,
                SyntaxKind::IDENT | SyntaxKind::STRING if !after_path => {
                    let txt = if t.kind() == SyntaxKind::STRING {
                        parse_string_text(t.text())?
                    } else {
                        t.text().to_string()
                    };
                    path.push(txt);
                }
                SyntaxKind::LT => {
                    after_path = true;
                    in_generics = true;
                    if path.len() == 1 && path[0] == "Enum" {
                        is_enum_head = true;
                    }
                }
                SyntaxKind::GT => {
                    in_generics = false;
                }
                SyntaxKind::QUESTION => {
                    is_optional = true;
                }
                _ => continue,
            },
            rowan::NodeOrToken::Node(n) => {
                if in_generics && matches!(n.kind(), SyntaxKind::TYPE_NODE | SyntaxKind::TUPLE_TYPE)
                {
                    let mut g = lower_type_node_from_cst(&n, source)?;
                    if is_enum_head {
                        attach_enum_variant_fields(&mut g, &n, source);
                    }
                    generics.push(g);
                } else if !in_generics && n.kind() == SyntaxKind::DICT {
                    // This shouldn't happen for a regular TYPE_NODE —
                    // DICT children are inside Enum variant alternatives,
                    // not directly under TYPE_NODE.
                    continue;
                }
            }
        }
    }
    // For Enum heads without any variant-struct alternative, clear
    // the tentative unit-variant markers so the rest of the pipeline
    // treats this as a classic untagged enum.
    if is_enum_head {
        // Bare unit-variants stay marked (legacy `parse_enum_alternative`
        // sets `variant_fields = Some(vec![])` for them). If no
        // generic actually carries a struct body, clear the
        // tentative markers so downstream treats this as a classic
        // untagged enum — matching the legacy fallthrough behaviour.
        let any_struct_form = generics
            .iter()
            .any(|g| g.variant_fields.as_ref().is_some_and(|f| !f.is_empty()));
        if !any_struct_form {
            for g in &mut generics {
                g.variant_fields = None;
            }
        }
    }

    if path.is_empty() {
        return None;
    }
    Some(crate::TypeNode {
        path,
        generics,
        is_optional,
        range: range_from_offsets(source, start, end),
        variant_fields: None,
        doc_comment: None,
    })
}

/// Apply the enum-variant-struct detection rules to a single generic
/// argument under an `Enum<...>` head. Mutates `g.variant_fields` in
/// place when the generic is a unit variant (bare IDENT) or a
/// struct-bodied variant (`Email { field: T }`); also extends `g.range`
/// to cover the trailing `{...}` body so downstream consumers see the
/// full source span. Extracted from `lower_type_node_from_cst` to keep
/// that function's main flow at one nesting level.
fn attach_enum_variant_fields(g: &mut crate::TypeNode, n: &SyntaxNode, source: &str) {
    // Bare single-segment IDENT-headed Enum alternative is a unit
    // variant. Legacy `parse_enum_alternative` calls `id` (IDENT-only)
    // first; if that fails (e.g. for STRING-headed `"hot"`) it falls
    // through to `parse_type_node` and never marks it as a variant.
    // We replicate by checking the first non-trivia token of `n`.
    let first_tok = n.children_with_tokens().find_map(|el| {
        el.into_token().filter(|t| {
            !matches!(
                t.kind(),
                SyntaxKind::WHITESPACE | SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT
            )
        })
    });
    let ident_headed = first_tok
        .map(|t| t.kind() == SyntaxKind::IDENT)
        .unwrap_or(false);
    if ident_headed
        && g.path.len() == 1
        && g.generics.is_empty()
        && !g.is_optional
        && g.variant_fields.is_none()
    {
        g.variant_fields = Some(Vec::new());
    }
    let Some(next) = n.next_sibling() else {
        return;
    };
    if next.kind() != SyntaxKind::DICT {
        return;
    }
    // Extend the variant TypeNode's range to cover the body `{...}`
    // — legacy `parse_enum_alternative` captures
    // `range = start_offset..end_offset` where end_offset is after `}`.
    let dict_end: usize = next.text_range().end().into();
    g.range = range_from_offsets(source, g.range.start.offset, dict_end);
    let mut fields: Vec<(String, crate::TypeNode)> = Vec::new();
    for f in next
        .children()
        .filter(|c| c.kind() == SyntaxKind::DICT_FIELD)
    {
        let name = f
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string());
        let ty = f.children().find_map(|c| match c.kind() {
            SyntaxKind::TYPE_NODE | SyntaxKind::TUPLE_TYPE => lower_type_node_from_cst(&c, source),
            SyntaxKind::VARIABLE_EXPR => {
                // Legacy `parse_variant_field` always calls
                // `parse_type_node`; the CST sometimes emits a
                // VARIABLE_EXPR for bare identifiers (`T`). Mirror
                // by building a TypeNode from the IDENT-only path.
                let segs: Vec<String> = c
                    .children_with_tokens()
                    .filter_map(|el| el.into_token())
                    .filter(|t| t.kind() == SyntaxKind::IDENT)
                    .map(|t| t.text().to_string())
                    .collect();
                if segs.is_empty() {
                    return None;
                }
                let r = c.text_range();
                let s: usize = r.start().into();
                let e: usize = r.end().into();
                Some(crate::TypeNode {
                    path: segs,
                    generics: Vec::new(),
                    is_optional: false,
                    range: range_from_offsets(source, s, e),
                    variant_fields: None,
                    doc_comment: None,
                })
            }
            _ => None,
        });
        if let (Some(name), Some(ty)) = (name, ty) {
            fields.push((name, ty));
        }
    }
    g.variant_fields = Some(fields);
}

/// Find the offset of the first non-trivia child element (token or
/// node) of `node`. Used to anchor ranges to where the legacy
/// combinator started consuming (after `soc0` / `parse_leading_comments`).
fn first_non_trivia_offset(node: &SyntaxNode) -> Option<usize> {
    for el in node.children_with_tokens() {
        match el {
            rowan::NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::WHITESPACE | SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT => {
                    continue
                }
                _ => return Some(t.text_range().start().into()),
            },
            rowan::NodeOrToken::Node(n) => return Some(n.text_range().start().into()),
        }
    }
    None
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

/// Lower an `Expr::Spread` CST node directly. The shape is `... inner`
/// (or `...<TypeHint> inner` for the typed-spread form). The typed
/// spread's `type_hint` stamps onto the inner Node's `type_hint`
/// field rather than the Spread wrapper itself — that's where the
/// legacy `parse_dict_entry` puts it so type-checked spreads inherit
/// the dict-typed-key flow.
fn lower_spread_expr_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();
    // Optional `<Type>` between `...` and the inner expression.
    let mut type_hint: Option<crate::TypeNode> = None;
    let mut inner_expr: Option<ast::Expr> = None;
    for child in node.children() {
        if child.kind() == SyntaxKind::TYPE_NODE && type_hint.is_none() && inner_expr.is_none() {
            type_hint = Some(lower_type_node_from_cst(&child, source)?);
            continue;
        }
        if let Some(e) = ast::Expr::cast(child) {
            inner_expr = Some(e);
            break;
        }
    }
    let inner_ast = inner_expr?;
    let mut inner = lower_expr_v2(&inner_ast, source)?;
    if let Some(th) = type_hint {
        inner.type_hint = Some(th);
    }
    Some(Node::new(
        Expr::Spread(inner),
        range_from_offsets(source, start, end),
    ))
}

/// Lower a `TYPE_NODE` CST node sitting at expression position into
/// the legacy `Expr::Type` wrapper. The CST emits `TYPE_NODE` directly
/// (no surrounding marker) when the type-expr disambiguation in
/// `parse_atomic` claims it as a type rather than a variable.
///
/// The CST node's `text_range()` may include leading trivia (rowan's
/// `open()` flushes pending trivia *into* the new node — see
/// `cst::open`). The legacy `parse_type_expr` captures
/// `start_offset = input.location()` *after* `soc0` is consumed by
/// `parse_atomic`, so the outer Node range should start at the first
/// non-trivia byte. The inner `TypeNode.range` already carries that
/// post-trivia offset (it's set inside `parse_type_node` after
/// `parse_leading_comments`), so reuse it for the outer wrapper.
fn lower_type_expr_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let t = lower_type_node_from_cst(node, source)?;
    let range = t.range;
    Some(Node::new(Expr::Type(t), range))
}

/// Lower a `UNARY_EXPR` CST node. The single operator token (`-`/`!`/
/// `+`) plus the operand expression. Maps to legacy `Expr::Unary(op,
/// operand)`. `+x` collapses to `x` (the legacy parser dropped the
/// sign for unary-`+` since it's a no-op).
fn lower_unary_expr_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();
    let op_token = node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| {
            matches!(
                t.kind(),
                SyntaxKind::MINUS | SyntaxKind::BANG | SyntaxKind::PLUS
            )
        })?;
    let operand_ast = node.children().find_map(ast::Expr::cast)?;
    let operand = lower_expr_v2(&operand_ast, source)?;
    let op = match op_token.kind() {
        SyntaxKind::MINUS => crate::Operator::Sub,
        SyntaxKind::BANG => crate::Operator::Not,
        SyntaxKind::PLUS => {
            // Legacy `parse_unary` only emits `!` and `-` as unary
            // ops. A leading `+` followed by whitespace + a number
            // (`+ 1`) is rejected — the legacy parser treats the `+`
            // as a stray token. Match that by failing the lowering;
            // `lower_document` surfaces this as a typed parse error.
            return None;
        }
        _ => return None,
    };
    Some(Node::new(
        Expr::Unary(op, operand),
        range_from_offsets(source, start, end),
    ))
}

/// Lower a `DICT` CST node into the legacy `Expr::Dict(pairs)` shape.
/// Each DICT_FIELD child becomes either:
///  * a `(TokenKey, Node)` pair, OR
///  * a hoisted [`crate::Directive`] on the outer Node's
///    `directives` (used for standalone `#schema X { ... }`,
///    `#import ... from ...`, `#main(...)` directive lines inside a
///    dict literal, which the legacy `parse_dict` accumulates onto
///    the dict node's `directives` field).
///
/// This walker mirrors the legacy `parse_dict` + `parse_pair` /
/// `parse_keyed_value` chain, including:
///  * Spread keys (`...base` and typed `...<T> base`).
///  * Typed keys (`Type key: value`), with the `type_hint` stamped
///    onto the value Node.
///  * Method-shorthand closures (`key(params) [-> Ret]: body` →
///    `value = Closure { params, return_type: type_hint, body }`).
///  * Dynamic keys (`[expr]: value`, typed `[<T> expr]: value`).
///  * Standalone-directive hoisting.
///  * Doc-comment + decorator/directive attachment on each pair.
fn lower_dict_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    // The CST's DICT node may include leading trivia (whitespace /
    // comments) before the opening `{` due to rowan's `open()` flush
    // rule. The legacy `parse_dict` captures
    // `start_offset = input.location()` at the position of `{`. Mirror
    // that by anchoring to the `{` token's start.
    let start: usize = node
        .children_with_tokens()
        .find_map(|el| {
            el.into_token().and_then(|t| {
                if t.kind() == SyntaxKind::L_BRACE {
                    Some(t.text_range().start().into())
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| node.text_range().start().into());
    let end: usize = node.text_range().end().into();
    let mut pairs: Vec<(TokenKey, Node)> = Vec::new();
    let mut standalone_directives: Vec<crate::Directive> = Vec::new();
    let fields: Vec<SyntaxNode> = node
        .children()
        .filter(|c| c.kind() == SyntaxKind::DICT_FIELD)
        .collect();
    let total = fields.len();
    for (idx, field) in fields.into_iter().enumerate() {
        // Detect a completely empty DICT_FIELD (the CST emits one
        // between two commas, e.g. `{ a: 1, , b: 2 }`). The legacy
        // `separated(0.., parse_pair, ...)` rejected this directly.
        // A trailing empty field (after the last comma) is fine —
        // that's the standard trailing-comma form.
        let is_empty = field
            .children_with_tokens()
            .filter_map(|el| match el {
                rowan::NodeOrToken::Token(t) => Some(t),
                rowan::NodeOrToken::Node(_) => None,
            })
            .all(|t| {
                matches!(
                    t.kind(),
                    SyntaxKind::WHITESPACE | SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT
                )
            })
            && field.children().next().is_none();
        if is_empty {
            if idx + 1 == total {
                // Trailing empty field — accept and skip.
                continue;
            }
            // Mid-list empty field — malformed (`, ,`). Strict mode
            // bails; recovering mode skips and lets surrounding
            // fields survive for the IDE.
            if is_recovering() {
                continue;
            }
            return None;
        }
        match lower_dict_field(&field, source) {
            Some(DictFieldOut::Pair(k, v)) => pairs.push((k, v)),
            Some(DictFieldOut::Directives(dirs)) => standalone_directives.extend(dirs),
            // Recovering mode: skip the bad field so siblings stay
            // reachable for completion. Strict mode: propagate the
            // failure as `ParseDocumentError`.
            None if is_recovering() => continue,
            None => return None,
        }
    }

    let mut out = Node::new(Expr::Dict(pairs), range_from_offsets(source, start, end));
    out.directives = standalone_directives;
    Some(out)
}

/// What a single DICT_FIELD lowered into. Mirrors
/// [`crate::structure::dict::DictEntry`].
///
/// The `Pair` variant is intentionally large (TokenKey + full Node tree)
/// — boxing it would just relocate the heap pressure rather than reduce
/// total alloc; the enum is only ever held briefly inside
/// [`lower_dict_field`]'s return and immediately destructured, so the
/// size difference between variants does not propagate through any
/// long-lived container.
#[allow(clippy::large_enum_variant)]
enum DictFieldOut {
    Pair(TokenKey, Node),
    Directives(Vec<crate::Directive>),
}

/// Lower one DICT_FIELD CST node.
fn lower_dict_field(node: &SyntaxNode, source: &str) -> Option<DictFieldOut> {
    // ---- 1. Gather leading attributes + doc comment. -------------------
    let mut decorators_before: Vec<crate::Decorator> = Vec::new();
    let mut directives_before: Vec<crate::Directive> = Vec::new();
    // Doc-comment: leading LINE_COMMENT / BLOCK_COMMENT trivia *before*
    // the first non-trivia child of DICT_FIELD. Re-use the shared
    // `first_non_trivia_offset` helper + `parse_leading_comments`
    // rather than rolling a private trivia scanner here.
    let field_start: usize = node.text_range().start().into();
    let doc_comment: Option<String> = first_non_trivia_offset(node)
        .filter(|end_off| *end_off > field_start)
        .and_then(|end_off| {
            let leading_slice = &source[field_start..end_off];
            crate::parse_leading_comments(leading_slice).0
        });

    // ---- 2. Walk children to identify the field shape. ----------------
    // Collect the children in order for dispatch.
    let mut spread_node: Option<SyntaxNode> = None;
    let mut type_hint: Option<crate::TypeNode> = None;
    let mut key_token: Option<crate::syntax::SyntaxToken> = None; // IDENT or STRING
    let mut dynamic_key_node: Option<SyntaxNode> = None; // Expr inside [...]
    let mut dynamic_key_type: Option<crate::TypeNode> = None; // <T> inside [<T> expr]
    let mut value_expr_ast: Option<ast::Expr> = None;
    let mut closure_node: Option<SyntaxNode> = None;
    let mut in_brack = false;
    let mut after_lt = false;
    // Track whether a top-level COLON has been seen in DICT_FIELD —
    // used to distinguish method-shorthand CLOSURE (COLON inside
    // CLOSURE) from a regular dict-field value that's a closure
    // expression (COLON before CLOSURE).
    let mut saw_dict_field_colon = false;
    // Pre-built TokenKey produced by the schema-colon directive split
    // when the schema name carries generics — `Page<T>` etc.
    let mut prebuilt_key: Option<TokenKey> = None;

    let child_iter = node.children_with_tokens();
    for el in child_iter {
        match el {
            rowan::NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::WHITESPACE | SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT => {
                    continue
                }
                SyntaxKind::L_BRACK => {
                    in_brack = true;
                }
                SyntaxKind::R_BRACK => {
                    in_brack = false;
                }
                SyntaxKind::LT if in_brack => {
                    after_lt = true;
                }
                SyntaxKind::GT if in_brack => {
                    after_lt = false;
                }
                SyntaxKind::IDENT | SyntaxKind::STRING => {
                    if !in_brack && key_token.is_none() && value_expr_ast.is_none() {
                        key_token = Some(t);
                    }
                }
                SyntaxKind::COLON => {
                    // Body starts after the colon.
                    if !in_brack {
                        saw_dict_field_colon = true;
                    }
                }
                _ => continue,
            },
            rowan::NodeOrToken::Node(n) => match n.kind() {
                SyntaxKind::SPREAD_EXPR => {
                    spread_node = Some(n);
                }
                SyntaxKind::DECORATOR => {
                    if let Some(dec) = ast::Decorator::cast(n.clone()) {
                        decorators_before.push(lower_decorator_v2(&dec, source)?);
                    }
                }
                SyntaxKind::DIRECTIVE => {
                    if let Some(dir) = ast::Directive::cast(n.clone()) {
                        // Schema-colon rewind: when `#schema Image: { ... }`
                        // appears inside a dict, the legacy parser
                        // returns the directive as Bare and leaves
                        // `Image: { ... }` for the surrounding
                        // dict-field grammar. The CST groups them
                        // under one DIRECTIVE node. Detect this here
                        // and synthesize the dict-field shape:
                        //   * Directive emitted as Bare onto
                        //     `directives_before`.
                        //   * Image becomes the field key (or a
                        //     Dynamic(Type) when generics are present).
                        //   * `{ ... }` becomes the field value.
                        if let Some(split) = split_schema_colon_directive(&n, source) {
                            directives_before.push(split.directive);
                            match split.key {
                                SchemaColonKey::SimpleIdent(tok) => {
                                    key_token = Some(tok);
                                }
                                SchemaColonKey::TypedDynamic(type_node) => {
                                    let range = type_node.range;
                                    prebuilt_key = Some(TokenKey::Dynamic(
                                        Node::new(Expr::Type(type_node), range),
                                        false,
                                    ));
                                }
                            }
                            value_expr_ast = Some(split.value);
                            continue;
                        }
                        directives_before.push(lower_directive_v2(&dir, source)?);
                    }
                }
                SyntaxKind::TYPE_NODE => {
                    if in_brack && after_lt {
                        // Type inside `[<T> expr]`.
                        dynamic_key_type = Some(lower_type_node_from_cst(&n, source)?);
                    } else if !in_brack && type_hint.is_none() && key_token.is_none() {
                        // Leading typed-key hint.
                        type_hint = Some(lower_type_node_from_cst(&n, source)?);
                    } else if !in_brack && saw_dict_field_colon && value_expr_ast.is_none() {
                        // TYPE_NODE in value position (`key: SomeType`
                        // or `key: Enum<...>`). Cast as an Expr::Type-
                        // shaped value.
                        if let Some(e) = ast::Expr::cast(n.clone()) {
                            value_expr_ast = Some(e);
                        }
                    }
                }
                SyntaxKind::TUPLE_TYPE => {
                    // v1.7 tuple-type as typed-key hint: `(Int, String) pair: ...`.
                    if !in_brack && type_hint.is_none() && key_token.is_none() {
                        type_hint = Some(lower_type_node_from_cst(&n, source)?);
                    }
                }
                SyntaxKind::CLOSURE => {
                    // Distinguish method-shorthand (`key(params): body`,
                    // where the CST emits the CLOSURE as the direct
                    // child of DICT_FIELD *without* a preceding COLON
                    // token, since the COLON sits inside the CLOSURE
                    // node) from a regular dict-field value that
                    // happens to be a `(p) => body` closure (where a
                    // COLON token sits between the key and the
                    // CLOSURE child). For the regular form treat the
                    // CLOSURE as a normal value Expr.
                    if saw_dict_field_colon {
                        if value_expr_ast.is_none() && !in_brack {
                            if let Some(e) = ast::Expr::cast(n.clone()) {
                                value_expr_ast = Some(e);
                            }
                        }
                    } else if closure_node.is_none() {
                        closure_node = Some(n);
                    }
                }
                _ => {
                    // Any other expression node — could be the dynamic-key
                    // inner expression (if in_brack) or the value.
                    if let Some(e) = ast::Expr::cast(n.clone()) {
                        if in_brack && dynamic_key_node.is_none() {
                            dynamic_key_node = Some(e.syntax().clone());
                        } else if value_expr_ast.is_none() && !in_brack {
                            value_expr_ast = Some(e);
                        }
                    }
                }
            },
        }
    }

    // ---- 3. Spread shape. ---------------------------------------------
    if let Some(sn) = spread_node {
        let s_r = sn.text_range();
        let s_start: usize = s_r.start().into();
        // The SPREAD_EXPR's "..." token range — we need just the
        // ellipsis position (3 bytes) for TokenKey::Spread.
        // Find the ELLIPSIS token end.
        let mut ellipsis_end = s_start + 3;
        for el in sn.children_with_tokens() {
            if let Some(t) = el.into_token() {
                if t.kind() == SyntaxKind::ELLIPSIS {
                    ellipsis_end = t.text_range().end().into();
                    break;
                }
            }
        }
        // Type hint inside `...<T>` becomes the inner's type_hint.
        let mut inner_type: Option<crate::TypeNode> = None;
        let mut inner_expr_ast: Option<ast::Expr> = None;
        let mut saw_lt = false;
        for el in sn.children_with_tokens() {
            match el {
                rowan::NodeOrToken::Token(t) => match t.kind() {
                    SyntaxKind::LT => saw_lt = true,
                    SyntaxKind::GT => saw_lt = false,
                    _ => {}
                },
                rowan::NodeOrToken::Node(n) => {
                    if n.kind() == SyntaxKind::TYPE_NODE && saw_lt {
                        inner_type = Some(lower_type_node_from_cst(&n, source)?);
                    } else if let Some(e) = ast::Expr::cast(n.clone()) {
                        if inner_expr_ast.is_none() {
                            inner_expr_ast = Some(e);
                        }
                    }
                }
            }
        }
        let inner_ast = inner_expr_ast?;
        let mut inner = lower_expr_v2(&inner_ast, source)?;
        if let Some(t) = inner_type {
            inner.type_hint = Some(t);
        }
        return Some(DictFieldOut::Pair(
            TokenKey::Spread(range_from_offsets(source, s_start, ellipsis_end)),
            inner,
        ));
    }

    // ---- 4. Attribute-only field (standalone directives). -------------
    if key_token.is_none()
        && dynamic_key_node.is_none()
        && value_expr_ast.is_none()
        && closure_node.is_none()
    {
        if decorators_before.is_empty() {
            return Some(DictFieldOut::Directives(directives_before));
        }
        // Stray standalone decorators are rejected by the legacy
        // parser; mirror that.
        return None;
    }

    // ---- 5. Compute the key. ------------------------------------------
    let key = if let Some(pk) = prebuilt_key {
        pk
    } else if let Some(dyn_node) = dynamic_key_node {
        let dyn_ast = ast::Expr::cast(dyn_node)?;
        let mut inner = lower_expr_v2(&dyn_ast, source)?;
        if let Some(t) = dynamic_key_type {
            inner.type_hint = Some(t);
        }
        TokenKey::Dynamic(inner, false)
    } else {
        let kt = key_token?;
        let tr = kt.text_range();
        let s: usize = tr.start().into();
        let e: usize = tr.end().into();
        let key_range = range_from_offsets(source, s, e);
        match kt.kind() {
            SyntaxKind::IDENT => TokenKey::String(kt.text().to_string(), key_range, false),
            SyntaxKind::STRING => {
                let decoded = parse_string_text(kt.text())?;
                TokenKey::String(decoded, key_range, false)
            }
            _ => return None,
        }
    };

    // ---- 6. Build the value. ------------------------------------------
    let value = if let Some(cls) = closure_node {
        // Method-shorthand desugar. The CST wraps the closure
        // (params + optional return type + body) into a CLOSURE node
        // via `open_at(closure_ck, CLOSURE)` where `closure_ck`
        // points at `(` of the params. Legacy `parse_keyed_value`
        // builds the closure differently:
        //   * `range = create_range(input, value_start, value_end)`
        //     where value_start/end bracket the BODY expression only.
        //   * `return_type = parsed_type_hint.clone()` — the typed-key
        //     hint becomes the return type, even when no `-> Ret` was
        //     written.
        // We replicate that by using the body's range and overriding
        // the closure's return_type with the leading type_hint.
        let cls_node = lower_closure_v2(&cls, source)?;
        let mut cls_node = cls_node;
        // Find the body Expr inside the CLOSURE CST node — last
        // non-CLOSURE_PARAM / non-TYPE_NODE child.
        let body_range_opt: Option<TokenRange> = {
            let mut last_body: Option<TokenRange> = None;
            for child in cls.children() {
                if matches!(
                    child.kind(),
                    SyntaxKind::CLOSURE_PARAM | SyntaxKind::TYPE_NODE
                ) {
                    continue;
                }
                if let Some(e) = ast::Expr::cast(child) {
                    let body = lower_expr_v2(&e, source)?;
                    last_body = Some(body.range);
                }
            }
            last_body
        };
        if let Some(br) = body_range_opt {
            cls_node.range = br;
        }
        // `lower_closure_v2` builds the `expr` `Arc` just above; it is not
        // shared with any other clone yet, so `try_unwrap` lets us move the
        // inner `Expr` out without copying the (potentially recursive)
        // closure body. Falls back to a `clone` only if a future refactor
        // shares the `Arc` earlier — defensive belt-and-braces.
        let owned_expr = std::sync::Arc::try_unwrap(std::mem::replace(
            &mut cls_node.expr,
            std::sync::Arc::new(Expr::Null),
        ))
        .unwrap_or_else(|shared| (*shared).clone());
        cls_node.expr = match owned_expr {
            Expr::Closure {
                params,
                return_type,
                body,
            } => {
                // Legacy applies `return_type: parsed_type_hint.clone()`
                // — this REPLACES any existing return type. Match that.
                let final_return = if type_hint.is_some() {
                    type_hint.clone()
                } else {
                    return_type
                };
                std::sync::Arc::new(Expr::Closure {
                    params,
                    return_type: final_return,
                    body,
                })
            }
            // Defensive: `lower_closure_v2` always yields `Closure` today,
            // but restore the original expr if a future change ever breaks
            // that invariant.
            other => std::sync::Arc::new(other),
        };
        cls_node
    } else {
        let value_ast = value_expr_ast?;
        let mut value = lower_expr_v2(&value_ast, source)?;
        if type_hint.is_some() {
            value.type_hint = type_hint.clone();
        }
        value
    };

    let value = value
        .with_decorators(decorators_before)
        .with_directives(directives_before)
        .with_doc_comment(doc_comment);

    Some(DictFieldOut::Pair(key, value))
}

/// Lower a `LIST` CST node into `Expr::List(items)`. Each child Expr
/// is one item (regular value or SPREAD_EXPR). Leading
/// directives/decorators between elements are attached to the
/// following item Node — mirroring `parse_atom`'s attribute-collection
/// loop that prepends them to the next atom in legacy code.
fn lower_list_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();
    let mut items: Vec<Node> = Vec::new();
    let mut pending_decs: Vec<crate::Decorator> = Vec::new();
    let mut pending_dirs: Vec<crate::Directive> = Vec::new();
    for child in node.children() {
        match child.kind() {
            SyntaxKind::DECORATOR => {
                if let Some(d) = ast::Decorator::cast(child.clone()) {
                    match lower_decorator_v2(&d, source) {
                        Some(dec) => pending_decs.push(dec),
                        None if is_recovering() => continue,
                        None => return None,
                    }
                }
            }
            SyntaxKind::DIRECTIVE => {
                if let Some(d) = ast::Directive::cast(child.clone()) {
                    match lower_directive_v2(&d, source) {
                        Some(dir) => pending_dirs.push(dir),
                        None if is_recovering() => continue,
                        None => return None,
                    }
                }
            }
            _ => {
                if let Some(e) = ast::Expr::cast(child.clone()) {
                    let item_opt = lower_expr_v2(&e, source);
                    let mut item = match item_opt {
                        Some(n) => n,
                        // Recovering mode: substitute a Null
                        // placeholder so the list's ordinal positions
                        // stay stable. Strict mode: bail.
                        None if is_recovering() => {
                            let r = child.text_range();
                            let start_o: usize = r.start().into();
                            let end_o: usize = r.end().into();
                            Node::new(Expr::Null, range_from_offsets(source, start_o, end_o))
                        }
                        None => return None,
                    };
                    if !pending_decs.is_empty() {
                        item.decorators = std::mem::take(&mut pending_decs);
                    }
                    if !pending_dirs.is_empty() {
                        item.directives = std::mem::take(&mut pending_dirs);
                    }
                    items.push(item);
                }
            }
        }
    }
    Some(Node::new(
        Expr::List(items),
        range_from_offsets(source, start, end),
    ))
}

/// Lower a `COMPREHENSION` CST node into `Expr::Comprehension`.
/// Children in source order: element Expr, then `for` IDENT bind, then
/// iterable Expr, optional `if` cond Expr.
fn lower_comprehension_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();

    // Read the bound identifier (IDENT after `for`).
    let mut id_text: Option<String> = None;
    let mut after_for = false;
    for el in node.children_with_tokens() {
        if let Some(t) = el.into_token() {
            if t.kind() == SyntaxKind::IDENT {
                let txt = t.text();
                if after_for && id_text.is_none() {
                    id_text = Some(txt.to_string());
                } else if txt == "for" {
                    after_for = true;
                }
            }
        }
    }
    let id = id_text?;

    // Three Expr children: element, iterable, optional condition.
    let mut exprs = node.children().filter_map(ast::Expr::cast);
    let element_ast = exprs.next()?;
    let iterable_ast = exprs.next()?;
    let condition_ast = exprs.next();
    let element = lower_expr_v2(&element_ast, source)?;
    let iterable = lower_expr_v2(&iterable_ast, source)?;
    let condition = match condition_ast {
        Some(c) => Some(lower_expr_v2(&c, source)?),
        None => None,
    };
    Some(Node::new(
        Expr::Comprehension {
            element,
            id,
            iterable,
            condition,
        },
        range_from_offsets(source, start, end),
    ))
}

/// Decode an F_STRING_LITERAL token's text. Non-raw f-strings honour
/// the same escape set as normal strings (`\n`, `\t`, `\\`, `\u{...}`,
/// `\"`, `\\<whitespace>` for line-continuation); raw f-strings (the
/// `f#"..."#` form) take the text verbatim.
fn decode_fstring_literal(text: &str, raw: bool) -> Option<String> {
    if raw {
        return Some(text.to_string());
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let escape = chars.next()?;
        match escape {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{08}'),
            'f' => out.push('\u{0C}'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            '"' => out.push('"'),
            'u' => {
                let cp = if chars.peek().copied() == Some('{') {
                    chars.next();
                    let mut hex = String::new();
                    loop {
                        let h = chars.next()?;
                        if h == '}' {
                            break;
                        }
                        if !h.is_ascii_hexdigit() {
                            return None;
                        }
                        hex.push(h);
                    }
                    if hex.is_empty() || hex.len() > 6 {
                        return None;
                    }
                    u32::from_str_radix(&hex, 16).ok()?
                } else {
                    let mut hex = String::new();
                    for _ in 0..4 {
                        let h = chars.next()?;
                        if !h.is_ascii_hexdigit() {
                            return None;
                        }
                        hex.push(h);
                    }
                    u32::from_str_radix(&hex, 16).ok()?
                };
                out.push(std::char::from_u32(cp)?);
            }
            w if w.is_whitespace() => {
                // Line-continuation: swallow all following whitespace.
                while let Some(&peek) = chars.peek() {
                    if peek.is_whitespace() {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Lower an `F_STRING` CST node into the legacy
/// `Expr::FString(Vec<FStringPart>)` shape. The CST already decomposed
/// the source into F_STRING_OPEN / F_STRING_LITERAL* /
/// F_STRING_INTERPOLATION* / F_STRING_CLOSE tokens + sub-nodes; we
/// walk them in source order, decoding non-raw escapes inside literal
/// chunks and merging adjacent Literal parts to match the legacy
/// `merge_parts` post-processing.
fn lower_fstring_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();
    // Detect raw form by looking at F_STRING_OPEN's text (`f"` vs
    // `f#"` etc. — raw if any `#` between `f` and `"`).
    let raw = node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| t.kind() == SyntaxKind::F_STRING_OPEN)
        .map(|t| t.text().contains('#'))
        .unwrap_or(false);

    let mut parts: Vec<crate::FStringPart> = Vec::new();
    for el in node.children_with_tokens() {
        match el {
            rowan::NodeOrToken::Token(t) => {
                if t.kind() == SyntaxKind::F_STRING_LITERAL {
                    let decoded = decode_fstring_literal(t.text(), raw)?;
                    if decoded.is_empty() {
                        continue;
                    }
                    if let Some(crate::FStringPart::Literal(ref mut last)) = parts.last_mut() {
                        last.push_str(&decoded);
                    } else {
                        parts.push(crate::FStringPart::Literal(decoded));
                    }
                }
            }
            rowan::NodeOrToken::Node(n) => {
                if n.kind() == SyntaxKind::F_STRING_INTERPOLATION {
                    // The interpolation has one inner Expr child.
                    let inner_ast = n.children().find_map(ast::Expr::cast)?;
                    let inner = lower_expr_v2(&inner_ast, source)?;
                    parts.push(crate::FStringPart::Interpolation(Box::new(inner)));
                }
            }
        }
    }

    Some(Node::new(
        Expr::FString(parts),
        range_from_offsets(source, start, end),
    ))
}

/// Lower a `VARIANT_CTOR` CST node into the legacy
/// `Expr::VariantCtor` shape. The CST emits tokens for the dotted
/// path (e.g. `Result.Ok` is IDENT DOT IDENT) followed by a DICT
/// child for the body braces.
fn lower_variant_ctor_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();

    // Path: every IDENT token in the head, in source order.
    let mut path: Vec<String> = Vec::new();
    for el in node.children_with_tokens() {
        if let Some(t) = el.into_token() {
            if t.kind() == SyntaxKind::IDENT {
                path.push(t.text().to_string());
            }
        }
    }
    if path.len() < 2 {
        return None;
    }
    let variant = path.pop().unwrap();

    // Body: the DICT child carries the constructor's fields.
    let body_dict = node.children().find(|c| c.kind() == SyntaxKind::DICT)?;
    let body_expr = ast::Expr::cast(body_dict)?;
    let body = lower_expr_v2(&body_expr, source)?;

    Some(Node::new(
        Expr::VariantCtor {
            enum_path: path,
            variant,
            body,
        },
        range_from_offsets(source, start, end),
    ))
}

/// Lower a CLOSURE_PARAM CST node into the legacy `ClosureParam`.
///
/// The CST emits CLOSURE_PARAM in two roles:
///  * Standalone-closure form `(p1, p2) => body` — params are
///    `[Type] ident` (type optional, but if present comes before name).
///  * Schema-method param `name: Type` — name first, then `:`, then
///    type. This branch is detected by checking for a COLON token
///    inside the node.
///
/// The `range` follows the legacy `parse_closure_param`:
///  * Standalone form: from `start_offset` (before any type) to
///    `end_offset` (after the name IDENT).
///  * Schema-method form: handled by `lower_schema_method_param`
///    instead, not here.
fn lower_closure_param_v2(node: &SyntaxNode, source: &str) -> Option<crate::ClosureParam> {
    let r = node.text_range();
    // Use the first non-trivia child as start (mirrors legacy
    // `start_offset = input.location()` after `soc0` in the
    // `(p1, p2)` list separator).
    let start: usize = node
        .children_with_tokens()
        .find_map(|el| match el {
            rowan::NodeOrToken::Token(t)
                if matches!(
                    t.kind(),
                    SyntaxKind::WHITESPACE | SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT
                ) =>
            {
                None
            }
            rowan::NodeOrToken::Token(t) => Some(t.text_range().start().into()),
            rowan::NodeOrToken::Node(n) => Some(n.text_range().start().into()),
        })
        .unwrap_or_else(|| r.start().into());
    let end: usize = r.end().into();

    let type_hint = node
        .children()
        .find(|c| c.kind() == SyntaxKind::TYPE_NODE)
        .and_then(|n| lower_type_node_from_cst(&n, source));
    // The closure param's name IDENT — for `Type ident` form it's
    // the last IDENT child (since TYPE_NODE may contain IDENT
    // tokens too — but TYPE_NODE is a Node child, not a token
    // child of CLOSURE_PARAM).
    let name_tok = node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|t| t.kind() == SyntaxKind::IDENT)
        .last()?;
    Some(crate::ClosureParam {
        name: name_tok.text().to_string(),
        type_hint,
        range: range_from_offsets(source, start, end),
    })
}

/// Lower a `CLOSURE` CST node. Shape: `( params ) [-> RetType] =>
/// body`. Maps to legacy `Expr::Closure { params, return_type, body }`.
fn lower_closure_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();

    let mut params: Vec<crate::ClosureParam> = Vec::new();
    let mut return_type: Option<crate::TypeNode> = None;
    let mut body_ast: Option<ast::Expr> = None;
    let mut saw_arrow = false;

    for el in node.children_with_tokens() {
        match el {
            rowan::NodeOrToken::Token(t) => {
                if t.kind() == SyntaxKind::THIN_ARROW {
                    saw_arrow = true;
                }
            }
            rowan::NodeOrToken::Node(n) => match n.kind() {
                SyntaxKind::CLOSURE_PARAM => {
                    params.push(lower_closure_param_v2(&n, source)?);
                }
                SyntaxKind::TYPE_NODE if saw_arrow && return_type.is_none() => {
                    return_type = Some(lower_type_node_from_cst(&n, source)?);
                }
                _ => {
                    if let Some(e) = ast::Expr::cast(n.clone()) {
                        if body_ast.is_none() {
                            body_ast = Some(e);
                        }
                    }
                }
            },
        }
    }

    let body_ast = body_ast?;
    let body = lower_expr_v2(&body_ast, source)?;
    Some(Node::new(
        Expr::Closure {
            params,
            return_type,
            body,
        },
        range_from_offsets(source, start, end),
    ))
}

/// Lower a `CALL_EXPR` CST node into the legacy `Expr::FnCall` shape.
/// The callee is normally a VARIABLE_EXPR whose path tokens we extract
/// via [`walk_path_tokens`]; the args list is the CALL_ARG child
/// walked via [`walk_call_arg_node`].
///
/// Range note: like BINARY_EXPR, the CST's CALL_EXPR `text_range()`
/// may extend beyond the actual callee start because the CST wraps
/// the call via a checkpoint. Legacy `parse_fn_call` captures
/// `start_offset = input.location()` *at the callee path's start*
/// (after `parse_atomic`'s soc0). We match that by reading the
/// callee VARIABLE_EXPR's range start.
fn lower_call_expr_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    // Find the callee Expr and the CALL_ARG node.
    let mut callee_node: Option<SyntaxNode> = None;
    let mut call_arg_node: Option<SyntaxNode> = None;
    for child in node.children() {
        if child.kind() == SyntaxKind::CALL_ARG {
            call_arg_node = Some(child);
        } else if callee_node.is_none() && ast::Expr::cast(child.clone()).is_some() {
            callee_node = Some(child);
        }
    }
    let callee = callee_node?;
    // Path extraction: the callee should be a VARIABLE_EXPR. Anything
    // else is malformed for the legacy `FnCall` shape (the analyzer
    // doesn't accept call-on-expression at parse time; postfix calls
    // require a path callee).
    if callee.kind() != SyntaxKind::VARIABLE_EXPR {
        return None;
    }
    let path = walk_path_tokens(&callee, source, /*is_reference=*/ false)?;
    let callee_start: usize = callee.text_range().start().into();
    // The end offset is the end of the CALL_ARG node (which covers
    // through `)`), or the end of the CALL_EXPR itself if no args
    // node found.
    let end: usize = call_arg_node
        .as_ref()
        .map(|n| n.text_range().end().into())
        .unwrap_or_else(|| node.text_range().end().into());
    let args = if let Some(args_node) = call_arg_node {
        walk_call_arg_node(&args_node, source)?
    } else {
        Vec::new()
    };
    // Legacy `parse_fn_call` verifies named-args-after-positional ordering.
    let mut saw_named = false;
    for a in &args {
        if a.name.is_some() {
            saw_named = true;
        } else if saw_named {
            return None;
        }
    }
    Some(Node::new(
        Expr::FnCall { path, args },
        range_from_offsets(source, callee_start, end),
    ))
}

/// Lower a `BINARY_EXPR` CST node. The CST already encoded operator
/// precedence + associativity in the tree shape (left/right children
/// are nested BINARY_EXPRs at higher / lower precedence). We just
/// read off the operator token and recurse on both sides.
///
/// Range note: the CST's BINARY_EXPR `text_range()` may extend past
/// the lhs subexpression's actual start because the CST wraps via
/// `open_at(lhs_ck, BINARY_EXPR)` where `lhs_ck` is captured *before*
/// any leading `(` of a grouped LHS. The legacy parser instead
/// computes the binary range as `combine_ranges(lhs.range, rhs.range)`
/// — i.e. the bounds of the actual operands, no surrounding parens.
/// We replicate that here.
fn lower_binary_expr_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let mut exprs = node.children().filter_map(ast::Expr::cast);
    let lhs_ast = exprs.next()?;
    let rhs_ast = exprs.next()?;
    let op_token = node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| {
            matches!(
                t.kind(),
                SyntaxKind::PLUS
                    | SyntaxKind::MINUS
                    | SyntaxKind::STAR
                    | SyntaxKind::SLASH
                    | SyntaxKind::PERCENT
                    | SyntaxKind::PLUS_PLUS
                    | SyntaxKind::EQ_EQ
                    | SyntaxKind::BANG_EQ
                    | SyntaxKind::LT
                    | SyntaxKind::GT
                    | SyntaxKind::LT_EQ
                    | SyntaxKind::GT_EQ
                    | SyntaxKind::AMP_AMP
                    | SyntaxKind::PIPE_PIPE
                    | SyntaxKind::PIPE
            )
        })?;
    let op = match op_token.kind() {
        SyntaxKind::PLUS => crate::Operator::Add,
        SyntaxKind::MINUS => crate::Operator::Sub,
        SyntaxKind::STAR => crate::Operator::Mul,
        SyntaxKind::SLASH => crate::Operator::Div,
        SyntaxKind::PERCENT => crate::Operator::Mod,
        SyntaxKind::PLUS_PLUS => crate::Operator::Concat,
        SyntaxKind::EQ_EQ => crate::Operator::Eq,
        SyntaxKind::BANG_EQ => crate::Operator::Ne,
        SyntaxKind::LT => crate::Operator::Lt,
        SyntaxKind::GT => crate::Operator::Gt,
        SyntaxKind::LT_EQ => crate::Operator::Le,
        SyntaxKind::GT_EQ => crate::Operator::Ge,
        SyntaxKind::AMP_AMP => crate::Operator::And,
        SyntaxKind::PIPE_PIPE => crate::Operator::Or,
        SyntaxKind::PIPE => crate::Operator::Pipe,
        _ => return None,
    };
    let lhs = lower_expr_v2(&lhs_ast, source)?;
    let rhs = lower_expr_v2(&rhs_ast, source)?;
    let combined = crate::combine_ranges(lhs.range, rhs.range);
    Some(Node::new(Expr::Binary(op, lhs, rhs), combined))
}

/// Lower a `TERNARY_EXPR` CST node. Three child expressions: cond,
/// then, else (in source order). Maps to legacy `Expr::Ternary`.
fn lower_ternary_expr_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();
    let mut exprs = node.children().filter_map(ast::Expr::cast);
    let cond_ast = exprs.next()?;
    let then_ast = exprs.next()?;
    let els_ast = exprs.next()?;
    let cond = lower_expr_v2(&cond_ast, source)?;
    let then = lower_expr_v2(&then_ast, source)?;
    let els = lower_expr_v2(&els_ast, source)?;
    Some(Node::new(
        Expr::Ternary { cond, then, els },
        range_from_offsets(source, start, end),
    ))
}

/// Lower a `WHERE_EXPR` CST node. Shape: `<expr> where <dict>`. The
/// bindings dict is wrapped in a `Node` whose `Expr::Dict` carries
/// each binding; the legacy `parse_where` produces the dict via
/// `parse_dict` (which still goes through `lower_expr_via_legacy` for
/// the dict body — that bridge retires later in P6 round 2).
fn lower_where_expr_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();
    let mut exprs = node.children().filter_map(ast::Expr::cast);
    let base_ast = exprs.next()?;
    // The bindings dict is the second expression child (the CST emits
    // a DICT under the WHERE_EXPR after the `where` keyword).
    let bindings_ast = exprs.next()?;
    let base = lower_expr_v2(&base_ast, source)?;
    let bindings = lower_expr_v2(&bindings_ast, source)?;
    Some(Node::new(
        Expr::Where {
            expr: base,
            bindings,
        },
        range_from_offsets(source, start, end),
    ))
}

/// Lower a `MATCH_EXPR` CST node. The scrutinee is the first
/// expression child; each `MATCH_ARM` carries a pattern (TYPE_NODE or
/// WILDCARD) and a body expression.
fn lower_match_expr_v2(node: &SyntaxNode, source: &str) -> Option<Node> {
    let r = node.text_range();
    let start: usize = r.start().into();
    let end: usize = r.end().into();
    let scrutinee_ast = node.children().find_map(ast::Expr::cast)?;
    let scrutinee = lower_expr_v2(&scrutinee_ast, source)?;
    let mut arms: Vec<(Node, Node)> = Vec::new();
    for arm in node
        .children()
        .filter(|c| c.kind() == SyntaxKind::MATCH_ARM)
    {
        let mut arm_exprs = arm.children().filter_map(ast::Expr::cast);
        let pat_ast = arm_exprs.next()?;
        let body_ast = arm_exprs.next()?;
        let pattern = lower_expr_v2(&pat_ast, source)?;
        let body = lower_expr_v2(&body_ast, source)?;
        arms.push((pattern, body));
    }
    Some(Node::new(
        Expr::Match {
            expr: scrutinee,
            arms,
        },
        range_from_offsets(source, start, end),
    ))
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
        ast::Expr::Dict(d) => lower_dict_v2(d.syntax(), source),
        ast::Expr::List(l) => lower_list_v2(l.syntax(), source),
        ast::Expr::Spread(s) => lower_spread_expr_v2(s.syntax(), source),
        ast::Expr::Comprehension(c) => lower_comprehension_v2(c.syntax(), source),
        // Slice 3: operators + calls. The legacy `parse_expr` already
        // routes binary precedence (`parse_pipe` → `parse_logic_or`
        // → ... → `parse_multiplicative`), unary, ternary, and
        // `parse_fn_call` (with positional + named args) — all reached
        // from `parse_atomic`. Routing each CST node through it keeps
        // operator associativity, precedence, and call-arg `name:`
        // detection byte-identical with no separate token-text → enum
        // table here.
        ast::Expr::Binary(b) => lower_binary_expr_v2(b.syntax(), source),
        ast::Expr::Unary(u) => lower_unary_expr_v2(u.syntax(), source),
        ast::Expr::Ternary(t) => lower_ternary_expr_v2(t.syntax(), source),
        ast::Expr::Call(c) => lower_call_expr_v2(c.syntax(), source),
        // Slice 4: control flow. `Closure`, `Match`, `Where`, and
        // `VariantCtor` all sit on top of expression-shaped CST nodes
        // whose byte ranges are accepted verbatim by `parse_expr`.
        // The closure shape (typed params, optional return type,
        // body) and match-arm pattern/body pairs round-trip
        // byte-identically through the legacy chain.
        ast::Expr::Closure(c) => lower_closure_v2(c.syntax(), source),
        ast::Expr::Match(m) => lower_match_expr_v2(m.syntax(), source),
        ast::Expr::Where(w) => lower_where_expr_v2(w.syntax(), source),
        ast::Expr::VariantCtor(v) => lower_variant_ctor_v2(v.syntax(), source),
        // Slice 5: f-strings. The CST decomposes an f-string into
        // F_STRING_LITERAL chunks + F_STRING_INTERPOLATION sub-nodes
        // for IDE highlighting, but the legacy parser keeps it as a
        // single `Expr::FString(Vec<FStringPart>)`. Routing the
        // F_STRING node's byte slice through `parse_expr` (which
        // reaches `parse_fmt_string` via `parse_atomic`) reconstructs
        // the legacy shape byte-identically — including the literal /
        // interpolation alternation and nested-expression
        // `TokenRange`s.
        ast::Expr::FString(fs) => lower_fstring_v2(fs.syntax(), source),
        // Slice 6: type expressions. The CST emits a bare TYPE_NODE
        // for `Int`, `List<T>`, `Foo?`, `(T1, T2, ...)` tuple types,
        // and tagged enum variants at any expression-shaped position.
        // The legacy parser surfaces these via `parse_type_expr`
        // (inside `parse_atomic`) as `Expr::Type(TypeNode)`. Routing
        // the TYPE_NODE byte slice through `parse_expr` preserves the
        // full TypeNode shape — `path` / `generics` / `is_optional` /
        // `variant_fields` / `range` / `doc_comment` — byte-
        // identically.
        ast::Expr::Type(t) => lower_type_expr_v2(t.syntax(), source),
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
pub fn lower_document_node_v2(doc: &ast::Document, source: &str) -> Option<Node> {
    // 1. Lower every leading directive + decorator. We capture them in
    //    source order within each kind — the legacy `parse_attributes`
    //    interleaves them in the input loop but stores them in two
    //    ordered Vecs, so a per-kind walk over CST children preserves
    //    the legacy ordering.
    let mut decorators: Vec<crate::Decorator> = Vec::new();
    for d in doc.decorators() {
        match lower_decorator_v2(&d, source) {
            Some(dec) => decorators.push(dec),
            None if is_recovering() => continue,
            None => return None,
        }
    }
    let mut directives: Vec<crate::Directive> = Vec::new();
    for d in doc.directives() {
        match lower_directive_v2(&d, source) {
            Some(dir) => directives.push(dir),
            None if is_recovering() => continue,
            None => return None,
        }
    }

    // 2. Lower the root expression. Recovering mode falls back to a
    //    placeholder Node when the root expression lowering fails so
    //    IDE callers (completion / hover / goto-def) still receive a
    //    navigable partial AST. Strict mode propagates the failure.
    let root_ast = doc.root_expr()?;
    let mut body = match lower_expr_v2(&root_ast, source) {
        Some(n) => n,
        None if is_recovering() => {
            let r = root_ast.syntax().text_range();
            let start_o: usize = r.start().into();
            let end_o: usize = r.end().into();
            let placeholder_expr = match root_ast.syntax().kind() {
                SyntaxKind::DICT => Expr::Dict(Vec::new()),
                SyntaxKind::LIST => Expr::List(Vec::new()),
                _ => Expr::Null,
            };
            Node::new(placeholder_expr, range_from_offsets(source, start_o, end_o))
        }
        None => return None,
    };

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
        directives.extend(std::mem::take(&mut body.directives));
    }

    // 5. Doc-comment: leading comments above the first attribute / root.
    //    Computed by running the legacy `parse_leading_comments`
    //    combinator on the byte prefix up to the first non-trivia
    //    offset.
    let leading_slice = source.get(0..start_offset).unwrap_or("");
    let (doc_comment, _) = crate::parse_leading_comments(leading_slice);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cst, parse_document, NodeId};

    /// Replace every [`NodeId`] in `node` with [`NodeId::SYNTHETIC`] so
    /// structural comparison is independent of allocation order.
    #[allow(dead_code)]
    fn strip_node_ids(node: &mut crate::Node) {
        node.id = NodeId::SYNTHETIC;
        // `Arc::make_mut` clones the inner `Expr` only when the `Arc` is
        // shared with another clone; in this test helper the parser tree
        // is uniquely owned, so no clone occurs in practice.
        match std::sync::Arc::make_mut(&mut node.expr) {
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

    #[test]
    fn lowering_detects_cst_error_descendant() {
        let parse = cst::parse_cst("{ broken @ # }");
        assert!(parse.has_errors() || has_error_descendant(&parse.syntax()));
        assert!(first_error_offset(&parse.syntax()).is_some() || parse.has_errors());
    }

    /// Every checked-in fixture that the parser accepts must lower
    /// without panicking, and the resulting Node must round-trip
    /// through `strip_node_ids` cleanly. The CST is the single source
    /// of truth for what's accepted; no per-construct parity guard is
    /// needed because the legacy combinator chain is gone.
    #[test]
    fn corpus_lowering_succeeds() {
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
        for path in files {
            let Ok(source) = fs::read_to_string(&path) else {
                continue;
            };
            if source.is_empty() {
                continue;
            }
            // Skip broken / known-error fixtures (they live under a
            // distinct dir and the parser is expected to reject them).
            if path.to_string_lossy().contains("/fixtures/broken/") {
                continue;
            }
            if let Ok(mut node) = parse_document(&source) {
                strip_node_ids(&mut node);
                checked += 1;
            }
        }
        assert!(checked > 0, "expected to lower at least one fixture");
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

    /// Trailing input must surface as `TrailingInput` with the
    /// offset stepped past inter-token trivia (matches the legacy
    /// behaviour callers were depending on).
    #[test]
    fn trailing_input_uses_legacy_offset() {
        let err = parse_document("{ a: 1 } true").unwrap_err();
        assert!(matches!(
            err,
            ParseDocumentError::TrailingInput { offset: 9, ref remaining }
                if remaining == "true"
        ));
    }

    /// The `#schema X: { ... }` colon-shape — Bare directive plus a
    /// separate `X: { ... }` dict field — must keep parsing without
    /// CST errors.
    #[test]
    fn schema_colon_body_form_parses() {
        let src = r#"{
            #schema Image: { name: String },
            data: { name: "img" }
        }"#;
        let parse = cst::parse_cst(src);
        let node = lower_document(&parse, src).expect("schema-colon shape lowers cleanly");
        assert!(matches!(*node.expr, Expr::Dict(_)));
        assert!(!parse.has_errors(), "no CST errors: {:?}", parse.errors);
    }

    #[test]
    fn parse_import_with_sha256_integrity() {
        let src = "#import lib from \"./lib.relon\" sha256:\"abcd1234\"\n{ x: 1 }";
        let node = parse_document(src).expect("parse #import with sha256");
        let dir = node
            .directives
            .iter()
            .find(|d| d.name == "import")
            .expect("import directive present");
        match &dir.body {
            crate::DirectiveBody::Import {
                path,
                integrity: Some(int),
                ..
            } => {
                assert_eq!(path, "./lib.relon");
                assert_eq!(int.algorithm, Some(crate::HashAlgorithm::Sha256));
                assert_eq!(int.algorithm_text, "sha256");
                assert_eq!(int.hex, "abcd1234");
                // Range must cover the algorithm IDENT through the hex
                // STRING (inclusive of quotes), not the path string.
                assert!(int.range.start.offset > 0);
                assert!(int.range.end.offset > int.range.start.offset);
            }
            other => panic!("unexpected import body shape: {other:?}"),
        }
    }

    #[test]
    fn parse_import_without_hash_is_unpinned() {
        let src = "#import lib from \"./lib.relon\"\n{ x: 1 }";
        let node = parse_document(src).expect("parse plain #import");
        let dir = node
            .directives
            .iter()
            .find(|d| d.name == "import")
            .expect("import directive present");
        match &dir.body {
            crate::DirectiveBody::Import { integrity, .. } => {
                assert!(integrity.is_none(), "expected no integrity pin");
            }
            other => panic!("unexpected import body shape: {other:?}"),
        }
    }

    #[test]
    fn parse_import_integrity_keeps_unknown_algorithm_in_ast() {
        // Unknown algorithms keep `algorithm = None` and preserve the
        // verbatim identifier in `algorithm_text` so the analyzer (not
        // the parser) can attach a span-aware diagnostic. What we
        // exercise here is that the *parser* lowers the directive
        // cleanly and the unknown name survives intact.
        let src = "#import lib from \"./lib.relon\" sha512:\"deadbeef\"\n{ x: 1 }";
        let node = parse_document(src).expect("parser accepts unknown algorithm");
        let dir = node
            .directives
            .iter()
            .find(|d| d.name == "import")
            .expect("import directive present");
        match &dir.body {
            crate::DirectiveBody::Import {
                integrity: Some(int),
                ..
            } => {
                assert_eq!(int.algorithm, None);
                assert_eq!(int.algorithm_text, "sha512");
            }
            other => panic!("expected integrity pin to be parsed, got {other:?}"),
        }
    }

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
