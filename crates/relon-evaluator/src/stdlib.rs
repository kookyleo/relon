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

    // Built-in WASI-backed capability primitives (P-clock / P-random).
    // These are the tree-walk gold-standard oracle for the compiled
    // backends: `clock()` reads the wall clock (ns since the Unix
    // epoch), `random()` reads 8 OS-entropy bytes. The ambient reads
    // live in the delimited gated region below. Registered with a
    // non-empty `NativeFnGate` so the evaluator enforces the capability
    // gate (`reads_clock` / `uses_rng`) via `check_native_fn_capability`
    // — an ungranted call raises `RuntimeError::CapabilityDenied`,
    // matching the compiled backends' `Op::CheckCap` trap.
    use relon_eval_api::context::NativeFnGate;
    let mut clock_gate = NativeFnGate::default();
    clock_gate.reads_clock = true;
    let mut rng_gate = NativeFnGate::default();
    rng_gate.uses_rng = true;
    ctx.register_fn("clock", clock_gate, Arc::new(Clock));
    ctx.register_fn("random", rng_gate, Arc::new(RandomFn));

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

    // Stdlib JSON Schema parity wave (2026-05-23). Format / regex /
    // numeric / list / json predicates that JSON Schema authors reach
    // for. All registered as free fns; the method-dispatch alias loop
    // below auto-binds them onto the receiver schemas (String / Int /
    // Float / List) so `s.is_email()` works alongside `is_email(s)`.
    let string_matches: Arc<dyn RelonFunction> = Arc::new(StringMatches);
    let string_starts_with: Arc<dyn RelonFunction> = Arc::new(StringStartsWith);
    let string_ends_with: Arc<dyn RelonFunction> = Arc::new(StringEndsWith);
    let is_email: Arc<dyn RelonFunction> = Arc::new(IsEmail);
    let is_uri: Arc<dyn RelonFunction> = Arc::new(IsUri);
    let is_uuid: Arc<dyn RelonFunction> = Arc::new(IsUuid);
    let is_iso_date: Arc<dyn RelonFunction> = Arc::new(IsIsoDate);
    let is_ipv4: Arc<dyn RelonFunction> = Arc::new(IsIpv4);
    let is_ipv6: Arc<dyn RelonFunction> = Arc::new(IsIpv6);
    let multiple_of: Arc<dyn RelonFunction> = Arc::new(MultipleOf);
    let to_json: Arc<dyn RelonFunction> = Arc::new(ToJson);
    let from_json: Arc<dyn RelonFunction> = Arc::new(FromJson);
    ctx.register_pure_fn("matches", Arc::clone(&string_matches));
    ctx.register_pure_fn("starts_with", Arc::clone(&string_starts_with));
    ctx.register_pure_fn("ends_with", Arc::clone(&string_ends_with));
    ctx.register_pure_fn("is_email", Arc::clone(&is_email));
    ctx.register_pure_fn("is_uri", Arc::clone(&is_uri));
    ctx.register_pure_fn("is_uuid", Arc::clone(&is_uuid));
    ctx.register_pure_fn("is_iso_date", Arc::clone(&is_iso_date));
    ctx.register_pure_fn("is_ipv4", Arc::clone(&is_ipv4));
    ctx.register_pure_fn("is_ipv6", Arc::clone(&is_ipv6));
    ctx.register_pure_fn("multiple_of", Arc::clone(&multiple_of));
    ctx.register_pure_fn("to_json", Arc::clone(&to_json));
    ctx.register_pure_fn("from_json", Arc::clone(&from_json));

    // Stdlib JSON Schema parity wave — batch 2: numeric helpers.
    let in_range: Arc<dyn RelonFunction> = Arc::new(InRange);
    let math_round: Arc<dyn RelonFunction> = Arc::new(MathRound);
    let math_floor: Arc<dyn RelonFunction> = Arc::new(MathFloor);
    let math_ceil: Arc<dyn RelonFunction> = Arc::new(MathCeil);
    let math_sqrt: Arc<dyn RelonFunction> = Arc::new(MathSqrt);
    let math_pow: Arc<dyn RelonFunction> = Arc::new(MathPow);
    ctx.register_pure_fn("in_range", Arc::clone(&in_range));
    ctx.register_pure_fn("round", Arc::clone(&math_round));
    ctx.register_pure_fn("floor", Arc::clone(&math_floor));
    ctx.register_pure_fn("ceil", Arc::clone(&math_ceil));
    ctx.register_pure_fn("sqrt", Arc::clone(&math_sqrt));
    ctx.register_pure_fn("pow", Arc::clone(&math_pow));

    // Stdlib JSON Schema parity wave — batch 3: list helpers.
    let list_unique: Arc<dyn RelonFunction> = Arc::new(ListUnique);
    let list_count: Arc<dyn RelonFunction> = Arc::new(ListCount);
    let list_every: Arc<dyn RelonFunction> = Arc::new(ListEvery);
    let list_some: Arc<dyn RelonFunction> = Arc::new(ListSome);
    ctx.register_pure_fn("unique", Arc::clone(&list_unique));
    ctx.register_pure_fn("count", Arc::clone(&list_count));
    ctx.register_pure_fn("every", Arc::clone(&list_every));
    ctx.register_pure_fn("some", Arc::clone(&list_some));
    // Method-form aliases so `xs.every(p)` / `xs.some(p)` work.
    ctx.register_pure_method("List", "every", list_every);
    ctx.register_pure_method("List", "some", list_some);
    ctx.register_pure_method("List", "unique", list_unique);

    // Stdlib JSON Schema parity wave — batch 5: string trim + dict
    // helpers + date parser.
    let string_trim: Arc<dyn RelonFunction> = Arc::new(StringTrim);
    let string_trim_start: Arc<dyn RelonFunction> = Arc::new(StringTrimStart);
    let string_trim_end: Arc<dyn RelonFunction> = Arc::new(StringTrimEnd);
    let dict_select_keys: Arc<dyn RelonFunction> = Arc::new(DictSelectKeys);
    let dict_omit_keys: Arc<dyn RelonFunction> = Arc::new(DictOmitKeys);
    let size_in_range: Arc<dyn RelonFunction> = Arc::new(SizeInRange);
    let parse_iso_date: Arc<dyn RelonFunction> = Arc::new(ParseIsoDate);
    ctx.register_pure_fn("trim", Arc::clone(&string_trim));
    ctx.register_pure_fn("trim_start", Arc::clone(&string_trim_start));
    ctx.register_pure_fn("trim_end", Arc::clone(&string_trim_end));
    ctx.register_pure_fn("select_keys", Arc::clone(&dict_select_keys));
    ctx.register_pure_fn("omit_keys", Arc::clone(&dict_omit_keys));
    ctx.register_pure_fn("size_in_range", Arc::clone(&size_in_range));
    ctx.register_pure_fn("parse_iso_date", Arc::clone(&parse_iso_date));
    // Method-form aliases for string trims + dict helpers.
    ctx.register_pure_method("String", "trim", string_trim);
    ctx.register_pure_method("String", "trim_start", string_trim_start);
    ctx.register_pure_method("String", "trim_end", string_trim_end);
    ctx.register_pure_method("Dict", "select_keys", dict_select_keys);
    ctx.register_pure_method("Dict", "omit_keys", dict_omit_keys);

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
    ctx.register_pure_method(crate::iter_protocol::BRAND, "next", Arc::new(IterNext));
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

