//! `relon cache stats` / `relon cache clean` contract.
//!
//! The subcommand operates on the same directory the `auto` /
//! `cranelift-aot` backends populate (`$XDG_CACHE_HOME/relon`). Tests
//! run against an isolated `XDG_CACHE_HOME` tempdir and synthesize
//! artifact files with the three recognised suffixes directly, so
//! they exercise the CLI's scan / clean logic without depending on a
//! host that can run the dlopen pipeline.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_relon-cli");

const SUFFIXES: [&str; 3] = [".relon-native-v1", ".relon-ir-v1", ".relon-schema-v1"];

fn run_cache(xdg_cache: &Path, action: &str) -> Output {
    Command::new(BINARY)
        .env("XDG_CACHE_HOME", xdg_cache)
        .args(["cache", action])
        .output()
        .expect("spawn relon CLI")
}

fn stdout_of(output: &Output) -> String {
    assert!(
        output.status.success(),
        "cache subcommand failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Lay down one fake cache entry (all three artifact kinds) plus one
/// unrelated file; returns the cache dir.
fn populate(xdg_cache: &Path) -> PathBuf {
    let cache_dir = xdg_cache.join("relon");
    std::fs::create_dir_all(&cache_dir).expect("create cache dir");
    let stem = "0".repeat(64);
    for suffix in SUFFIXES {
        std::fs::write(cache_dir.join(format!("{stem}{suffix}")), b"payload")
            .expect("write artifact");
    }
    std::fs::write(cache_dir.join("unrelated.txt"), b"keep me").expect("write unrelated");
    cache_dir
}

#[test]
fn stats_on_missing_directory_reports_empty() {
    let xdg = tempfile::tempdir().expect("tempdir");
    let stdout = stdout_of(&run_cache(xdg.path(), "stats"));
    assert!(
        stdout.contains("entries: 0 (0 native objects, 0 IR blobs, 0 schema blobs)"),
        "unexpected stats output: {stdout}"
    );
    assert!(stdout.contains("total bytes: 0"), "{stdout}");
}

#[test]
fn stats_counts_artifacts_and_bytes() {
    let xdg = tempfile::tempdir().expect("tempdir");
    let cache_dir = populate(xdg.path());

    let stdout = stdout_of(&run_cache(xdg.path(), "stats"));
    assert!(
        stdout.contains(&format!("cache dir: {}", cache_dir.display())),
        "{stdout}"
    );
    assert!(
        stdout.contains("entries: 3 (1 native objects, 1 IR blobs, 1 schema blobs)"),
        "{stdout}"
    );
    // Three files x b"payload" (7 bytes).
    assert!(stdout.contains("total bytes: 21"), "{stdout}");
    assert!(
        stdout.contains("unrelated entries (not counted): 1"),
        "{stdout}"
    );
}

#[test]
fn clean_removes_artifacts_but_spares_unrelated_files() {
    let xdg = tempfile::tempdir().expect("tempdir");
    let cache_dir = populate(xdg.path());

    let stdout = stdout_of(&run_cache(xdg.path(), "clean"));
    assert!(
        stdout.contains("removed 3 cache artifact(s), freed 21 bytes"),
        "{stdout}"
    );
    assert!(
        stdout.contains("unrelated entries left untouched: 1"),
        "{stdout}"
    );

    let remaining: Vec<String> = std::fs::read_dir(&cache_dir)
        .expect("cache dir still exists")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(remaining, vec!["unrelated.txt".to_string()]);

    // A second clean over the now-empty cache is a no-op, not an error.
    let stdout = stdout_of(&run_cache(xdg.path(), "clean"));
    assert!(
        stdout.contains("removed 0 cache artifact(s), freed 0 bytes"),
        "{stdout}"
    );
}
