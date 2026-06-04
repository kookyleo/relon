//! P3 §2.2 — wasm closed-world host co-compile (pure-compute inline).
//!
//! Sibling of `aot_wasm_wasi.rs` (the effectful → WASI-import path) and
//! `cocompile_inline.rs` (the native closed-world inline). This proves the
//! wasm line is now symmetric with the native/llvm line: a **pure-compute**
//! `#native` host fn is **co-compiled into the wasm unit and inlined**,
//! rather than crossing a WASI import boundary.
//!
//! Source: `#main(Int x) -> Int\npure_add(x)`. `pure_add` is a pure host
//! fn (empty capability gate — no `Op::CheckCap`), so on wasm32 closed-world
//! it routes to the direct `call @pure_add` that the wasm host-shim
//! co-compile links + inlines into the unit.
//!
//! Assertions (no fake green — mirrors `cocompile_inline.rs`'s discipline):
//!
//! 1. **import absent**: the linked wasm module declares NO
//!    `(import "env" "pure_add")` — the host fn was inlined into the unit,
//!    not imported. Contrast the WASI path, which *does* declare the
//!    import (asserted positively here too, on the same source emitted
//!    open-world, so the test proves a real behavioural difference rather
//!    than a vacuous absence).
//! 2. **value aligned**: the wasm sandbox result equals the native oracle
//!    (`pure_add(35) == 42`), decoded out of the buffer-protocol output.
//!    The value cannot be 42 unless the inlined host body actually ran.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use relon_codegen_llvm::{CodegenTarget, LlvmAotEvaluator, WorldMode};
use relon_eval_api::{Evaluator, NativeArgs, RelonFunction, RuntimeError, Value};

const SRC: &str = "#main(Int x) -> Int\npure_add(x)";
const HOST_FN: &str = "pure_add";
const ADD: i64 = 7;

/// The `#[no_mangle] extern "C"` pure host fn the wasm co-compile links +
/// inlines. `x.wrapping_add(7)` deliberately matches the WASI path's value
/// (`aot_wasm_wasi.rs`) and the native cranelift golden, and — being a
/// wrapping add over the full i64 range — emits no `range(iN …)` return
/// attribute that the LLVM-18 textual-IR parser would reject.
const HOST_SHIM_SRC: &str = r#"
#[no_mangle]
pub extern "C" fn pure_add(x: i64) -> i64 {
    x.wrapping_add(7)
}
"#;

/// Analyze options for ONE **pure** host fn: a default (empty) gate, so
/// the lowering emits no `Op::CheckCap` ahead of the `Op::CallNative` —
/// the in-codegen signal that the call is pure-compute and inlineable.
fn pure_options() -> relon_analyzer::AnalyzeOptions {
    let sig = relon_analyzer::FnSignature {
        name: HOST_FN.to_string(),
        generics: Vec::new(),
        params: vec![relon_analyzer::FnParam {
            name: "_".to_string(),
            ty: relon_analyzer::type_node_simple("Int"),
            optional: false,
        }],
        return_type: relon_analyzer::type_node_simple("Int"),
        variadic_tail: None,
    };
    let mut signatures = HashMap::new();
    signatures.insert(HOST_FN.to_string(), sig);
    // Pure: no gate registered → empty `required_bits` → no CheckCap.
    let mut names = HashSet::new();
    names.insert(HOST_FN.to_string());
    relon_analyzer::AnalyzeOptions {
        host_fn_names: names,
        host_fn_signatures: signatures,
        host_fn_gates: HashMap::new(),
        caps: relon_analyzer::Capabilities::default(),
        strict_mode: false,
        ..Default::default()
    }
}

/// Pure host fn for the native oracle.
struct PureAdd;

impl RelonFunction for PureAdd {
    fn call(
        &self,
        args: NativeArgs,
        _range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        match args.positional.first() {
            Some(Value::Int(x)) => Ok(Value::Int(x.wrapping_add(ADD))),
            other => Err(RuntimeError::Unsupported {
                reason: format!("PureAdd expects Int, got {other:?}"),
            }),
        }
    }
}

/// Native oracle: dispatch the registered pure host fn through MCJIT.
fn native_oracle(x: i64) -> i64 {
    let dynn: Arc<dyn RelonFunction> = Arc::new(PureAdd);
    let mut m: HashMap<String, Arc<dyn RelonFunction>> = HashMap::new();
    m.insert(HOST_FN.to_string(), dynn);
    let ev = LlvmAotEvaluator::from_source_with_options(SRC, &pure_options())
        .expect("native build")
        .with_host_fns(&m);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(x));
    match ev.run_main(args).expect("native dispatch") {
        Value::Int(v) => v,
        other => panic!("native oracle expected Int, got {other:?}"),
    }
}