// GATED-CAPABILITY-PRIMITIVES-BEGIN
//
// The two impls below are the ONLY ambient-API users in this file, and
// they are exactly what the `purity_guard` test endorses: gated host
// fns registered via `register_fn(name, gate, fn)` with a non-empty
// `NativeFnGate` (`reads_clock` / `uses_rng`). The purity scan excludes
// this delimited region — the ban targets *ungated* ambient APIs, not
// these capability-gated built-in primitives.

/// Built-in `clock()` primitive. Returns the wall-clock reading as an
/// `Int` count of nanoseconds since the Unix epoch — the same physical
/// clock the compiled backends read (native `SystemTime`, wasm WASI
/// `clock_time_get(CLOCK_REALTIME)`), so all three executors are
/// value-comparable within a time window. The capability gate
/// (`reads_clock`) is enforced by the evaluator before dispatch.
struct Clock;
impl RelonFunction for Clock {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args.into_positional(), 0, range)?;
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        Ok(Value::Int(ns))
    }
}

/// Built-in `random()` primitive. Returns 8 fresh random bytes packed
/// into an `Int` (little-endian), backed by OS entropy via
/// `/dev/urandom` — the same source shape the compiled backends use
/// (native `/dev/urandom`, wasm WASI `random_get`). The capability gate
/// (`uses_rng`) is enforced by the evaluator before dispatch.
struct RandomFn;
impl RelonFunction for RandomFn {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        use std::io::Read;
        expect_arg_count(&args.into_positional(), 0, range)?;
        let mut buf = [0u8; 8];
        let bits =
            match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
                Ok(()) => i64::from_le_bytes(buf),
                Err(_) => 0,
            };
        Ok(Value::Int(bits))
    }
}
// GATED-CAPABILITY-PRIMITIVES-END

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
///
/// Test-only convenience wrapper around
/// [`fold_string_with_ascii_hint`]; production code reaches in via
/// [`fold_string_to_smol_with_hint`] so the caller's pre-classified
/// ASCII fact propagates through the inline write-to-buffer fast path.
#[cfg(test)]
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
enum AsciiHint {
    /// Caller has not classified the input. The fold engine runs the
    /// SIMD scan + fast path as before.
    ///
    /// Live tree-walk callers (`String{Upper,Lower,Title}{,Locale}`)
    /// always derive a concrete `AllAscii` / `KnownNonAscii` hint from
    /// the input `SmolStr`, so this variant is only constructed by the
    /// test suite + the `#[cfg(test)]` `fold_string` wrapper. The
    /// fold engine still pattern-matches against it because future
    /// bytecode / trace-JIT callers may want the legacy SIMD-scan
    /// shape when no upstream classification exists.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "constructed only by the #[cfg(test)] `fold_string` wrapper + ascii_hint parity tests; surface call sites all classify via `AsciiHint::from_smol`"
        )
    )]
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

