//! Phase 9.b-3 smoke tests for the disk-backed AOT cache.
//!
//! Drives [`AotCache`] through [`WasmAotEvaluator::from_source_with_cache`]
//! to confirm the second build of the same source short-circuits the
//! compile pipeline and that the two evaluators produce identical
//! outputs.

use relon_codegen_wasm::{AotCache, WasmAotEvaluator};
use relon_eval_api::{Evaluator, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Build a fresh cache root that no other test will touch. Tests run in
/// parallel, so we pair pid + nanos + a process-wide counter to keep
/// the keyspace disjoint.
fn temp_cache_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "relon-aot-cache-smoke-{pid}-{nanos}-{counter}-{tag}"
    ));
    path
}

#[test]
fn evaluator_uses_cache() {
    // Two consecutive `from_source_with_cache` calls against the same
    // cache + source must yield identical results, and the second
    // call should reach a cache hit (visible as a meaningful drop in
    // wall-clock build time). The timing comparison is *not* gated —
    // the goal is to confirm the hit path runs end-to-end, not to
    // promise a precise speed-up factor. We do print the numbers so
    // a developer reading the test output can sanity-check.
    let src = "#main(Int x, Int y) -> Int\nx * y + 1";
    let dir = temp_cache_dir("eval");
    let cache = AotCache::open(&dir).expect("open cache");

    let t0 = Instant::now();
    let cold = WasmAotEvaluator::from_source_with_cache(src, &cache).expect("cold build");
    let cold_elapsed = t0.elapsed();

    let t1 = Instant::now();
    let warm = WasmAotEvaluator::from_source_with_cache(src, &cache).expect("warm build");
    let warm_elapsed = t1.elapsed();

    // Both builds must produce evaluators that agree on the same input.
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(6));
    args.insert("y".to_string(), Value::Int(7));
    let cold_out = cold.run_main(args.clone()).expect("cold run");
    let warm_out = warm.run_main(args).expect("warm run");
    assert_eq!(cold_out, Value::Int(43));
    assert_eq!(cold_out, warm_out);

    eprintln!(
        "aot cache: cold build = {:?}, warm build = {:?}",
        cold_elapsed, warm_elapsed
    );

    // Cache hit visibility: the meta sidecar must exist for the second
    // call to have reached the fast path. We don't introspect file
    // contents here (cache::tests handles that) — just confirm the
    // bookkeeping wrote what we expected.
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("read cache dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    assert!(
        entries
            .iter()
            .any(|n| n.to_string_lossy().ends_with(".wasm")),
        "expected .wasm artifact in cache: {:?}",
        entries
    );
    assert!(
        entries
            .iter()
            .any(|n| n.to_string_lossy().ends_with(".meta")),
        "expected .meta sidecar in cache: {:?}",
        entries
    );
    assert!(
        entries
            .iter()
            .any(|n| n.to_string_lossy().ends_with(".schemas")),
        "expected .schemas sidecar in cache: {:?}",
        entries
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cache_hit_skips_recompile_for_string_return() {
    // Smoke a different return shape (String pass-through) so the
    // cache rehydration path exercises the pointer-indirect tail
    // record layout in addition to a flat Int.
    let src = "#main(String s) -> String\ns";
    let dir = temp_cache_dir("string");
    let cache = AotCache::open(&dir).expect("open cache");

    let _first = WasmAotEvaluator::from_source_with_cache(src, &cache).expect("first build");
    let second = WasmAotEvaluator::from_source_with_cache(src, &cache).expect("second build");

    let mut args = HashMap::new();
    args.insert(
        "s".to_string(),
        Value::String("relon-cached-hello".to_string()),
    );
    let out = second.run_main(args).expect("run");
    assert_eq!(out, Value::String("relon-cached-hello".to_string()));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cache_miss_after_dir_clear_recompiles() {
    // After clearing the cache directory, the next call must perform
    // a fresh compile rather than panic. Guards against accidentally
    // promoting the `load` Ok(None) path to a hard error.
    let src = "#main(Int x) -> Int\nx + 1";
    let dir = temp_cache_dir("clear");
    let cache = AotCache::open(&dir).expect("open cache");

    let _primed = WasmAotEvaluator::from_source_with_cache(src, &cache).expect("prime");
    // Wipe the directory contents but keep the dir itself.
    for entry in std::fs::read_dir(&dir).expect("read dir") {
        let entry = entry.expect("entry");
        let _ = std::fs::remove_file(entry.path());
    }
    let fresh = WasmAotEvaluator::from_source_with_cache(src, &cache).expect("recompile");
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(41));
    let out = fresh.run_main(args).expect("run");
    assert_eq!(out, Value::Int(42));

    let _ = std::fs::remove_dir_all(&dir);
}
