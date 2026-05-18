//! v5-γ object-cache integration tests: drive `from_source_with_cache`
//! plus `from_cache_dir` end-to-end against an isolated tempfile cache
//! directory.
//!
//! Each test uses a fresh `tempfile::TempDir` and overrides
//! `XDG_CACHE_HOME` for the duration of the call via an explicit
//! `cache_dir` argument; we **never** mutate the global env so the
//! cargo-test thread pool doesn't see flicker.
//!
//! The HMAC key path is left at the host default
//! (`$XDG_DATA_HOME/relon/cache-key`); a missing key gracefully
//! degrades to no-HMAC mode per the integration layer's design.

use std::collections::HashMap;

use relon_codegen_native::{
    compute_source_hash, ir_cache_path_for, CraneliftAotEvaluator, SandboxConfig,
};
use relon_eval_api::{Evaluator, Value};

/// Source kept simple enough that the lowering pipeline always
/// produces a buffer-protocol IR — exercising the full cache write
/// + restore path against a real production-shaped module.
fn corpus_add_source() -> &'static str {
    "#main(Int x, Int y) -> Int\nx + y"
}

fn args(x: i64, y: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(2);
    m.insert("x".to_string(), Value::Int(x));
    m.insert("y".to_string(), Value::Int(y));
    m
}

#[test]
fn from_source_with_cache_writes_pair_on_first_call() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = corpus_add_source();

    // Before the call no cache file should exist.
    let hash = compute_source_hash(src, &SandboxConfig::default());
    let ir_path = ir_cache_path_for(dir.path(), hash);
    let obj_path = relon_object_cache::storage::cache_path_for(dir.path(), hash);
    assert!(!ir_path.exists(), "ir-cache should be absent initially");
    assert!(
        !obj_path.exists(),
        "object-cache should be absent initially"
    );

    let aot = CraneliftAotEvaluator::from_source_with_cache(src, dir.path())
        .expect("from_source_with_cache");
    // Live invocation works.
    let r = aot.run_main(args(40, 2)).expect("run_main");
    assert_eq!(r, Value::Int(42));

    // IR-cache file should now exist; object-cache file may or may
    // not exist depending on whether the host has a usable linker
    // (CI environments without ld surface as a best-effort skip).
    assert!(ir_path.exists(), "ir-cache file should be written");
}

#[test]
fn from_cache_dir_returns_none_on_miss() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = corpus_add_source();

    let opt = CraneliftAotEvaluator::from_cache_dir(src, dir.path()).expect("from_cache_dir");
    assert!(opt.is_none(), "fresh directory should miss");
}

#[test]
fn from_cache_dir_hits_after_from_source_with_cache() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = corpus_add_source();

    // First call: populate cache.
    let _ = CraneliftAotEvaluator::from_source_with_cache(src, dir.path())
        .expect("from_source_with_cache");

    // Second call: from_cache_dir should restore from the IR-cache
    // half. Only the IR-cache half is required for the fast restore
    // — the object-cache half is best-effort; if the host has no ld
    // the IR-cache survives alone and from_cache_dir returns None.
    let hash = compute_source_hash(src, &SandboxConfig::default());
    let obj_path = relon_object_cache::storage::cache_path_for(dir.path(), hash);
    if !obj_path.exists() {
        // No linker available — expected fallback on lean CI hosts.
        // Skip the rest of the test rather than fail; the
        // `from_source_with_cache_writes_pair_on_first_call` covers
        // the IR-cache contract.
        eprintln!("skipping cache-hit assertion: no object-cache file (linker missing?)");
        return;
    }

    let aot = CraneliftAotEvaluator::from_cache_dir(src, dir.path())
        .expect("from_cache_dir result")
        .expect("cache hit");
    let r = aot.run_main(args(40, 2)).expect("run_main from cache");
    assert_eq!(r, Value::Int(42));
}

