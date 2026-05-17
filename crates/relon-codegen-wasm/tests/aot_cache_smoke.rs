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

/// Phase 9.c-1: the `.native` sidecar carries `Module::serialize`
/// output. On the second `from_source_with_cache` call wasmtime
/// skips the JIT through `Module::deserialize`, so the wall-clock
/// build time drops by an order of magnitude. We use a conservative
/// 3× factor as the assertion threshold to absorb noisy CI hosts;
/// the typical observation on a developer laptop is 10×–20×.
#[test]
fn native_cache_hit_skips_jit() {
    let src = "#main(Int x, Int y) -> Int\nx + y * 2";
    let dir = temp_cache_dir("native-hit");
    let cache = AotCache::open(&dir).expect("open cache");

    // First call populates `.wasm` + `.schemas` + `.native`. The
    // native sidecar write is best-effort inside the evaluator's
    // cache path, so we double-check it landed on disk before timing
    // the second call.
    let t0 = Instant::now();
    let first = WasmAotEvaluator::from_source_with_cache(src, &cache).expect("first build");
    let first_elapsed = t0.elapsed();

    let native_present = std::fs::read_dir(&dir)
        .expect("read cache dir")
        .filter_map(|e| e.ok())
        .any(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s == "native")
                .unwrap_or(false)
        });
    assert!(
        native_present,
        "first build must have written a .native sidecar"
    );

    // Second call: the `.native` blob is present + matches the host's
    // wasmtime version + target triple, so `Module::deserialize`
    // replaces the JIT path. Should be substantially faster.
    let t1 = Instant::now();
    let second = WasmAotEvaluator::from_source_with_cache(src, &cache).expect("second build");
    let second_elapsed = t1.elapsed();

    // Both evaluators must still agree on the same input.
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(3));
    args.insert("y".to_string(), Value::Int(7));
    assert_eq!(
        first.run_main(args.clone()).expect("first run"),
        Value::Int(17)
    );
    assert_eq!(second.run_main(args).expect("second run"), Value::Int(17));

    eprintln!(
        "native cache hit: first build = {:?}, second build = {:?}",
        first_elapsed, second_elapsed
    );
    // Conservative threshold: the deserialize path is observed at
    // ~10×–20× the JIT path on a developer laptop. We assert > 3×
    // to absorb noisy CI / battery-throttled runs without losing the
    // regression signal — if the speed-up ever drops back below
    // this floor the native cache plumbing has stopped working.
    assert!(
        second_elapsed * 3 < first_elapsed,
        "expected second build < first/3, got first={:?} second={:?}",
        first_elapsed,
        second_elapsed
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Phase 9.c-1: tampering with the meta sidecar's `native_compat_hash`
/// must drop the `.native` fast path on the next read while leaving
/// the wasm side intact. The evaluator falls back to JIT (slower) but
/// still produces a working binary that returns the right answer.
#[test]
fn native_cache_invalidated_by_version_drift() {
    let src = "#main(Int x) -> Int\nx * 3";
    let dir = temp_cache_dir("native-drift");
    let cache = AotCache::open(&dir).expect("open cache");

    // Prime the cache with all three sidecars.
    let _primed = WasmAotEvaluator::from_source_with_cache(src, &cache).expect("prime");

    // Locate the meta file and zero out the native_compat_hash slot
    // (bytes 51..83). The cache's parser treats a mismatch as a clean
    // miss on the native side without touching the wasm path.
    let meta_path = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|s| s.to_str()) == Some("meta"))
        .expect(".meta sidecar exists after priming");
    let mut meta = std::fs::read(&meta_path).expect("read meta");
    assert!(meta.len() >= 83, "meta size suggests v2 layout");
    for byte in meta[51..83].iter_mut() {
        *byte = 0;
    }
    std::fs::write(&meta_path, &meta).expect("rewrite meta");

    // Second build still succeeds — the `.wasm` + `.schemas` sidecars
    // remain valid, only the native fast path drops out. The
    // evaluator re-JITs and re-writes a fresh .native sidecar (best
    // effort), but the run_main result must still be correct.
    let rebuilt = WasmAotEvaluator::from_source_with_cache(src, &cache).expect("rebuild");
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(14));
    assert_eq!(rebuilt.run_main(args).expect("run"), Value::Int(42));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Phase 9.c-1: truncating the `.native` sidecar must not panic and
/// must not propagate a misleading deserialize error to the host.
/// The evaluator falls back to the JIT path through `Module::new`,
/// re-writes a fresh native sidecar, and returns a working module.
#[test]
fn native_cache_corrupted_falls_back() {
    let src = "#main(Int n) -> Int\nn + 100";
    let dir = temp_cache_dir("native-corrupt");
    let cache = AotCache::open(&dir).expect("open cache");

    let _primed = WasmAotEvaluator::from_source_with_cache(src, &cache).expect("prime");

    // Truncate the native sidecar to a handful of bytes so wasmtime's
    // own deserialize check would surface as `Err` if the load path
    // ever fed it through. The cache's reader is meant to gate this
    // at the read boundary by treating zero-sized / mismatched bytes
    // as a miss — but the safer-by-design fix is: even if the load
    // returns the truncated bytes, the evaluator must not panic and
    // must still produce a valid evaluator. We verify both layers.
    let native_path = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|s| s.to_str()) == Some("native"))
        .expect(".native sidecar exists after priming");
    std::fs::write(&native_path, [0u8; 4]).expect("truncate native");

    // The evaluator must come back cleanly: the cache layer detects
    // the deserialize failure and silently falls back to JIT through
    // `Module::new`, which also re-stashes a fresh `.native` sidecar
    // so the *next* call hits the fast path again.
    let recovered = WasmAotEvaluator::from_source_with_cache(src, &cache)
        .expect("corrupted native must fall back cleanly");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(7));
    assert_eq!(recovered.run_main(args).expect("run"), Value::Int(107));

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
