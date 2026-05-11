use crate::error::RuntimeError;
use crate::eval::Context;
use crate::native_fn::{NativeArgs, NativeFnCaps, RelonFunction};
use crate::value::{Value, ValueDict};
use std::sync::Arc;

pub fn register_to(ctx: &mut Context) {
    // Language-level builtins — always in scope, no `#import` required.
    // See `docs/zh/guide/spec.md` §6.1: these are metadata operations
    // on data structures themselves, not std-module members.
    let len: Arc<dyn RelonFunction> = Arc::new(Len);
    ctx.register_pure_fn("len", Arc::clone(&len));
    ctx.register_pure_fn("_len", Arc::clone(&len));
    ctx.register_pure_fn("range", Arc::new(Range));
    ctx.register_pure_fn("type", Arc::new(Type));

    // Underscore intrinsics — the only Rust-side names in the
    // `std/<module>` namespace. `crates/relon-evaluator/src/std_relon/*.relon`
    // wraps them as the user-facing API; scripts reach the wrappers
    // via `@import("std/<module>", as=...)`. There is no top-level
    // `string.split` / `dict.merge` / ... — that would be a
    // runtime-private global, which the spec forbids (§1.1, §6).
    let list_map: Arc<dyn RelonFunction> = Arc::new(ListMap);
    let list_filter: Arc<dyn RelonFunction> = Arc::new(ListFilter);
    let list_reduce: Arc<dyn RelonFunction> = Arc::new(ListReduce);
    let list_contains: Arc<dyn RelonFunction> = Arc::new(ListContains);
    ctx.register_pure_fn("_list_map", Arc::clone(&list_map));
    ctx.register_pure_fn("_list_filter", Arc::clone(&list_filter));
    ctx.register_pure_fn("_list_reduce", Arc::clone(&list_reduce));
    ctx.register_pure_fn("_list_contains", Arc::clone(&list_contains));

    let string_split: Arc<dyn RelonFunction> = Arc::new(StringSplit);
    let string_join: Arc<dyn RelonFunction> = Arc::new(StringJoin);
    let string_replace: Arc<dyn RelonFunction> = Arc::new(StringReplace);
    let string_upper: Arc<dyn RelonFunction> = Arc::new(StringUpper);
    let string_lower: Arc<dyn RelonFunction> = Arc::new(StringLower);
    let string_contains: Arc<dyn RelonFunction> = Arc::new(StringContains);
    ctx.register_pure_fn("_string_split", Arc::clone(&string_split));
    ctx.register_pure_fn("_string_join", Arc::clone(&string_join));
    ctx.register_pure_fn("_string_replace", Arc::clone(&string_replace));
    ctx.register_pure_fn("_string_upper", Arc::clone(&string_upper));
    ctx.register_pure_fn("_string_lower", Arc::clone(&string_lower));
    ctx.register_pure_fn("_string_contains", Arc::clone(&string_contains));

    let dict_merge: Arc<dyn RelonFunction> = Arc::new(DictMerge);
    let dict_keys: Arc<dyn RelonFunction> = Arc::new(DictKeys);
    let dict_values: Arc<dyn RelonFunction> = Arc::new(DictValues);
    let dict_has_key: Arc<dyn RelonFunction> = Arc::new(DictHasKey);
    ctx.register_pure_fn("_dict_merge", Arc::clone(&dict_merge));
    ctx.register_pure_fn("_dict_keys", Arc::clone(&dict_keys));
    ctx.register_pure_fn("_dict_values", Arc::clone(&dict_values));
    ctx.register_pure_fn("_dict_has_key", Arc::clone(&dict_has_key));

    ctx.register_pure_fn("_math_abs", Arc::new(MathAbs));
    ctx.register_pure_fn("_math_max", Arc::new(MathMax));
    ctx.register_pure_fn("_math_min", Arc::new(MathMin));
    ctx.register_pure_fn("_math_clamp", Arc::new(MathClamp));

    // Schema-machinery validators. Spec §6.3 mandates these exist with
    // the documented semantics; they're consumed by the `#schema`
    // decorator, not by user-facing scripts directly.
    ctx.register_pure_fn("ensure.int", Arc::new(ValidatorInt));
    ctx.register_pure_fn("ensure.string", Arc::new(ValidatorString));
    ctx.register_pure_fn("ensure.bool", Arc::new(ValidatorBool));
    ctx.register_pure_fn("ensure.float", Arc::new(ValidatorFloat));
    ctx.register_pure_fn("ensure.list", Arc::new(ValidatorList));
    ctx.register_pure_fn("ensure.dict", Arc::new(ValidatorDict));
    ctx.register_pure_fn("ensure.at_least", Arc::new(ValidatorMin));
    ctx.register_pure_fn("ensure.at_most", Arc::new(ValidatorMax));
    ctx.register_pure_fn("ensure.one_of", Arc::new(ValidatorOneOf));
    ctx.register_pure_fn("ensure.required_fields", Arc::new(RequiredFields));
    ctx.register_pure_fn("ensure.requires", Arc::new(Requires));
    ctx.register_pure_fn("ensure.fields_equal", Arc::new(FieldEq));

    // Phase D 收尾: schema-rooted method aliases for the same Rust
    // intrinsics. Decision 14 (`schema-rooted-model-2026-05-11.md`):
    // `method` is the model's center; free-fn forms above remain for
    // backward compatibility and polymorphic-dispatch cases (`len(x)`
    // accepts String/List/Dict in one call). The aliases below let
    // `s.upper()`, `xs.map(f)`, `d.keys()` etc. dispatch directly
    // through the receiver-side `native_methods` table.
    //
    // Each handler accepts `(self, ...args)` as positional values; the
    // method-dispatch path in `Evaluator::try_call_native_method`
    // prepends the receiver before invoking, so the same `Arc<dyn
    // RelonFunction>` instance services both call shapes — no
    // adapter, no duplicate code path.
    //
    // Excluded from aliasing: `math.*`, `range`, `type`, `ensure.*`.
    // Decision 14 treats those as legitimate free-fn surface (numeric
    // helpers parameterized over a Number value, constructors,
    // reflection, validator combinators) — not type-rooted methods.
    //
    // `len` is special: it's polymorphic over String/List/Dict. We
    // keep the free-fn form (`len(x)`) and also expose `.len()` on
    // each of the three receivers so `s.len()` / `xs.len()` / `d.len()`
    // route through the same intrinsic.

    // String methods
    ctx.register_pure_method("String", "split", string_split);
    ctx.register_pure_method("String", "replace", string_replace);
    ctx.register_pure_method("String", "upper", string_upper);
    ctx.register_pure_method("String", "lower", string_lower);
    ctx.register_pure_method("String", "contains", Arc::clone(&string_contains));
    ctx.register_pure_method("String", "len", Arc::clone(&len));

    // List methods (note: `_string_join` takes `(List<T>, sep)`, so
    // its receiver is the List, not the String — register under List).
    ctx.register_pure_method("List", "map", list_map);
    ctx.register_pure_method("List", "filter", list_filter);
    ctx.register_pure_method("List", "reduce", list_reduce);
    ctx.register_pure_method("List", "contains", list_contains);
    ctx.register_pure_method("List", "join", string_join);
    ctx.register_pure_method("List", "len", Arc::clone(&len));

    // Dict methods
    ctx.register_pure_method("Dict", "merge", dict_merge);
    ctx.register_pure_method("Dict", "keys", dict_keys);
    ctx.register_pure_method("Dict", "values", dict_values);
    ctx.register_pure_method("Dict", "has_key", dict_has_key);
    ctx.register_pure_method("Dict", "len", len);

    // Decision 21 (Iterable lowering): each of String / List / Dict
    // gets an `iter()` that wraps the receiver into an `Iter`-branded
    // dict. The Comprehension evaluator (`Expr::Comprehension` arm in
    // `eval.rs`) recognizes this brand and drives iteration by reading
    // the wrapped `_source` plus `_kind` tag — `next()` itself is only
    // exposed as a witness slot for the `Iterable` constraint shape
    // check, not as a host-callable advance primitive (the iteration
    // state lives in the loop driver, not in a mutable Value).
    ctx.register_pure_method("List", "iter", Arc::new(IterFromList));
    ctx.register_pure_method("String", "iter", Arc::new(IterFromString));
    ctx.register_pure_method("Dict", "iter", Arc::new(IterFromDict));
    // `Iter.next()` is the user-callable advance primitive announced
    // by the `Iter<T>` core schema. Returns `Option<T>`: `Some` while
    // the cursor is in bounds, `None` once exhausted. The cursor lives
    // in a per-Context table (`Context::iter_cursors`), keyed by the
    // `_id` stamped into the Iter dict at construction time. See
    // schema-rooted-implementation-log §C.11 for the rationale.
    ctx.register_pure_method("Iter", "next", Arc::new(IterNext));
}

