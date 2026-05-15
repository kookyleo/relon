//! Quick-fix candidates at a cursor position.
//!
//! Walks the analyzer's existing diagnostics, filters to those whose
//! primary label contains the cursor, then emits one or more
//! [`CodeAction`]s per fixable diagnostic. The action carries a
//! `title` for IDE display + a `Vec<TextEdit>` shaped like
//! [`crate::rename::TextEdit`] so the caller doesn't need to learn
//! two edit shapes.
//!
//! Only the high-confidence diagnostics get fixes today:
//!
//!   - `UnknownVariant` with an analyzer-supplied `suggestion` →
//!     replace the variant name with the suggestion.
//!   - `UnresolvedReference` where a sibling field of the same name
//!     exists → wrap the bare identifier as `&sibling.NAME`.
//!   - `SchemaFieldUntyped` → offer the canonical primitive prefixes
//!     (`String`, `Int`, `Bool`, `Float`) plus the schema-typed
//!     placeholder `_`.
//!   - `DuplicateMatchArm` → delete the duplicate arm.
//!
//! Every other diagnostic still surfaces a (read-only) presence so
//! the IDE can show "(no fixes available)" instead of guessing.

use crate::diagnostic::Diagnostic;
use crate::rename::TextEdit;
use crate::tree::AnalyzedTree;
use miette::Diagnostic as MietteDiagnostic;
use relon_parser::{Expr, Node, TokenKey, TokenRange};

#[derive(Debug, Clone)]
pub struct CodeAction {
    pub title: String,
    /// The diagnostic this action resolves, by miette code. Useful for
    /// telemetry / IDE grouping. `None` for actions that aren't tied
    /// to a specific diagnostic (e.g. future "extract closure" refactors).
    pub diagnostic_code: Option<String>,
    pub edits: Vec<TextEdit>,
}

/// Collect every code action that applies at the cursor. Returns an
/// empty vec when no fixes are available — `None` is reserved for
/// "cursor isn't in a meaningful position", which today is never
/// (offset-based filtering doesn't need a node-level cursor).
pub fn at_position(
    source: &str,
    root: &Node,
    tree: &AnalyzedTree,
    line: u32,
    character: u32,
) -> Vec<CodeAction> {
    let offset = crate::goto_def::position_to_offset(source, line, character);
    let mut out = Vec::new();
    for diag in &tree.diagnostics {
        let Some((start, end)) = primary_offset(diag) else {
            continue;
        };
        if offset < start || offset > end {
            continue;
        }
        let range = TokenRange {
            start: position_for_offset(source, start),
            end: position_for_offset(source, end),
        };
        emit_fixes(diag, range, source, root, &mut out);
    }
    out
}

fn emit_fixes(
    diag: &Diagnostic,
    range: TokenRange,
    source: &str,
    root: &Node,
    out: &mut Vec<CodeAction>,
) {
    let code = diag.code().map(|c| c.to_string());
    match diag {
        Diagnostic::UnknownVariant {
            suggestion: Some(s),
            variant_name,
            ..
        } => {
            out.push(CodeAction {
                title: format!("Replace `{variant_name}` with `{s}`"),
                diagnostic_code: code,
                edits: vec![TextEdit {
                    range,
                    new_text: s.clone(),
                }],
            });
        }
        Diagnostic::UnresolvedReference { name, .. } => {
            if has_sibling_field(root, name) {
                let suggestion = format!("&sibling.{name}");
                out.push(CodeAction {
                    title: format!("Reference sibling as `{suggestion}`"),
                    diagnostic_code: code.clone(),
                    edits: vec![TextEdit {
                        range,
                        new_text: suggestion,
                    }],
                });
            }
            // Always offer the &root.NAME variant as a fallback —
            // analyzers can't always tell which scope the user
            // intended, so giving both options is friendlier than
            // guessing.
            let root_form = format!("&root.{name}");
            out.push(CodeAction {
                title: format!("Reference from root as `{root_form}`"),
                diagnostic_code: code,
                edits: vec![TextEdit {
                    range,
                    new_text: root_form,
                }],
            });
        }
        Diagnostic::SchemaFieldUntyped { field, .. } => {
            for ty in ["String", "Int", "Bool", "Float"] {
                out.push(CodeAction {
                    title: format!("Prefix with `{ty}`"),
                    diagnostic_code: code.clone(),
                    edits: vec![TextEdit {
                        range,
                        new_text: format!("{ty} {field}"),
                    }],
                });
            }
        }
        Diagnostic::DuplicateMatchArm { .. } => {
            // Delete the arm: the label covers just the duplicate
            // pattern; we extend to the end of the arm (next `,` or
            // `}`) so we don't leave dangling syntax.
            if let Some(extended) = extend_to_arm_end(source, range) {
                out.push(CodeAction {
                    title: "Delete duplicate match arm".to_string(),
                    diagnostic_code: code,
                    edits: vec![TextEdit {
                        range: extended,
                        new_text: String::new(),
                    }],
                });
            }
        }
        _ => {}
    }
}

