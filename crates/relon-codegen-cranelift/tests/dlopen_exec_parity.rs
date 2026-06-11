//! dlopen-execution acceptance tests: a cached cold start must run
//! the dlopen'd object (asserted via `is_dlopen_backed`) and produce
//! results identical to a fresh in-process compile, across both
//! scalar and buffer-protocol entry shapes — including String / List
//! marshalling. The security chain is exercised end-to-end: a
//! tampered HMAC tag refuses to load, and a generator-version
//! mismatch counts as a miss that the next compile overwrites.
//!
//! Every test routes the HMAC key through an **isolated**
//! `XDG_DATA_HOME` tempdir (set once per test process via
//! `OnceLock`) and an isolated per-test cache dir, so the user's
//! real `$XDG_DATA_HOME/relon/cache-key` and `$XDG_CACHE_HOME/relon`
//! are never touched. Lives in its own test binary so the env-var
//! mutation cannot race other test files (cargo runs each
//! `tests/*.rs` as a separate process).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use relon_codegen_cranelift::{object_cache_integration as cache_int, AotEvaluator, SandboxConfig};
use relon_eval_api::{Evaluator, SmolStr, Value};

/// One isolated `XDG_DATA_HOME` for the whole test process. All tests
/// call [`isolate_xdg_data_home`] first; the env var is written
/// exactly once inside `get_or_init` (which serialises concurrent
/// initialisers), after which every caller only reads it.
static ISOLATED_XDG: OnceLock<tempfile::TempDir> = OnceLock::new();

fn isolate_xdg_data_home() {
    ISOLATED_XDG.get_or_init(|| {
        let dir = tempfile::tempdir().expect("xdg tempdir");
        // SAFETY: called before any test in this process touches the
        // HMAC key path; `get_or_init` blocks concurrent callers
        // until the write completes.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.path());
        }
        dir
    });
}

/// Warm the cache for `src`, then return the dlopen-backed evaluator
/// from `from_cache_dir`. Returns `None` (with a skip note) when the
/// host cannot complete the object-cache pipeline (no system linker).
fn warm_then_load(dir: &std::path::Path, src: &str) -> Option<AotEvaluator> {
    let warm = AotEvaluator::from_source_with_cache(src, dir).expect("from_source_with_cache");
    assert!(
        !warm.is_dlopen_backed(),
        "fresh build must be JIT-backed, not dlopen-backed"
    );
    drop(warm);

    let hash = cache_int::compute_source_hash(src, &SandboxConfig::default());
    let obj_path = relon_object_cache::storage::cache_path_for(dir, hash);
    if !obj_path.exists() {
        eprintln!("skipping dlopen-exec assertion: no object-cache file (linker missing?)");
        return None;
    }

    let cached = AotEvaluator::from_cache_dir(src, dir)
        .expect("from_cache_dir result")
        .expect("cache hit expected after warm write");
    assert!(
        cached.is_dlopen_backed(),
        "cache hit must execute the dlopen'd object, not rebuild"
    );
    Some(cached)
}

/// Build a two-codepoint CJK string (U+4E2D U+6587) from escapes so
/// this source file stays ASCII-only.
fn cjk() -> String {
    "\u{4E2D}\u{6587}".to_string()
}

#[test]
fn string_marshalling_parity_fresh_vs_dlopen() {
    isolate_xdg_data_home();
    let dir = tempfile::tempdir().expect("tempdir");
    // f-string over a String param: exercises String in-marshalling,
    // const-data restore from the schema sidecar, and String return
    // decoding on the dlopen path.
    let src = "#main(String s) -> String\nf\"hi ${s}!\"";

    let fresh = AotEvaluator::from_source_with_cache(src, dir.path()).expect("fresh");
    let inputs = [String::new(), "world".to_string(), cjk()];
    let fresh_outs: Vec<Value> = inputs
        .iter()
        .map(|s| {
            let mut args = HashMap::new();
            args.insert("s".to_string(), Value::String(SmolStr::from(s.as_str())));
            fresh.run_main(args).expect("fresh run_main")
        })
        .collect();
    drop(fresh);

    let hash = cache_int::compute_source_hash(src, &SandboxConfig::default());
    if !relon_object_cache::storage::cache_path_for(dir.path(), hash).exists() {
        eprintln!("skipping String parity: no object-cache file (linker missing?)");
        return;
    }
    let cached = AotEvaluator::from_cache_dir(src, dir.path())
        .expect("from_cache_dir")
        .expect("cache hit");
    assert!(cached.is_dlopen_backed());
    for (s, want) in inputs.iter().zip(fresh_outs.iter()) {
        let mut args = HashMap::new();
        args.insert("s".to_string(), Value::String(SmolStr::from(s.as_str())));
        let got = cached.run_main(args).expect("cached run_main");
        assert_eq!(&got, want, "dlopen result diverged for input {s:?}");
    }
    assert_eq!(
        fresh_outs[1],
        Value::String(SmolStr::from("hi world!")),
        "sanity: expected f-string output"
    );
}

