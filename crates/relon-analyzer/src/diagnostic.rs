//! Aggregated diagnostic emitted by analyzer passes.
//!
//! Modeled after `RuntimeError` (miette-friendly) but designed to be
//! *collected* in a `Vec<Diagnostic>` rather than returned via `Result`.
//! That asymmetry is the whole point: an analyzer pass keeps walking the
//! tree even after a problem so the host gets every error from a file in
//! one shot, instead of fixing-rerunning-fixing.

use miette::{Diagnostic as MietteDiagnostic, SourceSpan};
use relon_parser::TokenRange;
use thiserror::Error;

/// Severity of an analyzer diagnostic.
///
/// `Error` blocks evaluation; `Warning` is informational and lets the
/// host decide policy. The analyzer never emits `Error` for issues that
/// the evaluator could legitimately resolve at runtime — only for things
/// that are statically broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Error, MietteDiagnostic)]
pub enum Diagnostic {
    #[error("@schema body must be a Dict (or Schema composition), got {found}")]
    #[diagnostic(
        code(relon::analyze::schema_body_not_dict),
        help("@schema expects `@schema Name: {{ ... }}` or `@schema Name: Base + {{ ... }}`.")
    )]
    SchemaBodyNotDict {
        found: String,
        #[label("not a schema body")]
        range: SourceSpan,
    },

    #[error("@schema field `{field}` is missing a type annotation")]
    #[diagnostic(
        code(relon::analyze::schema_field_untyped),
        help("Each schema field needs a type prefix, e.g. `String name: *` or `Int port: (p) => p > 0`.")
    )]
    SchemaFieldUntyped {
        field: String,
        #[label("type required here")]
        range: SourceSpan,
    },

    #[error("reference `{name}` does not match any field in scope")]
    #[diagnostic(
        code(relon::analyze::unresolved_reference),
        help("The analyzer couldn't find a sibling/root field with this name. If it's added by a spread or computed at runtime this warning may be a false positive.")
    )]
    UnresolvedReference {
        name: String,
        #[label("unresolved")]
        range: SourceSpan,
    },

    #[error(
        "static type mismatch in schema field `{field}`: expected {expected}, value is {found}"
    )]
    #[diagnostic(
        code(relon::analyze::static_type_mismatch),
        help("This binding's value can be classified at parse time and disagrees with the schema's declared type. The runtime check would also fail.")
    )]
    StaticTypeMismatch {
        field: String,
        expected: String,
        found: String,
        #[label("type doesn't match `{expected}`")]
        range: SourceSpan,
    },
}

impl Diagnostic {
    pub fn severity(&self) -> Severity {
        match self {
            // Structurally broken: the program can't proceed.
            Diagnostic::SchemaBodyNotDict { .. } | Diagnostic::SchemaFieldUntyped { .. } => {
                Severity::Error
            }
            // Informational: the analyzer's view is conservative — a
            // spread, closure binding, or runtime-computed field may
            // still resolve, so we don't gate evaluation on it.
            Diagnostic::UnresolvedReference { .. } => Severity::Warning,
            // Static type mismatches are warnings (not errors) so the
            // host can still try to evaluate. Runtime type-checking
            // produces the authoritative error if the binding actually
            // executes.
            Diagnostic::StaticTypeMismatch { .. } => Severity::Warning,
        }
    }
}

/// Convenience: convert a parser `TokenRange` into the `SourceSpan`
/// miette wants for `#[label]` fields.
pub fn span_of(range: TokenRange) -> SourceSpan {
    SourceSpan::from(range)
}
