//! P-clock / P-random — built-in WASI-backed capability primitives, e2e.
//!
//! Productionizes the `aot_wasm_std_wasi.rs` spike: a *source-level*
//! `clock()` / `random()` primitive (no hand-built LLVM fixture, no user
//! `#native`) emits, on wasm32, a **standard WASI preview1 import** —
//! `(import "wasi_snapshot_preview1" "clock_time_get")` /
//! `(import "wasi_snapshot_preview1" "random_get")` — satisfied by the
//! **off-the-shelf** `wasmtime-wasi` p1 host (NO relon-custom
//! `func_wrap`). On the native target the same primitive lowers to a
//! host runtime helper (SystemTime / /dev/urandom).
//!
//! Non-determinism honesty: clock / random values are NOT bit-equal
//! across executors, so this asserts per-tier credibility:
//!   * native clock lands within a sane window of the test's own
//!     SystemTime read; native random is non-constant;
//!   * the emitted wasm module's import section is the *standard* WASI
//!     symbol on the `wasi_snapshot_preview1` module (parsed directly);
//!   * the wasm clock, run under the standard WASI host, lands in the
//!     same window — proving the host genuinely produced the value
//!     through the standard import (no fake green);
//!   * the capability gate: granted -> value, ungranted -> trap.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use relon_analyzer::{AnalyzeOptions, Capabilities};
use relon_codegen_llvm::{CodegenTarget, EmitObjectInfo, LlvmAotEvaluator, WorldMode};
use relon_eval_api::{CapabilityBit, Evaluator, Value};

// A `#main` with a param + Int return lowers to the buffer-protocol
// entry shape (the legacy-i64 / fast entry has no `caps` slot, so the
// `Op::CheckCap` gate needs the buffer entry). The `seed` param is
// unused by the body — it only forces the buffer ABI, exactly as the
// effectful-host-fn `aot_wasm_wasi.rs` test does with `#main(Int x)`.
const CLOCK_SRC: &str = "#main(Int seed) -> Int\nclock()";
const RANDOM_SRC: &str = "#main(Int seed) -> Int\nrandom()";

/// Analyze options that grant the given caps (no host fns — the
/// primitives are language-level intrinsics).
fn opts_with_caps(caps: Capabilities) -> AnalyzeOptions {
    AnalyzeOptions {
        caps,
        strict_mode: false,
        ..Default::default()
    }
}

fn caps_clock() -> Capabilities {
    let mut c = Capabilities::default();
    c.reads_clock = true;
    c
}

fn caps_rng() -> Capabilities {
    let mut c = Capabilities::default();
    c.uses_rng = true;
    c
}

// ============================ NATIVE =================================

#[test]
fn native_clock_lands_in_wall_clock_window() {
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    let ev = LlvmAotEvaluator::from_source_with_options(CLOCK_SRC, &opts_with_caps(caps_clock()))
        .expect("native clock build")
        .with_granted_cap(CapabilityBit::ReadsClock.bit_index());
    let mut args = std::collections::HashMap::new();
    args.insert("seed".to_string(), Value::Int(0));
    let v = ev.run_main(args).expect("run clock");
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    let ns = match v {
        Value::Int(n) => n,
        other => panic!("clock() expected Int, got {other:?}"),
    };
    let slack = 5_000_000_000i64;
    assert!(
        ns >= before - slack && ns <= after + slack,
        "native clock {ns} ns outside window [{before}, {after}] (+/-5s)"
    );
}

#[test]
fn native_random_is_non_constant() {
    let build = || {
        LlvmAotEvaluator::from_source_with_options(RANDOM_SRC, &opts_with_caps(caps_rng()))
            .expect("native random build")
            .with_granted_cap(CapabilityBit::UsesRng.bit_index())
    };
    let pick = || {
        let mut args = std::collections::HashMap::new();
        args.insert("seed".to_string(), Value::Int(0));
        match build().run_main(args).expect("run random") {
            Value::Int(n) => n,
            other => panic!("random() expected Int, got {other:?}"),
        }
    };
    let (a, b, c) = (pick(), pick(), pick());
    assert!(
        !(a == b && b == c),
        "three native random() reads identical ({a}); RNG frozen"
    );
}

// ============================ WASM32 =================================

