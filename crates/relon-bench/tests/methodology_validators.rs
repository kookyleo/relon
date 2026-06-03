//! Source-level + behavioural validators for the `bench_stats`
//! percentile post-processor (criterion `sample.json` -> p50/p90/
//! p99/p99.9/max). Kept as a standalone test target so a refactor of
//! the percentile recovery path stays pinned to a known distribution.

use relon_bench::bench_stats::{collect_group_stats, RowStats, PERCENTILE_POINTS};
use std::fs;

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