struct ListMap;
impl RelonFunction for ListMap {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args.positional, 2, range)?;
        let list = expect_list(&args.positional[0], range)?;
        let func = &args.positional[1];
        let caps = args.caps();
        let mut results = Vec::with_capacity(list.len());
        for item in list {
            // Tick once per scanned element so `max_steps` reflects the
            // real per-iteration work, not just the single AST call.
            caps.tick(1, range)?;
            results.push(caps.call_relon(func, vec![item.clone()], range)?);
        }
        Ok(Value::list(results))
    }
}

struct ListFilter;
impl RelonFunction for ListFilter {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args.positional, 2, range)?;
        let list = expect_list(&args.positional[0], range)?;
        let func = &args.positional[1];
        let caps = args.caps();
        let mut results = Vec::new();
        for item in list {
            caps.tick(1, range)?;
            if caps
                .call_relon(func, vec![item.clone()], range)?
                .is_truthy()
            {
                results.push(item.clone());
            }
        }
        Ok(Value::list(results))
    }
}

struct ListReduce;
impl RelonFunction for ListReduce {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args.positional, 3, range)?;
        let list = expect_list(&args.positional[0], range)?;
        let mut acc = args.positional[1].clone();
        let func = &args.positional[2];
        let caps = args.caps();
        for item in list {
            caps.tick(1, range)?;
            acc = caps.call_relon(func, vec![acc, item.clone()], range)?;
        }
        Ok(acc)
    }
}

