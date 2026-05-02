use thiserror::Error;

use tokenizer::TokenKey;

#[derive(Error, Debug)]
pub enum EvaluatorError {
    #[error("Circular ref detected: {0}")]
    CircularRefError(String),

    #[error("Invalid ref: {0} @{1}")]
    InvalidRefError(String, String),

    #[error("Undefined variable: {0} @{1}")]
    UndefinedVariableError(String, String),

    #[error("Undefined function: {0} @{1}")]
    UndefinedFunctionError(String, String),

    #[error("Unexpected fn argument: {1} in {0}")]
    UnexpectedFnArgsError(String, String),

    #[error("unknown error")]
    Unknown,
}

pub fn new_circular_ref_error(circular_ref_nodes: Vec<String>) -> EvaluatorError {
    EvaluatorError::CircularRefError(circular_ref_nodes.join(", "))
}

pub fn new_invalid_ref_error(ref_target: &Vec<TokenKey>, node_name: String) -> EvaluatorError {
    let ref_target = ref_target
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<String>>()
        .join(".");
    EvaluatorError::InvalidRefError(ref_target, node_name)
}

pub fn new_undefined_variable_error(name: String, node_name: String) -> EvaluatorError {
    EvaluatorError::UndefinedVariableError(name, node_name)
}

pub fn new_undefined_function_error(name: String, node_name: String) -> EvaluatorError {
    EvaluatorError::UndefinedFunctionError(name, node_name)
}

pub fn new_unexpected_fn_args_error(fn_name: String, arg: String) -> EvaluatorError {
    EvaluatorError::UnexpectedFnArgsError(fn_name, arg)
}

#[cfg(test)]
mod tests {
    use std::fmt::Display;

    use super::*;

    fn assert<T: Display>(expected: &str, value: T) {
        assert_eq!(expected, value.to_string());
    }

    #[test]
    fn test_error() {
        let err = EvaluatorError::Unknown;
        assert("unknown error", err);
    }

    #[test]
    fn test_unit() {
        #[derive(Error, Debug)]
        #[error("unit error")]
        struct Error;

        assert("unit error", Error);
    }
}
