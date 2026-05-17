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

/// Phase a-1 CLI surface: `--fuel-limit 0` (the default) must not
/// change the answer the wasm-aot backend produces for a fold over a
/// 1000-element list. The point of the test is to lock down the
/// "unlimited" mode at the CLI layer — the per-call set_fuel reset
/// logic is exercised by the codegen-wasm smoke tests; here we only
/// want to catch a future refactor that accidentally clamps the limit
/// at the CLI boundary.
#[test]
fn fuel_limit_zero_completes_via_cli() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-fuel-limit-zero-{}.relon",
        std::process::id(),
    ));
    std::fs::write(
        &path,
        "#main(List<Int> xs) -> Int\nxs.fold(0, (Int acc, Int x) => acc + x)\n",
    )
    .expect("write fixture");

    let xs: Vec<i64> = (1..=1000).collect();
    let xs_json = serde_json::to_string(&xs).expect("encode xs");
    let args = format!("{{\"xs\": {xs_json}}}");

    let output = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("wasm-aot")
        .arg("--fuel-limit")
        .arg("0")
        .arg("--args")
        .arg(&args)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "CLI exited with non-zero status: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    // 1 + 2 + ... + 1000 = 500500
    assert_eq!(stdout.trim(), "500500", "stdout was: {stdout:?}");
}

/// Phase a-1: `--fuel-limit 10` against the same fold must fail —
/// the CLI must surface a non-zero exit and a `WasmStepLimitExceeded`
/// diagnostic on stderr. Catches a future refactor that silently
/// drops the flag or fails to wire it into `with_fuel_limit`.
#[test]
fn fuel_limit_tight_traps_via_cli() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-fuel-limit-tight-{}.relon",
        std::process::id(),
    ));
    std::fs::write(
        &path,
        "#main(List<Int> xs) -> Int\nxs.fold(0, (Int acc, Int x) => acc + x)\n",
    )
    .expect("write fixture");

    let xs: Vec<i64> = (1..=1000).collect();
    let xs_json = serde_json::to_string(&xs).expect("encode xs");
    let args = format!("{{\"xs\": {xs_json}}}");

    let output = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("wasm-aot")
        .arg("--fuel-limit")
        .arg("10")
        .arg("--args")
        .arg(&args)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        !output.status.success(),
        "CLI must exit non-zero on fuel exhaustion; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("step") || stderr.contains("fuel"),
        "stderr should mention step / fuel exhaustion: {stderr}"
    );
}