struct MathAbs;
impl RelonFunction for MathAbs {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        match &args[0] {
            Value::Int(n) => Ok(Value::Int(n.abs())),
            Value::Float(f) => Ok(Value::Float(f.abs().into())),
            other => Err(RuntimeError::TypeMismatch {
                expected: "Number".to_string(),
                found: other.type_name().to_string(),
                range,
            }),
        }
    }
}

fn to_f64_val(v: &Value) -> f64 {
    match v {
        Value::Int(n) => *n as f64,
        Value::Float(f) => f.0,
        _ => 0.0,
    }
}

struct MathMax;
impl RelonFunction for MathMax {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        Ok(if to_f64_val(&args[0]) > to_f64_val(&args[1]) {
            args[0].clone()
        } else {
            args[1].clone()
        })
    }
}

struct MathMin;
impl RelonFunction for MathMin {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        Ok(if to_f64_val(&args[0]) < to_f64_val(&args[1]) {
            args[0].clone()
        } else {
            args[1].clone()
        })
    }
}

struct MathClamp;
impl RelonFunction for MathClamp {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 3, range)?;
        let val = to_f64_val(&args[0]);
        let min = to_f64_val(&args[1]);
        let max = to_f64_val(&args[2]);
        Ok(if val < min {
            args[1].clone()
        } else if val > max {
            args[2].clone()
        } else {
            args[0].clone()
        })
    }
}