impl AsciiHint {
    /// Build a hint from a [`SmolStr`] caller. Inline payloads pay one
    /// vectorisable 22-byte scan; heap payloads delegate to
    /// `str::is_ascii`. See the type-level note on
    /// [`relon_eval_api::SmolStr::is_ascii`] for the cost breakdown.
    #[inline]
    fn from_smol(s: &SmolStr) -> Self {
        if s.is_ascii() {
            AsciiHint::AllAscii
        } else {
            AsciiHint::KnownNonAscii
        }
    }
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
/// `ascii_hint` lets the caller surface a pre-classified ASCII fact
/// (typically from [`relon_eval_api::SmolStr::is_ascii`]). When the
/// caller has already paid the scan we skip the redundant
/// `s.is_ascii()` re-check; on `AsciiHint::KnownNonAscii` we bail out
/// without touching the bytes at all. `AsciiHint::Unknown` keeps the
/// legacy shape (one `str::is_ascii` scan over the inline-cap
/// payload).
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
fn fold_string_to_smol_ascii_fast(
    s: &str,
    mode: CaseFoldMode,
    ascii_hint: AsciiHint,
) -> Option<SmolStr> {
    use relon_eval_api::SMOL_STR_INLINE_CAP;
    let bytes = s.as_bytes();
    if bytes.len() > SMOL_STR_INLINE_CAP {
        return None;
    }
    match ascii_hint {
        AsciiHint::AllAscii => {
            // Caller has proven the payload is pure ASCII — skip the
            // re-scan. Every byte is `< 0x80` so output length equals
            // input length and the mask + xor body below is safe.
        }
        AsciiHint::KnownNonAscii => {
            // Caller has proven the payload contains a byte `>= 0x80`.
            // The inline ASCII fast path's byte-equal precondition no
            // longer holds — bail straight to the general path.
            return None;
        }
        AsciiHint::Unknown => {
            // Legacy shape: scan ourselves. Cheap (≤ 22 bytes) but
            // wasted when the caller already paid via `SmolStr::is_ascii`.
            if !s.is_ascii() {
                return None;
            }
        }
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
/// `StringTitle` callers reach, plus the `#163` Tier 2c follow-up that
/// threads a pre-classified [`AsciiHint`] through to the fold engine.
///
/// The ASCII-fast inline path skips the `String::with_capacity` +
/// `Arc::from(String)` round-trip; the fallback re-uses
/// [`fold_string_with_ascii_hint`] for the full Unicode pipeline.
///
/// `ascii_hint` is typically derived from the input `SmolStr` via
/// [`AsciiHint::from_smol`] at the surface call site; future bytecode
/// / trace-JIT callers can read the same fact from
/// [`relon_trace_abi::STRING_RECORD_ASCII_FLAG_BIT`] without any
/// additional scan. Passing [`AsciiHint::Unknown`] preserves the
/// pre-#163 behaviour where the fold engine + the inline ASCII fast
/// path each run their own SIMD scan.
#[inline]
fn fold_string_to_smol_with_hint(
    s: &str,
    mode: CaseFoldMode,
    locale_turkish: bool,
    ascii_hint: AsciiHint,
) -> SmolStr {
    if !locale_turkish {
        if let Some(smol) = fold_string_to_smol_ascii_fast(s, mode, ascii_hint) {
            return smol;
        }
    }
    SmolStr::from(fold_string_with_ascii_hint(
        s,
        mode,
        locale_turkish,
        ascii_hint,
    ))
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
        // Reach the SmolStr container so the case-fold engine sees the
        // pre-classified ASCII fact via `AsciiHint::from_smol` —
        // bypasses the per-call SIMD scan inside
        // `fold_string_with_ascii_hint` for both ASCII and non-ASCII
        // payloads (see `preclassified_*` rows in
        // `crates/relon-bench/benches/ascii_case_fold.rs`).
        let smol = expect_smol_string(&args[0], range)?;
        let hint = AsciiHint::from_smol(smol);
        Ok(Value::String(fold_string_to_smol_with_hint(
            smol.as_str(),
            CaseFoldMode::Upper,
            false,
            hint,
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
        let smol = expect_smol_string(&args[0], range)?;
        let hint = AsciiHint::from_smol(smol);
        Ok(Value::String(fold_string_to_smol_with_hint(
            smol.as_str(),
            CaseFoldMode::Lower,
            false,
            hint,
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
        let smol = expect_smol_string(&args[0], range)?;
        let locale = expect_string(&args[1], range)?;
        let tr = relon_ir::full_case_folding::is_turkish_locale(locale);
        let hint = AsciiHint::from_smol(smol);
        Ok(Value::String(fold_string_to_smol_with_hint(
            smol.as_str(),
            CaseFoldMode::Upper,
            tr,
            hint,
        )))
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
        let smol = expect_smol_string(&args[0], range)?;
        let locale = expect_string(&args[1], range)?;
        let tr = relon_ir::full_case_folding::is_turkish_locale(locale);
        let hint = AsciiHint::from_smol(smol);
        Ok(Value::String(fold_string_to_smol_with_hint(
            smol.as_str(),
            CaseFoldMode::Lower,
            tr,
            hint,
        )))
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
        let smol = expect_smol_string(&args[0], range)?;
        let locale = expect_string(&args[1], range)?;
        let tr = relon_ir::full_case_folding::is_turkish_locale(locale);
        let hint = AsciiHint::from_smol(smol);
        Ok(Value::String(fold_string_to_smol_with_hint(
            smol.as_str(),
            CaseFoldMode::Title,
            tr,
            hint,
        )))
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
        let smol = expect_smol_string(&args[0], range)?;
        let hint = AsciiHint::from_smol(smol);
        Ok(Value::String(fold_string_to_smol_with_hint(
            smol.as_str(),
            CaseFoldMode::Title,
            false,
            hint,
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
        // BTreeMap, so the per-entry cost is real. (Iteration is
        // already key-sorted.)
        if !map.is_empty() {
            caps.tick(map.len() as u64, range)?;
        }
        Ok(Value::list(
            map.keys()
                .map(|k| Value::String(k.as_str().into()))
                .collect(),
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
        // BTreeMap iter is already key-sorted.
        Ok(Value::list(dict.map.values().cloned().collect()))
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
        Ok(make_iter_value(
            caps,
            crate::iter_protocol::KIND_LIST,
            args[0].clone(),
        ))
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
        Ok(make_iter_value(
            caps,
            crate::iter_protocol::KIND_STRING,
            args[0].clone(),
        ))
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
        Ok(make_iter_value(
            caps,
            crate::iter_protocol::KIND_DICT_ENTRIES,
            args[0].clone(),
        ))
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
        use crate::iter_protocol::{BRAND, FIELD_ID, FIELD_KIND, FIELD_SOURCE};
        use crate::iter_protocol::{KIND_DICT_ENTRIES, KIND_LIST, KIND_STRING};
        if iter_dict.brand.as_deref() != Some(BRAND) {
            return Err(RuntimeError::TypeMismatch {
                expected: BRAND.to_string(),
                found: iter_dict
                    .brand
                    .clone()
                    .unwrap_or_else(|| "Dict".to_string()),
                range,
            });
        }
        let kind = iter_dict
            .map
            .get(FIELD_KIND)
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
            .get(FIELD_SOURCE)
            .ok_or_else(|| RuntimeError::TypeMismatch {
                expected: "Iter with `_source` field".to_string(),
                found: "Iter without `_source`".to_string(),
                range,
            })?;
        let iter_id = iter_dict
            .map
            .get(FIELD_ID)
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
            KIND_LIST => {
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
            KIND_STRING => {
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
            KIND_DICT_ENTRIES => {
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
                // BTreeMap iter is key-sorted; matches the order used
                // by `materialize_iterable` so user-side `it.next()`
                // walks pairs in the same order a `for kv in d.iter()`
                // would. O(n) per `next()` (linear walk to idx); the
                // comprehension fast path avoids this entirely.
                let total = src_dict.map.len();
                caps.iter_cursor_fetch_and_inc(iter_id, total).map(|idx| {
                    let (key, v) = src_dict
                        .map
                        .iter()
                        .nth(idx)
                        .map(|(k, v)| (k.as_str(), v.clone()))
                        .unwrap_or(("", Value::Null));
                    Value::list(vec![Value::String(key.into()), v])
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
            map.insert(SmolStr::from("value"), v);
            Value::variant_dict(map, "Some".to_string(), "Option".to_string())
        }
        None => Value::variant_dict(
            std::collections::BTreeMap::<SmolStr, Value>::new(),
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
    map.insert(
        SmolStr::from(crate::iter_protocol::FIELD_KIND),
        Value::String(kind.into()),
    );
    map.insert(SmolStr::from(crate::iter_protocol::FIELD_SOURCE), source);
    // `_id` is `i64`-coerced from a `u64` so the existing
    // `Value::Int(i64)` representation can carry it without inventing
    // a new variant. `IterNext` reads it back via `as u64` round-trip.
    map.insert(
        SmolStr::from(crate::iter_protocol::FIELD_ID),
        Value::Int(caps.next_iter_id() as i64),
    );
    Value::branded_dict(map, Some(crate::iter_protocol::BRAND.to_string()))
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

/// Borrow the underlying [`SmolStr`] when the caller needs the
/// container itself (not just a `&str`) — typically because it intends
/// to surface the SmolStr-side ASCII oracle into a downstream helper
/// that takes an [`AsciiHint`].
///
/// The hot case-fold path (`upper` / `lower` / `title` / locale
/// variants) reaches in here so it can pass
/// `AsciiHint::AllAscii` / `KnownNonAscii` to
/// [`fold_string_to_smol_with_hint`], avoiding the per-call SIMD scan
/// the historical `AsciiHint::Unknown` shape forced on the fold
/// engine.
fn expect_smol_string(
    value: &Value,
    range: relon_parser::TokenRange,
) -> Result<&SmolStr, RuntimeError> {
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

// ============================================================
// Stdlib JSON Schema parity wave (2026-05-23)
// ============================================================
//
// The native fns below cover the `format` keyword family + the
// numeric / list combinators JSON Schema authors reach for. All
// pure-Rust, no I/O, no clocks — they slot into a `#schema` field
// predicate exactly like `glob_match`. See discussion in this
// session's transcript for the rationale + selection criteria.

/// `matches(s, pattern) -> Bool` — full regex (unlike `glob_match`'s
/// LuaJIT-subset). Re-compiled per call; predicate authors who care
/// about throughput can pre-anchor the pattern. The crate-level
/// `regex = "1"` dependency uses RE2-style guaranteed linear time so
/// untrusted patterns cannot ReDoS the evaluator.
struct StringMatches;
impl RelonFunction for StringMatches {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        let s = expect_string(&args[0], range)?;
        let pattern = expect_string(&args[1], range)?;
        let re = regex::Regex::new(pattern)
            .map_err(|e| RuntimeError::ValidationError(format!("invalid regex: {}", e), range))?;
        Ok(Value::Bool(re.is_match(s)))
    }
}

/// `ends_with(s, suffix) -> Bool` — sibling to the existing
/// `StringStartsWith`; no method-form yet, so we add both.
struct StringEndsWith;
impl RelonFunction for StringEndsWith {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        Ok(Value::Bool(
            expect_string(&args[0], range)?.ends_with(expect_string(&args[1], range)?),
        ))
    }
}

/// `is_email(s) -> Bool` — covers RFC 5321 §4.5.3.1.1 local-part
/// length cap (64), RFC 5321 §4.5.3.1.2 domain length cap (255),
/// and a deliberately conservative character set. We do not
/// implement the full RFC 5322 grammar (which permits quoted
/// strings, comments, etc.); the goal is JSON Schema
/// `"format": "email"` parity, which the spec leaves loosely
/// defined as "implementations SHOULD validate per RFC 5321".
struct IsEmail;
impl RelonFunction for IsEmail {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let s = expect_string(&args[0], range)?;
        Ok(Value::Bool(is_email_str(s)))
    }
}

fn is_email_str(s: &str) -> bool {
    let Some(at) = s.find('@') else { return false };
    let (local, domain_with_at) = s.split_at(at);
    let domain = &domain_with_at[1..];
    if local.is_empty() || local.len() > 64 {
        return false;
    }
    if domain.is_empty() || domain.len() > 255 {
        return false;
    }
    if !local
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ".!#$%&'*+/=?^_`{|}~-".contains(c))
    {
        return false;
    }
    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return false;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    for label in &labels {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return false;
        }
        if label.starts_with('-') || label.ends_with('-') {
            return false;
        }
    }
    true
}

/// `is_uri(s) -> Bool` — RFC 3986 absolute-URI shape: scheme `:`
/// non-empty rest. Conservative — does NOT validate authority /
/// query / fragment per-component grammar, but covers the JSON
/// Schema `"format": "uri"` common case (rejecting obvious
/// malformed inputs like "no-scheme" or ":empty-scheme").
struct IsUri;
impl RelonFunction for IsUri {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let s = expect_string(&args[0], range)?;
        Ok(Value::Bool(is_uri_str(s)))
    }
}

fn is_uri_str(s: &str) -> bool {
    let Some(colon) = s.find(':') else {
        return false;
    };
    let scheme = &s[..colon];
    let rest = &s[colon + 1..];
    if scheme.is_empty() || rest.is_empty() {
        return false;
    }
    let mut chars = scheme.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
}

/// `is_uuid(s) -> Bool` — RFC 4122 canonical text form, case-insensitive.
struct IsUuid;
impl RelonFunction for IsUuid {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let s = expect_string(&args[0], range)?;
        Ok(Value::Bool(is_uuid_str(s)))
    }
}

fn is_uuid_str(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *b != b'-' {
                    return false;
                }
            }
            _ => {
                if !b.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

/// `is_iso_date(s) -> Bool` — RFC 3339 full-date: `YYYY-MM-DD`.
/// Validates that day is in-range for the given month/year.
struct IsIsoDate;
impl RelonFunction for IsIsoDate {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let s = expect_string(&args[0], range)?;
        Ok(Value::Bool(is_iso_date_str(s)))
    }
}

fn is_iso_date_str(s: &str) -> bool {
    if s.len() != 10 {
        return false;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        s[0..4].parse::<u32>(),
        s[5..7].parse::<u32>(),
        s[8..10].parse::<u32>(),
    ) else {
        return false;
    };
    if !(1..=12).contains(&month) || day < 1 {
        return false;
    }
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap =
                (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
            if leap {
                29
            } else {
                28
            }
        }
        _ => unreachable!(),
    };
    day <= max_day
}

/// `is_ipv4(s) -> Bool` — dotted-quad, each octet 0..=255.
struct IsIpv4;
impl RelonFunction for IsIpv4 {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let s = expect_string(&args[0], range)?;
        // Route through `core::net::Ipv4Addr` (added in 1.77) — the
        // ambient-API purity guard scans for the legacy std-prefixed
        // name, and `core::net` shares the same parser without it.
        Ok(Value::Bool(s.parse::<core::net::Ipv4Addr>().is_ok()))
    }
}

/// `is_ipv6(s) -> Bool` — RFC 4291 8-group hex with `::` shorthand.
struct IsIpv6;
impl RelonFunction for IsIpv6 {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let s = expect_string(&args[0], range)?;
        Ok(Value::Bool(s.parse::<core::net::Ipv6Addr>().is_ok()))
    }
}

/// `multiple_of(n, divisor) -> Bool` — JSON Schema `multipleOf`.
/// Accepts Int/Int, Float/Float, Float/Int, Int/Float; division-
/// by-zero returns false (matches the JSON Schema "MUST be strictly
/// greater than 0" reading conservatively).
struct MultipleOf;
impl RelonFunction for MultipleOf {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        let result = match (&args[0], &args[1]) {
            (Value::Int(n), Value::Int(d)) => {
                if *d == 0 {
                    false
                } else {
                    n % d == 0
                }
            }
            (Value::Float(n), Value::Float(d)) => {
                let n = n.into_inner();
                let d = d.into_inner();
                if d == 0.0 {
                    false
                } else {
                    (n / d).fract().abs() < 1e-9
                }
            }
            (Value::Int(n), Value::Float(d)) => {
                let d = d.into_inner();
                if d == 0.0 {
                    false
                } else {
                    ((*n as f64) / d).fract().abs() < 1e-9
                }
            }
            (Value::Float(n), Value::Int(d)) => {
                let n = n.into_inner();
                if *d == 0 {
                    false
                } else {
                    (n / (*d as f64)).fract().abs() < 1e-9
                }
            }
            _ => {
                return Err(RuntimeError::TypeMismatch {
                    expected: "Int or Float operands".to_string(),
                    found: format!("{} / {}", args[0].type_name(), args[1].type_name()),
                    range,
                });
            }
        };
        Ok(Value::Bool(result))
    }
}

/// `to_json(v) -> String` — serialise any Value to compact JSON.
/// Mirrors `relon-evaluator::projector`'s output but as a free fn so
/// predicate authors can `to_json(x)` for diagnostic embedding.
struct ToJson;
impl RelonFunction for ToJson {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let json = value_to_json(&args[0]);
        let s = serde_json::to_string(&json).map_err(|e| {
            RuntimeError::ValidationError(format!("to_json serialise failed: {e}"), range)
        })?;
        Ok(Value::String(s.into()))
    }
}

/// `from_json(s) -> Value` — parse a JSON string into Relon's Value.
/// Numbers parse to Int when they round-trip exactly, otherwise
/// Float; objects parse to Dict (no brand); arrays to List.
struct FromJson;
impl RelonFunction for FromJson {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let s = expect_string(&args[0], range)?;
        let json: serde_json::Value = serde_json::from_str(s).map_err(|e| {
            RuntimeError::ValidationError(format!("from_json parse failed: {e}"), range)
        })?;
        Ok(json_to_value(json))
    }
}

/// Minimal `Value` → `serde_json::Value` for stdlib `to_json`. Schema /
/// EnumSchema / Closure / Wildcard / Brand-only / Native types fall to
/// `null` because they have no JSON representation; user code that
/// needs richer projection should reach for the host facade's
/// `projector::to_json_value` which respects #brand / Selector rules.
fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(f.into_inner())
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => serde_json::Value::String(s.as_str().to_owned()),
        Value::List(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        Value::Dict(dict) => {
            let map = dict
                .map
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), value_to_json(v)))
                .collect();
            serde_json::Value::Object(map)
        }
        _ => serde_json::Value::Null,
    }
}

fn json_to_value(json: serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f.into())
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => Value::String(s.into()),
        serde_json::Value::Array(a) => Value::list(a.into_iter().map(json_to_value).collect()),
        serde_json::Value::Object(o) => {
            Value::dict(o.into_iter().map(|(k, v)| (k, json_to_value(v))))
        }
    }
}

// ============================================================
// Stdlib JSON Schema parity wave — batch 2: numeric helpers
// ============================================================

/// `in_range(n, lo, hi) -> Bool` — inclusive range check. Accepts
/// any mix of Int / Float. JSON Schema `minimum` + `maximum`
/// covered.
struct InRange;
impl RelonFunction for InRange {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 3, range)?;
        let n = to_f64_val(&args[0]);
        let lo = to_f64_val(&args[1]);
        let hi = to_f64_val(&args[2]);
        Ok(Value::Bool(n >= lo && n <= hi))
    }
}

