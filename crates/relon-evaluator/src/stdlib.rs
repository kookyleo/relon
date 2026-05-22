use crate::error::RuntimeError;
use crate::native_fn::{NativeArgs, NativeFnCaps, RelonFunction};
use crate::value::{Value, ValueDict};
use relon_eval_api::context::Context;
use relon_eval_api::SmolStr;
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
    let string_title: Arc<dyn RelonFunction> = Arc::new(StringTitle);
    // v3++ b-6: locale-aware case folding. Surface names mirror the
    // wasm-AOT stdlib slots; the dispatch path inside `fold_string`
    // honours the Turkish / Azerbaijani overrides via the second
    // `String` parameter.
    let string_upper_locale: Arc<dyn RelonFunction> = Arc::new(StringUpperLocale);
    let string_lower_locale: Arc<dyn RelonFunction> = Arc::new(StringLowerLocale);
    let string_title_locale: Arc<dyn RelonFunction> = Arc::new(StringTitleLocale);
    // v3++ b-5: Unicode normalization (UAX #15). All four arcs delegate
    // to `relon_ir::normalization`, the shared algorithm the wasm-AOT
    // backend also embeds — so both executors stay byte-for-byte in
    // lock-step against UCD 14.0.0.
    let string_nfc: Arc<dyn RelonFunction> = Arc::new(StringNfc);
    let string_nfd: Arc<dyn RelonFunction> = Arc::new(StringNfd);
    let string_nfkc: Arc<dyn RelonFunction> = Arc::new(StringNfkc);
    let string_nfkd: Arc<dyn RelonFunction> = Arc::new(StringNfkd);
    let string_contains: Arc<dyn RelonFunction> = Arc::new(StringContains);
    // 2026-05-21: Tier-2 glob matcher. Surface name `glob_match`
    // matches the bundled stdlib slot 37; the cranelift backend
    // intercepts the same fn_index and routes through a host helper
    // for AOT-compiled scripts.
    let string_glob_match: Arc<dyn RelonFunction> = Arc::new(StringGlobMatch);
    ctx.register_pure_fn("_string_split", Arc::clone(&string_split));
    ctx.register_pure_fn("_string_join", Arc::clone(&string_join));
    ctx.register_pure_fn("_string_replace", Arc::clone(&string_replace));
    ctx.register_pure_fn("_string_upper", Arc::clone(&string_upper));
    ctx.register_pure_fn("_string_lower", Arc::clone(&string_lower));
    ctx.register_pure_fn("_string_title", Arc::clone(&string_title));
    ctx.register_pure_fn("_string_upper_locale", Arc::clone(&string_upper_locale));
    ctx.register_pure_fn("_string_lower_locale", Arc::clone(&string_lower_locale));
    ctx.register_pure_fn("_string_title_locale", Arc::clone(&string_title_locale));
    ctx.register_pure_fn("_string_nfc", Arc::clone(&string_nfc));
    ctx.register_pure_fn("_string_nfd", Arc::clone(&string_nfd));
    ctx.register_pure_fn("_string_nfkc", Arc::clone(&string_nfkc));
    ctx.register_pure_fn("_string_nfkd", Arc::clone(&string_nfkd));
    ctx.register_pure_fn("_string_contains", Arc::clone(&string_contains));
    ctx.register_pure_fn("_string_glob_match", Arc::clone(&string_glob_match));
    ctx.register_pure_fn("glob_match", Arc::clone(&string_glob_match));

    let dict_merge: Arc<dyn RelonFunction> = Arc::new(DictMerge);
    let dict_keys: Arc<dyn RelonFunction> = Arc::new(DictKeys);
    let dict_values: Arc<dyn RelonFunction> = Arc::new(DictValues);
    let dict_has_key: Arc<dyn RelonFunction> = Arc::new(DictHasKey);
    ctx.register_pure_fn("_dict_merge", Arc::clone(&dict_merge));
    ctx.register_pure_fn("_dict_keys", Arc::clone(&dict_keys));
    ctx.register_pure_fn("_dict_values", Arc::clone(&dict_values));
    ctx.register_pure_fn("_dict_has_key", Arc::clone(&dict_has_key));

    let math_abs: Arc<dyn RelonFunction> = Arc::new(MathAbs);
    let math_max: Arc<dyn RelonFunction> = Arc::new(MathMax);
    let math_min: Arc<dyn RelonFunction> = Arc::new(MathMin);
    let math_clamp: Arc<dyn RelonFunction> = Arc::new(MathClamp);
    ctx.register_pure_fn("_math_abs", Arc::clone(&math_abs));
    ctx.register_pure_fn("_math_max", Arc::clone(&math_max));
    ctx.register_pure_fn("_math_min", Arc::clone(&math_min));
    ctx.register_pure_fn("_math_clamp", Arc::clone(&math_clamp));
    // v6-δ M1 R4: also register the bare names so corpus / IR sources
    // that call `abs(x)` / `min(a, b)` / `max(a, b)` / `clamp(v, lo, hi)`
    // directly (mirroring the cranelift backend's stdlib free-fn
    // surface) don't surface `FunctionNotFound` against the tree-walker.
    // The relon-side wrapper modules at `std_relon/math.relon` keep
    // working — `@import("std/math", as=math); math.abs(...)` reaches
    // the same handlers via `_math_abs` etc.
    ctx.register_pure_fn("abs", Arc::clone(&math_abs));
    ctx.register_pure_fn("max", Arc::clone(&math_max));
    ctx.register_pure_fn("min", Arc::clone(&math_min));
    ctx.register_pure_fn("clamp", math_clamp);

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
    // method-dispatch path in `TreeWalkEvaluator::try_call_native_method`
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
    // v3++ b-4: word-boundary aware title case. Mirrors the
    // wasm-AOT body (`crates/relon-ir/src/stdlib.rs::title_string`)
    // — split on Unicode whitespace, upper-case the first cased
    // codepoint of each word, lower-case the rest, and keep
    // combining marks attached to their base cluster.
    ctx.register_pure_method("String", "title", string_title);
    // v3++ b-6: locale-aware case folding methods. Surface names
    // `upper_locale` / `lower_locale` / `title_locale` accept the
    // locale string as the second argument; method-form
    // `s.upper_locale("tr")` and free-form `upper_locale(s, "tr")`
    // both route through the same handler.
    ctx.register_pure_method("String", "upper_locale", string_upper_locale);
    ctx.register_pure_method("String", "lower_locale", string_lower_locale);
    ctx.register_pure_method("String", "title_locale", string_title_locale);
    // v3++ b-5: Unicode normalization forms. Each method delegates to
    // the shared `relon_ir::normalization` algorithm; the wasm-AOT body
    // walks the same data tables so backend tests can compare results
    // byte-for-byte.
    ctx.register_pure_method("String", "nfc", string_nfc);
    ctx.register_pure_method("String", "nfd", string_nfd);
    ctx.register_pure_method("String", "nfkc", string_nfkc);
    ctx.register_pure_method("String", "nfkd", string_nfkd);
    ctx.register_pure_method("String", "contains", Arc::clone(&string_contains));
    ctx.register_pure_method("String", "glob_match", Arc::clone(&string_glob_match));
    ctx.register_pure_method("String", "len", Arc::clone(&len));
    // v6-δ M1 R4: corpus / IR-side sources use `length()` /
    // `is_empty()` as method aliases for the same intrinsics the
    // wasm-AOT / cranelift backend register. Adding the aliases on
    // both String and List keeps the three-way differential corpus
    // honest without forcing users to remember which length-flavour
    // each backend speaks.
    ctx.register_pure_method("String", "length", Arc::clone(&len));
    ctx.register_pure_method("String", "is_empty", Arc::new(IsEmpty));
    ctx.register_pure_method("String", "concat", Arc::new(StringConcat));
    ctx.register_pure_method("String", "substring", Arc::new(StringSubstring));
    ctx.register_pure_method("String", "starts_with", Arc::new(StringStartsWith));

    // List methods (note: `_string_join` takes `(List<T>, sep)`, so
    // its receiver is the List, not the String — register under List).
    ctx.register_pure_method("List", "map", list_map);
    ctx.register_pure_method("List", "filter", list_filter);
    ctx.register_pure_method("List", "reduce", list_reduce);
    ctx.register_pure_method("List", "contains", list_contains);
    ctx.register_pure_method("List", "join", string_join);
    ctx.register_pure_method("List", "len", Arc::clone(&len));
    // v6-δ M1 R4: see String.length / String.is_empty above for
    // rationale. `sum` + `max` are list-aggregations the cranelift
    // backend already exposes as `list_int_sum` etc.; `length` is the
    // `len()` alias the corpus uses.
    ctx.register_pure_method("List", "length", Arc::clone(&len));
    ctx.register_pure_method("List", "is_empty", Arc::new(IsEmpty));
    ctx.register_pure_method("List", "sum", Arc::new(ListSum));
    ctx.register_pure_method("List", "max", Arc::new(ListMax));

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
        // `TreeWalkEvaluator::call_function` is still the authority for the
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
        Ok(Value::String(args[0].type_name().into()))
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
            parts.push(Value::String(part.into()));
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
        Ok(Value::String(parts.join(separator).into()))
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
        Ok(Value::String(input.replace(from, to).into()))
    }
}