struct Len;
impl RelonFunction for Len {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        match &args[0] {
            Value::String(s) => Ok(Value::Int(s.len() as i64)),
            Value::List(l) => Ok(Value::Int(l.len() as i64)),
            Value::Dict(d) => Ok(Value::Int(d.map.len() as i64)),
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
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let caps_handle = args.caps();
        let caps_max = caps_handle.max_value_elements();
        let positional = args.positional.clone();
        let (start, end) = match positional.len() {
            1 => (0, expect_int(&positional[0], range)?),
            2 => (
                expect_int(&positional[0], range)?,
                expect_int(&positional[1], range)?,
            ),
            _ => {
                return Err(RuntimeError::TypeMismatch {
                    expected: "1 or 2 arguments".to_string(),
                    found: format!("{}", positional.len()),
                    range,
                })
            }
        };
        let requested_len = (end as i128 - start as i128).max(0) as u128;
        // Step-budget pre-flight: charge the full requested length
        // *before* allocating. Complements `max_value_elements` —
        // a host that leaves `max_value_elements = None` but sets
        // `max_steps = Some(1_000)` still refuses `range(0, 10M)`
        // because the tick budget exhausts before we ever reach the
        // `Vec<Value>::with_capacity` call inside `collect`. Cheap
        // path: `tick` is a no-op when `max_steps` is None.
        if requested_len > 0 {
            let ticks = if requested_len > u64::MAX as u128 {
                u64::MAX
            } else {
                requested_len as u64
            };
            caps_handle.tick(ticks, range)?;
        }
        // Pre-flight enforcement of `Capabilities::max_value_elements`.
        // Without this an oversized request (`range(0, 10_000_000_000)`)
        // would allocate the full `Vec<Value>` before the evaluator's
        // post-call `check_value_size` ever runs — OOM-ing the host long
        // before the cap fires. Compare the requested length (saturating
        // to handle inverted ranges and `i64` underflow) against the cap
        // up front and refuse early. The post-call catch-all in
        // `Evaluator::call_function` is still the authority for the
        // narrow `actual == limit + 1` race; this check just stops the
        // allocator from being weaponized.
        if let Some(limit) = caps_max {
            if requested_len > limit as u128 {
                let actual = if requested_len > usize::MAX as u128 {
                    usize::MAX
                } else {
                    requested_len as usize
                };
                return Err(RuntimeError::ValueTooLarge {
                    limit,
                    actual,
                    range,
                });
            }
        }
        Ok(Value::list((start..end).map(Value::Int).collect()))
    }
}

fn expect_int(value: &Value, range: relon_parser::TokenRange) -> Result<i64, RuntimeError> {
    match value {
        Value::Int(value) => Ok(*value),
        other => Err(RuntimeError::TypeMismatch {
            expected: "Int".to_string(),
            found: other.type_name().to_string(),
            range,
        }),
    }
}

struct Type;
impl RelonFunction for Type {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        Ok(Value::String(args[0].type_name().to_string()))
    }
}

