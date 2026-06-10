//! Reverse stdlib-drift defense + tier-2 tree-walker-only execution
//! coverage.
//!
//! Background: `stdlib::register_to` (crates/relon-evaluator/src/stdlib.rs)
//! registers ~50 free functions via `register_pure_fn(name, ...)`. The
//! analyzer's drift-defense test
//! (`relon-analyzer::typecheck::tests::stage3_stdlib_signatures_cover_all_register_fn_names`)
//! only checks that a *curated* allowlist of names HAS analyzer
//! signatures; it never enumerates the evaluator's real register sites.
//! As a result a whole wave of free functions — the JSON-Schema parity
//! batch (`is_email` / `to_json` / `trim` / `unique` / `count` / ...) and
//! the numeric helpers (`sqrt` / `pow` / `round` / `floor` / `ceil`) —
//! drifted in with ZERO analyzer signatures and ZERO test coverage.
//!
//! The functions in that drift set are *tier-2 tree-walker-only*: because
//! they lack an analyzer signature, the analyzer rejects any program that
//! calls them in its default strict mode with
//! `ExpressionTypeUnknown: "call to <fn> has no static return type"`. They
//! can only be reached through the tree-walking evaluator; they do NOT run
//! on the cranelift / LLVM / wasm-AOT backends.
//!
//! This module:
//!   1. Pins that drift set (`TIER2_TREEWALK_ONLY_DRIFT`) and FAILS if the
//!      evaluator gains a NEW free fn without either an analyzer signature
//!      or an explicit drift-allowlist entry — i.e. the reverse of the
//!      analyzer-side test, driven from the evaluator's real register
//!      sites rather than a curated name list.
//!   2. Exercises a representative slice of the drift set through the
//!      tree-walker against a golden fixture so they get *some* execution
//!      coverage.

