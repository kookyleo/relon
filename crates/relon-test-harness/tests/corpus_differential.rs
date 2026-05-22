//! Differential test runner — for each corpus case, run both
//! backends and assert `DiffOutcome != Mismatch`.
//!
//! Cases the cranelift backend doesn't yet handle surface as
//! `CraneliftUnsupported`, which is *not* a test failure today —
//! it documents work remaining for the next tranche. The runner
//! prints a per-tier summary so the report at the bottom shows
//! what's covered.

use relon_test_harness::corpus::{all_cases, Tier};
use relon_test_harness::{diff_test, ratchet, DiffOutcome};

#[test]
fn corpus_runs_through_both_backends_without_mismatch() {
    let cases = all_cases();
    let total = cases.len();

    let mut match_ok = 0usize;
    let mut match_trap = 0usize;
    let mut unsupported = 0usize;
    let mut tw_missing = 0usize;
    let mut per_tier_supported: std::collections::BTreeMap<&'static str, (usize, usize)> =
        std::collections::BTreeMap::new();

    let mut failures: Vec<(String, String)> = Vec::new();
    let mut ratchet_violations = Vec::new();

    for case in &cases {
        let tier_label = tier_label(case.tier);
        let counts = per_tier_supported.entry(tier_label).or_default();
        counts.1 += 1; // total per tier

        let args = (case.args_factory)();
        match diff_test(case.source, args) {
            Ok(outcome) => {
                if let Some(v) = ratchet::check_two_way(case.name, &outcome, case.supported_by) {
                    ratchet_violations.push(v);
                }
                match outcome {
                    DiffOutcome::MatchOk => {
                        match_ok += 1;
                        counts.0 += 1;
                    }
                    DiffOutcome::MatchTrap => {
                        match_trap += 1;
                        counts.0 += 1;
                    }
                    DiffOutcome::CraneliftUnsupported { .. } => {
                        unsupported += 1;
                    }
                    DiffOutcome::TreeWalkMissingStdlibSurface { .. } => {
                        // Cranelift is correct; tree-walker lags on the
                        // free-function stdlib surface. Soft-counted so the
                        // tier breakdown still credits cranelift coverage.
                        tw_missing += 1;
                        counts.0 += 1;
                    }
                }
            }
            Err(e) => {
                failures.push((case.name.to_string(), format!("{e}")));
            }
        }
    }

    eprintln!(
        "Differential corpus: {total} cases / {match_ok} match_ok / {match_trap} match_trap / {unsupported} cranelift_unsupported / {tw_missing} tree_walk_missing / {} mismatch",
        failures.len()
    );
    for (tier, (passed, tot)) in &per_tier_supported {
        eprintln!("  tier {tier}: {passed}/{tot} on cranelift");
    }

    if !failures.is_empty() {
        for (name, err) in &failures {
            eprintln!("FAIL {name}: {err}");
        }
        panic!("{} differential corpus mismatches", failures.len());
    }

    if !ratchet_violations.is_empty() {
        for v in &ratchet_violations {
            eprintln!("RATCHET {v}");
        }
        panic!(
            "{} ratchet violation(s): a claimed-support backend regressed to its fallback",
            ratchet_violations.len()
        );
    }
}

#[test]
fn corpus_arith_tier_must_match() {
    // Strict-mode gate for the tier the cranelift backend covers
    // post-v5-β-2 buffer-protocol wiring. Every ArithControl case
    // must produce either `MatchOk` or `MatchTrap`. Cases that the
    // *analyzer* (not the codegen) rejects upstream are tolerated as
    // `CraneliftUnsupported` because the cranelift surface can't
    // observe them — they're tree-walk-only by construction.
    let cases: Vec<_> = all_cases()
        .into_iter()
        .filter(|c| c.tier == Tier::ArithControl)
        .collect();

    for case in &cases {
        let args = (case.args_factory)();
        match diff_test(case.source, args) {
            Ok(DiffOutcome::MatchOk) | Ok(DiffOutcome::MatchTrap) => {}
            Ok(DiffOutcome::TreeWalkMissingStdlibSurface { .. }) => {
                // Cranelift produced a valid answer; tree-walker lags
                // on the free-function stdlib surface (e.g. `abs(x)`
                // resolves through the IR pipeline but not through the
                // AST evaluator). Accepted on the strict gate.
            }
            Ok(DiffOutcome::CraneliftUnsupported { reason, .. }) => {
                // Analyzer rejects (e.g. forward-references in
                // `where` chains) emit a CraneliftError::Analyze
                // surface that the harness wraps as
                // `CraneliftUnsupported`. The tree-walker side
                // doesn't go through the analyzer's strict pass, so
                // it succeeds where the IR pipeline can't reach.
                // These cases are documented as "analyzer-only" and
                // accepted on the strict gate.
                if reason.contains("analyzer reported") {
                    continue;
                }
                panic!(
                    "ArithControl tier regression on case `{}`: cranelift surfaced unsupported \
                     ({reason}). Strict gate expects `MatchOk` / `MatchTrap`.",
                    case.name
                );
            }
            Err(e) => panic!("ArithControl tier case `{}` mismatch: {e}", case.name),
        }
    }
}

fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::ArithControl => "arith_control",
        Tier::StdlibSimple => "stdlib_simple",
        Tier::StdlibMemory => "stdlib_memory",
        Tier::StdlibCaseFold => "stdlib_case_fold",
        Tier::StdlibList => "stdlib_list",
        Tier::StdlibNormalize => "stdlib_normalize",
        Tier::DictReturn => "dict_return",
        Tier::Closure => "closure",
    }
}