macro_rules! type_validator {
    ($struct_name:ident, $variant:ident, $expected:expr) => {
        struct $struct_name;
        impl RelonFunction for $struct_name {
            fn call(
                &self,
                args: NativeArgs,
                range: relon_parser::TokenRange,
            ) -> Result<Value, RuntimeError> {
                let args = args.into_positional();
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
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
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
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
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
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
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
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
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
            .find(|field| !dict.map.contains_key(field.as_str()))
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
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
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
        let needs_required = dict.map.get(field).is_some_and(Value::is_truthy);
        let has_required = dict.map.get(required).is_some_and(Value::is_truthy);
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
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
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
        if dict.map.get(left) != dict.map.get(right) {
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

struct StringSplit;
impl RelonFunction for StringSplit {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let caps = args.caps();
        let args = args.positional.clone();
        expect_arg_count(&args, 2, range)?;
        let input = expect_string(&args[0], range)?;
        let separator = expect_string(&args[1], range)?;
        if separator.is_empty() {
            return Err(RuntimeError::UnsupportedOperator(
                "split separator cannot be empty".to_string(),
                range,
            ));
        }
        // Build the result piece-by-piece so we can tick once per
        // emitted output. Mirrors `_string_split`'s shape (returns the
        // same `List<String>`) but routes through the step budget.
        let mut parts = Vec::new();
        for part in input.split(separator) {
            caps.tick(1, range)?;
            parts.push(Value::String(part.to_string()));
        }
        Ok(Value::list(parts))
    }
}

struct StringJoin;
impl RelonFunction for StringJoin {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let caps = args.caps();
        let args = args.positional.clone();
        expect_arg_count(&args, 2, range)?;
        let values = expect_list(&args[0], range)?;
        let separator = expect_string(&args[1], range)?;
        let mut parts = Vec::with_capacity(values.len());
        for value in values {
            caps.tick(1, range)?;
            parts.push(format!("{}", value));
        }
        Ok(Value::String(parts.join(separator)))
    }
}

struct StringReplace;
impl RelonFunction for StringReplace {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let caps = args.caps();
        let args = args.positional.clone();
        expect_arg_count(&args, 3, range)?;
        let input = expect_string(&args[0], range)?;
        let from = expect_string(&args[1], range)?;
        let to = expect_string(&args[2], range)?;
        // Charge one tick per replacement found. Empty `from` would
        // make `String::replace` insert `to` at every boundary
        // (codepoint count + 1); we tick by that count too so the
        // budget reflects the actual edit work.
        let occurrences = if from.is_empty() {
            input.chars().count() + 1
        } else {
            input.matches(from).count()
        };
        if occurrences > 0 {
            caps.tick(occurrences as u64, range)?;
        }
        Ok(Value::String(input.replace(from, to)))
    }
}

struct StringUpper;
impl RelonFunction for StringUpper {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        Ok(Value::String(
            expect_string(&args[0], range)?.to_uppercase(),
        ))
    }
}

struct StringLower;
impl RelonFunction for StringLower {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        Ok(Value::String(
            expect_string(&args[0], range)?.to_lowercase(),
        ))
    }
}

struct StringContains;
impl RelonFunction for StringContains {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        Ok(Value::Bool(
            expect_string(&args[0], range)?.contains(expect_string(&args[1], range)?),
        ))
    }
}

struct DictMerge;
impl RelonFunction for DictMerge {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let caps = args.caps();
        let args = args.positional.clone();
        if args.is_empty() {
            return Err(RuntimeError::TypeMismatch {
                expected: "at least 1 argument".to_string(),
                found: "0".to_string(),
                range,
            });
        }
        let mut result = args[0].clone();
        for patch in args.iter().skip(1) {
            // Charge one tick per top-level key in the patch. Nested
            // dict merges recurse inside `deep_merge`, but the top-level
            // key count is a fair proxy for the work this merge does
            // at this level — large flat patches now cost proportional
            // budget.
            if let Value::Dict(d) = patch {
                if !d.map.is_empty() {
                    caps.tick(d.map.len() as u64, range)?;
                }
            }
            result.deep_merge(patch);
        }
        if matches!(result, Value::Dict(_)) {
            Ok(result)
        } else {
            Err(RuntimeError::TypeMismatch {
                expected: "Dict".to_string(),
                found: result.type_name().to_string(),
                range,
            })
        }
    }
}

struct DictKeys;
impl RelonFunction for DictKeys {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let caps = args.caps();
        let args = args.positional.clone();
        expect_arg_count(&args, 1, range)?;
        let map = &expect_dict(&args[0], range)?.map;
        // Charge for every scanned entry — keys() iterates the whole
        // BTreeMap and sorts it, so the per-entry cost is real.
        if !map.is_empty() {
            caps.tick(map.len() as u64, range)?;
        }
        let mut keys = map.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        Ok(Value::list(keys.into_iter().map(Value::String).collect()))
    }
}

struct DictValues;
impl RelonFunction for DictValues {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let caps = args.caps();
        let args = args.positional.clone();
        expect_arg_count(&args, 1, range)?;
        let dict = expect_dict(&args[0], range)?;
        if !dict.map.is_empty() {
            caps.tick(dict.map.len() as u64, range)?;
        }
        let mut keys = dict.map.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        Ok(Value::list(
            keys.into_iter()
                .filter_map(|key| dict.map.get(&key).cloned())
                .collect(),
        ))
    }
}

