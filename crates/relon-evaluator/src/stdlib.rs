use crate::error::RuntimeError;
use crate::eval::{Context, RelonFunction};
use crate::value::Value;
use std::sync::Arc;

pub fn register_to(ctx: &mut Context) {
    ctx.register_fn("len", Arc::new(Len));
    ctx.register_fn("range", Arc::new(Range));
    ctx.register_fn("type", Arc::new(Type));
    ctx.register_fn("int", Arc::new(ValidatorInt));
    ctx.register_fn("string", Arc::new(ValidatorString));
    ctx.register_fn("bool", Arc::new(ValidatorBool));
    ctx.register_fn("float", Arc::new(ValidatorFloat));
    ctx.register_fn("list", Arc::new(ValidatorList));
    ctx.register_fn("dict", Arc::new(ValidatorDict));
    ctx.register_fn("min", Arc::new(ValidatorMin));
    ctx.register_fn("max", Arc::new(ValidatorMax));
    ctx.register_fn("one_of", Arc::new(ValidatorOneOf));
    ctx.register_fn("required_fields", Arc::new(RequiredFields));
    ctx.register_fn("requires", Arc::new(Requires));
    ctx.register_fn("field_eq", Arc::new(FieldEq));
    ctx.register_fn("split", Arc::new(Split));
    ctx.register_fn("join", Arc::new(Join));
    ctx.register_fn("replace", Arc::new(Replace));
    ctx.register_fn("to_upper", Arc::new(ToUpper));
    ctx.register_fn("to_lower", Arc::new(ToLower));
    ctx.register_fn("merge", Arc::new(Merge));
    ctx.register_fn("keys", Arc::new(Keys));
    ctx.register_fn("values", Arc::new(Values));
    ctx.register_fn("contains", Arc::new(Contains));
}

struct Len;
impl RelonFunction for Len {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(RuntimeError::TypeMismatch {
                expected: "1 argument".to_string(),
                found: format!("{}", args.len()),
                range,
            });
        }
        match &args[0] {
            Value::String(s) => Ok(Value::Int(s.len() as i64)),
            Value::List(l) => Ok(Value::Int(l.len() as i64)),
            Value::Dict(d) => Ok(Value::Int(d.len() as i64)),
            _ => Err(RuntimeError::TypeMismatch {
                expected: "String/List/Dict".to_string(),
                found: args[0].type_name().to_string(),
                range,
            }),
        }
    }
}

struct Range;
impl RelonFunction for Range {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let (start, end) = match args.len() {
            1 => (
                0,
                match &args[0] {
                    Value::Int(i) => *i,
                    _ => 0,
                },
            ),
            2 => (
                match &args[0] {
                    Value::Int(i) => *i,
                    _ => 0,
                },
                match &args[1] {
                    Value::Int(i) => *i,
                    _ => 0,
                },
            ),
            _ => {
                return Err(RuntimeError::TypeMismatch {
                    expected: "1 or 2 arguments".to_string(),
                    found: format!("{}", args.len()),
                    range,
                })
            }
        };
        Ok(Value::List((start..end).map(Value::Int).collect()))
    }
}

struct Type;
impl RelonFunction for Type {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(RuntimeError::TypeMismatch {
                expected: "1 arg".to_string(),
                found: args.len().to_string(),
                range,
            });
        }
        Ok(Value::String(args[0].type_name().to_string()))
    }
}

macro_rules! type_validator {
    ($struct_name:ident, $variant:ident, $expected:expr) => {
        struct $struct_name;
        impl RelonFunction for $struct_name {
            fn call(
                &self,
                args: Vec<Value>,
                range: relon_parser::TokenRange,
            ) -> Result<Value, RuntimeError> {
                if !(1..=2).contains(&args.len()) {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "1 or 2 args (value, message?)".to_string(),
                        found: args.len().to_string(),
                        range,
                    });
                }
                if let Value::$variant(_) = &args[0] {
                    Ok(args[0].clone())
                } else {
                    validation_failure(
                        &args,
                        1,
                        RuntimeError::TypeMismatch {
                            expected: $expected.to_string(),
                            found: args[0].type_name().to_string(),
                            range,
                        },
                        range,
                    )
                }
            }
        }
    };
}

type_validator!(ValidatorInt, Int, "Int");
type_validator!(ValidatorString, String, "String");
type_validator!(ValidatorBool, Bool, "Bool");
type_validator!(ValidatorFloat, Float, "Float");
type_validator!(ValidatorList, List, "List");
type_validator!(ValidatorDict, Dict, "Dict");

