//! Differential test corpus — `(source, args, expected)` tuples
//! driving the harness across both backends.
//!
//! Cases are grouped by tier so we can label which ones are
//! expected to pass `MatchOk` once cranelift widens to the next
//! tranche. Each entry's `tier` field is informational today
//! (the harness reports `CraneliftUnsupported` rather than
//! failing) but lets a future strict-mode runner enforce
//! "tier T must Match" once the lowering catches up.
//!
//! ## Support claims & ratchet (review-improvement-180)
//!
//! Each [`CorpusCase`] carries an explicit `supported_by` list — the
//! backends that **claim** to handle the source today. The drivers
//! ([`crate::diff_test`] / [`crate::three_way::diff_test_3way`])
//! treat a backend's
//! `Unsupported` / `NotApplicable` surface as a soft pass **only**
//! when the backend is not in `supported_by`. If a backend in the
//! claim list bounces, the corpus harness tests fail loud — that's
//! the ratchet that stops a backend from silently regressing into
//! its fallback after it had taken responsibility for a case.
//!
//! Today's support matrix:
//!
//! | tier | tree-walk | cranelift-AOT | trace-JIT | bytecode VM |
//! | ---- | --------- | ------------- | --------- | ----------- |
//! | ArithControl (28) | 28 | 27 (`let_chain` analyzer-only) | 22 | 27 |
//! | StdlibSimple (9) | 9 | 9 | 9 | 9 |
//! | StdlibMemory (6) | 6 | 6 | 4 (`StrConcatN` shapes not in recipe) | 0 (`String` return shape) |
//! | StdlibCaseFold (8) | 8 | 8 | 8 | 0 (`String` return shape) |
//! | StdlibList (5) | 5 | 5 | 2 (`*_range_*` shapes not in recipe) | 3 (`*_range_*` only) |
//! | StdlibNormalize (2) | 2 | 2 | 2 | 0 |
//! | DictReturn (2) | 2 | 2 | 0 | 1 (`dict_simple_return` only) |
//!
//! When a backend widens its envelope, the corpus entry's
//! `supported_by` list grows; the ratchet then locks in the new
//! coverage so a later refactor can't quietly un-widen.

use std::collections::HashMap;

use ordered_float::OrderedFloat;
use relon_eval_api::Value;

use crate::BackendKind;

/// One differential test case.
#[derive(Debug, Clone)]
pub struct CorpusCase {
    /// Short identifier used in test failure messages.
    pub name: &'static str,
    /// Relon source. Currently expected to be `#main(...) -> ...`
    /// shaped for `run_main` dispatch; library-mode cases live in
    /// a separate corpus group once the harness grows
    /// `eval_root`-driven differential testing.
    pub source: &'static str,
    /// `#main` argument map. Sticking to `Int` / `Bool` / `String`
    /// today; floats / lists / dicts arrive in tranche 3+.
    pub args_factory: fn() -> HashMap<String, Value>,
    /// Lowering tier that this case is expected to enter `MatchOk`
    /// at. Today the cranelift backend only handles `ArithControl`;
    /// `Stdlib*` / `Dict*` cases gracefully surface
    /// `CraneliftUnsupported`.
    pub tier: Tier,
    /// Backends that claim to handle this source today. If a backend
    /// in this list returns its `Unsupported` / `NotApplicable`
    /// fallback the corpus harness fails — see module-level docs.
    pub supported_by: &'static [BackendKind],
}

// ---- Support-claim tables ----------------------------------------------------
//
// Per-backend lists are recomputed by walking the live harness once
// (see `docs/internal/review-improvement-180-harness-ratchet-…`).
// Tree-walk is the reference impl; the only cases excluded from
// tree-walk's claim are ones the AST evaluator legitimately reports
// `FunctionNotFound` on (none today — the `TreeWalkMissingStdlibSurface`
// soft pass exists for forward-compat).
//
// Each constant matches the **current** observed behaviour. The
// ratchet asserts the harness does not regress *below* this list. Add
// a case here when a backend's envelope widens.

/// Claim list: every backend supports every case in its tier. Used
/// for cases where all 4 backends agree.
const FULL_SUPPORT: &[BackendKind] = &[
    BackendKind::TreeWalk,
    BackendKind::CraneliftAot,
    BackendKind::TraceJit,
    BackendKind::Bytecode,
];

/// Reference + cranelift + bytecode; trace-JIT recipe absent.
const TW_CR_BC: &[BackendKind] = &[
    BackendKind::TreeWalk,
    BackendKind::CraneliftAot,
    BackendKind::Bytecode,
];

/// Reference + cranelift + trace-JIT; bytecode envelope excludes it.
const TW_CR_TJ: &[BackendKind] = &[
    BackendKind::TreeWalk,
    BackendKind::CraneliftAot,
    BackendKind::TraceJit,
];

/// Reference + cranelift only. Trace-JIT not in recipe catalogue and
/// bytecode envelope rejects the entry shape.
const TW_CR: &[BackendKind] = &[BackendKind::TreeWalk, BackendKind::CraneliftAot];

/// Reference only — analyzer rejects on the IR / cranelift side, so
/// neither cranelift nor any backend downstream of it can claim
/// support.
const TW_ONLY: &[BackendKind] = &[BackendKind::TreeWalk];

/// Lowering tier the case is expected to pass on cranelift-AOT.
/// Each tier corresponds to one stdlib / IR-coverage milestone in
/// the v5-β-2 plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Arithmetic + comparison + simple control flow (β-1 coverage).
    ArithControl,
    /// Simple stdlib: `length` / `is_empty` / `abs` / `min` / `max` /
    /// `list_*_length`.
    StdlibSimple,
    /// Memory stdlib: `concat` / `substring` / `starts_with`.
    StdlibMemory,
    /// Case folding: `upper` / `lower` / `title` (+ locale variants).
    StdlibCaseFold,
    /// List higher-order: `list_int_map` / `filter` / `fold`.
    StdlibList,
    /// Unicode normalization: `nfd` / `nfkd` / `nfc` / `nfkc`.
    StdlibNormalize,
    /// Dict construction + schema-rooted return.
    DictReturn,
    /// Closure / first-class lambda.
    Closure,
}

