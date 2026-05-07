use miette::Diagnostic;
use relon_parser::TokenRange;
use thiserror::Error;

/// Render a chain of identifiers/paths joined by `→`. Used by the
/// `CircularReference` and `CircularImport` `Display` impls so the error
/// message reads naturally instead of dumping a debug-formatted `Vec`.
fn format_chain(chain: &[String]) -> String {
    chain.join(" → ")
}

#[derive(Error, Debug, Diagnostic)]
pub enum RuntimeError {
    #[error("Variable not found: {0}")]
    #[diagnostic(
        code(relon::eval::variable_not_found),
        help("Check that the name is spelled correctly and is in scope at this point.")
    )]
    VariableNotFound(String, #[label("undefined")] TokenRange),

    #[error("Type mismatch: expected {expected}, found {found}")]
    #[diagnostic(code(relon::eval::type_mismatch))]
    TypeMismatch {
        expected: String,
        found: String,
        #[label("expected {expected}, got {found}")]
        range: TokenRange,
    },

    #[error("Validation failed: {0}")]
    #[diagnostic(code(relon::eval::validation_failed))]
    ValidationError(String, #[label("validation failed here")] TokenRange),

    #[error("Division by zero")]
    #[diagnostic(
        code(relon::eval::division_by_zero),
        help("The right-hand operand of `/` or `%` evaluated to 0.")
    )]
    DivisionByZero(#[label("divisor is zero")] TokenRange),

    #[error("Function not found: {0}")]
    #[diagnostic(code(relon::eval::function_not_found))]
    FunctionNotFound(String, #[label("called here")] TokenRange),

    #[error("Circular reference detected: {}", format_chain(.cycle))]
    #[diagnostic(
        code(relon::eval::circular_reference),
        help("Each entry depends on a later one in the cycle. Break the loop or replace one of the references with a literal value.")
    )]
    CircularReference {
        /// Path segments that form the cycle, in declaration order.
        cycle: Vec<String>,
        #[label("triggers the cycle")]
        range: TokenRange,
    },

    #[error("Unsupported operator {0:?}")]
    #[diagnostic(code(relon::eval::unsupported_operator))]
    UnsupportedOperator(String, #[label("not supported here")] TokenRange),

    #[error("Invalid identifier: {0}")]
    #[diagnostic(
        code(relon::eval::invalid_identifier),
        help("Function/decorator names must start with a letter or underscore and contain only alphanumeric characters or underscores.")
    )]
    InvalidIdentifier(String, #[label("invalid identifier")] TokenRange),

    #[error("IO error: {0}")]
    #[diagnostic(code(relon::eval::io_error))]
    IoError(String),

    #[error("Module not found at path: {0}")]
    #[diagnostic(
        code(relon::eval::module_not_found),
        help("Check the path is relative to the importing file (or absolute) and that the file exists.")
    )]
    ModuleNotFound(String, #[label("import target missing")] miette::SourceSpan),

    #[error("Parse error in module {path}: {message}")]
    #[diagnostic(code(relon::eval::module_parse_error))]
    ModuleParseError {
        path: String,
        message: String,
        #[label("imported here")]
        range: miette::SourceSpan,
    },

    #[error("Circular import detected: {}", format_chain(.0))]
    #[diagnostic(
        code(relon::eval::circular_import),
        help("Two or more modules import each other. Restructure so the dependency is one-way.")
    )]
    CircularImport(
        Vec<String>,
        #[label("import that closes the cycle")] miette::SourceSpan,
    ),

    #[error("Numeric overflow")]
    #[diagnostic(code(relon::eval::numeric_overflow))]
    NumericOverflow(#[label("overflowed here")] TokenRange),

    #[error("Step limit exceeded ({limit} evaluation steps)")]
    #[diagnostic(
        code(relon::eval::step_limit_exceeded),
        help("The script ran longer than the configured `max_steps` budget. Raise `Capabilities::max_steps` or refactor recursive / iterative work.")
    )]
    StepLimitExceeded {
        limit: u64,
        #[label("budget exhausted here")]
        range: TokenRange,
    },

    #[error("Recursion limit exceeded ({limit} levels)")]
    #[diagnostic(
        code(relon::eval::recursion_limit_exceeded),
        help("A type-check or schema-validation pass nested deeper than the runtime's safety bound. Restructure the recursive type or value so it doesn't self-reference past this depth.")
    )]
    RecursionLimitExceeded {
        limit: usize,
        #[label("depth limit reached here")]
        range: TokenRange,
    },

    #[error("Value too large: {actual} elements exceeds limit of {limit}")]
    #[diagnostic(
        code(relon::eval::value_too_large),
        help("A list/dict grew past `Capabilities::max_value_bytes`. Raise the limit or shrink the value.")
    )]
    ValueTooLarge {
        limit: usize,
        actual: usize,
        #[label("constructed here")]
        range: TokenRange,
    },

    #[error("Capability denied: native function `{name}` ({reason})")]
    #[diagnostic(
        code(relon::eval::capability_denied),
        help("This Context is sandboxed. Add the function name to `Capabilities::allow_native_fn` to permit it.")
    )]
    CapabilityDenied {
        name: String,
        reason: String,
        #[label("call rejected by sandbox")]
        range: TokenRange,
    },
}
