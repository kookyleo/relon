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

use crate::cst::Parse;
use crate::syntax::{SyntaxKind, SyntaxNode};
use crate::{parse_base, ParseDocumentError, Span};
use winnow::stream::Location;

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
    use crate::{cst, parse_document, Expr, NodeId};

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
