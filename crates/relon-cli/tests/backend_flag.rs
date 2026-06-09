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

/// F3: a cross-region **branded-struct** return
/// (`#main(List<Server> servers) -> Wrapper { servers: servers, n: 7 }`)
/// with a CJK String field must produce byte-identical JSON on tree-walk
/// and cranelift-AOT. Same cross-region mechanism as the anon-`Dict` case
/// above, but reached via the branded dict-into-record lowering path.
#[test]
fn cross_region_branded_struct_cjk_byte_equal_across_backends() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-crossregion-branded-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#schema Server { name: String, port: Int }\n\
         #schema Wrapper { servers: List<Server>, n: Int }\n\
         #main(List<Server> servers) -> Wrapper { servers: servers, n: 7 }\n",
    )
    .expect("write fixture");

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
        tw.contains(&cjk) && tw.contains("8080") && tw.contains('7'),
        "tree-walk output must carry the CJK field + port + n: {tw:?}"
    );
    assert_eq!(
        tw, aot,
        "tree-walk vs cranelift-aot cross-region branded-struct JSON differs:\n  tw  = {tw:?}\n  aot = {aot:?}"
    );
}

/// F4: a top-level parameter-**field** list return
/// (`#main(Outer o) -> List<Server>\no.items`) with a CJK String field
/// must produce byte-identical JSON on tree-walk and cranelift-AOT. The
/// returned list is `o`'s field, reached through a field walk; post-F1 the
/// field-load pushes the field list root's arena-absolute offset and the
/// in-place region-walk return + verifier decode it cross-region.
#[test]
fn param_field_list_return_cjk_byte_equal_across_backends() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-paramfield-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#schema Server { name: String, port: Int }\n\
         #schema Outer { items: List<Server>, n: Int }\n\
         #main(Outer o) -> List<Server>\no.items\n",
    )
    .expect("write fixture");

    let cjk: String = [0x4E2Du32, 0x6587u32]
        .iter()
        .map(|c| char::from_u32(*c).unwrap())
        .collect();
    let args_json = format!(
        r#"{{"o":{{"items":[{{"name":"{cjk}","port":8080}},{{"name":"","port":0}},{{"name":"edge","port":-1}}],"n":3}}}}"#
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
        "tree-walk vs cranelift-aot parameter-field list JSON differs:\n  tw  = {tw:?}\n  aot = {aot:?}"
    );
}

/// F6: a deep nested-schema field chain (`o.inner.tags`) returned from the
/// CLI must produce byte-identical JSON on tree-walk and cranelift-AOT.
/// The chain descends through an intermediate `Inner` sub-record to a
/// `List<String>` leaf; the cross-region in-place return must reproduce
/// the CJK / empty / multi-element list exactly.
#[test]
fn deep_chain_list_return_cjk_byte_equal_across_backends() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-deepchain-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#schema Inner { tags: List<String>, n: Int }\n\
         #schema Outer { inner: Inner, m: Int }\n\
         #main(Outer o) -> List<String>\no.inner.tags\n",
    )
    .expect("write fixture");

    let cjk: String = [0x4E2Du32, 0x6587u32]
        .iter()
        .map(|c| char::from_u32(*c).unwrap())
        .collect();
    let args_json = format!(r#"{{"o":{{"inner":{{"tags":["{cjk}","","edge"],"n":3}},"m":9}}}}"#);

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
        tw.contains(&cjk) && tw.contains("edge"),
        "tree-walk output must carry the deep-chain CJK + element: {tw:?}"
    );
    assert_eq!(
        tw, aot,
        "tree-walk vs cranelift-aot deep-chain list JSON differs:\n  tw  = {tw:?}\n  aot = {aot:?}"
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

#[test]
fn cli_args_decode_builtin_tuple_array_with_main_signature() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-builtin-tuple-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#main(Tuple<Int, String> pair) -> String\n\
         pair.1\n",
    )
    .expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--args")
        .arg(r#"{"pair":[7,"x"]}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "CLI must decode JSON array as Value::Tuple for Tuple<...>; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    assert_eq!(stdout.trim(), r#""x""#, "stdout was: {stdout:?}");
}

#[test]
fn cli_args_decode_tuple_schema_array_with_main_signature() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-tuple-schema-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#schema IPv4 (Int, Int, Int, Int)\n\
         #main(IPv4 ip) -> Int\n\
         ip.0 + ip.1 + ip.2 + ip.3\n",
    )
    .expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--args")
        .arg(r#"{"ip":[127,0,0,1]}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "CLI must decode JSON array as Value::Tuple for IPv4; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    assert_eq!(stdout.trim(), "128", "stdout was: {stdout:?}");
}

