//! Negative-test coverage for the corpus support-claim ratchet.
//!
//! The corpus-driving tests (`corpus_differential`, `two_way_corpus`)
//! assert that every entry's `supported_by` list is
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
use relon_test_harness::ratchet;
use relon_test_harness::two_way::TwoWayResult;
use relon_test_harness::{BackendKind, DiffOutcome};

/// `DiffOutcome` driver: a case that claims `CraneliftAot` support but
/// the driver observed `CraneliftUnsupported` must surface as a
/// violation.
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

/// `DiffOutcome` driver: when the same `CraneliftUnsupported` outcome
/// arrives for a case that does *not* claim cranelift support, the
/// ratchet must stay silent (soft pass).
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

/// `TwoWayResult` driver: claiming `CraneliftAot` support but
/// observing `CraneliftUnsupported` is a regression.
#[test]
fn ratchet_two_way_result_cranelift_claim_violation() {
    let outcome = TwoWayResult::CraneliftUnsupported {
        tree_walk: Ok(Value::Int(7)),
        reason: "synthetic: analyzer rejected".to_string(),
    };
    let claim = [BackendKind::TreeWalk, BackendKind::CraneliftAot];
    let v = ratchet::check_two_way_result("probe_cr_result_claim", &outcome, &claim)
        .expect("expected violation");
    assert_eq!(v.backend, BackendKind::CraneliftAot);
}

/// `TwoWayResult` driver: `CraneliftUnsupported` for a case that does
/// not claim cranelift support is the canonical soft pass; the
/// ratchet must not fire.
#[test]
fn ratchet_two_way_result_cranelift_no_claim_no_violation() {
    let outcome = TwoWayResult::CraneliftUnsupported {
        tree_walk: Ok(Value::Int(7)),
        reason: "synthetic: analyzer rejected".to_string(),
    };
    let claim = [BackendKind::TreeWalk];
    assert!(
        ratchet::check_two_way_result("probe_cr_result_no_claim", &outcome, &claim).is_none(),
        "no claim => no violation"
    );
}

/// Sanity: an `Agree` outcome never fires the ratchet, regardless of
/// which backends are in the claim list.
#[test]
fn ratchet_all_agree_silent() {
    let outcome = TwoWayResult::Agree(Value::Int(123));
    let claim = [BackendKind::TreeWalk, BackendKind::CraneliftAot];
    assert!(
        ratchet::check_two_way_result("probe_agree", &outcome, &claim).is_none(),
        "Agree never produces a violation"
    );
}
