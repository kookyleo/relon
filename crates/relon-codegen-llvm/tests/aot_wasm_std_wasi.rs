//! §8 follow-up SPIKE — relon wasm imports the **standard WASI** clock,
//! satisfied by an **off-the-shelf** `wasmtime-wasi` host.
//!
//! ## Why this exists (the design question §8 poses)
//!
//! The production effectful path (`crate::wasi_host` + `tests/aot_wasm_wasi.rs`)
//! lowers an effectful `#native` host fn to a **relon-custom**
//! `(import "env" "<name>")`, satisfied by relon's own wasmtime runner via
//! `Linker::func_wrap`. That binds the module to relon's runner — it is *not*
//! a portable, ecosystem-native wasm module.
//!
//! §8's north star is that a relon wasm product imports **standard WASI**
//! (`wasi_snapshot_preview1::clock_time_get` etc.) so **any** WASI host
//! (wasmtime-wasi / wasmer / jco) can run it. The hard mismatch (see the
//! report): relon's effectful surface is **arbitrary user `#native` fns**
//! with host-chosen signatures, while standard WASI is a **fixed interface
//! with fixed signatures + linear-memory marshalling** (`clock_time_get`
//! takes `(clock_id: i32, precision: i64, *time: i32) -> errno: i32` and
//! writes a `u64` nanosecond timestamp through the pointer). An arbitrary
//! `(i64)->i64` user fn does NOT map onto that shape — closing the gap needs
//! a **built-in, WASI-backed capability primitive** (a clock the compiler
//! knows how to lower), not a user fn.
//!
//! This spike proves the **codegen mechanism** for that primitive in
//! isolation, ahead of any source-level surface:
//!
//!   1. emit a wasm module whose clock read is a **standard**
//!      `(import "wasi_snapshot_preview1" "clock_time_get")` — set via the
//!      LLVM `wasm-import-module` / `wasm-import-name` function attributes,
//!      *not* the default `env` module;
//!   2. marshal through **linear memory** exactly as the WASI ABI requires
//!      (pass a scratch pointer, read back the `u64` the host wrote);
//!   3. satisfy it with the **standard** `wasmtime-wasi` preview1 host
//!      (`p1::add_to_linker_sync` + `WasiP1Ctx`) — NO relon-custom
//!      `func_wrap`, proving the module needs nothing relon-specific.
//!
//! The value alignment is real: the host writes a monotone wall-clock
//! reading; we assert the imported errno is `0` (success) and the returned
//! timestamp is non-zero and within a sane window of the test process's own
//! `SystemTime` reading — i.e. the standard WASI host genuinely produced it.
//!
//! Scope honesty: this is a **hand-built LLVM fixture**, because relon today
//! has no source-level clock primitive (the effectful surface is entirely
//! user `#native`; the stdlib is provably pure — see
//! `relon-evaluator stdlib_rs_uses_no_ambient_apis`). The spike validates the
//! codegen + standard-WASI-host half; wiring it to a source primitive is the
//! larger design called out in the report.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use inkwell::context::Context;
use inkwell::module::Linkage;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
};
use inkwell::OptimizationLevel;

/// `wasm32-wasi` triple — same one the production emitter pins
/// (`evaluator.rs WASM32_TRIPLE`).
const WASM32_TRIPLE: &str = "wasm32-wasi";

/// Build a `wasm32-wasi` object-emit `TargetMachine`. Mirrors the
/// production `create_object_target_machine(Wasm32)` arm.
fn wasm32_machine() -> TargetMachine {
    Target::initialize_webassembly(&InitializationConfig::default());
    let triple = TargetTriple::create(WASM32_TRIPLE);
    let t = Target::from_triple(&triple).expect("wasm32 target from_triple");
    t.create_target_machine(
        &triple,
        "",
        "",
        OptimizationLevel::Default,
        RelocMode::Static,
        CodeModel::Default,
    )
    .expect("create_target_machine(wasm32)")
}

