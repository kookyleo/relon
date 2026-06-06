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
    ]
}
