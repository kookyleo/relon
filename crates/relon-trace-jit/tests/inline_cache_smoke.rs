//! Polymorphic inline cache smoke tests.

use relon_trace_jit::{CacheResult, InlineCache, ObservedType};

#[test]
fn monomorphic_single_type_hits_repeatedly() {
    let ic: InlineCache<1> = InlineCache::new();
    assert_eq!(ic.check(ObservedType::I32), CacheResult::Miss);
    for _ in 0..100 {
        assert_eq!(ic.check(ObservedType::I32), CacheResult::Hit);
    }
    assert_eq!(ic.hit_count(), 100);
    assert_eq!(ic.miss_count(), 1);
}

#[test]
fn polymorphic_two_types_alternating_hits() {
    let ic: InlineCache<2> = InlineCache::new();
    // Seed both slots.
    assert_eq!(ic.check(ObservedType::I32), CacheResult::Miss);
    assert_eq!(ic.check(ObservedType::I64), CacheResult::Miss);
    // Now alternate.
    for _ in 0..20 {
        assert_eq!(ic.check(ObservedType::I32), CacheResult::Hit);
        assert_eq!(ic.check(ObservedType::I64), CacheResult::Hit);
    }
    assert_eq!(ic.hit_count(), 40);
    assert_eq!(ic.miss_count(), 2);
}

#[test]
fn megamorphic_four_types_all_hit() {
    let ic: InlineCache<4> = InlineCache::new();
    for ty in [
        ObservedType::I32,
        ObservedType::I64,
        ObservedType::F64,
        ObservedType::Bool,
    ] {
        assert_eq!(ic.check(ty), CacheResult::Miss);
    }
    // Now every type should hit.
    for ty in [
        ObservedType::Bool,
        ObservedType::F64,
        ObservedType::I64,
        ObservedType::I32,
    ] {
        assert_eq!(ic.check(ty), CacheResult::Hit);
    }
    assert_eq!(ic.miss_count(), 4);
    assert_eq!(ic.hit_count(), 4);
}

#[test]
fn miss_evicts_lru_when_full() {
    // N=2 cache; saturate, then a third type forces eviction of LRU.
    let ic: InlineCache<2> = InlineCache::new();
    ic.check(ObservedType::I32); // slot: [I32, None]
    ic.check(ObservedType::I64); // slot: [I64, I32]
                                 // I32 is now LRU. Bring in F64 -> evict I32.
    ic.check(ObservedType::F64); // slot: [F64, I64]
                                 // I32 should now miss again.
    assert_eq!(ic.check(ObservedType::I32), CacheResult::Miss);
    // I64 should hit (still in cache after F64 eviction).
    // Wait -- after I32 miss with [F64, I64], we evict LRU=I64
    // and store I32 -> slot: [I32, F64]. So I64 now misses.
    assert_eq!(ic.check(ObservedType::I64), CacheResult::Miss);
}

#[test]
fn empty_slot_preferred_over_existing_entry() {
    // N=4 with only two entries -- a third distinct type should
    // fill the next empty slot rather than evict an existing one.
    let ic: InlineCache<4> = InlineCache::new();
    ic.check(ObservedType::I32);
    ic.check(ObservedType::I64);
    ic.check(ObservedType::F64);
    // All three previously-inserted types should still hit.
    assert_eq!(ic.check(ObservedType::I32), CacheResult::Hit);
    assert_eq!(ic.check(ObservedType::I64), CacheResult::Hit);
    assert_eq!(ic.check(ObservedType::F64), CacheResult::Hit);
}

#[test]
fn hit_count_and_miss_count_track_independently() {
    let ic: InlineCache<2> = InlineCache::new();
    assert_eq!(ic.hit_count(), 0);
    assert_eq!(ic.miss_count(), 0);
    ic.check(ObservedType::I32); // miss
    ic.check(ObservedType::I32); // hit
    ic.check(ObservedType::I64); // miss
    ic.check(ObservedType::I64); // hit
    ic.check(ObservedType::I32); // hit
    assert_eq!(ic.miss_count(), 2);
    assert_eq!(ic.hit_count(), 3);
}

#[test]
fn reset_clears_state() {
    let ic: InlineCache<4> = InlineCache::new();
    ic.check(ObservedType::I32);
    ic.check(ObservedType::I64);
    ic.check(ObservedType::I32);
    assert!(ic.hit_count() > 0);
    ic.reset();
    assert_eq!(ic.hit_count(), 0);
    assert_eq!(ic.miss_count(), 0);
    // After reset, the previously-cached I32 should miss.
    assert_eq!(ic.check(ObservedType::I32), CacheResult::Miss);
}

#[test]
fn ptr_type_distinguished_from_integer_types() {
    let ic: InlineCache<2> = InlineCache::new();
    assert_eq!(ic.check(ObservedType::I64), CacheResult::Miss);
    // Ptr is NOT == I64 even though both occupy 8 bytes.
    assert_eq!(ic.check(ObservedType::Ptr), CacheResult::Miss);
    assert_eq!(ic.check(ObservedType::Ptr), CacheResult::Hit);
    assert_eq!(ic.check(ObservedType::I64), CacheResult::Hit);
}
