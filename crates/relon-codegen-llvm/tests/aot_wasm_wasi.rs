//! P3 §2.2 — effectful `#native` host fn → wasm import, end to end.
//!
//! The source `#main(Int x) -> Int\nclock_add(x)` declares an effectful
//! host fn `clock_add` (gated on `reads_clock`). On the **native** target
//! it lowers to a dynamic `relon_llvm_call_native` dispatch resolved
//! through the MCJIT host-fn registry (the `gap_callnative.rs` golden,
//! validated against cranelift). On **wasm32** that helper is unreachable
//! inside the sandbox, so `crate::wasi_host` lowers the call to a plain
//! `call @clock_add` against an **undefined external** symbol, which the
//! LLVM WebAssembly backend turns into an `(import "env" "clock_add")`
//! entry kept unresolved by `wasm-ld --allow-undefined`.
//!
//! This test proves the boundary is *really* wired:
//!
//! 1. native oracle: `run_main` dispatches the registered host fn,
//!    `clock_add(35) == 42`;
//! 2. wasm32 emit → `wasm-ld` link → wasmtime: the same source runs in
//!    the sandbox, the `env::clock_add` import is satisfied by a wasmtime
//!    `Linker::func_wrap` host implementation, and the buffer-protocol
//!    output decodes to the SAME `42`;
//! 3. the host implementation's hit counter proves the import was truly
//!    invoked (no fake green — the value cannot be 42 without the import
//!    firing, and the counter cross-checks it).
//!
//! The capability gate (`Op::CheckCap` on the `reads_clock` bit) rides
//! the trailing `i64 caps` param of the buffer entry; we pass the granted
//! bit so the in-sandbox gate opens before the call leaves to the host.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use relon_codegen_llvm::{CodegenTarget, LlvmAotEvaluator, WorldMode};
use relon_eval_api::{CapabilityBit, Evaluator, NativeArgs, RelonFunction, RuntimeError, Value};

const SRC: &str = "#main(Int x) -> Int\nclock_add(x)";
const HOST_FN: &str = "clock_add";
/// The effectful host fn adds 7 to its single Int arg.
const ADD: i64 = 7;

/// Analyze options describing one effectful, `reads_clock`-gated host fn.
/// Mirrors `gap_callnative.rs` so the native oracle and the wasm path
/// consume an identical analyze surface.
fn host_options() -> relon_analyzer::AnalyzeOptions {
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
    let mut gate = relon_analyzer::NativeFnGate::default();
    gate.reads_clock = true;
    let mut gates = HashMap::new();
    gates.insert(HOST_FN.to_string(), gate);
    let mut names = HashSet::new();
    names.insert(HOST_FN.to_string());
    let mut caps = relon_analyzer::Capabilities::default();
    caps.reads_clock = true; // granted at analyze so the build lowers the call
    relon_analyzer::AnalyzeOptions {
        host_fn_names: names,
        host_fn_signatures: signatures,
        host_fn_gates: gates,
        caps,
        strict_mode: false,
        ..Default::default()
    }
}

/// Effectful host fn: adds `ADD` to its Int arg, counting invocations.
struct ClockAdd {
    hits: AtomicU64,
}

impl RelonFunction for ClockAdd {
    fn call(
        &self,
        args: NativeArgs,
        _range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        match args.positional.first() {
            Some(Value::Int(x)) => Ok(Value::Int(x.wrapping_add(ADD))),
            other => Err(RuntimeError::Unsupported {
                reason: format!("ClockAdd expects Int, got {other:?}"),
            }),
        }
    }
}

/// Native oracle: dispatch the registered host fn through MCJIT and read
/// the Int result. This is the value the wasm sandbox must reproduce.
fn native_oracle(x: i64) -> i64 {
    let native = Arc::new(ClockAdd {
        hits: AtomicU64::new(0),
    });
    let dynn: Arc<dyn RelonFunction> = native.clone();
    let mut m: HashMap<String, Arc<dyn RelonFunction>> = HashMap::new();
    m.insert(HOST_FN.to_string(), dynn);

    let ev = LlvmAotEvaluator::from_source_with_options(SRC, &host_options())
        .expect("native build")
        .with_host_fns(&m)
        .with_granted_cap(CapabilityBit::ReadsClock.bit_index());
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(x));
    let v = match ev.run_main(args).expect("native dispatch") {
        Value::Int(v) => v,
        other => panic!("native oracle expected Int, got {other:?}"),
    };
    assert_eq!(
        native.hits.load(Ordering::SeqCst),
        1,
        "native host fn invoked exactly once"
    );
    v
}

