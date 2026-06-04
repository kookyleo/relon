//! P-fs Stage 2 — built-in `read_dir(path) -> List<String>`,
//! cross-backend parity.
//!
//! `read_dir` is a String-in / List<String>-out capability primitive
//! (`Op::ReadDir`, gated by `reads_fs`). Its result IS deterministic
//! once the entry names are sorted (the helpers sort byte-lexicographic
//! because `std::fs::read_dir` / `fd_readdir` iteration order is
//! OS-unspecified), so the executors are byte-equal.
//!
//! ## three-way (native) + wasm status
//!
//! P-fs Stage 2 ships **native-only**, mirroring how `read_file`
//! Stage 1 first landed three native arms before the wasm arm:
//!
//!   1. tree-walk gold standard (`relon_evaluator`),
//!   2. cranelift native AOT (`relon_codegen_cranelift`),
//!   3. llvm native AOT (`relon_codegen_llvm`).
//!
//! The wasm32 arm is deferred: the standard WASI preview1 `fd_readdir`
//! protocol returns an OS-ordered dirent stream that would need a paged
//! cookie loop AND an in-linear-memory sort of variable-length names,
//! emitted as raw LLVM IR. Rather than ship a silent / incorrect
//! listing, the wasm lowering raises a loud codegen error — asserted by
//! [`read_dir_wasm_is_a_loud_codegen_error`].
//!
//! ## the listing is observed as a direct `List<String>` return
//!
//! A top-level `#main() -> List<String>` return is now marshalled by
//! the cranelift / llvm-native `StoreField { ty: ListString }` path:
//! the whole pointer-array record (header `[len][off_0..]` plus the
//! per-entry `[slen][utf8]` String records) is copied into the output
//! buffer's tail and every inner offset relocated into the buffer's
//! coordinate system (`emit_store_field_list_string` /
//! `emit_store_list_string`). `read_dir(".")` is therefore returned
//! straight out of `#main` and the three native executors compared
//! whole-list (sorted, so byte-deterministic). The wasm32 arm stays
//! deferred for two independent reasons: `fd_readdir` enumeration is
//! unimplemented AND the relon-rs `EmittedFieldType` triple has no
//! `ListString` tag yet (the AOT-binding / wasm-object emit path).

use std::collections::HashMap;
use std::path::PathBuf;

use relon_analyzer::{AnalyzeOptions, Capabilities};
use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::{CodegenTarget, LlvmAotEvaluator, WorldMode};
use relon_eval_api::{CapabilityBit, Evaluator, RuntimeError, Value};

/// Fixture entry names, intentionally written out of order so the
/// helpers' sort is observable.
const WRITE_ORDER: &[&str] = &["zeta.txt", "alpha.txt", "middle.txt"];
/// The byte-lexicographically sorted order all backends must produce.
const SORTED: &[&str] = &["alpha.txt", "middle.txt", "zeta.txt"];

/// Serializes the fs-reading tests in this binary (mirrors
/// `read_file_four_way::fixture`): the thread-local sandbox root
/// isolates each test's root, but these tests also drive concurrent
/// JIT / MCJIT through process-global toolchain state.
static FS_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Create a fresh temp dir holding the fixture files, point the FS
/// sandbox root at it, and return the dir plus a serialization guard the
/// caller holds for the whole test (caller cleans up the dir).
fn fixture(tag: &str) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
    let guard = FS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!(
        "relon_rd_3way_{tag}_{}_{:p}",
        std::process::id(),
        &WRITE_ORDER
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    for name in WRITE_ORDER {
        std::fs::write(dir.join(name), b"x").expect("write fixture entry");
    }
    relon_util::set_fs_sandbox_root(&dir);
    (dir, guard)
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

/// `read_dir(".")` returned directly as the `#main` result. The
/// `StoreField { ty: ListString }` marshalling copies the whole
/// pointer-array record into the output buffer's tail with each inner
/// offset relocated, so the sorted listing is observed as a direct
/// `List<String>` return on all three native backends.
const SRC: &str = "#main(Int i) -> List<String>\n\
                   read_dir(\".\")";

/// Pull the sorted entry names out of the returned `List<String>`.
fn result_of(v: &Value) -> Vec<String> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|e| match e {
                Value::String(s) => s.to_string(),
                other => panic!("expected String element, got {other:?}"),
            })
            .collect(),
        other => panic!("expected List<String> return, got {other:?}"),
    }
}

fn arg_i(i: i64) -> HashMap<String, Value> {
    HashMap::from([("i".to_string(), Value::Int(i))])
}

/// Tree-walk gold standard listing.
fn run_tree_walk(i: i64) -> Vec<String> {
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
            arg_i(i),
        )
        .expect("tree-walk run_main");
    result_of(&v)
}

