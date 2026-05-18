//! Differential test runner — for each corpus case, run both
//! backends and assert `DiffOutcome != Mismatch`.
//!
//! Cases the cranelift backend doesn't yet handle surface as
//! `CraneliftUnsupported`, which is *not* a test failure today —
//! it documents work remaining for the next tranche. The runner
//! prints a per-tier summary so the report at the bottom shows
//! what's covered.

use relon_test_harness::corpus::{all_cases, Tier};
use relon_test_harness::{diff_test, DiffOutcome};

#[test]
fn corpus_runs_through_both_backends_without_mismatch() {
    let cases = all_cases();
    let total = cases.len();

    let mut match_ok = 0usize;
    let mut match_trap = 0usize;
    let mut unsupported = 0usize;
    let mut per_tier_supported: std::collections::BTreeMap<&'static str, (usize, usize)> =
        std::collections::BTreeMap::new();

    let mut failures: Vec<(String, String)> = Vec::new();

    for case in &cases {
        let tier_label = tier_label(case.tier);
        let counts = per_tier_supported.entry(tier_label).or_default();
        counts.1 += 1; // total per tier

        let args = (case.args_factory)();
        match diff_test(case.source, args) {
            Ok(DiffOutcome::MatchOk) => {
                match_ok += 1;
                counts.0 += 1;
            }
            Ok(DiffOutcome::MatchTrap) => {
                match_trap += 1;
                counts.0 += 1;
            }
            Ok(DiffOutcome::CraneliftUnsupported { .. }) => {
                unsupported += 1;
            }
            Err(e) => {
                failures.push((case.name.to_string(), format!("{e}")));
            }
        }
    }

    eprintln!(
        "Differential corpus: {total} cases / {match_ok} match_ok / {match_trap} match_trap / {unsupported} cranelift_unsupported / {} mismatch",
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
}

#[test]
#[ignore = "becomes the strict-mode gate once v5-β-2 wires cranelift::from_source to cover ArithControl"]
fn corpus_arith_tier_must_match() {
    // Strict-mode probe for the tier the cranelift backend
    // already covers. Today cranelift::from_source produces buffer-
    // protocol IR shapes the backend can't lower; the lowering
    // pipeline emits `I32` handshake params instead of the `I64`
    // user params the backend expects. Once item #9 (full
    // from_source pipeline) lands, unignore this test — every case
    // in the ArithControl tier must transition from
    // `CraneliftUnsupported` to `MatchOk`.
    let cases: Vec<_> = all_cases()
        .into_iter()
        .filter(|c| c.tier == Tier::ArithControl)
        .collect();

    for case in &cases {
        let args = (case.args_factory)();
        match diff_test(case.source, args) {
            Ok(DiffOutcome::MatchOk) | Ok(DiffOutcome::MatchTrap) => {}
            Ok(DiffOutcome::CraneliftUnsupported { reason, .. }) => {
                panic!(
                    "ArithControl tier regression on case `{}`: cranelift surfaced unsupported \
                     ({reason}). This case is expected to pass `MatchOk` on the current envelope.",
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