/// Factory helpers — `args_factory` is a fn-pointer so the corpus
/// stays `Copy` / static-friendly (a `HashMap` is not).
fn no_args() -> HashMap<String, Value> {
    HashMap::new()
}

fn one_int(name: &str, v: i64) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert(name.to_string(), Value::Int(v));
    m
}

fn two_ints(an: &str, av: i64, bn: &str, bv: i64) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert(an.to_string(), Value::Int(av));
    m.insert(bn.to_string(), Value::Int(bv));
    m
}

fn args_n_5() -> HashMap<String, Value> {
    one_int("n", 5)
}

fn args_n_42() -> HashMap<String, Value> {
    one_int("n", 42)
}

fn args_s_world() -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert("s".to_string(), Value::String("world".into()));
    m
}

// ---- ArithControl ----

fn args_x_40_y_2() -> HashMap<String, Value> {
    two_ints("x", 40, "y", 2)
}
fn args_x_7_y_6() -> HashMap<String, Value> {
    two_ints("x", 7, "y", 6)
}
fn args_x_neg3_y_10() -> HashMap<String, Value> {
    two_ints("x", -3, "y", 10)
}
fn args_x_17_y_5() -> HashMap<String, Value> {
    two_ints("x", 17, "y", 5)
}
fn args_x_5_y_0() -> HashMap<String, Value> {
    two_ints("x", 5, "y", 0)
}
fn args_x_neg42() -> HashMap<String, Value> {
    one_int("x", -42)
}
fn args_x_42() -> HashMap<String, Value> {
    one_int("x", 42)
}
fn args_x_max() -> HashMap<String, Value> {
    one_int("x", i64::MAX)
}
fn args_x_min() -> HashMap<String, Value> {
    one_int("x", i64::MIN)
}
fn args_x_3_y_minus_1() -> HashMap<String, Value> {
    two_ints("x", 3, "y", -1)
}
fn args_x_5_y_5() -> HashMap<String, Value> {
    two_ints("x", 5, "y", 5)
}
fn args_x_neg10_y_neg5() -> HashMap<String, Value> {
    two_ints("x", -10, "y", -5)
}