/// v3++ b-6: shared case-fold engine used by `upper` / `lower` /
/// `title` / `upper_locale` / `lower_locale` / `title_locale`.
///
/// Walks the input codepoint-by-codepoint and emits one of the
/// following per cp, in priority order:
///
///   1. Turkish / Azerbaijani override (only when `locale_turkish`).
///   2. Final-sigma context (only for lower mode, when cp == U+03A3).
///   3. FULL multi-codepoint folding (UAX #21 unconditional table).
///   4. Rust stdlib full case mapping (`char::to_uppercase` /
///      `char::to_lowercase`) — already pulls UCD data, gives us the
///      remaining simple + multi-cp behaviour for free.
///   5. Identity (combining marks pass through unchanged when not
///      `at_word_start` for the title flow).
fn fold_string(s: &str, mode: CaseFoldMode, locale_turkish: bool) -> String {
    fold_string_with_ascii_hint(s, mode, locale_turkish, AsciiHint::Unknown)
}

/// Tier 2c (#153) classification hint passed in from the caller.
///
/// Surface bodies (`upper` / `lower` / `title` / locale variants) call
/// in via the plain [`fold_string`] entry point and supply
/// [`AsciiHint::Unknown`]; the fast path then runs its usual SIMD
/// scan to decide whether to skip the slow per-codepoint loop. When
/// a future caller can prove the input is pure ASCII upstream — e.g.
/// the StringRef record's [`relon_trace_abi::STRING_RECORD_ASCII_FLAG_BIT`]
/// is set after intern / record-build — it can pass
/// [`AsciiHint::AllAscii`] to skip the per-call scan entirely.
///
/// `KnownNonAscii` lets a future intern-table classifier report the
/// opposite fact and skip the SIMD scan in the other direction; the
/// slow path runs over the whole input from codepoint 0. v3++ b-6
/// has no callers passing this yet, but the variant is here so the
/// fold engine has the full state space rather than a default-true
/// / default-false split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "AllAscii / KnownNonAscii are reached only by the #153 parity tests today; surface call site lands in the follow-up that plumbs the StringRef ASCII flag bit into the evaluator's Value -> &str path"
)]
enum AsciiHint {
    /// Caller has not classified the input. The fold engine runs the
    /// SIMD scan + fast path as before.
    Unknown,
    /// Caller has proven the input is all `< 0x80`. The fold engine
    /// skips the SIMD scan and goes straight to the mask + xor
    /// (or Title walker) over the whole payload.
    AllAscii,
    /// Caller has proven the input contains at least one byte
    /// `>= 0x80`. The fold engine skips the SIMD scan and goes
    /// straight to the per-codepoint slow path.
    KnownNonAscii,
}

