//! P-fs Stage 1 — built-in `read_file(path) -> String`, cross-backend
//! parity.
//!
//! `read_file` is a String-in / String-out capability primitive
//! (`Op::ReadFile`, gated by `reads_fs`). Unlike clock()/random() its
//! result IS deterministic (a fixed fixture file), so the executors are
//! **byte-equal**. This test runs the same source through FOUR executors
//! and asserts identical content:
//!
//!   1. tree-walk gold standard (`relon_evaluator`),
//!   2. cranelift native AOT (`relon_codegen_cranelift`),
//!   3. llvm native AOT (`relon_codegen_llvm`),
//!   4. wasm32 AOT via standard WASI preview1 (`path_open` / `fd_read` /
//!      `fd_close`) under an off-the-shelf `wasmtime-wasi` host with a
//!      single `preopened_dir`.
//!
//! The three native arms resolve the path against the shared filesystem
//! sandbox root (`relon_util`); the wasm arm resolves it against the
//! preopened directory the standard WASI host exposes as dirfd 3 — the
//! wasm-native analogue of the sandbox root.
//!
//! The wasm marshalling (see `wasi_cap.rs::emit_read_file_wasi`): the
//! `Op::ReadFile` body opens the path against dirfd 3, points an iovec
//! at a fresh scratch-region String record's payload (`record+4`), reads
//! the file bytes straight in, stamps `nread` into the record's `[len]`
//! header, and pushes the record offset as a `String` operand — the
//! existing return-store path then copies it into the buffer tail
//! exactly like the native helper's return value.

use std::collections::HashMap;
use std::path::PathBuf;

use relon_analyzer::{AnalyzeOptions, Capabilities};
use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::{CodegenTarget, EmitObjectInfo, LlvmAotEvaluator, WorldMode};
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

/// ArenaState linear-memory layout offsets for the buffer-protocol
/// handshake. The LLVM emitter bakes these as compile-time constants
/// from the host `size_of::<usize>() == 8`, so the SAME 8-byte-base
/// layout applies in the wasm module (mirrors `aot_wasm.rs`).
const STATE_OFF_ARENA_BASE: usize = 0; // i64 (8 bytes)
const STATE_OFF_ARENA_LEN: usize = 8; // u32
const STATE_OFF_TAIL_CURSOR: usize = 12; // u32
const STATE_OFF_SCRATCH_CURSOR: usize = 16; // u32
const STATE_OFF_SCRATCH_BASE: usize = 20; // u32
const STATE_OFF_TRAP_CODE: usize = 24; // u64
const STATE_SIZE: usize = 40; // through host_fns (8 bytes @ 32)

