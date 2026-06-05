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
use std::sync::atomic::{AtomicU64, Ordering};

const BINARY: &str = env!("CARGO_BIN_EXE_relon-cli");

/// Per-call counter so parallel tests in this file each get a unique
/// fixture path. `std::process::id()` is shared across all tests in
/// the same binary, and the previous "{pid}-{backend}" scheme
/// collided when `backends_produce_identical_output` and
/// `tree_walk_backend_runs_main` both used `tree_walk` concurrently:
/// one would `remove_file` mid-spawn of the other and the CLI saw a
/// missing path. The counter discriminates every invocation.
static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write a one-off `#main(Int x) -> Int : x * 2` source file under
/// the system temp dir and run the CLI against it with the supplied
/// backend flag plus `--args`. Returns the captured stdout (utf-8).
fn run_doubler(backend: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-backend-{}-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
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

/// F1b: a cross-region object return
/// (`#main(List<Server> servers) -> Dict { servers: servers, n: 1 }`)
/// with a CJK String field must produce byte-identical JSON on tree-walk
/// and cranelift-AOT. The object head is built in `out_buf` but the
/// `servers` field points at parameter data in `in_buf`; the cranelift
/// path runs the multi-region verifier + cross-region reader, and its
/// JSON must match the tree-walk oracle exactly.
#[test]
fn cross_region_object_cjk_byte_equal_across_backends() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-crossregion-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#schema Server { name: String, port: Int }\n\
         #main(List<Server> servers) -> Dict\n{ servers: servers, n: 1 }\n",
    )
    .expect("write fixture");

    // A multibyte CJK name (U+4E2D U+6587), an empty-string element, and an
    // ASCII one, so the cross-region String + list walk is exercised. The
    // CJK is built from code points so this source file stays ASCII-only.
    let cjk: String = [0x4E2Du32, 0x6587u32]
        .iter()
        .map(|c| char::from_u32(*c).unwrap())
        .collect();
    let args_json = format!(
        r#"{{"servers":[{{"name":"{cjk}","port":8080}},{{"name":"","port":0}},{{"name":"edge","port":-1}}]}}"#
    );

    let run = |backend: &str| -> String {
        let out = Command::new(BINARY)
            .arg("run")
            .arg(&path)
            .arg("--backend")
            .arg(backend)
            .arg("--args")
            .arg(&args_json)
            .output()
            .expect("spawn relon CLI");
        assert!(
            out.status.success(),
            "CLI exited non-zero for {backend}: stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8(out.stdout).expect("utf-8 stdout")
    };

    let tw = run("tree-walk");
    let aot = run("cranelift-aot");
    let _ = std::fs::remove_file(&path);

    assert!(
        tw.contains(&cjk) && tw.contains("8080"),
        "tree-walk output must carry the CJK field + port: {tw:?}"
    );
    assert_eq!(
        tw, aot,
        "tree-walk vs cranelift-aot cross-region object JSON differs:\n  tw  = {tw:?}\n  aot = {aot:?}"
    );
}

/// `--trust` is honoured by tree-walk + bytecode but is a no-op on the
/// cranelift-AOT backend (no host-fn registry to grant capabilities
/// from). It must NOT be silently dropped: the CLI warns on stderr so
/// the operator is not misled, while the run still succeeds. tree-walk,
/// which DOES honour `--trust`, must not emit that warning.
#[test]
fn trust_on_cranelift_aot_warns_but_still_runs() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-trust-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(&path, "#main(Int x) -> Int\nx * 2\n").expect("write fixture");

    let run = |backend: &str| -> (String, String) {
        let out = Command::new(BINARY)
            .arg("run")
            .arg(&path)
            .arg("--trust")
            .arg("--backend")
            .arg(backend)
            .arg("--args")
            .arg(r#"{"x": 21}"#)
            .output()
            .expect("spawn relon CLI");
        assert!(
            out.status.success(),
            "CLI exited non-zero for {backend} --trust: stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
        (
            String::from_utf8(out.stdout).expect("utf-8 stdout"),
            String::from_utf8(out.stderr).expect("utf-8 stderr"),
        )
    };

    let (aot_out, aot_err) = run("cranelift-aot");
    assert_eq!(
        aot_out.trim(),
        "42",
        "cranelift-aot --trust still runs: {aot_out:?}"
    );
    assert!(
        aot_err.contains("--trust has no effect on the cranelift-AOT backend"),
        "cranelift-aot must warn that --trust is a no-op; stderr was: {aot_err:?}"
    );

    let (tw_out, tw_err) = run("tree-walk");
    assert_eq!(tw_out.trim(), "42", "tree-walk --trust runs: {tw_out:?}");
    assert!(
        !tw_err.contains("--trust has no effect"),
        "tree-walk honours --trust and must not emit the no-op warning; stderr: {tw_err:?}"
    );

    let _ = std::fs::remove_file(&path);
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
