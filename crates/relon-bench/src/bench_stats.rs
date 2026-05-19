//! Per-sample distribution helper for criterion 0.5 JSON output.
//!
//! v6-λ-0 (bench methodology hardening, 2026-05-19) targets **Trap F —
//! distribution hiding**. Criterion's default `estimates.json` reports
//! `mean`, `median`, `std_dev`, `slope`, but no per-sample tail. Tail
//! events (trace deopt, allocator scavenge, page fault, NMI) are what
//! the Relon-vs-LuaJIT honest comparison cares about (D5 — p99 tail
//! latency, see `docs/internal/relon-vs-luajit-rigorous-plan.md` §1).
//!
//! ## File layout we read
//!
//! For each bench row, criterion writes a `new/sample.json` whose
//! shape is:
//!
//! ```json
//! {
//!   "sampling_mode": "Linear",
//!   "iters": [..., 5115.0, ...],
//!   "times": [..., 6072600.0, ...]
//! }
//! ```
//!
//! `iters[i]` is the iteration count the harness chose for sample `i`;
//! `times[i]` is the total wall-clock nanoseconds the harness spent in
//! that sample (sum of all `iters[i]` invocations of the measurement
//! closure). The per-iteration estimate for sample `i` is therefore
//! `times[i] / iters[i]` nanoseconds.
//!
//! ## Percentiles
//!
//! We extract `p50` / `p90` / `p99` / `p99.9` / `max` directly from
//! the per-sample `times[i] / iters[i]` series. This is the **per-
//! sample** distribution (≈ 100 points by default, 200 if the bench
//! row bumps `sample_size`), not the per-iteration distribution: each
//! sample averages over `iters[i]` invocations, so single-invocation
//! tail outliers are already smeared by criterion's sampler. For tail
//! events that recur every K invocations (K ≪ iters[i]) the sample
//! mean is still pushed up though, so `max`/`p99` is meaningful.
//!
//! For per-iteration tail latency (where a single deopt event in a
//! million invocations must be caught), use the `--profile-time` or
//! custom Throughput::Elements raw-event capture (out of scope for
//! v6-λ-0; carried over to λ-3 LuaJIT p99 wiring).

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Percentile points reported per bench row.
pub const PERCENTILE_POINTS: &[(f64, &str)] = &[
    (0.50, "p50"),
    (0.90, "p90"),
    (0.99, "p99"),
    (0.999, "p99.9"),
    (1.00, "max"),
];

/// Raw deserialised criterion `sample.json` shape (the subset we use).
#[derive(Debug, Deserialize)]
struct SampleJson {
    iters: Vec<f64>,
    times: Vec<f64>,
}

/// Raw deserialised criterion `benchmark.json` shape (the subset we
/// use). When the bench row declares
/// `group.throughput(Throughput::Elements(N))`, criterion records `N`
/// here as `throughput.Elements`. We use it to convert per-sample
/// per-call ns into per-element ns (the column most reports want).
#[derive(Debug, Deserialize)]
struct BenchmarkJson {
    throughput: Option<ThroughputJson>,
}

#[derive(Debug, Deserialize)]
struct ThroughputJson {
    #[serde(rename = "Elements")]
    elements: Option<f64>,
}

/// One bench row's distribution summary.
#[derive(Debug, Clone)]
pub struct RowStats {
    /// Bench row name, as discovered on disk. For criterion rows
    /// emitted via `BenchmarkId::new("backend", "trace_jit_loop")` this
    /// is `"backend/trace_jit_loop"`.
    pub row: String,
    /// Per-sample per-iteration time in nanoseconds, sorted ascending.
    /// Length equals criterion's `sample_size` (default 100, the v6-λ-0
    /// hardened harness sets 200 to keep p99.9 from 100 samples honest).
    ///
    /// **Unit**: ns per `b.iter_custom` inner invocation (i.e. per
    /// closure call). For Relon's `trace_jit_hot_loop` bench, each
    /// closure call drives `HOT_LOOP_N = 1_000_000` body iters, so to
    /// recover per-element-cost divide by `elements_per_call`.
    pub per_sample_ns: Vec<f64>,
    /// `Throughput::Elements(N)` value from `benchmark.json`, if
    /// present. None means the bench didn't declare elements throughput
    /// (in which case the reporter shows per-call cost only).
    pub elements_per_call: Option<f64>,
}