/// Emit + link a wasm32 module for the pure-host-fn source under the given
/// `WorldMode`. ClosedWorld supplies the host shim for co-compile; OpenWorld
/// leaves the host fn as a WASI import. Returns `(wasm_bytes, info)`.
fn build_wasm(
    entry: &str,
    world: WorldMode,
) -> Result<(Vec<u8>, relon_codegen_llvm::EmitObjectInfo), String> {
    if relon_codegen_llvm::wasm_link::find_wasm_ld().is_none() {
        return Err("wasm-ld not found on PATH (install lld)".into());
    }
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let obj: PathBuf = tmp.join(format!("relon_ccw_{entry}_{pid}.o"));
    let wasm: PathBuf = tmp.join(format!("relon_ccw_{entry}_{pid}.wasm"));

    let shim = match world {
        WorldMode::ClosedWorld => Some(HOST_SHIM_SRC),
        WorldMode::OpenWorld => None,
    };
    let info = LlvmAotEvaluator::emit_object_for_target(
        SRC,
        entry,
        &obj,
        &pure_options(),
        world,
        shim,
        CodegenTarget::Wasm32,
    )
    .map_err(|e| format!("wasm32 emit_object: {e:?}"))?;

    let obj_bytes = std::fs::read(&obj).map_err(|e| format!("read obj: {e}"))?;
    assert_eq!(&obj_bytes[..4], b"\0asm", "emitted object is not wasm");

    relon_codegen_llvm::wasm_link::link_wasm_object(&obj, &wasm, entry)
        .map_err(|e| format!("wasm-ld link: {e:?}"))?;
    let bytes = std::fs::read(&wasm).map_err(|e| format!("read wasm: {e}"))?;
    assert_eq!(&bytes[..4], b"\0asm", "linked module magic");
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&wasm);
    Ok((bytes, info))
}

/// Replicated `#[repr(C)] ArenaState` byte layout (same constants as
/// `aot_wasm.rs` / `aot_wasm_wasi.rs`).
const STATE_OFF_ARENA_BASE: usize = 0;
const STATE_OFF_TAIL_CURSOR: usize = 12;
const STATE_OFF_SCRATCH_BASE: usize = 20;
const STATE_SIZE: usize = 40;

/// Run the buffer-protocol wasm entry in wasmtime. No `env::pure_add`
/// import is registered — the closed-world module must NOT need one
/// (the host fn is inlined into the unit). Instantiation would fail if an
/// unsatisfied import survived, so a successful run is itself a check.
fn run_in_wasmtime(
    bytes: &[u8],
    entry: &str,
    info: &relon_codegen_llvm::EmitObjectInfo,
    x: i64,
) -> i64 {
    use wasmtime::{Engine, Extern, Linker, Module, Store, Val};

    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("wasmtime Module::new");
    let mut store = Store::new(&engine, ());
    // Deliberately register NO host import: the closed-world module is
    // self-contained. If the host fn were still an import, instantiate
    // would fail.
    let linker = Linker::new(&engine);
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate (closed-world module needs no host import)");

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
    let state_ptr = align8(heap_base);
    let arena_off = align8(state_ptr + STATE_SIZE as u32);
    let in_ptr = 0u32;
    let in_len = info.main_root_size;
    let out_ptr = align8(in_ptr + in_len);
    let out_cap = align8(info.return_root_size.max(8) + 16);
    let scratch_base = align8(out_ptr + out_cap);
    let scratch_size = 4096u32;
    let arena_bytes = scratch_base + scratch_size;

    let needed = (arena_off + arena_bytes) as usize;
    let cur = memory.data_size(&store);
    if needed > cur {
        let extra_pages = (needed - cur).div_ceil(65536) as u64;
        memory.grow(&mut store, extra_pages).expect("grow memory");
    }

    let arena_abs = arena_off;
    let mut state = [0u8; STATE_SIZE];
    state[STATE_OFF_ARENA_BASE..STATE_OFF_ARENA_BASE + 8]
        .copy_from_slice(&(arena_abs as u64).to_le_bytes());
    state[STATE_OFF_TAIL_CURSOR..STATE_OFF_TAIL_CURSOR + 4]
        .copy_from_slice(&info.return_root_size.to_le_bytes());
    state[STATE_OFF_SCRATCH_BASE..STATE_OFF_SCRATCH_BASE + 4]
        .copy_from_slice(&scratch_base.to_le_bytes());
    memory
        .write(&mut store, state_ptr as usize, &state)
        .expect("write state");

    let mut in_record = vec![0u8; info.main_root_size as usize];
    let in_off = info.main_fields[0].offset as usize;
    in_record[in_off..in_off + 8].copy_from_slice(&x.to_le_bytes());
    memory
        .write(&mut store, (arena_abs + in_ptr) as usize, &in_record)
        .expect("write in record");

    let func = instance
        .get_func(&mut store, entry)
        .unwrap_or_else(|| panic!("export `{entry}` missing"));
    // caps = 0: pure host fn needs no capability.
    let params = [
        Val::I32(state_ptr as i32),
        Val::I32(in_ptr as i32),
        Val::I32(in_len as i32),
        Val::I32(out_ptr as i32),
        Val::I32(out_cap as i32),
        Val::I64(0),
    ];
    let mut results = [Val::I32(0)];
    func.call(&mut store, &params, &mut results)
        .expect("buffer entry call");
    let written = match results[0] {
        Val::I32(v) => v,
        other => panic!("expected i32 bytes-written, got {other:?}"),
    };
    assert!(
        written >= 0,
        "buffer entry returned trap sentinel {written}"
    );

    let ret_off = info.return_fields[0].offset as usize;
    let mut out = vec![0u8; info.return_root_size as usize];
    memory
        .read(&store, (arena_abs + out_ptr) as usize, &mut out)
        .expect("read out record");
    i64::from_le_bytes(out[ret_off..ret_off + 8].try_into().unwrap())
}

