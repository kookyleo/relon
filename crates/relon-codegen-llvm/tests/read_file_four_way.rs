//! P-fs Stage 1 — built-in `read_file(path) -> String`, cross-backend
//! parity.
//!
//! `read_file` is a String-in / String-out capability primitive
//! (`Op::ReadFile`, gated by `reads_fs`). Unlike clock()/random() its
//! result IS deterministic (a fixed fixture file), so the executors are
//! **byte-equal**. This test runs the same source through three
//! executors and asserts identical content:
//!
//!   1. tree-walk gold standard (`relon_evaluator`),
//!   2. cranelift native AOT (`relon_codegen_cranelift`),
//!   3. llvm native AOT (`relon_codegen_llvm`).
//!
//! All three resolve the path against the shared filesystem sandbox
//! root (`relon_util`), the native analogue of the wasm preopened dir.
//!
//! Honest scope: the wasm32 lowering of `read_file` is **deferred** in
//! P-fs Stage 1 (the WASI fd-protocol marshalling into the
//! out_ptr-relative tail-record convention is a larger block than the
//! native arms — see `wasi_cap.rs::emit_read_file_wasi`). A wasm emit of
//! a `read_file` program is asserted to *refuse to compile* rather than
//! emit a silently-wrong module (no fake green); the standard preview1
//! fd protocol + preopened-dir host stays proven by the spike
//! `tests/aot_wasm_wasi_fs_spike.rs`.

use std::collections::HashMap;
use std::path::PathBuf;

use relon_analyzer::{AnalyzeOptions, Capabilities};
use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::{CodegenTarget, LlvmAotEvaluator, WorldMode};
use relon_eval_api::{CapabilityBit, Evaluator, RuntimeError, Value};

const CONTENT: &str = "relon read_file four-way parity\nsecond line\n";
const FILENAME: &str = "fixture.txt";

/// Create a fresh temp dir holding the fixture file, point the shared
/// FS sandbox root at it, and return the dir (caller cleans up).
fn fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "relon_rf_4way_{tag}_{}_{:p}",
        std::process::id(),
        &CONTENT
    ));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    std::fs::write(dir.join(FILENAME), CONTENT).expect("write fixture");
    relon_util::set_fs_sandbox_root(&dir);
    dir
}

fn caps_reads_fs() -> Capabilities {
    let mut c = Capabilities::default();
    c.reads_fs = true;
    c
}

fn opts_reads_fs() -> AnalyzeOptions {
    AnalyzeOptions {
        caps: caps_reads_fs(),
        strict_mode: false,
        ..Default::default()
    }
}

const SRC: &str = "#main() -> String\nread_file(\"fixture.txt\")";

/// Tree-walk gold standard.
fn run_tree_walk() -> String {
    use relon_evaluator::{Context, TreeWalkEvaluator};
    use relon_parser::parse_document;
    let node = parse_document(SRC).expect("parse");
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    ctx.capabilities = caps_reads_fs();
    let ctx = std::sync::Arc::new({
        let mut ctx = ctx;
        TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    });
    let v = TreeWalkEvaluator::new(std::sync::Arc::clone(&ctx))
        .run_main(
            &std::sync::Arc::new(relon_eval_api::scope::Scope::default()),
            HashMap::new(),
        )
        .expect("tree-walk run_main");
    match v {
        Value::String(s) => s.to_string(),
        other => panic!("tree-walk: expected String, got {other:?}"),
    }
}

/// Cranelift native AOT.
fn run_cranelift() -> String {
    let ev = AotEvaluator::from_source_with_options(SRC, &opts_reads_fs())
        .expect("cranelift from_source")
        .with_granted_cap(CapabilityBit::ReadsFs.bit_index());
    match ev.run_main(HashMap::new()).expect("cranelift run_main") {
        Value::String(s) => s.to_string(),
        other => panic!("cranelift: expected String, got {other:?}"),
    }
}

