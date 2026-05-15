//! Mode-matrix probe: for each scenario below, runs the same source
//! through `analyze_with_options` twice — once with `strict_mode: false`
//! and once with `strict_mode: true` — and collects the diagnostic
//! codes that fire under each. The single test prints a markdown table
//! so `cargo test -- --nocapture` doubles as the source of truth for
//! the public strict-mode reference (`docs/en/guide/strict-mode.md`).
//!
//! Each row's `expect_lax` / `expect_strict` set is asserted, so this
//! file also acts as a regression net: if a future analyzer pass
//! reclassifies a scenario, the test breaks and the doc must be
//! updated to match.

use miette::Diagnostic as MietteDiagnostic;
use relon_analyzer::{analyze_with_options, AnalyzeOptions};
use relon_parser::parse_document;
use std::collections::BTreeSet;

struct Case {
    id: &'static str,
    desc: &'static str,
    source: &'static str,
    /// Diagnostic miette codes expected under `strict_mode: false`.
    /// Empty = silent.
    expect_lax: &'static [&'static str],
    /// Diagnostic miette codes expected under `strict_mode: true`.
    expect_strict: &'static [&'static str],
}

// Helpers (`run`, `diag_codes`) were folded into the `matrix` body
// once the per-case host_fn_names threading made them no longer
// reusable in their original shape. Keeping the imports minimal.