/// Emit + `wasm-ld`-link the wasm32 `read_file` module. Returns the
/// linked bytes + the emit info, or `Err` (skip) when `wasm-ld` is
/// unavailable.
fn build_wasm_read_file(src: &str, entry: &str) -> Result<(Vec<u8>, EmitObjectInfo), String> {
    if relon_codegen_llvm::wasm_link::find_wasm_ld().is_none() {
        return Err("wasm-ld not found on PATH (install lld)".into());
    }
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let obj = tmp.join(format!("relon_rf4_{entry}_{pid}.o"));
    let wasm = tmp.join(format!("relon_rf4_{entry}_{pid}.wasm"));

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

/// Lay an `ArenaState` + arena in wasm linear memory, run the
/// `read_file` buffer entry under an off-the-shelf `wasmtime-wasi`
/// preview1 host with `dir` preopened as dirfd 3, and return either the
/// decoded String (`Ok`) or the recorded `trap_code` (`Err`).
///
/// Arena layout (mirrors `run_main_buffer`): `[in_buf | out_buf (root +
/// String tail) | scratch]`. `arena_len` is set so the wasm body's
/// `fd_read` capacity (`arena_len - payload_off`) is well-defined.
fn run_wasm_read_file(
    bytes: &[u8],
    entry: &str,
    info: &EmitObjectInfo,
    dir: &std::path::Path,
    caps: i64,
) -> Result<String, u64> {
    use wasmtime::{Engine, Extern, Linker, Module, Store, Val};
    use wasmtime_wasi::p1::{self, WasiP1Ctx};
    use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("wasmtime Module::new");

    // Off-the-shelf WASI host: a single preopened dir lands at dirfd 3.
    let wasi: WasiP1Ctx = WasiCtxBuilder::new()
        .preopened_dir(dir, ".", DirPerms::READ, FilePerms::READ)
        .expect("preopened_dir")
        .build_p1();
    let mut store = Store::new(&engine, wasi);
    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    p1::add_to_linker_sync(&mut linker, |cx| cx).expect("add standard WASIp1 to linker");
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate (standard WASI fd-protocol imports must be satisfied)");

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

    // The path (`"fixture.txt"`) is a `ConstString` baked into the
    // const-data pool, NOT a #main arg — so there is no input record;
    // the const pool lives at the arena prefix (offset 0) and the body's
    // `Op::ConstString` resolves the path out of it. Mirror
    // `run_main_buffer`'s `[const_data | in_buf | out_buf (root + tail) |
    // scratch]` layout exactly.
    let align8 = |v: u32| (v + 7) & !7u32;
    let const_data = &info.const_data;
    let const_data_len = const_data.len() as u32;
    let state_ptr = align8(heap_base);
    let arena_off = align8(state_ptr + STATE_SIZE as u32);
    let in_ptr = align8(const_data_len); // arena-relative
    let in_len = 0u32; // no #main args
    let out_ptr = align8(in_ptr + in_len);
    // Generous out region: root + a String tail cushion.
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

    // Write the ArenaState (LE). `arena_len` MUST be set so the wasm
    // body's fd_read capacity (`arena_len - payload_off`) is sane.
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
    // Copy the const-data pool into the arena prefix (offset 0).
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

    // Read `trap_code` back: a non-zero value means a capability /
    // sandbox trap fired (the host surfaces it as a typed error).
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

    // Decode the String return: the fixed-area slot at the return
    // field's offset holds a buffer-relative (out_ptr-relative) offset
    // to a `[len:u32][utf8]` record in the out tail.
    let mut out = vec![0u8; out_cap as usize];
    memory
        .read(&store, (arena_abs + out_ptr) as usize, &mut out)
        .expect("read out region");
    let ret_off = info.return_fields[0].offset as usize;
    let rec_off = u32::from_le_bytes(out[ret_off..ret_off + 4].try_into().unwrap()) as usize;
    let len = u32::from_le_bytes(out[rec_off..rec_off + 4].try_into().unwrap()) as usize;
    let payload = &out[rec_off + 4..rec_off + 4 + len];
    Ok(String::from_utf8(payload.to_vec()).expect("utf-8 content"))
}

/// The emitted wasm `read_file` module imports the **standard** preview1
/// fd-protocol symbols on `wasi_snapshot_preview1` (NOT relon's custom
/// `env`). Parsed straight from the import section.
#[test]
fn read_file_wasm_imports_standard_wasi_fd_protocol() {
    let (bytes, _info) = match build_wasm_read_file(SRC, "relon_main_read_file_imp") {
        Ok(v) => v,
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("read_file_four_way: wasm-ld unavailable; skipped import check");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    use wasmtime::{Engine, ExternType, Module};
    let engine = Engine::default();
    let module = Module::new(&engine, &bytes).expect("Module::new");
    let imports: Vec<(String, String)> = module
        .imports()
        .map(|i| (i.module().to_string(), i.name().to_string()))
        .collect();
    for want in ["path_open", "fd_read", "fd_close"] {
        let found = module.imports().any(|imp| {
            imp.module() == "wasi_snapshot_preview1"
                && imp.name() == want
                && matches!(imp.ty(), ExternType::Func(_))
        });
        assert!(
            found,
            "module must import (wasi_snapshot_preview1 {want} (func)); imports: {imports:?}"
        );
        let leaked = module
            .imports()
            .any(|imp| imp.module() == "env" && imp.name() == want);
        assert!(
            !leaked,
            "{want} leaked onto custom `env` module; retarget attrs ignored: {imports:?}"
        );
    }
}

/// Four-way bit-equal: tree-walk, cranelift, llvm-native, AND wasm32
/// (standard WASI + preopened host) all produce the same file content.
#[test]
fn read_file_four_way_byte_equal() {
    let dir = fixture("4way");

    let tw = run_tree_walk();
    let cl = run_cranelift();
    let llvm = run_llvm_native();

    assert_eq!(tw, CONTENT, "tree-walk content mismatch");
    assert_eq!(cl, tw, "cranelift not byte-equal to tree-walk");
    assert_eq!(llvm, tw, "llvm-native not byte-equal to tree-walk");

    let (bytes, info) = match build_wasm_read_file(SRC, "relon_main_read_file") {
        Ok(v) => v,
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("read_file_four_way: wasm-ld unavailable; skipped wasm arm");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("{e}");
        }
    };
    // Caps: grant ReadsFs in the i64 bitmask.
    let caps = 1i64 << CapabilityBit::ReadsFs.bit_index();
    let wasm = run_wasm_read_file(&bytes, "relon_main_read_file", &info, &dir, caps)
        .expect("wasm read_file must produce content (granted ReadsFs)");

    assert_eq!(
        wasm, tw,
        "wasm32 (standard WASI + preopened host) not byte-equal to tree-walk gold standard"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The wasm arm enforces `ReadsFs`: withholding the runtime grant (caps
/// bitmask = 0) makes the `Op::CheckCap` prologue trap, recording a
/// non-zero `trap_code` (CapabilityDenied) rather than reading the file.
#[test]
fn read_file_ungranted_traps_on_wasm() {
    let dir = fixture("wasm_ungranted");

    let (bytes, info) = match build_wasm_read_file(SRC, "relon_main_read_file") {
        Ok(v) => v,
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("read_file_four_way: wasm-ld unavailable; skipped wasm ungranted check");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("{e}");
        }
    };
    // caps = 0: no ReadsFs grant.
    let result = run_wasm_read_file(&bytes, "relon_main_read_file", &info, &dir, 0);
    let _ = std::fs::remove_dir_all(&dir);
    // `NativeTrap::CapabilityDenied` == 3 (mirrors cranelift's TrapKind).
    const CAPABILITY_DENIED: u64 = 3;
    match result {
        Err(code) => assert_eq!(
            code, CAPABILITY_DENIED,
            "wasm ungranted read_file: expected CapabilityDenied trap_code, got {code}"
        ),
        Ok(content) => panic!("wasm ungranted read_file must trap, but read {content:?}"),
    }
}
