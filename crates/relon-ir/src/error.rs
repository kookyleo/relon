//! Lowering errors surfaced when the analyzer-supplied tree cannot
//! be expressed in the v1.beta IR. Each variant carries enough
//! context to render a single-line user message; richer diagnostics
//! (source spans rendered with `miette`) are out of scope for Phase
//! 1.beta and will land alongside the analyzer-error display
//! pipeline.

use relon_eval_api::layout::LayoutError;
use relon_eval_api::schema_lower::SchemaLowerError;
use relon_parser::{Operator, TokenRange};
use thiserror::Error;

use crate::ir::IrType;

/// Reasons lowering can fail. The IR rejects, on principle, anything
/// outside the Phase 1.beta surface (Int / Float literals, the
/// arithmetic operators, and `#main` parameter references); the
/// remaining variants reflect either missing inputs or future
/// surface that's not yet wired through.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum LoweringError {
    /// The entry module has no `#main(...)` directive on its root
    /// node. v1.beta only lowers entry programs; library modules
    /// (static config, schema-only files) hit this path.
    #[error("no #main directive found in entry module `{module}`")]
    MissingMain {
        /// Canonical id of the module looked up in the workspace.
        module: String,
    },
    /// A `#main` parameter or return type is something other than
    /// `Int` / `Float`. v1.beta restricts the entry signature to
    /// scalar numerics so the binary handshake stays trivial; later
    /// phases extend this to `String` / `Bool` / schemas.
    #[error("unsupported type in #main: `{type_name}` (Phase 1.beta supports Int / Float only)")]
    UnsupportedTypeInMain {
        /// The offending type name as written in source.
        type_name: String,
        /// Source range of the type annotation.
        range: TokenRange,
    },
    /// Encountered an expression shape that v1.beta doesn't lower.
    /// The body of `#main` may only contain integer / float
    /// literals, the five arithmetic operators (`+`, `-`, `*`, `/`,
    /// `%`), and direct references to its declared parameters.
    /// Everything else — dicts, lists, closures, calls, ternaries,
    /// comprehensions, where, match, f-strings, references — is
    /// rejected with this error.
    #[error(
        "unsupported expression `{kind}` in lowering (Phase 1.beta supports Int / Float literals, arithmetic, #main param refs only)"
    )]
    UnsupportedExpr {
        /// Stable debug name of the expression variant (e.g. `"List"`,
        /// `"Closure"`). Matches [`relon_parser::Expr::kind`].
        kind: String,
        /// Source range of the offending node.
        range: TokenRange,
    },
    /// A binary / unary operator outside the arithmetic set (`+`,
    /// `-`, `*`, `/`, `%`). Comparison / logical / concat operators
    /// arrive in later phases.
    #[error("unsupported operator `{op:?}` in lowering")]
    UnsupportedOperator {
        /// The offending operator.
        op: Operator,
        /// Source range of the binary expression node.
        range: TokenRange,
    },
    /// A bare identifier reference whose head doesn't match any
    /// `#main` parameter. v1.beta has no scope beyond `#main`'s
    /// param list (no `where`, no `let`, no top-level bindings).
    #[error("unresolved variable `{name}` in lowering (only #main parameters are in scope)")]
    UnresolvedVariable {
        /// The offending identifier as written in source.
        name: String,
        /// Source range of the variable reference.
        range: TokenRange,
    },
    /// The named module wasn't found in the supplied `WorkspaceTree`.
    /// Surfaced by [`crate::lowering::lower_workspace`] when the
    /// caller-supplied entry id doesn't match any module.
    #[error("entry module `{module}` not found in workspace")]
    EntryModuleNotFound {
        /// The id the caller passed in.
        module: String,
    },
    /// Phase 2.b: a `#main` parameter or return type can't be lowered
    /// to the canonical schema form. Wraps the canonical-side error
    /// (which already knows the field name and the offending type)
    /// so callers can match on it without re-deriving the message.
    #[error(transparent)]
    SchemaLower(#[from] SchemaLowerError),
    /// Phase 2.b: the schema laid out fine canonically but the layout
    /// pass refused to size it (variable-size types, overflow). Wraps
    /// `relon_eval_api::layout::LayoutError` for the same reason as
    /// `SchemaLower`.
    #[error(transparent)]
    Layout(#[from] LayoutError),
    /// Phase 2.c: the `cond` slot of an `if` (ternary) expression
    /// lowered to a non-`Bool` IR type. The codegen path can only
    /// branch on a 0/1 byte; numeric / pointer truthiness is not part
    /// of the surface, so the front end is responsible for inserting
    /// an explicit comparison first.
    #[error("if condition must be Bool, got `{got:?}`")]
    IfConditionNotBool {
        /// The IR type the condition produced.
        got: IrType,
        /// Source range of the `if` (ternary) expression.
        range: TokenRange,
    },
    /// Phase 2.c: the `then` and `else` arms of an `if` (ternary)
    /// expression lowered to incompatible IR types. The wasm `if`
    /// block's result-type slot demands both branches push the same
    /// value type; lowering refuses the body rather than synthesising
    /// a silent coercion.
    #[error("if branches disagree on type: then={then_ty:?}, else={else_ty:?}")]
    IfBranchTypeMismatch {
        /// IR type the `then` branch produced.
        then_ty: IrType,
        /// IR type the `else` branch produced.
        else_ty: IrType,
        /// Source range of the `if` (ternary) expression.
        range: TokenRange,
    },
}