/// Tier 2c (#153) entry point that lets the caller surface the
/// ASCII-flag fact bypassing the per-call SIMD scan.
///
/// Identical UAX #21 semantics to [`fold_string`]; the only difference
/// is that an [`AsciiHint::AllAscii`] caller saves one
/// `scan_ascii_prefix` pass per fold (~3 cycles / byte after auto-
/// vectorisation).
fn fold_string_with_ascii_hint(
    s: &str,
    mode: CaseFoldMode,
    locale_turkish: bool,
    ascii_hint: AsciiHint,
) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at_word_start = true;

    // v3++ item 4: SIMD ASCII fast-path. ASCII letters fold by
    // mask-and-xor (b ^ 0x20) and the FULL / Σ / combining-mark
    // tables only contain non-ASCII inputs, so for the (very
    // frequent) all-ASCII case we can skip the per-cp decode + table
    // lookup entirely. Turkish locale is opted out because its
    // overrides `I <-> ı` / `i <-> İ` produce 2-byte UTF-8 output
    // from ASCII input, which the byte-in / byte-out fast path can't
    // express.
    //
    // Tier 2c (#153): when the caller has already proven the payload
    // is pure ASCII (e.g. the StringRef record's flag bit is set), we
    // route through `case_fold_ascii_fast_into_string` and skip the
    // per-call `scan_ascii_prefix` SIMD pass. Saves ~3 cycles / byte
    // on the auto-vec scan + the entry into the mask+xor loop.
    //
    // The fast path appends folded bytes directly into `out`'s UTF-8
    // buffer (every byte is `< 0x80`, hence a 1-byte UTF-8 codepoint).
    // It returns the number of *bytes* consumed and the updated
    // `at_word_start` flag. Because the prefix is ASCII, byte index
    // == codepoint index, so the slow loop below resumes at
    // codepoint index `consumed` with no re-decoding.
    let fast_mode = match mode {
        CaseFoldMode::Upper => Some(relon_ir::ascii_fold_simd::AsciiFoldMode::Upper),
        CaseFoldMode::Lower => Some(relon_ir::ascii_fold_simd::AsciiFoldMode::Lower),
        CaseFoldMode::Title => Some(relon_ir::ascii_fold_simd::AsciiFoldMode::Title),
    };
    let fast_consumed = if !locale_turkish {
        if let Some(fm) = fast_mode {
            match ascii_hint {
                AsciiHint::AllAscii => {
                    // Caller has guaranteed every byte is `< 0x80`.
                    // Skip the scan and consume the whole payload.
                    let r = relon_ir::ascii_fold_simd::case_fold_ascii_fast_into_string(
                        s.as_bytes(),
                        fm,
                        at_word_start,
                        &mut out,
                    );
                    at_word_start = r.at_word_start;
                    r.consumed
                }
                AsciiHint::KnownNonAscii => {
                    // Caller has guaranteed there is at least one
                    // non-ASCII byte. Skip the SIMD scan and let the
                    // per-codepoint slow path handle the whole
                    // payload starting at index 0.
                    0
                }
                AsciiHint::Unknown => {
                    let r = relon_ir::ascii_fold_simd::fold_ascii_prefix_into_string(
                        s.as_bytes(),
                        fm,
                        at_word_start,
                        &mut out,
                    );
                    at_word_start = r.at_word_start;
                    r.consumed
                }
            }
        } else {
            0
        }
    } else {
        0
    };
    if fast_consumed == s.len() {
        // Whole input was ASCII — fast path produced byte-identical
        // output; skip the cp-decode loop entirely.
        return out;
    }

    // Slow path: cp-by-cp from `fast_consumed`. ASCII bytes count
    // 1:1 against codepoints, so the cp index also starts at
    // `fast_consumed`.
    let cps: Vec<u32> = s.chars().map(|c| c as u32).collect();
    for (i, &cp) in cps.iter().enumerate().skip(fast_consumed) {
        let is_mark = relon_ir::combining_marks::is_combining_mark(cp);
        if is_mark {
            // Combining marks: pass through unchanged. The word-boundary
            // flag stays as-is because marks belong to their base
            // codepoint's grapheme cluster.
            if let Some(c) = char::from_u32(cp) {
                out.push(c);
            }
            continue;
        }

        if mode == CaseFoldMode::Title {
            if let Some(c) = char::from_u32(cp) {
                if c.is_whitespace() {
                    out.push(c);
                    at_word_start = true;
                    continue;
                }
            }
        }

        let effective_mode = match mode {
            CaseFoldMode::Upper => CaseFoldMode::Upper,
            CaseFoldMode::Lower => CaseFoldMode::Lower,
            CaseFoldMode::Title => {
                if at_word_start {
                    CaseFoldMode::Upper
                } else {
                    CaseFoldMode::Lower
                }
            }
        };
        at_word_start = false;

        // Final sigma context — only when lowering Σ (U+03A3).
        if effective_mode == CaseFoldMode::Lower && cp == 0x03A3 {
            let final_form = relon_ir::full_case_folding::is_final_sigma_context(&cps, i);
            let mapped = if final_form { 0x03C2 } else { 0x03C3 };
            if let Some(c) = char::from_u32(mapped) {
                out.push(c);
            }
            continue;
        }

        // Turkish locale overrides take precedence over default tables.
        if locale_turkish {
            let entry = match effective_mode {
                CaseFoldMode::Upper => relon_ir::full_case_folding::turkish_upper_entry(cp),
                CaseFoldMode::Lower => relon_ir::full_case_folding::turkish_lower_entry(cp),
                CaseFoldMode::Title => unreachable!("normalised above"),
            };
            if let Some((len, slots)) = entry {
                for &m in &slots[..len as usize] {
                    if let Some(c) = char::from_u32(m) {
                        out.push(c);
                    }
                }
                continue;
            }
        }

        // FULL multi-codepoint mappings (e.g. ß -> SS, ﬁ -> FI).
        let full_entry = match effective_mode {
            CaseFoldMode::Upper => relon_ir::full_case_folding::full_upper_entry(cp),
            CaseFoldMode::Lower => relon_ir::full_case_folding::full_lower_entry(cp),
            CaseFoldMode::Title => unreachable!("normalised above"),
        };
        if let Some((len, slots)) = full_entry {
            for &m in &slots[..len as usize] {
                if let Some(c) = char::from_u32(m) {
                    out.push(c);
                }
            }
            continue;
        }

        // Fall back to Rust's char API for the simple 1:1 cases.
        if let Some(c) = char::from_u32(cp) {
            match effective_mode {
                CaseFoldMode::Upper => {
                    for u in c.to_uppercase() {
                        out.push(u);
                    }
                }
                CaseFoldMode::Lower => {
                    for u in c.to_lowercase() {
                        out.push(u);
                    }
                }
                CaseFoldMode::Title => unreachable!("normalised above"),
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaseFoldMode {
    Upper,
    Lower,
    Title,
}

/// `#161` write-to-buffer fast path for `to_upper` / `to_lower` /
/// `title` on short ASCII inputs.
///
/// When the payload is ≤ [`relon_eval_api::SMOL_STR_INLINE_CAP`] bytes
/// **and** entirely ASCII, output length equals input length and every
/// byte is a single-byte UTF-8 codeunit. We can therefore write the
/// folded bytes directly into the `SmolStr` inline slot via
/// [`SmolStr::try_build_inline`], skipping the
/// `String::with_capacity` allocation + `Arc<str>` wrap that the
/// historical `fold_string(...).into()` path paid even on inline-sized
/// outputs.
///
/// Returns `None` and falls through to the general
/// [`fold_string`] path for any of:
///
///   * Payload longer than the inline cap (heap-side anyway).
///   * Non-ASCII payload — multi-codepoint mappings (`ß` -> `SS`,
///     `ﬁ` -> `FI`, sigma-final, combining marks) can make output
///     length differ from input length, so the byte-equal precondition
///     no longer holds.
///   * Turkish locale — the `i` / `I` overrides emit 2-byte UTF-8
///     output from 1-byte ASCII input, breaking the byte-equal
///     contract.
#[inline]
fn fold_string_to_smol_ascii_fast(s: &str, mode: CaseFoldMode) -> Option<SmolStr> {
    use relon_eval_api::SMOL_STR_INLINE_CAP;
    let bytes = s.as_bytes();
    if bytes.len() > SMOL_STR_INLINE_CAP {
        return None;
    }
    if !s.is_ascii() {
        return None;
    }
    let ir_mode = match mode {
        CaseFoldMode::Upper => relon_ir::ascii_fold_simd::AsciiFoldMode::Upper,
        CaseFoldMode::Lower => relon_ir::ascii_fold_simd::AsciiFoldMode::Lower,
        CaseFoldMode::Title => relon_ir::ascii_fold_simd::AsciiFoldMode::Title,
    };
    // Inline buffer write: the slice handed to the writer is exactly
    // `bytes.len()` bytes long, and the body below emits exactly that
    // many bytes (output length == input length for ASCII upper /
    // lower / title). The mask + xor body is the same one
    // [`relon_ir::ascii_fold_simd::fold_ascii_prefix_upper_lower`]
    // implements; we inline it here to write directly into the inline
    // slot — going through the IR helper would force a scratch
    // `Vec<u8>` allocation, defeating the alloc-skip the inline path
    // exists for.
    SmolStr::try_build_inline(bytes.len(), |out| match ir_mode {
        relon_ir::ascii_fold_simd::AsciiFoldMode::Upper => {
            // upper(b) = (b in 'a'..='z') ? b ^ 0x20 : b
            for (i, &b) in bytes.iter().enumerate() {
                let in_range = b.wrapping_sub(b'a') < 26;
                out[i] = b ^ if in_range { 0x20 } else { 0x00 };
            }
        }
        relon_ir::ascii_fold_simd::AsciiFoldMode::Lower => {
            // lower(b) = (b in 'A'..='Z') ? b ^ 0x20 : b
            for (i, &b) in bytes.iter().enumerate() {
                let in_range = b.wrapping_sub(b'A') < 26;
                out[i] = b ^ if in_range { 0x20 } else { 0x00 };
            }
        }
        relon_ir::ascii_fold_simd::AsciiFoldMode::Title => {
            // title walks the prefix tracking word-boundary state:
            // ASCII whitespace resets `at_word_start = true`, the first
            // non-whitespace codepoint after that uppers, every later
            // codepoint in the word lowers.
            let mut at_word_start = true;
            for (i, &b) in bytes.iter().enumerate() {
                if b.is_ascii_whitespace() {
                    out[i] = b;
                    at_word_start = true;
                    continue;
                }
                out[i] = if at_word_start {
                    let in_range = b.wrapping_sub(b'a') < 26;
                    b ^ if in_range { 0x20 } else { 0x00 }
                } else {
                    let in_range = b.wrapping_sub(b'A') < 26;
                    b ^ if in_range { 0x20 } else { 0x00 }
                };
                at_word_start = false;
            }
        }
    })
}

/// `#161` write-to-buffer entry the `StringUpper` / `StringLower` /
/// `StringTitle` callers reach. The ASCII-fast inline path skips the
/// `String::with_capacity` + `Arc::from(String)` round-trip; the
/// fallback re-uses [`fold_string`] for the full Unicode pipeline.
#[inline]
fn fold_string_to_smol(s: &str, mode: CaseFoldMode, locale_turkish: bool) -> SmolStr {
    if !locale_turkish {
        if let Some(smol) = fold_string_to_smol_ascii_fast(s, mode) {
            return smol;
        }
    }
    SmolStr::from(fold_string(s, mode, locale_turkish))
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
        let s = expect_string(&args[0], range)?;
        Ok(Value::String(fold_string_to_smol(
            s,
            CaseFoldMode::Upper,
            false,
        )))
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
        let s = expect_string(&args[0], range)?;
        Ok(Value::String(fold_string_to_smol(
            s,
            CaseFoldMode::Lower,
            false,
        )))
    }
}

/// v3++ b-6: locale-aware case folding. Surface names
/// `upper_locale` / `lower_locale` / `title_locale`. The locale string
/// is parsed via [`relon_ir::full_case_folding::is_turkish_locale`] —
/// only `tr` / `az` (with optional `-XX` / `_XX` region) flips into
/// the Turkish override branch; every other locale falls back to the
/// default UAX #21 behaviour.
struct StringUpperLocale;
impl RelonFunction for StringUpperLocale {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        let s = expect_string(&args[0], range)?;
        let locale = expect_string(&args[1], range)?;
        let tr = relon_ir::full_case_folding::is_turkish_locale(locale);
        Ok(Value::String(fold_string_to_smol(s, CaseFoldMode::Upper, tr)))
    }
}

struct StringLowerLocale;
impl RelonFunction for StringLowerLocale {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        let s = expect_string(&args[0], range)?;
        let locale = expect_string(&args[1], range)?;
        let tr = relon_ir::full_case_folding::is_turkish_locale(locale);
        Ok(Value::String(fold_string_to_smol(s, CaseFoldMode::Lower, tr)))
    }
}