/// `round(n) -> Int` — banker's rounding (round-half-to-even via
/// f64's `round_ties_even` since 1.77). Int input returns
/// unchanged.
struct MathRound;
impl RelonFunction for MathRound {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        match &args[0] {
            Value::Int(n) => Ok(Value::Int(*n)),
            Value::Float(f) => Ok(Value::Int(f.into_inner().round_ties_even() as i64)),
            other => Err(RuntimeError::TypeMismatch {
                expected: "Number".to_string(),
                found: other.type_name().to_string(),
                range,
            }),
        }
    }
}

/// `floor(n) -> Int`
struct MathFloor;
impl RelonFunction for MathFloor {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        match &args[0] {
            Value::Int(n) => Ok(Value::Int(*n)),
            Value::Float(f) => Ok(Value::Int(f.into_inner().floor() as i64)),
            other => Err(RuntimeError::TypeMismatch {
                expected: "Number".to_string(),
                found: other.type_name().to_string(),
                range,
            }),
        }
    }
}

/// `ceil(n) -> Int`
struct MathCeil;
impl RelonFunction for MathCeil {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        match &args[0] {
            Value::Int(n) => Ok(Value::Int(*n)),
            Value::Float(f) => Ok(Value::Int(f.into_inner().ceil() as i64)),
            other => Err(RuntimeError::TypeMismatch {
                expected: "Number".to_string(),
                found: other.type_name().to_string(),
                range,
            }),
        }
    }
}

