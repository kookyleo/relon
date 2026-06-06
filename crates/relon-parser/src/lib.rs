//! Relon parser.
//!
//! Two public entry points cover the full surface:
//!
//! * [`parse_document`] — strict-parse. Returns
//!   `Result<Node, ParseDocumentError>` and rejects any input that
//!   doesn't form a complete document. Use it from the analyzer's
//!   main entry, the evaluator, the formatter, and the CLI — they
//!   all want a hard fail on broken input.
//! * [`parse_document_recovering`] — IDE-facing partial-AST entry.
//!   Returns a [`ParsedDocument`] (partial AST + diagnostics) that
//!   is always populated, even on completely broken input. Use it
//!   from completion / hover / goto-def callers that must keep
//!   offering features while the user is mid-edit (`#`, `&`, `@`,
//!   `{a:`, ...).
//!
//! Internals:
//!
//! * [`cst`] / [`syntax`] — rowan CST, the single source of truth
//!   for what input the parser accepts.
//! * [`ast`] — typed wrappers over the CST nodes.
//! * `lower` — CST → legacy [`Node`] / [`Expr`] / [`TokenKey`]
//!   tree. The legacy tree is still public because the analyzer and
//!   evaluator depend on its semantic shape; new consumers should
//!   prefer the [`ast`] wrappers (cheap, ranged, error-tolerant).

#![forbid(unsafe_code)]

pub mod ast;
pub mod cst;
pub mod directive;
pub mod fast_path;
pub mod lex;
// `lower` is an implementation detail: it owns the CST → legacy `Node`
// translation that backs `parse_document` / `parse_document_recovering`.
// Downstream callers should only depend on those entry points (and the
// resulting `Node` / `Expr` tree); the lowering walker, partial-recovery
// scope guard, and offset-translation helpers are subject to change as
// the rowan rewrite continues and are deliberately not part of the
// public API surface.
pub(crate) mod lower;
pub mod rewrite;
pub mod syntax;
pub mod token;

pub use fast_path::parse_document_fast;

pub use token::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseDocumentError {
    Parse { offset: usize, message: String },
    TrailingInput { offset: usize, remaining: String },
}

impl std::fmt::Display for ParseDocumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse { message, .. } => write!(f, "parse error: {message}"),
            Self::TrailingInput { offset, remaining } => {
                write!(f, "trailing input at byte {offset}: {remaining:?}")
            }
        }
    }
}

impl std::error::Error for ParseDocumentError {}

impl ParseDocumentError {
    pub fn source_span(&self) -> Option<miette::SourceSpan> {
        match self {
            Self::Parse { offset, .. } => Some((*offset, 1).into()),
            Self::TrailingInput { offset, remaining } => {
                Some((*offset, remaining.len().max(1)).into())
            }
        }
    }
}

/// Parse a Relon document into the legacy [`Node`] tree.
///
/// The entry point routes every call through the rowan CST
/// ([`cst::parse_cst`]) first, then hands off to
/// `lower::lower_document` for the typed-tree construction. The
/// CST is the single source of truth for what input the parser
/// accepts; downstream consumers (analyzer / evaluator / fmt / wasm
/// / lsp / cli) keep seeing the same `Node` / `Expr` shape they did
/// pre-rowan-rewrite. See the `lower` module for the migration design note.
///
/// This is the strict-parsing entry point — any CST error or
/// lowering failure surfaces as a typed [`ParseDocumentError`]. Use
/// it from the analyzer's main entry, the evaluator, fmt, and the
/// CLI where the caller wants a hard fail on broken input. For IDE
/// features (completion, hover, goto-def) that must tolerate
/// in-progress edits, prefer [`parse_document_recovering`] — it
/// never returns `Err` and yields a partial [`ParsedDocument`] +
/// diagnostics.
pub fn parse_document(source: &str) -> Result<Node, ParseDocumentError> {
    let parse = cst::parse_cst(source);
    lower::lower_document(&parse, source)
}

/// One span-bearing diagnostic from a partial parse. Emitted by
/// [`parse_document_recovering`] for every CST recovery point plus
/// any sub-tree the lowering walker could not turn into a [`Node`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDiagnostic {
    pub message: String,
    pub range: TokenRange,
}