/// Does the wasm module declare an `(import "env" "<name>" (func ...))`?
fn has_func_import(bytes: &[u8], name: &str) -> bool {
    use wasmtime::{Engine, ExternType, Module};
    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("Module::new");
    let found = module.imports().any(|imp| {
        imp.module() == "env" && imp.name() == name && matches!(imp.ty(), ExternType::Func(_))
    });
    found
}

/// Closed-world: the pure host fn is **co-compiled + inlined** into the
/// wasm unit — NO `env::pure_add` import — and the value aligns the native
/// oracle. The companion open-world build of the SAME source DOES declare
/// the import, proving the difference is real (not a vacuous absence).
#[test]
fn pure_host_fn_inlined_into_wasm_unit_no_import_and_aligns_native() {
    let x = 35i64;
    let want = native_oracle(x);
    assert_eq!(want, x + ADD, "native oracle sanity ({x} + {ADD})");

    // --- closed-world (co-compile + inline) ---
    let cw_entry = "relon_main_pure_cw";
    match build_wasm(cw_entry, WorldMode::ClosedWorld) {
        Ok((bytes, info)) => {
            // (1) import MUST be absent — host fn inlined into the unit.
            assert!(
                !has_func_import(&bytes, HOST_FN),
                "closed-world wasm module must NOT import `env::{HOST_FN}` \
                 (the pure host fn must be inlined into the unit)"
            );
            // The native MCJIT helper must never appear either.
            let blob = String::from_utf8_lossy(&bytes);
            assert!(
                !blob.contains("relon_llvm_call_native"),
                "closed-world wasm must not reference the native MCJIT helper"
            );

            // (2) value aligned with the native oracle.
            let got = run_in_wasmtime(&bytes, cw_entry, &info, x);
            assert_eq!(
                got, want,
                "closed-world wasm result {got} != native oracle {want} \
                 (inlined host body must compute the same value)"
            );

            // --- open-world (WASI import) differential: SAME source, but
            // the host fn crosses the boundary as an import. Proves the
            // closed-world absence above is a genuine behavioural delta. ---
            let ow_entry = "relon_main_pure_ow";
            let (ow_bytes, _ow_info) =
                build_wasm(ow_entry, WorldMode::OpenWorld).expect("open-world wasm build");
            assert!(
                has_func_import(&ow_bytes, HOST_FN),
                "open-world wasm module MUST import `env::{HOST_FN}` \
                 (the differential anchor for the closed-world inline)"
            );
            eprintln!(
                "cocompile_wasm: closed-world inline VERIFIED — no env::{HOST_FN} import, \
                 value={got} aligns native oracle {want}; open-world differential has the import"
            );
        }
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("cocompile_wasm: wasm-ld unavailable; skipped (native oracle still ran)");
        }
        Err(e) => panic!("{e}"),
    }
}