/// `sqrt(n) -> Float` — IEEE-754 sqrt; negative input returns
/// `NaN` per f64 semantics rather than erroring.
struct MathSqrt;
impl RelonFunction for MathSqrt {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        Ok(Value::Float(to_f64_val(&args[0]).sqrt().into()))
    }
}

/// `pow(base, exp) -> Float` — IEEE-754 powf.
struct MathPow;
impl RelonFunction for MathPow {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        Ok(Value::Float(
            to_f64_val(&args[0]).powf(to_f64_val(&args[1])).into(),
        ))
    }
}

// ============================================================
// Stdlib JSON Schema parity wave — batch 3: list helpers
// ============================================================

/// `unique(xs) -> Bool` — JSON Schema `uniqueItems`. O(N²) equality
/// (Value doesn't implement Hash); cheap for the typical small
/// list lengths predicates work with.
struct ListUnique;
impl RelonFunction for ListUnique {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let xs = expect_list(&args[0], range)?;
        for i in 0..xs.len() {
            for j in (i + 1)..xs.len() {
                if xs[i] == xs[j] {
                    return Ok(Value::Bool(false));
                }
            }
        }
        Ok(Value::Bool(true))
    }
}

/// `every(xs, p) -> Bool` — short-circuiting universal quantifier.
/// `xs.every(p)` is JSON Schema `contains: { allOf: [<p>] }` if
/// every element matches `<p>`. Empty list returns `true` (vacuous
/// truth, matches mathematical convention + JS `Array.prototype.every`).
struct ListEvery;
impl RelonFunction for ListEvery {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args.positional, 2, range)?;
        let list = expect_list(&args.positional[0], range)?;
        let func = &args.positional[1];
        let caps = args.caps();
        let items = list.to_vec();
        for item in items {
            caps.tick(1, range)?;
            let result = caps.call_relon(func, vec![item], range)?;
            match result {
                Value::Bool(true) => continue,
                Value::Bool(false) => return Ok(Value::Bool(false)),
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "Bool".to_string(),
                        found: other.type_name().to_string(),
                        range,
                    })
                }
            }
        }
        Ok(Value::Bool(true))
    }
}