/// Hand-build the wasm32 relocatable object. The module:
///   * declares `clock_time_get` as an **undefined external**
///     `(i32 clock_id, i64 precision, i32 *time) -> i32 errno`, tagged with
///     `wasm-import-module="wasi_snapshot_preview1"` /
///     `wasm-import-name="clock_time_get"` so the LLVM wasm backend emits the
///     **standard WASI** import (not the default `env::clock_time_get`);
///   * exports `relon_read_clock_ns(i32 scratch_ptr) -> i64`: it calls
///     `clock_time_get(0, 0, scratch_ptr)`, and on errno==0 loads the
///     `u64` timestamp the host wrote at `scratch_ptr` and returns it
///     (returns `-errno` on failure so the caller can tell them apart).
///
/// Writes the emitted object to `obj_path` (`\0asm` relocatable).
fn emit_clock_object(entry: &str, obj_path: &std::path::Path) {
    let ctx = Context::create();
    let module = ctx.create_module("relon_std_wasi_clock_spike");
    module.set_triple(&TargetTriple::create(WASM32_TRIPLE));

    let i32_t = ctx.i32_type();
    let i64_t = ctx.i64_type();
    let ptr_t = ctx.ptr_type(inkwell::AddressSpace::default());

    // (i32 clock_id, i64 precision, i32 *time) -> i32 errno.
    // On wasm32 the `*time` pointer is a 32-bit linear-memory offset; LLVM
    // lowers the opaque `ptr` to the wasm `i32` address operand.
    let clock_fn_ty = i32_t.fn_type(&[i32_t.into(), i64_t.into(), ptr_t.into()], false);
    let clock_fn = module.add_function("clock_time_get", clock_fn_ty, Some(Linkage::External));
    // The two attributes that retarget the import OFF the default `env`
    // module ONTO standard WASI. This is the whole codegen crux of §8.
    clock_fn.add_attribute(
        inkwell::attributes::AttributeLoc::Function,
        ctx.create_string_attribute("wasm-import-module", "wasi_snapshot_preview1"),
    );
    clock_fn.add_attribute(
        inkwell::attributes::AttributeLoc::Function,
        ctx.create_string_attribute("wasm-import-name", "clock_time_get"),
    );

    // relon_read_clock_ns(i32 scratch_ptr) -> i64
    let entry_ty = i64_t.fn_type(&[i32_t.into()], false);
    let entry_fn = module.add_function(entry, entry_ty, None);
    let builder = ctx.create_builder();
    let bb = ctx.append_basic_block(entry_fn, "entry");
    builder.position_at_end(bb);

    // scratch_ptr (i32) -> linear-memory pointer.
    let scratch_i32 = entry_fn.get_nth_param(0).unwrap().into_int_value();
    let scratch_ptr = builder
        .build_int_to_ptr(scratch_i32, ptr_t, "scratch_ptr")
        .unwrap();

    // clock_time_get(CLOCK_REALTIME=0, precision=0, scratch_ptr) -> errno
    let clock_realtime = i32_t.const_zero();
    let precision = i64_t.const_zero();
    let call_site = builder
        .build_call(
            clock_fn,
            &[clock_realtime.into(), precision.into(), scratch_ptr.into()],
            "errno",
        )
        .unwrap();
    let errno = match call_site.try_as_basic_value() {
        inkwell::values::ValueKind::Basic(inkwell::values::BasicValueEnum::IntValue(v)) => v,
        other => panic!("clock_time_get returned {other:?}, expected i32 errno"),
    };

    // if errno != 0 -> return -(errno as i64); else load the u64 the host
    // wrote at scratch_ptr (this is the standard WASI marshalling: the host
    // owns the buffer write, the guest reads it back).
    let ok_bb = ctx.append_basic_block(entry_fn, "ok");
    let err_bb = ctx.append_basic_block(entry_fn, "err");
    let is_err = builder
        .build_int_compare(
            inkwell::IntPredicate::NE,
            errno,
            i32_t.const_zero(),
            "is_err",
        )
        .unwrap();
    builder
        .build_conditional_branch(is_err, err_bb, ok_bb)
        .unwrap();

    builder.position_at_end(ok_bb);
    let ts = builder
        .build_load(i64_t, scratch_ptr, "ts")
        .unwrap()
        .into_int_value();
    builder.build_return(Some(&ts)).unwrap();

    builder.position_at_end(err_bb);
    let errno64 = builder.build_int_s_extend(errno, i64_t, "errno64").unwrap();
    let neg = builder.build_int_neg(errno64, "neg_errno").unwrap();
    builder.build_return(Some(&neg)).unwrap();

    // Emit the relocatable wasm32 object. Mirror the production emitter:
    // write straight to a file (LLVM-17 `wasm-ld` rejects the object the
    // in-memory buffer path produces here — `write_to_file` is the path
    // `evaluator.rs emit_object_for_target` uses and links cleanly).
    let machine = wasm32_machine();
    module.set_data_layout(&machine.get_target_data().get_data_layout());
    machine
        .write_to_file(&module, FileType::Object, obj_path)
        .expect("wasm32 write_to_file");
    let bytes = std::fs::read(obj_path).expect("read emitted obj");
    assert_eq!(&bytes[..4], b"\0asm", "emitted object is not a wasm object");
}

/// Emit + `wasm-ld`-link the clock module. Returns `Err` (skip) when
/// `wasm-ld` is unavailable, mirroring `aot_wasm_wasi.rs`.
fn build_wasm(entry: &str) -> Result<Vec<u8>, String> {
    if relon_codegen_llvm::wasm_link::find_wasm_ld().is_none() {
        return Err("wasm-ld not found on PATH (install lld)".into());
    }
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let obj: PathBuf = tmp.join(format!("relon_stdwasi_{entry}_{pid}.o"));
    let wasm: PathBuf = tmp.join(format!("relon_stdwasi_{entry}_{pid}.wasm"));

    emit_clock_object(entry, &obj);

    relon_codegen_llvm::wasm_link::link_wasm_object(&obj, &wasm, entry)
        .map_err(|e| format!("wasm-ld link: {e:?}"))?;
    let bytes = std::fs::read(&wasm).map_err(|e| format!("read wasm: {e}"))?;
    assert_eq!(&bytes[..4], b"\0asm", "linked module magic");
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&wasm);
    Ok(bytes)
}

