//! P-fs Stage 3 — built-in `stat(path) -> Dict`, cross-backend parity.
//!
//! `stat` is a String-in / Dict-out capability primitive (`Op::Stat`,
//! gated by `reads_fs`); the dict is `{is_dir: Bool, size: Int}`. The
//! result IS deterministic for a fixed fixture (a known-size file + a
//! directory), so the executors are bit-equal on both fields.
//!
//! ## four-way (native + wasm)
//!
//! Unlike `read_dir` (Stage 2, native-only), `stat`'s wasm arm IS
//! implemented: WASI preview1 `path_filestat_get` writes a fixed 64-byte
//! `filestat` struct into linear memory (no paged dirent cookie loop),
//! and the lowering reads `filetype` (offset 16; `3` == directory) and
//! `size` (offset 32) out of it. So all FOUR executors run:
//!
//!   1. tree-walk gold standard (`relon_evaluator`),
//!   2. cranelift native AOT (`relon_codegen_cranelift`),
//!   3. llvm native AOT (`relon_codegen_llvm`),
//!   4. wasm32 AOT via standard WASI preview1 (`path_filestat_get`) under
//!      an off-the-shelf `wasmtime-wasi` host with a single preopened dir.
//!
//! ## how the Dict is observed
//!
//! A top-level `#main() -> Dict` return is refused by the cranelift /
//! llvm-native return-root copy path (the dict record is a pointer-array
//! relocation the return-store path doesn't relocate). Instead we read
//! the individual fields with the const-string dict subscript
//! `stat(path)["size"]` / `stat(path)["is_dir"]` — a fully IR-lowered
//! linear-scan probe (no backend-specific helper), so it returns a plain
//! scalar through the normal fixed-area return field on every backend.

use std::collections::HashMap;
use std::path::PathBuf;

use relon_analyzer::{AnalyzeOptions, Capabilities};
use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::{CodegenTarget, EmitObjectInfo, LlvmAotEvaluator, WorldMode};
use relon_eval_api::{CapabilityBit, Evaluator, RuntimeError, Value};

/// Fixture file contents; its byte length is the `size` the backends
/// must agree on.
const CONTENT: &str = "relon stat four-way parity fixture\n";
const FILENAME: &str = "fixture.txt";
const SUBDIR: &str = "subdir";