/// `some(xs, p) -> Bool` — short-circuiting existential quantifier.
/// JSON Schema `contains: <p>` parity. Empty list returns `false`.
struct ListSome;
impl RelonFunction for ListSome {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        expect_arg_count(&args.positional, 2, range)?;
        let list = expect_list(&args.positional[0], range)?;
        let func = &args.positional[1];
        let caps = args.caps();
        let items = list.to_vec();
        for item in items {
            caps.tick(1, range)?;
            let result = caps.call_relon(func, vec![item], range)?;
            match result {
                Value::Bool(true) => return Ok(Value::Bool(true)),
                Value::Bool(false) => continue,
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "Bool".to_string(),
                        found: other.type_name().to_string(),
                        range,
                    })
                }
            }
        }
        Ok(Value::Bool(false))
    }
}

/// `trim(s) -> String` — strip leading + trailing Unicode whitespace.
struct StringTrim;
impl RelonFunction for StringTrim {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        Ok(Value::String(expect_string(&args[0], range)?.trim().into()))
    }
}

/// `trim_start(s) -> String`
struct StringTrimStart;
impl RelonFunction for StringTrimStart {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        Ok(Value::String(
            expect_string(&args[0], range)?.trim_start().into(),
        ))
    }
}

/// `trim_end(s) -> String`
struct StringTrimEnd;
impl RelonFunction for StringTrimEnd {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        Ok(Value::String(
            expect_string(&args[0], range)?.trim_end().into(),
        ))
    }
}

