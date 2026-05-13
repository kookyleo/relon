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

    #[error("`#extend {name}` targets schema `{name}` which is not declared in scope")]
    #[diagnostic(
        code(relon::analyze::extend_unknown_schema),
        help(
            "`#extend X with {{ ... }}` requires `X` to be either a built-in type (String / Int / List / ...) or a `#schema X` declared in the same module / a transitively imported module. Declare `X` first, or correct the spelling."
        )
    )]
    ExtendUnknownSchema {
        name: String,
        #[label("no schema by this name in scope")]
        range: SourceSpan,
    },

    #[error("method `{method}` declared more than once for schema `{schema}`")]
    #[diagnostic(
        code(relon::analyze::method_name_conflict),
        help(
            "Each schema may bind a given method name only once across its `with {{ ... }}` block and any `#extend` blocks visible to the current module. Rename one of the methods, or move the override into a different module that this one doesn't import together."
        )
    )]
    MethodNameConflict {
        schema: String,
        method: String,
        #[label("first defined here")]
        first: SourceSpan,
        #[label("redefined with the same name")]
        second: SourceSpan,
    },

    #[error("method `{method}` is not declared on schema `{schema}`")]
    #[diagnostic(
        code(relon::analyze::unknown_method),
        help(
            "Schema-rooted dispatch resolves `value.method(...)` and `Schema.method(...)` against the schema's `with {{ ... }}` block (and any `#extend` blocks visible here). Add the method, or check the spelling and import paths."
        )
    )]
    UnknownMethod {
        schema: String,
        method: String,
        #[label("no such method on this schema")]
        range: SourceSpan,
    },

    #[error(
        "method `{method}` on schema `{schema}` is `#private` and cannot be called from outside"
    )]
    #[diagnostic(
        code(relon::analyze::private_method_violation),
        help(
            "`#private` methods are only callable from the same `with {{ ... }}` block (sibling methods on the same schema). Drop the `#private` directive, or move the caller into the same block."
        )
    )]
    PrivateMethodViolation {
        schema: String,
        method: String,
        #[label("private method called from outside its `with {{ ... }}` block")]
        range: SourceSpan,
    },

    #[error(
        "`#derive {constraint}` witness `{method}` does not match the constraint's expected shape (expected `{expected_shape}`, found `{found_shape}`)"
    )]
    #[diagnostic(
        code(relon::analyze::constraint_witness_shape_mismatch),
        help(
            "A `#derive Constraint` pragma promotes the next method into the constraint's witness slot — its name, parameter list, and return type must match the constraint definition exactly. Either rewrite the method's signature to match the expected shape, or drop the `#derive` if this method isn't intended as a witness."
        )
    )]
    ConstraintWitnessShapeMismatch {
        /// Constraint name from the `#derive C` pragma (e.g. `Equatable`).
        constraint: String,
        /// Method as the user named it (may differ from the constraint's
        /// expected witness name).
        method: String,
        /// Expected witness shape, rendered as
        /// `eq(other: Self) -> Bool` etc. — see
        /// [`crate::constraints`].
        expected_shape: String,
        /// Actual method signature as written in source.
        found_shape: String,
        #[label("witness shape doesn't match `{constraint}`")]
        range: SourceSpan,
    },

    #[error("match arms produce incompatible types: {}", arm_types.join(" vs "))]
    #[diagnostic(
        code(relon::analyze::match_arm_type_mismatch),
        help(
            "Every non-wildcard arm should evaluate to a value of the same shape (or share a common Optional supertype). Either align the arm bodies or wrap the result in a sum-type schema."
        )
    )]
    MatchArmTypeMismatch {
        /// Enum being matched, if statically known. Lets the message
        /// read "match on `Notification` arms produce …".
        enum_name: Option<String>,
        /// Names of the inferred arm-body types in source order.
        arm_types: Vec<String>,
        #[label("arms diverge here")]
        range: SourceSpan,
    },

    #[error("unknown type name `{name}`")]
    #[diagnostic(
        code(relon::analyze::unknown_type_name),
        help(
            "The analyzer couldn't resolve this name to a builtin or user-declared schema. Check spelling, or add an `#import` / `#schema` for it."
        )
    )]
    UnknownTypeName {
        name: String,
        #[label("not a builtin or declared schema")]
        range: SourceSpan,
    },

    #[error("`#main` return type mismatch: expected {expected}, got {found}")]
    #[diagnostic(
        code(relon::analyze::main_return_type_mismatch),
        help(
            "The body of an entry program must produce a value matching the `#main(...) -> Type` declaration."
        )
    )]
    MainReturnTypeMismatch {
        expected: String,
        found: String,
        #[label("body produces {found}")]
        range: SourceSpan,
    },

    #[error("function `{fn_name}` expects {expected} arg(s), found {found}")]
    #[diagnostic(
        code(relon::analyze::fn_call_arg_count),
        help(
            "Stage 3 — the analyzer has a static signature for this function and the call's arity disagrees. Add or remove arguments so the count matches."
        )
    )]
    FnCallArgCountMismatch {
        fn_name: String,
        expected: String,
        found: usize,
        #[label("wrong arity")]
        range: SourceSpan,
    },

    #[error("argument `{param_name}` of `{fn_name}` expects {expected}, got {found}")]
    #[diagnostic(
        code(relon::analyze::fn_call_arg_type),
        help(
            "Stage 3 — the analyzer has a static signature for this function and one of the arguments disagrees with the parameter's declared type."
        )
    )]
    FnCallArgTypeMismatch {
        fn_name: String,
        param_name: String,
        expected: String,
        found: String,
        #[label("type mismatch")]
        range: SourceSpan,
    },

    #[error(
        "native function `{fn_name}` requires capability `{capability}`, but it isn't granted"
    )]
    #[diagnostic(
        code(relon::analyze::capability_required),
        help(
            "This native fn was registered with `register_fn` requiring `{capability}`, but the host's `Capabilities` doesn't grant it. Grant the capability (e.g. `caps.{capability} = true`) or stop calling this fn from a script-reachable path."
        )
    )]
    CapabilityRequired {
        fn_name: String,
        capability: String,
        #[label("would be denied at runtime")]
        range: SourceSpan,
    },

    #[error("division by zero in constant expression")]
    #[diagnostic(
        code(relon::analyze::const_div_zero),
        help(
            "The right-hand operand of `/` or `%` folds to a literal `0`; the same expression would raise DivisionByZero at runtime."
        )
    )]
    ConstDivisionByZero {
        #[label("divisor evaluates to 0 here")]
        range: SourceSpan,
    },

    #[error("numeric overflow in constant expression ({op})")]
    #[diagnostic(
        code(relon::analyze::const_numeric_overflow),
        help(
            "This arithmetic on integer literals exceeds i64 range; the same expression would raise NumericOverflow at runtime."
        )
    )]
    ConstNumericOverflow {
        op: String,
        #[label("overflows here")]
        range: SourceSpan,
    },

    // === v1.3 strict-mode diagnostics ===
    #[error("dynamic spread requires a `<T>` type hint under `#strict`")]
    #[diagnostic(
        code(relon::analyze::missing_spread_type_hint),
        help(
            "Strict mode forbids spreading a value whose static shape isn't known. Annotate the spread with the spread source's schema, e.g. `...<Extra> e`, or remove `#strict`."
        )
    )]
    MissingSpreadTypeHint {
        #[label("untyped spread")]
        range: SourceSpan,
    },

    #[error("dynamic dict key requires a `<T>` type hint under `#strict`")]
    #[diagnostic(
        code(relon::analyze::missing_dynamic_key_type_hint),
        help(
            "Strict mode forbids `[expr]: value` keys whose type isn't statically declared. Annotate the key, e.g. `[<String> k]: value`, or remove `#strict`."
        )
    )]
    MissingDynamicKeyTypeHint {
        #[label("untyped dynamic key")]
        range: SourceSpan,
    },

    #[error("strict mode: type of reference `{name}` cannot be inferred")]
    #[diagnostic(
        code(relon::analyze::unknown_reference_type),
        help(
            "Strict mode requires every value to have a statically inferable type. The reference resolves but its target carries no declared type. Add a type annotation at the target, or remove `#strict`."
        )
    )]
    UnknownReferenceType {
        /// Final segment whose type couldn't be determined. For a
        /// single-segment failure (`Variable("u")` with no static
        /// type) this is the bare head; for a multi-segment failure
        /// (`o.unknown` where `o` is a known schema but `unknown`
        /// isn't a declared field) this is the failing tail segment.
        name: String,
        /// Full dotted path the strict pass walked, in source order
        /// (`["o", "unknown"]`). Lets diagnostic consumers reconstruct
        /// the chain of fields that led to the failure without
        /// re-walking the AST.
        path: Vec<String>,
        #[label("type unknown")]
        range: SourceSpan,
    },

    #[error("strict mode: schema `{name}` is not declared in this workspace")]
    #[diagnostic(
        code(relon::analyze::unresolved_schema),
        help(
            "Strict mode requires every schema reference to point at a declared `#schema` definition. Declare the schema or remove `#strict`."
        )
    )]
    UnresolvedSchema {
        name: String,
        #[label("schema not found")]
        range: SourceSpan,
    },

    #[error("strict mode: cannot statically infer this expression ({reason})")]
    #[diagnostic(
        code(relon::analyze::inference_limit),
        help(
            "Strict mode requires every expression to have a derivable static type. Either annotate the surrounding binding so inference has a target, or remove `#strict`."
        )
    )]
    InferenceLimit {
        reason: String,
        #[label("inference failed")]
        range: SourceSpan,
    },

    #[error(
        "strict mode forbids native fn `{fn_name}` whose return type isn't statically declared"
    )]
    #[diagnostic(
        code(relon::analyze::strict_forbids_native_return),
        help(
            "Native functions reach a host implementation that may produce any value; strict mode demands a static return type. Either expose the fn through `host_fn_signatures` with a declared return type, or stop calling it from a strict module."
        )
    )]
    StrictForbidsNativeReturn {
        fn_name: String,
        #[label("untyped native call")]
        range: SourceSpan,
    },

    #[error("strict mode requires closure parameter `{param_name}` to declare a type")]
    #[diagnostic(
        code(relon::analyze::strict_forbids_untyped_closure_param),
        help(
            "Strict mode forbids closure parameters whose type defaults to `Any`. Annotate each parameter with a type (e.g. `(Int n) => n + 1`), or remove `#strict`."
        )
    )]
    StrictForbidsUntypedClosureParam {
        param_name: String,
        #[label("missing type for `{param_name}`")]
        range: SourceSpan,
    },

    // (v1.5 `StrictForbidsUntypedMainParam` retired in v1.6 — the
    // generic `ExplicitAnyForbidden` covers `#main(Any x)` in every
    // mode, replacing the strict-only variant.)
    #[error(
        "strict mode requires closure body inference — `{role}` cannot be classified statically"
    )]
    #[diagnostic(
        code(relon::analyze::strict_forbids_unclassified_closure_body),
        help(
            "Strict mode demands every closure body produce a derivable type. Either declare `-> ReturnType`, or refactor the body so its type is reachable from inference. Removing `#strict` is the other escape hatch."
        )
    )]
    StrictForbidsUnclassifiedClosureBody {
        role: String,
        #[label("uninferable closure body")]
        range: SourceSpan,
    },

    #[error("duplicate field `{field}` produced by spread")]
    #[diagnostic(
        code(relon::analyze::duplicate_field),
        help(
            "A spread source contributes a key that's already declared on the dict. Rename one of the conflicting entries, or restructure the spread so it doesn't double-bind the same name."
        )
    )]
    DuplicateField {
        field: String,
        #[label("duplicate field")]
        range: SourceSpan,
    },

    #[error("type `Any` is not allowed in user code (`{context}`)")]
    #[diagnostic(
        code(relon::analyze::explicit_any_forbidden),
        help(
            "v1.6 retired `Any` from the user-facing surface. Use a concrete type (`Int`, `String`, ...), a parameterized container (`List<T>`, `Dict<String, V>`), an `Enum<...>` for sum types, or declare a `#schema` for structured payloads. If you genuinely need an opaque pass-through, define a single-field schema and pass it explicitly."
        )
    )]
    ExplicitAnyForbidden {
        /// Where the `Any` token appeared, named for the diagnostic
        /// message: e.g. `schema field`, `#main parameter`, `closure
        /// parameter`, `closure return type`, `typed binding`.
        context: String,
        #[label("`Any` is no longer accepted here")]
        range: SourceSpan,
    },

    #[error("index key for schema `{schema}` expects {expected}, got {found}")]
    #[diagnostic(
        code(relon::analyze::method_generic_arg_mismatch),
        help(
            "The schema declares `#derive Indexable index(key: {expected}) -> ...` (or a constraint-specified equivalent). Pass a `{expected}` here; the analyzer previously deferred this check to runtime, where it surfaced as a `TypeMismatch` from inside the method body."
        )
    )]
    MethodGenericArgMismatch {
        /// Receiver schema name (`Bag`, `Cache`, ...).
        schema: String,
        /// Method whose generic parameter the call site violates
        /// (`index`, `at`, ...).
        method: String,
        /// Argument name from the method declaration (`key`, `idx`, ...).
        param_name: String,
        /// Expected type (e.g. `Int`) — what the method's parameter
        /// declares after constraint-generic substitution.
        expected: String,
        /// Actual type the call site supplied.
        found: String,
        #[label("expected `{expected}` for `{param_name}`")]
        range: SourceSpan,
    },

    #[error(
        "method `{method}` on schema `{schema}` redeclares a generic name `{generic}` already bound by the schema"
    )]
    #[diagnostic(
        code(relon::analyze::method_generic_shadows_schema_generic),
        help(
            "The method's `<{generic}>` shadows the schema's `<{generic}>` of the same name. Substitution at the call site binds the receiver's `{generic}` first, so the method-level placeholder silently rebinds the same key — readers can't tell which `{generic}` is meant in the body. Rename the method generic (e.g. `<U>` instead of `<{generic}>`) so each binding key is distinct."
        )
    )]
    MethodGenericShadowsSchemaGeneric {
        /// Owning schema name (`List`, `Bag`, ...).
        schema: String,
        /// Method that introduces the colliding generic name.
        method: String,
        /// The generic name that collides (`T`, `K`, ...).
        generic: String,
        #[label("method generic shadows schema generic `{generic}`")]
        range: SourceSpan,
    },

    #[error("bare `{type_name}` requires explicit type parameter(s) in `{context}`")]
    #[diagnostic(
        code(relon::analyze::bare_generic_container),
        help(
            "v1.7 retires the bare-generic shorthand. Write `List<T>`, `Dict<K, V>`, or `Closure<...>` with explicit element / parameter / return types — bare `List` was equivalent to `List<Any>` and v1.6 already banned `Any` from the user surface. `Enum<...>` likewise needs at least one alternative."
        )
    )]
    BareGenericContainer {
        /// The bare type name encountered (`List`, `Dict`, `Closure`,
        /// `Fn`, `Enum`).
        type_name: String,
        /// Same `context` shape as `ExplicitAnyForbidden` so
        /// diagnostics from both checks read uniformly.
        context: String,
        #[label("missing type parameter(s)")]
        range: SourceSpan,
    },

    #[error("`&this` used outside a list-iteration context")]
    #[diagnostic(
        code(relon::analyze::this_outside_iteration),
        help(
            "`&this` is the current iteration element of a list / list-comprehension. In Dict scope it falls back to `&root`, which works but obscures intent — prefer `&root` directly so readers don't have to know about the fallback."
        )
    )]
    ThisOutsideIteration {
        #[label("equivalent to `&root` here")]
        range: SourceSpan,
    },

    #[error("`&{base}` used outside a list-iteration context")]
    #[diagnostic(
        code(relon::analyze::iteration_ref_outside_list),
        help(
            "`&prev` / `&next` / `&index` only have meaning while iterating a list (or inside a list comprehension's body). Using them anywhere else evaluates to a runtime `VariableNotFound`."
        )
    )]
    IterationRefOutsideList {
        /// Which iteration ref triggered the diagnostic — `prev`,
        /// `next`, or `index`. Stored as a string so the message
        /// formats cleanly across all three.
        base: String,
        #[label("requires a list context")]
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
            | Diagnostic::RootSchemaInvalidValue { .. }
            | Diagnostic::ExtendUnknownSchema { .. }
            | Diagnostic::MethodNameConflict { .. }
            | Diagnostic::UnknownMethod { .. }
            | Diagnostic::PrivateMethodViolation { .. }
            | Diagnostic::ConstraintWitnessShapeMismatch { .. }
            // Static type mismatches are derivable from source + schema
            // alone — the workhorse of Stage 1 hardening. Surface them
            // as errors so the evaluator never reaches a code path that
            // would re-discover the same problem at runtime.
            | Diagnostic::StaticTypeMismatch { .. }
            | Diagnostic::MatchArmTypeMismatch { .. }
            | Diagnostic::UnknownTypeName { .. }
            | Diagnostic::MainReturnTypeMismatch { .. }
            | Diagnostic::FnCallArgCountMismatch { .. }
            | Diagnostic::FnCallArgTypeMismatch { .. }
            // Stage 4: capability errors are derivable from source +
            // host_fn_gates + caps alone — surface as Error so the
            // evaluator never reaches the gated call.
            | Diagnostic::CapabilityRequired { .. }
            // Stage 5: literal-only arithmetic that would explode at
            // runtime (div-by-zero / i64 overflow) is fully derivable
            // from source — promote to Error so the evaluator never
            // re-discovers the same problem.
            | Diagnostic::ConstDivisionByZero { .. }
            | Diagnostic::ConstNumericOverflow { .. }
            // v1.3: strict-mode diagnostics gate evaluation just like
            // every other static error — the language contract is
            // "everything statically inferable is checked statically".
            | Diagnostic::MissingSpreadTypeHint { .. }
            | Diagnostic::MissingDynamicKeyTypeHint { .. }
            | Diagnostic::UnknownReferenceType { .. }
            | Diagnostic::UnresolvedSchema { .. }
            | Diagnostic::InferenceLimit { .. }
            | Diagnostic::StrictForbidsNativeReturn { .. }
            // v1.5: every closure parameter must declare a type
            // under strict mode; closure bodies must be statically
            // classifiable. (`StrictForbidsUntypedMainParam` was
            // retired in v1.6 — the generic `ExplicitAnyForbidden`
            // diagnostic now covers `#main(Any x)` in every mode.)
            | Diagnostic::StrictForbidsUntypedClosureParam { .. }
            | Diagnostic::StrictForbidsUnclassifiedClosureBody { .. }
            // v1.6: `Any` is banned from the user-facing surface in
            // every mode. Reaches Error because nothing the user
            // could have meant by writing `Any` is well-defined any
            // more — the language has concrete types, parameterized
            // containers, enums, and schemas to cover the space.
            | Diagnostic::ExplicitAnyForbidden { .. }
            // v1.7: bare `List` / `Dict` / `Closure` / `Fn` / `Enum`
            // (no generic parameters) is equivalent to leaking `Any`
            // through the back door — Error severity in every mode.
            | Diagnostic::BareGenericContainer { .. }
            // Schema-rooted §J follow-up: a concrete arg-type for a
            // method's constraint-supplied generic param (e.g.
            // `bag["abc"]` against `index(key: Int)`) — Error because
            // the runtime would otherwise raise `TypeMismatch` from
            // inside the method body.
            | Diagnostic::MethodGenericArgMismatch { .. }
            // `&prev` / `&next` / `&index` outside any list iteration
            // are statically broken: the evaluator will raise
            // `VariableNotFound`. Surface as Error so the user fixes
            // it before running.
            | Diagnostic::IterationRefOutsideList { .. }
            | Diagnostic::DuplicateField { .. } => Severity::Error,
            // Informational: the analyzer's view is conservative — a
            // spread, closure binding, or runtime-computed field may
            // still resolve, so we don't gate evaluation on it.
            Diagnostic::UnresolvedReference { .. } => Severity::Warning,
            // `&this` outside iteration still evaluates (falls back to
            // `&root` per reference.rs). Warn rather than error — the
            // program runs, but `&root` is the clearer spelling.
            Diagnostic::ThisOutsideIteration { .. } => Severity::Warning,
            // Schema-rooted §J follow-up: same-name shadowing between
            // a method's `<T>` and the owning schema's `<T>` produces
            // a confusing substitution order (the method's binding
            // silently rebinds the schema's key). Warning rather than
            // error because the program does run — readers just can't
            // tell which `T` is meant in the body.
            Diagnostic::MethodGenericShadowsSchemaGeneric { .. } => Severity::Warning,
        }
    }
}

/// Convenience: convert a parser `TokenRange` into the `SourceSpan`
/// miette wants for `#[label]` fields.
pub fn span_of(range: TokenRange) -> SourceSpan {
    SourceSpan::from(range)
}