#[test]
fn cli_args_decode_nested_tuple_schema_field() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-nested-tuple-schema-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#schema Pair (Int, String)\n\
         #schema Wrapper { Pair pair: * }\n\
         #main(Wrapper w) -> String\n\
         w.pair.1\n",
    )
    .expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--args")
        .arg(r#"{"w":{"pair":[7,"x"]}}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "CLI must recursively decode schema tuple fields; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    assert_eq!(stdout.trim(), r#""x""#, "stdout was: {stdout:?}");
}

#[test]
fn cli_args_decode_json_null_as_option_none_when_typed() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-option-null-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(&path, "#main(Option<Int> maybe) -> Option<Int>\nmaybe\n")
        .expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--args")
        .arg(r#"{"maybe":null}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "Option<Int> JSON null should decode as Option.None; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    assert_eq!(stdout.trim(), "null", "stdout was: {stdout:?}");
}

#[test]
fn cli_args_reject_targetless_json_null() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-targetless-null-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(&path, "#main(Int x) -> Int\nx\n").expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--args")
        .arg(r#"{"x":1,"extra":null}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        !out.status.success(),
        "targetless JSON null should be rejected; stdout={}",
        String::from_utf8_lossy(&out.stdout),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("JSON null needs an Option<T> target type"),
        "stderr should explain targetless null; got: {stderr}"
    );
}

#[test]
fn cli_args_reject_nested_targetless_json_null() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-targetless-nested-null-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#main(Int x) -> Int
x
",
    )
    .expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--args")
        .arg(r#"{"x":1,"extra":{"k":null}}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        !out.status.success(),
        "nested targetless JSON null should be rejected; stdout={}",
        String::from_utf8_lossy(&out.stdout),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("JSON null is not a Relon value")
            || stderr.contains("JSON null needs an Option<T> target type"),
        "stderr should explain targetless null; got: {stderr}"
    );
}

#[test]
fn cli_args_keep_list_semantics_and_reject_heterogeneous_list() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-list-mismatch-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(&path, "#main(List<Int> xs) -> Int\nlen(xs)\n").expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--args")
        .arg(r#"{"xs":[1,"x"]}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        !out.status.success(),
        "List<Int> must reject a JSON array with a string element; stdout={}",
        String::from_utf8_lossy(&out.stdout),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("List<Int>")
            && stderr.contains("expected JSON")
            && stderr.contains("integer"),
        "stderr should report the list element type mismatch; got: {stderr}"
    );
}

#[test]
fn cli_args_decode_unit_enum_variant_from_string() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-enum-string-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#enum Stat { Up, Down }\n\
         #main(Stat s) -> Stat\n\
         s\n",
    )
    .expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--args")
        .arg(r#"{"s":"Up"}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "CLI should decode JSON string as a unit enum variant; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
    assert_eq!(
        json,
        serde_json::json!({ "Up": {} }),
        "stdout was: {stdout:?}"
    );
}

#[test]
fn cli_args_decode_spread_imported_unit_enum_variant_from_string() {
    let dir = std::env::temp_dir().join(format!(
        "relon-cli-import-enum-spread-{}-{}",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let main_path = dir.join("main.relon");
    let lib_path = dir.join("lib.relon");
    std::fs::write(&lib_path, "#enum Stat { Up, Down }\n{}\n").expect("write lib fixture");
    std::fs::write(
        &main_path,
        "#import * from \"./lib.relon\"\n#main(Stat s) -> Stat\ns\n",
    )
    .expect("write main fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&main_path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--trust")
        .arg("--args")
        .arg(r#"{"s":"Up"}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.status.success(),
        "CLI should decode JSON string as a spread-imported unit enum variant; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
    assert_eq!(json, serde_json::json!({ "Up": {} }));
}

#[test]
fn cli_args_decode_alias_imported_unit_enum_variant_from_string() {
    let dir = std::env::temp_dir().join(format!(
        "relon-cli-import-enum-alias-{}-{}",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let main_path = dir.join("main.relon");
    let lib_path = dir.join("lib.relon");
    std::fs::write(&lib_path, "#enum Stat { Up, Down }\n{}\n").expect("write lib fixture");
    std::fs::write(
        &main_path,
        "#import lib from \"./lib.relon\"\n#main(lib.Stat s) -> lib.Stat\ns\n",
    )
    .expect("write main fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&main_path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--trust")
        .arg("--args")
        .arg(r#"{"s":"Down"}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.status.success(),
        "CLI should decode JSON string as an alias-imported unit enum variant; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
    assert_eq!(json, serde_json::json!({ "Down": {} }));
}

#[test]
fn cli_args_reject_payload_enum_variant_from_string() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-enum-payload-string-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#enum Msg { Email { address: String }, Push }\n\
         #main(Msg m) -> String\n\
         \"ok\"\n",
    )
    .expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--args")
        .arg(r#"{"m":"Email"}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        !out.status.success(),
        "payload enum variant should not decode from a bare string; stdout={}",
        String::from_utf8_lossy(&out.stdout),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("requires payload fields"),
        "stderr should explain payload requirement; got: {stderr}"
    );
}

#[test]
fn cli_args_decode_struct_payload_enum_variant_from_object() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-enum-payload-object-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#enum Msg { Email { address: String, subject: String }, Push }\n\
         #main(Msg m) -> Msg\n\
         m\n",
    )
    .expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--args")
        .arg(r#"{"m":{"Email":{"address":"a@b.c","subject":"hi"}}}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "CLI should decode externally tagged struct enum payload; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
    assert_eq!(
        json,
        serde_json::json!({ "Email": { "address": "a@b.c", "subject": "hi" } })
    );
}

#[test]
fn cli_args_decode_tuple_payload_enum_variant_from_array() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-enum-payload-array-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#enum Packet { Pair(Int, String), Empty }\n\
         #main(Packet p) -> Packet\n\
         p\n",
    )
    .expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--args")
        .arg(r#"{"p":{"Pair":[7,"x"]}}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "CLI should decode externally tagged tuple enum payload; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
    assert_eq!(json, serde_json::json!({ "Pair": [7, "x"] }));
}

#[test]
fn cli_args_decode_option_some_externally_tagged_payload() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-option-some-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#main(Option<Int> x) -> Int\n\
         x match { Some(v): v + 1, None: 0 }\n",
    )
    .expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--args")
        .arg(r#"{"x":{"Some":{"value":41}}}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "CLI should decode externally tagged Option.Some; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    assert_eq!(stdout.trim(), "42", "stdout was: {stdout:?}");
}

#[test]
fn cli_args_decode_result_ok_externally_tagged_payload() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-result-ok-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#main(Result<Int, String> r) -> Int\n\
         r match { Ok(v): v + 1, Err(e): 0 }\n",
    )
    .expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .arg("--args")
        .arg(r#"{"r":{"Ok":{"value":41}}}"#)
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "CLI should decode externally tagged Result.Ok; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    assert_eq!(stdout.trim(), "42", "stdout was: {stdout:?}");
}

