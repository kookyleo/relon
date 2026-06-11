//! Two-way differential corpus runner.
//!
//! Drives every entry in [`all_cases`] through [`diff_test_2way`] and
//! reports a per-tier breakdown of:
//!
//! - `Agree` — tree-walk and cranelift-AOT matched bitwise.
//! - `BothTrap` — both backends trapped equivalently.
//! - `CraneliftUnsupported` — cranelift-AOT bounced. The harness
//!   records this as a soft pass so future cranelift widening
//!   re-enters strict mode without a corpus rewrite.
//! - `Mismatch` — the backends disagreed. Hard failure.
//!
//! Every case must surface through a passing variant carrying an
//! explicit trap / unsupported reason — no silent skips.

use std::collections::BTreeMap;

use relon_test_harness::corpus::{all_cases, Tier};
use relon_test_harness::ratchet;
use relon_test_harness::two_way::{diff_test_2way, TwoWayResult};

#[derive(Debug, Default)]
struct TierCounts {
    agree: usize,
    both_trap: usize,
    cranelift_unsupported: usize,
    tree_walk_missing: usize,
    mismatch: usize,
    total: usize,
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

#[test]
fn corpus_two_way_diff_aggregates() {
    let cases = all_cases();
    let total = cases.len();
    let mut per_tier: BTreeMap<&'static str, TierCounts> = BTreeMap::new();
    let mut agree = 0usize;
    let mut both_trap = 0usize;
    let mut cranelift_unsupported = 0usize;
    let mut tree_walk_missing = 0usize;
    let mut mismatches: Vec<(String, String)> = Vec::new();
    let mut ratchet_violations = Vec::new();

    for case in &cases {
        let counts = per_tier.entry(tier_label(case.tier)).or_default();
        counts.total += 1;
        let args = (case.args_factory)();
        let result = match diff_test_2way(case.source, args) {
            Ok(r) => r,
            Err(e) => {
                mismatches.push((case.name.to_string(), format!("setup error: {e}")));
                counts.mismatch += 1;
                continue;
            }
        };
        if let Some(v) = ratchet::check_two_way_result(case.name, &result, case.supported_by) {
            ratchet_violations.push(v);
        }
        match &result {
            TwoWayResult::Agree(_) => {
                agree += 1;
                counts.agree += 1;
            }
            TwoWayResult::BothTrap => {
                both_trap += 1;
                counts.both_trap += 1;
            }
            TwoWayResult::CraneliftUnsupported { .. } => {
                cranelift_unsupported += 1;
                counts.cranelift_unsupported += 1;
            }
            TwoWayResult::TreeWalkMissingStdlibSurface { .. } => {
                tree_walk_missing += 1;
                counts.tree_walk_missing += 1;
            }
            other @ TwoWayResult::Mismatch { .. } => {
                mismatches.push((case.name.to_string(), format!("{other:?}")));
                counts.mismatch += 1;
            }
        }
    }

    eprintln!(
        "[two-way corpus] total={total} agree={agree} both_trap={both_trap} \
         cranelift_unsupported={cranelift_unsupported} \
         tree_walk_missing={tree_walk_missing} mismatches={}",
        mismatches.len()
    );
    for (tier, c) in &per_tier {
        eprintln!(
            "  tier {tier}: agree={} both_trap={} cranelift_unsupported={} \
             tree_walk_missing={} mismatch={} (of {})",
            c.agree, c.both_trap, c.cranelift_unsupported, c.tree_walk_missing, c.mismatch, c.total
        );
    }
    for (name, err) in &mismatches {
        eprintln!("  MISMATCH {name}: {err}");
    }

    assert!(
        mismatches.is_empty(),
        "{} two-way mismatches",
        mismatches.len()
    );

    if !ratchet_violations.is_empty() {
        for v in &ratchet_violations {
            eprintln!("RATCHET {v}");
        }
        panic!(
            "{} two-way ratchet violation(s): a claimed-support backend regressed to its fallback",
            ratchet_violations.len()
        );
    }

    // Gate: pinned from the measured corpus split (241 Agree +
    // 11 BothTrap + 4 CraneliftUnsupported of 256). The Agree floor
    // sits at the measured count so a case silently dropping out of
    // bit-agreement is caught; widening the corpus only ever raises
    // the measured number.
    assert!(
        agree >= 241,
        "two-way gate: expected >= 241 Agree, got {agree} (of {total})"
    );
    // Sanity: every case must land on a passing variant. Mismatch
    // is the only hard failure; we already panic above when any
    // exist, but pin the count here so an accidental regression to
    // `Mismatch` is obvious in the test output.
    let passing = agree + both_trap + cranelift_unsupported + tree_walk_missing;
    assert_eq!(passing, total, "every case must reach a passing variant");
}

#[test]
fn corpus_two_way_arith_tier_agree_or_trap() {
    // Strict gate: every ArithControl-tier case must produce `Agree`
    // (numeric / bool answer matches across both backends) or
    // `BothTrap` (div-by-zero etc.) — except the known
    // `CraneliftUnsupported` analyzer bounce (`let_chain`).
    let cases: Vec<_> = all_cases()
        .into_iter()
        .filter(|c| c.tier == Tier::ArithControl)
        .collect();
    let mut agree = 0usize;
    let mut both_trap = 0usize;
    let mut other = Vec::new();
    for case in &cases {
        let args = (case.args_factory)();
        match diff_test_2way(case.source, args).expect("setup") {
            TwoWayResult::Agree(_) => agree += 1,
            TwoWayResult::BothTrap => both_trap += 1,
            r => other.push((case.name.to_string(), format!("{r:?}"))),
        }
    }
    eprintln!(
        "[two-way arith] {} cases: {} Agree + {} BothTrap + {} other",
        cases.len(),
        agree,
        both_trap,
        other.len()
    );
    for (name, reason) in &other {
        eprintln!("  arith other: {name} -> {reason}");
    }
    // Pinned from the measured split: 23 Agree + 4 BothTrap, with
    // exactly 1 `CraneliftUnsupported` (`let_chain`, the cranelift
    // analyzer's forward-ref bounce) allowed through `other`.
    assert!(
        agree + both_trap >= 27,
        "ArithControl tier: expected >= 27 Agree+BothTrap, got {} (of {})",
        agree + both_trap,
        cases.len()
    );
    assert!(
        other.len() <= 1,
        "ArithControl tier: at most the let_chain cranelift bounce may sit outside \
         Agree/BothTrap; got {other:?}"
    );
}
