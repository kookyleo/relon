//! Phase 8 CLI parity test: drives a tiny `#main`-style program
//! through both backends and confirms the JSON output matches.
//!
//! Cargo writes the integration-test binary path into `CARGO_BIN_EXE_*`
//! at link time, so the test can spawn the freshly-built `relon`
//! binary without an off-tree path lookup.

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
fn wasm_aot_backend_runs_main() {
    let out = run_doubler("wasm-aot");
    assert!(out.trim() == "42", "wasm-aot stdout was: {out:?}");
}

#[test]
fn backends_produce_identical_output() {
    let tw = run_doubler("tree-walk");
    let aot = run_doubler("wasm-aot");
    assert_eq!(tw, aot, "tree-walk vs wasm-aot output differs");
}

/// Phase 10-b end-to-end: an entry file that pulls a `#schema` from
/// an imported module runs cleanly under `--backend wasm-aot`. Before
/// Phase 10-b the wasm-AOT backend rejected the workspace because
/// `lower_workspace_single` could not see cross-file declarations;
/// the post-fix path uses `from_workspace` and resolves `User`.
#[test]
fn wasm_aot_backend_resolves_cross_file_schema() {
    let dir =
        std::env::temp_dir().join(format!("relon-cli-phase10b-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    let util_path = dir.join("util.relon");
    let main_path = dir.join("main.relon");
    std::fs::write(&util_path, "#schema User { Int age: * }\n{}\n").expect("write util");
    std::fs::write(
        &main_path,
        "#import * from \"./util.relon\"\n#main(User u) -> Int\nu.age * 2\n",
    )
    .expect("write main");

    let output = Command::new(BINARY)
        .arg("run")
        .arg(&main_path)
        .arg("--backend")
        .arg("wasm-aot")
        // The fixture lives outside the std/* virtual namespace, so we
        // hand the CLI `--trust` to enable the filesystem resolver.
        // Otherwise the workspace pass would refuse to load
        // `./util.relon` with a sandbox `ModuleNotFound`.
        .arg("--trust")
        .arg("--args")
        .arg(r#"{"u": {"age": 21}}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        output.status.success(),
        "CLI exited with non-zero status: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert_eq!(stdout.trim(), "42", "wasm-aot stdout was: {stdout:?}");
}