#[test]
fn cli_args_decode_optional_shorthand_to_option_value() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-optional-shorthand-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#main(Option<Int> x) -> Int\n\
         x match { Some(v): v + 1, None: 0 }\n",
    )
    .expect("write fixture");

    let run = |args: &str| -> String {
        let out = Command::new(BINARY)
            .arg("run")
            .arg(&path)
            .arg("--backend")
            .arg("tree-walk")
            .arg("--args")
            .arg(args)
            .output()
            .expect("spawn relon CLI");
        assert!(
            out.status.success(),
            "CLI should decode optional shorthand args; stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8(out.stdout).expect("utf-8 stdout")
    };

    let some_out = run(r#"{"x":41}"#);
    let none_out = run(r#"{"x":null}"#);
    let _ = std::fs::remove_file(&path);

    assert_eq!(some_out.trim(), "42", "stdout was: {some_out:?}");
    assert_eq!(none_out.trim(), "0", "stdout was: {none_out:?}");
}

#[test]
fn cli_runs_rust_like_enum_struct_variant_constructor() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-enum-struct-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#enum Notification { Email { address: String, subject: String }, Push }\n\
         Notification.Email { address: \"a@b.c\", subject: \"hi\" }\n",
    )
    .expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "CLI should run #enum struct variant constructor; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
    assert_eq!(
        json,
        serde_json::json!({ "Email": { "address": "a@b.c", "subject": "hi" } }),
        "stdout was: {stdout:?}"
    );
}

#[test]
fn cli_runs_rust_like_enum_tuple_variant_constructor() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-enum-tuple-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &path,
        "#enum Packet { Pair(Int, String), Empty }
\
         Packet.Pair(7, \"x\")
",
    )
    .expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "CLI should run #enum tuple variant constructor; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
    assert_eq!(
        json,
        serde_json::json!({ "Pair": [7, "x"] }),
        "stdout was: {stdout:?}"
    );
}

#[test]
fn cli_runs_rust_like_enum_unit_variant_path() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-enum-unit-path-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(&path, "#enum Stat { Up, Down }\nStat.Up\n").expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "CLI should run #enum unit variant path; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
    assert_eq!(
        json,
        serde_json::json!({ "Up": {} }),
        "stdout was: {stdout:?}"
    );
}

#[test]
fn cli_typed_field_preserves_enum_variant_brand() {
    let path = std::env::temp_dir().join(format!(
        "relon-cli-enum-typed-field-{}-{}.relon",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(&path, "#enum Stat { Up, Down }\n{ Stat s: Stat.Up }\n").expect("write fixture");

    let out = Command::new(BINARY)
        .arg("run")
        .arg(&path)
        .arg("--backend")
        .arg("tree-walk")
        .output()
        .expect("spawn relon CLI");

    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "typed enum field should preserve variant brand; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
    assert_eq!(
        json,
        serde_json::json!({ "s": { "Up": {} } }),
        "stdout was: {stdout:?}"
    );
}