/// Serializes the fs-reading tests in this binary (mirrors
/// `read_file_four_way::fixture`): the thread-local sandbox root isolates
/// each test's root, but these tests also drive concurrent JIT / MCJIT /
/// wasm-ld / wasmtime through process-global toolchain state.
static FS_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Create a fresh temp dir holding the fixture file + a subdirectory,
/// point the FS sandbox root at it, and return the dir plus a
/// serialization guard the caller holds for the whole test.
fn fixture(tag: &str) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
    let guard = FS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!(
        "relon_stat_4way_{tag}_{}_{:p}",
        std::process::id(),
        &CONTENT
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    std::fs::write(dir.join(FILENAME), CONTENT).expect("write fixture");
    std::fs::create_dir_all(dir.join(SUBDIR)).expect("create subdir");
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

// The `stat(...)` dict is bound in a `where` so the subscript receiver is
// a plain variable (`s`) — the `from_source`-reachable dict-probe shape on
// the compiled backends. The scalar is returned through a one-field schema
// (`R { Int result: * }`), mirroring the `read_dir` test: a top-level
// `#main -> Int` whose body types as `Dict<String, Any>` trips the return
// typecheck, but a schema field accepts the (runtime-Int) probe result.
//
/// `stat("fixture.txt")["size"]` — the file's byte length, as Int. One
/// shared source across all four backends (size is a deterministic Int).
const SIZE_SRC: &str = "#schema R { Int result: * }\n\
                        #main() -> R\n\
                        { result: (s[\"size\"] where { s: stat(\"fixture.txt\") }) }";

/// `is_dir` is a `Bool` (tree-walk) / i64 0/1 (compiled dict-probe). The
/// same `Int result` field accepts both ONLY after bridging the
/// representation: the tree-walk compares the `Bool` against `true`, the
/// compiled / wasm backends compare the i64 probe against `1` (Bool-eq is
/// not lowered there, Int-eq is). Both `== ...` produce a `Bool`, then a
/// `? 1 : 0` ternary folds it to the SAME `Int` 0/1 — so the asserted
/// value is bit-equal even though the operand representation differs by
/// backend. `is_dir_src(path, true)` builds the tree-walk form;
/// `is_dir_src(path, false)` the compiled/wasm form.
fn is_dir_src(path: &str, tree_walk: bool) -> String {
    let rhs = if tree_walk { "true" } else { "1" };
    format!(
        "#schema R {{ Int result: * }}\n\
         #main() -> R\n\
         {{ result: (((s[\"is_dir\"] == {rhs}) ? 1 : 0) where {{ s: stat(\"{path}\") }}) }}"
    )
}

/// Tree-walk gold standard: run `src`, expecting an Int / Bool scalar,
/// normalised to i64 (Bool -> 0/1).
fn run_tree_walk(src: &str) -> i64 {
    use relon_evaluator::{Context, TreeWalkEvaluator};
    use relon_parser::parse_document;
    let node = parse_document(src).expect("parse");
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
    scalar_i64(&v)
}

/// Pull the `result` field out of the returned schema dict, normalised
/// to i64 (Bool -> 0/1).
fn scalar_i64(v: &Value) -> i64 {
    let inner = match v {
        Value::Dict(d) => d.map.get("result").expect("return dict has `result`"),
        other => panic!("expected schema Dict return, got {other:?}"),
    };
    match inner {
        Value::Int(n) => *n,
        other => panic!("expected Int `result`, got {other:?}"),
    }
}

fn run_cranelift(src: &str) -> i64 {
    let ev = AotEvaluator::from_source_with_options(src, &opts_reads_fs())
        .expect("cranelift from_source")
        .with_granted_cap(CapabilityBit::ReadsFs.bit_index());
    scalar_i64(&ev.run_main(HashMap::new()).expect("cranelift run_main"))
}

fn run_llvm_native(src: &str) -> i64 {
    let ev = LlvmAotEvaluator::from_source_with_options(src, &opts_reads_fs())
        .expect("llvm from_source")
        .with_granted_cap(CapabilityBit::ReadsFs.bit_index());
    scalar_i64(&ev.run_main(HashMap::new()).expect("llvm run_main"))
}

#[test]
fn stat_three_native_executors_are_bit_equal() {
    let (dir, _serial) = fixture("equal");
    let expected_size = CONTENT.len() as i64;

    // size
    let tw = run_tree_walk(SIZE_SRC);
    assert_eq!(tw, expected_size, "tree-walk size mismatch");
    assert_eq!(run_cranelift(SIZE_SRC), tw, "cranelift size not bit-equal");
    assert_eq!(run_llvm_native(SIZE_SRC), tw, "llvm size not bit-equal");

    // is_dir for the regular file -> 0
    let tw = run_tree_walk(&is_dir_src(FILENAME, true));
    assert_eq!(tw, 0, "tree-walk file is_dir must be false");
    assert_eq!(
        run_cranelift(&is_dir_src(FILENAME, false)),
        tw,
        "cranelift file is_dir not bit-equal"
    );
    assert_eq!(
        run_llvm_native(&is_dir_src(FILENAME, false)),
        tw,
        "llvm file is_dir not bit-equal"
    );

    // is_dir for the directory -> 1
    let tw = run_tree_walk(&is_dir_src(SUBDIR, true));
    assert_eq!(tw, 1, "tree-walk dir is_dir must be true");
    assert_eq!(
        run_cranelift(&is_dir_src(SUBDIR, false)),
        tw,
        "cranelift dir is_dir not bit-equal"
    );
    assert_eq!(
        run_llvm_native(&is_dir_src(SUBDIR, false)),
        tw,
        "llvm dir is_dir not bit-equal"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stat_ungranted_traps_on_native_backends() {
    let (dir, _serial) = fixture("ungranted");

    let cl = AotEvaluator::from_source_with_options(SIZE_SRC, &opts_reads_fs())
        .expect("cranelift from_source");
    let cl_err = cl
        .run_main(HashMap::new())
        .expect_err("cranelift ungranted stat must trap");
    assert!(
        matches!(cl_err, RuntimeError::CapabilityDenied { .. }),
        "cranelift: expected CapabilityDenied, got {cl_err:?}"
    );

    let llvm = LlvmAotEvaluator::from_source_with_options(SIZE_SRC, &opts_reads_fs())
        .expect("llvm from_source");
    let llvm_err = llvm
        .run_main(HashMap::new())
        .expect_err("llvm ungranted stat must trap");
    assert!(
        matches!(llvm_err, RuntimeError::CapabilityDenied { .. }),
        "llvm-native: expected CapabilityDenied, got {llvm_err:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stat_path_escape_traps_on_native_backends() {
    let (dir, _serial) = fixture("escape");
    const ESCAPE_SRC: &str = "#schema R { Int result: * }\n\
                              #main() -> R\n\
                              { result: (s[\"size\"] where { s: stat(\"../escape.txt\") }) }";

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

// ---- wasm32 arm (standard WASI `path_filestat_get`) ----

/// ArenaState linear-memory layout offsets (mirror
/// `read_file_four_way`).
const STATE_OFF_ARENA_BASE: usize = 0; // i64 (8 bytes)
const STATE_OFF_ARENA_LEN: usize = 8; // u32
const STATE_OFF_TAIL_CURSOR: usize = 12; // u32
const STATE_OFF_SCRATCH_CURSOR: usize = 16; // u32
const STATE_OFF_SCRATCH_BASE: usize = 20; // u32
const STATE_OFF_TRAP_CODE: usize = 24; // u64
const STATE_SIZE: usize = 40;

fn build_wasm(src: &str, entry: &str) -> Result<(Vec<u8>, EmitObjectInfo), String> {
    if relon_codegen_llvm::wasm_link::find_wasm_ld().is_none() {
        return Err("wasm-ld not found on PATH (install lld)".into());
    }
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let obj = tmp.join(format!("relon_st4_{entry}_{pid}.o"));
    let wasm = tmp.join(format!("relon_st4_{entry}_{pid}.wasm"));

    let info = LlvmAotEvaluator::emit_object_for_target(
        src,
        entry,
        &obj,
        &opts_reads_fs(),
        WorldMode::OpenWorld,
        None,
        CodegenTarget::Wasm32,
    )
    .map_err(|e| format!("wasm32 emit: {e:?}"))?;
    relon_codegen_llvm::wasm_link::link_wasm_object(&obj, &wasm, entry)
        .map_err(|e| format!("wasm-ld link: {e:?}"))?;
    let bytes = std::fs::read(&wasm).map_err(|e| format!("read wasm: {e}"))?;
    assert_eq!(&bytes[..4], b"\0asm", "linked module magic");
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&wasm);
    Ok((bytes, info))
}

/// Run the scalar-returning `stat` buffer entry under a `wasmtime-wasi`
/// preview1 host (`dir` preopened as dirfd 3). Returns the decoded Int
/// (`Ok`) or the recorded `trap_code` (`Err`).
fn run_wasm(
    bytes: &[u8],
    entry: &str,
    info: &EmitObjectInfo,
    dir: &std::path::Path,
    caps: i64,
) -> Result<i64, u64> {
    use wasmtime::{Engine, Extern, Linker, Module, Store, Val};
    use wasmtime_wasi::p1::{self, WasiP1Ctx};
    use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("wasmtime Module::new");

    let wasi: WasiP1Ctx = WasiCtxBuilder::new()
        .preopened_dir(dir, ".", DirPerms::READ, FilePerms::READ)
        .expect("preopened_dir")
        .build_p1();
    let mut store = Store::new(&engine, wasi);
    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    p1::add_to_linker_sync(&mut linker, |cx| cx).expect("add standard WASIp1 to linker");
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate (standard WASI imports must be satisfied)");

    let memory = match instance.get_export(&mut store, "memory") {
        Some(Extern::Memory(m)) => m,
        _ => panic!("module missing exported `memory`"),
    };
    let heap_base = match instance.get_export(&mut store, "__heap_base") {
        Some(Extern::Global(g)) => match g.get(&mut store) {
            Val::I32(v) => v as u32,
            other => panic!("__heap_base not i32: {other:?}"),
        },
        _ => panic!("module missing exported `__heap_base`"),
    };

    let align8 = |v: u32| (v + 7) & !7u32;
    let const_data = &info.const_data;
    let const_data_len = const_data.len() as u32;
    let state_ptr = align8(heap_base);
    let arena_off = align8(state_ptr + STATE_SIZE as u32);
    let in_ptr = align8(const_data_len);
    let in_len = 0u32;
    let out_ptr = align8(in_ptr + in_len);
    let out_cap = align8(info.return_root_size.max(8) + 65536);
    let scratch_base = align8(out_ptr + out_cap);
    let scratch_size = 65536u32;
    let arena_bytes = scratch_base + scratch_size;

    let needed = (arena_off + arena_bytes) as usize;
    let cur = memory.data_size(&store);
    if needed > cur {
        let extra_pages = (needed - cur).div_ceil(65536) as u64;
        memory
            .grow(&mut store, extra_pages)
            .expect("grow linear memory");
    }

    let arena_abs = arena_off;
    let mut state = [0u8; STATE_SIZE];
    state[STATE_OFF_ARENA_BASE..STATE_OFF_ARENA_BASE + 8]
        .copy_from_slice(&(arena_abs as u64).to_le_bytes());
    state[STATE_OFF_ARENA_LEN..STATE_OFF_ARENA_LEN + 4].copy_from_slice(&arena_bytes.to_le_bytes());
    state[STATE_OFF_TAIL_CURSOR..STATE_OFF_TAIL_CURSOR + 4]
        .copy_from_slice(&info.return_root_size.to_le_bytes());
    state[STATE_OFF_SCRATCH_CURSOR..STATE_OFF_SCRATCH_CURSOR + 4]
        .copy_from_slice(&0u32.to_le_bytes());
    state[STATE_OFF_SCRATCH_BASE..STATE_OFF_SCRATCH_BASE + 4]
        .copy_from_slice(&scratch_base.to_le_bytes());
    memory
        .write(&mut store, state_ptr as usize, &state)
        .expect("write state");
    if !const_data.is_empty() {
        memory
            .write(&mut store, arena_abs as usize, const_data)
            .expect("write const data");
    }

    let func = instance
        .get_func(&mut store, entry)
        .unwrap_or_else(|| panic!("export `{entry}` missing"));
    let params = [
        Val::I32(state_ptr as i32),
        Val::I32(in_ptr as i32),
        Val::I32(in_len as i32),
        Val::I32(out_ptr as i32),
        Val::I32(out_cap as i32),
        Val::I64(caps),
    ];
    let mut results = [Val::I32(0)];
    func.call(&mut store, &params, &mut results)
        .expect("buffer entry call");

    let mut trap = [0u8; 8];
    memory
        .read(
            &store,
            (state_ptr as usize) + STATE_OFF_TRAP_CODE,
            &mut trap,
        )
        .expect("read trap_code");
    let trap_code = u64::from_le_bytes(trap);
    if trap_code != 0 {
        return Err(trap_code);
    }

    // Decode the scalar Int return from the fixed-area slot.
    let mut out = vec![0u8; out_cap as usize];
    memory
        .read(&store, (arena_abs + out_ptr) as usize, &mut out)
        .expect("read out region");
    let ret_off = info.return_fields[0].offset as usize;
    Ok(i64::from_le_bytes(
        out[ret_off..ret_off + 8].try_into().unwrap(),
    ))
}

/// The emitted wasm `stat` module imports the **standard** preview1
/// `path_filestat_get` on `wasi_snapshot_preview1` (NOT relon's `env`).
#[test]
fn stat_wasm_imports_standard_wasi_filestat() {
    let (bytes, _info) = match build_wasm(SIZE_SRC, "relon_main_stat_imp") {
        Ok(v) => v,
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("stat_four_way: wasm-ld unavailable; skipped import check");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    use wasmtime::{Engine, ExternType, Module};
    let engine = Engine::default();
    let module = Module::new(&engine, &bytes).expect("Module::new");
    let found = module.imports().any(|imp| {
        imp.module() == "wasi_snapshot_preview1"
            && imp.name() == "path_filestat_get"
            && matches!(imp.ty(), ExternType::Func(_))
    });
    assert!(
        found,
        "module must import (wasi_snapshot_preview1 path_filestat_get (func))"
    );
    let leaked = module
        .imports()
        .any(|imp| imp.module() == "env" && imp.name() == "path_filestat_get");
    assert!(!leaked, "path_filestat_get leaked onto custom `env` module");
}

/// Four-way bit-equal: tree-walk, cranelift, llvm-native, AND wasm32
/// (standard WASI `path_filestat_get` + preopened host) agree on both
/// the file `size` and the `is_dir` flag (file = 0, dir = 1).
#[test]
fn stat_four_way_bit_equal() {
    let (dir, _serial) = fixture("4way");
    let expected_size = CONTENT.len() as i64;

    // Native arms.
    let size_tw = run_tree_walk(SIZE_SRC);
    let file_dir_tw = run_tree_walk(&is_dir_src(FILENAME, true));
    let dir_dir_tw = run_tree_walk(&is_dir_src(SUBDIR, true));
    assert_eq!(size_tw, expected_size, "tree-walk size");
    assert_eq!(file_dir_tw, 0, "tree-walk file is_dir");
    assert_eq!(dir_dir_tw, 1, "tree-walk dir is_dir");
    assert_eq!(run_cranelift(SIZE_SRC), size_tw);
    assert_eq!(run_llvm_native(SIZE_SRC), size_tw);
    assert_eq!(run_cranelift(&is_dir_src(SUBDIR, false)), dir_dir_tw);
    assert_eq!(run_llvm_native(&is_dir_src(SUBDIR, false)), dir_dir_tw);

    // wasm arm. The compiled (Int-eq) is_dir form is what the wasm
    // backend lowers.
    let caps = 1i64 << CapabilityBit::ReadsFs.bit_index();
    for (src, entry, want) in [
        (SIZE_SRC.to_string(), "relon_main_stat_size", size_tw),
        (
            is_dir_src(FILENAME, false),
            "relon_main_stat_fdir",
            file_dir_tw,
        ),
        (
            is_dir_src(SUBDIR, false),
            "relon_main_stat_ddir",
            dir_dir_tw,
        ),
    ] {
        let (bytes, info) = match build_wasm(&src, entry) {
            Ok(v) => v,
            Err(reason) if reason.contains("wasm-ld not found") => {
                eprintln!("stat_four_way: wasm-ld unavailable; skipped wasm arm");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                panic!("{e}");
            }
        };
        let got = run_wasm(&bytes, entry, &info, &dir, caps)
            .expect("wasm stat must produce a value (granted ReadsFs)");
        assert_eq!(
            got, want,
            "wasm32 (standard WASI path_filestat_get) not bit-equal to tree-walk for {entry}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The wasm arm enforces `ReadsFs`: withholding the runtime grant makes
/// the `Op::CheckCap` prologue trap (CapabilityDenied) rather than
/// reading metadata.
#[test]
fn stat_ungranted_traps_on_wasm() {
    let (dir, _serial) = fixture("wasm_ungranted");

    let (bytes, info) = match build_wasm(SIZE_SRC, "relon_main_stat") {
        Ok(v) => v,
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("stat_four_way: wasm-ld unavailable; skipped wasm ungranted check");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("{e}");
        }
    };
    let result = run_wasm(&bytes, "relon_main_stat", &info, &dir, 0);
    let _ = std::fs::remove_dir_all(&dir);
    const CAPABILITY_DENIED: u64 = 3;
    match result {
        Err(code) => assert_eq!(
            code, CAPABILITY_DENIED,
            "wasm ungranted stat: expected CapabilityDenied trap_code, got {code}"
        ),
        Ok(v) => panic!("wasm ungranted stat must trap, but read {v}"),
    }
}
