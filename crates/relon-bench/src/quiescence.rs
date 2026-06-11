#![forbid(unsafe_code)]

//! v6-λ-machine (2026-05-19): read-only machine quiescence self-check used
//! by the LuaJIT comparison bench rounds (`trace_jit_hot_loop` and the
//! upcoming `cmp_lua` group).
//!
//! Mirrors `docs/internal/archive/relon-vs-luajit-rigorous-plan.md` §6. The companion
//! script `scripts/bench_quiescence.sh` performs the *privileged* setup
//! (cpu governor / no_turbo / perf-stat noise); this module verifies the
//! outcome at bench startup so a forgotten setup doesn't silently bias
//! results. Reading `/sys/devices/system/cpu/.../scaling_governor` and
//! `/proc/loadavg` requires no privileges.
//!
//! Behaviour:
//!
//! - Every CPU must report `scaling_governor = performance`. The check
//!   tolerates CPUs without a `cpufreq/scaling_governor` node (e.g. inside
//!   containers / inside QEMU) — missing nodes log a warning but don't fail.
//! - `intel_pstate/no_turbo` must be `1` if it exists. If only `cpufreq/boost`
//!   exists (AMD path), it must be `0`. If neither exists the host is taken to
//!   have no turbo knob (e.g. ARM) and the check passes.
//! - `/proc/loadavg` 1-minute load must be < 1.0 (machine is otherwise idle).
//! - All thermal zones are *logged*; they do not gate the run today (we don't
//!   know which zone is the bench CPU on every box).
//!
//! Override: setting `RELON_BENCH_FORCE_RUN=1` in the environment downgrades
//! every failure into a logged warning so dev iteration on locked-down
//! machines (e.g. CI without intel_pstate access) stays unblocked.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Override env var: setting this to a non-empty value bypasses the
/// quiescence gate (still emits the report to stderr).
pub const FORCE_RUN_ENV: &str = "RELON_BENCH_FORCE_RUN";

/// 1-minute loadavg threshold above which the machine is considered
/// "not otherwise idle". Loose enough to accommodate the bench process
/// itself (one core busy), strict enough to catch a stray docker / IDE.
pub const LOADAVG_MAX_1MIN: f64 = 1.0;

/// Outcome of the quiescence check. Always returned; the `errors` field
/// is non-empty iff at least one gate failed (modulo the
/// `RELON_BENCH_FORCE_RUN` override).
#[derive(Debug, Clone)]
pub struct QuiescenceReport {
    /// Per-CPU governor readings. Tuple is `(cpu_id, governor)`. Missing
    /// nodes report `"missing"` as the governor.
    pub governors: Vec<(u32, String)>,
    /// `intel_pstate/no_turbo` value if the node exists. `None` means
    /// the path doesn't exist on this host.
    pub no_turbo: Option<String>,
    /// `cpufreq/boost` value if the node exists (AMD path).
    pub boost: Option<String>,
    /// 1-minute load average from `/proc/loadavg`.
    pub loadavg_1min: f64,
    /// 5-minute load average for context (not gated on).
    pub loadavg_5min: f64,
    /// Per-thermal-zone temperatures in degrees Celsius. Tuple is
    /// `(zone_name, type, temp_celsius)`. Logged only, not gated.
    pub thermal: Vec<ThermalReading>,
    /// Accumulated gate failures. Empty iff the machine is quiescent.
    pub errors: Vec<QuiescenceFailure>,
    /// Set if `RELON_BENCH_FORCE_RUN` was non-empty when the check ran.
    pub force_run: bool,
}

#[derive(Debug, Clone)]
pub struct ThermalReading {
    pub zone: String,
    pub kind: String,
    pub celsius: f64,
}

/// Individual gate failures. Each carries enough context to be actionable.
#[derive(Debug, Clone)]
pub enum QuiescenceFailure {
    /// At least one CPU is not at performance governor.
    GovernorNotPerformance {
        cpus: Vec<u32>,
        observed: Vec<String>,
    },
    /// Intel `no_turbo` exists but is not `1`.
    TurboEnabled { observed: String },
    /// AMD `cpufreq/boost` exists and is not `0`.
    BoostEnabled { observed: String },
    /// 1-minute load average above threshold.
    LoadTooHigh { observed: f64, threshold: f64 },
    /// `/proc/loadavg` couldn't be read or parsed.
    LoadavgUnavailable { reason: String },
}

