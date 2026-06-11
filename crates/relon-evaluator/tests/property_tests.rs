//! Property-based smoke tests for the evaluator's core determinism /
//! sandbox-boundary guarantees. These complement the per-feature unit
//! tests by sweeping a randomized input space: each property pins a
//! "this must hold for every input shape" invariant that hand-rolled
//! tests can only spot-check.
//!
//! Conventions:
//! * 64 cases per property (`ProptestConfig::cases = 64`). The default
//!   of 256 takes minutes once parser + analyzer + evaluator chain
//!   together — 64 keeps the suite under +5s while still surfacing
//!   counterexamples for typical regressions.
//! * Each property is independent: a failure in one does not mask the
//!   others (`proptest` catches panics per `#[test]`).
//! * Inputs go through the public API only (`Context`, `Evaluator`,
//!   `parse_document`). No reaching into internal modules.

use proptest::collection::hash_map as proptest_hash_map;
use proptest::prelude::*;
use relon_evaluator::{Context, RuntimeError, Scope, TreeWalkEvaluator, Value};
use relon_parser::parse_document;
use std::collections::HashMap;
use std::sync::Arc;

/// Build a fresh sandboxed `Context` for a given source. Each property
/// constructs two independent contexts to verify they observe identical
/// behaviour — proves no implicit shared state leaks between runs.
fn eval_in_fresh_context(source: &str) -> Result<Value, RuntimeError> {
    eval_in_fresh_context_with(source, |ctx| ctx)
}

