//! #171 regression: when the per-installation HMAC key cannot be
//! provisioned, the native cache must refuse to write or read the
//! object / IR / schema triple. The earlier code path silently fell
//! back to a `hmac_key = None` write and a permissive
//! (now-removed) integrity-mode load — any local attacker with
//! write access to the cache directory could then drop an
//! unauthenticated ELF and have it dlopen'd into the host process.
//!
//! This test pins `XDG_DATA_HOME` to a read-only directory so
//! `relon_object_cache::ensure_key()` fails (cannot create the key
//! file). With that failure mode in place, both the write and load
//! paths must short-circuit without touching the cache directory.
//!
//! Lives in its own test binary so the env-var mutation does not
//! race other tests in the codegen-native suite — cargo runs each
//! `tests/*.rs` file as a separate process.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use relon_codegen_native::{
    object_cache_integration as cache_int, AotEvaluator, SandboxConfig,
};

fn corpus_source() -> &'static str {
    "#main(Int x, Int y) -> Int\nx + y"
}

#[test]
fn cache_writes_are_refused_when_hmac_key_unavailable() {
    // 1. Stage a tempdir that we will set as `XDG_DATA_HOME` and
    // then make read-only so `ensure_key()` cannot create the key
    // file in `<dir>/relon/cache-key`.
    let xdg = tempfile::tempdir().expect("xdg tempdir");
    let mut perms = std::fs::metadata(xdg.path()).unwrap().permissions();
    perms.set_mode(0o500); // r-x for owner; no write.
    std::fs::set_permissions(xdg.path(), perms).expect("chmod xdg");

    let cache_dir = tempfile::tempdir().expect("cache tempdir");

    // SAFETY: this test binary owns the global env until cargo joins
    // it; we restore the prior value before returning to keep
    // post-test diagnostics clean.
    let saved = std::env::var_os("XDG_DATA_HOME");
    unsafe {
        std::env::set_var("XDG_DATA_HOME", xdg.path());
    }

    // 2. `from_source_with_cache` runs codegen + best-effort cache.
    // It should succeed (live JIT still works) but write nothing
    // into `cache_dir` because `ensure_key()` fails.
    let aot = AotEvaluator::from_source_with_cache(corpus_source(), cache_dir.path())
        .expect("from_source_with_cache");
    // Sanity check the live invocation still answers.
    use relon_eval_api::{Evaluator, Value};
    let mut args = std::collections::HashMap::new();
    args.insert("x".to_string(), Value::Int(40));
    args.insert("y".to_string(), Value::Int(2));
    assert_eq!(aot.run_main(args).expect("run_main"), Value::Int(42));

    let source_hash = cache_int::compute_source_hash(corpus_source(), &SandboxConfig::default());
    let obj_path = relon_object_cache::storage::cache_path_for(cache_dir.path(), source_hash);
    let ir_path = cache_int::ir_cache_path_for(cache_dir.path(), source_hash);
    let schema_path =
        relon_codegen_native::schema_cache::schema_cache_path_for(cache_dir.path(), source_hash);
    assert!(
        !obj_path.exists(),
        "object cache must be absent when HMAC key cannot be provisioned"
    );
    assert!(
        !ir_path.exists(),
        "IR cache must be absent when HMAC key cannot be provisioned"
    );
    assert!(
        !schema_path.exists(),
        "schema cache must be absent when HMAC key cannot be provisioned"
    );

    // 3. `from_cache_dir` must also refuse rather than fall back to
    // an unauthenticated read.
    let opt = AotEvaluator::from_cache_dir(corpus_source(), cache_dir.path())
        .expect("from_cache_dir result");
    assert!(
        opt.is_none(),
        "from_cache_dir must return None when HMAC key cannot be provisioned"
    );

    // Restore permissions so tempdir Drop can clean up.
    let mut perms = std::fs::metadata(xdg.path()).unwrap().permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(xdg.path(), perms).expect("restore chmod");
    unsafe {
        match saved {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }
}