struct DictHasKey;
impl RelonFunction for DictHasKey {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        Ok(Value::Bool(
            expect_dict(&args[0], range)?
                .map
                .contains_key(expect_string(&args[1], range)?),
        ))
    }
}

/// Iter-builder for `List<T>.iter()`. Decision 21 (Iterable lowering):
/// wraps the receiver list into an `Iter`-branded dict consumed by the
/// `Expr::Comprehension` evaluator. The wrapped representation is
/// deliberately a plain dict so the rest of the runtime (clone, brand
/// dispatch, serialization fallbacks) keeps working unchanged.
struct IterFromList;
impl RelonFunction for IterFromList {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let caps = args.caps();
        let args = args.positional.clone();
        expect_arg_count(&args, 1, range)?;
        // expect_list validates the receiver shape; the value itself
        // is what we wrap (cheap Arc clone — no element copy).
        let _ = expect_list(&args[0], range)?;
        Ok(make_iter_value(caps, "list", args[0].clone()))
    }
}

/// Iter-builder for `String.iter()`. The element type is `String`
/// (one-char-per-step). UTF-8 boundary aware via `String::chars`.
struct IterFromString;
impl RelonFunction for IterFromString {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let caps = args.caps();
        let args = args.positional.clone();
        expect_arg_count(&args, 1, range)?;
        let _ = expect_string(&args[0], range)?;
        Ok(make_iter_value(caps, "string", args[0].clone()))
    }
}

/// Iter-builder for `Dict<K, V>.iter()`. Entries iterate in sorted key
/// order (matches `Dict.keys()`). Element shape per step is a 2-tuple
/// `(K, V)` encoded as `Value::list([k, v])` since the runtime does
/// not have a dedicated Tuple value variant.
struct IterFromDict;
impl RelonFunction for IterFromDict {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let caps = args.caps();
        let args = args.positional.clone();
        expect_arg_count(&args, 1, range)?;
        let _ = expect_dict(&args[0], range)?;
        Ok(make_iter_value(caps, "dict_entries", args[0].clone()))
    }
}

