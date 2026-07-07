//! `relon run --pretty` contract.
//!
//! The flag was historically dead: `bool` + `default_value_t = true`
//! derived clap's `SetTrue` action, so the value was `true` no matter
//! what the operator passed and the compact serialization branch was
//! unreachable. These tests pin the repaired surface:
//!
//! * default        -> pretty (multi-line, indented) JSON
//! * `--pretty`     -> pretty JSON (explicit spelling of the default)
//! * `--pretty=true`  -> pretty JSON
//! * `--pretty=false` -> compact single-line JSON
//!
//! Both spellings must produce byte-equivalent *values* (only the
//! whitespace differs), and the exit code stays 0 throughout.

use std::path::PathBuf;
use std::process::Command;

const BINARY: &str = env!("CARGO_BIN_EXE_relon-cli");

/// Library-mode source with enough nesting that pretty and compact
/// serializations visibly differ (an object always splits across
/// lines under `to_string_pretty`).
const SOURCE: &str = r#"{
    "name": "pretty-probe",
    "ports": [80, 443],
    "nested": { "enabled": true }
}
"#;

fn write_probe(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("probe.relon");
    std::fs::write(&path, SOURCE).expect("write probe.relon");
    path
}

fn run_ok(args: &[&str], file: &PathBuf) -> String {
    let output = Command::new(BINARY)
        .arg("run")
        .arg("--backend")
        .arg("tree-walk")
        .args(args)
        .arg(file)
        .output()
        .expect("spawn relon CLI");
    assert!(
        output.status.success(),
        "relon run {args:?} failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout is UTF-8")
}

#[test]
fn default_and_explicit_pretty_are_multiline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = write_probe(&dir);

    for args in [&[][..], &["--pretty"][..], &["--pretty=true"][..]] {
        let stdout = run_ok(args, &file);
        assert!(
            stdout.trim_end().lines().count() > 1,
            "expected pretty (multi-line) output for {args:?}, got: {stdout}"
        );
    }
}

#[test]
fn pretty_false_is_single_line_compact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = write_probe(&dir);

    let stdout = run_ok(&["--pretty=false"], &file);
    assert_eq!(
        stdout.trim_end().lines().count(),
        1,
        "expected compact single-line output, got: {stdout}"
    );
}

#[test]
fn pretty_and_compact_agree_on_the_value() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = write_probe(&dir);

    let pretty: serde_json::Value =
        serde_json::from_str(&run_ok(&[], &file)).expect("pretty output parses as JSON");
    let compact: serde_json::Value = serde_json::from_str(&run_ok(&["--pretty=false"], &file))
        .expect("compact output parses as JSON");
    assert_eq!(pretty, compact, "serialization mode changed the value");
}