#[test]
fn cache_hit_produces_same_result_as_fresh_build() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = corpus_add_source();

    // Fresh build via from_source_with_cache (cache miss path).
    let fresh =
        CraneliftAotEvaluator::from_source_with_cache(src, dir.path()).expect("fresh build");
    let fresh_result = fresh.run_main(args(100, 23)).expect("fresh run_main");
    drop(fresh);

    let hash = compute_source_hash(src, &SandboxConfig::default());
    let obj_path = relon_object_cache::storage::cache_path_for(dir.path(), hash);
    if !obj_path.exists() {
        eprintln!("skipping cache-hit parity assertion: no object-cache file");
        return;
    }

    let cached = CraneliftAotEvaluator::from_cache_dir(src, dir.path())
        .expect("from_cache_dir")
        .expect("cache hit");
    let cached_result = cached.run_main(args(100, 23)).expect("cached run_main");
    assert_eq!(
        fresh_result, cached_result,
        "fresh vs cached run_main should agree"
    );
    assert_eq!(cached_result, Value::Int(123));
}

#[test]
fn corrupted_object_cache_invalidates_pair() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = corpus_add_source();

    let _ = CraneliftAotEvaluator::from_source_with_cache(src, dir.path())
        .expect("from_source_with_cache");
    let hash = compute_source_hash(src, &SandboxConfig::default());
    let obj_path = relon_object_cache::storage::cache_path_for(dir.path(), hash);
    if !obj_path.exists() {
        eprintln!("skipping corruption test: no object-cache file (linker missing?)");
        return;
    }

    // Tamper with the middle of the file (magic + version stay
    // intact so we hit the HMAC-mismatch path, not the truncated
    // path). The relon-object-cache HMAC tag covers the whole body;
    // flipping a single object byte invalidates the tag.
    let mut buf = std::fs::read(&obj_path).expect("read");
    let mid = buf.len() / 2;
    buf[mid] ^= 0xFF;
    std::fs::write(&obj_path, &buf).expect("rewrite");

    let opt = CraneliftAotEvaluator::from_cache_dir(src, dir.path()).expect("from_cache_dir");
    assert!(opt.is_none(), "tampered cache should be invalidated");
    assert!(
        !obj_path.exists(),
        "tampered object-cache file should be deleted"
    );
}

#[test]
fn corrupted_ir_cache_invalidates_pair() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = corpus_add_source();

    let _ = CraneliftAotEvaluator::from_source_with_cache(src, dir.path())
        .expect("from_source_with_cache");
    let hash = compute_source_hash(src, &SandboxConfig::default());
    let ir_path = ir_cache_path_for(dir.path(), hash);

    // Tamper with the IR cache.
    let mut buf = std::fs::read(&ir_path).expect("read ir cache");
    let last = buf.len() - 1;
    buf[last] ^= 0xFF;
    std::fs::write(&ir_path, &buf).expect("rewrite");

    let opt = CraneliftAotEvaluator::from_cache_dir(src, dir.path()).expect("from_cache_dir");
    // Without an object-cache file, the IR-cache corruption alone
    // is enough to trip the pair invalidation since from_cache_dir
    // requires both halves to be present and consistent.
    assert!(opt.is_none(), "tampered IR cache should yield None");
}

#[test]
fn missing_ir_cache_invalidates_pair() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = corpus_add_source();

    let _ = CraneliftAotEvaluator::from_source_with_cache(src, dir.path())
        .expect("from_source_with_cache");
    let hash = compute_source_hash(src, &SandboxConfig::default());
    let ir_path = ir_cache_path_for(dir.path(), hash);
    let obj_path = relon_object_cache::storage::cache_path_for(dir.path(), hash);
    if !obj_path.exists() {
        eprintln!("skipping missing-IR test: no object-cache file");
        return;
    }

    // Delete the IR-cache half only — the loader should treat this
    // as a miss and clean up the object-cache half too.
    std::fs::remove_file(&ir_path).expect("remove ir cache");

    let opt = CraneliftAotEvaluator::from_cache_dir(src, dir.path()).expect("from_cache_dir");
    assert!(opt.is_none(), "missing ir-cache should yield miss");
    // The companion object-cache file should be cleaned up so the
    // next from_source_with_cache call writes a consistent pair.
    assert!(
        !obj_path.exists(),
        "stale object-cache should be cleaned on IR-cache miss"
    );
}

