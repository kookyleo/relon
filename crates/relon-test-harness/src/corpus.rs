//! Differential test corpus — `(source, args, expected)` tuples
//! driving the harness across both backends.
//!
//! Cases are grouped by tier so we can label which ones are
//! expected to pass `MatchOk` once cranelift widens to the next
//! tranche. Each entry's `tier` field is informational today
//! (the harness reports `CraneliftUnsupported` rather than
//! failing) but lets a future strict-mode runner enforce
//! "tier T must Match" once the lowering catches up.

use std::collections::HashMap;

use relon_eval_api::Value;

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
}

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
        },
        CorpusCase {
            name: "arith_sub",
            source: "#main(Int x, Int y) -> Int\nx - y",
            args_factory: args_x_neg3_y_10,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "arith_mul",
            source: "#main(Int x, Int y) -> Int\nx * y",
            args_factory: args_x_7_y_6,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "arith_div",
            source: "#main(Int x, Int y) -> Int\nx / y",
            args_factory: args_x_17_y_5,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "arith_mod",
            source: "#main(Int x, Int y) -> Int\nx % y",
            args_factory: args_x_17_y_5,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "arith_div_negative",
            source: "#main(Int x, Int y) -> Int\nx / y",
            args_factory: args_x_neg10_y_neg5,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "arith_chain",
            source: "#main(Int x, Int y) -> Int\nx * y + x",
            args_factory: args_x_3_y_minus_1,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "arith_paren",
            source: "#main(Int x, Int y) -> Int\n(x + y) * x",
            args_factory: args_x_5_y_5,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "arith_negate_via_sub",
            source: "#main(Int x) -> Int\n0 - x",
            args_factory: args_x_42,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "arith_div_by_zero_traps",
            source: "#main(Int x, Int y) -> Int\nx / y",
            args_factory: args_x_5_y_0,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "arith_mod_by_zero_traps",
            source: "#main(Int x, Int y) -> Int\nx % y",
            args_factory: args_x_5_y_0,
            tier: Tier::ArithControl,
        },
        // ---- cmp ----
        CorpusCase {
            name: "cmp_eq_true",
            source: "#main(Int x, Int y) -> Bool\nx == y",
            args_factory: args_x_5_y_5,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "cmp_eq_false",
            source: "#main(Int x, Int y) -> Bool\nx == y",
            args_factory: args_x_5_y_0,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "cmp_ne",
            source: "#main(Int x, Int y) -> Bool\nx != y",
            args_factory: args_x_5_y_5,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "cmp_lt",
            source: "#main(Int x, Int y) -> Bool\nx < y",
            args_factory: args_x_3_y_minus_1,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "cmp_le_eq",
            source: "#main(Int x, Int y) -> Bool\nx <= y",
            args_factory: args_x_5_y_5,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "cmp_gt",
            source: "#main(Int x, Int y) -> Bool\nx > y",
            args_factory: args_x_17_y_5,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "cmp_ge_eq",
            source: "#main(Int x, Int y) -> Bool\nx >= y",
            args_factory: args_x_5_y_5,
            tier: Tier::ArithControl,
        },
        // ---- control flow ----
        CorpusCase {
            name: "if_true_arm",
            source: "#main(Int x, Int y) -> Int\nx > y ? x : y",
            args_factory: args_x_17_y_5,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "if_false_arm",
            source: "#main(Int x, Int y) -> Int\nx > y ? x : y",
            args_factory: args_x_neg3_y_10,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "if_nested",
            source:
                "#main(Int x) -> Int\nx > 0 ? (x > 10 ? x * 2 : x + 1) : (0 - x)",
            args_factory: args_x_42,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "if_nested_neg",
            source:
                "#main(Int x) -> Int\nx > 0 ? (x > 10 ? x * 2 : x + 1) : (0 - x)",
            args_factory: args_x_neg42,
            tier: Tier::ArithControl,
        },
        // ---- let-binding (Relon uses `where { name: value }` postfix) ----
        CorpusCase {
            name: "let_then_add",
            source: "#main(Int x) -> Int\n(y + 1) where { y: x * 2 }",
            args_factory: args_x_42,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "let_chain",
            source: "#main(Int x) -> Int\nc where { a: x + 1, b: a * 2, c: b - 3 }",
            args_factory: args_x_42,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "let_uses_cond",
            source: "#main(Int x) -> Int\n(y * 2) where { y: x > 0 ? x : 0 - x }",
            args_factory: args_x_neg42,
            tier: Tier::ArithControl,
        },
        // ---- boundary values ----
        CorpusCase {
            name: "boundary_max_plus_one",
            source: "#main(Int x) -> Int\nx + 1",
            args_factory: args_x_max,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "boundary_min_minus_one",
            source: "#main(Int x) -> Int\nx - 1",
            args_factory: args_x_min,
            tier: Tier::ArithControl,
        },
        CorpusCase {
            name: "boundary_zero_times_x",
            source: "#main(Int x) -> Int\n0 * x",
            args_factory: args_x_max,
            tier: Tier::ArithControl,
        },
        // ---- StdlibSimple: not yet on cranelift, but tree-walk validates the corpus shape ----
        CorpusCase {
            name: "stdlib_abs_pos",
            source: "#main(Int x) -> Int\nabs(x)",
            args_factory: args_x_42,
            tier: Tier::StdlibSimple,
        },
        CorpusCase {
            name: "stdlib_abs_neg",
            source: "#main(Int x) -> Int\nabs(x)",
            args_factory: args_x_neg42,
            tier: Tier::StdlibSimple,
        },
        CorpusCase {
            name: "stdlib_min",
            source: "#main(Int x, Int y) -> Int\nmin(x, y)",
            args_factory: args_x_17_y_5,
            tier: Tier::StdlibSimple,
        },
        CorpusCase {
            name: "stdlib_max",
            source: "#main(Int x, Int y) -> Int\nmax(x, y)",
            args_factory: args_x_17_y_5,
            tier: Tier::StdlibSimple,
        },
        CorpusCase {
            name: "stdlib_min_neg",
            source: "#main(Int x, Int y) -> Int\nmin(x, y)",
            args_factory: args_x_neg10_y_neg5,
            tier: Tier::StdlibSimple,
        },
        CorpusCase {
            name: "stdlib_length_const",
            source: "#main() -> Int\n\"hello\".length()",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
        },
        CorpusCase {
            name: "stdlib_is_empty_false",
            source: "#main() -> Bool\n\"hi\".is_empty()",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
        },
        CorpusCase {
            name: "stdlib_is_empty_true",
            source: "#main() -> Bool\n\"\".is_empty()",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
        },
        CorpusCase {
            name: "stdlib_list_int_length",
            source: "#main() -> Int\n[1, 2, 3, 4, 5].length()",
            args_factory: no_args,
            tier: Tier::StdlibSimple,
        },
        // ---- StdlibMemory: needs scratch arena ----
        CorpusCase {
            name: "stdlib_concat_const",
            source: "#main() -> String\n\"foo\".concat(\"bar\")",
            args_factory: no_args,
            tier: Tier::StdlibMemory,
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
        },
        // #165 — three-leaf chain hits the minimal `StrConcatN { 3 }`
        // shape the fold gate accepts (outer Add + lhs itself a
        // Binary(Add)).
        CorpusCase {
            name: "str_concat_chain_three_way",
            source: "#main() -> String\n\"foo\" + \"bar\" + \"baz\"",
            args_factory: no_args,
            tier: Tier::StdlibMemory,
        },
        CorpusCase {
            name: "stdlib_substring",
            source: "#main() -> String\n\"hello\".substring(1, 3)",
            args_factory: no_args,
            tier: Tier::StdlibMemory,
        },
        CorpusCase {
            name: "stdlib_starts_with_true",
            source: "#main() -> Bool\n\"hello world\".starts_with(\"hello\")",
            args_factory: no_args,
            tier: Tier::StdlibMemory,
        },
        CorpusCase {
            name: "stdlib_starts_with_false",
            source: "#main() -> Bool\n\"hello world\".starts_with(\"world\")",
            args_factory: no_args,
            tier: Tier::StdlibMemory,
        },
        // ---- StdlibCaseFold ----
        CorpusCase {
            name: "stdlib_upper_ascii",
            source: "#main() -> String\n\"hello\".upper()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
        },
        CorpusCase {
            name: "stdlib_lower_ascii",
            source: "#main() -> String\n\"WORLD\".lower()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
        },
        CorpusCase {
            name: "stdlib_title_ascii",
            source: "#main() -> String\n\"hello world\".title()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
        },
        CorpusCase {
            name: "stdlib_upper_unicode_greek",
            source: "#main() -> String\n\"σίγμα\".upper()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
        },
        CorpusCase {
            name: "stdlib_lower_final_sigma",
            source: "#main() -> String\n\"ΣΙΓΜΑ\".lower()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
        },
        // v3++ b-7 reframed: FULL multi-cp + Σ-context coverage.
        CorpusCase {
            name: "stdlib_upper_sharp_s",
            source: "#main() -> String\n\"straße\".upper()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
        },
        CorpusCase {
            name: "stdlib_lower_final_sigma_at_end",
            source: "#main() -> String\n\"ΟΔΥΣΣΕΥΣ\".lower()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
        },
        CorpusCase {
            name: "stdlib_upper_ligature_fi",
            source: "#main() -> String\n\"ﬁle\".upper()",
            args_factory: no_args,
            tier: Tier::StdlibCaseFold,
        },
        // ---- StdlibList: higher-order needs closure ABI ----
        CorpusCase {
            name: "stdlib_list_sum",
            source: "#main() -> Int\n[1, 2, 3, 4, 5].sum()",
            args_factory: no_args,
            tier: Tier::StdlibList,
        },
        CorpusCase {
            name: "stdlib_list_max",
            source: "#main() -> Int\n[3, 1, 4, 1, 5, 9, 2, 6].max()",
            args_factory: no_args,
            tier: Tier::StdlibList,
        },
        // review-improvement-160 bytecode M3 phase 2: the IR-level
        // `list.sum(range(...))` peephole desugar.  The tree-walker
        // resolves the call through the dynamic `std/list` module
        // loader; the bytecode + cranelift backends recognise the
        // pattern in `lower_fn_call` and emit an explicit `Op::Loop`
        // accumulator.  Differential agreement here is the regression
        // guard — without the peephole, the IR side surfaces
        // `UnresolvedVariable("list")` and fails the diff.
        CorpusCase {
            name: "stdlib_list_sum_range_n",
            source: "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))",
            args_factory: || one_int("n", 100),
            tier: Tier::StdlibList,
        },
        CorpusCase {
            name: "stdlib_list_sum_range_start_end",
            source: "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(5, n))",
            args_factory: || one_int("n", 100),
            tier: Tier::StdlibList,
        },
        CorpusCase {
            name: "stdlib_list_sum_range_empty",
            source: "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))",
            args_factory: || one_int("n", 0),
            tier: Tier::StdlibList,
        },
        // ---- StdlibNormalize: most complex ----
        CorpusCase {
            name: "stdlib_nfd_combine",
            source: "#main() -> String\n\"é\".nfd()",
            args_factory: no_args,
            tier: Tier::StdlibNormalize,
        },
        CorpusCase {
            name: "stdlib_nfc_combine",
            source: "#main() -> String\n\"e\\u0301\".nfc()",
            args_factory: no_args,
            tier: Tier::StdlibNormalize,
        },
        // ---- DictReturn: schema-rooted output buffer ----
        CorpusCase {
            name: "dict_simple_return",
            source: "#schema Out { Int v: * }\n#main(Int x) -> Out\n{ v: x * 2 }",
            args_factory: args_x_42,
            tier: Tier::DictReturn,
        },
        CorpusCase {
            name: "dict_with_string_return",
            source: "#schema Out { String name: *, Int n: * }\n#main() -> Out\n{ name: \"alice\", n: 42 }",
            args_factory: no_args,
            tier: Tier::DictReturn,
        },
    ]
}
