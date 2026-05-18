//! v5-γ corpus differential: drive every corpus case twice through
//! the `Backend::Auto` evaluator. The first call populates the
//! on-disk cache as a side-effect of `from_source_with_cache`; the
//! second call exercises the cache-hit path through
//! `from_cache_dir`. Both calls must produce the same `Value` (or
//! the same trap) — drift indicates either a cache-pair invariant
//! drift or a non-deterministic codegen pass.
//!
//! The cache directory is a `tempfile::TempDir` so concurrent test
//! threads cannot interfere. We override `XDG_CACHE_HOME` for the
//! duration of the test process via `env::set_var` — the harness
//! does not parallelise other env-var consumers, so the override is
//! safe within this test binary.

use std::collections::HashMap;

use relon::{new_evaluator, Backend};
use relon_test_harness::corpus::all_cases;

/// Compare two `relon_eval_api::Value`s for bit-identical equality.
/// Mirrors the helper in `relon_test_harness::lib::value_bit_eq` so
/// the differential ignores HashMap iteration order for `Dict`s.
fn value_bit_eq(
    a: &relon_eval_api::Value,
    b: &relon_eval_api::Value,
) -> bool {
    relon_test_harness::value_bit_eq(a, b)
}

#[test]
fn corpus_round_trips_through_cache_hit_path() {
    // Isolate the on-disk cache to a tempdir so the test stays
    // hermetic. We override XDG_CACHE_HOME *and* HOME so the
    // default_cache_dir resolution lands inside the temp tree
    // regardless of which branch the lookup hits.
    let cache_root = tempfile::tempdir().expect("tempdir for cache");
    let prev_xdg = std::env::var_os("XDG_CACHE_HOME");
    let prev_home = std::env::var_os("HOME");
    // SAFETY: we restore both env vars in a drop guard below.
    // SAFETY: env::set_var is "safe" but races with concurrent
    // reads; this harness binary is single-threaded by default and
    // we restore before returning.
    unsafe {
        std::env::set_var("XDG_CACHE_HOME", cache_root.path());
        std::env::set_var("HOME", cache_root.path());
    }
    struct EnvGuard {
        prev_xdg: Option<std::ffi::OsString>,
        prev_home: Option<std::ffi::OsString>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev_xdg.take() {
                    Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                    None => std::env::remove_var("XDG_CACHE_HOME"),
                }
                match self.prev_home.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }
    let _guard = EnvGuard {
        prev_xdg,
        prev_home,
    };

    let cases = all_cases();
    let mut auto_ok = 0usize;
    let mut auto_unsupported = 0usize;
    let mut total_runs = 0usize;
    let mut failures: Vec<(String, String)> = Vec::new();

    for case in &cases {
        // First pass: source -> from_source_with_cache (writes
        // cache pair) -> run_main.
        let args = (case.args_factory)();
        let auto_first = match new_evaluator(case.source, Backend::Auto) {
            Ok(ev) => ev,
            Err(e) => {
                failures.push((
                    case.name.to_string(),
                    format!("first new_evaluator: {e}"),
                ));
                continue;
            }
        };
        let r1 = auto_first.run_main(args.clone());
        drop(auto_first);
        total_runs += 1;

        // Second pass: same source -> from_cache_dir hit -> run_main.
        let auto_second = match new_evaluator(case.source, Backend::Auto) {
            Ok(ev) => ev,
            Err(e) => {
                failures.push((
                    case.name.to_string(),
                    format!("second new_evaluator: {e}"),
                ));
                continue;
            }
        };
        let r2 = auto_second.run_main(args.clone());
        total_runs += 1;

        // Outcomes must agree. We do *not* require `r1` / `r2` to
        // be `Ok` — some corpus cases trap; both passes must trap
        // the same way.
        match (&r1, &r2) {
            (Ok(v1), Ok(v2)) => {
                if value_bit_eq(v1, v2) {
                    auto_ok += 1;
                } else {
                    failures.push((
                        case.name.to_string(),
                        format!("value drift: {v1:?} vs {v2:?}"),
                    ));
                }
            }
            (Err(e1), Err(e2)) => {
                // Trap message equality is too strict; compare
                // discriminant + type name.
                let s1 = format!("{e1:?}");
                let s2 = format!("{e2:?}");
                // Soft-equality on the leading discriminant tag.
                let head1 = s1.split('(').next().unwrap_or("").trim();
                let head2 = s2.split('(').next().unwrap_or("").trim();
                if head1 == head2 || s1 == s2 {
                    if s1.contains("Unsupported") {
                        auto_unsupported += 1;
                    } else {
                        auto_ok += 1;
                    }
                } else {
                    failures.push((
                        case.name.to_string(),
                        format!("trap drift: {head1} vs {head2}"),
                    ));
                }
            }
            (Ok(v), Err(e)) | (Err(e), Ok(v)) => {
                failures.push((
                    case.name.to_string(),
                    format!("Ok-vs-Err drift: {v:?} vs {e:?}"),
                ));
            }
        }
    }

    eprintln!(
        "[auto-cache] {} cases / {} runs / {} ok / {} unsupported / {} failures",
        cases.len(),
        total_runs,
        auto_ok,
        auto_unsupported,
        failures.len(),
    );
    if !failures.is_empty() {
        for (name, msg) in &failures {
            eprintln!("[auto-cache] FAIL {name}: {msg}");
        }
        panic!("{} cases drifted between first and second invoke", failures.len());
    }
}

/// Tighter smoke: pick a single arithmetic source and assert the
/// two passes hit the same numeric answer. Faster to run than the
/// full corpus, useful as a per-PR gate.
#[test]
fn arithmetic_source_round_trips_through_cache_hit_path() {
    let cache_root = tempfile::tempdir().expect("tempdir");
    let prev_xdg = std::env::var_os("XDG_CACHE_HOME");
    let prev_home = std::env::var_os("HOME");
    unsafe {
        std::env::set_var("XDG_CACHE_HOME", cache_root.path());
        std::env::set_var("HOME", cache_root.path());
    }
    struct G {
        x: Option<std::ffi::OsString>,
        h: Option<std::ffi::OsString>,
    }
    impl Drop for G {
        fn drop(&mut self) {
            unsafe {
                match self.x.take() {
                    Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                    None => std::env::remove_var("XDG_CACHE_HOME"),
                }
                match self.h.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }
    let _g = G {
        x: prev_xdg,
        h: prev_home,
    };

    let src = "#main(Int x, Int y) -> Int\nx * y + 7";
    let args = {
        let mut m = HashMap::with_capacity(2);
        m.insert("x".to_string(), relon_eval_api::Value::Int(6));
        m.insert("y".to_string(), relon_eval_api::Value::Int(7));
        m
    };

    let first = new_evaluator(src, Backend::Auto).expect("first new_evaluator");
    let r1 = first.run_main(args.clone()).expect("first run_main");
    drop(first);

    let second = new_evaluator(src, Backend::Auto).expect("second new_evaluator");
    let r2 = second.run_main(args.clone()).expect("second run_main");
    assert_eq!(r1, r2, "cache round-trip must reproduce the same answer");
    assert_eq!(r1, relon_eval_api::Value::Int(49));
}