/// `select_keys(d, ks) -> Dict` — project a dict onto a subset of
/// keys. Missing keys are silently dropped. Mirrors JSON Schema
/// `additionalProperties: false` post-filter use case.
struct DictSelectKeys;
impl RelonFunction for DictSelectKeys {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        let dict = expect_dict(&args[0], range)?;
        let keys = expect_string_list(&args[1], range)?;
        let mut out = std::collections::BTreeMap::new();
        for k in &keys {
            if let Some(v) = dict.map.get(k.as_str()) {
                out.insert(crate::value::SmolStr::from(k.as_str()), v.clone());
            }
        }
        Ok(Value::dict(out))
    }
}

/// `omit_keys(d, ks) -> Dict` — drop a key set from a dict.
struct DictOmitKeys;
impl RelonFunction for DictOmitKeys {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 2, range)?;
        let dict = expect_dict(&args[0], range)?;
        let keys = expect_string_list(&args[1], range)?;
        let drop: std::collections::HashSet<&str> = keys.iter().map(|s| s.as_str()).collect();
        let mut out = std::collections::BTreeMap::new();
        for (k, v) in dict.map.iter() {
            if !drop.contains(k.as_str()) {
                out.insert(k.clone(), v.clone());
            }
        }
        Ok(Value::dict(out))
    }
}

/// `size_in_range(d, lo, hi) -> Bool` — JSON Schema
/// `minProperties` / `maxProperties` covered. Inclusive bounds.
/// Also accepts a List receiver, in which case it's
/// `minItems` / `maxItems`.
struct SizeInRange;
impl RelonFunction for SizeInRange {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 3, range)?;
        let len = match &args[0] {
            Value::Dict(d) => d.map.len() as i64,
            Value::List(l) => l.len() as i64,
            Value::String(s) => s.chars().count() as i64,
            other => {
                return Err(RuntimeError::TypeMismatch {
                    expected: "Dict / List / String".to_string(),
                    found: other.type_name().to_string(),
                    range,
                });
            }
        };
        let lo = expect_int(&args[1], range)?;
        let hi = expect_int(&args[2], range)?;
        Ok(Value::Bool(len >= lo && len <= hi))
    }
}