struct StringTitleLocale;
impl RelonFunction for StringTitleLocale {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        let s = expect_string(&args[0], range)?;
        let locale = expect_string(&args[1], range)?;
        let tr = relon_ir::full_case_folding::is_turkish_locale(locale);
        Ok(Value::String(fold_string_to_smol(s, CaseFoldMode::Title, tr)))
    }
}

/// v3++ b-4: word-boundary aware case fold mirroring the wasm-AOT
/// `title` body in `crates/relon-ir/src/stdlib.rs`.
///
/// Algorithm:
///   * Walk the input codepoint-by-codepoint.
///   * Whitespace (`char::is_whitespace`) passes through unchanged and
///     resets the `at_word_start` flag.
///   * Unicode combining marks
///     (`crates/relon-ir/src/combining_marks.rs::is_combining_mark`)
///     pass through unchanged and do **not** flip `at_word_start` — a
///     mark belongs to its base codepoint's cluster.
///   * Every other codepoint is upper-cased when `at_word_start` is
///     set, otherwise lower-cased. The flag clears after the first
///     such codepoint of each word.
///
/// Stays in lock-step with the wasm-AOT body's behaviour so backend
/// tests can compare results bit-for-bit.
struct StringTitle;
impl RelonFunction for StringTitle {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let s = expect_string(&args[0], range)?;
        Ok(Value::String(fold_string_to_smol(
            s,
            CaseFoldMode::Title,
            false,
        )))
    }
}

