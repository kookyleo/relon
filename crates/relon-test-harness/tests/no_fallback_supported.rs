//! Wave R6 capstone proof: `auto` never silently falls back to the
//! tree-walk interpreter over the **declared supported surface**.
//!
//! The supported surface is enumerated in
//! [`relon_test_harness::ledger::SUPPORTED_SURFACE`] — one `Covered` row
//! per R1–R9 (and base-envelope) construct, each naming a corpus case
//! that lowers cleanly. For every such row this test:
//!
//! 1. looks the corpus case up in [`relon_test_harness::corpus`],
//! 2. runs it through `Backend::Auto` (the production landing surface),
//! 3. inspects the `AutoEvaluator`'s recorded dispatch route, and
//! 4. asserts `auto` reached the **compiled** backend
//!    ([`DispatchRoute::Aot`]) rather than the silent capability fallback
//!    ([`DispatchRoute::UnsupportedFallback`]).
//!
//! It also bit-checks the produced value against the tree-walk oracle, so
//! a "compiled backend ran but produced the wrong value" regression is
//! caught here too — not just the fallback.
//!
//! ## Trivial-scalar-`#main` perf-route carve-out
//!
//! `auto` has a *performance* short-circuit ([`DispatchRoute::TrivialMain`])
//! that routes a trivial scalar `#main` (single scalar param + a
//! literal/arith body) straight to the tree-walker, skipping the
//! cranelift cold-start. That is NOT a capability fallback — the compiled
//! backend would handle the shape fine; it is simply faster not to. So
//! when a supported-surface case happens to be classified trivial, this
//! test does **not** treat `TrivialMain` as a fallback: it asserts
//! value-equality with the oracle and moves on. (In practice the
//! supported-surface cases are all non-trivial — lists / closures /
//! fn-calls / matches / where — so they take the `Aot` route; the
//! carve-out is defensive against a future trivial case being added.)
//!
//! ## How "did it fall back" is detected
//!
//! Detection is via the always-compiled observability hook
//! [`relon::AutoEvaluator::last_dispatch_route`]: `run_main` records which
//! arm it took with a single relaxed store. The production fallback arm
//! records [`DispatchRoute::UnsupportedFallback`]; a clean compiled run
//! records [`DispatchRoute::Aot`]. The hook is observation-only — it
//! never influences dispatch — so reading it in the test does not perturb
//! the behaviour it measures.

use std::collections::HashMap;

use relon::{AutoEvaluator, DispatchRoute, Evaluator};
use relon_eval_api::Value;
use relon_test_harness::corpus::{all_cases, CorpusCase};
use relon_test_harness::ledger::{Status, SUPPORTED_SURFACE};

/// Look a corpus case up by name. Panics with a clear message if the
/// supported-surface table names a case that no longer exists — that is a
/// ledger / corpus drift the bijection-style guards should never allow.
fn find_case(name: &str) -> CorpusCase {
    all_cases()
        .into_iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| {
            panic!(
                "SUPPORTED_SURFACE names corpus case `{name}` but it is absent from \
                 corpus::all_cases() — fix the ledger row or restore the case"
            )
        })
}

/// Tree-walk oracle value for `(source, args)`. The oracle must succeed
/// or trap; a trap is returned as `Err` so the caller can compare against
/// the auto run (both should trap equivalently).
fn oracle(source: &str, args: HashMap<String, Value>) -> Result<Value, String> {
    let ev = relon::new_evaluator(source, relon::Backend::TreeWalk)
        .unwrap_or_else(|e| panic!("tree-walk setup failed for oracle: {e}"));
    ev.run_main(args).map_err(|e| format!("{e:?}"))
}

#[test]
fn supported_surface_is_all_covered() {
    // Sanity: the table is the *supported* surface, so every row must be
    // Covered. A Capped row here would be a category error.
    for e in SUPPORTED_SURFACE {
        assert_eq!(
            e.status,
            Status::Covered,
            "SUPPORTED_SURFACE row `{}` is not Covered — it does not belong here",
            e.construct
        );
        assert!(
            !e.corpus.is_empty(),
            "SUPPORTED_SURFACE row `{}` has no corpus case",
            e.construct
        );
    }
    assert!(
        !SUPPORTED_SURFACE.is_empty(),
        "supported surface is empty — the capstone proves nothing"
    );
}

