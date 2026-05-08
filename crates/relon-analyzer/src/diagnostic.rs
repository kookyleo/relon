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
    #[error("#schema body must be a Dict (or Schema composition), got {found}")]
    #[diagnostic(
        code(relon::analyze::schema_body_not_dict),
        help("#schema expects `#schema Name {{ ... }}` or `#schema Name Base + {{ ... }}`.")
    )]
    SchemaBodyNotDict {
        found: String,
        #[label("not a schema body")]
        range: SourceSpan,
    },

    #[error("#schema field `{field}` is missing a type annotation")]
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

    #[error("non-exhaustive match on `{enum_name}`: missing variant(s) `{}`", missing_variants.join("`, `"))]
    #[diagnostic(
        code(relon::analyze::non_exhaustive_match),
        help("Cover every variant of the matched enum, or add a `_` wildcard arm.")
    )]
    NonExhaustiveMatch {
        enum_name: String,
        missing_variants: Vec<String>,
        #[label("missing variants")]
        range: SourceSpan,
    },

    #[error("unknown variant `{variant_name}` of `{enum_name}`")]
    #[diagnostic(
        code(relon::analyze::unknown_variant),
        help("{}", suggestion.as_deref().map(|s| format!("did you mean `{s}`?")).unwrap_or_else(|| format!("the variants of `{enum_name}` are listed in its #schema definition.")))
    )]
    UnknownVariant {
        enum_name: String,
        variant_name: String,
        suggestion: Option<String>,
        #[label("not a variant of `{enum_name}`")]
        range: SourceSpan,
    },

    #[error("duplicate match arm for variant `{variant_name}` of `{enum_name}`")]
    #[diagnostic(
        code(relon::analyze::duplicate_match_arm),
        help("Each variant should appear at most once in a match expression.")
    )]
    DuplicateMatchArm {
        enum_name: String,
        variant_name: String,
        #[label("duplicate arm")]
        range: SourceSpan,
    },

    #[error("Enum<...> mixes named variants with literal/type alternatives")]
    #[diagnostic(
        code(relon::analyze::heterogeneous_enum),
        help("A tagged Enum's alternatives must all be named variants. Either remove the literal/type members or split into separate Enums.")
    )]
    HeterogeneousEnum {
        #[label("inconsistent enum form")]
        range: SourceSpan,
    },

    #[error(
        "schema field `{field}`: cannot combine an explicit type prefix `{type_prefix}` with `#brand`"
    )]
    #[diagnostic(
        code(relon::analyze::schema_field_brand_conflict),
        help("Both forms declare the field's type — pick one. Either drop the type prefix and keep `#brand`, or drop `#brand` and keep the prefix.")
    )]
    SchemaFieldBrandConflict {
        field: String,
        type_prefix: String,
        #[label("conflicting `#brand` here")]
        range: SourceSpan,
    },

    #[error("schema field `{field}`: `#brand` body must be a type")]
    #[diagnostic(
        code(relon::analyze::schema_field_brand_invalid_arg),
        help(
            "Pass a type expression: `#brand Weather`, `#brand geo.Location`, `#brand \"Weather\"`, or a generic like `#brand Map<String, Int>`."
        )
    )]
    SchemaFieldBrandInvalidArg {
        field: String,
        #[label("not a type")]
        range: SourceSpan,
    },

    #[error("duplicate `#main(...)` directive")]
    #[diagnostic(
        code(relon::analyze::duplicate_main_directive),
        help(
            "A file may declare at most one `#main(...)` entry signature. Combine the parameter lists into a single `#main(A a, B b, ...)`."
        )
    )]
    DuplicateMainDirective {
        #[label("first declared here")]
        first: SourceSpan,
        #[label("redeclared")]
        second: SourceSpan,
    },

    #[error("duplicate root-level `#schema` name `{name}`")]
    #[diagnostic(
        code(relon::analyze::duplicate_root_schema_name),
        help(
            "Each root-level `#schema Name Body` entry must have a unique name. Pick distinct names so the merged schema scope is unambiguous."
        )
    )]
    DuplicateRootSchemaName {
        name: String,
        #[label("first declared here")]
        first: SourceSpan,
        #[label("redeclared with the same name")]
        second: SourceSpan,
    },

    #[error("root-level `#schema {name} : ...` collides with dict-field `#schema {name} : ...`")]
    #[diagnostic(
        code(relon::analyze::root_schema_collides_with_field),
        help(
            "Pick one form: either declare the schema in the root-directive stack as `#schema {name} : ...`, or as a dict-field directive inside the root dict — not both."
        )
    )]
    RootSchemaCollidesWithField {
        name: String,
        #[label("declared at the root-directive level")]
        root_range: SourceSpan,
        #[label("also declared as a dict field")]
        field_range: SourceSpan,
    },

    #[error(
        "root-level `#schema {name} : ...` body must be a Dict or `Enum<...>`, got {found_type}"
    )]
    #[diagnostic(
        code(relon::analyze::root_schema_invalid_value),
        help(
            "The body of a root-level `#schema Name Body` directive must be the schema body itself — either a dict literal `{{ ... }}` or an `Enum<...>` type."
        )
    )]
    RootSchemaInvalidValue {
        name: String,
        found_type: String,
        #[label("not a schema body")]
        range: SourceSpan,
    },
}

impl Diagnostic {
    pub fn severity(&self) -> Severity {
        match self {
            // Structurally broken: the program can't proceed.
            Diagnostic::SchemaBodyNotDict { .. }
            | Diagnostic::SchemaFieldUntyped { .. }
            | Diagnostic::NonExhaustiveMatch { .. }
            | Diagnostic::UnknownVariant { .. }
            | Diagnostic::DuplicateMatchArm { .. }
            | Diagnostic::HeterogeneousEnum { .. }
            | Diagnostic::SchemaFieldBrandConflict { .. }
            | Diagnostic::SchemaFieldBrandInvalidArg { .. }
            | Diagnostic::DuplicateMainDirective { .. }
            | Diagnostic::DuplicateRootSchemaName { .. }
            | Diagnostic::RootSchemaCollidesWithField { .. }
            | Diagnostic::RootSchemaInvalidValue { .. } => Severity::Error,
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