/// Result of [`parse_document_recovering`]. Always populated — even
/// completely unrecoverable input yields an empty `nodes` + a
/// non-empty `diagnostics` list.
#[derive(Debug, Clone)]
pub struct ParsedDocument {
    /// Top-level nodes successfully lowered from the CST. Empty if
    /// the CST root is unrecoverable; otherwise contains as many
    /// partial nodes as the lowering could produce. A clean Relon
    /// document yields exactly one element — the legacy single-root
    /// `Node` — but the API shape is `Vec<_>` for forward
    /// compatibility with future multi-top-level forms.
    pub nodes: Vec<Node>,
    /// Span-bearing diagnostics describing why parsing was
    /// incomplete. Empty iff the source parsed cleanly.
    pub diagnostics: Vec<ParseDiagnostic>,
}

/// Parse `source` into a partial AST + diagnostics. Never returns
/// `Err` for any byte input — even completely broken sources
/// produce an empty `nodes` vec + a populated `diagnostics` list.
///
/// This is the IDE entry point: use it from completion / hover /
/// goto-def call sites that need to keep offering features while
/// the user is mid-edit (`#`, `&`, `@`, `{`, `{a:`, `{ ?`, ...).
/// The strict counterpart [`parse_document`] still surfaces a hard
/// `Err` for callers that require well-formed input (evaluator,
/// fmt, CLI, analyzer main entry).
///
/// Implementation: routes through [`cst::parse_cst`] (which never
/// panics) and then walks the resulting CST, lowering every
/// top-level expression child via `lower::lower_document_node_v2`.
/// CST `ERROR` spans + lowering misses are collected as
/// [`ParseDiagnostic`]s with byte-accurate ranges.
pub fn parse_document_recovering(source: &str) -> ParsedDocument {
    let _scope = lower::RecoveringScope::enter();
    let parse = cst::parse_cst(source);
    let mut nodes: Vec<Node> = Vec::new();
    let mut diagnostics: Vec<ParseDiagnostic> = Vec::new();

    // Surface every CST parser error as a diagnostic. Each one
    // carries a byte offset into the original source; we widen the
    // range to a 1-byte span so the IDE has something to anchor to.
    for err in &parse.errors {
        let end = (err.offset + 1).min(source.len().max(err.offset));
        diagnostics.push(ParseDiagnostic {
            message: err.message.clone(),
            range: lower::range_from_offsets(source, err.offset, end),
        });
    }

    if let Some(doc) = ast::document_of(parse.syntax()) {
        // Lowering yields a single root Node when it succeeds. The
        // partial-tolerant lowering already substitutes placeholders
        // for individual sub-tree failures, so the only remaining
        // `None` case here is when the CST has no recognizable root
        // expression at all (e.g. a lone `@` or `#` with nothing
        // attached). When that happens we still synthesize an empty
        // top-level Node so IDE callers can attach completion to
        // whatever cursor context they have.
        if doc.root_expr().is_some() {
            if let Some(node) = lower::lower_document_node_v2(&doc, source) {
                nodes.push(node);
            } else {
                // Defensive: with partial-tolerant lowering this should
                // never trigger, but keep a synthesized empty root and
                // a diagnostic on the document range so the IDE has
                // something to attach to.
                let end_offset = source.len();
                nodes.push(Node {
                    id: NodeId::alloc(),
                    expr: std::sync::Arc::new(Expr::Null),
                    decorators: Vec::new(),
                    directives: Vec::new(),
                    type_hint: None,
                    range: lower::range_from_offsets(source, 0, end_offset),
                    doc_comment: None,
                });
                if parse.errors.is_empty() {
                    diagnostics.push(ParseDiagnostic {
                        message: "could not lower CST to legacy Node".to_string(),
                        range: lower::range_from_offsets(source, 0, end_offset),
                    });
                }
            }
        } else {
            // No root expression slot — e.g. a lone `@`, `#`, or
            // similar prefix the user is still typing. Synthesize an
            // empty placeholder root so completion still has a
            // navigable AST hook to dispatch off; the diagnostics list
            // already explains the missing piece.
            let end_offset = source.len();
            nodes.push(Node {
                id: NodeId::alloc(),
                expr: std::sync::Arc::new(Expr::Null),
                decorators: Vec::new(),
                directives: Vec::new(),
                type_hint: None,
                range: lower::range_from_offsets(source, 0, end_offset),
                doc_comment: None,
            });
            if parse.errors.is_empty() {
                diagnostics.push(ParseDiagnostic {
                    message: "empty document".to_string(),
                    range: lower::range_from_offsets(source, 0, 0),
                });
            }
        }
    }

    ParsedDocument { nodes, diagnostics }
}