struct ValidatorMin;
impl RelonFunction for ValidatorMin {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        if !(2..=3).contains(&args.len()) {
            return Err(RuntimeError::TypeMismatch {
                expected: "2 or 3 args (value, min, message?)".to_string(),
                found: args.len().to_string(),
                range,
            });
        }
        let is_valid = match (&args[0], &args[1]) {
            (Value::Int(v), Value::Int(m)) => v >= m,
            (Value::Float(v), Value::Float(m)) => v >= m,
            (Value::Int(v), Value::Float(m)) => (*v as f64) >= m.into_inner(),
            (Value::Float(v), Value::Int(m)) => v.into_inner() >= (*m as f64),
            _ => {
                return Err(RuntimeError::TypeMismatch {
                    expected: "Number".to_string(),
                    found: args[0].type_name().to_string(),
                    range,
                })
            }
        };
        if is_valid {
            Ok(args[0].clone())
        } else {
            validation_failure(
                &args,
                2,
                RuntimeError::TypeMismatch {
                    expected: format!(">= {}", args[1]),
                    found: format!("{}", args[0]),
                    range,
                },
                range,
            )
        }
    }
}

struct ValidatorMax;
impl RelonFunction for ValidatorMax {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        if !(2..=3).contains(&args.len()) {
            return Err(RuntimeError::TypeMismatch {
                expected: "2 or 3 args (value, max, message?)".to_string(),
                found: args.len().to_string(),
                range,
            });
        }
        let is_valid = match (&args[0], &args[1]) {
            (Value::Int(v), Value::Int(m)) => v <= m,
            (Value::Float(v), Value::Float(m)) => v <= m,
            (Value::Int(v), Value::Float(m)) => (*v as f64) <= m.into_inner(),
            (Value::Float(v), Value::Int(m)) => v.into_inner() <= (*m as f64),
            _ => {
                return Err(RuntimeError::TypeMismatch {
                    expected: "Number".to_string(),
                    found: args[0].type_name().to_string(),
                    range,
                })
            }
        };
        if is_valid {
            Ok(args[0].clone())
        } else {
            validation_failure(
                &args,
                2,
                RuntimeError::TypeMismatch {
                    expected: format!("<= {}", args[1]),
                    found: format!("{}", args[0]),
                    range,
                },
                range,
            )
        }
    }
}

struct ValidatorOneOf;
impl RelonFunction for ValidatorOneOf {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        if !(2..=3).contains(&args.len()) {
            return Err(RuntimeError::TypeMismatch {
                expected: "2 or 3 args (value, list, message?)".to_string(),
                found: args.len().to_string(),
                range,
            });
        }
        if let Value::List(allowed) = &args[1] {
            if allowed.contains(&args[0]) {
                return Ok(args[0].clone());
            }
            return validation_failure(
                &args,
                2,
                RuntimeError::TypeMismatch {
                    expected: format!("one of {:?}", allowed),
                    found: format!("{}", args[0]),
                    range,
                },
                range,
            );
        }
        Err(RuntimeError::TypeMismatch {
            expected: "List for allowed values".to_string(),
            found: args[1].type_name().to_string(),
            range,
        })
    }
}

struct RequiredFields;
impl RelonFunction for RequiredFields {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        if !(2..=3).contains(&args.len()) {
            return Err(RuntimeError::TypeMismatch {
                expected: "2 or 3 args (dict, fields, message?)".to_string(),
                found: args.len().to_string(),
                range,
            });
        }
        let dict = expect_dict(&args[0], range)?;
        let fields = expect_string_list(&args[1], range)?;
        if let Some(field) = fields
            .iter()
            .find(|field| !dict.get(field.as_str()).is_some_and(Value::is_truthy))
        {
            return validation_failure(
                &args,
                2,
                RuntimeError::ValidationError(
                    format!("required field `{field}` is missing"),
                    range,
                ),
                range,
            );
        }
        Ok(args[0].clone())
    }
}

struct Requires;
impl RelonFunction for Requires {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        if !(3..=4).contains(&args.len()) {
            return Err(RuntimeError::TypeMismatch {
                expected: "3 or 4 args (dict, field, required_field, message?)".to_string(),
                found: args.len().to_string(),
                range,
            });
        }
        let dict = expect_dict(&args[0], range)?;
        let field = expect_string(&args[1], range)?;
        let required = expect_string(&args[2], range)?;
        let needs_required = dict.get(field).is_some_and(Value::is_truthy);
        let has_required = dict.get(required).is_some_and(Value::is_truthy);
        if needs_required && !has_required {
            return validation_failure(
                &args,
                3,
                RuntimeError::ValidationError(
                    format!("field `{required}` is required when `{field}` is truthy"),
                    range,
                ),
                range,
            );
        }
        Ok(args[0].clone())
    }
}

struct FieldEq;
impl RelonFunction for FieldEq {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        if !(3..=4).contains(&args.len()) {
            return Err(RuntimeError::TypeMismatch {
                expected: "3 or 4 args (dict, left_field, right_field, message?)".to_string(),
                found: args.len().to_string(),
                range,
            });
        }
        let dict = expect_dict(&args[0], range)?;
        let left = expect_string(&args[1], range)?;
        let right = expect_string(&args[2], range)?;
        if dict.get(left) != dict.get(right) {
            return validation_failure(
                &args,
                3,
                RuntimeError::ValidationError(
                    format!("fields `{left}` and `{right}` must be equal"),
                    range,
                ),
                range,
            );
        }
        Ok(args[0].clone())
    }
}