/// LLVM native AOT.
fn run_llvm_native() -> String {
    let ev = LlvmAotEvaluator::from_source_with_options(SRC, &opts_reads_fs())
        .expect("llvm from_source")
        .with_granted_cap(CapabilityBit::ReadsFs.bit_index());
    match ev.run_main(HashMap::new()).expect("llvm run_main") {
        Value::String(s) => s.to_string(),
        other => panic!("llvm-native: expected String, got {other:?}"),
    }
}

#[test]
fn read_file_three_native_executors_are_byte_equal() {
    let dir = fixture("equal");

    let tw = run_tree_walk();
    let cl = run_cranelift();
    let llvm = run_llvm_native();

    assert_eq!(tw, CONTENT, "tree-walk content mismatch");
    assert_eq!(
        cl, tw,
        "cranelift content not byte-equal to tree-walk gold standard"
    );
    assert_eq!(
        llvm, tw,
        "llvm-native content not byte-equal to tree-walk gold standard"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_file_ungranted_traps_on_native_backends() {
    let dir = fixture("ungranted");

    // Build with the cap granted (analyze passes) but withhold the
    // runtime grant — the `Op::CheckCap` prologue must trap.
    let cl = AotEvaluator::from_source_with_options(SRC, &opts_reads_fs())
        .expect("cranelift from_source");
    let cl_err = cl
        .run_main(HashMap::new())
        .expect_err("cranelift ungranted read_file must trap");
    assert!(
        matches!(cl_err, RuntimeError::CapabilityDenied { .. }),
        "cranelift: expected CapabilityDenied, got {cl_err:?}"
    );

    let llvm = LlvmAotEvaluator::from_source_with_options(SRC, &opts_reads_fs())
        .expect("llvm from_source");
    let llvm_err = llvm
        .run_main(HashMap::new())
        .expect_err("llvm ungranted read_file must trap");
    assert!(
        matches!(llvm_err, RuntimeError::CapabilityDenied { .. }),
        "llvm-native: expected CapabilityDenied, got {llvm_err:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_file_path_escape_traps_on_native_backends() {
    let dir = fixture("escape");
    const ESCAPE_SRC: &str = "#main() -> String\nread_file(\"../escape.txt\")";

    let cl = AotEvaluator::from_source_with_options(ESCAPE_SRC, &opts_reads_fs())
        .expect("cranelift from_source")
        .with_granted_cap(CapabilityBit::ReadsFs.bit_index());
    let cl_err = cl
        .run_main(HashMap::new())
        .expect_err("cranelift path escape must trap");
    assert!(
        matches!(cl_err, RuntimeError::CapabilityDenied { .. }),
        "cranelift: expected CapabilityDenied for escape, got {cl_err:?}"
    );

    let llvm = LlvmAotEvaluator::from_source_with_options(ESCAPE_SRC, &opts_reads_fs())
        .expect("llvm from_source")
        .with_granted_cap(CapabilityBit::ReadsFs.bit_index());
    let llvm_err = llvm
        .run_main(HashMap::new())
        .expect_err("llvm path escape must trap");
    assert!(
        matches!(llvm_err, RuntimeError::CapabilityDenied { .. }),
        "llvm-native: expected CapabilityDenied for escape, got {llvm_err:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Honest scope: the wasm32 lowering of `read_file` is deferred in P-fs
/// Stage 1. Emitting a wasm object for a `read_file` program must
/// **refuse to compile** (loud codegen error) rather than silently emit
/// a wrong module. The standard preview1 fd protocol + preopened-dir
/// host stays proven by `tests/aot_wasm_wasi_fs_spike.rs`.
#[test]
fn read_file_wasm_lowering_is_refused_not_silently_wrong() {
    let tmp = std::env::temp_dir();
    let obj = tmp.join(format!("relon_rf_wasm_{}.o", std::process::id()));
    let result = LlvmAotEvaluator::emit_object_for_target(
        SRC,
        "relon_main_read_file",
        &obj,
        &opts_reads_fs(),
        WorldMode::OpenWorld,
        None,
        CodegenTarget::Wasm32,
    );
    let _ = std::fs::remove_file(&obj);
    let err = result.expect_err("wasm read_file emit must be refused (deferred lowering)");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("ReadFile") || msg.contains("read_file"),
        "expected a read_file-not-implemented codegen error, got: {msg}"
    );
}
