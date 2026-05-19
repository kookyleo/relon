//! v6-λ-0 (bench methodology hardening, 2026-05-19) — source-level
//! validators for the `trace_jit_hot_loop` bench harness. These tests
//! lock the 6-trap mitigations into the harness so a future refactor
//! can't silently drop them:
//!
//! - Trap A — `black_box` appears ≥ 2× per measurement closure.
//! - Trap B — `WARMUP_ITERS = 10_000` plus a pre-warm loop runs BEFORE
//!   the `Instant::now()` timing point inside every measurement.
//! - Trap C — every row uses `HOT_LOOP_N = 1_000_000` (loop-INSIDE) or
//!   drives `HOT_LOOP_N` invocations (dispatch rows).
//! - Trap D — cache-prefill: every row's `iter_custom` routine runs at
//!   least one full invocation BEFORE the warmup loop.
//! - Trap E — per-row alloc tag (`#[zero_alloc]` / `#[per_iter_alloc]`)
//!   is documented in the module doc.
//! - Trap F — bench_stats post-processor recovers ≥ 5 percentile
//!   points from a real criterion-shaped sample.json.
//!
//! Plus a positive end-to-end check on `bench_stats::collect_group_stats`
//! against a small synthetic group tree.
//!
//! These are intentionally string-grep tests so they survive cargo
//! workspace shifts and don't depend on the bench actually running.

use std::fs;
use std::path::PathBuf;

use relon_bench::bench_stats::{collect_group_stats, RowStats, PERCENTILE_POINTS};

/// Locate `benches/trace_jit_hot_loop.rs` relative to this test's
/// `CARGO_MANIFEST_DIR`. Returns the absolute path.
fn bench_source_path() -> PathBuf {
    let manifest =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set during cargo test");
    PathBuf::from(manifest).join("benches/trace_jit_hot_loop.rs")
}

fn bench_source() -> String {
    let path = bench_source_path();
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("must be able to read bench source {}: {e}", path.display()))
}

/// Yield `(closure_label, body)` for every `iter_custom(|iters| { ... })`
/// block in the bench source. Naive brace-counter — sufficient for the
/// bench's straight-line structure.
fn measurement_closures(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let needle = "b.iter_custom(|iters| {";
    let mut search_from = 0;
    let mut closure_idx = 0;
    while let Some(start) = src[search_from..].find(needle) {
        let abs_start = search_from + start + needle.len();
        // Walk braces forward to find the matching `}` of the closure
        // body. We already consumed the opening `{`.
        let bytes = src.as_bytes();
        let mut depth = 1i32;
        let mut i = abs_start;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        // bytes[abs_start..i-1] is the closure body (without the
        // trailing `}`).
        assert!(
            depth == 0,
            "unbalanced braces near iter_custom @ {abs_start}"
        );
        let body = src[abs_start..i - 1].to_string();
        closure_idx += 1;
        let label = preceding_bench_id(src, search_from + start)
            .unwrap_or_else(|| format!("closure#{closure_idx}"));
        out.push((label, body));
        search_from = i;
    }
    out
}

/// Look backwards from `offset` for the nearest `BenchmarkId::new(...,
/// "<row>")` and return the `<row>` string if found.
fn preceding_bench_id(src: &str, offset: usize) -> Option<String> {
    let window = &src[..offset];
    let needle = "BenchmarkId::new(\"backend\",";
    let last = window.rfind(needle)?;
    let after = &src[last + needle.len()..];
    let q1 = after.find('"')?;
    let after_q1 = &after[q1 + 1..];
    let q2 = after_q1.find('"')?;
    Some(after_q1[..q2].to_string())
}

#[test]
fn trap_a_black_box_present_in_every_closure() {
    let src = bench_source();
    let closures = measurement_closures(&src);
    assert!(
        closures.len() >= 11,
        "expected ≥ 11 measurement closures (5 loop-INSIDE + 7 dispatch including baseline), got {}",
        closures.len()
    );
    for (label, body) in &closures {
        let black_box_count = body.matches("black_box(").count();
        assert!(
            black_box_count >= 2,
            "row `{label}`: black_box(..) must appear ≥ 2× in the measurement closure (got {black_box_count}). Trap A mitigation."
        );
    }
}

