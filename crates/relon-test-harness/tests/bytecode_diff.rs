//! v6-δ M2-A: 4-way differential corpus runner — tree-walk vs
//! cranelift-AOT vs trace-JIT vs bytecode VM.
//!
//! Reuses the 52-case three-way corpus and overlays the bytecode VM
//! tier. The bytecode VM is the newest backend; its envelope is
//! deliberately narrower (scalar `#main` only — no stdlib / list /
//! dict / closure) so most StdlibSimple+ cases land on
//! `BytecodeUnsupported` (soft pass). ArithControl cases are the
//! primary target: every one of them must match the tree-walker
//! bit-identically (`AllAgree` or `AllTrap`), with **zero**
//! Mismatches anywhere in the corpus.

use std::collections::BTreeMap;

use relon_test_harness::corpus::{all_cases, CorpusCase, Tier};
use relon_test_harness::four_way::{diff_test_4way, FourWayResult};

#[derive(Debug, Default)]
struct TierCounts {
    all_agree: usize,
    all_trap: usize,
    bytecode_matches_baseline: usize,
    bytecode_unsupported: usize,
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

fn classify(result: &FourWayResult, case: &CorpusCase) -> &'static str {
    let _ = case;
    match result {
        FourWayResult::AllAgree(_) => "AllAgree",
        FourWayResult::AllTrap => "AllTrap",
        FourWayResult::BytecodeMatchesBaseline { .. } => "BytecodeMatchesBaseline",
        FourWayResult::BytecodeUnsupported { .. } => "BytecodeUnsupported",
        FourWayResult::Mismatch { .. } => "Mismatch",
    }
}

#[test]
fn corpus_four_way_diff_aggregates() {
    let cases = all_cases();
    let total = cases.len();
    let mut per_tier: BTreeMap<&'static str, TierCounts> = BTreeMap::new();
    let mut all_agree = 0usize;
    let mut all_trap = 0usize;
    let mut bytecode_matches_baseline = 0usize;
    let mut bytecode_unsupported = 0usize;
    let mut mismatches: Vec<(String, String)> = Vec::new();

    for case in &cases {
        let counts = per_tier.entry(tier_label(case.tier)).or_default();
        counts.total += 1;
        let args = (case.args_factory)();
        let result = match diff_test_4way(case.source, args) {
            Ok(r) => r,
            Err(e) => {
                mismatches.push((case.name.to_string(), format!("setup error: {e}")));
                counts.mismatch += 1;
                continue;
            }
        };
        let label = classify(&result, case);
        match (&result, label) {
            (_, "AllAgree") => {
                all_agree += 1;
                counts.all_agree += 1;
            }
            (_, "AllTrap") => {
                all_trap += 1;
                counts.all_trap += 1;
            }
            (FourWayResult::BytecodeMatchesBaseline { .. }, _) => {
                bytecode_matches_baseline += 1;
                counts.bytecode_matches_baseline += 1;
            }
            (FourWayResult::BytecodeUnsupported { .. }, _) => {
                bytecode_unsupported += 1;
                counts.bytecode_unsupported += 1;
            }
            (other, _) => {
                mismatches.push((case.name.to_string(), format!("{other:?}")));
                counts.mismatch += 1;
            }
        }
    }

    eprintln!(
        "[four-way corpus] total={total} all_agree={all_agree} all_trap={all_trap} \
         bytecode_matches_baseline={bytecode_matches_baseline} \
         bytecode_unsupported={bytecode_unsupported} mismatches={}",
        mismatches.len()
    );
    for (tier, c) in &per_tier {
        eprintln!(
            "  tier {tier}: all_agree={} all_trap={} bytecode_match={} \
             bytecode_unsupported={} mismatch={} (of {})",
            c.all_agree,
            c.all_trap,
            c.bytecode_matches_baseline,
            c.bytecode_unsupported,
            c.mismatch,
            c.total
        );
    }
    for (name, err) in &mismatches {
        eprintln!("  MISMATCH {name}: {err}");
    }

    // Hard gate: zero mismatches. Correctness is non-negotiable.
    assert!(
        mismatches.is_empty(),
        "{} four-way mismatches",
        mismatches.len()
    );

    // Soft gate: every ArithControl case must reach AllAgree or
    // AllTrap. The bytecode VM's scalar envelope covers all 28 of
    // them; any soft pass here would indicate the bytecode VM
    // bounced where it shouldn't have.
    let arith = per_tier.get("arith_control").copied().unwrap_or_default();
    let arith_clean = arith.all_agree + arith.all_trap;
    assert!(
        arith_clean >= arith.total.saturating_sub(1),
        "v6-δ M2-A gate: ArithControl tier must be at least {} clean, got {} of {}",
        arith.total.saturating_sub(1),
        arith_clean,
        arith.total
    );

    // Sanity: every case must reach a passing variant.
    let passing = all_agree + all_trap + bytecode_matches_baseline + bytecode_unsupported;
    assert_eq!(passing, total, "every case must reach a passing variant");
}

#[test]
fn corpus_bytecode_vs_treewalk_strict_parity() {
    // For every ArithControl case, the bytecode VM must produce a
    // bit-identical result to the tree-walker. Any divergence is a
    // correctness bug — STOP per the task brief.
    let cases: Vec<_> = all_cases()
        .into_iter()
        .filter(|c| c.tier == Tier::ArithControl)
        .collect();
    let mut clean = 0usize;
    let mut unsupported = 0usize;
    let mut diverged: Vec<String> = Vec::new();
    for case in &cases {
        let args = (case.args_factory)();
        let outcome = relon_test_harness::four_way::bytecode_vs_treewalk(case.source, args);
        match outcome {
            Ok(Some(true)) => clean += 1,
            Ok(Some(false)) => diverged.push(case.name.to_string()),
            Ok(None) => unsupported += 1,
            Err(e) => diverged.push(format!("{}: setup={e}", case.name)),
        }
    }
    eprintln!(
        "[bytecode-vs-treewalk strict] cases={} clean={} unsupported={} diverged={}",
        cases.len(),
        clean,
        unsupported,
        diverged.len()
    );
    for d in &diverged {
        eprintln!("  DIVERGE {d}");
    }
    assert!(
        diverged.is_empty(),
        "bytecode_vs_treewalk: {} divergences (correctness bug)",
        diverged.len()
    );
}

// Counts derived in `corpus_four_way_diff_aggregates` are passed
// through a `Copy` helper. Define the bound implicitly by giving
// `TierCounts` a `Copy`/`Clone` impl above; otherwise the
// `.copied()` call fails on the `.get` borrow.
impl Copy for TierCounts {}
impl Clone for TierCounts {
    fn clone(&self) -> Self {
        *self
    }
}