/// v3++ b-5: Unicode normalization (UAX #15) — Canonical Composition.
///
/// Calls into [`relon_ir::normalization::to_nfc`], which shares its
/// data tables (`NFD_INDEX` / `NFD_POOL` / `CCC_TABLE` /
/// `COMPOSITION_PAIRS`) with the wasm-AOT backend body in
/// `crates/relon-ir/src/stdlib.rs`. Both executors therefore see the
/// same byte-for-byte UCD 14.0.0 data, ensuring no silent drift.
///
/// Hangul syllables are decomposed and composed algorithmically per
/// UAX #15 section 16; the rest of the input runs through the shared
/// `decompose -> reorder -> compose` pipeline.
struct StringNfc;
impl RelonFunction for StringNfc {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let s = expect_string(&args[0], range)?;
        Ok(Value::String(relon_ir::normalization::to_nfc(s).into()))
    }
}

/// v3++ b-5: Unicode normalization — Canonical Decomposition.
/// Mirrors the wasm-AOT body; same shared data tables.
struct StringNfd;
impl RelonFunction for StringNfd {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let s = expect_string(&args[0], range)?;
        Ok(Value::String(relon_ir::normalization::to_nfd(s).into()))
    }
}

/// v3++ b-5: Unicode normalization — Compatibility Composition.
/// Mirrors the wasm-AOT body; same shared data tables.
struct StringNfkc;
impl RelonFunction for StringNfkc {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let s = expect_string(&args[0], range)?;
        Ok(Value::String(relon_ir::normalization::to_nfkc(s).into()))
    }
}