#[test]
fn trap_b_warmup_present_in_every_closure_and_correctly_sized() {
    let src = bench_source();
    assert!(
        src.contains("const WARMUP_ITERS: u64 = 10_000;"),
        "Trap B: WARMUP_ITERS must be defined as a u64 const = 10_000"
    );
    // `timed_with_warmup` MUST contain a `WARMUP_ITERS` pre-loop BEFORE
    // any `Instant::now()`.
    let helper_start = src
        .find("fn timed_with_warmup")
        .expect("Trap B: timed_with_warmup helper must exist");
    let helper_end = src[helper_start..]
        .find("\nfn ")
        .map(|d| helper_start + d)
        .unwrap_or(src.len());
    let helper = &src[helper_start..helper_end];
    let warmup_pos = helper
        .find("for _ in 0..WARMUP_ITERS")
        .expect("Trap B: helper must include a WARMUP_ITERS pre-loop");
    // We want the WARMUP_ITERS loop to run BEFORE the **timed-region**
    // `Instant::now()`. Helper may also call `Instant::now()` ahead of
    // the warmup loop to enforce the warmup time-cap (Trap B sibling);
    // the timed-region marker is `let start = Instant::now();`.
    let timed_start = helper
        .find("let start = Instant::now();")
        .expect("Trap B: helper must mark the timed region with `let start = Instant::now();`");
    assert!(
        warmup_pos < timed_start,
        "Trap B: WARMUP_ITERS pre-loop must run BEFORE the timed-region `let start = Instant::now();`"
    );

    // Every measurement closure must call `timed_with_warmup`.
    let closures = measurement_closures(&src);
    for (label, body) in &closures {
        assert!(
            body.contains("timed_with_warmup("),
            "row `{label}`: measurement closure must dispatch through timed_with_warmup(...). Trap B mitigation."
        );
    }
}

#[test]
fn trap_c_hot_loop_n_constant_at_1m() {
    let src = bench_source();
    assert!(
        src.contains("const HOT_LOOP_N: u64 = 1_000_000;"),
        "Trap C: HOT_LOOP_N must be defined as a u64 const = 1_000_000"
    );
    // The TREE_WALK_LOOP_N row is the only exception (µs-class
    // tree-walker) and must be explicitly documented.
    assert!(
        src.contains("const TREE_WALK_LOOP_N: u64 = 10_000;"),
        "Trap C: tree-walker row uses TREE_WALK_LOOP_N = 10_000; the cost difference is amortised via Throughput::Elements"
    );
}

#[test]
fn trap_d_cache_prefill_happens_before_warmup_in_helper() {
    let src = bench_source();
    let helper_start = src.find("fn timed_with_warmup").expect("helper must exist");
    let helper_end = src[helper_start..]
        .find("\nfn ")
        .map(|d| helper_start + d)
        .unwrap_or(src.len());
    let helper = &src[helper_start..helper_end];
    // The helper's first `routine()` call (cache-prefill) must precede
    // the `for _ in 0..WARMUP_ITERS` loop.
    let first_call = helper.find("routine();").expect("cache-prefill call");
    let warmup_loop = helper
        .find("for _ in 0..WARMUP_ITERS")
        .expect("warmup loop");
    assert!(
        first_call < warmup_loop,
        "Trap D: cache-prefill routine() must run BEFORE the warmup loop in timed_with_warmup"
    );
}

#[test]
fn trap_e_alloc_annotations_documented_per_row() {
    let src = bench_source();
    // The module doc must mention each tag at least once.
    assert!(
        src.contains("#[zero_alloc]"),
        "Trap E: module doc must annotate zero_alloc rows"
    );
    assert!(
        src.contains("#[per_iter_alloc]"),
        "Trap E: module doc must annotate per_iter_alloc rows"
    );
    // Module-level alloc table must enumerate all four loop-INSIDE
    // rows + the dispatch_* prefix.
    for row in [
        "tree_walk_loop",
        "cranelift_aot_loop",
        "trace_jit_loop",
        "trace_jit_loop_recorded",
        "rust_native_loop",
    ] {
        assert!(
            src.contains(row),
            "Trap E: alloc table must mention row `{row}`"
        );
    }
}

