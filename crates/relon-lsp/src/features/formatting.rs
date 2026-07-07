//! `textDocument/formatting`.
//!
//! Full-document formatting through `relon-fmt`. The formatter is
//! canonical (gofmt-style): fixed four-space indent, deterministic
//! layout. Client-supplied `FormattingOptions` (tab size, spaces vs
//! tabs) are therefore intentionally ignored.
//!
//! Unlike the other feature modules this one takes the raw source
//! rather than a `DocumentEntry`: `relon_fmt::format_source` runs its
//! own strict parse, so the cached recovering-parse AST is irrelevant
//! here.
//!
//! ## Documents with syntax errors
//!
//! `relon_fmt::format_source` strict-parses its input and returns
//! `Error::Parse` for any syntax error (verified empirically — e.g.
//! `"{ a: }"` → `parse error: expected expression`, and the empty
//! document is also a parse error). Following the rustfmt /
//! rust-analyzer convention we refuse to format a document that does
//! not parse: return `None` (a null LSP result, i.e. no edits) instead
//! of emitting a half-broken rewrite. The user keeps their buffer
//! untouched and the parse diagnostics already published for the
//! document explain why nothing happened.

use crate::position::offset_to_position;
use lsp_types::{Position, Range, TextEdit};

/// Format the whole document. Returns a single full-document
/// replacement `TextEdit` (the common, robust LSP shape), `None` when
/// the source has syntax errors or is already canonically formatted.
pub fn compute(source: &str) -> Option<Vec<TextEdit>> {
    let formatted = relon_fmt::format_source(source).ok()?;
    if formatted == source {
        // Already canonical — no edits. Returning `None` (instead of
        // an identity edit) keeps well-behaved clients from marking
        // the buffer dirty on format-on-save.
        return None;
    }
    Some(vec![TextEdit {
        range: Range {
            start: Position::new(0, 0),
            end: offset_to_position(source, source.len()),
        },
        new_text: formatted,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_unformatted_document_with_full_replacement_edit() {
        let src = "{a:1,b:2}";
        let edits = compute(src).expect("valid source must produce edits");
        assert_eq!(edits.len(), 1, "single full-document edit expected");
        let edit = &edits[0];
        assert_eq!(edit.new_text, "{\n    a: 1,\n    b: 2\n}\n");
        assert_eq!(edit.range.start, Position::new(0, 0));
        // End of a single-line source: line 0, character = source length.
        assert_eq!(edit.range.end, Position::new(0, src.len() as u32));
    }

    #[test]
    fn edit_range_covers_multiline_document() {
        let src = "{\n  a: 1,\n      b: 2\n}\n";
        let edits = compute(src).expect("valid source must produce edits");
        assert_eq!(edits.len(), 1);
        // Trailing newline puts the range end at the start of line 4.
        assert_eq!(edits[0].range.start, Position::new(0, 0));
        assert_eq!(edits[0].range.end, Position::new(4, 0));
    }

    #[test]
    fn already_formatted_document_yields_no_edits() {
        let src = "{a:1,b:2}";
        let canonical = compute(src).expect("valid source")[0].new_text.clone();
        assert_eq!(
            compute(&canonical),
            None,
            "canonical output must be a fixed point"
        );
    }

    #[test]
    fn syntax_error_yields_no_edits() {
        // rustfmt convention: refuse to format what doesn't parse.
        assert_eq!(compute("{ a: }"), None);
        assert_eq!(compute("{ a: 1 xyz"), None);
    }

    #[test]
    fn empty_document_yields_no_edits() {
        // relon-fmt treats the empty document as a parse error.
        assert_eq!(compute(""), None);
    }
}