/// User-callable `Iter.next()` advance primitive — returns the next
/// element wrapped in `Option.Some { value: ... }`, or `Option.None {}`
/// once the underlying source is exhausted. The cursor itself lives in
/// a per-Context table (`Context::iter_cursors`); the immutable-
/// `Value` invariant (`Arc`-shared, no interior mutability) rules out
/// storing a per-instance cursor inside the dict directly.
/// Implementation log §C.11 captures the rationale for siting the
/// cursor table on `Context`.
///
/// Tenant isolation: each `Context` owns its own cursor table and id
/// counter. Two concurrent Contexts never see each other's cursors,
/// and dropping a Context releases every cursor it owned. An `Iter`
/// value built in Context A and used in Context B reads as exhausted
/// (`None`) because B's table has no entry for A's `_id`.
///
/// Semantic notes:
/// * Aliased iterators (`Iter<Int> it2: it`) share the same `_id` and
///   therefore the same cursor — the standard "iterator handle" model.
///   A user who wants a fresh cursor re-calls `xs.iter()`.
/// * Returning `Option.None {}` is idempotent: continuing to call
///   `next()` after exhaustion keeps returning `None`. The cursor
///   stops advancing once it reaches `len`.
/// * `Iter.next()` does **not** drive `for x in c: ...` /
///   `[for x in c: ...]` comprehensions. Those go through
///   `materialize_iterable` in `eval.rs` which reads `_kind`/`_source`
///   directly — faster than per-element host-fn dispatch and lets the
///   comprehension's iteration count stay independent of any prior
///   `next()` calls on the same `Iter` value.
struct IterNext;
impl RelonFunction for IterNext {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let caps = args.caps();
        let args = args.positional.clone();
        expect_arg_count(&args, 1, range)?;
        let iter_dict = expect_dict(&args[0], range)?;
        if iter_dict.brand.as_deref() != Some("Iter") {
            return Err(RuntimeError::TypeMismatch {
                expected: "Iter".to_string(),
                found: iter_dict
                    .brand
                    .clone()
                    .unwrap_or_else(|| "Dict".to_string()),
                range,
            });
        }
        let kind = iter_dict
            .map
            .get("_kind")
            .and_then(|v| match v {
                Value::String(s) => Some(s.as_str()),
                _ => None,
            })
            .ok_or_else(|| RuntimeError::TypeMismatch {
                expected: "Iter with `_kind` String field".to_string(),
                found: "Iter without `_kind`".to_string(),
                range,
            })?;
        let source = iter_dict
            .map
            .get("_source")
            .ok_or_else(|| RuntimeError::TypeMismatch {
                expected: "Iter with `_source` field".to_string(),
                found: "Iter without `_source`".to_string(),
                range,
            })?;
        let iter_id = iter_dict
            .map
            .get("_id")
            .and_then(|v| match v {
                Value::Int(i) => Some(*i as u64),
                _ => None,
            })
            .ok_or_else(|| RuntimeError::TypeMismatch {
                expected: "Iter with `_id` Int field".to_string(),
                found: "Iter without `_id`".to_string(),
                range,
            })?;
        // Per-kind: compute element-count, then atomically advance the
        // cursor. `iter_cursor_fetch_and_inc` performs the bounded
        // check and increment under one critical section so concurrent
        // advances on the same id remain consistent.
        let element = match kind {
            "list" => {
                let items = match source {
                    Value::List(l) => l,
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "List source for Iter(kind=list)".to_string(),
                            found: other.type_name().to_string(),
                            range,
                        })
                    }
                };
                caps.iter_cursor_fetch_and_inc(iter_id, items.len())
                    .map(|idx| items[idx].clone())
            }
            "string" => {
                let s = match source {
                    Value::String(s) => s,
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "String source for Iter(kind=string)".to_string(),
                            found: other.type_name().to_string(),
                            range,
                        })
                    }
                };
                // Char count, not byte length — `_kind=string` iter is
                // one element per codepoint. We re-walk the string each
                // call: O(n) per next(), so a hot loop is O(n²). The
                // alternative (cache the char vec) is left for a future
                // optimization — user-driven iteration is a rare path,
                // and comprehensions take the fast `materialize_iterable`
                // route.
                let chars: Vec<char> = s.chars().collect();
                caps.iter_cursor_fetch_and_inc(iter_id, chars.len())
                    .map(|idx| Value::String(chars[idx].to_string()))
            }
            "dict_entries" => {
                let src_dict = match source {
                    Value::Dict(d) => d,
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "Dict source for Iter(kind=dict_entries)".to_string(),
                            found: other.type_name().to_string(),
                            range,
                        })
                    }
                };
                // Key-sort each call. Same O(n log n) per `next()` as
                // the char-vec rebuild above; comprehension fast path
                // avoids this entirely. Matches the iteration order
                // used by `materialize_iterable` so user-side
                // `it.next()` walks pairs in the same order a
                // `for kv in d.iter()` would.
                let mut keys: Vec<&String> = src_dict.map.keys().collect();
                keys.sort();
                caps.iter_cursor_fetch_and_inc(iter_id, keys.len())
                    .map(|idx| {
                        let key: &String = keys[idx];
                        let v = src_dict.map.get(key).cloned().unwrap_or(Value::Null);
                        Value::list(vec![Value::String(key.clone()), v])
                    })
            }
            other => {
                return Err(RuntimeError::TypeMismatch {
                    expected: "Iter._kind in {list, string, dict_entries}".to_string(),
                    found: other.to_string(),
                    range,
                })
            }
        };
        Ok(option_value(element))
    }
}

