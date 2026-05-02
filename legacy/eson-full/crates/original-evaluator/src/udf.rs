use crate::error::{new_undefined_function_error, new_unexpected_fn_args_error, EvaluatorError};
use crate::expr::ExprUnit;
use tokenizer::TokenRange;

pub trait Udf {
    fn call(&self) -> Result<ExprUnit, EvaluatorError>;
}

pub(crate) struct UdfDouble(ExprUnit);

impl Udf for UdfDouble {
    fn call(&self) -> Result<ExprUnit, EvaluatorError> {
        match &self.0 {
            ExprUnit::UnitPrimNumberInt(i, _) => {
                Ok(ExprUnit::UnitPrimNumberInt(i * 2, TokenRange::default()))
            }
            // UdfDouble only accepts UnitPrimNumberInt
            others => Err(new_unexpected_fn_args_error(
                String::from("UdfDouble"),
                others.type_name(),
            )),
        }
    }
}

fn new_udf(name: &str, args: Vec<ExprUnit>) -> Result<Box<dyn Udf>, EvaluatorError> {
    match name {
        "fn_double" => Ok(Box::new(UdfDouble(args[0].clone()))),
        _ => Err(new_undefined_function_error(
            name.to_string(),
            "unknown".to_string(),
        )),
    }
}

pub(crate) fn udf_call(name: &str, args: Vec<ExprUnit>) -> Result<ExprUnit, EvaluatorError> {
    let udf = new_udf(name, args);
    match udf {
        Ok(udf) => udf.call(),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use tokenizer::TokenRange;

    #[test]
    fn test_udf_double() {
        use crate::expr::ExprUnit;
        use crate::udf::udf_call;
        let result = udf_call(
            "fn_double",
            vec![ExprUnit::UnitPrimNumberInt(1, TokenRange::default())],
        )
        .unwrap();
        assert_eq!(
            result,
            ExprUnit::UnitPrimNumberInt(2, TokenRange::default())
        );
    }
}