impl fmt::Display for QuiescenceFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GovernorNotPerformance { cpus, observed } => write!(
                f,
                "scaling_governor != performance on cpus {cpus:?} (observed: {observed:?}). \
                 Run `scripts/bench_quiescence.sh` (needs sudo)."
            ),
            Self::TurboEnabled { observed } => write!(
                f,
                "intel_pstate/no_turbo = {observed} (expected 1). \
                 `echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo`."
            ),
            Self::BoostEnabled { observed } => write!(
                f,
                "cpufreq/boost = {observed} (expected 0). \
                 `echo 0 | sudo tee /sys/devices/system/cpu/cpufreq/boost`."
            ),
            Self::LoadTooHigh {
                observed,
                threshold,
            } => write!(
                f,
                "/proc/loadavg 1-min = {observed:.2} (>= {threshold:.2}). \
                 Close background apps and re-run."
            ),
            Self::LoadavgUnavailable { reason } => {
                write!(f, "could not read /proc/loadavg: {reason}")
            }
        }
    }
}

/// Top-level error wrapping a [`QuiescenceReport`] that failed at least
/// one gate. `Display`s as a multi-line summary suitable for `panic!`.
///
/// Boxed to keep `Result<QuiescenceReport, _>` small (the report itself
/// is multi-kilobyte once every CPU governor is enumerated).
#[derive(Debug)]
pub struct QuiescenceError {
    pub report: Box<QuiescenceReport>,
}

impl fmt::Display for QuiescenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "machine not quiescent for bench:")?;
        for err in &self.report.errors {
            writeln!(f, "  - {err}")?;
        }
        writeln!(
            f,
            "(set {FORCE_RUN_ENV}=1 to bypass and run anyway; expect noisy results.)"
        )?;
        Ok(())
    }
}

impl std::error::Error for QuiescenceError {}

/// Convenience entry point: returns `Ok(report)` if the machine is
/// quiescent (or `RELON_BENCH_FORCE_RUN=1`), `Err(QuiescenceError)`
/// otherwise. The error embeds the full report so callers can log
/// everything before bailing.
pub fn verify_quiescence() -> Result<QuiescenceReport, QuiescenceError> {
    let report = collect_report();
    let force_run = report.force_run;
    if report.errors.is_empty() || force_run {
        Ok(report)
    } else {
        Err(QuiescenceError {
            report: Box::new(report),
        })
    }
}

/// Collect a fresh quiescence report from sysfs / procfs. Pure read-only;
/// always succeeds (errors become `QuiescenceFailure` entries in the
/// report rather than `Err`).
pub fn collect_report() -> QuiescenceReport {
    let force_run = std::env::var(FORCE_RUN_ENV)
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    let mut errors = Vec::new();

    // ---- CPU governor --------------------------------------------------
    let governors = read_governors();
    let (non_perf_cpus, non_perf_observed) = governors
        .iter()
        .filter(|(_, g)| g != "performance" && g != "missing")
        .fold(
            (Vec::new(), Vec::new()),
            |(mut cpus, mut obs), (cpu, gov)| {
                cpus.push(*cpu);
                obs.push(gov.clone());
                (cpus, obs)
            },
        );
    if !non_perf_cpus.is_empty() {
        errors.push(QuiescenceFailure::GovernorNotPerformance {
            cpus: non_perf_cpus,
            observed: non_perf_observed,
        });
    }

    // ---- Turbo / boost -------------------------------------------------
    let no_turbo = read_sysfs("/sys/devices/system/cpu/intel_pstate/no_turbo").ok();
    if let Some(ref v) = no_turbo {
        if v != "1" {
            errors.push(QuiescenceFailure::TurboEnabled {
                observed: v.clone(),
            });
        }
    }
    let boost = read_sysfs("/sys/devices/system/cpu/cpufreq/boost").ok();
    if let Some(ref v) = boost {
        if v != "0" && no_turbo.is_none() {
            // Only enforce the boost knob when no_turbo is unavailable;
            // otherwise no_turbo is the canonical knob and boost is a
            // sibling indicator.
            errors.push(QuiescenceFailure::BoostEnabled {
                observed: v.clone(),
            });
        }
    }

    // ---- Load average --------------------------------------------------
    let (loadavg_1min, loadavg_5min) = match read_loadavg() {
        Ok(la) => la,
        Err(e) => {
            errors.push(QuiescenceFailure::LoadavgUnavailable {
                reason: e.to_string(),
            });
            (0.0, 0.0)
        }
    };
    if loadavg_1min >= LOADAVG_MAX_1MIN {
        errors.push(QuiescenceFailure::LoadTooHigh {
            observed: loadavg_1min,
            threshold: LOADAVG_MAX_1MIN,
        });
    }

    // ---- Thermal (log only) -------------------------------------------
    let thermal = read_thermal_zones();

    QuiescenceReport {
        governors,
        no_turbo,
        boost,
        loadavg_1min,
        loadavg_5min,
        thermal,
        errors,
        force_run,
    }
}