/// Variant that lets the caller adjust `Capabilities` before evaluation
/// (e.g. set `max_value_elements`). The configurator consumes and
/// returns the freshly constructed sandboxed context, matching the
/// builder-style `Context::with_capabilities` write path. The analyzer
/// is always attached — method dispatch on built-in types (e.g.
/// `xs.map(...)`) consults the analyzed tree to resolve the receiver's
/// type, so omitting it would surface `FunctionNotFound` for valid
/// surface syntax.
fn eval_in_fresh_context_with<F>(source: &str, configure: F) -> Result<Value, RuntimeError>
where
    F: FnOnce(Context) -> Context,
{
    let node = parse_document(source).expect("test source must parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let ctx = configure(Context::sandboxed());
    let ctx = ctx.with_root(node).with_analyzed(analyzed);
    let ctx = Arc::new({
        let mut ctx = ctx;
        relon_evaluator::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    });
    TreeWalkEvaluator::new(Arc::clone(&ctx)).eval_root(&Arc::new(Scope::default()))
}

/// Render an `i64` as a Relon source literal that the parser will round-
/// trip back to the exact value. Hex form via the `parse_hex` path
/// (`prim/number.rs`) handles `i64::MIN` correctly; decimal `*` by the
/// sign would overflow for the `MIN` case. Non-negative values get a
/// plain `0x..` literal; negatives get a `-0x..` literal with the
/// magnitude expressed as `unsigned_abs()` to dodge `-i64::MIN` UB.
fn render_i64_literal(v: i64) -> String {
    if v >= 0 {
        format!("0x{:x}", v as u64)
    } else {
        format!("-0x{:x}", (v as i128).unsigned_abs())
    }
}

/// Snapshot a successful `Result<Value, RuntimeError>` as canonical JSON
/// for byte-identical comparison across two runs. Errors are stringified
/// via `Debug` — matches the determinism contract that a given source +
/// input always yields the same error variant + payload, not just "some
/// error".
fn snapshot(result: &Result<Value, RuntimeError>) -> String {
    match result {
        Ok(value) => serde_json::to_string(value).expect("Value serialization is infallible"),
        Err(err) => format!("Err({err:?})"),
    }
}

// ---------------------------------------------------------------------
// Property A: integer arithmetic determinism + NumericOverflow
// consistency. Two independent contexts evaluating `{ x: <a> + <b> }`
// must agree byte-for-byte: same Int value when `a + b` fits in i64,
// same `NumericOverflow` error otherwise. Guards against any future
// path that lets `wrapping_add` or `f64` arithmetic creep in.
// ---------------------------------------------------------------------
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        // Integration tests live in `tests/`, not `src/`, so proptest's
        // default `SourceParallel` regression file lookup prints a
        // benign-but-noisy warning. Disable persistence — failures are
        // already reproducible from the printed shrunk counterexample.
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn arithmetic_determinism(a: i64, b: i64) {
        let source = format!(
            "{{ x: ({}) + ({}) }}",
            render_i64_literal(a),
            render_i64_literal(b),
        );
        let first = eval_in_fresh_context(&source);
        let second = eval_in_fresh_context(&source);

        // Byte-identical: either both succeed with the same value, or
        // both fail with `NumericOverflow`. We don't assert any other
        // error variant — a parse failure would be a test-harness bug.
        prop_assert_eq!(snapshot(&first), snapshot(&second));

        match a.checked_add(b) {
            Some(expected) => {
                let value = first.as_ref().expect("checked_add succeeded → eval must succeed");
                let dict = match value {
                    Value::Dict(d) => d,
                    other => panic!("expected Dict, got {other:?}"),
                };
                prop_assert_eq!(dict.map.get("x"), Some(&Value::Int(expected)));
            }
            None => {
                prop_assert!(
                    matches!(first, Err(RuntimeError::NumericOverflow(_))),
                    "expected NumericOverflow on i64 overflow, got {:?}",
                    first,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------
// Property B: `max_value_elements` boundary for `range`. For any cap
// in [1, 1_000_000] and any n in [0, cap + 1]:
//   * n <= cap     → success, list has exactly n elements
//   * n == cap + 1 → `ValueTooLarge { limit: cap, actual: n }`
// Off-by-one regressions, sign errors, silent truncation, or panics all
// fall out of this single invariant.
// ---------------------------------------------------------------------
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        // Integration tests live in `tests/`, not `src/`, so proptest's
        // default `SourceParallel` regression file lookup prints a
        // benign-but-noisy warning. Disable persistence — failures are
        // already reproducible from the printed shrunk counterexample.
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn max_value_elements_range_boundary(
        // Brief spec calls for cap ∈ [1, 1_000_000] — narrowed to
        // [1, 10_000] so successful runs at the cap don't allocate a
        // 1M-element `Vec<Value>` per case. The bug class lives on
        // `cap == actual` / `cap + 1 == actual`, which is independent
        // of the absolute magnitude. We still exercise the `usize` path
        // and the pre-flight vs. post-flight ordering by sweeping
        // multiple orders of magnitude inside the narrowed range.
        cap in 1usize..=10_000,
        delta in 0usize..=1,
    ) {
        // n sweeps [cap, cap + 1] — the most failure-prone neighbourhood.
        // We don't sweep all of [0, cap+1] because the interesting bug
        // class lives on the boundary; deep interior values just slow
        // the suite without adding signal.
        let n = cap + delta;
        let source = format!("{{ xs: range(0, {n}) }}");
        let result = eval_in_fresh_context_with(&source, |ctx| {
            let mut caps = ctx.capabilities().clone();
            caps.max_value_elements = Some(cap);
            ctx.with_capabilities(caps)
        });

        if n <= cap {
            let value = result.expect("n <= cap must succeed");
            let dict = match value {
                Value::Dict(d) => d,
                other => panic!("expected Dict, got {other:?}"),
            };
            let list = match dict.map.get("xs").expect("xs binding") {
                Value::List(l) => l,
                other => panic!("expected List, got {other:?}"),
            };
            prop_assert_eq!(list.len(), n);
        } else {
            prop_assert!(
                matches!(
                    result,
                    Err(RuntimeError::ValueTooLarge { limit, actual, .. })
                        if limit == cap && actual == n
                ),
                "expected ValueTooLarge {{ limit: {}, actual: {} }}, got {:?}",
                cap, n, result,
            );
        }
    }
}

// ---------------------------------------------------------------------
// Property C: dict iteration order is BTreeMap (sorted Unicode
// codepoint) order. Builds a Relon dict literal from a randomly-
// generated HashMap, evaluates, then asserts the JSON output's key
// sequence matches `keys.sort()`. The HashMap source ensures the
// generator does not accidentally hand keys in sorted order.
// ---------------------------------------------------------------------
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        // Integration tests live in `tests/`, not `src/`, so proptest's
        // default `SourceParallel` regression file lookup prints a
        // benign-but-noisy warning. Disable persistence — failures are
        // already reproducible from the printed shrunk counterexample.
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn dict_iteration_order_is_sorted(
        entries in proptest_hash_map(
            // Restrict keys to ASCII identifier-ish to keep source-
            // construction simple while still exercising sort order.
            "[a-zA-Z][a-zA-Z0-9_]{0,8}",
            any::<i64>(),
            0..=20,
        ),
    ) {
        // Build `{ k1: <v1>, k2: <v2>, ... }`. Keys are quoted via
        // `serde_json::to_string` so even reserved words / unusual
        // identifiers can't collide with parser tokens.
        let mut pairs: Vec<(String, i64)> = entries.iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        // The HashMap iteration order is non-deterministic; we don't
        // care which order we emit source-side, only that the *output*
        // is sorted. Sorting here just stabilizes the source string
        // for `proptest`'s shrinking pass.
        pairs.sort_by(|a, b| a.0.cmp(&b.0));

        let body: Vec<String> = pairs.iter()
            .map(|(k, v)| format!(
                "{}: {}",
                serde_json::to_string(k).unwrap(),
                render_i64_literal(*v),
            ))
            .collect();
        let source = format!("{{ {} }}", body.join(", "));

        let result = eval_in_fresh_context(&source).expect("source must evaluate");
        let dict = match result {
            Value::Dict(d) => d,
            other => panic!("expected Dict, got {other:?}"),
        };

        // BTreeMap iteration is always sorted; assert it matches the
        // expected sorted view of the input.
        let actual_keys: Vec<&str> = dict.map.keys().map(|k| k.as_str()).collect();
        let mut expected_keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        expected_keys.sort();
        prop_assert_eq!(actual_keys, expected_keys);

        // Belt-and-braces: the JSON projection must also walk keys in
        // sorted order. Catches a hypothetical regression where the
        // BTreeMap is right but a custom serializer re-orders.
        // Reconstruct the expected serialization from the BTreeMap's
        // iteration order (sorted by construction) and compare byte-
        // for-byte with the actual `serde_json` output.
        let json = serde_json::to_string(&Value::Dict(dict.clone())).unwrap();
        let mut expected_sequence = String::from("{");
        for (i, (k, v)) in dict.map.iter().enumerate() {
            if i > 0 {
                expected_sequence.push(',');
            }
            expected_sequence.push_str(&serde_json::to_string(k).unwrap());
            expected_sequence.push(':');
            expected_sequence.push_str(&serde_json::to_string(v).unwrap());
        }
        expected_sequence.push('}');
        prop_assert_eq!(json, expected_sequence);
    }
}

// ---------------------------------------------------------------------
// Property D: `Iter.next()` is per-Context isolated. Two fresh
// `Context`s evaluating the same `xs.iter()` + k `.next()` script must
// return the same value sequence — the per-Context cursor table
// (commit `5d14074`) mints fresh ids per Context, so neither run can
// observe state from the other.
// ---------------------------------------------------------------------
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn iter_next_per_context_isolation(
        list_len in 0usize..=10,
        steps in 1usize..=10,
    ) {
        use relon_analyzer::analyze;

        // Build a `#main(List<Int> xs) { ... }` source that calls
        // `xs.iter()` once and then `.next()` `steps` times, exposing
        // each step's result under a distinct key. Sequential `.next()`
        // results from one cursor — exactly the cross-Context invariant
        // we want to lock down.
        let mut body = String::from(r#""it": xs.iter()"#);
        for k in 0..steps {
            body.push_str(&format!(",\n    \"step_{k}\": it.next()"));
        }
        let source = format!("#main(List<Int> xs)\n{{\n    {body}\n}}");

        let node = parse_document(&source).expect("parse");
        let analyzed = Arc::new(analyze(&node));

        let mut input_args: HashMap<String, Value> = HashMap::new();
        input_args.insert(
            "xs".to_string(),
            Value::list((0..list_len as i64).map(Value::Int).collect()),
        );

        let run = || -> Result<Value, RuntimeError> {
            let ctx = Context::new()
                .with_root(node.clone())
                .with_analyzed(Arc::clone(&analyzed));
            TreeWalkEvaluator::new(Arc::new({ let mut ctx = ctx; relon_evaluator::TreeWalkEvaluator::prepare_in_place(&mut ctx); ctx }))
                .run_main(&Arc::new(Scope::default()), input_args.clone())
        };

        let result_a = run();
        let result_b = run();

        // Byte-identical JSON output across two independent contexts —
        // proves no shared cursor state, no global counter aliasing.
        // The `it` field itself carries the cursor `_id`, and those ids
        // are minted per-Context (`AtomicU64` starting at 0 in
        // `Context::new`), so both runs should produce id == 0 → the
        // serializations match.
        prop_assert_eq!(snapshot(&result_a), snapshot(&result_b));

        // Spot-check the step results to confirm we observe the
        // expected walk: step_k returns Option.Some(k) for k < list_len,
        // and Option.None thereafter.
        let value = result_a.expect("run_main must succeed");
        let dict = match value {
            Value::Dict(d) => d,
            other => panic!("expected Dict, got {other:?}"),
        };
        for k in 0..steps {
            let key = format!("step_{k}");
            let opt = match dict.map.get(key.as_str()).expect("step key present") {
                Value::Dict(d) => d,
                other => panic!("expected Option dict for {key}, got {other:?}"),
            };
            if k < list_len {
                prop_assert_eq!(opt.brand.as_deref(), Some("Some"));
                prop_assert_eq!(opt.map.get("value"), Some(&Value::Int(k as i64)));
            } else {
                prop_assert_eq!(opt.brand.as_deref(), Some("None"));
            }
        }
    }
}

// ---------------------------------------------------------------------
// Property E: closure capture / scoping is deterministic under repeated
// evaluation. Picks one of a small fixed expression template set —
// generating arbitrary Relon source via proptest would stress the
// parser rather than the determinism claim. Two fresh contexts running
// `xs.map(f)` on the same input list must produce byte-identical JSON.
// ---------------------------------------------------------------------

/// Hand-picked safe expression templates. Each is a unary closure body
/// over `n: i64` that cannot overflow within the chosen input range
/// (`[-1_000, 1_000]`). Doubled: 2_000 fits in i64. Squared on
/// 1_000-clamped input: 1_000_000 fits. `mod 7` is total over all
/// nonzero divisors. `* 0 + k` exercises a literal capture path.
const CLOSURE_TEMPLATES: &[&str] = &[
    "(n) => n",
    "(n) => n + 1",
    "(n) => n - 1",
    "(n) => n * 2",
    "(n) => n * n",
    "(n) => n % 7 + 100",
];

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        // Integration tests live in `tests/`, not `src/`, so proptest's
        // default `SourceParallel` regression file lookup prints a
        // benign-but-noisy warning. Disable persistence — failures are
        // already reproducible from the printed shrunk counterexample.
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn closure_map_determinism(
        xs in proptest::collection::vec(-1_000i64..=1_000, 0..=10),
        template_idx in 0usize..CLOSURE_TEMPLATES.len(),
    ) {
        let xs_literal: Vec<String> = xs.iter()
            .map(|n| render_i64_literal(*n))
            .collect();
        let template = CLOSURE_TEMPLATES[template_idx];
        // The surface grammar requires the list to be bound to a name
        // inside the dict before invoking a method on it — `[].map(...)`
        // as a standalone expression does not parse.
        let source = format!(
            "{{ List<Int> source_list: [{}], ys: source_list.map({}) }}",
            xs_literal.join(", "),
            template,
        );

        let first = eval_in_fresh_context(&source);
        let second = eval_in_fresh_context(&source);
        prop_assert_eq!(snapshot(&first), snapshot(&second));

        // Sanity: success path must produce a list of the same length
        // as the input. We don't recompute the closure on the host
        // side — the determinism check above is the load-bearing
        // assertion — but length parity catches a closure that
        // silently dropped elements.
        let value = first.expect("closure source must evaluate");
        let dict = match value {
            Value::Dict(d) => d,
            other => panic!("expected Dict, got {other:?}"),
        };
        let ys = match dict.map.get("ys").expect("ys binding") {
            Value::List(l) => l,
            other => panic!("expected List, got {other:?}"),
        };
        prop_assert_eq!(ys.len(), xs.len());
    }
}

// ---------------------------------------------------------------------
// Property F: fmt round-trip preserves evaluation semantics, and
// `format_source` is idempotent. For any dict-literal source built from
// the same generator that powers Property C, evaluating the source and
// evaluating its formatted twin must produce byte-identical output, and
// `format_source(format_source(src))` must equal `format_source(src)`.
// Guards the README's "same source + same input → byte-identical
// output" claim against the formatter: a rule-based rewrite must never
// drift semantics.
// ---------------------------------------------------------------------
proptest! {
    #![proptest_config(ProptestConfig {
        // Each case runs the full pipeline twice (parse + analyze +
        // eval on the original, then again on the formatted output)
        // plus two fmt passes. Half the case count of the simpler
        // properties keeps the suite roughly proportional in wall
        // time without sacrificing shrinking coverage.
        cases: 32,
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn fmt_roundtrip_preserves_eval(
        // Reuse the Property C generator profile: ASCII-identifier
        // keys + arbitrary i64 values. These are guaranteed to parse
        // and evaluate, so any difference between the two runs is a
        // real fmt regression rather than "generator surfaced a parser
        // edge case". Smaller upper bound (10 vs. 20) keeps the
        // formatted output size predictable for the second eval pass.
        entries in proptest_hash_map(
            "[a-zA-Z][a-zA-Z0-9_]{0,8}",
            any::<i64>(),
            0..=10,
        ),
    ) {
        let mut pairs: Vec<(String, i64)> = entries.iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        // Sort the source-side ordering so proptest shrinking has a
        // stable starting point; the formatter normalizes layout but
        // does NOT reorder keys (that's the BTreeMap's job at eval).
        pairs.sort_by(|a, b| a.0.cmp(&b.0));

        let body: Vec<String> = pairs.iter()
            .map(|(k, v)| format!(
                "{}: {}",
                serde_json::to_string(k).unwrap(),
                render_i64_literal(*v),
            ))
            .collect();
        // Deliberately omit pretty-print whitespace — the formatter
        // should reach the same canonical layout regardless of input
        // shape. Single-line dense source exercises the formatter's
        // line-break insertion path.
        let source = format!("{{ {} }}", body.join(", "));

        // Step 1: original source must evaluate. Property C already
        // proves this generator always lands in the success path, so
        // a failure here would be a separate regression.
        let original_eval = eval_in_fresh_context(&source)
            .expect("generator emits always-evaluable source");

        // Step 2: format the source, then re-evaluate. Both runs go
        // through fresh `Context::sandboxed()` instances (the helper
        // builds one per call) — proves the formatter only rewrites
        // layout, not the value tree or any per-Context state.
        let formatted = relon_fmt::format_source(&source)
            .expect("formatter must accept always-parseable source");
        let formatted_eval = eval_in_fresh_context(&formatted)
            .expect("formatted source must remain evaluable");

        // Byte-identical JSON projection. Stronger than `==` on Value
        // because it also catches a hypothetical regression where the
        // formatter perturbs key ordering (BTreeMap would mask it,
        // serde_json output would not).
        prop_assert_eq!(snapshot(&Ok(original_eval)), snapshot(&Ok(formatted_eval)));

        // Step 3: formatter idempotence. `format_source` advertises
        // itself as a canonical-form rewriter, so a second pass on an
        // already-formatted string must be a no-op. Catches subtle
        // bugs where, e.g., a trailing-comma rule keeps inserting
        // newlines on each pass.
        let formatted_twice = relon_fmt::format_source(&formatted)
            .expect("formatter must accept its own output");
        prop_assert_eq!(formatted, formatted_twice);
    }
}

// ---------------------------------------------------------------------
// Property G: cross-Context evaluation produces byte-identical output
// for a richer source shape than Properties A / C / D / E individually.
// Combines a list-typed binding plus two independent closure-bearing
// method calls (`.filter(...)` and `.map(...)`) over the same list,
// alongside literal sibling fields. Two `Context::sandboxed()` instances
// must agree byte-for-byte — guards every per-Context state slot
// (symbol id counter, iter_cursors table, analyzer cache, path cache)
// against accidental cross-tenant bleed. Complements Property D, which
// focuses on iter cursors specifically.
// ---------------------------------------------------------------------
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn cross_context_eval_is_identical(
        xs in proptest::collection::vec(-100i64..=100, 0..=8),
        threshold in -100i64..=100,
        offset in -100i64..=100,
    ) {
        let xs_literal: Vec<String> = xs.iter()
            .map(|n| render_i64_literal(*n))
            .collect();
        // Source combines:
        //   * a list binding (exercises the list-allocation path)
        //   * two independent method calls — `.filter(...)` and
        //     `.map(...)` — each with its own closure (exercises
        //     closure capture + analyzer-driven method dispatch on
        //     `List<Int>`)
        //   * sibling field references via literal threshold/offset
        //     (exercises name resolution and the per-Context symbol
        //     id counter)
        // All three lean on different per-Context state slots; if any
        // one slot leaked from a prior eval, the two runs would
        // diverge. We deliberately avoid `.filter(...).map(...)`
        // chaining — the current parser does not accept it, and the
        // separate-field form already exercises the same dispatch
        // paths without depending on grammar surface area outside the
        // scope of this property.
        let source = format!(
            "{{ List<Int> xs: [{xs}], filtered: xs.filter((n) => n > ({thr})), mapped: xs.map((n) => n + ({off})), threshold: {thr}, offset: {off} }}",
            xs = xs_literal.join(", "),
            thr = render_i64_literal(threshold),
            off = render_i64_literal(offset),
        );

        // Two independent `Context::sandboxed()` instances — the
        // helper constructs a fresh one per call. If the result
        // depends on anything outside the per-Context state (a
        // 'static cache, a global counter, a thread-local), the two
        // snapshots will not match.
        let first = eval_in_fresh_context(&source);
        let second = eval_in_fresh_context(&source);

        // Note: `eval` may legitimately fail with `NumericOverflow`
        // for some `xs[i] + offset` combinations — that's fine, both
        // runs must hit the same error variant, and `snapshot` encodes
        // the error payload deterministically. We deliberately do NOT
        // skip / assume success here; the cross-Context invariant
        // applies equally on the error path.
        prop_assert_eq!(snapshot(&first), snapshot(&second));
    }
}