/// Emit + link the wasm32 module for the effectful-host-fn source.
/// Returns `(wasm_bytes, EmitObjectInfo)` or an `Err` carrying the
/// reason when `wasm-ld` is unavailable (so the caller skips rather than
/// fails the suite on a host without `lld`).
fn build_wasm(entry: &str) -> Result<(Vec<u8>, relon_codegen_llvm::EmitObjectInfo), String> {
    if relon_codegen_llvm::wasm_link::find_wasm_ld().is_none() {
        return Err("wasm-ld not found on PATH (install lld)".into());
    }
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let obj: PathBuf = tmp.join(format!("relon_wasi_{entry}_{pid}.o"));
    let wasm: PathBuf = tmp.join(format!("relon_wasi_{entry}_{pid}.wasm"));

    let info = LlvmAotEvaluator::emit_object_for_target(
        SRC,
        entry,
        &obj,
        &host_options(),
        WorldMode::OpenWorld,
        None,
        CodegenTarget::Wasm32,
    )
    .map_err(|e| format!("wasm32 emit_object: {e:?}"))?;

    // Must be a buffer-protocol entry: `Op::CallNative` needs the
    // `*state` pointer + the `caps` slot only the buffer entry threads.
    assert!(
        matches!(info.shape, relon_codegen_llvm::EmittedEntryShape::Buffer),
        "expected Buffer entry shape, got {:?}",
        info.shape
    );
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

/// Byte layout of the native `#[repr(C)] ArenaState` struct, replicated
/// in wasm linear memory (same constants as `aot_wasm.rs`).
const STATE_OFF_ARENA_BASE: usize = 0; // i64
const STATE_OFF_TAIL_CURSOR: usize = 12; // u32
const STATE_OFF_SCRATCH_BASE: usize = 20; // u32
const STATE_SIZE: usize = 40;

/// Run the buffer-protocol wasm entry in wasmtime with:
///   * a real arena laid out in linear memory (mirrors `aot_wasm.rs`'s
///     `run_buffer_in_wasmtime`), and
///   * the `env::clock_add` **import satisfied by a host closure** that
///     adds `ADD` and bumps `hit_counter`, proving the call truly left
///     the sandbox.
///
/// `caps` is the trailing `i64` capability bitmask; we pass the granted
/// `reads_clock` bit so the in-sandbox `Op::CheckCap` gate opens.
fn run_wasi_in_wasmtime(
    bytes: &[u8],
    entry: &str,
    info: &relon_codegen_llvm::EmitObjectInfo,
    x: i64,
    caps: i64,
    hit_counter: Arc<AtomicU64>,
) -> i64 {
    use wasmtime::{Engine, Extern, Linker, Module, Store, Val};

    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("wasmtime Module::new");
    let mut store = Store::new(&engine, ());

    // Satisfy the `(import "env" "clock_add")` the wasm module declares.
    // i64-bits ABI: the operand stack passes the Int arg as its i64 bit
    // pattern; the host returns the i64 result the same way.
    let mut linker = Linker::new(&engine);
    {
        let hits = hit_counter.clone();
        linker
            .func_wrap("env", HOST_FN, move |arg: i64| -> i64 {
                hits.fetch_add(1, Ordering::SeqCst);
                arg.wrapping_add(ADD)
            })
            .expect("register clock_add import");
    }

    let instance = linker
        .instantiate(&mut store, &module)
        .expect("wasmtime instantiate (import must be satisfied)");

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

    // Lay out [ArenaState @ state_ptr][arena @ arena_off]; inside the
    // arena: [in_buf @ 0][out_buf][scratch]. Mirrors `aot_wasm.rs`.
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

    // Pack the single Int arg `x` at its declared field offset.
    let mut in_record = vec![0u8; info.main_root_size as usize];
    let in_off = info.main_fields[0].offset as usize;
    in_record[in_off..in_off + 8].copy_from_slice(&x.to_le_bytes());
    memory
        .write(&mut store, (arena_abs + in_ptr) as usize, &in_record)
        .expect("write in record");

    // Call (state_ptr, in_ptr, in_len, out_ptr, out_cap, caps) -> i32.
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
    let written = match results[0] {
        Val::I32(v) => v,
        other => panic!("expected i32 bytes-written, got {other:?}"),
    };
    assert!(
        written >= 0,
        "buffer entry returned trap sentinel {written} (cap gate / dispatch failed)"
    );

    // Decode the Int return out of the output record.
    let ret_off = info.return_fields[0].offset as usize;
    let mut out = vec![0u8; info.return_root_size as usize];
    memory
        .read(&store, (arena_abs + out_ptr) as usize, &mut out)
        .expect("read out record");
    i64::from_le_bytes(out[ret_off..ret_off + 8].try_into().unwrap())
}

/// Effectful host fn crosses the sandbox boundary as a wasm import and
/// the value aligns the native oracle. Granted cap → the gate opens, the
/// import fires, `clock_add(35) == 42`.
#[test]
fn effectful_host_fn_via_wasm_import_aligns_native() {
    let x = 35i64;
    let want = native_oracle(x);
    assert_eq!(want, x + ADD, "native oracle sanity ({x} + {ADD})");

    let entry = "relon_main_clock_add";
    match build_wasm(entry) {
        Ok((bytes, info)) => {
            let hits = Arc::new(AtomicU64::new(0));
            let caps = 1i64 << CapabilityBit::ReadsClock.bit_index();
            let got = run_wasi_in_wasmtime(&bytes, entry, &info, x, caps, hits.clone());
            assert_eq!(
                got, want,
                "wasm32 import result {got} != native oracle {want}"
            );
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "the env::clock_add import must have fired exactly once \
                 (proves the call really left the sandbox)"
            );
        }
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("aot_wasm_wasi: wasm-ld unavailable; skipped (native oracle still ran)");
        }
        Err(e) => panic!("{e}"),
    }
}