struct Split;
impl RelonFunction for Split {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args, 2, range)?;
        let input = expect_string(&args[0], range)?;
        let separator = expect_string(&args[1], range)?;
        if separator.is_empty() {
            return Err(RuntimeError::UnsupportedOperator(
                "split separator cannot be empty".to_string(),
                range,
            ));
        }
        Ok(Value::List(
            input
                .split(separator)
                .map(|part| Value::String(part.to_string()))
                .collect(),
        ))
    }
}

struct Join;
impl RelonFunction for Join {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args, 2, range)?;
        let values = expect_list(&args[0], range)?;
        let separator = expect_string(&args[1], range)?;
        let mut parts = Vec::with_capacity(values.len());
        for value in values {
            parts.push(expect_string(value, range)?.to_string());
        }
        Ok(Value::String(parts.join(separator)))
    }
}

struct Replace;
impl RelonFunction for Replace {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args, 3, range)?;
        Ok(Value::String(expect_string(&args[0], range)?.replace(
            expect_string(&args[1], range)?,
            expect_string(&args[2], range)?,
        )))
    }
}

struct ToUpper;
impl RelonFunction for ToUpper {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args, 1, range)?;
        Ok(Value::String(
            expect_string(&args[0], range)?.to_uppercase(),
        ))
    }
}

struct ToLower;
impl RelonFunction for ToLower {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args, 1, range)?;
        Ok(Value::String(
            expect_string(&args[0], range)?.to_lowercase(),
        ))
    }
}

struct Merge;
impl RelonFunction for Merge {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        if args.is_empty() {
            return Err(RuntimeError::TypeMismatch {
                expected: "at least 1 argument".to_string(),
                found: "0".to_string(),
                range,
            });
        }
        let mut merged = std::collections::HashMap::new();
        for value in &args {
            merged.extend(expect_dict(value, range)?.clone());
        }
        Ok(Value::Dict(merged))
    }
}

struct Keys;
impl RelonFunction for Keys {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args, 1, range)?;
        let mut keys = expect_dict(&args[0], range)?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        Ok(Value::List(keys.into_iter().map(Value::String).collect()))
    }
}

struct Values;
impl RelonFunction for Values {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args, 1, range)?;
        let dict = expect_dict(&args[0], range)?;
        let mut keys = dict.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        Ok(Value::List(
            keys.into_iter()
                .filter_map(|key| dict.get(&key).cloned())
                .collect(),
        ))
    }
}

struct Contains;
impl RelonFunction for Contains {
    fn call(
        &self,
        args: Vec<Value>,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args, 2, range)?;
        match &args[0] {
            Value::Dict(dict) => Ok(Value::Bool(
                dict.contains_key(expect_string(&args[1], range)?),
            )),
            Value::List(values) => Ok(Value::Bool(values.contains(&args[1]))),
            Value::String(input) => {
                Ok(Value::Bool(input.contains(expect_string(&args[1], range)?)))
            }
            other => Err(RuntimeError::TypeMismatch {
                expected: "Dict/List/String".to_string(),
                found: other.type_name().to_string(),
                range,
            }),
        }
    }
}

fn expect_arg_count(
    args: &[Value],
    expected: usize,
    range: relon_parser::TokenRange,
) -> Result<(), RuntimeError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(RuntimeError::TypeMismatch {
            expected: format!("{expected} argument(s)"),
            found: args.len().to_string(),
            range,
        })
    }
}

fn expect_string(value: &Value, range: relon_parser::TokenRange) -> Result<&str, RuntimeError> {
    match value {
        Value::String(value) => Ok(value),
        other => Err(RuntimeError::TypeMismatch {
            expected: "String".to_string(),
            found: other.type_name().to_string(),
            range,
        }),
    }
}

fn expect_list(value: &Value, range: relon_parser::TokenRange) -> Result<&[Value], RuntimeError> {
    match value {
        Value::List(value) => Ok(value),
        other => Err(RuntimeError::TypeMismatch {
            expected: "List".to_string(),
            found: other.type_name().to_string(),
            range,
        }),
    }
}

fn expect_string_list(
    value: &Value,
    range: relon_parser::TokenRange,
) -> Result<Vec<String>, RuntimeError> {
    let values = expect_list(value, range)?;
    let mut strings = Vec::with_capacity(values.len());
    for value in values {
        strings.push(expect_string(value, range)?.to_string());
    }
    Ok(strings)
}

fn expect_dict(
    value: &Value,
    range: relon_parser::TokenRange,
) -> Result<&std::collections::HashMap<String, Value>, RuntimeError> {
    match value {
        Value::Dict(value) => Ok(value),
        other => Err(RuntimeError::TypeMismatch {
            expected: "Dict".to_string(),
            found: other.type_name().to_string(),
            range,
        }),
    }
}

fn validation_failure(
    args: &[Value],
    message_index: usize,
    default: RuntimeError,
    range: relon_parser::TokenRange,
) -> Result<Value, RuntimeError> {
    if let Some(message) = args.get(message_index) {
        Err(RuntimeError::ValidationError(
            expect_string(message, range)?.to_string(),
            range,
        ))
    } else {
        Err(default)
    }
}