/// v3++ b-5: Unicode normalization — Compatibility Decomposition.
/// Mirrors the wasm-AOT body; same shared data tables.
struct StringNfkd;
impl RelonFunction for StringNfkd {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let s = expect_string(&args[0], range)?;
        Ok(Value::String(relon_ir::normalization::to_nfkd(s).into()))
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

/// 2026-05-21: Tier-2 LuaJIT-pattern-subset glob matcher
/// (`glob_match(s, pattern) -> Bool`).
///
/// Delegates to the shared algorithm in [`relon_ir::glob::glob_match`]
/// so the tree-walker and the cranelift host-helper backend stay
/// byte-for-byte in lock-step. The matcher itself is anchored, case-
/// sensitive, char-by-char Unicode, and runs in linear time over
/// `|s| * |p|` with no exponential-backtracking surface — see the
/// module doc-comment in `relon-ir/src/glob.rs` for the supported
/// syntax and malformed-pattern handling.
struct StringGlobMatch;
impl RelonFunction for StringGlobMatch {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        let s = expect_string(&args[0], range)?;
        let pattern = expect_string(&args[1], range)?;
        Ok(Value::Bool(relon_ir::glob::glob_match(s, pattern)))
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
        Ok(Value::list(
            keys.into_iter().map(|k| Value::String(k.into())).collect(),
        ))
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
                    .map(|idx| Value::String(chars[idx].to_string().into()))
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
                        Value::list(vec![Value::String(key.as_str().into()), v])
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
    map.insert("_kind".to_string(), Value::String(kind.into()));
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

// ---- v6-δ M1 R4: stdlib free-fn surface for the three-way corpus ----

/// `s.is_empty()` / `xs.is_empty()` — polymorphic over String / List /
/// Dict; matches the wasm-AOT backend's `is_empty` stdlib slot. Mirrors
/// the cranelift backend's `IrType::String, "is_empty"` lowering.
struct IsEmpty;
impl RelonFunction for IsEmpty {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        match &args[0] {
            Value::String(s) => Ok(Value::Bool(s.is_empty())),
            Value::List(l) => Ok(Value::Bool(l.is_empty())),
            Value::Dict(d) => Ok(Value::Bool(d.map.is_empty())),
            other => Err(RuntimeError::TypeMismatch {
                expected: "String/List/Dict".to_string(),
                found: other.type_name().to_string(),
                range,
            }),
        }
    }
}

/// `s.concat(t)` — string concatenation. Tree-walker's existing
/// `_string_replace` etc. handle one-shot transforms; we model `concat`
/// as a 2-arg String op so the corpus's `"foo".concat("bar")` reaches
/// AllAgree without going through a `+` overload (the source-level
/// `+` operator is integer addition only).
struct StringConcat;
impl RelonFunction for StringConcat {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        let lhs = expect_string(&args[0], range)?;
        let rhs = expect_string(&args[1], range)?;
        // `#161` write-to-buffer: route through `SmolStr::concat` so
        // short-string outputs (≤ 22 bytes total) land inline without
        // the `String::with_capacity` + `Arc::from(String)` round-trip
        // the old `out.into()` path always paid.
        Ok(Value::String(SmolStr::concat(lhs, rhs)))
    }
}

/// `s.substring(start, length)` — start-byte-index + byte-length.
/// Matches the wasm-AOT / cranelift backend's `substring` body which
/// takes `(s, start, len)`. Indices are clamped against `[0, s.len()]`
/// so off-by-one corpus inputs don't panic; the bounds-check trap the
/// cranelift body raises is intentionally relaxed here because the
/// tree-walker is the fallback path and never needs to mirror the
/// trap (callers see `TraceJitNotApplicable` / etc.).
struct StringSubstring;
impl RelonFunction for StringSubstring {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 3, range)?;
        let s = expect_string(&args[0], range)?;
        let start = expect_int(&args[1], range)?;
        let length = expect_int(&args[2], range)?;
        let s_len = s.len() as i64;
        let start = start.clamp(0, s_len) as usize;
        let length = length.max(0) as usize;
        let end = (start + length).min(s.len());
        if end <= start {
            return Ok(Value::String(SmolStr::new_empty()));
        }
        // Walk to the nearest char boundary to keep utf-8 well-formed
        // on inputs the corpus may feed in (the wasm-AOT body indexes
        // strictly by byte so this is a deliberately conservative
        // bridge).
        let real_start = s
            .char_indices()
            .find(|(i, _)| *i >= start)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        let real_end = s
            .char_indices()
            .find(|(i, _)| *i >= end)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        Ok(Value::String(s[real_start..real_end].into()))
    }
}

/// `s.starts_with(prefix)` — Boolean prefix check. Mirrors the
/// wasm-AOT backend's `starts_with` body.
struct StringStartsWith;
impl RelonFunction for StringStartsWith {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        let s = expect_string(&args[0], range)?;
        let prefix = expect_string(&args[1], range)?;
        Ok(Value::Bool(s.starts_with(prefix)))
    }
}

/// `xs.sum()` over a `List<Int>`. Float lists return `Float`; mixed
/// lists fall through to a TypeMismatch.
struct ListSum;
impl RelonFunction for ListSum {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let list = expect_list(&args[0], range)?;
        // Two-pass classification: if every element is Int we return
        // Int; if every element is Float we return Float; otherwise
        // TypeMismatch (the cranelift backend's typed `list_int_sum`
        // / `list_float_sum` slots refuse a mixed list at lowering
        // time, so the surfaces match by design).
        let mut all_int = true;
        let mut all_float = true;
        for v in list {
            match v {
                Value::Int(_) => all_float = false,
                Value::Float(_) => all_int = false,
                _ => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "List<Int> or List<Float>".to_string(),
                        found: v.type_name().to_string(),
                        range,
                    })
                }
            }
        }
        if all_int {
            let mut acc: i64 = 0;
            for v in list {
                if let Value::Int(x) = v {
                    acc = acc.wrapping_add(*x);
                }
            }
            Ok(Value::Int(acc))
        } else if all_float {
            let mut acc: f64 = 0.0;
            for v in list {
                if let Value::Float(x) = v {
                    acc += x.into_inner();
                }
            }
            Ok(Value::Float(acc.into()))
        } else {
            // Empty list: sum is 0 (Int).
            Ok(Value::Int(0))
        }
    }
}

