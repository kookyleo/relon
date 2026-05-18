//! CLI parity test: drives a tiny `#main`-style program through both
//! supported backends and confirms the JSON output matches.
//!
//! v5-β-2 stage 4: wasm-AOT was retired here. The only backends the
//! CLI now exposes are `tree-walk` (the interpreter) and
//! `cranelift-aot` (native machine code via cranelift JIT). The old
//! `--backend wasm-aot` flag is gone; callers should migrate to
//! `--backend cranelift-aot` (or `--backend auto`, which routes
//! `run_main` through cranelift transparently).

use std::process::Command;

const BINARY: &str = env!("CARGO_BIN_EXE_relon-cli");

/// Write a one-off `#main(Int x) -> Int : x * 2` source file under
/// the system temp dir and run the CLI against it with the supplied
/// backend flag plus `--args`. Returns the captured stdout (utf-8).
fn run_doubler(backend: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-backend-{}-{}.relon",
        std::process::id(),
        backend.replace('-', "_"),
    ));
    std::fs::write(&path, "#main(Int x) -> Int\nx * 2\n").expect("write fixture");

    let output = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg(backend)
        .arg("--args")
        .arg(r#"{"x": 21}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "CLI exited with non-zero status for backend {backend}: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("utf-8 stdout")
}

#[test]
fn tree_walk_backend_runs_main() {
    let out = run_doubler("tree-walk");
    assert!(out.trim() == "42", "tree-walk stdout was: {out:?}");
}

#[test]
fn cranelift_aot_backend_runs_main() {
    let out = run_doubler("cranelift-aot");
    assert!(out.trim() == "42", "cranelift-aot stdout was: {out:?}");
}

#[test]
fn backends_produce_identical_output() {
    let tw = run_doubler("tree-walk");
    let aot = run_doubler("cranelift-aot");
    assert_eq!(tw, aot, "tree-walk vs cranelift-aot output differs");
}