#[test]
fn matrix() {
    let cases: &[Case] = &[
        // ---- 1. Spread ----
        Case {
            id: "spread_literal",
            desc: "spread a dict literal",
            source: r#"{ ...{a: 1} }"#,
            expect_lax: &[],
            expect_strict: &[],
        },
        Case {
            id: "spread_typed_sibling",
            desc: "spread a sibling field typed as Schema",
            source: r#"
#schema Extra { Int n: * }
{
    Extra e: { n: 1 },
    out: { ...e }
}
"#,
            expect_lax: &[],
            expect_strict: &[],
        },
        Case {
            id: "spread_non_dict_source",
            desc: "spread a statically non-dict reference (Int sum)",
            // `src` is `Int`, not a dict/schema. v2: `NonSpreadableSource`
            // fires cross-mode — no `<T>` hint can rescue a non-dict
            // source, so the program is wrong regardless of strict mode.
            source: r#"{ src: 1 + 2, out: { ...src } }"#,
            expect_lax: &["relon::analyze::non_spreadable_source"],
            expect_strict: &["relon::analyze::non_spreadable_source"],
        },
        Case {
            id: "spread_unknown_source",
            desc: "spread a reference whose static type is genuinely unknown",
            // `n` is an untyped closure parameter — analyzer literally
            // can't know its shape without an annotation, so strict
            // demands a `<T>` hint while non-strict accepts the silent
            // fallback to runtime checks.
            // The closure return type is still inferable here (a dict
            // literal), so `closure_return_type_unknown` doesn't fire
            // — only the param-missing-type and the spread-source-
            // unknown diagnostics surface.
            source: "#schema Extra { Int n: * }\n{ f: (n) => { ...n } }",
            expect_lax: &[],
            expect_strict: &[
                "relon::analyze::spread_source_type_unknown",
                "relon::analyze::closure_param_type_missing",
            ],
        },
        Case {
            id: "spread_with_hint",
            desc: "untyped source + explicit <Schema> hint",
            source: "#schema Extra { Int n: * }\n{ e: { n: 1 }, out: { ...<Extra> e } }",
            expect_lax: &[],
            expect_strict: &[],
        },
        // ---- 2. Dynamic dict key ----
        Case {
            id: "dyn_key_untyped",
            desc: "[expr] key without <T> hint",
            source: r#"{ k: "foo", out: { [&sibling.k]: 1 } }"#,
            expect_lax: &[],
            expect_strict: &["relon::analyze::dynamic_key_type_unknown"],
        },
        Case {
            id: "dyn_key_hint",
            desc: "[<String> expr] key with type hint",
            source: r#"{ k: "foo", out: { [<String> &sibling.k]: 1 } }"#,
            expect_lax: &[],
            expect_strict: &[],
        },
        // ---- 3. Schema reference ----
        Case {
            id: "schema_undeclared_in_spread",
            desc: "spread <UndeclaredSchema>",
            // v2 cross-mode: `<Missing>` doesn't point at a declared
            // schema in the workspace — the runtime would error too,
            // so non-strict no longer gives this a free pass.
            source: r#"{ e: { a: 1 }, out: { ...<Missing> e } }"#,
            expect_lax: &["relon::analyze::unresolved_schema"],
            expect_strict: &["relon::analyze::unresolved_schema"],
        },
        // ---- 4. Native fn calls (declared via host_fn_names without signature) ----
        Case {
            id: "native_without_signature",
            desc: "host-registered native fn, no signature",
            // Strict additionally flags the call site with ExpressionTypeUnknown
            // because the return type can't be derived from the missing
            // signature — the two diagnostics double up.
            source: r#"{ out: ext_call(1) }"#,
            expect_lax: &[],
            expect_strict: &[
                "relon::analyze::expression_type_unknown",
                "relon::analyze::native_fn_signature_missing",
            ],
        },
        // ---- 5. Closure parameter / return ----
        Case {
            id: "closure_typed_param",
            desc: "closure parameter with declared type",
            source: r#"{ f: (Int n) => n + 1 }"#,
            expect_lax: &[],
            expect_strict: &[],
        },
        Case {
            id: "closure_untyped_param",
            desc: "closure parameter without declared type",
            // Strict fires both: param missing type AND the body's
            // synthesized return falls to `Any` because the param leaked
            // `Any` into the body scope.
            source: r#"{ f: (n) => n + 1 }"#,
            expect_lax: &[],
            expect_strict: &[
                "relon::analyze::closure_return_type_unknown",
                "relon::analyze::closure_param_type_missing",
            ],
        },
        Case {
            id: "closure_unclassified_body",
            desc: "closure body lands on Any (untyped param + native call)",
            source: r#"{ f: (n) => ext_call(n) }"#,
            expect_lax: &[],
            expect_strict: &[
                "relon::analyze::native_fn_signature_missing",
                "relon::analyze::closure_return_type_unknown",
                "relon::analyze::closure_param_type_missing",
            ],
        },
        // ---- 6. Reference path ----
        Case {
            id: "path_known_field",
            desc: "schema-typed path on a declared field",
            source: "#schema User { String name: * }\n{ User u: { name: \"x\" }, out: &sibling.u.name }",
            expect_lax: &[],
            expect_strict: &[],
        },
        Case {
            id: "path_unknown_segment",
            desc: "descend into an undeclared schema field",
            // v2 cross-mode: the path-tail walker has positive knowledge
            // that `User` has no `unknown` field — emits
            // `unknown_reference_type` in both modes. The legacy
            // resolver also fires its `unresolved_reference` warning
            // alongside; the two together pin both the resolution
            // failure and the type-walk failure on the same span.
            source: "#schema User { String name: * }\n{ User u: { name: \"x\" }, out: &sibling.u.unknown }",
            expect_lax: &[
                "relon::analyze::unknown_reference_type",
                "relon::analyze::unresolved_reference",
            ],
            expect_strict: &[
                "relon::analyze::unknown_reference_type",
                "relon::analyze::unresolved_reference",
            ],
        },
        Case {
            id: "path_descend_into_leaf",
            desc: "descend past a leaf-typed segment",
            // v2 cross-mode: `u.id: Int`, descending into `Int` has no
            // valid program semantics — fires in both modes.
            source: "#schema User { Int id: * }\n{ User u: { id: 1 }, out: &sibling.u.id.something }",
            expect_lax: &["relon::analyze::unknown_reference_type"],
            expect_strict: &["relon::analyze::unknown_reference_type"],
        },
        Case {
            id: "ref_head_unresolved",
            desc: "free identifier — no enclosing binding",
            // Head-unresolved is still warning-only in non-strict
            // because spread / runtime might supply the binding. Strict
            // escalates the warning to a hard error.
            source: r#"{ a: 1, total: nowhere }"#,
            expect_lax: &["relon::analyze::unresolved_reference"],
            expect_strict: &[
                "relon::analyze::unknown_reference_type",
                "relon::analyze::unresolved_reference",
            ],
        },
        // ---- 7. #main parameters ----
        Case {
            id: "main_typed_param",
            desc: "#main(Int x) -> Dict<...>",
            source: "#main(Int x) -> Dict<String, Int>\n{ result: 0 }\n",
            expect_lax: &[],
            expect_strict: &[],
        },
        // NOTE: `#main(x)` (untyped param) is rejected by the parser
        // itself — the directive's grammar requires `Type name` for
        // every parameter — so `ClosureParamTypeMissing` is
        // unreachable from `#main`. The diagnostic only applies to
        // closures (covered by `closure_untyped_param`). Keeping this
        // out of the matrix avoids documenting a row that's impossible
        // to trigger from source.
        // ---- 8. Cross-mode bans (v1.6 / v1.7) ----
        Case {
            id: "explicit_any_field",
            desc: "explicit `Any` type annotation",
            source: r#"{ Any x: 1 }"#,
            expect_lax: &["relon::analyze::explicit_any_forbidden"],
            expect_strict: &["relon::analyze::explicit_any_forbidden"],
        },
        Case {
            id: "bare_list",
            desc: "bare `List` without generic argument",
            source: r#"{ List xs: [1, 2, 3] }"#,
            expect_lax: &["relon::analyze::bare_generic_container"],
            expect_strict: &["relon::analyze::bare_generic_container"],
        },
        Case {
            id: "bare_dict",
            desc: "bare `Dict` without generic arguments",
            source: r#"{ Dict m: { a: 1 } }"#,
            expect_lax: &["relon::analyze::bare_generic_container"],
            expect_strict: &["relon::analyze::bare_generic_container"],
        },
        Case {
            id: "duplicate_field",
            desc: "spread re-declares an existing key (typed source)",
            // Typed spread (`<Extra> e`) lets the analyzer compare keys.
            // Mode-independent: DuplicateField fires under both lax and
            // strict.
            source: "#schema Extra { Int a: * }\n{ Extra e: { a: 1 }, out: { a: 2, ...<Extra> e } }",
            expect_lax: &["relon::analyze::duplicate_field"],
            expect_strict: &["relon::analyze::duplicate_field"],
        },
    ];

    let mut native_signatures = std::collections::HashMap::new();
    let _ = native_signatures.insert("noop".to_string(), ());
    // We seed `host_fn_names` for the native-fn cases so the analyzer
    // sees `ext_call` as a registered fn rather than a typo.
    let mut host_fn_names = std::collections::HashSet::new();
    host_fn_names.insert("ext_call".to_string());

    let mut rows = Vec::new();
    let mut failures = Vec::new();

    for c in cases {
        // Re-run with seeded host_fn_names for native-fn cases.
        let parse = parse_document(c.source)
            .unwrap_or_else(|e| panic!("case {}: parse error: {}\nsource:\n{}", c.id, e, c.source));
        let opts_lax = AnalyzeOptions {
            strict_mode: false,
            host_fn_names: host_fn_names.clone(),
            ..AnalyzeOptions::default()
        };
        let opts_strict = AnalyzeOptions {
            strict_mode: true,
            host_fn_names: host_fn_names.clone(),
            ..AnalyzeOptions::default()
        };
        let lax: BTreeSet<String> = analyze_with_options(&parse, &opts_lax)
            .diagnostics
            .iter()
            .filter_map(|d| d.code().map(|c| c.to_string()))
            .collect();
        let strict: BTreeSet<String> = analyze_with_options(&parse, &opts_strict)
            .diagnostics
            .iter()
            .filter_map(|d| d.code().map(|c| c.to_string()))
            .collect();

        let expect_lax: BTreeSet<String> = c.expect_lax.iter().map(|s| s.to_string()).collect();
        let expect_strict: BTreeSet<String> =
            c.expect_strict.iter().map(|s| s.to_string()).collect();

        if lax != expect_lax {
            failures.push(format!(
                "{}: lax expected {:?}, got {:?}",
                c.id, expect_lax, lax
            ));
        }
        if strict != expect_strict {
            failures.push(format!(
                "{}: strict expected {:?}, got {:?}",
                c.id, expect_strict, strict
            ));
        }

        rows.push((c.id, c.desc, lax, strict));
    }

    // Always print the table so `-- --nocapture` reveals the matrix.
    println!("\n## strict-mode matrix\n");
    println!("| id | scenario | non-strict | strict |");
    println!("|---|---|---|---|");
    for (id, desc, lax, strict) in &rows {
        let fmt = |s: &BTreeSet<String>| -> String {
            if s.is_empty() {
                "—".to_string()
            } else {
                s.iter()
                    .map(|c| format!("`{}`", c))
                    .collect::<Vec<_>>()
                    .join(" + ")
            }
        };
        println!("| {} | {} | {} | {} |", id, desc, fmt(lax), fmt(strict));
    }
    println!();

    if !failures.is_empty() {
        panic!(
            "{} matrix failures:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }
}
