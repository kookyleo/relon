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

/// v6-fix-D2 cold-start: `--lite` forces tree-walk and skips the
/// core-carrier analyzer pass. For a `#main(Int x) -> Int : x * 2`
/// shape the answer must match the default path exactly. This is a
/// behavior contract, not a perf gate — perf lives in
/// `crates/relon-bench/benches/cmp_lua.rs`.
#[test]
fn lite_mode_matches_default_on_scalar_main() {
    let path = std::env::temp_dir().join(format!("relon-cli-lite-{}.relon", std::process::id()));
    std::fs::write(&path, "#main(Int x) -> Int\nx * 2\n").expect("write fixture");

    let run = |extra: &[&str]| -> String {
        let mut cmd = Command::new(BINARY);
        cmd.arg("run").arg(&path).arg("--args").arg(r#"{"x": 21}"#);
        for e in extra {
            cmd.arg(e);
        }
        let out = cmd.output().expect("spawn relon CLI");
        assert!(
            out.status.success(),
            "CLI exited non-zero for {extra:?}: stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8(out.stdout).expect("utf-8 stdout")
    };

    let default_out = run(&[]);
    let lite_out = run(&["--lite"]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(
        default_out.trim(),
        "42",
        "default stdout was: {default_out:?}"
    );
    assert_eq!(lite_out.trim(), "42", "lite stdout was: {lite_out:?}");
    assert_eq!(default_out, lite_out, "default vs --lite output differs");
}

/// v6-fix-D2: `--lite` is incompatible with cranelift-AOT / bytecode
/// — the flag's contract is "force tree-walk plus skip heavy lazy
/// init", so a conflicting `--backend` argument must surface as a
/// clean error rather than silently swap the backend.
#[test]
fn lite_rejects_cranelift_aot_backend() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-lite-reject-{}.relon",
        std::process::id()
    ));
    std::fs::write(&path, "#main(Int x) -> Int\nx + 1\n").expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--lite")
        .arg("--backend")
        .arg("cranelift-aot")
        .arg("--args")
        .arg(r#"{"x": 41}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        !out.status.success(),
        "CLI accepted --lite with cranelift-aot; should have errored"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--lite"),
        "stderr should mention --lite; got: {err}"
    );
}