/// Emit + `wasm-ld`-link the wasm32 module for `src`. Returns `Err`
/// (skip) when `wasm-ld` is unavailable.
fn build_wasm(
    src: &str,
    entry: &str,
    caps: Capabilities,
) -> Result<(Vec<u8>, EmitObjectInfo), String> {
    if relon_codegen_llvm::wasm_link::find_wasm_ld().is_none() {
        return Err("wasm-ld not found on PATH (install lld)".into());
    }
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let obj: PathBuf = tmp.join(format!("relon_wasicap_{entry}_{pid}.o"));
    let wasm: PathBuf = tmp.join(format!("relon_wasicap_{entry}_{pid}.wasm"));

    let info = LlvmAotEvaluator::emit_object_for_target(
        src,
        entry,
        &obj,
        &opts_with_caps(caps),
        WorldMode::OpenWorld,
        None,
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

/// Replicated `#[repr(C)] ArenaState` byte layout (mirrors aot_wasm_wasi.rs).
const STATE_OFF_ARENA_BASE: usize = 0;
const STATE_OFF_TAIL_CURSOR: usize = 12;
const STATE_OFF_SCRATCH_BASE: usize = 20;
const STATE_SIZE: usize = 40;

/// Run the buffer-protocol wasm entry under wasmtime with the
/// **standard** `wasmtime-wasi` preview1 host satisfying the WASI
/// import (NO relon-custom func_wrap). Returns the decoded Int result.
fn run_under_standard_wasi(bytes: &[u8], entry: &str, info: &EmitObjectInfo, caps: i64) -> i64 {
    use wasmtime::{Engine, Extern, Linker, Module, Store, Val};
    use wasmtime_wasi::p1::{self, WasiP1Ctx};
    use wasmtime_wasi::WasiCtxBuilder;

    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("wasmtime Module::new");

    // THE PROOF: the WASI import is satisfied by the standard
    // wasmtime-wasi p1 host, not a relon-custom func_wrap.
    let wasi: WasiP1Ctx = WasiCtxBuilder::new().build_p1();
    let mut store = Store::new(&engine, wasi);
    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    p1::add_to_linker_sync(&mut linker, |cx| cx).expect("add standard WASIp1 to linker");

    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate (standard WASI import must be satisfied)");

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
    let out_ptr = align8(in_ptr + in_len.max(8));
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

    let ret_off = info.return_fields[0].offset as usize;
    let mut out = vec![0u8; info.return_root_size as usize];
    memory
        .read(&store, (arena_abs + out_ptr) as usize, &mut out)
        .expect("read out record");
    i64::from_le_bytes(out[ret_off..ret_off + 8].try_into().unwrap())
}

/// The emitted wasm module imports the **standard** WASI clock symbol on
/// the `wasi_snapshot_preview1` module (NOT relon-custom `env`), parsed
/// straight from the import section.
#[test]
fn wasm_clock_imports_standard_wasi_symbol() {
    let entry = "relon_main_clock";
    match build_wasm(CLOCK_SRC, entry, caps_clock()) {
        Ok((bytes, _)) => {
            use wasmtime::{Engine, ExternType, Module};
            let engine = Engine::default();
            let module = Module::new(&engine, &bytes).expect("Module::new");
            let imports: Vec<(String, String)> = module
                .imports()
                .map(|i| (i.module().to_string(), i.name().to_string()))
                .collect();
            let found = module.imports().any(|imp| {
                imp.module() == "wasi_snapshot_preview1"
                    && imp.name() == "clock_time_get"
                    && matches!(imp.ty(), ExternType::Func(_))
            });
            assert!(
                found,
                "module must import (wasi_snapshot_preview1 clock_time_get); imports: {imports:?}"
            );
            let leaked = module
                .imports()
                .any(|imp| imp.module() == "env" && imp.name() == "clock_time_get");
            assert!(!leaked, "clock leaked onto custom `env`: {imports:?}");
        }
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("wasi_cap_primitives: wasm-ld unavailable; skipped import-section check");
        }
        Err(e) => panic!("{e}"),
    }
}

/// The emitted wasm module imports the **standard** WASI `random_get`.
#[test]
fn wasm_random_imports_standard_wasi_symbol() {
    let entry = "relon_main_random";
    match build_wasm(RANDOM_SRC, entry, caps_rng()) {
        Ok((bytes, _)) => {
            use wasmtime::{Engine, ExternType, Module};
            let engine = Engine::default();
            let module = Module::new(&engine, &bytes).expect("Module::new");
            let imports: Vec<(String, String)> = module
                .imports()
                .map(|i| (i.module().to_string(), i.name().to_string()))
                .collect();
            let found = module.imports().any(|imp| {
                imp.module() == "wasi_snapshot_preview1"
                    && imp.name() == "random_get"
                    && matches!(imp.ty(), ExternType::Func(_))
            });
            assert!(
                found,
                "module must import (wasi_snapshot_preview1 random_get); imports: {imports:?}"
            );
        }
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("wasi_cap_primitives: wasm-ld unavailable; skipped import-section check");
        }
        Err(e) => panic!("{e}"),
    }
}

/// End-to-end: the standard WASI clock import, satisfied by the
/// off-the-shelf wasmtime-wasi host, produces a value that lands in the
/// test process's own wall-clock window — the host genuinely produced it
/// through the standard import.
#[test]
fn wasm_clock_runs_under_standard_wasi_host() {
    let entry = "relon_main_clock_run";
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    match build_wasm(CLOCK_SRC, entry, caps_clock()) {
        Ok((bytes, info)) => {
            let caps = 1i64 << CapabilityBit::ReadsClock.bit_index();
            let ns = run_under_standard_wasi(&bytes, entry, &info, caps);
            let after = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64;
            let slack = 5_000_000_000i64;
            assert!(
                ns >= before - slack && ns <= after + slack,
                "wasm WASI clock {ns} ns outside window [{before}, {after}] (+/-5s); \
                 the standard WASI host's value did not align the wall clock"
            );
        }
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("wasi_cap_primitives: wasm-ld unavailable; skipped clock e2e run");
        }
        Err(e) => panic!("{e}"),
    }
}

