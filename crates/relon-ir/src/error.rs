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
    /// A `#main` parameter or return type falls outside the structured
    /// buffer-protocol envelope the compiled backends decode. Supported
    /// today: the scalar leaves (`Int` / `Float` / `Bool` / `Null` /
    /// `String`), `List<scalar>`, user-`#schema` structs with scalar
    /// fields, and `List<Schema>`. Explicitly NOT supported (loud cap,
    /// not a silent fallthrough): `Dict<_, _>` params, nested-list
    /// params (`List<List<…>>`), and other deeply-nested composites —
    /// their input decoding has no buffer-protocol decode path yet.
    #[error(
        "unsupported type in #main: `{type_name}` — compiled backends decode scalars, \
         List<scalar>, schema-struct, and List<Schema>; Dict and nested-list params are \
         not yet supported"
    )]
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
    /// Phase 3.b: a dict-literal field type can't be lowered to the
    /// Phase 3.b canonical surface. Either the schema declares a type
    /// the layout pass refuses (e.g. `Option<T>`, `List<String>`) or
    /// the dict carries no static brand — the latter is screened
    /// upstream by the analyzer, this variant is the codegen-side
    /// belt-and-braces.
    #[error("unsupported dict field type `{ty}` in field `{field}` of schema `{schema}`")]
    UnsupportedFieldType {
        /// Schema the offending field belongs to.
        schema: String,
        /// Field name that triggered the error.
        field: String,
        /// Human-readable rendering of the offending type.
        ty: String,
        /// Source range of the field's value expression.
        range: TokenRange,
    },
    /// Phase 3.b: a branded dict literal omitted a field that has no
    /// schema-side default expression, so the lowering pass has no
    /// value to write into the slot.
    #[error("dict literal missing field `{field}` for schema `{schema}` (no default declared)")]
    MissingFieldNoDefault {
        /// Schema name the missing field belongs to.
        schema: String,
        /// Field name with no user-supplied value nor default.
        field: String,
        /// Source range of the dict literal expression.
        range: TokenRange,
    },
    /// Phase 3.b: a dict literal references an unknown brand — the
    /// outer `type_hint` names a schema not present in the analyzed
    /// tree.
    #[error("unknown schema brand `{name}` in dict literal")]
    UnknownSchemaBrand {
        /// Schema name that failed to resolve.
        name: String,
        /// Source range of the dict literal expression.
        range: TokenRange,
    },
    /// Phase 3.b: a dict literal's default expression refers to a
    /// field name that doesn't exist in the schema. Surfaces as a
    /// codegen-side error because the analyzer still has limited
    /// dependency tracking on inter-field references inside default
    /// expressions.
    #[error(
        "default expression for field `{field}` of schema `{schema}` references unknown field `{referenced}`"
    )]
    UnknownFieldReferenceInDefault {
        /// Schema name that owns the offending default expression.
        schema: String,
        /// Field name whose default refers to a non-existent sibling.
        field: String,
        /// Sibling field name that doesn't exist on the schema.
        referenced: String,
        /// Source range of the default expression.
        range: TokenRange,
    },
    /// Phase 4.a: a call expression names a stdlib method we don't
    /// (yet) implement, or the receiver / argument shape doesn't fit
    /// any builtin signature. Carries the unresolved name + arity so
    /// the diagnostic message points at the missing entry rather
    /// than failing with a generic UnsupportedExpr.
    #[error("unknown stdlib method `{name}` (arity={arity}) in lowering")]
    UnknownStdlibMethod {
        /// The offending method / function name as written in source.
        name: String,
        /// Number of arguments at the call site (including receiver
        /// when lowered from method-call form).
        arity: u32,
        /// Source range of the call expression.
        range: TokenRange,
    },
    /// Phase 4.a: a stdlib call landed with an argument type that
    /// doesn't match the function's declared signature. Surfaces a
    /// receiver / argument typing bug that the analyzer hasn't
    /// caught yet (e.g. `(1).length()` would target the String-only
    /// `length` overload with an Int receiver).
    #[error(
        "stdlib `{name}` got argument of type `{got:?}`, expected `{expected:?}` (arg index {arg_idx})"
    )]
    StdlibArgTypeMismatch {
        /// Stdlib function name.
        name: String,
        /// Argument position in the function signature.
        arg_idx: u32,
        /// IR type the caller produced.
        got: IrType,
        /// IR type the signature requires.
        expected: IrType,
        /// Source range of the call expression.
        range: TokenRange,
    },
    /// Phase 3.b: cyclic field-default dependencies — the schema's
    /// fields refer to each other in a way the topological emit pass
    /// can't satisfy. The lowering pass reports the cycle even when
    /// the user-supplied dict happens to provide every field that
    /// participates in the cycle so a future schema-level lint can
    /// hook the same diagnostic.
    #[error(
        "cyclic field-default dependency in schema `{schema}`: {}",
        .cycle.join(" -> ")
    )]
    CyclicFieldDependency {
        /// Schema whose default expressions form the cycle.
        schema: String,
        /// Field names in the order they participate in the cycle,
        /// repeating the first name at the end (e.g. `["a", "b", "a"]`).
        cycle: Vec<String>,
        /// Source range of the dict literal where the cycle was
        /// detected.
        range: TokenRange,
    },
    /// Phase 10-a: a closure value was used in a position that escapes
    /// the wasm module boundary. Wasm-side closures are represented as
    /// scratch-heap pointers whose lifetime ends when `run_main`
    /// returns and the scratch cursor resets; carrying one through the
    /// binary handshake (a `#main` parameter or return value) would
    /// dangle. Detected at lowering when:
    /// * the `#main` signature declares a `Closure<...>` / `Fn<...>`
    ///   parameter or return type, or
    /// * a `#main` body returns an expression whose IR type is
    ///   [`crate::IrType::Closure`].
    #[error("closure value cannot cross the wasm module boundary: {context}")]
    ClosureAcrossBoundary {
        /// Human-readable description of where the closure surfaced
        /// (e.g. `"#main parameter `f`"`, `"#main return type"`).
        context: String,
        /// Source range of the offending declaration / expression.
        range: TokenRange,
    },
    /// Phase 10-a: a closure body references a captured value whose
    /// IR type the closure-conversion pass refuses to put into the
    /// captures struct. Reserved for types that have no static byte
    /// layout (e.g. `Null`); the lowering pass surfaces this rather
    /// than emitting a malformed scratch alloc.
    #[error(
        "closure capture `{name}` has unsupported type `{ty:?}` (Phase 10-a captures must be sized scalars or pointer slots)"
    )]
    UnsupportedClosureCapture {
        /// Name of the captured variable as written in source.
        name: String,
        /// IR type the resolver assigned to the capture.
        ty: IrType,
        /// Source range of the lambda expression that owns the
        /// offending capture.
        range: TokenRange,
    },
    /// Phase 10-b: the same top-level schema name is declared in two
    /// reachable modules with structurally different bodies. The
    /// wasm-AOT pipeline merges every reachable module's `#schema`
    /// table into one resolver; without rejecting this here a `#main`
    /// signature that names `User` would non-deterministically pick up
    /// one of the two definitions depending on the import-graph BFS
    /// order, which would silently break canonical-hash determinism.
    #[error(
        "schema `{name}` is declared in `{first_module}` and `{second_module}` with different shapes — wasm-AOT requires a single canonical definition"
    )]
    DuplicateSchemaAcrossFiles {
        /// Schema name in conflict.
        name: String,
        /// First reachable module that declared the schema.
        first_module: String,
        /// Second reachable module that re-declared the same name with
        /// a different shape.
        second_module: String,
    },
    /// Phase 10-b: more than one reachable module declares a `#main`
    /// directive. wasm-AOT lowers exactly one entry per build; an
    /// imported library accidentally tagging its top-level expression
    /// `#main(...)` would otherwise compete with the entry's signature
    /// and silently shadow it on a hash collision. The IR pass rejects
    /// the workspace and points at the second offender so the user can
    /// decide which file is meant to be the entry.
    #[error(
        "multiple `#main` directives in workspace: entry `{entry_module}` and imported `{other_module}` — only the entry file may declare `#main`"
    )]
    MultipleMainDirectives {
        /// Canonical id of the entry module the host asked to lower.
        entry_module: String,
        /// Canonical id of the imported module that also carries a
        /// `#main` directive.
        other_module: String,
    },
}