/// The emitted wasm module really declares an `(import "env" "clock_add")`
/// — i.e. the effectful host fn was lowered to an import, not inlined and
/// not the native MCJIT helper. Checks the wasm import section directly so
/// the proof doesn't rely on the call merely succeeding.
#[test]
fn emitted_module_declares_clock_add_import() {
    let entry = "relon_main_clock_add_imp";
    match build_wasm(entry) {
        Ok((bytes, _info)) => {
            // The native MCJIT dispatch helper must NOT appear as an
            // import on the wasm path (it is unreachable in the sandbox).
            let blob = String::from_utf8_lossy(&bytes);
            assert!(
                blob.contains(HOST_FN),
                "linked wasm module does not reference the `{HOST_FN}` import symbol"
            );
            assert!(
                !blob.contains("relon_llvm_call_native"),
                "wasm module must not import the native MCJIT dispatch helper"
            );

            // Stronger check: parse the wasm import section via wasmtime
            // and confirm `env::clock_add` is a declared func import.
            use wasmtime::{Engine, ExternType, Module};
            let engine = Engine::default();
            let module = Module::new(&engine, &bytes).expect("Module::new");
            let found = module.imports().any(|imp| {
                imp.module() == "env"
                    && imp.name() == HOST_FN
                    && matches!(imp.ty(), ExternType::Func(_))
            });
            assert!(
                found,
                "wasm module is missing the (import \"env\" \"{HOST_FN}\" (func ...)) entry; \
                 imports: {:?}",
                module
                    .imports()
                    .map(|i| (i.module().to_string(), i.name().to_string()))
                    .collect::<Vec<_>>()
            );
        }
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("aot_wasm_wasi: wasm-ld unavailable; skipped import-section check");
        }
        Err(e) => panic!("{e}"),
    }
}