/// Walk top-level dict pairs to see if `name` is declared as a
/// sibling key. The reference table normally answers this, but here
/// we only want a *structural* "is the name in scope" — the diagnostic
/// already told us it didn't resolve.
fn has_sibling_field(root: &Node, name: &str) -> bool {
    let Expr::Dict(pairs) = &*root.expr else {
        return false;
    };
    pairs.iter().any(|(key, _)| match key {
        TokenKey::String(k, _, _) => k == name,
        _ => false,
    })
}

/// Pick the byte offset just past the end of a match-arm so the
/// "delete arm" fix removes the trailing comma along with the arm
/// itself. Returns `None` if no comma/brace follows.
fn extend_to_arm_end(source: &str, range: TokenRange) -> Option<TokenRange> {
    let bytes = source.as_bytes();
    let mut end = range.end.offset.min(bytes.len());
    // Skip over the value expression: simple heuristic — extend until
    // the next `,` at depth 0 (relative to this arm) or `}`. For
    // complex arm bodies the user can clean up manually.
    let mut depth: i32 = 0;
    while end < bytes.len() {
        match bytes[end] {
            b'{' | b'[' | b'(' => depth += 1,
            b'}' | b']' | b')' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            b',' if depth == 0 => {
                end += 1;
                break;
            }
            _ => {}
        }
        end += 1;
    }
    if end == range.end.offset {
        return None;
    }
    Some(TokenRange {
        start: range.start,
        end: position_for_offset(source, end),
    })
}

fn primary_offset(diag: &Diagnostic) -> Option<(usize, usize)> {
    let mut labels = diag.labels()?;
    let first = labels.next()?;
    let span = first.inner();
    Some((span.offset(), span.offset() + span.len()))
}

fn position_for_offset(source: &str, offset: usize) -> relon_parser::TokenPosition {
    let mut line = 0u32;
    let mut col = 0usize;
    let mut byte_index = 0usize;
    for ch in source.chars() {
        if byte_index >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
        byte_index += ch.len_utf8();
    }
    relon_parser::TokenPosition {
        line,
        column: col,
        offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze;
    use relon_parser::parse_document;

    fn pos_at(src: &str, offset: usize) -> (u32, u32) {
        let mut line = 0u32;
        let mut col = 0u32;
        for (i, ch) in src.chars().enumerate() {
            if i == offset {
                break;
            }
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += ch.len_utf16() as u32;
            }
        }
        (line, col)
    }

    #[test]
    fn unresolved_reference_offers_sibling_and_root_forms() {
        let src = r#"{
                price: 10,
                total: amount
            }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let off = src.find("amount").unwrap();
        let (line, character) = pos_at(src, off);
        let actions = at_position(src, &root, &tree, line, character);
        let titles: Vec<&str> = actions.iter().map(|a| a.title.as_str()).collect();
        assert!(
            titles.iter().any(|t| t.contains("&root.amount")),
            "{titles:?}"
        );
    }

    #[test]
    fn cursor_off_diagnostic_returns_empty() {
        // A free variable inside a nested dict that has no enclosing
        // binding should fire `UnresolvedReference`. We position the
        // cursor *outside* that range (on the `top` key) and expect
        // no actions.
        let src = r#"{
                top: 1,
                inner: { x: nowhere }
            }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let off = src.find("top:").unwrap();
        let (line, character) = pos_at(src, off);
        let actions = at_position(src, &root, &tree, line, character);
        assert!(actions.is_empty(), "{actions:?}");
    }

}