#[test]
fn list_int_cross_region_parity_fresh_vs_dlopen() {
    isolate_xdg_data_home();
    let dir = tempfile::tempdir().expect("tempdir");
    // Cross-region object return: the `xs` field points at parameter
    // data in `in_buf`, so the dlopen path must run the same
    // multi-region verifier + reader the JIT path uses.
    let src = "#main(List<Int> xs) -> Dict\n{ xs: xs, n: 1 }";

    let make_args = || {
        let mut args = HashMap::new();
        args.insert(
            "xs".to_string(),
            Value::List(Arc::new(vec![
                Value::Int(-7),
                Value::Int(0),
                Value::Int(i64::MAX),
            ])),
        );
        args
    };

    let fresh = AotEvaluator::from_source_with_cache(src, dir.path()).expect("fresh");
    let fresh_out = fresh.run_main(make_args()).expect("fresh run_main");
    drop(fresh);

    let Some(cached) = warm_already_loaded(dir.path(), src) else {
        return;
    };
    let cached_out = cached.run_main(make_args()).expect("cached run_main");
    assert_eq!(fresh_out, cached_out, "fresh vs dlopen List<Int> diverged");
}

/// Like [`warm_then_load`] but for sources whose cache was already
/// written by a prior `from_source_with_cache` in the same test.
fn warm_already_loaded(dir: &std::path::Path, src: &str) -> Option<AotEvaluator> {
    let hash = cache_int::compute_source_hash(src, &SandboxConfig::default());
    if !relon_object_cache::storage::cache_path_for(dir, hash).exists() {
        eprintln!("skipping dlopen-exec assertion: no object-cache file (linker missing?)");
        return None;
    }
    let cached = AotEvaluator::from_cache_dir(src, dir)
        .expect("from_cache_dir")
        .expect("cache hit");
    assert!(cached.is_dlopen_backed());
    Some(cached)
}

#[test]
fn list_string_identity_parity_fresh_vs_dlopen() {
    isolate_xdg_data_home();
    let dir = tempfile::tempdir().expect("tempdir");
    // In-place List<String> identity return: the dlopen path must
    // honour the negative in-place sentinel + region verifier chain.
    let src = "#main(List<String> ss) -> List<String>\nss";

    let make_args = || {
        let mut args = HashMap::new();
        args.insert(
            "ss".to_string(),
            Value::List(Arc::new(vec![
                Value::String(SmolStr::from("")),
                Value::String(SmolStr::from(cjk().as_str())),
                Value::String(SmolStr::from("edge")),
            ])),
        );
        args
    };

    let fresh = AotEvaluator::from_source_with_cache(src, dir.path()).expect("fresh");
    let fresh_out = fresh.run_main(make_args()).expect("fresh run_main");
    drop(fresh);

    let Some(cached) = warm_already_loaded(dir.path(), src) else {
        return;
    };
    let cached_out = cached.run_main(make_args()).expect("cached run_main");
    assert_eq!(
        fresh_out, cached_out,
        "fresh vs dlopen List<String> diverged"
    );
}

#[test]
fn buffer_and_scalar_shapes_both_execute_via_dlopen() {
    isolate_xdg_data_home();
    let dir = tempfile::tempdir().expect("tempdir");
    // Buffer-protocol scalar shape (the production default for
    // `from_source*`): two Int params, Int return.
    let src = "#main(Int x, Int y) -> Int\nx * y + 1";
    let Some(cached) = warm_then_load(dir.path(), src) else {
        return;
    };
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(6));
    args.insert("y".to_string(), Value::Int(7));
    assert_eq!(cached.run_main(args).expect("run_main"), Value::Int(43));
}