#[test]
fn different_source_does_not_hit_existing_cache() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_a = corpus_add_source();
    let src_b = "#main(Int x, Int y) -> Int\nx * y"; // different body

    let _ = CraneliftAotEvaluator::from_source_with_cache(src_a, dir.path())
        .expect("from_source_with_cache a");

    let opt_b = CraneliftAotEvaluator::from_cache_dir(src_b, dir.path()).expect("from_cache_dir b");
    assert!(opt_b.is_none(), "different source should miss the cache");
}

#[test]
fn cache_hits_are_concurrency_safe() {
    use std::thread;

    let dir = tempfile::tempdir().expect("tempdir");
    let src = corpus_add_source();

    // Populate cache once on the main thread.
    let _ = CraneliftAotEvaluator::from_source_with_cache(src, dir.path())
        .expect("from_source_with_cache");

    let hash = compute_source_hash(src, &SandboxConfig::default());
    let obj_path = relon_object_cache::storage::cache_path_for(dir.path(), hash);
    if !obj_path.exists() {
        eprintln!("skipping concurrency assertion: no object-cache file");
        return;
    }

    // Spin up several threads that each try to from_cache_dir the
    // same key. All should succeed without tripping a torn-write
    // assertion in the loader.
    let mut handles = Vec::new();
    for _ in 0..4 {
        let path = dir.path().to_path_buf();
        let src_owned = src.to_string();
        handles.push(thread::spawn(move || {
            let opt =
                CraneliftAotEvaluator::from_cache_dir(&src_owned, &path).expect("from_cache_dir");
            let aot = opt.expect("cache hit");
            aot.run_main(args(7, 8)).expect("run_main")
        }));
    }
    for h in handles {
        let v = h.join().expect("join");
        assert_eq!(v, Value::Int(15));
    }
}

#[test]
fn loader_round_trip_from_emitted_stub_bytes() {
    // Exercise the relon-object-cache `LoadedObject::from_bytes`
    // path end-to-end: emit a stub ET_REL via cranelift-object,
    // link to ET_DYN via relon-object-link, then load it through
    // memfd + dlopen and dlsym the entry. Validates the loader
    // pipeline is wired correctly without depending on the cache
    // file format.
    let stub_bytes = relon_codegen_native::object_cache_integration::emit_entry_stub_object()
        .expect("emit_entry_stub_object");
    let dyn_bytes = match relon_object_link::link_to_dyn(&stub_bytes, "x86_64-unknown-linux-gnu") {
        Ok(b) => b,
        Err(relon_object_link::LinkError::LinkerNotFound) => {
            eprintln!("skipping loader round-trip: no usable linker on PATH");
            return;
        }
        Err(e) => panic!("link_to_dyn: {e}"),
    };
    let loaded = relon_object_cache::LoadedObject::from_bytes(
        &dyn_bytes,
        "x86_64-unknown-linux-gnu",
        &["relon_main_entry", "__relon_capability_vtable"],
    )
    .expect("LoadedObject::from_bytes");
    let entry = loaded.resolve("relon_main_entry").expect("entry resolved");
    assert!(!entry.is_null(), "entry pointer should be non-null");
    let vt = loaded
        .resolve("__relon_capability_vtable")
        .expect("vtable resolved");
    assert!(!vt.is_null(), "vtable pointer should be non-null");
    // The stub returns i32 0; call it through a typed fn pointer
    // to prove the dlopen path is callable end-to-end.
    type EntryFn = unsafe extern "C" fn(
        *const std::ffi::c_void, // state
        i32,
        i32,
        i32,
        i32, // four buffer slots
        i64, // caps
    ) -> i32;
    let f: EntryFn = unsafe { std::mem::transmute(entry) };
    let rc = unsafe { f(std::ptr::null(), 0, 0, 0, 0, 0) };
    assert_eq!(rc, 0, "stub entry should return zero");
}