/// `xs.max()` over a `List<Int>` / `List<Float>`. Returns the largest
/// element (signed for Int). Empty list surfaces a typed
/// `TypeMismatch` carrying "non-empty list"; the cranelift backend's
/// behaviour on empty lists is undefined-by-design so picking a typed
/// error here is the more honest surface.
struct ListMax;
impl RelonFunction for ListMax {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let list = expect_list(&args[0], range)?;
        if list.is_empty() {
            return Err(RuntimeError::TypeMismatch {
                expected: "non-empty list".to_string(),
                found: "empty list".to_string(),
                range,
            });
        }
        let first = &list[0];
        match first {
            Value::Int(seed) => {
                let mut acc = *seed;
                for v in &list[1..] {
                    let x = expect_int(v, range)?;
                    if x > acc {
                        acc = x;
                    }
                }
                Ok(Value::Int(acc))
            }
            Value::Float(seed) => {
                let mut acc = seed.into_inner();
                for v in &list[1..] {
                    match v {
                        Value::Float(x) => {
                            let xv = x.into_inner();
                            if xv > acc {
                                acc = xv;
                            }
                        }
                        other => {
                            return Err(RuntimeError::TypeMismatch {
                                expected: "List<Float>".to_string(),
                                found: other.type_name().to_string(),
                                range,
                            })
                        }
                    }
                }
                Ok(Value::Float(acc.into()))
            }
            other => Err(RuntimeError::TypeMismatch {
                expected: "List<Int> or List<Float>".to_string(),
                found: other.type_name().to_string(),
                range,
            }),
        }
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

#[cfg(test)]
mod full_case_folding_tests {
    //! v3++ b-6 tree-walk smoke tests for UAX #21 full case folding.
    //!
    //! These cover the three behaviours wired into the host
    //! `fold_string` helper: unconditional multi-codepoint mappings
    //! (`ß` -> `SS`, ligatures, `İ` -> `i\u{0307}`), Greek final-sigma
    //! context (`Σ` -> `ς` vs `σ`), and Turkish / Azerbaijani locale
    //! overrides (`I` <-> `ı` / `İ` <-> `i`).
    //!
    //! The wasm-AOT backend currently provides locale dispatch only;
    //! multi-cp and final-sigma are deferred there. The same UAX #21
    //! reference data backs both executors so they will converge once
    //! the wasm body gains multi-cp output emission.

    use super::{fold_string, CaseFoldMode};

    fn upper(s: &str) -> String {
        fold_string(s, CaseFoldMode::Upper, false)
    }
    fn lower(s: &str) -> String {
        fold_string(s, CaseFoldMode::Lower, false)
    }
    fn title(s: &str) -> String {
        fold_string(s, CaseFoldMode::Title, false)
    }
    fn upper_tr(s: &str) -> String {
        fold_string(s, CaseFoldMode::Upper, true)
    }
    fn lower_tr(s: &str) -> String {
        fold_string(s, CaseFoldMode::Lower, true)
    }
    fn title_tr(s: &str) -> String {
        fold_string(s, CaseFoldMode::Title, true)
    }

    // ----- Unconditional multi-codepoint mappings -----

    #[test]
    fn sharp_s_uppercases_to_ss() {
        assert_eq!(upper("stra\u{00DF}e"), "STRASSE");
    }

    #[test]
    fn fi_ligature_uppercases_to_fi() {
        assert_eq!(upper("\u{FB01}ne"), "FINE");
    }

    #[test]
    fn fl_ligature_uppercases_to_fl() {
        assert_eq!(upper("\u{FB02}ow"), "FLOW");
    }

    // ----- Greek final-sigma context -----

    #[test]
    fn final_sigma_at_word_end_uses_curly_form() {
        // ΟΔΥΣΣΕΥΣ — last Σ is final, middle Σ are not.
        // Greek `Υ` (U+03A5) lowercases to `υ` (U+03C5).
        // The middle "ΣΣ" pair: first sigma is not final (followed by
        // cased letters), second is also not final.
        assert_eq!(
            lower("\u{039F}\u{0394}\u{03A5}\u{03A3}\u{03A3}\u{0395}\u{03A5}\u{03A3}"),
            "\u{03BF}\u{03B4}\u{03C5}\u{03C3}\u{03C3}\u{03B5}\u{03C5}\u{03C2}"
        );
    }

    #[test]
    fn non_final_sigma_followed_by_cased_letter() {
        // "ΣΑ" — Σ at index 0 is followed by Α (cased), so it must
        // lower to σ (non-final).
        assert_eq!(lower("\u{03A3}\u{0391}"), "\u{03C3}\u{03B1}");
    }

    #[test]
    fn isolated_sigma_lowercases_to_curly_when_no_preceding_cased() {
        // UAX #21: Final_Sigma requires a preceding cased letter. A
        // standalone Σ has no preceding cased context, so it falls
        // through to σ (non-final). This matches ICU.
        assert_eq!(lower("\u{03A3}"), "\u{03C3}");
    }

    #[test]
    fn final_sigma_with_intervening_case_ignorable() {
        // "OΣ'" — apostrophe is case-ignorable, so Σ at index 1 sees
        // no following cased codepoint and uses the final form.
        assert_eq!(lower("O\u{03A3}'"), "o\u{03C2}'");
    }

