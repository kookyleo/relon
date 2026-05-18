//! HotCounter smoke tests.

use relon_trace_jit::{HotCounter, RecordResult, COUNTER_SATURATED};

#[test]
fn first_record_returns_cold() {
    let hc = HotCounter::new(4, 5);
    assert_eq!(hc.record(0), RecordResult::Cold);
}

#[test]
fn intermediate_records_heating() {
    let hc = HotCounter::new(1, 5);
    assert_eq!(hc.record(0), RecordResult::Cold);
    assert_eq!(hc.record(0), RecordResult::Heating(2));
    assert_eq!(hc.record(0), RecordResult::Heating(3));
    assert_eq!(hc.record(0), RecordResult::Heating(4));
    assert_eq!(hc.record(0), RecordResult::HotTrigger);
}

#[test]
fn after_trigger_counter_is_saturated() {
    let hc = HotCounter::new(1, 2);
    hc.record(0);
    let res = hc.record(0);
    assert_eq!(res, RecordResult::HotTrigger);
    assert_eq!(hc.peek(0), COUNTER_SATURATED);
}

#[test]
fn already_hot_is_idempotent() {
    let hc = HotCounter::new(1, 2);
    hc.record(0);
    hc.record(0); // trigger
    for _ in 0..10 {
        assert_eq!(hc.record(0), RecordResult::AlreadyHot);
    }
}

#[test]
fn default_threshold_is_10() {
    let hc = HotCounter::with_default_threshold(1);
    assert_eq!(hc.threshold(), 10);
}

#[test]
fn capacity_reflects_constructor() {
    let hc = HotCounter::new(7, 3);
    assert_eq!(hc.capacity(), 7);
}

#[test]
fn threshold_of_one_triggers_immediately() {
    let hc = HotCounter::new(1, 1);
    assert_eq!(hc.record(0), RecordResult::HotTrigger);
}