/// All corpus cases. Layered by tier; the harness runs them all and
/// reports per-tier pass / supported / unsupported counts.
pub fn all_cases() -> Vec<CorpusCase> {
    vec![
        // ---- ArithControl: 25 cases that the v5-β-1 cranelift envelope handles ----
        CorpusCase {
            name: "arith_add",
            source: "#main(Int x, Int y) -> Int\nx + y",
            args_factory: args_x_40_y_2,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "arith_sub",
            source: "#main(Int x, Int y) -> Int\nx - y",
            args_factory: args_x_neg3_y_10,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "arith_mul",
            source: "#main(Int x, Int y) -> Int\nx * y",
            args_factory: args_x_7_y_6,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "arith_div",
            source: "#main(Int x, Int y) -> Int\nx / y",
            args_factory: args_x_17_y_5,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "arith_mod",
            source: "#main(Int x, Int y) -> Int\nx % y",
            args_factory: args_x_17_y_5,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "arith_div_negative",
            source: "#main(Int x, Int y) -> Int\nx / y",
            args_factory: args_x_neg10_y_neg5,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "arith_chain",
            source: "#main(Int x, Int y) -> Int\nx * y + x",
            args_factory: args_x_3_y_minus_1,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "arith_paren",
            source: "#main(Int x, Int y) -> Int\n(x + y) * x",
            args_factory: args_x_5_y_5,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "arith_negate_via_sub",
            source: "#main(Int x) -> Int\n0 - x",
            args_factory: args_x_42,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        // Trap cases: trace-JIT recorder doesn't model the trap path
        // (synth recipe returns a wrapping value); tree-walk +
        // cranelift + bytecode all raise the same `DivisionByZero`.
        CorpusCase {
            name: "arith_div_by_zero_traps",
            source: "#main(Int x, Int y) -> Int\nx / y",
            args_factory: args_x_5_y_0,
            tier: Tier::ArithControl,
            supported_by: TW_CR_BC,
        },
        CorpusCase {
            name: "arith_mod_by_zero_traps",
            source: "#main(Int x, Int y) -> Int\nx % y",
            args_factory: args_x_5_y_0,
            tier: Tier::ArithControl,
            supported_by: TW_CR_BC,
        },
        // ---- cmp ----
        CorpusCase {
            name: "cmp_eq_true",
            source: "#main(Int x, Int y) -> Bool\nx == y",
            args_factory: args_x_5_y_5,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "cmp_eq_false",
            source: "#main(Int x, Int y) -> Bool\nx == y",
            args_factory: args_x_5_y_0,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "cmp_ne",
            source: "#main(Int x, Int y) -> Bool\nx != y",
            args_factory: args_x_5_y_5,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "cmp_lt",
            source: "#main(Int x, Int y) -> Bool\nx < y",
            args_factory: args_x_3_y_minus_1,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "cmp_le_eq",
            source: "#main(Int x, Int y) -> Bool\nx <= y",
            args_factory: args_x_5_y_5,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "cmp_gt",
            source: "#main(Int x, Int y) -> Bool\nx > y",
            args_factory: args_x_17_y_5,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "cmp_ge_eq",
            source: "#main(Int x, Int y) -> Bool\nx >= y",
            args_factory: args_x_5_y_5,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        // ---- control flow ----
        CorpusCase {
            name: "if_true_arm",
            source: "#main(Int x, Int y) -> Int\nx > y ? x : y",
            args_factory: args_x_17_y_5,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "if_false_arm",
            source: "#main(Int x, Int y) -> Int\nx > y ? x : y",
            args_factory: args_x_neg3_y_10,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "if_nested",
            source:
                "#main(Int x) -> Int\nx > 0 ? (x > 10 ? x * 2 : x + 1) : (0 - x)",
            args_factory: args_x_42,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "if_nested_neg",
            source:
                "#main(Int x) -> Int\nx > 0 ? (x > 10 ? x * 2 : x + 1) : (0 - x)",
            args_factory: args_x_neg42,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        // ---- let-binding (Relon uses `where { name: value }` postfix) ----
        CorpusCase {
            name: "let_then_add",
            source: "#main(Int x) -> Int\n(y + 1) where { y: x * 2 }",
            args_factory: args_x_42,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        // `let_chain` (forward-references inside `where`) — the
        // analyzer rejects this source before IR lowering, so neither
        // cranelift nor any downstream backend ever sees the body.
        // Tree-walker is the only backend that handles it.
        CorpusCase {
            name: "let_chain",
            source: "#main(Int x) -> Int\nc where { a: x + 1, b: a * 2, c: b - 3 }",
            args_factory: args_x_42,
            tier: Tier::ArithControl,
            supported_by: TW_ONLY,
        },
        CorpusCase {
            name: "let_uses_cond",
            source: "#main(Int x) -> Int\n(y * 2) where { y: x > 0 ? x : 0 - x }",
            args_factory: args_x_neg42,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        // ---- boundary values ----
        // Overflow boundary cases: trace synth returns a wrapping
        // value; tw + cr + bytecode all trap on `NumericOverflow`.
        CorpusCase {
            name: "boundary_max_plus_one",
            source: "#main(Int x) -> Int\nx + 1",
            args_factory: args_x_max,
            tier: Tier::ArithControl,
            supported_by: TW_CR_BC,
        },
        CorpusCase {
            name: "boundary_min_minus_one",
            source: "#main(Int x) -> Int\nx - 1",
            args_factory: args_x_min,
            tier: Tier::ArithControl,
            supported_by: TW_CR_BC,
        },
        CorpusCase {
            name: "boundary_zero_times_x",
            source: "#main(Int x) -> Int\n0 * x",
            args_factory: args_x_max,
            tier: Tier::ArithControl,
            supported_by: FULL_SUPPORT,
        },
        // ---- StdlibSimple: all 4 backends agree today ----
        CorpusCase {
            name: "stdlib_abs_pos",
            source: "#main(Int x) -> Int\nabs(x)",
            args_factory: args_x_42,
            tier: Tier::StdlibSimple,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "stdlib_abs_neg",
            source: "#main(Int x) -> Int\nabs(x)",
            args_factory: args_x_neg42,
            tier: Tier::StdlibSimple,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "stdlib_min",
            source: "#main(Int x, Int y) -> Int\nmin(x, y)",
            args_factory: args_x_17_y_5,
            tier: Tier::StdlibSimple,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "stdlib_max",
            source: "#main(Int x, Int y) -> Int\nmax(x, y)",
            args_factory: args_x_17_y_5,
            tier: Tier::StdlibSimple,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "stdlib_min_neg",
            source: "#main(Int x, Int y) -> Int\nmin(x, y)",
            args_factory: args_x_neg10_y_neg5,
            tier: Tier::StdlibSimple,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "stdlib_length_const",
            source: "#main() -> Int\n\"hello\".length()",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "stdlib_is_empty_false",
            source: "#main() -> Bool\n\"hi\".is_empty()",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "stdlib_is_empty_true",
            source: "#main() -> Bool\n\"\".is_empty()",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: FULL_SUPPORT,
        },
        CorpusCase {
            name: "stdlib_list_int_length",
            source: "#main() -> Int\n[1, 2, 3, 4, 5].length()",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: FULL_SUPPORT,
        },
        // ---- StdlibMemory: needs scratch arena ----
        // The bytecode VM scaffold today rejects `String`-typed return
        // fields, so every entry here excludes `Bytecode` from
        // `supported_by`. The trace-JIT recipe catalogue handles the
        // const-table forms but not the `+`-chain shapes (which need
        // `Op::StrConcatN` synth coverage).
        CorpusCase {
            name: "stdlib_concat_const",
            source: "#main() -> String\n\"foo\".concat(\"bar\")",
            args_factory: no_args,
            tier: Tier::StdlibMemory,
            supported_by: TW_CR_TJ,
        },
        // #165 — left-leaning 4-leaf String concat chain folds to one
        // `Op::StrConcatN { 4 }` in IR; the four-way harness validates
        // tree-walker / bytecode / cranelift / trace-JIT all agree on
        // the joined payload (single-alloc shape for each).
        CorpusCase {
            name: "str_concat_chain_four_way",
            source: "#main() -> String\n\"foo\" + \"bar\" + \"baz\" + \"qux\"",
            args_factory: no_args,
            tier: Tier::StdlibMemory,
            supported_by: TW_CR,
        },
        // #165 — three-leaf chain hits the minimal `StrConcatN { 3 }`
        // shape the fold gate accepts (outer Add + lhs itself a
        // Binary(Add)).
        CorpusCase {
            name: "str_concat_chain_three_way",
            source: "#main() -> String\n\"foo\" + \"bar\" + \"baz\"",
            args_factory: no_args,
            tier: Tier::StdlibMemory,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "stdlib_substring",
            source: "#main() -> String\n\"hello\".substring(1, 3)",
            args_factory: no_args,
            tier: Tier::StdlibMemory,
            supported_by: TW_CR_TJ,
        },
        CorpusCase {
            name: "stdlib_starts_with_true",
            source: "#main() -> Bool\n\"hello world\".starts_with(\"hello\")",
            args_factory: no_args,
            tier: Tier::StdlibMemory,
            supported_by: TW_CR_TJ,
        },
        CorpusCase {
            name: "stdlib_starts_with_false",
            source: "#main() -> Bool\n\"hello world\".starts_with(\"world\")",
            args_factory: no_args,
            tier: Tier::StdlibMemory,
            supported_by: TW_CR_TJ,
        },
        // Wave R2 — f-string lowering. A pure static desugar: literal
        // parts become `Op::ConstString`, interpolations are coerced to
        // `String` (identity for a String, `Op::IntToStr` for an Int),
        // and the parts join via `Op::StrConcatN`. These return `String`
        // (bytecode rejects the shape) and are not in the trace recipe
        // catalogue, so both claim `TW_CR`. Byte-exactness with the
        // tree-walker's `Display` coercion is the differential guard.
        CorpusCase {
            name: "fstring_string_interp",
            source: "#main(String s) -> String\nf\"hi ${s}!\"",
            args_factory: args_s_world,
            tier: Tier::StdlibMemory,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "fstring_int_interp",
            source: "#main(Int n) -> String\nf\"n=${n}\"",
            args_factory: args_n_42,
            tier: Tier::StdlibMemory,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "fstring_int_interp_negative",
            source: "#main(Int n) -> String\nf\"v=${n}.\"",
            args_factory: || one_int("n", -7),
            tier: Tier::StdlibMemory,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "fstring_mixed_parts",
            source: "#main(Int n) -> String\nf\"a${n}b${n}c\"",
            args_factory: args_n_5,
            tier: Tier::StdlibMemory,
            supported_by: TW_CR,
        },
        // ---- StdlibCaseFold ----
        // All case-fold entries return `String`, which the bytecode
        // M2-A scaffold rejects; cranelift + tree-walk + the trace
        // const-table all agree.
        CorpusCase {
            name: "stdlib_upper_ascii",
            source: "#main() -> String\n\"hello\".upper()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
            supported_by: TW_CR_TJ,
        },
        CorpusCase {
            name: "stdlib_lower_ascii",
            source: "#main() -> String\n\"WORLD\".lower()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
            supported_by: TW_CR_TJ,
        },
        CorpusCase {
            name: "stdlib_title_ascii",
            source: "#main() -> String\n\"hello world\".title()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
            supported_by: TW_CR_TJ,
        },
        CorpusCase {
            name: "stdlib_upper_unicode_greek",
            source: "#main() -> String\n\"σίγμα\".upper()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
            supported_by: TW_CR_TJ,
        },
        CorpusCase {
            name: "stdlib_lower_final_sigma",
            source: "#main() -> String\n\"ΣΙΓΜΑ\".lower()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
            supported_by: TW_CR_TJ,
        },
        // v3++ b-7 reframed: FULL multi-cp + Σ-context coverage.
        CorpusCase {
            name: "stdlib_upper_sharp_s",
            source: "#main() -> String\n\"straße\".upper()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
            supported_by: TW_CR_TJ,
        },
        CorpusCase {
            name: "stdlib_lower_final_sigma_at_end",
            source: "#main() -> String\n\"ΟΔΥΣΣΕΥΣ\".lower()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
            supported_by: TW_CR_TJ,
        },
        CorpusCase {
            name: "stdlib_upper_ligature_fi",
            source: "#main() -> String\n\"ﬁle\".upper()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
            supported_by: TW_CR_TJ,
        },
        // ---- StdlibList: higher-order needs closure ABI ----
        // The two const-list cases are covered by the trace const
        // table; bytecode rejects the list-literal entry shape.
        CorpusCase {
            name: "stdlib_list_sum",
            source: "#main() -> Int\n[1, 2, 3, 4, 5].sum()",
            args_factory: no_args,
            tier: Tier::StdlibList,
            supported_by: TW_CR_TJ,
        },
        CorpusCase {
            name: "stdlib_list_max",
            source: "#main() -> Int\n[3, 1, 4, 1, 5, 9, 2, 6].max()",
            args_factory: no_args,
            tier: Tier::StdlibList,
            supported_by: TW_CR_TJ,
        },
        // review-improvement-160 bytecode M3 phase 2: the IR-level
        // `list.sum(range(...))` peephole desugar.  The tree-walker
        // resolves the call through the dynamic `std/list` module
        // loader; the bytecode + cranelift backends recognise the
        // pattern in `lower_fn_call` and emit an explicit `Op::Loop`
        // accumulator.  Differential agreement here is the regression
        // guard — without the peephole, the IR side surfaces
        // `UnresolvedVariable("list")` and fails the diff. The
        // `range`-based shapes are *not* in the trace recipe
        // catalogue, so `TraceJit` is intentionally excluded.
        CorpusCase {
            name: "stdlib_list_sum_range_n",
            source: "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))",
            args_factory: || one_int("n", 100),
            tier: Tier::StdlibList,
            supported_by: TW_CR_BC,
        },
        CorpusCase {
            name: "stdlib_list_sum_range_start_end",
            source: "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(5, n))",
            args_factory: || one_int("n", 100),
            tier: Tier::StdlibList,
            supported_by: TW_CR_BC,
        },
        CorpusCase {
            name: "stdlib_list_sum_range_empty",
            source: "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))",
            args_factory: || one_int("n", 0),
            tier: Tier::StdlibList,
            supported_by: TW_CR_BC,
        },
        // Wave R2 — pipe operator. `range(n) | list.sum` is a pure
        // static desugar of `list.sum(range(n))`: the lowering prepends
        // the pipe LHS as the call's first positional arg, so it folds
        // into the exact same `Op::Loop` accumulator the spelled-out
        // call above emits. Differential agreement here is the
        // regression guard for the pipe desugar.
        CorpusCase {
            name: "pipe_range_into_list_sum",
            source: "#import list from \"std/list\"\n#main(Int n) -> Int\nrange(n) | list.sum",
            args_factory: || one_int("n", 100),
            tier: Tier::StdlibList,
            supported_by: TW_CR_BC,
        },
        // ---- Wave R3: general list-building higher-order ops ----
        // `range(n)` as a materialised List<Int> value (not folded inside
        // an eliding consumer), general `.map`/`.filter`/`reduce` and the
        // `_list_map`/`_list_filter`/`_list_reduce` free-function forms,
        // plus comprehension desugared onto the same machinery. The
        // cranelift backend lowers these via `emit_range_materialize`
        // (range value) and the bundled `list_int_map`/`filter`/`fold`
        // bodies (closure dispatched through the proven `Op::CallClosure`
        // substrate). Differential agreement vs the tree-walk oracle is
        // the regression guard. List-returning shapes use `TW_CR` (the
        // bytecode VM rejects List<Int> return entry shapes and these
        // `range`-based shapes aren't in the trace recipe catalogue);
        // `_list_reduce` returns Int and rides `TW_CR` for the same
        // range-recipe reason.
        CorpusCase {
            name: "r3_range_value",
            source: "#main(Int n) -> List<Int>\nrange(n)",
            args_factory: || one_int("n", 5),
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3_range_value_empty",
            source: "#main(Int n) -> List<Int>\nrange(n)",
            args_factory: || one_int("n", 0),
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3_range_map",
            source: "#main(Int n) -> List<Int>\nrange(n).map((Int x) => x * x)",
            args_factory: || one_int("n", 5),
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3_range_filter",
            source: "#main(Int n) -> List<Int>\nrange(n).filter((Int x) => x > 1)",
            args_factory: || one_int("n", 5),
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3_range_filter_empty",
            source: "#main(Int n) -> List<Int>\nrange(n).filter((Int x) => x > 100)",
            args_factory: || one_int("n", 5),
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3_list_map_free",
            source: "#main(Int n) -> List<Int>\n_list_map(range(n), (Int x) => x + 100)",
            args_factory: || one_int("n", 4),
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3_list_filter_free",
            source: "#main(Int n) -> List<Int>\n_list_filter(range(n), (Int x) => x % 2 == 0)",
            args_factory: || one_int("n", 6),
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3_list_reduce_free",
            source: "#import list from \"std/list\"\n#main(Int n) -> Int\n_list_reduce(range(n), 0, (Int a, Int x) => a + x)",
            args_factory: || one_int("n", 5),
            tier: Tier::StdlibList,
            supported_by: TW_CR_BC,
        },
        CorpusCase {
            name: "r3_comprehension",
            source: "#main(Int n) -> List<Int>\n[x * 2 for x in range(n)]",
            args_factory: || one_int("n", 4),
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3_comprehension_if",
            source: "#main(Int n) -> List<Int>\n[x * 10 for x in range(n) if x > 1]",
            args_factory: || one_int("n", 5),
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        // ---- Wave R3b: typed list higher-order ops over List<Float> +
        //      the element-type-changing numeric `map` shapes. The
        //      `list_float_*` bundled bodies share the `List<Int>` record
        //      layout (8-byte slots) and dispatch the closure per element
        //      via `Op::CallClosure`, so cranelift / tree-walk agree
        //      element-by-element. Float arithmetic inside the closure is
        //      IEEE-754 (no overflow trap), matched to the tree-walker.
        //      List-returning shapes ride `TW_CR` (the bytecode VM rejects
        //      List return entry shapes and these aren't in the trace
        //      recipe catalogue); the `Float`-returning reduce rides
        //      `TW_CR` too (Float scalar return is out of the bytecode
        //      entry envelope). The wasm / llvm-native legs are covered by
        //      the dedicated `list_float_hof_four_way` test +
        //      `aot_wasm_parity::r3b_float_reduce_*`.
        CorpusCase {
            name: "r3b_float_map",
            source: "#main() -> List<Float>\n[1.0, 2.0, 3.0].map((Float x) => x * 2.0)",
            args_factory: no_args,
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3b_float_filter",
            source: "#main() -> List<Float>\n[0.5, 1.5, 2.5, 0.9].filter((Float x) => x > 1.0)",
            args_factory: no_args,
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3b_float_reduce_sum",
            source: "#main() -> Float\n\
                      _list_reduce([1.0, 2.0, 3.0, 4.0], 0.0, (Float a, Float x) => a + x)",
            args_factory: no_args,
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3b_float_reduce_max",
            source: "#main() -> Float\n\
                      _list_reduce([3.0, 1.0, 4.0, 1.5, 9.0, 2.0], 0.0, \
                      (Float a, Float x) => x > a ? x : a)",
            args_factory: no_args,
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        // Element-type-changing numeric map: result list element type is
        // taken from the closure's inferred return type.
        CorpusCase {
            name: "r3b_int_to_float_map",
            source: "#main() -> List<Float>\n_list_map([1, 2, 3], (Int x) => x * 2.0)",
            args_factory: no_args,
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3b_float_to_int_map",
            source: "#main() -> List<Int>\n_list_map([1.5, 2.7, 3.2], (Float x) => x > 2.0 ? 1 : 0)",
            args_factory: no_args,
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        // Empty source through a non-literal (range filtered to nothing,
        // re-mapped to Float) — the `[]` literal isn't lowerable, so the
        // empty-list edge rides a range-derived source.
        CorpusCase {
            name: "r3b_float_map_empty",
            source: "#main(Int n) -> List<Float>\n\
                      _list_map(range(n).filter((Int x) => x > 100), (Int x) => x * 1.0)",
            args_factory: || one_int("n", 5),
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        // ---- Wave R3c: String-result list `map` family. The bundled
        //      `list_string_map` / `list_int_map_to_string` /
        //      `list_float_map_to_string` bodies build the result as a
        //      `List<String>` pointer-array record (`[count][off_i]…`,
        //      4-byte slots) in scratch — every slot an arena-relative
        //      String handle the closure produced — and the entry returns
        //      it via the in-place region-walk ABI (no relocation,
        //      byte-equal to the tree-walk `_list_map`). These ride
        //      `TW_CR`: `List<String>` returns are three-way today
        //      (tree-walk + cranelift + llvm-native, proven in
        //      `list_string_hof_three_way`); the wasm leg is not yet
        //      decodable for a `List<String>` return (see that test's
        //      module doc). `List<String>` filter stays capped (no
        //      provable `String -> Bool` predicate).
        CorpusCase {
            name: "r3c_string_map_concat",
            source: "#main() -> List<String>\n[\"a\", \"b\", \"c\"].map((String s) => s + \"!\")",
            args_factory: no_args,
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3c_string_map_free",
            source: "#main() -> List<String>\n_list_map([\"x\", \"y\"], (String s) => s + s)",
            args_factory: no_args,
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        // The headline shape: a numeric source mapped to String via an
        // f-string closure, result element type taken from the closure's
        // inferred `String` return.
        CorpusCase {
            name: "r3c_range_map_fstring",
            source: "#main(Int n) -> List<String>\nrange(n).map((Int x) => f\"v${x}\")",
            args_factory: || one_int("n", 4),
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3c_range_map_fstring_empty",
            source: "#main(Int n) -> List<String>\nrange(n).map((Int x) => f\"v${x}\")",
            args_factory: || one_int("n", 0),
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3c_int_map_to_string_free",
            source: "#main() -> List<String>\n_list_map([1, 2, 3], (Int x) => f\"#${x}\")",
            args_factory: no_args,
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r3c_float_map_to_string",
            source: "#main() -> List<String>\n[1.5, 2.5].map((Float x) => \"f\")",
            args_factory: no_args,
            tier: Tier::StdlibList,
            supported_by: TW_CR,
        },
        // ---- Wave R7: scalar-returning Float math stdlib. `abs` (Float
        //      overload), `floor` / `ceil` / `round` (Float -> Int via
        //      saturating `as i64`), `sqrt` (Float, NaN on negatives).
        //      Each lowers to the new `Op::F64Unary` / `Op::F64ToI64Sat`
        //      float intrinsics (NOT a libcall) — byte-equal with the
        //      tree-walk oracle (`f64::abs` / `floor` / `ceil` /
        //      `round_ties_even` / `sqrt`). These ride `TW_CR`: a scalar
        //      Float/Int return is established four-way (incl. wasm) in
        //      `aot_wasm_parity::r7_*`; the bytecode / trace tiers keep
        //      their Float-scalar exclusion (mirrors the R3b float rows).
        //      `pow` stays capped — it needs a `pow` libcall with no
        //      native wasm instruction.
        CorpusCase {
            name: "r7_abs_float",
            source: "#main() -> Float\nabs(0.0 - 5.5)",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r7_floor",
            source: "#main() -> Int\nfloor(3.7)",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r7_floor_neg",
            source: "#main() -> Int\nfloor(0.0 - 3.2)",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r7_ceil",
            source: "#main() -> Int\nceil(3.2)",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r7_round_ties_even_down",
            source: "#main() -> Int\nround(2.5)",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r7_round_ties_even_up",
            source: "#main() -> Int\nround(3.5)",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r7_sqrt",
            source: "#main() -> Float\nsqrt(9.0)",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r7_sqrt_int_widen",
            source: "#main() -> Float\nsqrt(16)",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        // Wave R8: byte-level string stdlib lowered four-way. Tree-walk ==
        // cranelift here; the wasm + llvm-native legs live in
        // `relon-codegen-llvm::aot_wasm_parity::r8_*`. Edges covered:
        // empty string, no-match / overlap / empty-`from` / grow replace.
        // `trim` / `trim_start` / `trim_end` stay capped (UTF-8 decode
        // seam unsupported on LLVM-native / wasm — see the `string_ops`
        // module docs); `matches` (regex) and `split` (List<String>) too.
        CorpusCase {
            name: "r8_len",
            source: "#main() -> Int\nlen(\"hello\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r8_len_unicode",
            source: "#main() -> Int\nlen(\"café\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r8_ends_with_true",
            source: "#main() -> Bool\nends_with(\"hello\", \"lo\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r8_ends_with_false",
            source: "#main() -> Bool\nends_with(\"hello\", \"xo\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r8_ends_with_empty",
            source: "#main() -> Bool\nends_with(\"hello\", \"\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r8_replace_all",
            source: "#main() -> String\n\"aXbXc\".replace(\"X\", \"-\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r8_replace_overlap",
            source: "#main() -> String\n\"aaa\".replace(\"aa\", \"b\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r8_replace_nomatch",
            source: "#main() -> String\n\"abc\".replace(\"X\", \"-\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r8_replace_empty_from",
            source: "#main() -> String\n\"ab\".replace(\"\", \"-\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r8_replace_empty_from_unicode",
            source: "#main() -> String\n\"café\".replace(\"\", \"X\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        // Wave R9: Bool-returning `is_uuid` validator lowered four-way.
        // Tree-walk == cranelift here; the wasm + llvm-native legs live in
        // `relon-codegen-llvm::aot_wasm_parity::r9_*`. Edges covered: valid
        // canonical UUID (lower / upper hex), wrong length, dash-position
        // failure, and a non-hex byte. Sibling validators stay capped:
        // `is_email` / `is_uri` (UTF-8 decode seam), `is_ipv4` / `is_ipv6`
        // (`core::net` parser, no wasm body), `is_iso_date` (needs integer
        // div/rem for the leap-year test, no `DivS` / `RemS` IR op).
        CorpusCase {
            name: "r9_is_uuid_valid",
            source: "#main() -> Bool\nis_uuid(\"12345678-1234-1234-1234-123456789012\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r9_is_uuid_upper_hex",
            source: "#main() -> Bool\nis_uuid(\"ABCDEF01-ABCD-ABCD-ABCD-ABCDEF012345\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r9_is_uuid_too_short",
            source: "#main() -> Bool\nis_uuid(\"12345678-1234-1234-1234-12345678901\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r9_is_uuid_bad_dash",
            source: "#main() -> Bool\nis_uuid(\"12345678X1234-1234-1234-123456789012\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r9_is_uuid_nonhex",
            source: "#main() -> Bool\nis_uuid(\"1234567g-1234-1234-1234-123456789012\")",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        // ---- StdlibNormalize: most complex ----
        // Bytecode rejects `String`-typed return; trace const table
        // pre-computes both NFD / NFC payloads.
        CorpusCase {
            name: "stdlib_nfd_combine",
            source: "#main() -> String\n\"é\".nfd()",
            args_factory: no_args,
            tier: Tier::StdlibNormalize,
            supported_by: TW_CR_TJ,
        },
        CorpusCase {
            name: "stdlib_nfc_combine",
            source: "#main() -> String\n\"e\\u0301\".nfc()",
            args_factory: no_args,
            tier: Tier::StdlibNormalize,
            supported_by: TW_CR_TJ,
        },
        // ---- DictReturn: schema-rooted output buffer ----
        // The trace recipe catalogue doesn't model dict construction;
        // bytecode handles `Int`-only return dicts but rejects `String`
        // fields.
        CorpusCase {
            name: "dict_simple_return",
            source: "#schema Out { Int v: * }\n#main(Int x) -> Out\n{ v: x * 2 }",
            args_factory: args_x_42,
            tier: Tier::DictReturn,
            supported_by: TW_CR_BC,
        },
        CorpusCase {
            name: "dict_with_string_return",
            source: "#schema Out { String name: *, Int n: * }\n#main() -> Out\n{ name: \"alice\", n: 42 }",
            args_factory: no_args,
            tier: Tier::DictReturn,
            supported_by: TW_CR,
        },
        // ---- Wave R10: backward static `&sibling` / `&root` field refs ----
        // A later anon-Dict-return field reads an EARLIER field's value
        // through `&sibling.<name>` (or `&root.<name>` — at the entry
        // dict, which IS the document root, both resolve to the same
        // field). The reference reuses the source-ordered field-let graph:
        // each host-visible scalar field is registered as an internal let
        // before later fields lower, and the reference lowers to the same
        // `Op::LetGet` a bare let read would. Reference-in-dict-field is a
        // `#relaxed`-mode feature (strict-mode static type derivation of a
        // reference is a separate analyzer concern; the production
        // `examples/pricing.relon` surface is `#relaxed`), so these carry
        // the directive. `Int` Dict return ⇒ `TW_CR` (the bytecode VM and
        // trace recipe catalogue don't model anon-Dict construction).
        CorpusCase {
            name: "r10_sibling_backward",
            source: "#relaxed\n#main(Int a, Int b) -> Dict\n{ x: a + b, y: &sibling.x * 2 }",
            args_factory: args_x_40_y_2,
            tier: Tier::DictReturn,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r10_root_backward_entry_level",
            source: "#relaxed\n#main(Int a, Int b) -> Dict\n{ x: a + b, y: &root.x * 2 }",
            args_factory: args_x_40_y_2,
            tier: Tier::DictReturn,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r10_sibling_chain",
            source: "#relaxed\n#main(Int a, Int b) -> Dict\n{ x: a + b, y: &sibling.x * 2, z: &root.x + &sibling.y }",
            args_factory: args_x_17_y_5,
            tier: Tier::DictReturn,
            supported_by: TW_CR,
        },
        // ---- Wave R10b: STRICT-mode `&sibling` / `&root` derivation ----
        // Same backward `&sibling.x` / `&root.x` chain as `r10_sibling_chain`
        // but WITHOUT the `#relaxed` directive. R10b taught the strict-mode
        // analyzer to derive a single-segment, backward `&sibling.<name>` /
        // entry-level `&root.<name>` reference's type from the target
        // sibling/root field's static type (`relon-analyzer`'s
        // `infer::infer_reference`), so the same program now passes strict
        // analysis. The lowering is unchanged (R10 already handles the
        // backward field-let graph), so this proves the strict path runs
        // byte-equal four-way. `Int` Dict return ⇒ `TW_CR` (bytecode VM and
        // trace recipes don't model anon-Dict construction).
        CorpusCase {
            name: "r10b_strict_sibling_chain",
            source: "#main(Int a, Int b) -> Dict\n{ x: a + b, y: &sibling.x * 2, z: &root.x + &sibling.y }",
            args_factory: args_x_17_y_5,
            tier: Tier::DictReturn,
            supported_by: TW_CR,
        },
        // ---- Wave R11: field decorators on the anon-Dict-return path ----
        // A decorated field `@deco(args) k: v` desugars to the call
        // `deco(v, args)` — the decorated value is the FIRST positional
        // arg, the decorator's own args follow (matching the tree-walk
        // `fallback_decorator`, which prepends `value` before the
        // evaluated decorator args). The decorator resolves to an
        // `#internal` field-form function (lifted to a closure let), so
        // the desugared call lowers through `try_lower_local_closure_call`.
        // `Int` Dict return ⇒ `TW_CR` (bytecode / trace don't model
        // anon-Dict construction). Verified byte-equal four-way for the
        // scalar-Int shape (wasm + llvm-native legs in
        // `relon-codegen-llvm::aot_wasm_parity`).
        CorpusCase {
            name: "r11_int_decorator",
            source: "#relaxed\n#main(Int p) -> Dict\n\
                     { #internal\n add(v, n): v + n,\n @add(100)\n x: p }",
            args_factory: || one_int("p", 5),
            tier: Tier::DictReturn,
            supported_by: TW_CR,
        },
        // Stacked decorators apply bottom-up (`@a @b v ≡ a(b(v))`): the
        // decorator nearest the value (`@mul`) wraps first, the outermost
        // (`@add`) wraps last. `@add(1) @mul(10) x: 5` ⇒ add(mul(5,10),1)
        // = 51.
        CorpusCase {
            name: "r11_stacked_decorators",
            source: "#relaxed\n#main(Int p) -> Dict\n\
                     { #internal\n add(v, n): v + n,\n #internal\n mul(v, n): v * n,\n \
                     @add(1) @mul(10)\n x: p }",
            args_factory: || one_int("p", 5),
            tier: Tier::DictReturn,
            supported_by: TW_CR,
        },
        // A builtin `@`-decorator (`@value`) has no compiled call form;
        // it caps loudly in `desugar_field_decorators` and `auto` falls
        // back to the tree-walk interpreter. Tree-walk only — the
        // compiled backend rejects it on purpose.
        CorpusCase {
            name: "r11_capped_builtin_value_decorator",
            source: "#relaxed\n#main(Int p) -> Dict\n{ @value(999)\n x: p }",
            args_factory: || one_int("p", 7),
            tier: Tier::DictReturn,
            supported_by: TW_ONLY,
        },
        // ---- Wave R4: static const-fold of `type(v)` ----
        // In strict mode the argument's IR type is statically known, so
        // `type(v)` lowers to a constant canonical type-name String
        // (`IrType::type_name`, asserted byte-equal to
        // `Value::type_name`). The argument is still evaluated for trap
        // parity, then discarded. `String` return ⇒ tree-walk +
        // cranelift claim (bytecode rejects String return; the trace
        // recipe catalogue has no `type` shape).
        CorpusCase {
            name: "r4_type_int",
            source: "#main(Int n) -> String\ntype(n)",
            args_factory: || one_int("n", 5),
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r4_type_float",
            source: "#main(Float f) -> String\ntype(f)",
            args_factory: || {
                HashMap::from([("f".to_string(), Value::Float(OrderedFloat(1.5)))])
            },
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r4_type_string",
            source: "#main(String s) -> String\ntype(s)",
            args_factory: || {
                HashMap::from([("s".to_string(), Value::String("hi".into()))])
            },
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        CorpusCase {
            name: "r4_type_bool",
            source: "#main(Bool b) -> String\ntype(b)",
            args_factory: || HashMap::from([("b".to_string(), Value::Bool(true))]),
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        // Coarsening: a `List<Int>` argument → "List" (every concrete
        // list element tag folds to the same name).
        CorpusCase {
            name: "r4_type_list_coarsen",
            source: "#main(Int n) -> String\ntype(range(n))",
            args_factory: || one_int("n", 3),
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        // Trap parity: the argument is evaluated before the type string
        // is produced, so an overflowing sub-expression traps identically
        // to the tree-walk `type` builtin (which reads
        // `args[0].type_name()` only after evaluating `args[0]`).
        CorpusCase {
            name: "r4_type_arg_overflow_traps",
            source: "#main(Int n) -> String\ntype(n * 9223372036854775807 + 9223372036854775807)",
            args_factory: || one_int("n", 2),
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        // ---- Wave R5: static arm selection of strict-mode `match` ----
        // The scrutinee's IR type is statically known, so the winning arm
        // is selected at compile time (no runtime brand dispatch). The
        // scrutinee is still evaluated + discarded for trap / ordering
        // parity (the R4 `type(v)` discard pattern), then the selected
        // arm's body is lowered as the result. `String`-returning shapes
        // ride `TW_CR` (bytecode rejects String return; the trace recipe
        // catalogue has no match shape) exactly like the R4 entries.
        //
        // A builtin-scalar pattern (`Int`) against an `Int` scrutinee
        // matches; the wildcard never fires.
        CorpusCase {
            name: "r5_match_int_arm",
            source: "#main(Int n) -> String\nn match { Int: \"int\", *: \"other\" }",
            args_factory: || one_int("n", 5),
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        // A non-literal arm body (`n * 2`) lowered as real IR — proves the
        // selected body is general codegen, not a folded constant. Int
        // return rides `TW_CR_BC` (bytecode handles Int return).
        CorpusCase {
            name: "r5_match_int_body_arith",
            source: "#main(Int n) -> Int\nn match { Int: n * 2, *: 0 }",
            args_factory: || one_int("n", 7),
            tier: Tier::StdlibSimple,
            supported_by: TW_CR_BC,
        },
        // Source ordering: two arms both statically match; the FIRST wins.
        CorpusCase {
            name: "r5_match_ordering_first_wins",
            source: "#main(Int n) -> String\nn match { Int: \"a\", Int: \"b\", *: \"c\" }",
            args_factory: || one_int("n", 3),
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        // A builtin-scalar pattern naming a DIFFERENT scalar than the
        // static type (`Int` arm vs a `String` scrutinee) provably never
        // matches; the wildcard wins.
        CorpusCase {
            name: "r5_match_scalar_mismatch_falls_to_wildcard",
            source: "#main(String s) -> String\ns match { Int: \"int\", *: \"other\" }",
            args_factory: || {
                HashMap::from([("s".to_string(), Value::String("hi".into()))])
            },
            tier: Tier::StdlibSimple,
            supported_by: TW_CR,
        },
        // ---- Wave R12: spread (`...x`) — capped, by-design boundaries ----
        //
        // Both spread forms are rejected before they reach the cranelift
        // lowering pass, for two orthogonal reasons. They are registered
        // `TW_ONLY` (the tree-walker is the only backend that runs them):
        // the corpus ratchet then asserts cranelift *gracefully* caps
        // rather than silently miscompiling.
        //
        // LIST spread `[...a, b, ...c]`: the analyzer infers a list
        // literal as a tuple (`infer/mod.rs` `Expr::List` ->
        // `InferredType::Tuple`), and a spread element keeps the source's
        // own type (`Expr::Spread(inner) -> infer_type(inner)`). So
        // `[...[1,2,3], 4, ...[5,6]]` infers as
        // `Tuple(Tuple(Int,Int,Int), Int, Tuple(Int,Int))`, never a
        // `List<Int>`. The tuple-folds-into-`List<T>` subsumption
        // (`infer/mod.rs` "List" arm) requires *every* tuple element to
        // satisfy the slot's `T`; a `List`/`Tuple` element never satisfies
        // a scalar `T`, so any `-> List<scalar>` return slot rejects with
        // a return-type mismatch. With no declared return the entry type
        // is `<missing>`, which the compiled backend's marshaller also
        // rejects. Spread flattening only happens at runtime in the
        // tree-walker (`eval.rs` `Expr::List` spread branch). Lifting this
        // would change list/spread *type inference* (fold a spread
        // source's element type into the surrounding tuple), not add a
        // codegen lowering — out of scope for an IR-coverage wave.
        CorpusCase {
            name: "r12_list_spread_capped",
            source: "#main()\n[...[1, 2, 3], 4, ...[5, 6]]",
            args_factory: no_args,
            tier: Tier::StdlibList,
            supported_by: TW_ONLY,
        },
        // DICT spread `{ ...base, k: v }`: blocked at the Dict-by-design
        // gap (Wave R9: "unsupported expression Dict in lowering"). The
        // compiled backends route objects through schema records and
        // cannot construct or return a free `Dict` value at all, so the
        // entry return type is `<missing>` and lowering caps before the
        // spread is ever considered. Spread *override*
        // (`{ ...{a:1}, a:2 }`) is a hard analyzer error
        // (`duplicate field produced by spread`) in every backend — the
        // language does not silently let later keys win. The no-override
        // merge below runs on the tree-walker and caps on cranelift.
        CorpusCase {
            name: "r12_dict_spread_capped",
            source: "#main()\n{ ...{ a: 1, b: 2 }, c: 3 }",
            args_factory: no_args,
            tier: Tier::DictReturn,
            supported_by: TW_ONLY,
        },
    ]
}