    // ----- Default Σ / İ behaviour without locale -----

    #[test]
    fn upper_istanbul_keeps_capital_dotted_i() {
        // Default upper-case of "İstanbul" preserves U+0130 and
        // uppercases the rest.
        assert_eq!(upper("\u{0130}stanbul"), "\u{0130}STANBUL");
    }

    #[test]
    fn lower_capital_i_with_dot_decomposes_to_i_plus_combining_dot() {
        // The unconditional FULL_LOWER entry for U+0130 is the
        // multi-codepoint `i\u{0307}` form per SpecialCasing.txt.
        assert_eq!(lower("\u{0130}"), "i\u{0307}");
    }

    // ----- Turkish / Azerbaijani locale overrides -----

    #[test]
    fn upper_locale_tr_lowercase_i_to_dotted_i() {
        assert_eq!(upper_tr("istanbul"), "\u{0130}STANBUL");
    }

    #[test]
    fn lower_locale_tr_capital_i_to_dotless() {
        assert_eq!(lower_tr("ISTANBUL"), "\u{0131}stanbul");
    }

    #[test]
    fn lower_locale_default_capital_i_to_lowercase_i() {
        assert_eq!(lower("I"), "i");
    }

    #[test]
    fn title_locale_tr_first_letter_dotted() {
        assert_eq!(title_tr("istanbul"), "\u{0130}stanbul");
    }

    // ----- Roundtrip / idempotence -----

    #[test]
    fn upper_idempotent_on_latin() {
        let s = "HELLO WORLD";
        assert_eq!(upper(s), s);
    }

    #[test]
    fn lower_idempotent_on_latin() {
        let s = "hello world";
        assert_eq!(lower(s), s);
    }

    #[test]
    fn title_roundtrip_two_words() {
        assert_eq!(title("hello world"), "Hello World");
    }

    // ----- Combining mark handling -----

    #[test]
    fn combining_mark_does_not_break_word_boundary() {
        // "cafe\u{0301} bar" — `e\u{0301}` is a single cluster. After
        // the space, `b` is the new word-start. Tree-walk emits
        // "Cafe\u{0301} Bar".
        assert_eq!(title("cafe\u{0301} bar"), "Cafe\u{0301} Bar");
    }

    #[test]
    fn combining_mark_after_sigma_does_not_break_final_sigma() {
        // "OΣ\u{0301}" — combining acute is case-ignorable, so Σ at
        // index 1 still qualifies as word-final.
        assert_eq!(lower("O\u{03A3}\u{0301}"), "o\u{03C2}\u{0301}");
    }
}

#[cfg(test)]
mod ascii_hint_tests {
    //! Tier 2c (#153) — caller-supplied ASCII classification hint.
    //!
    //! The fold engine must produce byte-identical output regardless of
    //! whether the caller passes `AsciiHint::Unknown` (the historical
    //! shape — fold runs its own SIMD scan) or `AsciiHint::AllAscii` /
    //! `AsciiHint::KnownNonAscii` (the producer side has already paid
    //! the classification cost, typically via the StringRef record's
    //! flag bit). These tests pin the parity guarantee so a future
    //! caller that wires the StringRef flag into the evaluator gets
    //! semantics-equivalent behaviour for free.
    use super::{fold_string_with_ascii_hint, AsciiHint, CaseFoldMode};

    fn run(s: &str, mode: CaseFoldMode) -> (String, String, String) {
        let unknown = fold_string_with_ascii_hint(s, mode, false, AsciiHint::Unknown);
        let all_ascii = if s.is_ascii() {
            fold_string_with_ascii_hint(s, mode, false, AsciiHint::AllAscii)
        } else {
            unknown.clone()
        };
        let known_non_ascii = fold_string_with_ascii_hint(s, mode, false, AsciiHint::KnownNonAscii);
        (unknown, all_ascii, known_non_ascii)
    }

    #[test]
    fn ascii_input_matches_across_hints() {
        for s in [
            "",
            "a",
            "Z",
            "Hello, World!",
            "the quick brown fox",
            "0123456789",
            "  leading  spaces",
        ] {
            for mode in [
                CaseFoldMode::Upper,
                CaseFoldMode::Lower,
                CaseFoldMode::Title,
            ] {
                let (unknown, all_ascii, known_non_ascii) = run(s, mode);
                assert_eq!(unknown, all_ascii, "s={s:?} mode={mode:?}");
                // KnownNonAscii forces the slow path; for ASCII input
                // it must still produce the same output because the
                // slow path's per-codepoint logic agrees with the
                // mask + xor on ASCII codepoints.
                assert_eq!(unknown, known_non_ascii, "s={s:?} mode={mode:?}");
            }
        }
    }

    #[test]
    fn non_ascii_input_matches_unknown_and_known_non_ascii() {
        // `AsciiHint::AllAscii` is contractually invalid for non-ASCII
        // inputs (it would skip the SIMD scan and mask-flip the high
        // bytes), so we only assert `Unknown` vs `KnownNonAscii`
        // parity here. The fold engine's slow path is the same in
        // both shapes.
        for s in [
            "caf\u{00E9}",
            "stra\u{00DF}e",
            "\u{03A3}\u{0391}",
            "Welt: ich bin\u{00E9}",
        ] {
            for mode in [
                CaseFoldMode::Upper,
                CaseFoldMode::Lower,
                CaseFoldMode::Title,
            ] {
                let unknown = fold_string_with_ascii_hint(s, mode, false, AsciiHint::Unknown);
                let known_non_ascii =
                    fold_string_with_ascii_hint(s, mode, false, AsciiHint::KnownNonAscii);
                assert_eq!(unknown, known_non_ascii, "s={s:?} mode={mode:?}");
            }
        }
    }
}