impl RowStats {
    /// Read a criterion `sample.json` from `path` and produce a sorted
    /// per-sample-per-iter ns series under the row label `row`.
    pub fn from_sample_json(row: impl Into<String>, path: &Path) -> Result<Self, BenchStatsError> {
        let raw =
            fs::read_to_string(path).map_err(|e| BenchStatsError::Io(path.to_path_buf(), e))?;
        let parsed: SampleJson = serde_json::from_str(&raw)
            .map_err(|e| BenchStatsError::Parse(path.to_path_buf(), e.to_string()))?;
        if parsed.iters.len() != parsed.times.len() {
            return Err(BenchStatsError::ShapeMismatch {
                path: path.to_path_buf(),
                iters_len: parsed.iters.len(),
                times_len: parsed.times.len(),
            });
        }
        if parsed.iters.is_empty() {
            return Err(BenchStatsError::Empty(path.to_path_buf()));
        }
        let mut per_sample_ns: Vec<f64> = parsed
            .times
            .iter()
            .zip(parsed.iters.iter())
            .map(|(t, n)| if *n > 0.0 { *t / *n } else { f64::INFINITY })
            .collect();
        per_sample_ns.sort_by(|a, b| a.partial_cmp(b).expect("times are finite per criterion"));
        Ok(Self {
            row: row.into(),
            per_sample_ns,
            elements_per_call: None,
        })
    }

    /// Same as [`from_sample_json`] but also reads `benchmark.json`
    /// (a sibling file in the same `new/` directory) so the per-row
    /// `Throughput::Elements(N)` is captured. Use this for the canonical
    /// path; `from_sample_json` is the building block.
    pub fn from_criterion_row_dir(
        row: impl Into<String>,
        row_new_dir: &Path,
    ) -> Result<Self, BenchStatsError> {
        let sample_json = row_new_dir.join("sample.json");
        let mut stats = Self::from_sample_json(row, &sample_json)?;
        let benchmark_json = row_new_dir.join("benchmark.json");
        if benchmark_json.exists() {
            let raw = fs::read_to_string(&benchmark_json)
                .map_err(|e| BenchStatsError::Io(benchmark_json.clone(), e))?;
            // benchmark.json is hand-tolerant: missing throughput is OK.
            if let Ok(parsed) = serde_json::from_str::<BenchmarkJson>(&raw) {
                stats.elements_per_call = parsed.throughput.and_then(|t| t.elements);
            }
        }
        Ok(stats)
    }

    /// Per-element percentile in ns. Divides the per-sample per-call
    /// figure by `elements_per_call` if set; falls back to per-call
    /// ns if no throughput was declared.
    pub fn per_element_percentile(&self, q: f64) -> f64 {
        let per_call = self.percentile(q);
        match self.elements_per_call {
            Some(n) if n > 0.0 => per_call / n,
            _ => per_call,
        }
    }

    /// Linear-interpolated percentile in nanoseconds per iter.
    /// `q` in `[0.0, 1.0]`.
    pub fn percentile(&self, q: f64) -> f64 {
        debug_assert!((0.0..=1.0).contains(&q), "percentile fraction out of range");
        let n = self.per_sample_ns.len();
        if n == 0 {
            return f64::NAN;
        }
        if n == 1 {
            return self.per_sample_ns[0];
        }
        let rank = q * (n as f64 - 1.0);
        let lo = rank.floor() as usize;
        let hi = (lo + 1).min(n - 1);
        let frac = rank - lo as f64;
        self.per_sample_ns[lo] * (1.0 - frac) + self.per_sample_ns[hi] * frac
    }

    /// Convenience: emit (label, ns) for every entry in
    /// [`PERCENTILE_POINTS`].
    pub fn percentile_table(&self) -> Vec<(&'static str, f64)> {
        PERCENTILE_POINTS
            .iter()
            .map(|(q, label)| (*label, self.percentile(*q)))
            .collect()
    }
}

/// Walk a criterion `target/criterion/<group>/<dim>/<row>/new/sample.json`
/// tree and collect every row's distribution. Returns rows sorted by
/// label for deterministic output.
///
/// Layout produced by criterion 0.5 when the bench builds rows via
/// `BenchmarkGroup` + `BenchmarkId::new(dim, row)`:
///
/// ```text
/// <group_root>/
///   <dim>/
///     <row>/
///       new/
///         sample.json
///         estimates.json
///         ...
/// ```
///
/// The bench under test (`trace_jit_hot_loop.rs`) uses
/// `group = v6_epsilon_hot_loop`, `dim = backend`, `row = ...`. For
/// generality we accept the **group root** directory and walk all
/// `<dim>/<row>/new/sample.json` children.
pub fn collect_group_stats(group_root: &Path) -> Result<Vec<RowStats>, BenchStatsError> {
    if !group_root.is_dir() {
        return Err(BenchStatsError::MissingGroupRoot(group_root.to_path_buf()));
    }
    let mut rows = Vec::new();
    for dim_entry in
        fs::read_dir(group_root).map_err(|e| BenchStatsError::Io(group_root.to_path_buf(), e))?
    {
        let dim_entry = dim_entry.map_err(|e| BenchStatsError::Io(group_root.to_path_buf(), e))?;
        let dim_path = dim_entry.path();
        if !dim_path.is_dir() {
            continue;
        }
        let dim_name = dim_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        // Skip criterion's own report aggregation dir.
        if dim_name == "report" {
            continue;
        }
        for row_entry in
            fs::read_dir(&dim_path).map_err(|e| BenchStatsError::Io(dim_path.clone(), e))?
        {
            let row_entry = row_entry.map_err(|e| BenchStatsError::Io(dim_path.clone(), e))?;
            let row_path = row_entry.path();
            if !row_path.is_dir() {
                continue;
            }
            let row_name = row_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if row_name == "report" {
                continue;
            }
            let new_dir = row_path.join("new");
            let sample_json = new_dir.join("sample.json");
            if !sample_json.exists() {
                continue;
            }
            let label = format!("{dim_name}/{row_name}");
            rows.push(RowStats::from_criterion_row_dir(label, &new_dir)?);
        }
    }
    rows.sort_by(|a, b| a.row.cmp(&b.row));
    Ok(rows)
}