/// Returns `[(cpu_id, governor)]` for every `/sys/devices/system/cpu/cpu*/`
/// node whose name matches `cpu<digits>`. CPUs without a `cpufreq/scaling_governor`
/// file get governor `"missing"` (e.g. offline CPUs).
fn read_governors() -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let root = Path::new("/sys/devices/system/cpu");
    let entries = match fs::read_dir(root) {
        Ok(it) => it,
        Err(_) => return out,
    };
    let mut paths: Vec<(u32, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let rest = name.strip_prefix("cpu")?;
            let id: u32 = rest.parse().ok()?;
            Some((id, e.path()))
        })
        .collect();
    paths.sort_by_key(|(id, _)| *id);
    for (id, path) in paths {
        let gov_file = path.join("cpufreq").join("scaling_governor");
        let gov = match fs::read_to_string(&gov_file) {
            Ok(s) => s.trim().to_string(),
            Err(_) => "missing".to_string(),
        };
        out.push((id, gov));
    }
    out
}

fn read_sysfs<P: AsRef<Path>>(path: P) -> io::Result<String> {
    fs::read_to_string(path).map(|s| s.trim().to_string())
}

fn read_loadavg() -> io::Result<(f64, f64)> {
    let s = fs::read_to_string("/proc/loadavg")?;
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "loadavg malformed",
        ));
    }
    let one: f64 = parts[0]
        .parse()
        .map_err(|e: std::num::ParseFloatError| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let five: f64 = parts[1]
        .parse()
        .map_err(|e: std::num::ParseFloatError| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((one, five))
}

fn read_thermal_zones() -> Vec<ThermalReading> {
    let mut out = Vec::new();
    let root = Path::new("/sys/class/thermal");
    let entries = match fs::read_dir(root) {
        Ok(it) => it,
        Err(_) => return out,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let kind = fs::read_to_string(path.join("type"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "?".to_string());
        let celsius = fs::read_to_string(path.join("temp"))
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .map(|m| m / 1000.0)
            .unwrap_or(0.0);
        out.push(ThermalReading {
            zone: name,
            kind,
            celsius,
        });
    }
    out.sort_by(|a, b| a.zone.cmp(&b.zone));
    out
}

impl QuiescenceReport {
    /// Pretty-print a short summary suitable for the bench log header.
    pub fn summary(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let perf_count = self
            .governors
            .iter()
            .filter(|(_, g)| g == "performance")
            .count();
        let _ = writeln!(
            s,
            "machine quiescence: governors={perf_count}/{total} perf, no_turbo={nt}, boost={b}, load1={la1:.2}, errors={ne}",
            total = self.governors.len(),
            nt = self.no_turbo.as_deref().unwrap_or("(n/a)"),
            b = self.boost.as_deref().unwrap_or("(n/a)"),
            la1 = self.loadavg_1min,
            ne = self.errors.len(),
        );
        for t in &self.thermal {
            let _ = writeln!(s, "  thermal {} ({}): {:.1} C", t.zone, t.kind, t.celsius);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_report_does_not_panic() {
        // Whatever the host state, collecting must succeed without panic.
        let report = collect_report();
        // Either we found at least one cpu node, or the test runs in a
        // container without /sys/devices/system/cpu. Both are allowed.
        let _ = report.summary();
    }

    #[test]
    fn force_run_env_toggles_pass() {
        // The unit test cannot mutate sysfs, but we can assert that the
        // override flag is picked up. To avoid interfering with other
        // tests we read directly (no env mutation needed): if the var is
        // set in the env, force_run is true; else false.
        let r = collect_report();
        let env_set = std::env::var(FORCE_RUN_ENV)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        assert_eq!(r.force_run, env_set);
    }

    #[test]
    fn governor_failure_displays_actionable_hint() {
        let fail = QuiescenceFailure::GovernorNotPerformance {
            cpus: vec![0, 1],
            observed: vec!["powersave".into(), "powersave".into()],
        };
        let msg = format!("{fail}");
        assert!(msg.contains("bench_quiescence.sh"));
        assert!(msg.contains("performance"));
    }
}