#[test]
fn read_dir_three_native_executors_are_byte_equal() {
    let (dir, _serial) = fixture("equal");

    let cl = AotEvaluator::from_source_with_options(SRC, &opts_reads_fs())
        .expect("cranelift from_source")
        .with_granted_cap(CapabilityBit::ReadsFs.bit_index());
    let llvm = LlvmAotEvaluator::from_source_with_options(SRC, &opts_reads_fs())
        .expect("llvm from_source")
        .with_granted_cap(CapabilityBit::ReadsFs.bit_index());

    let want: Vec<String> = SORTED.iter().map(|s| s.to_string()).collect();
    let tw = run_tree_walk(0);
    assert_eq!(tw, want, "tree-walk sorted listing mismatch");

    let cl_v = result_of(&cl.run_main(arg_i(0)).expect("cranelift run_main"));
    assert_eq!(
        cl_v, tw,
        "cranelift read_dir() List<String> return not byte-equal to tree-walk gold standard"
    );

    let llvm_v = result_of(&llvm.run_main(arg_i(0)).expect("llvm run_main"));
    assert_eq!(
        llvm_v, tw,
        "llvm-native read_dir() List<String> return not byte-equal to tree-walk gold standard"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A const `#main() -> List<String>` literal returns the same bytes on
/// all three native backends — the `StoreField { ty: ListString }`
/// pointer-array marshalling exercised without the `read_dir` helper
/// (the const-pool `ConstListString` blob is the source record).
#[test]
fn const_list_string_return_three_way() {
    const CONST_SRC: &str = "#main(Int i) -> List<String>\n[\"b\", \"a\", \"c\"]";
    let want = vec!["b".to_string(), "a".to_string(), "c".to_string()];

    let cl = AotEvaluator::from_source(CONST_SRC).expect("cranelift from_source");
    let llvm = LlvmAotEvaluator::from_source(CONST_SRC).expect("llvm from_source");

    let cl_v = result_of(&cl.run_main(arg_i(0)).expect("cranelift run_main"));
    assert_eq!(cl_v, want, "cranelift const List<String> return mismatch");
    let llvm_v = result_of(&llvm.run_main(arg_i(0)).expect("llvm run_main"));
    assert_eq!(llvm_v, want, "llvm const List<String> return mismatch");
    assert_eq!(cl_v, llvm_v, "cranelift vs llvm const return divergence");
}

#[test]
fn read_dir_ungranted_traps_on_native_backends() {
    let (dir, _serial) = fixture("ungranted");

    // Build with the cap granted (analyze passes) but withhold the
    // runtime grant — the `Op::CheckCap` prologue must trap.
    let cl = AotEvaluator::from_source_with_options(SRC, &opts_reads_fs())
        .expect("cranelift from_source");
    let cl_err = cl
        .run_main(arg_i(0))
        .expect_err("cranelift ungranted read_dir must trap");
    assert!(
        matches!(cl_err, RuntimeError::CapabilityDenied { .. }),
        "cranelift: expected CapabilityDenied, got {cl_err:?}"
    );

    let llvm = LlvmAotEvaluator::from_source_with_options(SRC, &opts_reads_fs())
        .expect("llvm from_source");
    let llvm_err = llvm
        .run_main(arg_i(0))
        .expect_err("llvm ungranted read_dir must trap");
    assert!(
        matches!(llvm_err, RuntimeError::CapabilityDenied { .. }),
        "llvm-native: expected CapabilityDenied, got {llvm_err:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_dir_path_escape_traps_on_native_backends() {
    let (dir, _serial) = fixture("escape");
    const ESCAPE_SRC: &str = "#schema R { String result: * }\n\
                              #main(Int i) -> R\n\
                              { result: (entries[i] where { entries: read_dir(\"../escape\") }) }";

    let cl = AotEvaluator::from_source_with_options(ESCAPE_SRC, &opts_reads_fs())
        .expect("cranelift from_source")
        .with_granted_cap(CapabilityBit::ReadsFs.bit_index());
    let cl_err = cl
        .run_main(arg_i(0))
        .expect_err("cranelift path escape must trap");
    assert!(
        matches!(cl_err, RuntimeError::CapabilityDenied { .. }),
        "cranelift: expected CapabilityDenied for escape, got {cl_err:?}"
    );

    let llvm = LlvmAotEvaluator::from_source_with_options(ESCAPE_SRC, &opts_reads_fs())
        .expect("llvm from_source")
        .with_granted_cap(CapabilityBit::ReadsFs.bit_index());
    let llvm_err = llvm
        .run_main(arg_i(0))
        .expect_err("llvm path escape must trap");
    assert!(
        matches!(llvm_err, RuntimeError::CapabilityDenied { .. }),
        "llvm-native: expected CapabilityDenied for escape, got {llvm_err:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// P-fs Stage 2 is native-only: emitting `read_dir` for wasm32 must be
/// a loud codegen error (the `fd_readdir` dirent-stream protocol is
/// deferred), NOT a silent / incorrect listing.
#[test]
fn read_dir_wasm_is_a_loud_codegen_error() {
    let (dir, _serial) = fixture("wasm_reject");

    // Use the schema-wrapped String-return shape (read_dir bound in a
    // `where`, indexed) so the wasm emit reaches the `read_dir` /
    // `fd_readdir` body codegen error rather than tripping on the
    // separate `List<String>` return-marshalling triple gap first — this
    // test pins the fd_readdir enumeration deferral specifically.
    const WASM_REJECT_SRC: &str = "#schema R { String result: * }\n\
                                   #main(Int i) -> R\n\
                                   { result: (entries[i] where { entries: read_dir(\".\") }) }";
    let obj = std::env::temp_dir().join(format!("relon_rd_wasm_{}.o", std::process::id()));
    let err = LlvmAotEvaluator::emit_object_for_target(
        WASM_REJECT_SRC,
        "relon_main_read_dir",
        &obj,
        &opts_reads_fs(),
        WorldMode::OpenWorld,
        None,
        CodegenTarget::Wasm32,
    )
    .expect_err("read_dir wasm32 emit must be a loud codegen error");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("read_dir") && msg.contains("wasm32") && msg.contains("fd_readdir"),
        "wasm32 read_dir error must name read_dir / wasm32 / fd_readdir; got: {msg}"
    );
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_dir_all(&dir);
}