#[test]
fn tampered_hmac_tag_refuses_load_and_falls_back() {
    isolate_xdg_data_home();
    let dir = tempfile::tempdir().expect("tempdir");
    let src = "#main(Int x, Int y) -> Int\nx + y";

    let _ = AotEvaluator::from_source_with_cache(src, dir.path()).expect("warm");
    let hash = cache_int::compute_source_hash(src, &SandboxConfig::default());
    let obj_path = relon_object_cache::storage::cache_path_for(dir.path(), hash);
    if !obj_path.exists() {
        eprintln!("skipping HMAC-tag tamper test: no object-cache file");
        return;
    }

    // Flip one byte inside the trailing 32-byte HMAC tag — the body
    // stays a valid ELF, only the authentication fails. The loader
    // must refuse without crashing and remove the file.
    let mut buf = std::fs::read(&obj_path).expect("read");
    let tag_byte = buf.len() - 16;
    buf[tag_byte] ^= 0x01;
    std::fs::write(&obj_path, &buf).expect("rewrite");

    let opt = AotEvaluator::from_cache_dir(src, dir.path()).expect("from_cache_dir");
    assert!(opt.is_none(), "tampered HMAC tag must refuse to load");
    assert!(
        !obj_path.exists(),
        "object with a bad HMAC tag should be invalidated"
    );

    // Fallback path still answers: a fresh compile (which also
    // rewrites the cache) produces the correct value.
    let rebuilt = AotEvaluator::from_source_with_cache(src, dir.path()).expect("rebuild");
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(40));
    args.insert("y".to_string(), Value::Int(2));
    assert_eq!(rebuilt.run_main(args).expect("run_main"), Value::Int(42));
}

#[test]
fn generator_version_mismatch_is_miss_then_recompile_overwrites() {
    isolate_xdg_data_home();
    let dir = tempfile::tempdir().expect("tempdir");
    let src = "#main(Int x, Int y) -> Int\nx - y";

    let _ = AotEvaluator::from_source_with_cache(src, dir.path()).expect("warm");
    let hash = cache_int::compute_source_hash(src, &SandboxConfig::default());
    let obj_path = relon_object_cache::storage::cache_path_for(dir.path(), hash);
    if !obj_path.exists() {
        eprintln!("skipping generator-version test: no object-cache file");
        return;
    }

    // Re-store the same (valid, correctly HMAC'd) object under a
    // *stale* generator version. This exercises the metadata-level
    // version check in isolation: the HMAC verifies fine, but the
    // generator stamp no longer matches the running codegen.
    // (The first defence layer — GENERATOR_VERSION mixed into the
    // cache-key filename — cannot be varied from a test because the
    // constant is baked into `compute_source_hash`; a real version
    // bump changes the filename and is a plain miss by construction.)
    let key = relon_object_cache::ensure_key().expect("hmac key");
    let triple = cache_int::host_target_triple();
    let entry = relon_object_cache::load(
        dir.path(),
        hash,
        triple,
        Some(&key),
        relon_object_cache::IntegrityMode::HmacRequired,
    )
    .expect("load fresh entry")
    .expect("entry present");
    let mut stale_md = entry.metadata.clone();
    stale_md.generator_version = "relon-codegen-cranelift v5-gamma 0-stale-test".to_string();
    relon_object_cache::store(
        dir.path(),
        hash,
        triple,
        &entry.object_bytes,
        &stale_md,
        Some(&key),
    )
    .expect("re-store with stale generator version");

    // 1. Version mismatch counts as a miss and removes the stale file.
    let opt = AotEvaluator::from_cache_dir(src, dir.path()).expect("from_cache_dir");
    assert!(opt.is_none(), "stale generator version must miss");
    assert!(
        !obj_path.exists(),
        "stale-versioned object should be removed so the rebuild overwrites"
    );

    // 2. Recompile overwrites the entry...
    let rebuilt = AotEvaluator::from_source_with_cache(src, dir.path()).expect("rebuild");
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(50));
    args.insert("y".to_string(), Value::Int(8));
    let fresh_out = rebuilt.run_main(args.clone()).expect("rebuilt run_main");
    assert_eq!(fresh_out, Value::Int(42));
    assert!(obj_path.exists(), "rebuild must overwrite the object cache");
    drop(rebuilt);

    // 3. ...and the overwritten entry dlopen-executes with the same
    // result as the fresh compile.
    let cached = AotEvaluator::from_cache_dir(src, dir.path())
        .expect("from_cache_dir after rebuild")
        .expect("cache hit after rebuild");
    assert!(cached.is_dlopen_backed());
    assert_eq!(cached.run_main(args).expect("cached run_main"), fresh_out);
}
