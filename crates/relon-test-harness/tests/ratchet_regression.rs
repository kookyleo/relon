//! Negative-test coverage for the corpus support-claim ratchet.
//!
//! The corpus-driving tests (`corpus_differential`, `three_way_corpus`,
//! `bytecode_diff`) assert that every entry's `supported_by` list is
//! honoured by the live harness. This file complements that with the
//! opposite direction: feed the ratchet a hand-crafted "claimed but
//! bounced" outcome and assert that it surfaces a violation. Without
//! the negative test, a bug that turned every ratchet check into a
//! no-op would let silent regressions slip back in.
//!
//! The tests construct soft-pass variants of the public outcome enums
//! directly — no live evaluator round-trips — so the regression cost
//! stays in microseconds rather than seconds.

use relon_eval_api::Value;
use relon_test_harness::four_way::FourWayResult;
use relon_test_harness::ratchet;
use relon_test_harness::three_way::ThreeWayResult;
use relon_test_harness::{BackendKind, DiffOutcome};

/// Two-way: a case that claims `CraneliftAot` support but the driver
/// observed `CraneliftUnsupported` must surface as a violation.
#[test]
fn ratchet_two_way_cranelift_claim_violation() {
    let outcome = DiffOutcome::CraneliftUnsupported {
        tree_walk: Ok(Value::Int(42)),
        reason: "synthetic: analyzer rejected".to_string(),
    };
    let claim = [BackendKind::TreeWalk, BackendKind::CraneliftAot];
    let v = ratchet::check_two_way("probe_cr_unsupported", &outcome, &claim)
        .expect("expected a ratchet violation");
    assert_eq!(v.case, "probe_cr_unsupported");
    assert_eq!(v.backend, BackendKind::CraneliftAot);
}

/// Two-way: when the same `CraneliftUnsupported` outcome arrives for
/// a case that does *not* claim cranelift support, the ratchet must
/// stay silent (soft pass).
#[test]
fn ratchet_two_way_cranelift_no_claim_no_violation() {
    let outcome = DiffOutcome::CraneliftUnsupported {
        tree_walk: Ok(Value::Int(42)),
        reason: "synthetic: analyzer rejected".to_string(),
    };
    let claim = [BackendKind::TreeWalk];
    assert!(
        ratchet::check_two_way("probe_cr_no_claim", &outcome, &claim).is_none(),
        "no claim => no violation"
    );
}

/// Three-way: claiming `TraceJit` support but observing
/// `TraceJitNotApplicable` is a regression.
#[test]
fn ratchet_three_way_trace_jit_claim_violation() {
    let outcome = ThreeWayResult::TraceJitNotApplicable {
        baseline: Value::Int(7),
        reason: "synthetic: recipe missing".to_string(),
    };
    let claim = [
        BackendKind::TreeWalk,
        BackendKind::CraneliftAot,
        BackendKind::TraceJit,
    ];
    let v =
        ratchet::check_three_way("probe_tj_claim", &outcome, &claim).expect("expected violation");
    assert_eq!(v.backend, BackendKind::TraceJit);
}

/// Three-way: `TraceJitNotApplicable` is the canonical fallback for
/// backends that don't claim trace-JIT; ratchet must not fire.
#[test]
fn ratchet_three_way_trace_jit_no_claim_no_violation() {
    let outcome = ThreeWayResult::TraceJitNotApplicable {
        baseline: Value::Int(7),
        reason: "synthetic: outside catalogue".to_string(),
    };
    let claim = [BackendKind::TreeWalk, BackendKind::CraneliftAot];
    assert!(
        ratchet::check_three_way("probe_tj_no_claim", &outcome, &claim).is_none(),
        "no claim => no violation"
    );
}

/// Four-way: `BytecodeUnsupported` against a claim list that includes
/// `Bytecode` must surface a violation.
#[test]
fn ratchet_four_way_bytecode_claim_violation() {
    let baseline = ThreeWayResult::AllAgree(Value::Int(5));
    let outcome = FourWayResult::BytecodeUnsupported {
        baseline: Box::new(baseline),
        reason: "synthetic: M2-A scaffold rejected String return".to_string(),
    };
    let claim = [
        BackendKind::TreeWalk,
        BackendKind::CraneliftAot,
        BackendKind::TraceJit,
        BackendKind::Bytecode,
    ];
    let violations = ratchet::check_four_way("probe_bc_claim", &outcome, &claim);
    assert_eq!(
        violations.len(),
        1,
        "expected exactly one violation, got {violations:?}"
    );
    assert_eq!(violations[0].backend, BackendKind::Bytecode);
}

/// Four-way: same outcome but the case does *not* claim Bytecode
/// support — ratchet stays silent. Mirrors a case like
/// `stdlib_concat_const` today.
#[test]
fn ratchet_four_way_bytecode_no_claim_no_violation() {
    let baseline = ThreeWayResult::AllAgree(Value::Int(5));
    let outcome = FourWayResult::BytecodeUnsupported {
        baseline: Box::new(baseline),
        reason: "synthetic: String return".to_string(),
    };
    let claim = [
        BackendKind::TreeWalk,
        BackendKind::CraneliftAot,
        BackendKind::TraceJit,
    ];
    let violations = ratchet::check_four_way("probe_bc_no_claim", &outcome, &claim);
    assert!(
        violations.is_empty(),
        "no claim => no violation, got {violations:?}"
    );
}

/// Four-way: `BytecodeMatchesBaseline` means bytecode was right but
/// the trace-JIT skipped. When the case claims `TraceJit`, the
/// ratchet fires for the trace-JIT tier.
#[test]
fn ratchet_four_way_trace_jit_skipped_under_claim() {
    let outcome = FourWayResult::BytecodeMatchesBaseline {
        value: Value::Int(15),
        trace_skip_reason: "trace_jit_outside_catalogue".to_string(),
    };
    let claim = [
        BackendKind::TreeWalk,
        BackendKind::CraneliftAot,
        BackendKind::TraceJit,
        BackendKind::Bytecode,
    ];
    let violations = ratchet::check_four_way("probe_tj_skip", &outcome, &claim);
    assert_eq!(
        violations.len(),
        1,
        "expected one violation for trace-JIT skip, got {violations:?}"
    );
    assert_eq!(violations[0].backend, BackendKind::TraceJit);
}

/// Four-way `BytecodeMatchesBaseline` with the `cranelift_unsupported`
/// reason prefix routes the violation to the cranelift tier when the
/// case claims cranelift.
#[test]
fn ratchet_four_way_cranelift_skip_routed_correctly() {
    let outcome = FourWayResult::BytecodeMatchesBaseline {
        value: Value::Int(15),
        trace_skip_reason: "cranelift_unsupported".to_string(),
    };
    let claim = [
        BackendKind::TreeWalk,
        BackendKind::CraneliftAot,
        BackendKind::Bytecode,
    ];
    let violations = ratchet::check_four_way("probe_cr_skip", &outcome, &claim);
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].backend, BackendKind::CraneliftAot);
}

/// Sanity: an `AllAgree` outcome never fires the ratchet, regardless
/// of which backends are in the claim list.
#[test]
fn ratchet_all_agree_silent() {
    let outcome = FourWayResult::AllAgree(Value::Int(123));
    let claim = [
        BackendKind::TreeWalk,
        BackendKind::CraneliftAot,
        BackendKind::TraceJit,
        BackendKind::Bytecode,
    ];
    let violations = ratchet::check_four_way("probe_agree", &outcome, &claim);
    assert!(
        violations.is_empty(),
        "AllAgree never produces a violation, got {violations:?}"
    );
}
