//! End-to-end object-cache acceptance for the CLI default path
//! (`--backend auto`): a *cold* run compiles and writes the cache
//! triple, a *hot* run of the same source dlopen-executes the cached
//! object, and both must print byte-identical JSON — which must also
//! match the tree-walk oracle.
//!
//! Isolation: every spawned CLI child gets its own tempdir-backed
//! `XDG_CACHE_HOME` (object cache location) and `XDG_DATA_HOME`
//! (HMAC key location) via `Command::env`, so the user's real caches
//! and key are never touched and the hot run can only hit the cache
//! this test just wrote.

use std::process::Command;

const BINARY: &str = env!("CARGO_BIN_EXE_relon-cli");

/// Non-trivial fixture (schema + List<Server> cross-region Dict
/// return) so the `auto` backend takes the cranelift-AOT + cache
/// path rather than the trivial-scalar tree-walk short-circuit, and
/// so String / List marshalling is exercised end-to-end.
const SOURCE: &str = "#schema Server { name: String, port: Int }\n\
                      #main(List<Server> servers) -> Dict\n{ servers: servers, n: 1 }\n";

fn args_json() -> String {
    // CJK name built from code points so this file stays ASCII-only;
    // plus empty-string and ASCII elements for edge coverage.
    let cjk: String = [0x4E2Du32, 0x6587u32]
        .iter()
        .map(|c| char::from_u32(*c).unwrap())
        .collect();
    format!(
        r#"{{"servers":[{{"name":"{cjk}","port":8080}},{{"name":"","port":0}},{{"name":"edge","port":-1}}]}}"#
    )
}

/// Spawn the CLI against `path` with the given backend and isolated
/// XDG dirs. Returns raw stdout bytes (parity must be byte-exact).
fn run_cli(
    path: &std::path::Path,
    backend: &str,
    xdg_cache: &std::path::Path,
    xdg_data: &std::path::Path,
) -> Vec<u8> {
    let out = Command::new(BINARY)
        .arg("run")
        .arg(path)
        .arg("--backend")
        .arg(backend)
        .arg("--args")
        .arg(args_json())
        .env("XDG_CACHE_HOME", xdg_cache)
        .env("XDG_DATA_HOME", xdg_data)
        .output()
        .expect("spawn relon CLI");
    assert!(
        out.status.success(),
        "CLI exited non-zero for backend {backend}: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    out.stdout
}

/// `true` when `$XDG_CACHE_HOME/relon` contains a stored object
/// (`*.relon-native-v1`) — i.e. the cold run completed the full
/// emit-object + link + HMAC-store pipeline.
fn object_cache_written(xdg_cache: &std::path::Path) -> bool {
    let dir = xdg_cache.join("relon");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_name()
            .to_string_lossy()
            .ends_with(".relon-native-v1")
    })
}

#[test]
fn cli_cold_then_hot_auto_runs_are_byte_equal_and_match_tree_walk() {
    let xdg_cache = tempfile::tempdir().expect("xdg cache tempdir");
    let xdg_data = tempfile::tempdir().expect("xdg data tempdir");
    let fixture_dir = tempfile::tempdir().expect("fixture tempdir");
    let path = fixture_dir.path().join("cold_hot_parity.relon");
    std::fs::write(&path, SOURCE).expect("write fixture");

    // Cold run: cache dirs start empty, so `auto` compiles from
    // source and stores the cache triple as a side effect.
    assert!(
        !object_cache_written(xdg_cache.path()),
        "precondition: cache must start empty"
    );
    let cold = run_cli(&path, "auto", xdg_cache.path(), xdg_data.path());

    if !object_cache_written(xdg_cache.path()) {
        // Lean host without a system linker: the object store is
        // skipped (loudly, on the CLI side). The parity assertion
        // below would only compare two fresh compiles, so flag the
        // skip instead of green-washing.
        eprintln!(
            "skipping hot-run dlopen assertion: cold run wrote no object cache (linker missing?)"
        );
        return;
    }

    // Hot run: same source + same XDG dirs → `from_cache_dir` hit →
    // dlopen-execute. Output must be byte-identical to the cold run.
    let hot = run_cli(&path, "auto", xdg_cache.path(), xdg_data.path());
    assert_eq!(
        cold, hot,
        "cold (fresh compile) vs hot (dlopen cache hit) stdout diverged"
    );

    // Oracle: tree-walk interpreter prints the same bytes.
    let oracle = run_cli(&path, "tree-walk", xdg_cache.path(), xdg_data.path());
    assert_eq!(cold, oracle, "auto stdout diverged from tree-walk oracle");

    // Sanity: the output actually carries the payload.
    let text = String::from_utf8(hot).expect("utf-8 stdout");
    assert!(
        text.contains("8080") && text.contains("edge"),
        "unexpected CLI output: {text:?}"
    );
}