/// `parse_iso_date(s) -> Dict { year, month, day }` — parse an
/// `YYYY-MM-DD` string into a structured dict. Returns `Value::Null`
/// when the format is invalid. Avoids a `chrono` dep — date math
/// stays on the caller side via `year` / `month` / `day` fields.
struct ParseIsoDate;
impl RelonFunction for ParseIsoDate {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let s = expect_string(&args[0], range)?;
        if !is_iso_date_str(s) {
            return Ok(Value::Null);
        }
        let year: i64 = s[0..4].parse().unwrap();
        let month: i64 = s[5..7].parse().unwrap();
        let day: i64 = s[8..10].parse().unwrap();
        let mut map = std::collections::BTreeMap::new();
        map.insert(crate::value::SmolStr::from("year"), Value::Int(year));
        map.insert(crate::value::SmolStr::from("month"), Value::Int(month));
        map.insert(crate::value::SmolStr::from("day"), Value::Int(day));
        Ok(Value::dict(map))
    }
}

/// `count(xs) -> Int` — list length as Int. Convenience wrapper so
/// predicate authors don't need `xs.length()` (which routes through
/// the polymorphic `len` and returns Int already, but `count` reads
/// more naturally in a numeric predicate context).
struct ListCount;
impl RelonFunction for ListCount {
    fn call(
        &self,
        args: NativeArgs,
        range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        let args = args.into_positional();
        expect_arg_count(&args, 1, range)?;
        let xs = expect_list(&args[0], range)?;
        Ok(Value::Int(xs.len() as i64))
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
        // Exclude the delimited gated-capability-primitive region. Those
        // two impls (`clock` / `random`) are the endorsed exception: they
        // ARE gated host fns (`register_fn` + non-empty `NativeFnGate`),
        // which is exactly what this guard's error message directs ambient
        // capabilities toward. The ban applies to *ungated* ambient APIs.
        let source: String = match (
            source.find("// GATED-CAPABILITY-PRIMITIVES-BEGIN"),
            source.find("// GATED-CAPABILITY-PRIMITIVES-END"),
        ) {
            (Some(b), Some(e)) if e > b => {
                let mut s = source[..b].to_string();
                s.push_str(&source[e..]);
                s
            }
            _ => source.to_string(),
        };
        let source = source.as_str();
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

#[cfg(test)]
mod ascii_hint_wiring_tests {
    //! `#163` follow-up — parity checks for the SmolStr-side ASCII
    //! oracle wired into [`fold_string_to_smol_with_hint`] / the
    //! `String{Upper,Lower,Title}{,Locale}` surface helpers.
    //!
    //! The hint must change *performance only*: the byte-identical
    //! output is the load-bearing contract every downstream caller
    //! depends on. These tests fix the input via [`SmolStr`] (matching
    //! the live call shape) and assert that
    //! `fold_string_to_smol_with_hint(s, mode, false, hint_from_smol)`
    //! matches the legacy `Unknown` path.
    use super::{fold_string_to_smol_with_hint, AsciiHint, CaseFoldMode};
    use relon_eval_api::SmolStr;

    fn parity(input: &str) {
        let smol = SmolStr::from_borrowed(input);
        let hint = AsciiHint::from_smol(&smol);
        for mode in [
            CaseFoldMode::Upper,
            CaseFoldMode::Lower,
            CaseFoldMode::Title,
        ] {
            let with_hint = fold_string_to_smol_with_hint(smol.as_str(), mode, false, hint);
            let unknown =
                fold_string_to_smol_with_hint(smol.as_str(), mode, false, AsciiHint::Unknown);
            assert_eq!(
                with_hint.as_str(),
                unknown.as_str(),
                "input={input:?} mode={mode:?} hint={hint:?}"
            );
        }
    }

    #[test]
    fn inline_ascii_payload_matches_unknown() {
        // Inline path (≤ 22 bytes) + ASCII — exercises the
        // `AsciiHint::AllAscii` arm in `fold_string_to_smol_ascii_fast`
        // which now skips the redundant `s.is_ascii()` re-scan.
        for input in ["hi", "Hello, World!", "the QUICK brown fox 1"] {
            parity(input);
        }
    }

    #[test]
    fn inline_non_ascii_payload_matches_unknown() {
        // Inline path + non-ASCII — exercises the
        // `AsciiHint::KnownNonAscii` arm which now bails out of the
        // inline fast path without touching the bytes.
        for input in ["caf\u{00E9}", "stra\u{00DF}e", "n\u{00E9}e"] {
            parity(input);
        }
    }

    #[test]
    fn heap_ascii_payload_matches_unknown() {
        // Heap path (> 22 bytes) + ASCII — the inline fast path is
        // off-limits because the output would not fit; the hint
        // propagates into `fold_string_with_ascii_hint` and routes
        // through `case_fold_ascii_fast_into_string`.
        let big = "a".repeat(64);
        parity(&big);
        parity("the quick brown fox jumps over the lazy dog 1234567890");
    }

    #[test]
    fn heap_non_ascii_payload_matches_unknown() {
        // Heap path + non-ASCII — `KnownNonAscii` skips the
        // `fold_ascii_prefix_into_string` scan and lands directly in
        // the per-codepoint slow path. Multi-cp mappings (ß -> SS,
        // sigma-final, combining marks) must still produce the
        // legacy output.
        let mixed = format!("{}stra\u{00DF}e {}", "x".repeat(16), "y".repeat(16));
        parity(&mixed);
        parity("\u{03A3}\u{0391} \u{03A3}\u{03B1} \u{03A3}\u{0391}");
    }
}
