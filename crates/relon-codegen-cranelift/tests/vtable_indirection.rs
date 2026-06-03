//! v5-γ stage 2 vtable-indirection smoke tests.
//!
//! Covers:
//! 1. `VtableSlot` layout invariants stay stable across builds.
//! 2. `populate_vtable` materialises three non-null host helper
//!    pointers in slot order.
//! 3. Cached cold-start through `from_cache_dir` actually dispatches
//!    a host helper (`relon_now`) through the dlopen'd ET_DYN — i.e.
//!    the deadline guard inside `run_main` reads the wall clock via
//!    the vtable and round-trips correctly.

use std::collections::HashMap;

use relon_codegen_cranelift::vtable::{populate_vtable, VtableSlot, VTABLE_BYTES, VTABLE_SYMBOL};
use relon_codegen_cranelift::AotEvaluator;
use relon_eval_api::{Evaluator, Value};
use tempfile::tempdir;

#[test]
fn vtable_layout_matches_slot_count() {
    // 5 active slots × 8-byte pointer = 40 bytes used; the reserved
    // section is 32 slots so we have headroom for new helpers.
    assert_eq!(VtableSlot::COUNT, 5);
    let active_bytes: usize = VtableSlot::COUNT as usize * 8;
    assert!(VTABLE_BYTES >= active_bytes);
    assert_eq!(VtableSlot::RelonNow.offset_bytes(), 0);
    assert_eq!(VtableSlot::RelonRaiseTrap.offset_bytes(), 8);
    assert_eq!(VtableSlot::RelonCapLookup.offset_bytes(), 16);
    assert_eq!(VtableSlot::RelonGlobMatch.offset_bytes(), 24);
    assert_eq!(VtableSlot::RelonCallNative.offset_bytes(), 32);
    assert_eq!(VTABLE_SYMBOL, "__relon_capability_vtable");
}

#[test]
fn populate_vtable_yields_three_non_null_pointers() {
    let mut buf = [0u8; VTABLE_BYTES];
    unsafe { populate_vtable(buf.as_mut_ptr()) };
    let slots = buf.as_ptr() as *const *const u8;
    unsafe {
        assert!(!(*slots.add(0)).is_null(), "RelonNow slot");
        assert!(!(*slots.add(1)).is_null(), "RelonRaiseTrap slot");
        assert!(!(*slots.add(2)).is_null(), "RelonCapLookup slot");
        assert!(!(*slots.add(3)).is_null(), "RelonGlobMatch slot");
        assert!(!(*slots.add(4)).is_null(), "RelonCallNative slot");
    }
}

#[test]
fn cached_cold_start_dispatches_now_helper_through_vtable() {
    // End-to-end: drive `from_source_with_cache` once to populate
    // the on-disk cache triple, then `from_cache_dir` to round-trip
    // through dlopen + vtable populate. The body emits a deadline
    // guard whose prologue calls `relon_now(state)` via the vtable;
    // a working `run_main` result proves the dlopen'd ET_DYN
    // resolved the vtable correctly.
    let cache = tempdir().expect("tempdir");
    let src = "#main(Int x, Int y) -> Int\nx + y";

    let warm =
        AotEvaluator::from_source_with_cache(src, cache.path()).expect("from_source_with_cache");
    // Sanity: the warm evaluator answers correctly.
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(7));
    args.insert("y".to_string(), Value::Int(5));
    let warm_out = warm.run_main(args.clone()).expect("warm run_main");
    assert_eq!(warm_out, Value::Int(12));
    drop(warm);

    // Cache hit -> dlopen-exec path.
    let opt = AotEvaluator::from_cache_dir(src, cache.path()).expect("from_cache_dir result");
    let cached = opt.expect("cache hit");
    let cached_out = cached.run_main(args).expect("cached run_main");
    assert_eq!(cached_out, Value::Int(12));
}

#[test]
fn cached_cold_start_supports_subtraction() {
    // A second body shape so we exercise more than the +
    // case-specific code path. Same dlopen-exec wiring underneath.
    let cache = tempdir().expect("tempdir");
    let src = "#main(Int a, Int b) -> Int\na - b";

    let warm = AotEvaluator::from_source_with_cache(src, cache.path()).expect("warm");
    drop(warm);

    let cached = AotEvaluator::from_cache_dir(src, cache.path())
        .expect("from_cache_dir")
        .expect("cache hit");
    let mut args = HashMap::new();
    args.insert("a".to_string(), Value::Int(10));
    args.insert("b".to_string(), Value::Int(3));
    assert_eq!(cached.run_main(args).expect("run_main"), Value::Int(7));
}

#[test]
fn cached_cold_start_traps_on_division_by_zero() {
    // The trap-block prologue routes through
    // `RelonRaiseTrap` slot. A divide-by-zero proves the trap path
    // is wired through the vtable rather than a direct symbol
    // reference (which would fail to resolve in the dlopen'd ET_DYN).
    let cache = tempdir().expect("tempdir");
    let src = "#main(Int n, Int d) -> Int\nn / d";

    let warm = AotEvaluator::from_source_with_cache(src, cache.path()).expect("warm");
    drop(warm);

    let cached = AotEvaluator::from_cache_dir(src, cache.path())
        .expect("from_cache_dir")
        .expect("cache hit");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(10));
    args.insert("d".to_string(), Value::Int(0));
    let err = cached.run_main(args).expect_err("div-by-zero should trap");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("divi")
            || msg.to_lowercase().contains("trap")
            || msg.to_lowercase().contains("zero"),
        "expected div-by-zero error, got: {msg}"
    );
}
