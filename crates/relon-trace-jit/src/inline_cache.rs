//! Polymorphic inline cache (PIC) for type-check guards.
//!
//! When the type-specialisation pass inserts a `Guard(TypeCheck(var,
//! ty))` op, the v6-gamma codegen has two choices for the runtime
//! check:
//!
//! - Always invoke the host's expensive `observed_type_of(var)`
//!   lookup, and compare against the recorded `ObservedType`.
//! - Or, emit a small *inline cache* slot adjacent to the guard. Each
//!   trace execution first compares the new observation against the
//!   N most recently seen types; on a hit the costly lookup is
//!   skipped, on a miss the new observation is recorded (evicting
//!   the LRU slot) and the slow path runs.
//!
//! This module provides the data structure. `N` is a compile-time
//! `const` generic; the type-spec pass picks `N=2` by default
//! (mildly polymorphic), but `N=1` (monomorphic) and `N=4`
//! (megamorphic) are also available for tuning. Higher `N` costs
//! more guard slots at no asymptotic benefit -- benchmarks decide.
//!
//! ## Concurrency
//!
//! The cache lives inline next to the compiled trace. Per design
//! note §1.4, traces are not shared between threads (each thread
//! owns its own JIT-compiled buffer), so `Cell<...>` interior
//! mutability suffices and we avoid the cost of atomics. If we
//! later share traces, this struct needs to switch to
//! `AtomicU64`-packed slots.
//!
//! ## LRU policy
//!
//! Slot 0 is most-recently-used. On a hit at index `i`, slots `0..i`
//! shift to `1..=i`, and the hit value lands in slot 0. On a miss,
//! the LRU slot (last `Some` or `None` -- prefer empty) is overwritten
//! at slot 0 and the rest shift down. This is a textbook LRU done
//! cheaply on tiny `N`.

use std::cell::Cell;

use crate::trace_ir::ObservedType;

/// Outcome of `InlineCache::check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheResult {
    /// Observation matched one of the cached types. The slow-path
    /// guard check can be skipped.
    Hit,
    /// Observation didn't match. Caller must run the slow path and
    /// (typically) deopt or update its trace if the new type is
    /// incompatible.
    Miss,
}

/// Compile-time-sized inline cache.
///
/// Common choices:
/// - `InlineCache<1>` -- monomorphic. Smallest, fastest on a hit.
/// - `InlineCache<2>` -- bimorphic / mildly polymorphic. Default.
/// - `InlineCache<4>` -- megamorphic. Beyond this size, fall back
///   to the slow path unconditionally; an unbounded cache defeats
///   the purpose.
#[derive(Debug)]
pub struct InlineCache<const N: usize> {
    cache: Cell<[Option<ObservedType>; N]>,
    hit_count: Cell<u32>,
    miss_count: Cell<u32>,
}

impl<const N: usize> Default for InlineCache<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> InlineCache<N> {
    /// Build an empty cache. All slots start as `None`.
    pub fn new() -> Self {
        Self {
            cache: Cell::new([None; N]),
            hit_count: Cell::new(0),
            miss_count: Cell::new(0),
        }
    }

    /// Probe the cache for `observed`. On hit, promote the hit
    /// slot to MRU. On miss, evict the LRU slot and install
    /// `observed` at MRU. Returns whether the lookup hit.
    pub fn check(&self, observed: ObservedType) -> CacheResult {
        let mut slots = self.cache.get();

        // Look for a match.
        let hit_idx = slots
            .iter()
            .position(|s| s.map(|t| t == observed).unwrap_or(false));

        match hit_idx {
            Some(0) => {
                // Already MRU, no reshuffle needed.
                self.hit_count.set(self.hit_count.get().saturating_add(1));
                self.cache.set(slots);
                CacheResult::Hit
            }
            Some(i) => {
                // Promote: shift 0..i down by one, place hit at 0.
                let hit_val = slots[i];
                for j in (1..=i).rev() {
                    slots[j] = slots[j - 1];
                }
                slots[0] = hit_val;
                self.cache.set(slots);
                self.hit_count.set(self.hit_count.get().saturating_add(1));
                CacheResult::Hit
            }
            None => {
                // Miss: pick eviction target. Prefer the first
                // empty slot from the back; otherwise the very
                // last slot (LRU).
                let evict_at = slots
                    .iter()
                    .rposition(|s| s.is_none())
                    .unwrap_or(N.saturating_sub(1));
                // Shift 0..evict_at down by one to make room at 0.
                for j in (1..=evict_at).rev() {
                    slots[j] = slots[j - 1];
                }
                slots[0] = Some(observed);
                self.cache.set(slots);
                self.miss_count.set(self.miss_count.get().saturating_add(1));
                CacheResult::Miss
            }
        }
    }

    /// Number of hits since construction.
    pub fn hit_count(&self) -> u32 {
        self.hit_count.get()
    }

    /// Number of misses since construction.
    pub fn miss_count(&self) -> u32 {
        self.miss_count.get()
    }

    /// Snapshot of current slot contents. Slot 0 is MRU.
    pub fn slots(&self) -> [Option<ObservedType>; N] {
        self.cache.get()
    }

    /// Reset all slots and counters. Useful when a trace is
    /// re-recorded; the deopt machinery is otherwise the source of
    /// truth and shouldn't peek inside.
    pub fn reset(&self) {
        self.cache.set([None; N]);
        self.hit_count.set(0);
        self.miss_count.set(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monomorphic_steady_state_hits() {
        let ic: InlineCache<1> = InlineCache::new();
        assert_eq!(ic.check(ObservedType::I32), CacheResult::Miss);
        for _ in 0..10 {
            assert_eq!(ic.check(ObservedType::I32), CacheResult::Hit);
        }
        assert_eq!(ic.hit_count(), 10);
        assert_eq!(ic.miss_count(), 1);
    }

    #[test]
    fn monomorphic_type_change_misses_then_settles() {
        let ic: InlineCache<1> = InlineCache::new();
        assert_eq!(ic.check(ObservedType::I32), CacheResult::Miss);
        assert_eq!(ic.check(ObservedType::I64), CacheResult::Miss);
        // I32 was evicted.
        assert_eq!(ic.check(ObservedType::I32), CacheResult::Miss);
        assert_eq!(ic.miss_count(), 3);
    }

    #[test]
    fn slots_layout_after_promotion() {
        let ic: InlineCache<2> = InlineCache::new();
        ic.check(ObservedType::I32);
        ic.check(ObservedType::I64);
        // After two distinct misses: slots = [I64, I32].
        assert_eq!(
            ic.slots(),
            [Some(ObservedType::I64), Some(ObservedType::I32)]
        );
        // Hit I32 -> promoted to slot 0.
        assert_eq!(ic.check(ObservedType::I32), CacheResult::Hit);
        assert_eq!(
            ic.slots(),
            [Some(ObservedType::I32), Some(ObservedType::I64)]
        );
    }

    #[test]
    fn empty_slot_preferred_over_lru() {
        let ic: InlineCache<4> = InlineCache::new();
        ic.check(ObservedType::I32); // -> [I32, None, None, None]
        ic.check(ObservedType::I64); // -> [I64, I32, None, None]
                                     // Next miss should fill the next empty slot, NOT evict I32.
        ic.check(ObservedType::F64); // -> [F64, I64, I32, None]
        assert_eq!(
            ic.slots(),
            [
                Some(ObservedType::F64),
                Some(ObservedType::I64),
                Some(ObservedType::I32),
                None,
            ]
        );
    }
}
