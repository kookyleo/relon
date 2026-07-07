//! `relon check --format json` contract.
//!
//! The JSON mode prints a single JSON array of diagnostic objects to
//! stdout. Each object carries `code`, `severity`, `message`, `file`,
//! `start` / `end` (1-based `{line, column}` objects, null when no
//! source position is known) and `help` (null when absent). Exit-code
//! semantics match the human mode: non-zero iff the check fails.
//!
//! Follows the `docs_contract.rs` precedent of stripping inherited
//! color-forcing env vars so stderr assertions cannot be poisoned by
//! the runner's shell.

use std::path::PathBuf;
use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_relon-cli");

fn run_check(args: &[&str], file: &PathBuf) -> Output {
    let mut command = Command::new(BINARY);
    strip_color_env(&mut command);
    command
        .arg("check")
        .args(args)
        .arg(file)
        .output()
        .expect("spawn relon CLI")
}

/// Same rationale as `docs_contract.rs`: color-forcing variables
/// inherited from the test runner would make miette emit ANSI codes.
fn strip_color_env(command: &mut Command) {
    for var in ["FORCE_COLOR", "NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE"] {
        command.env_remove(var);
    }
}

fn write_fixture(dir: &tempfile::TempDir, name: &str, source: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, source).expect("write fixture");
    path
}

fn parse_stdout_array(output: &Output) -> Vec<serde_json::Value> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<Vec<serde_json::Value>>(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not a JSON array: {e}\nstdout: {stdout}"))
}

#[test]
fn clean_file_yields_empty_array_and_exit_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = write_fixture(&dir, "clean.relon", "{ \"port\": 8080 }\n");

    let output = run_check(&["--format", "json"], &file);
    assert!(
        output.status.success(),
        "clean check failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(parse_stdout_array(&output), Vec::<serde_json::Value>::new());
}

#[test]
fn analyzer_error_yields_structured_diagnostics_and_nonzero_exit() {
    let dir = tempfile::tempdir().expect("tempdir");
    // `Bogus` is not a builtin or user-declared type: a stable
    // `unknown_type_name` error with a well-defined source span.
    let file = write_fixture(&dir, "bad.relon", "#main(Bogus x) -> Int\nx\n");

    let output = run_check(&["--format", "json"], &file);
    assert!(!output.status.success(), "expected non-zero exit");

    let entries = parse_stdout_array(&output);
    assert!(!entries.is_empty(), "expected at least one diagnostic");
    let unknown_type = entries
        .iter()
        .find(|e| e["code"] == "relon::analyze::unknown_type_name")
        .expect("unknown_type_name diagnostic present");

    assert_eq!(unknown_type["severity"], "error");
    assert_eq!(
        unknown_type["message"],
        serde_json::json!("unknown type name `Bogus`")
    );
    assert_eq!(
        unknown_type["file"].as_str().expect("file is a string"),
        file.canonicalize().expect("canonicalize").to_string_lossy()
    );
    // `Bogus` sits on line 1, columns 7..12 (1-based, end-exclusive
    // span rendered as the position one past the last character).
    assert_eq!(
        unknown_type["start"],
        serde_json::json!({"line": 1, "column": 7})
    );
    assert_eq!(
        unknown_type["end"],
        serde_json::json!({"line": 1, "column": 12})
    );
    assert!(
        unknown_type["help"].as_str().is_some_and(|h| !h.is_empty()),
        "help should be present for unknown_type_name"
    );
}

#[test]
fn entry_parse_error_recovers_a_real_position() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = write_fixture(&dir, "broken.relon", "{ \"a\": \n");

    let output = run_check(&["--format", "json"], &file);
    assert!(!output.status.success(), "expected non-zero exit");

    let entries = parse_stdout_array(&output);
    let parse_error = entries
        .iter()
        .find(|e| e["code"] == "relon::workspace::module_parse_error")
        .expect("module_parse_error diagnostic present");
    assert_eq!(parse_error["severity"], "error");
    // The workspace records the raw parse error with a zero span; the
    // JSON renderer re-parses the entry to recover the real position,
    // so the span must not be null.
    assert!(
        parse_error["start"]["line"].as_u64().is_some(),
        "parse error should carry a recovered start position: {parse_error}"
    );
}

#[test]
fn backend_incompatibility_is_reported_as_a_diagnostic() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Library-mode file (no `#main`) is a hard incompatibility for
    // the explicit cranelift-aot backend in both output formats.
    let file = write_fixture(&dir, "lib.relon", "{ \"port\": 8080 }\n");

    let json_output = run_check(&["--format", "json", "--backend", "cranelift-aot"], &file);
    assert!(!json_output.status.success(), "expected non-zero exit");
    let entries = parse_stdout_array(&json_output);
    let incompat = entries
        .iter()
        .find(|e| e["code"] == "relon::check::backend_incompatible")
        .expect("backend_incompatible diagnostic present");
    assert_eq!(incompat["severity"], "error");
    assert!(incompat["start"].is_null() && incompat["end"].is_null());

    // Exit-code parity with human mode on the same input.
    let human_output = run_check(&["--backend", "cranelift-aot"], &file);
    assert_eq!(json_output.status.code(), human_output.status.code());
}

#[test]
fn human_mode_output_is_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = write_fixture(&dir, "clean.relon", "{ \"port\": 8080 }\n");

    let output = run_check(&[], &file);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout,
        "ok: analyzer\nbackend auto: compatible (library-mode routes to tree-walk)\n"
    );
}