use crate::{Context, RuntimeError, Scope, TreeWalkEvaluator, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

/// Names registered with `register_pure_fn` by the evaluator that have
/// NO signature in the analyzer's `stdlib_signatures` table. These are
/// the tier-2 tree-walker-only free functions: reachable through the
/// tree-walking evaluator only, never on a compiled backend, and invisible
/// to static type inference (any call site infers `ExpressionTypeUnknown`).
///
/// This constant pins the *currently known* drift. The reverse-drift test
/// computes (registered free fns) − (analyzer-known names) and asserts it
/// equals exactly this set, so adding a new free fn without an analyzer
/// signature — or shipping a signature for one of these — fails the test
/// and forces a deliberate decision.
const TIER2_TREEWALK_ONLY_DRIFT: &[&str] = &[
    // -- string: case-fold / normalization / glob (underscore intrinsics
    //    plus the bare `glob_match` surface) --
    "_string_title",
    "_string_upper_locale",
    "_string_lower_locale",
    "_string_title_locale",
    "_string_nfc",
    "_string_nfd",
    "_string_nfkc",
    "_string_nfkd",
    "_string_glob_match",
    "glob_match",
    // -- math: bare-name aliases + the JSON-Schema numeric wave --
    "abs",
    "max",
    "min",
    "clamp",
    "round",
    "floor",
    "ceil",
    "sqrt",
    "pow",
    "in_range",
    "multiple_of",
    // -- string predicates / transforms (JSON-Schema parity wave) --
    "matches",
    "starts_with",
    "ends_with",
    "is_email",
    "is_uri",
    "is_uuid",
    "is_iso_date",
    "is_ipv4",
    "is_ipv6",
    "trim",
    "trim_start",
    "trim_end",
    // -- list helpers (JSON-Schema parity wave) --
    "unique",
    "count",
    "every",
    "some",
    // -- dict helpers + json + date --
    "select_keys",
    "omit_keys",
    "size_in_range",
    "parse_iso_date",
    "to_json",
];

/// Scan `stdlib.rs` for every `register_pure_fn("<name>", ...)` call and
/// return the set of free-fn names actually registered by the evaluator.
///
/// We parse the source text rather than spinning up a `Context` and
/// reading back its registered-fn table because the registry is a
/// `pub(crate)`-internal detail of `relon-eval-api`; a source scan keeps
/// the drift check anchored to the literal register sites the maintainer
/// edits, which is exactly what we want to defend.
///
/// Note: `register_pure_method(Type, name, ...)` registrations are NOT
/// included — methods dispatch through the receiver's `native_methods`
/// table and are never looked up in the analyzer's free-fn
/// `stdlib_signatures`, so they are out of scope for this signature-drift
/// check.
fn registered_free_fn_names() -> BTreeSet<String> {
    const SRC: &str = include_str!("stdlib.rs");
    const MARKER: &str = "register_pure_fn(\"";
    let mut names = BTreeSet::new();
    let mut rest = SRC;
    while let Some(pos) = rest.find(MARKER) {
        rest = &rest[pos + MARKER.len()..];
        if let Some(end) = rest.find('"') {
            names.insert(rest[..end].to_string());
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    names
}

/// Reverse drift defense, driven from the evaluator's real register sites.
///
/// Asserts that the set of `register_pure_fn` names lacking an analyzer
/// signature equals exactly [`TIER2_TREEWALK_ONLY_DRIFT`]. Fails loudly
/// when a maintainer:
///   * adds a NEW free fn without an analyzer signature (it lands in the
///     computed drift but not the pinned allowlist), or
///   * finally gives one of the drift fns a signature (it leaves the
///     computed drift but lingers in the pinned allowlist), or
///   * removes / renames a drift fn.
#[test]
fn reverse_stdlib_drift_is_pinned() {
    let registered = registered_free_fn_names();
    assert!(
        !registered.is_empty(),
        "failed to scan any register_pure_fn names out of stdlib.rs"
    );

    // Sanity: the analyzer-covered names the curated allowlist already
    // guards must really be registered (guards against the scan silently
    // missing the early register block).
    for known in ["len", "range", "type", "ensure.int", "_math_abs"] {
        assert!(
            registered.contains(known),
            "expected `{known}` among scanned register_pure_fn names; scan is broken"
        );
    }

    let analyzer_known: BTreeSet<String> = relon_analyzer::stdlib_signatures::stdlib_fn_names()
        .map(|s| s.to_string())
        .collect();

    let actual_drift: BTreeSet<String> = registered.difference(&analyzer_known).cloned().collect();
    let pinned_drift: BTreeSet<String> = TIER2_TREEWALK_ONLY_DRIFT
        .iter()
        .map(|s| s.to_string())
        .collect();

    // Allowlist hygiene: no stale entries that aren't even registered.
    let stale: Vec<&String> = pinned_drift.difference(&registered).collect();
    assert!(
        stale.is_empty(),
        "TIER2_TREEWALK_ONLY_DRIFT lists names that are not registered as free fns: {stale:?}"
    );

    let newly_drifted: Vec<&String> = actual_drift.difference(&pinned_drift).collect();
    let now_covered: Vec<&String> = pinned_drift.difference(&actual_drift).collect();
    assert!(
        newly_drifted.is_empty() && now_covered.is_empty(),
        "reverse stdlib drift changed.\n  NEW drift (registered free fn with no analyzer \
         signature, not yet in the tier-2 allowlist): {newly_drifted:?}\n  RESOLVED (now has a \
         signature — drop from TIER2_TREEWALK_ONLY_DRIFT): {now_covered:?}"
    );
}

/// Run a relon source string through the tree-walking evaluator,
/// bypassing the analyzer's strict static-type gate. This is the only
/// execution path the tier-2 tree-walker-only stdlib fns can take.
fn tree_walk(source: &str) -> Value {
    tree_walk_result(source).expect("fixture must evaluate")
}

fn tree_walk_result(source: &str) -> Result<Value, RuntimeError> {
    let node = relon_parser::parse_document(source).expect("fixture must parse");
    let mut ctx = Context::new().with_root(node);
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let ctx = Arc::new(ctx);
    TreeWalkEvaluator::new(Arc::clone(&ctx)).eval_root(&Arc::new(Scope::default()))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root above crates/relon-evaluator")
}

/// Execution coverage for a representative slice of the tier-2
/// tree-walker-only stdlib (sqrt / pow / round / floor / ceil / in_range /
/// trim / unique / count / to_json). Drives the fixture through the
/// tree-walker — NOT the cross-backend golden harness, which the analyzer
/// would reject — and compares the serialized result to the golden JSON.
#[test]
fn tier2_treewalk_only_fixture_executes() {
    let root = workspace_root();
    let fixture = root.join("fixtures/golden/tier2_treewalk/stdlib_treewalk_only.relon");
    let golden = root.join("fixtures/golden/tier2_treewalk/stdlib_treewalk_only.json");

    let source = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|e| panic!("read {}: {e}", fixture.display()));
    let value = tree_walk(&source);
    let actual: serde_json::Value =
        serde_json::to_value(&value).expect("tree-walk result serializes to JSON");

    let expected_raw = std::fs::read_to_string(&golden)
        .unwrap_or_else(|e| panic!("read {}: {e}", golden.display()));
    let expected: serde_json::Value =
        serde_json::from_str(&expected_raw).expect("golden JSON parses");

    assert_eq!(
        actual,
        expected,
        "tier-2 tree-walker-only golden mismatch.\n  actual: {}",
        serde_json::to_string_pretty(&actual).unwrap()
    );
}

/// Boundary-semantics goldens for `min` / `max` / `clamp` / `abs` /
/// `_list_contains`, pinned byte-for-byte ahead of the stdlib
/// twin-retirement (native Rust struct vs `std_relon/*.relon` wrapper
/// collapsing to a single relon-sourced implementation).
///
/// Every value below was captured from the tree-walker BEFORE the
/// retirement; the retirement must keep each one identical. The cases
/// deliberately cover the spots where Rust float intrinsics and relon
/// branch semantics could diverge:
///   * NaN through min / max / clamp (branch semantics: NaN compares
///     false, so the *other* / fall-through operand wins — NOT the
///     IEEE `fmin` / `fmax` NaN-suppression rule).
///   * signed zero: `-0.0 < 0.0` is false, so min / max return whichever
///     argument sits on the fall-through side, preserving its sign bit.
///   * mixed Int / Float arguments return the ORIGINAL value (no
///     numeric coercion of the winner).
///   * `_list_contains` equality is `Value::eq` (OrderedFloat: NaN ==
///     NaN is true, -0.0 == 0.0 is true) over an empty / hit / miss /
///     cross-type matrix.
fn assert_pinned(src: &str, expected_debug: &str) {
    let value = tree_walk(src);
    assert_eq!(
        format!("{value:?}"),
        expected_debug,
        "boundary golden drifted for fixture: {src}"
    );
}

/// Same pin, but the fixture needs a std module import; the expression
/// is wrapped as `{{ "r": <expr> }}` and the golden compares the `r`
/// member.
fn assert_pinned_with_import(module: &str, src: &str, expected_debug: &str) {
    let doc = format!("#import {module} from \"std/{module}\"\n{{ \"r\": {src} }}");
    let value = tree_walk(&doc);
    let Value::Dict(dict) = &value else {
        panic!("module fixture must evaluate to a dict: {src}");
    };
    let r = dict.map.get("r").expect("fixture dict has `r`");
    assert_eq!(
        format!("{r:?}"),
        expected_debug,
        "boundary golden drifted for module fixture: {src}"
    );
}

#[test]
fn math_min_max_boundary_goldens_are_pinned() {
    // Int / mixed / tie.
    assert_pinned("min(1, 2)", "Int(1)");
    assert_pinned("min(2, 1)", "Int(1)");
    assert_pinned("min(7, 7)", "Int(7)");
    assert_pinned("max(1, 2)", "Int(2)");
    assert_pinned("max(2, 1)", "Int(2)");
    assert_pinned("max(7, 7)", "Int(7)");
    assert_pinned("min(1, 2.0)", "Int(1)");
    assert_pinned("min(3, 2.0)", "Float(2.0)");
    assert_pinned("max(1, 2.0)", "Float(2.0)");
    // Signed zero: comparison is false for equal magnitudes, so the
    // second argument falls through with its sign bit intact.
    assert_pinned("min(-0.0, 0.0)", "Float(0.0)");
    assert_pinned("min(0.0, -0.0)", "Float(-0.0)");
    assert_pinned("max(-0.0, 0.0)", "Float(0.0)");
    assert_pinned("max(0.0, -0.0)", "Float(-0.0)");
    // NaN: `NaN < x` / `NaN > x` are false, so the second operand wins
    // when NaN is first, and NaN itself falls through when second.
    assert_pinned("min(sqrt(-1.0), 1.0)", "Float(1.0)");
    assert_pinned("min(1.0, sqrt(-1.0))", "Float(NaN)");
    assert_pinned("max(sqrt(-1.0), 1.0)", "Float(1.0)");
    assert_pinned("max(1.0, sqrt(-1.0))", "Float(NaN)");
    // Module-path form must agree with the bare form.
    assert_pinned_with_import("math", "math.min(sqrt(-1.0), 1.0)", "Float(1.0)");
    assert_pinned_with_import("math", "math.min(-0.0, 0.0)", "Float(0.0)");
    assert_pinned_with_import("math", "math.max(1.0, sqrt(-1.0))", "Float(NaN)");
}

#[test]
fn math_clamp_boundary_goldens_are_pinned() {
    assert_pinned("clamp(5, 0, 10)", "Int(5)");
    assert_pinned("clamp(-1, 0, 10)", "Int(0)");
    assert_pinned("clamp(11, 0, 10)", "Int(10)");
    // Inverted bounds: `v < lo` is checked first, so lo wins.
    assert_pinned("clamp(5, 10, 0)", "Int(10)");
    assert_pinned("clamp(5, 0.0, 10)", "Int(5)");
    // Signed zero / NaN fall-through (NaN comparisons are all false,
    // so a NaN value passes through unclamped).
    assert_pinned("clamp(-0.0, 0.0, 1.0)", "Float(-0.0)");
    assert_pinned("clamp(0.0, -0.0, 1.0)", "Float(0.0)");
    assert_pinned("clamp(sqrt(-1.0), 0.0, 1.0)", "Float(NaN)");
    assert_pinned("clamp(0.5, sqrt(-1.0), 1.0)", "Float(0.5)");
    assert_pinned("clamp(2.0, 0.0, sqrt(-1.0))", "Float(2.0)");
    assert_pinned_with_import("math", "math.clamp(sqrt(-1.0), 0.0, 1.0)", "Float(NaN)");
    assert_pinned_with_import("math", "math.clamp(5, 10, 0)", "Int(10)");
}

#[test]
fn math_abs_boundary_goldens_are_pinned() {
    assert_pinned("abs(5)", "Int(5)");
    assert_pinned("abs(-5)", "Int(5)");
    assert_pinned("abs(0)", "Int(0)");
    // Float abs clears the sign bit (f64::abs), including -0.0 -> 0.0.
    assert_pinned("abs(-0.0)", "Float(0.0)");
    assert_pinned("abs(-5.5)", "Float(5.5)");
    assert_pinned("abs(sqrt(-1.0))", "Float(NaN)");
    assert_pinned_with_import("math", "math.abs(-0.0)", "Float(0.0)");
    assert_pinned_with_import("math", "math.abs(-7)", "Int(7)");
}

#[test]
fn list_contains_boundary_goldens_are_pinned() {
    assert_pinned("_list_contains([], 1)", "Bool(false)");
    assert_pinned("_list_contains([1, 2, 3], 1)", "Bool(true)");
    assert_pinned("_list_contains([1, 2, 3], 3)", "Bool(true)");
    assert_pinned("_list_contains([1, 2, 3], 4)", "Bool(false)");
    assert_pinned("_list_contains([\"a\", \"b\"], \"a\")", "Bool(true)");
    // Cross-type: Int vs Float never compare equal under `Value::eq`.
    assert_pinned("_list_contains([1], 1.0)", "Bool(false)");
    assert_pinned("_list_contains([1.0], 1)", "Bool(false)");
    assert_pinned("_list_contains([[1, 2], [3]], [1, 2])", "Bool(true)");
    assert_pinned("_list_contains([true], true)", "Bool(true)");
    // OrderedFloat equality: -0.0 == 0.0, NaN == NaN.
    assert_pinned("_list_contains([-0.0], 0.0)", "Bool(true)");
    assert_pinned("_list_contains([sqrt(-1.0)], sqrt(-1.0))", "Bool(true)");
    assert_pinned_with_import("list", "list.contains([], 1)", "Bool(false)");
    assert_pinned_with_import("list", "list.contains([1, 2], 2)", "Bool(true)");
    assert_pinned_with_import("list", "list.contains([0.0], -0.0)", "Bool(true)");
}

/// Single-source guard for the retired stdlib twins.
///
/// `min` / `max` / `clamp` / Int-`abs` / `contains` used to exist twice:
/// a native Rust `RelonFunction` *and* a delegating wrapper in
/// `std_relon/*.relon`. After the retirement the `.relon` text is the
/// only implementation (the registered names dispatch to it through
/// `RelonSourcedFn`). This test fails if either side regresses:
///   * the `.relon` wrapper goes back to delegating to a retired
///     underscore intrinsic (twin reborn), or
///   * `stdlib.rs` re-grows a native body for one of the retired names.
#[test]
fn retired_stdlib_twins_have_single_source() {
    // Match the *call* form `_math_min(` so prose comments may still
    // name the retired intrinsics.
    let math = include_str!("std_relon/math.relon");
    for retired_delegate in ["_math_min(", "_math_max(", "_math_clamp("] {
        assert!(
            !math.contains(retired_delegate),
            "std_relon/math.relon must implement min/max/clamp itself, \
             not delegate to retired native `{retired_delegate}...)`"
        );
    }
    // `abs` keeps exactly one `_math_abs(` call: the Float branch of
    // its type dispatch (the native is Float-only `f64::abs`).
    assert_eq!(
        math.matches("_math_abs(").count(),
        1,
        "std_relon/math.relon `abs` delegates to `_math_abs` only for \
         the Float branch"
    );

    let list = include_str!("std_relon/list.relon");
    assert!(
        !list.contains("_list_contains("),
        "std_relon/list.relon must implement `contains` itself (fold \
         over _list_reduce), not delegate to a retired native"
    );

    let stdlib = include_str!("stdlib.rs");
    for retired_native in [
        "struct MathMin",
        "struct MathMax",
        "struct MathClamp",
        "struct MathAbs;",
        "struct ListContains",
    ] {
        assert!(
            !stdlib.contains(retired_native),
            "stdlib.rs re-grew retired native `{retired_native}` — the \
             std_relon/*.relon implementation is the single source of truth"
        );
    }
}

/// Post-retirement semantics that intentionally CHANGED (degenerate
/// inputs only — every well-typed result is pinned byte-for-byte by the
/// `*_boundary_goldens_are_pinned` tests above):
///   * `abs(i64::MIN)`: the retired native's `i64::abs` panicked in
///     debug builds / wrapped in release; the relon `-x` is the
///     evaluator's *checked* negation, so it now traps cleanly.
///   * non-numeric `min` / `max` / `clamp` input: the retired natives'
///     `to_f64_val` coerced non-numbers to `0.0` and returned a garbage
///     operand (e.g. `min("a", "b")` -> `"b"`); the relon ternaries
///     compare with the language `<` / `>`, which rejects non-numbers.
#[test]
fn retired_twin_degenerate_inputs_now_trap() {
    let err = tree_walk_result("abs(-9223372036854775807 - 1)")
        .expect_err("abs(i64::MIN) must trap as NumericOverflow");
    assert!(
        matches!(&err, RuntimeError::NumericOverflow(_)),
        "expected NumericOverflow, got {err:?}"
    );

    for src in [
        "min(\"a\", \"b\")",
        "max(\"a\", \"b\")",
        "clamp(\"m\", \"a\", \"z\")",
    ] {
        let err = tree_walk_result(src).expect_err("non-numeric comparison must trap");
        assert!(
            matches!(&err, RuntimeError::TypeMismatch { .. }),
            "expected TypeMismatch for `{src}`, got {err:?}"
        );
    }
}

#[test]
fn from_json_is_not_registered() {
    let err = tree_walk_result(r#"from_json("[1,2]")"#)
        .expect_err("from_json must not be available as a stdlib function");
    assert!(
        matches!(&err, RuntimeError::FunctionNotFound(name, _) if name == "from_json"),
        "expected FunctionNotFound(from_json), got {err:?}"
    );
}