/// Extract leading comments as a single doc string. Walks the byte
/// prefix of `source` consuming whitespace + `//` line / `/* */`
/// block comments until the first non-trivia byte. Returns the
/// joined doc-comment text (if any) plus the number of bytes
/// consumed — callers use the count to advance their cursor.
pub fn parse_leading_comments(source: &str) -> (Option<String>, usize) {
    let bytes = source.as_bytes();
    let mut i = 0;
    let mut comments: Vec<String> = Vec::new();
    loop {
        // Skip whitespace.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        // Try a `//` line comment.
        if i + 2 <= bytes.len() && &bytes[i..i + 2] == b"//" {
            let start = i + 2;
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'\n' && bytes[end] != b'\r' {
                end += 1;
            }
            comments.push(source[start..end].trim().to_string());
            i = end;
            continue;
        }
        // Try a `/* ... */` block comment.
        if i + 2 <= bytes.len() && &bytes[i..i + 2] == b"/*" {
            let start = i + 2;
            let mut end = start;
            while end + 1 < bytes.len() && !(bytes[end] == b'*' && bytes[end + 1] == b'/') {
                end += 1;
            }
            comments.push(source[start..end].trim().to_string());
            if end + 1 < bytes.len() {
                i = end + 2;
            } else {
                i = bytes.len();
            }
            continue;
        }
        break;
    }
    let joined = if comments.is_empty() {
        None
    } else {
        Some(comments.join("\n"))
    };
    (joined, i)
}

/// Combine two `TokenRange`s — start from `start.start`, end from
/// `end.end`. Used by binary-expression lowering to compute the
/// operand-bounded range.
pub fn combine_ranges(start: TokenRange, end: TokenRange) -> TokenRange {
    TokenRange {
        start: start.start,
        end: end.end,
    }
}

pub(crate) fn position_at_source(source: &str, offset: usize) -> TokenPosition {
    let offset = offset.min(source.len());
    let end = if source.is_char_boundary(offset) {
        offset
    } else {
        let mut boundary = offset;
        while boundary > 0 && !source.is_char_boundary(boundary) {
            boundary -= 1;
        }
        boundary
    };

    let mut line = 1u32;
    let mut column = 1usize;
    let mut chars = source[..end].chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                line += 1;
                column = 1;
            }
            '\n' => {
                line += 1;
                column = 1;
            }
            _ => column += 1,
        }
    }

    TokenPosition {
        line,
        column,
        offset,
    }
}

