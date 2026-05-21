//! Hot-counter table used to decide when to start tracing.
//!
//! The design (doc §1.2) leaves the atomicity of the increment as a
//! "host decision". This scaffolding uses non-atomic [`Cell`] -- the
//! counter does not need to be precise across threads; an extra
//! recording trigger every now and then is fine, and avoiding a
//! `lock incl` on the fast path is significantly cheaper.
//!
//! Multi-threaded use is therefore safe in the sense that no UB will
//! arise from torn writes (a `Cell<u32>` is `!Sync`, so callers must
//! pin each counter table to a thread or wrap it in `thread_local!`).
//! Future v6-gamma work may switch to an `AtomicU32` per site if a
//! benchmark proves the contention cost is acceptable.

use std::cell::Cell;

use crate::trace_ir::SsaVar;

/// Sentinel value used to permanently mark a counter as "already
/// triggered". Picked at the top of the u32 range so it sticks out in
/// dumps and never collides with normal increment values.
pub const COUNTER_SATURATED: u32 = u32::MAX;

/// Outcome of a [`HotCounter::record`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordResult {
    /// Still well below the threshold.
    Cold,
    /// Below threshold but counter is climbing. Returns the new count.
    Heating(u32),
    /// Threshold just hit; caller should start a trace recording.
    HotTrigger,
    /// Counter was already saturated -- trace was triggered (and
    /// presumably installed) previously, so the caller must NOT
    /// retrigger.
    AlreadyHot,
}

/// Non-atomic per-site hot counter table.
///
/// Internal storage uses `Cell<u32>` to give cheap interior
/// mutability. The table is therefore `!Sync`; callers either run it
/// per-thread or wrap it in `thread_local!`.
pub struct HotCounter {
    counters: Box<[Cell<u32>]>,
    threshold: u32,
}

impl HotCounter {
    /// Create a new hot-counter table with `capacity` slots and a
    /// trigger threshold. The threshold defaults documented in the
    /// design are 10 (LuaJIT default).
    pub fn new(capacity: usize, threshold: u32) -> Self {
        assert!(threshold > 0, "threshold must be positive");
        let mut v = Vec::with_capacity(capacity);
        v.resize_with(capacity, || Cell::new(0));
        Self {
            counters: v.into_boxed_slice(),
            threshold,
        }
    }

    /// Convenience constructor with the design default threshold.
    pub fn with_default_threshold(capacity: usize) -> Self {
        Self::new(capacity, 10)
    }

    /// Number of counter slots.
    pub fn capacity(&self) -> usize {
        self.counters.len()
    }

    /// Configured trigger threshold.
    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Read a counter without modifying it (mainly for tests).
    pub fn peek(&self, fn_id: u32) -> u32 {
        self.counters[fn_id as usize].get()
    }

    /// Record one execution of `fn_id`.
    ///
    /// Semantics:
    /// 1. If the counter is `COUNTER_SATURATED`, return [`RecordResult::AlreadyHot`].
    /// 2. Otherwise increment the counter (saturating at `threshold`).
    /// 3. If the post-increment value reached the threshold, switch
    ///    the counter to `COUNTER_SATURATED` and return
    ///    [`RecordResult::HotTrigger`]. The next call for this slot
    ///    returns `AlreadyHot`.
    /// 4. If the new value is exactly 1 and the threshold is greater
    ///    than 1, return [`RecordResult::Cold`].
    /// 5. Else return [`RecordResult::Heating`]`(count)`.
    pub fn record(&self, fn_id: u32) -> RecordResult {
        let slot = &self.counters[fn_id as usize];
        let cur = slot.get();
        if cur == COUNTER_SATURATED {
            return RecordResult::AlreadyHot;
        }
        let next = cur.saturating_add(1);
        if next >= self.threshold {
            slot.set(COUNTER_SATURATED);
            return RecordResult::HotTrigger;
        }
        slot.set(next);
        if next == 1 {
            RecordResult::Cold
        } else {
            RecordResult::Heating(next)
        }
    }

    /// Convenience: record by `SsaVar` (handy if callers index by
    /// function-entry ssa id during tests).
    pub fn record_var(&self, var: SsaVar) -> RecordResult {
        self.record(var.raw())
    }

    /// Reset a counter back to zero. Mainly useful for tests; in
    /// production we usually leave saturated slots saturated.
    pub fn reset(&self, fn_id: u32) {
        self.counters[fn_id as usize].set(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_triggers_exactly_at_n() {
        let hc = HotCounter::new(2, 3);
        assert_eq!(hc.record(0), RecordResult::Cold);
        assert_eq!(hc.record(0), RecordResult::Heating(2));
        assert_eq!(hc.record(0), RecordResult::HotTrigger);
        // Now saturated.
        assert_eq!(hc.record(0), RecordResult::AlreadyHot);
        assert_eq!(hc.peek(0), COUNTER_SATURATED);
    }

    #[test]
    fn separate_slots_are_independent() {
        let hc = HotCounter::new(4, 2);
        assert_eq!(hc.record(2), RecordResult::Cold);
        assert_eq!(hc.record(3), RecordResult::Cold);
        assert_eq!(hc.record(2), RecordResult::HotTrigger);
        assert_eq!(hc.record(3), RecordResult::HotTrigger);
    }

    #[test]
    fn reset_restarts_counter() {
        let hc = HotCounter::new(1, 2);
        hc.record(0);
        hc.record(0); // triggers, saturates
        assert_eq!(hc.record(0), RecordResult::AlreadyHot);
        hc.reset(0);
        assert_eq!(hc.record(0), RecordResult::Cold);
    }

    #[test]
    #[should_panic]
    fn threshold_zero_panics() {
        HotCounter::new(1, 0);
    }
}
