//! Convert `relon-analyzer` diagnostics into LSP-shaped diagnostics.
//!
//! Kept apart from the server loop so the conversion can be tested
//! without spinning up a transport. Position math is done against the
//! original source string (`source`) to translate byte offsets back to
//! line/column pairs the LSP client can render.

use crate::position::offset_to_position;
use lsp_types::{Diagnostic as LspDiagnostic, DiagnosticSeverity, NumberOrString, Range};
// Bring the miette `Diagnostic` trait into scope so we can call
// `.code()` / `.labels()` on the analyzer's `Diagnostic` enum, which
// derives the trait. The `as _` alias avoids a name clash with our
// `relon_analyzer::Diagnostic` re-export.
use miette::Diagnostic as _;
use miette::SourceSpan;
use relon_analyzer::{Diagnostic, Severity};

/// Map analyzer-level severity to LSP severity. `Warning` maps to LSP
/// warning, `Error` to LSP error.
fn map_severity(sev: Severity) -> DiagnosticSeverity {
    match sev {
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
    }
}

/// Convert one analyzer diagnostic against `source`. The source string
/// is needed because LSP positions are line/column, not byte offsets,
/// and the analyzer carries positions as `SourceSpan`.
pub fn to_lsp(diag: &Diagnostic, source: &str) -> LspDiagnostic {
    let span = primary_span(diag);
    let range = span_to_range(span, source);
    LspDiagnostic {
        range,
        severity: Some(map_severity(diag.severity())),
        code: diag.code().map(|c| NumberOrString::String(c.to_string())),
        code_description: None,
        source: Some("relon".to_string()),
        message: diag.to_string(),
        related_information: None,
        tags: None,
        data: None,
    }
}

/// Walk a list of analyzer diagnostics, returning LSP diagnostics in
/// the same order.
pub fn batch_to_lsp(diags: &[Diagnostic], source: &str) -> Vec<LspDiagnostic> {
    diags.iter().map(|d| to_lsp(d, source)).collect()
}

/// Pull the first `SourceSpan` out of a diagnostic. Every analyzer
/// diagnostic currently has exactly one labelled span, but the API is
/// expressed via `miette::Diagnostic::labels` so we degrade gracefully.
fn primary_span(diag: &Diagnostic) -> SourceSpan {
    if let Some(mut labels) = diag.labels() {
        if let Some(label) = labels.next() {
            return *label.inner();
        }
    }
    // Fallback: an empty span at the start of the file. Should not
    // happen with the analyzer's current diagnostics.
    SourceSpan::from((0, 0))
}

/// Convert a byte-offset `SourceSpan` to an LSP line/column range.
fn span_to_range(span: SourceSpan, source: &str) -> Range {
    let start = span.offset();
    let end = start + span.len();
    Range {
        start: offset_to_position(source, start),
        end: offset_to_position(source, end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_analyzer::analyze;
    use relon_parser::parse_document;

    fn first_diag(src: &str) -> (Diagnostic, String) {
        let node = parse_document(src).unwrap();
        let mut tree = analyze(&node);
        let diag = tree.diagnostics.remove(0);
        (diag, src.to_string())
    }

    #[test]
    fn converts_schema_body_not_dict_to_lsp_error() {
        let (diag, source) = first_diag("{ #schema Bad 42 }");
        let lsp = to_lsp(&diag, &source);
        assert_eq!(lsp.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(lsp.source.as_deref(), Some("relon"));
        // "42" starts at column 16 (line 0).
        assert_eq!(lsp.range.start.line, 0);
        assert!(lsp.range.start.character > 0);
    }

    #[test]
    fn maps_severities() {
        assert_eq!(map_severity(Severity::Error), DiagnosticSeverity::ERROR);
        assert_eq!(map_severity(Severity::Warning), DiagnosticSeverity::WARNING);
    }

    #[test]
    fn position_handles_multiline_source() {
        let source = "ab\ncd\nef";
        // offset 6 = 'e' on line 2, col 0
        let pos = offset_to_position(source, 6);
        assert_eq!(pos.line, 2);
        assert_eq!(pos.character, 0);
    }
}