/// Yield the expression-shaped child nodes of `node` for AST walkers
/// (analyzer passes, LSP enclosing-scope lookups, ...). Decorators,
/// directives, and type hints are intentionally *not* included — those
/// have their own dedicated walkers that need different semantics.
pub fn child_nodes(node: &Node) -> Vec<&Node> {
    let mut out = Vec::new();
    match &*node.expr {
        Expr::Dict(pairs) => {
            for (_, value) in pairs {
                out.push(value);
            }
        }
        Expr::List(items) => out.extend(items.iter()),
        Expr::Spread(inner) => out.push(inner),
        Expr::Comprehension {
            element,
            iterable,
            condition,
            ..
        } => {
            out.push(element);
            out.push(iterable);
            if let Some(cond) = condition {
                out.push(cond);
            }
        }
        Expr::Binary(_, l, r) => {
            out.push(l);
            out.push(r);
        }
        Expr::Unary(_, inner) => out.push(inner),
        Expr::Ternary { cond, then, els } => {
            out.push(cond);
            out.push(then);
            out.push(els);
        }
        Expr::FnCall { args, .. } => {
            for arg in args {
                out.push(&arg.value);
            }
        }
        Expr::FString(parts) => {
            for part in parts {
                if let crate::FStringPart::Interpolation(n) = part {
                    out.push(n);
                }
            }
        }
        Expr::Where { expr, bindings } => {
            out.push(expr);
            out.push(bindings);
        }
        Expr::Match { expr, arms } => {
            out.push(expr);
            for (pat, body) in arms {
                out.push(pat);
                out.push(body);
            }
        }
        Expr::Closure { body, .. } => out.push(body),
        Expr::VariantCtor { body, .. } => out.push(body),
        Expr::Reference { .. }
        | Expr::Variable(_)
        | Expr::Type(_)
        | Expr::Wildcard
        | Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::String(_) => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_comments() {
        let src = r##"/* hello world */
// this is a test file
{}"##;
        let node = parse_document(src).unwrap();
        assert!(matches!(*node.expr, Expr::Dict(_)));
    }

    #[test]
    fn test_parse_document_accepts_trailing_trivia() {
        assert!(parse_document("{ a: 1 } // trailing\n /* ok */").is_ok());
    }

    #[test]
    fn test_parse_document_rejects_trailing_tokens() {
        let err = parse_document("{ a: 1 } true").unwrap_err();
        assert!(matches!(
            err,
            ParseDocumentError::TrailingInput {
                offset: 9,
                ref remaining
            } if remaining == "true"
        ));
        assert_eq!(err.source_span(), Some((9, 4).into()));
    }

    #[test]
    fn test_parse_document_reports_parse_error_span() {
        let err = parse_document("{ a: }").unwrap_err();
        assert!(matches!(err, ParseDocumentError::Parse { .. }));
        assert!(err.source_span().is_some());
    }

    #[test]
    fn test_token_range_has_line_and_column() {
        let node = parse_document("// leading\n{\n  answer: 42\n}\n").unwrap();
        assert_eq!(node.range.start.line, 2);
        assert_eq!(node.range.start.column, 1);
        assert_eq!(node.range.end.line, 4);
        assert_eq!(node.range.end.column, 2);

        if let Expr::Dict(pairs) = &*node.expr {
            let TokenKey::String(_, key_range, _) = &pairs[0].0 else {
                panic!("Expected string key")
            };
            assert_eq!(key_range.start.line, 3);
            assert_eq!(key_range.start.column, 3);
            assert_eq!(pairs[0].1.range.start.line, 3);
            assert_eq!(pairs[0].1.range.start.column, 11);
        } else {
            panic!("Expected dict")
        }
    }

    #[test]
    fn test_simple_root() {
        let node = parse_document(r#"{ "a": 1 }"#).unwrap();
        if let Expr::Dict(pairs) = &*node.expr {
            assert_eq!(pairs.len(), 1);
        } else {
            panic!()
        }

        let node = parse_document("// comment \n {foo: 1, bar: 2,}").unwrap();
        if let Expr::Dict(pairs) = &*node.expr {
            assert_eq!(pairs.len(), 2);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_expr_integration() {
        let node = parse_document(r#"{ "a": 1 != 2 }"#).unwrap();
        if let Expr::Dict(pairs) = &*node.expr {
            assert!(matches!(*pairs[0].1.expr, Expr::Binary(Operator::Ne, _, _)));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_comment_decorator_integration() {
        let node = parse_document(
            r###"
                // foo decorator
                @foo
                { "a": 1 }"###,
        )
        .unwrap();
        assert_eq!(node.decorators.len(), 1);
        assert_eq!(node.decorators[0].path[0].to_string_key(), "foo");
    }

    #[test]
    fn test_list_integration() {
        let node = parse_document(r#"[1, 2, 3]"#).unwrap();
        if let Expr::List(elements) = &*node.expr {
            assert_eq!(elements.len(), 3);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_ref_dict() {
        let node = parse_document(r#"{ "a": &sibling.b, "b": 2 }"#).unwrap();
        if let Expr::Dict(pairs) = &*node.expr {
            assert_eq!(pairs.len(), 2);
            assert!(matches!(
                *pairs[0].1.expr,
                Expr::Reference {
                    base: RefBase::Sibling,
                    ..
                }
            ));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_ref_list() {
        let node = parse_document(r#"[&sibling.b[1], 2]"#).unwrap();
        if let Expr::List(elements) = &*node.expr {
            assert_eq!(elements.len(), 2);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_var_list() {
        let node = parse_document(r#"[a, 2]"#).unwrap();
        if let Expr::List(elements) = &*node.expr {
            assert!(matches!(*elements[0].expr, Expr::Variable(_)));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_fn_call_list() {
        let node = parse_document(r#"[f({a: 1}), 2]"#).unwrap();
        if let Expr::List(elements) = &*node.expr {
            assert!(matches!(*elements[0].expr, Expr::FnCall { .. }));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_fmt_string_list() {
        let node = parse_document(r#"[f"a ${ &sibling.b[1] }", "b"]"#).unwrap();
        if let Expr::List(elements) = &*node.expr {
            assert!(matches!(*elements[0].expr, Expr::FString(_)));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_root_ref_in_fmt_string_dict() {
        assert!(parse_document(r#"{ "a": f"a ${ &root.b[0] }", "b": [0, 1] }"#).is_ok());
    }

    #[test]
    fn test_doc_comment_extraction() {
        let src = r#"{
            // line 1
            // line 2
            a: 1,
            /* block */
            b: 2
        }"#;
        let node = parse_document(src).unwrap();
        if let Expr::Dict(pairs) = &*node.expr {
            assert_eq!(pairs[0].1.doc_comment.as_deref(), Some("line 1\nline 2"));
            assert_eq!(pairs[1].1.doc_comment.as_deref(), Some("block"));
        } else {
            panic!()
        }
    }

    /// v1.2: root may be any expression, not just `dict` / `list`
    /// literals — the parser accepts atomic / variant / arithmetic /
    /// fn-call roots as well.
    #[test]
    fn test_root_accepts_atomic_literals() {
        let node = parse_document("42").unwrap();
        assert!(matches!(*node.expr, Expr::Int(42)));

        let node = parse_document(r#""hello""#).unwrap();
        assert!(matches!(*node.expr, Expr::String(_)));

        let node = parse_document("true").unwrap();
        assert!(matches!(*node.expr, Expr::Bool(true)));

        let node = parse_document("null").unwrap();
        assert!(matches!(*node.expr, Expr::Null));
    }

    #[test]
    fn test_root_accepts_binary_expression() {
        let node = parse_document("1 + 2").unwrap();
        assert!(matches!(*node.expr, Expr::Binary(Operator::Add, _, _)));
    }

    #[test]
    fn test_root_accepts_variant_constructor() {
        let node = parse_document("Result.Ok { value: 1 }").unwrap();
        assert!(matches!(*node.expr, Expr::VariantCtor { .. }));
    }

    #[test]
    fn test_root_accepts_fn_call() {
        let node = parse_document("range(0, 10)").unwrap();
        assert!(matches!(*node.expr, Expr::FnCall { .. }));
    }

    /// Pre-v1.2 root forms (dict / list literals) must keep parsing
    /// — v1.2 is a strict superset.
    #[test]
    fn test_root_dict_and_list_still_work() {
        let node = parse_document("{ a: 1 }").unwrap();
        assert!(matches!(*node.expr, Expr::Dict(_)));

        let node = parse_document("[1, 2, 3]").unwrap();
        assert!(matches!(*node.expr, Expr::List(_)));
    }

    /// Truly invalid input is still rejected — v1.2 widens the root
    /// shape but does not weaken parser strictness elsewhere.
    #[test]
    fn test_root_rejects_garbage() {
        assert!(parse_document("").is_err());
        assert!(parse_document("   \n\t  ").is_err());
        assert!(parse_document("{ bad syntax").is_err());
    }

    // -----------------------------------------------------------------
    // parse_document_recovering — IDE-facing partial-AST entry point.
    // -----------------------------------------------------------------

    #[test]
    fn recovering_clean_input_yields_one_node_no_diagnostics() {
        let result = parse_document_recovering("{ a: 1, b: 2 }");
        assert_eq!(result.nodes.len(), 1);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        if let Expr::Dict(pairs) = &*result.nodes[0].expr {
            assert_eq!(pairs.len(), 2);
        } else {
            panic!("expected Dict root");
        }
    }

    #[test]
    fn recovering_never_errs_on_partial_inputs() {
        // Every one of these inputs would force `parse_document` to
        // surface an `Err`; the recovering API must absorb each one.
        for src in &[
            "#", "&", "@", "{", "{a:", "{ ?", "}", "[", "(", "f\"hi ${", "", "   ", "\n\t",
        ] {
            let result = parse_document_recovering(src);
            // We only assert: never panics, never crashes. Diagnostics
            // may or may not be populated — empty input is its own
            // edge case.
            let _ = result.nodes;
            let _ = result.diagnostics;
        }
    }

    #[test]
    fn recovering_reports_diagnostic_for_unterminated_dict() {
        let result = parse_document_recovering("{ a: ");
        assert!(
            !result.diagnostics.is_empty(),
            "expected at least one diagnostic for unterminated dict"
        );
        // The span should fall within the source.
        for diag in &result.diagnostics {
            assert!(
                diag.range.start.offset <= 5,
                "diagnostic offset out of range: {:?}",
                diag
            );
        }
    }

    #[test]
    fn recovering_includes_empty_document_diagnostic() {
        let result = parse_document_recovering("");
        // Partial-tolerant contract: every input yields at least one
        // navigable root Node — empty source gets a Null placeholder.
        assert_eq!(result.nodes.len(), 1);
        assert!(matches!(&*result.nodes[0].expr, Expr::Null));
        // Either a CST error or our own "empty document" — but
        // diagnostics MUST be non-empty (the caller has nothing to
        // attach a "you must write something" hint to otherwise).
        assert!(!result.diagnostics.is_empty());
    }

    #[test]
    fn recovering_completes_partial_for_lone_hash() {
        // The IDE feeds us `#` to look up directive completions.
        // We must yield a diagnostic + leave the byte-offset usable.
        let result = parse_document_recovering("#");
        assert!(!result.diagnostics.is_empty());
    }

    #[test]
    fn recovering_completes_partial_for_lone_amp() {
        let result = parse_document_recovering("&");
        assert!(!result.diagnostics.is_empty());
    }

    #[test]
    fn recovering_always_yields_at_least_one_node() {
        // Every input the IDE can hand us — including completely
        // broken prefixes — must produce a non-empty `nodes` vec so
        // downstream completion has an AST root to dispatch off.
        for src in [
            "@",
            "#",
            "&",
            "{",
            "{ @",
            "{ x: 1, @ }",
            "[",
            "}",
            "{ a:",
            "{ ?",
            "f\"hi ${",
            "(",
            "",
        ] {
            let r = parse_document_recovering(src);
            assert!(
                !r.nodes.is_empty(),
                "expected at least one partial node for src {:?}, got 0",
                src
            );
        }
    }

    #[test]
    fn recovering_at_decorator_keeps_sibling_fields() {
        // The smoking-gun case the user filed: a stray `@` between
        // dict fields should NOT erase the entire dict; the well-
        // formed siblings must survive so completion can walk them
        // for decorator suggestions.
        let r = parse_document_recovering("{ fmt: (v) => v + 1, @ y: 2 }");
        assert_eq!(r.nodes.len(), 1, "expected partial Dict root");
        match &*r.nodes[0].expr {
            Expr::Dict(fields) => {
                let has_fmt = fields.iter().any(|(k, _)| {
                    matches!(
                        k,
                        TokenKey::String(s, _, _) if s == "fmt"
                    )
                });
                assert!(
                    has_fmt,
                    "expected the `fmt` sibling to survive partial lowering, got {:?}",
                    fields
                );
            }
            other => panic!("expected Dict root, got {:?}", other),
        }
    }
}