#[test]
fn no_fallback_over_supported_surface() {
    // Each row's corpus case must, through `Backend::Auto`, reach the
    // compiled backend (or the trivial-main perf route) and agree with
    // the tree-walk oracle — never the silent capability fallback.
    let mut fallbacks: Vec<String> = Vec::new();
    let mut host_errors: Vec<String> = Vec::new();

    for entry in SUPPORTED_SURFACE {
        let case = find_case(entry.corpus);
        let source = case.source;
        let args = (case.args_factory)();

        // Build the production auto evaluator directly so we can read its
        // dispatch route after the run.
        let auto = AutoEvaluator::new(source).unwrap_or_else(|e| {
            panic!(
                "auto setup failed for `{}` (case `{}`): {e}",
                entry.construct, entry.corpus
            )
        });

        let auto_result = auto.run_main(args.clone());
        let route = auto.last_dispatch_route();
        let oracle_result = oracle(source, args);

        match route {
            DispatchRoute::Aot => {
                // The compiled backend handled it — the whole point.
                // Cross-check the value/trap against the oracle.
                assert_results_agree(entry.construct, entry.corpus, &auto_result, &oracle_result);
            }
            DispatchRoute::TrivialMain => {
                // Perf carve-out: NOT a capability fallback. The compiled
                // backend could express this shape; auto just chose the
                // faster tree-walk for a trivial scalar `#main`. Assert
                // value-equality and accept the route.
                assert_results_agree(entry.construct, entry.corpus, &auto_result, &oracle_result);
            }
            DispatchRoute::UnsupportedFallback => {
                // This is the regression Wave R6 forbids: a construct on
                // the declared supported surface fell back to tree-walk
                // because the compiled backend declined the shape.
                fallbacks.push(format!(
                    "  - `{}` (case `{}`, {}): auto took the cranelift-AOT capability fallback",
                    entry.construct, entry.corpus, entry.wave
                ));
            }
            DispatchRoute::HostError => {
                host_errors.push(format!(
                    "  - `{}` (case `{}`): auto surfaced a host/setup error: {:?}",
                    entry.construct, entry.corpus, auto_result
                ));
            }
            DispatchRoute::None => {
                panic!(
                    "auto recorded no dispatch route for `{}` (case `{}`) — run_main did not run",
                    entry.construct, entry.corpus
                );
            }
        }
    }

    assert!(
        host_errors.is_empty(),
        "supported-surface cases surfaced host/setup errors through auto:\n{}",
        host_errors.join("\n")
    );
    assert!(
        fallbacks.is_empty(),
        "Wave R6 violation — auto silently fell back to tree-walk for \
         {} declared-supported construct(s):\n{}\n\nEither the construct \
         regressed out of the compiled backend (fix the lowering), or it \
         is no longer four-way and its SUPPORTED_SURFACE row should be \
         removed / re-Capped (honesty rule).",
        fallbacks.len(),
        fallbacks.join("\n")
    );
}