/// The emitted module declares the **standard** WASI clock import on the
/// `wasi_snapshot_preview1` module (NOT relon's custom `env`), and does NOT
/// declare any `env::*` custom import. Parsed straight from the wasm import
/// section so the proof doesn't rely on the call merely succeeding.
#[test]
fn emitted_module_imports_standard_wasi_clock() {
    let entry = "relon_read_clock_ns_imp";
    match build_wasm(entry) {
        Ok(bytes) => {
            use wasmtime::{Engine, ExternType, Module};
            let engine = Engine::default();
            let module = Module::new(&engine, &bytes).expect("Module::new");

            let imports: Vec<(String, String)> = module
                .imports()
                .map(|i| (i.module().to_string(), i.name().to_string()))
                .collect();

            let found_std_clock = module.imports().any(|imp| {
                imp.module() == "wasi_snapshot_preview1"
                    && imp.name() == "clock_time_get"
                    && matches!(imp.ty(), ExternType::Func(_))
            });
            assert!(
                found_std_clock,
                "module must import (wasi_snapshot_preview1 clock_time_get (func)); imports: {imports:?}"
            );

            // The clock must NOT have leaked onto the relon-custom `env`
            // module — that would mean the import retarget attributes were
            // ignored and the module is still relon-runner-bound.
            let leaked_env_clock = module
                .imports()
                .any(|imp| imp.module() == "env" && imp.name() == "clock_time_get");
            assert!(
                !leaked_env_clock,
                "clock import leaked onto custom `env` module; retarget attrs ignored: {imports:?}"
            );
        }
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("aot_wasm_std_wasi: wasm-ld unavailable; skipped import-section check");
        }
        Err(e) => panic!("{e}"),
    }
}

/// End-to-end: the standard WASI clock import is satisfied by the
/// **off-the-shelf** `wasmtime-wasi` preview1 host (NOT a relon-custom
/// `func_wrap`), the guest marshals the `u64` back out of linear memory, and
/// the value aligns the test process's own wall clock.
#[test]
fn standard_wasi_clock_runs_under_wasmtime_wasi_host() {
    let entry = "relon_read_clock_ns";
    let bytes = match build_wasm(entry) {
        Ok(b) => b,
        Err(reason) if reason.contains("wasm-ld not found") => {
            eprintln!("aot_wasm_std_wasi: wasm-ld unavailable; skipped e2e run");
            return;
        }
        Err(e) => panic!("{e}"),
    };

    use wasmtime::{Engine, Extern, Linker, Module, Store, Val};
    use wasmtime_wasi::p1::{self, WasiP1Ctx};
    use wasmtime_wasi::WasiCtxBuilder;

    // A reference reading from the SAME wall clock the WASI host reads,
    // taken just before the guest call, to bound the returned timestamp.
    let before_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    let engine = Engine::default();
    let module = Module::new(&engine, &bytes).expect("wasmtime Module::new");

    // THE PROOF: the import is satisfied by the *standard* wasmtime-wasi
    // preview1 host — `p1::add_to_linker_sync` wires `clock_time_get` (and the
    // rest of WASIp1) against `WasiP1Ctx`. No relon-custom `func_wrap`.
    let wasi: WasiP1Ctx = WasiCtxBuilder::new().build_p1();
    let mut store = Store::new(&engine, wasi);
    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    p1::add_to_linker_sync(&mut linker, |cx| cx).expect("add standard WASIp1 to linker");

    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate (standard WASI clock import must be satisfied)");

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

    // Hand the clock an 8-byte scratch slot past the static data.
    let scratch = (heap_base + 7) & !7u32;
    let needed = (scratch + 8) as usize;
    let cur = memory.data_size(&store);
    if needed > cur {
        let pages = (needed - cur).div_ceil(65536) as u64;
        memory.grow(&mut store, pages).expect("grow memory");
    }

    let func = instance
        .get_func(&mut store, entry)
        .unwrap_or_else(|| panic!("export `{entry}` missing"));
    let params = [Val::I32(scratch as i32)];
    let mut results = [Val::I64(0)];
    func.call(&mut store, &params, &mut results)
        .expect("clock entry call");
    let ts = match results[0] {
        Val::I64(v) => v,
        other => panic!("expected i64 timestamp, got {other:?}"),
    };

    let after_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    assert!(
        ts >= 0,
        "clock_time_get returned errno {} (negated) — standard WASI host did not satisfy the call",
        -ts
    );
    let ts = ts as u64;

    // Value alignment: the standard WASI host produced a real wall-clock
    // reading, so it must fall within [before, after] of our own reading off
    // the same clock (allow a generous slack for scheduling).
    let slack_ns = 5_000_000_000u64; // 5s
    assert!(
        ts >= before_ns.saturating_sub(slack_ns) && ts <= after_ns.saturating_add(slack_ns),
        "WASI clock reading {ts} ns outside the host window [{before_ns}, {after_ns}] (+/-5s); \
         the standard WASI host's value did not align the process wall clock"
    );
}