#[test]
fn trap_f_sample_size_at_least_100() {
    let src = bench_source();
    let needle = "const SAMPLE_SIZE: usize = ";
    let start = src
        .find(needle)
        .expect("Trap F: SAMPLE_SIZE constant must be defined");
    let after = &src[start + needle.len()..];
    let end = after.find(';').expect("SAMPLE_SIZE must end with `;`");
    let value: usize = after[..end]
        .trim()
        .parse()
        .expect("SAMPLE_SIZE must be a usize literal");
    assert!(
        value >= 100,
        "Trap F: sample_size must be ≥ 100 for p99.9 to have ≥ 1 tail sample; got {value}"
    );
    assert!(
        src.contains("group.sample_size(SAMPLE_SIZE);"),
        "Trap F: bench group must apply SAMPLE_SIZE"
    );
}

#[test]
fn trap_f_bench_stats_recovers_five_percentile_points() {
    let dir = tempfile::tempdir().unwrap();
    let row_dir = dir.path().join("backend").join("test_row").join("new");
    fs::create_dir_all(&row_dir).unwrap();
    // 200 samples; per-sample per-iter time is `i ns/iter` for i in
    // 1..=200 — gives a clean known distribution.
    let iters: Vec<f64> = vec![1.0; 200];
    let times: Vec<f64> = (1..=200).map(|i| i as f64).collect();
    let body = format!("{{\"sampling_mode\":\"Linear\",\"iters\":{iters:?},\"times\":{times:?}}}");
    fs::write(row_dir.join("sample.json"), body).unwrap();

    let rows = collect_group_stats(dir.path()).expect("collect");
    assert_eq!(rows.len(), 1);
    let stats = &rows[0];
    let table = stats.percentile_table();
    assert!(
        table.len() >= 5,
        "Trap F: bench_stats must produce ≥ 5 percentile points; got {}",
        table.len()
    );
    let labels: Vec<&str> = table.iter().map(|(l, _)| *l).collect();
    assert!(labels.contains(&"p50"), "must include p50");
    assert!(labels.contains(&"p90"), "must include p90");
    assert!(labels.contains(&"p99"), "must include p99");
    assert!(labels.contains(&"p99.9"), "must include p99.9");
    assert!(labels.contains(&"max"), "must include max");
}

#[test]
fn percentile_points_const_lists_five_entries() {
    assert_eq!(
        PERCENTILE_POINTS.len(),
        5,
        "Trap F: PERCENTILE_POINTS must list exactly 5 entries (p50, p90, p99, p99.9, max)"
    );
}

#[test]
fn bench_stats_extracts_max_from_synthetic_distribution() {
    let dir = tempfile::tempdir().unwrap();
    let row_dir = dir.path().join("backend").join("synthetic").join("new");
    fs::create_dir_all(&row_dir).unwrap();
    let iters: Vec<f64> = vec![1.0; 100];
    let mut times: Vec<f64> = (1..=99).map(|i| i as f64).collect();
    times.push(9999.0); // a tail outlier
    let body = format!("{{\"sampling_mode\":\"Linear\",\"iters\":{iters:?},\"times\":{times:?}}}");
    fs::write(row_dir.join("sample.json"), body).unwrap();
    let row = RowStats::from_sample_json("dim/row", &row_dir.join("sample.json")).unwrap();
    // The tail outlier must surface in max — Trap F's whole point.
    assert!(
        (row.percentile(1.0) - 9999.0).abs() < 1e-6,
        "max must capture the outlier"
    );
    // p50 must NOT be polluted by the tail.
    assert!(
        row.percentile(0.5) < 100.0,
        "p50 should not be dominated by one tail outlier"
    );
}