/// End-to-end: the standard WASI `random_get` import, satisfied by the
/// off-the-shelf wasmtime-wasi host, produces a non-constant value.
#[test]
fn wasm_random_runs_under_standard_wasi_host() {
    let entry = "relon_main_random_run";
    match build_wasm(RANDOM_SRC, entry, caps_rng()) {
        Ok((bytes, info)) => {
            let caps = 1i64 << CapabilityBit::UsesRng.bit_index();
            let a = run_under_standard_wasi(&bytes, entry, &info, caps);
            let b = run_under_standard_wasi(&bytes, entry, &info, caps);
            let c = run_under_standard_wasi(&bytes, entry, &info, caps);
            assert!(
                !(a == b && b == c),
                "three wasm WASI random() reads identical ({a}); host RNG frozen or marshalling broken"
            );
        }
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("wasi_cap_primitives: wasm-ld unavailable; skipped random e2e run");
        }
        Err(e) => panic!("{e}"),
    }
}

/// Capability gate on wasm: an ungranted caps bitmask (the
/// `Op::CheckCap` guard sees the bit clear) traps before the WASI import
/// fires — the buffer entry returns the negative trap sentinel.
#[test]
fn wasm_clock_ungranted_traps() {
    let entry = "relon_main_clock_deny";
    match build_wasm(CLOCK_SRC, entry, caps_clock()) {
        Ok((bytes, info)) => {
            use wasmtime::{Engine, Extern, Linker, Module, Store, Val};
            use wasmtime_wasi::p1::{self, WasiP1Ctx};
            use wasmtime_wasi::WasiCtxBuilder;

            let engine = Engine::default();
            let module = Module::new(&engine, &bytes).expect("Module::new");
            let wasi: WasiP1Ctx = WasiCtxBuilder::new().build_p1();
            let mut store = Store::new(&engine, wasi);
            let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
            p1::add_to_linker_sync(&mut linker, |cx| cx).expect("add WASIp1");
            let instance = linker
                .instantiate(&mut store, &module)
                .expect("instantiate");

            let memory = match instance.get_export(&mut store, "memory") {
                Some(Extern::Memory(m)) => m,
                _ => panic!("missing memory"),
            };
            let heap_base = match instance.get_export(&mut store, "__heap_base") {
                Some(Extern::Global(g)) => match g.get(&mut store) {
                    Val::I32(v) => v as u32,
                    other => panic!("__heap_base not i32: {other:?}"),
                },
                _ => panic!("missing __heap_base"),
            };
            let align8 = |v: u32| (v + 7) & !7u32;
            let state_ptr = align8(heap_base);
            let arena_off = align8(state_ptr + STATE_SIZE as u32);
            let out_ptr = align8(info.main_root_size.max(8));
            let out_cap = align8(info.return_root_size.max(8) + 16);
            let scratch_base = align8(out_ptr + out_cap);
            let needed = (arena_off + scratch_base + 4096) as usize;
            let cur = memory.data_size(&store);
            if needed > cur {
                memory
                    .grow(&mut store, (needed - cur).div_ceil(65536) as u64)
                    .expect("grow");
            }
            let mut state = [0u8; STATE_SIZE];
            state[STATE_OFF_ARENA_BASE..STATE_OFF_ARENA_BASE + 8]
                .copy_from_slice(&(arena_off as u64).to_le_bytes());
            state[STATE_OFF_TAIL_CURSOR..STATE_OFF_TAIL_CURSOR + 4]
                .copy_from_slice(&info.return_root_size.to_le_bytes());
            state[STATE_OFF_SCRATCH_BASE..STATE_OFF_SCRATCH_BASE + 4]
                .copy_from_slice(&scratch_base.to_le_bytes());
            memory
                .write(&mut store, state_ptr as usize, &state)
                .expect("write state");

            let func = instance.get_func(&mut store, entry).expect("entry");
            // caps = 0: the reads_clock bit is NOT granted.
            let params = [
                Val::I32(state_ptr as i32),
                Val::I32(0),
                Val::I32(info.main_root_size as i32),
                Val::I32(out_ptr as i32),
                Val::I32(out_cap as i32),
                Val::I64(0),
            ];
            let mut results = [Val::I32(0)];
            let call = func.call(&mut store, &params, &mut results);
            // The cap gate fires either as a returned trap sentinel
            // (negative bytes-written) or a wasm trap — both are denials.
            let denied = match call {
                Ok(()) => matches!(results[0], Val::I32(w) if w < 0),
                Err(_) => true,
            };
            assert!(
                denied,
                "ungranted wasm clock() must be denied, got {results:?}"
            );
        }
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("wasi_cap_primitives: wasm-ld unavailable; skipped wasm cap-gate check");
        }
        Err(e) => panic!("{e}"),
    }
}