/// A deliberately-Capped construct must still fall back *cleanly* through
/// auto — taking the unsupported-shape arm, not the compiled `Aot` route —
/// and produce the correct value via the tree-walk oracle. This pins the
/// other half of the honesty contract: Capped shapes keep working, just
/// slower, and they keep falling back (the production behaviour Wave R6
/// does NOT change for genuinely-Capped shapes).
///
/// `is_ipv4` is a stable example of a genuinely-capped validator: it
/// routes through `core::net::Ipv4Addr::parse`, which has no
/// wasm-portable body, so it is recorded Capped (it is NOT on
/// [`SUPPORTED_SURFACE`]) and the cranelift backend declines the `#main`
/// shape, so `auto` falls back. (`is_email` / `is_uri` USED to be the
/// example here — both are now compiled four-way over byte-level ASCII
/// structure — and `is_iso_date` was the most recent capped example, now
/// compiled four-way too: its leap-year test uses `Op::Mod(I32)` against
/// non-zero constant divisors, with no UTF-8 decode and no trap.)
#[test]
fn capped_validator_falls_back_cleanly() {
    let source = "#main() -> Bool\nis_ipv4(\"192.168.0.1\")";
    let auto = AutoEvaluator::new(source)
        .unwrap_or_else(|e| panic!("auto setup unexpectedly failed for capped is_ipv4 case: {e}"));

    let auto_result = auto.run_main(HashMap::new());
    let route = auto.last_dispatch_route();

    // The expected, correct outcome: a clean capability fallback — NOT a
    // hard error and NOT a silent wrong value. (If a future lowering wave
    // teaches the compiled backend `is_ipv4`, this becomes `Aot`; flip
    // it to SUPPORTED_SURFACE then. Until then, fallback is the honest
    // state.)
    assert_eq!(
        route,
        DispatchRoute::UnsupportedFallback,
        "capped is_ipv4 case should take the clean unsupported-shape fallback, got {route:?}"
    );

    // The fallback value must still match the oracle bit-for-bit.
    let oracle_result = oracle(source, HashMap::new());
    assert_results_agree("capped is_ipv4", "<inline>", &auto_result, &oracle_result);
}

/// Honesty pin for the JSON-Schema predicate arms that intentionally stay
/// capped: a Float `multiple_of` (the `(Int, Float)` arm here — `Op::Mod(F64)`
/// has no native cranelift / wasm remainder, and the oracle's
/// `fract().abs() < 1e-9` tolerance has no four-way body) and a String
/// `size_in_range` (the oracle counts Unicode code points via
/// `chars().count()`, which needs the UTF-8 decode seam LLVM-native / wasm do
/// not lower). Both must take the clean unsupported-shape fallback — never a
/// silent wrong value — and still agree with the tree-walk oracle.
#[test]
fn capped_predicate_arms_fall_back_cleanly() {
    for source in [
        // Float multiple_of arm: 10 % 2.5 == 0 (oracle true), but the
        // compiled backend declines the Float operand.
        "#main() -> Bool\nmultiple_of(10, 2.5)",
        // String size_in_range arm: "abc" has 3 code points (oracle true).
        "#main() -> Bool\nsize_in_range(\"abc\", 1, 5)",
    ] {
        let auto = AutoEvaluator::new(source)
            .unwrap_or_else(|e| panic!("auto setup unexpectedly failed for `{source}`: {e}"));
        let auto_result = auto.run_main(HashMap::new());
        let route = auto.last_dispatch_route();
        assert_eq!(
            route,
            DispatchRoute::UnsupportedFallback,
            "capped arm `{source}` should take the clean unsupported-shape fallback, got {route:?}"
        );
        let oracle_result = oracle(source, HashMap::new());
        assert_results_agree("capped predicate arm", source, &auto_result, &oracle_result);
    }
}

/// Compare an auto run against the tree-walk oracle. Both-ok ⇒ values must
/// be bit-equal; both-err ⇒ accepted (both trapped); ok-vs-err ⇒ a hard
/// divergence.
fn assert_results_agree(
    construct: &str,
    case: &str,
    auto: &Result<Value, relon_eval_api::RuntimeError>,
    oracle: &Result<Value, String>,
) {
    match (auto, oracle) {
        (Ok(a), Ok(o)) => assert!(
            relon_test_harness::value_bit_eq(a, o),
            "value divergence for `{construct}` (case `{case}`):\n  auto   = {a:?}\n  oracle = {o:?}"
        ),
        (Err(_), Err(_)) => { /* both trapped — accepted (trap parity is the corpus harness's job) */
        }
        (Ok(a), Err(o)) => panic!(
            "`{construct}` (case `{case}`): auto produced a value but oracle trapped\n  auto   = {a:?}\n  oracle = {o}"
        ),
        (Err(a), Ok(o)) => panic!(
            "`{construct}` (case `{case}`): auto trapped but oracle produced a value\n  auto   = {a:?}\n  oracle = {o:?}"
        ),
    }
}
