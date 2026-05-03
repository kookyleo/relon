use miette::Diagnostic;
use relon_parser::TokenRange;
use thiserror::Error;

#[derive(Error, Debug, Diagnostic)]
pub enum RuntimeError {
    #[error("Variable not found: {0}")]
    #[diagnostic(code(relon::eval::variable_not_found))]
    VariableNotFound(String, #[label("here")] TokenRange),

    #[error("Type mismatch: expected {expected}, found {found}")]
    #[diagnostic(code(relon::eval::type_mismatch))]
    TypeMismatch {
        expected: String,
        found: String,
        #[label("here")]
        range: TokenRange,
    },

    #[error("Validation failed: {0}")]
    #[diagnostic(code(relon::eval::validation_failed))]
    ValidationError(String, #[label("here")] TokenRange),

    #[error("Division by zero")]
    #[diagnostic(code(relon::eval::division_by_zero))]
    DivisionByZero(#[label("here")] TokenRange),

    #[error("Function not found: {0}")]
    #[diagnostic(code(relon::eval::function_not_found))]
    FunctionNotFound(String, #[label("here")] TokenRange),

    #[error("Circular reference detected at {0:?}")]
    #[diagnostic(code(relon::eval::circular_reference))]
    CircularReference(Vec<String>),

    #[error("Unsupported operator {0:?}")]
    #[diagnostic(code(relon::eval::unsupported_operator))]
    UnsupportedOperator(String, #[label("here")] TokenRange),

    #[error("Invalid identifier: {0}. Function/Decorator names must start with a letter or underscore and contain only alphanumeric characters or underscores.")]
    #[diagnostic(code(relon::eval::invalid_identifier))]
    InvalidIdentifier(String, #[label("here")] TokenRange),

    #[error("IO error: {0}")]
    #[diagnostic(code(relon::eval::io_error))]
    IoError(String),

    #[error("Module not found at path: {0}")]
    #[diagnostic(code(relon::eval::module_not_found))]
    ModuleNotFound(String, #[label("here")] miette::SourceSpan),

    #[error("Parse error in module {path}: {message}")]
    #[diagnostic(code(relon::eval::module_parse_error))]
    ModuleParseError {
        path: String,
        message: String,
        #[label("imported here")]
        range: miette::SourceSpan,
    },

    #[error("Circular import detected: {0:?}")]
    #[diagnostic(code(relon::eval::circular_import))]
    CircularImport(Vec<String>, #[label("here")] miette::SourceSpan),
}