/// Build an `Option.Some { value }` (when `inner` is `Some`) or
/// `Option.None {}` variant dict. Matches the prelude's `Option<T>`
/// tagged-enum shape so downstream `match`/projection sees a normal
/// `Option` value.
fn option_value(inner: Option<Value>) -> Value {
    match inner {
        Some(v) => {
            let mut map = std::collections::BTreeMap::new();
            map.insert("value".to_string(), v);
            Value::variant_dict(map, "Some".to_string(), "Option".to_string())
        }
        None => Value::variant_dict(
            std::collections::BTreeMap::new(),
            "None".to_string(),
            "Option".to_string(),
        ),
    }
}

/// Build an `Iter`-branded dict carrying `_kind` (driver dispatch tag),
/// `_source` (the underlying collection value), and `_id` (the
/// per-construction cursor key consumed by `Iter.next()`). The
/// Comprehension evaluator (`materialize_iterable` in `eval.rs`) reads
/// only `_kind`/`_source` and walks the source directly — it does not
/// advance the cursor table, so user-driven `next()` and a
/// comprehension over the same iter remain independent.
///
/// The cursor table itself lives on [`crate::eval::Context`] — see
/// `Context::iter_cursors` / `Context::next_iter_id`. We reach it via
/// the [`NativeFnCaps`] handle so this intrinsic stays Context-
/// agnostic and so cursor state never leaks into process-global
/// storage. Cursors clear at the top of every `eval_root` /
/// `run_main`, so a Context reused across top-level runs never
/// accumulates entries.
pub(crate) fn make_iter_value(caps: &dyn NativeFnCaps, kind: &str, source: Value) -> Value {
    let mut map = std::collections::BTreeMap::new();
    map.insert("_kind".to_string(), Value::String(kind.to_string()));
    map.insert("_source".to_string(), source);
    // `_id` is `i64`-coerced from a `u64` so the existing
    // `Value::Int(i64)` representation can carry it without inventing
    // a new variant. `IterNext` reads it back via `as u64` round-trip.
    map.insert("_id".to_string(), Value::Int(caps.next_iter_id() as i64));
    Value::branded_dict(map, Some("Iter".to_string()))
}

struct ListContains;
impl RelonFunction for ListContains {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let caps = args.caps();
        let args = args.positional.clone();
        expect_arg_count(&args, 2, range)?;
        let list = expect_list(&args[0], range)?;
        // Tick per scanned element. Early-return on match is fine —
        // we still charge for the elements actually compared, so a hit
        // near the front stays cheap.
        let needle = &args[1];
        for item in list {
            caps.tick(1, range)?;
            if item == needle {
                return Ok(Value::Bool(true));
            }
        }
        Ok(Value::Bool(false))
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

fn expect_dict(value: &Value, range: relon_parser::TokenRange) -> Result<&ValueDict, RuntimeError> {
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

#[cfg(test)]
mod purity_guard {
    /// stdlib intrinsics must remain structurally pure: no I/O, no
    /// clocks, no RNG, no env. The 6-bit capability model gates host
    /// fns; this test guards that nobody quietly adds `use std::fs;`
    /// (etc.) to `stdlib.rs` and bypasses the gate.
    ///
    /// If a real ambient capability is needed (e.g. `std/time`),
    /// expose it as a host-facing module via `register_fn(name, gate, fn)`
    /// with the matching `NativeFnGate` bit set, *not* as an ungated
    /// stdlib intrinsic.
    #[test]
    fn stdlib_rs_uses_no_ambient_apis() {
        let source = include_str!("stdlib.rs");
        // Trim this test's own banned-list literals and the leading
        // doc comment so the scan doesn't flag itself.
        let source = match source.find("#[cfg(test)]\nmod purity_guard") {
            Some(idx) => &source[..idx],
            None => source,
        };
        let banned = [
            "std::fs",
            "std::env",
            "std::net",
            "std::process",
            "SystemTime",
            "Instant::now",
            "rand::",
            "chrono::",
            "tokio::fs",
            "tokio::net",
            "reqwest",
        ];
        for needle in banned {
            assert!(
                !source.contains(needle),
                "stdlib.rs must not reference `{needle}` — ambient state must be a gated host fn (use `register_fn` with a `NativeFnGate` bit), not an ungated stdlib intrinsic.",
            );
        }
    }
}