/// Format the percentile table as a Markdown table — useful for
/// embedding in bench-rewrite stage reports.
///
/// Columns are **per element / per inner-loop-iter** when the row's
/// `elements_per_call` is set (the bench declared
/// `Throughput::Elements(N)` and N elements happen per closure call).
/// Otherwise columns are per closure call.
pub fn render_markdown_table(rows: &[RowStats]) -> String {
    let mut out = String::new();
    out.push_str("| Row | p50 (ns/elem) | p90 | p99 | p99.9 | max | samples | elements/call |\n");
    out.push_str("|---|---|---|---|---|---|---|---|\n");
    for row in rows {
        let p50 = row.per_element_percentile(0.50);
        let p90 = row.per_element_percentile(0.90);
        let p99 = row.per_element_percentile(0.99);
        let p999 = row.per_element_percentile(0.999);
        let max = row.per_element_percentile(1.00);
        let elem = match row.elements_per_call {
            Some(n) => format!("{n}"),
            None => "(per-call)".to_string(),
        };
        out.push_str(&format!(
            "| `{}` | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {} | {} |\n",
            row.row,
            p50,
            p90,
            p99,
            p999,
            max,
            row.per_sample_ns.len(),
            elem,
        ));
    }
    out
}

/// Public error surface for the bench-stats helper.
#[derive(Debug, thiserror::Error)]
pub enum BenchStatsError {
    #[error("I/O reading {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("failed to parse criterion sample JSON at {0}: {1}")]
    Parse(PathBuf, String),
    #[error("criterion sample arrays out of sync at {path}: iters={iters_len} times={times_len}")]
    ShapeMismatch {
        path: PathBuf,
        iters_len: usize,
        times_len: usize,
    },
    #[error("criterion sample arrays empty at {0}")]
    Empty(PathBuf),
    #[error("criterion group root missing or not a directory: {0}")]
    MissingGroupRoot(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_sample(path: &Path, iters: &[f64], times: &[f64]) {
        let body = format!(
            "{{\"sampling_mode\":\"Linear\",\"iters\":{:?},\"times\":{:?}}}",
            iters, times
        );
        let mut f = fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn percentile_extracts_p50_p99_max() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("sample.json");
        // Per-iter ns: 1, 2, 3, ..., 10
        let iters: Vec<f64> = (1..=10).map(|i| i as f64).collect();
        let times: Vec<f64> = iters
            .iter()
            .enumerate()
            .map(|(i, n)| (i as f64 + 1.0) * n)
            .collect();
        write_sample(&p, &iters, &times);
        let stats = RowStats::from_sample_json("dim/row", &p).unwrap();
        assert!((stats.percentile(0.5) - 5.5).abs() < 1e-6, "p50 = 5.5");
        assert!((stats.percentile(1.0) - 10.0).abs() < 1e-6, "max = 10");
        assert_eq!(stats.per_sample_ns.len(), 10);
    }

    #[test]
    fn collect_walks_group_tree() {
        let dir = tempdir().unwrap();
        let group = dir.path().join("group");
        let row_dir = group.join("backend").join("row_a").join("new");
        fs::create_dir_all(&row_dir).unwrap();
        write_sample(&row_dir.join("sample.json"), &[1.0, 2.0], &[10.0, 40.0]);
        let stats = collect_group_stats(&group).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].row, "backend/row_a");
        assert!((stats[0].percentile(0.0) - 10.0).abs() < 1e-6);
        assert!((stats[0].percentile(1.0) - 20.0).abs() < 1e-6);
    }

    #[test]
    fn percentile_table_renders_five_points() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("sample.json");
        let iters: Vec<f64> = vec![1.0; 100];
        let times: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        write_sample(&p, &iters, &times);
        let stats = RowStats::from_sample_json("dim/row", &p).unwrap();
        let table = stats.percentile_table();
        assert_eq!(table.len(), 5);
        assert_eq!(table[0].0, "p50");
        assert_eq!(table[4].0, "max");
    }
}
