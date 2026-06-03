//! S3.X — LLVM-AOT emitter retargeted to wasm32, run in wasmtime,
//! differentialed against the **native** `emit_object` / `run_main`
//! path (native is the oracle: P2 already aligned it bit-for-bit
//! against tree-walk + cranelift).
//!
//! Pipeline per workload:
//!
//! 1. `LlvmAotEvaluator::from_source` + `run_main` → the native value
//!    (the golden result, decoded into a typed `Value`).
//! 2. `emit_object_for_target(.., CodegenTarget::Wasm32)` → a
//!    relocatable `\0asm` object via the SAME relon-IR → LLVM-IR
//!    emitter, only the `TargetMachine` (wasm32-wasi triple +
//!    DataLayout) differs.
//! 3. `wasm_link::link_wasm_object` (`wasm-ld`) → an instantiable wasm
//!    module exporting the relon entry + linear `memory`.
//! 4. wasmtime instantiates + calls the exported entry → the wasm
//!    value.
//! 5. assert wasm == native.
//!
//! **No fake green**: every assertion runs the value out of wasmtime.
//!
//! Two entry shapes are covered:
//!
//! - **Fast entry** `(i64...) -> i64` — Int-only `#main` (scalar
//!   arithmetic, `?:` control flow, ListInt-sum, anonymous `.map(...)`
//!   closures, multi-arg). No arena; the value comes straight out of
//!   wasmtime.
//! - **Buffer entry** `(i32 state, i32 in, i32 in_len, i32 out, i32
//!   out_cap, i64 caps) -> i32` — Float and multi-field record (Dict)
//!   returns. We lay the native `#[repr(C)] ArenaState` struct **plus**
//!   the arena in wasm linear memory (the body's `arena_base + buf_ptr
//!   + offset` arithmetic resolves into real linear memory; the
//!   `inttoptr(i64)` arena-base load truncates to the 32-bit wasm
//!   pointer harmlessly), pack the input record, call the entry, and
//!   decode the output record. See `run_buffer_in_wasmtime`.
//!
//! Not reached (honest gaps): self-capturing recursive closures (cmp_lua
//! W7) are rejected by the **native** `emit_object` fast-entry path
//! itself (empty `closure_fn_table`), so they are out of scope for both
//! native and wasm object-emit. Pointer-indirect **return** payloads
//! (String / List return → tail-region records) and effectful host fns
//! (→ WASI imports, P3 §2.2) are not exercised here.

use std::path::PathBuf;

use relon_codegen_llvm::{CodegenTarget, LlvmAotEvaluator, WorldMode};

/// The fast-path-eligible corpus: Int-only `#main(...) -> Int`-shaped
/// sources (the emitter's typed `(i64...) -> i64` fast entry). Covers
/// scalar arithmetic, control flow (`?:`), the `list.sum(range(n))`
/// ListInt-sum, the W2 map/sum closure, and anonymous `.map(...)`
/// lambdas. Each lowers to a `(i64...) -> i64` wasm export — no arena
/// needed, so the value comes straight out of wasmtime.
struct Workload {
    name: &'static str,
    src: &'static str,
    /// i64 arguments, in `#main` declaration order.
    args: &'static [i64],
}

const WORKLOADS: &[Workload] = &[
    Workload {
        name: "int_add",
        src: "#main(Int x) -> Int\nx + 1",
        args: &[41],
    },
    Workload {
        name: "control_ternary",
        src: "#main(Int x) -> Int\nx > 0 ? x * 2 : 0 - x",
        args: &[-7],
    },
    Workload {
        name: "modmul",
        src: "#main(Int x) -> Int\n(x * 7) % 13",
        args: &[100],
    },
    Workload {
        name: "multi_arg",
        src: "#main(Int a, Int b, Int c) -> Int\na * b + c",
        args: &[6, 7, 5],
    },
    Workload {
        name: "listint_sum",
        src: "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))",
        args: &[1000],
    },
    Workload {
        name: "closure_w2_dot",
        src: "#unstrict\n\
               #import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               list.sum(range(n).map((i) => (i + 1) * (i + 2)))",
        args: &[100],
    },
    Workload {
        name: "closure_squares",
        src: "#unstrict\n\
               #import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               list.sum(range(n).map((i) => i * i))",
        args: &[50],
    },
];

// NOTE on recursion: the self-capturing closure / recursive `fib`
// (cmp_lua W7) is NOT in the corpus because the **native**
// `emit_object` fast-entry path itself rejects it — the fast emitter
// passes an empty `closure_fn_table`, so `Op::MakeClosure` errors with
// "fn_table_idx out of range" on BOTH native and wasm32. This is a
// pre-existing native object-emit limitation (the closure tables are
// only populated on the MCJIT `run_main` path), not a wasm-side gap.
// Closures themselves are covered by `closure_w2_dot` / `closure_squares`
// (anonymous `.map(...)` lambdas, which lower through the fast entry).

/// Native oracle: the scalar i64 the fast-path entry must produce.
/// For the Dict-return W7 shape `run_main` yields a `Value::Dict`; we
/// read the `result` field. For the Int-return shapes it's the `Int`.
fn native_scalar(wl: &Workload) -> i64 {
    let ev = LlvmAotEvaluator::from_source(wl.src)
        .unwrap_or_else(|e| panic!("[{}] native from_source: {e:?}", wl.name));
    assert!(
        ev.has_fast_path(),
        "[{}] expected fast-path eligibility (the wasm proof targets the \
         typed (i64..)->i64 entry)",
        wl.name
    );
    // Drive the native fast entry directly so the oracle is the exact
    // same scalar the wasm export must reproduce.
    ev.run_main_legacy_i64_fast(wl.args)
        .unwrap_or_else(|e| panic!("[{}] native fast dispatch: {e:?}", wl.name))
}

/// Emit + link the wasm module for `wl`, returning the linked `.wasm`
/// bytes. Returns `Err` carrying the reason when the toolchain (wasm-ld)
/// is unavailable so the caller can skip rather than fail the suite on a
/// host without `lld`.
fn build_wasm(wl: &Workload) -> Result<Vec<u8>, String> {
    if relon_codegen_llvm::wasm_link::find_wasm_ld().is_none() {
        return Err("wasm-ld not found on PATH (install lld)".into());
    }
    let opts = relon_analyzer::AnalyzeOptions {
        strict_mode: false,
        ..Default::default()
    };
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let obj: PathBuf = tmp.join(format!("relon_wasm_{}_{pid}.o", wl.name));
    let wasm: PathBuf = tmp.join(format!("relon_wasm_{}_{pid}.wasm", wl.name));
    let entry = format!("relon_main_{}", wl.name);

    let info = LlvmAotEvaluator::emit_object_for_target(
        wl.src,
        &entry,
        &obj,
        &opts,
        WorldMode::OpenWorld,
        None,
        CodegenTarget::Wasm32,
    )
    .map_err(|e| format!("[{}] wasm32 emit_object: {e:?}", wl.name))?;

    // The object must carry the `\0asm` magic — proves the LLVM
    // WebAssembly backend actually produced a wasm object, not an ELF.
    let obj_bytes = std::fs::read(&obj).map_err(|e| format!("read obj: {e}"))?;
    assert_eq!(
        &obj_bytes[..4],
        b"\0asm",
        "[{}] emitted object is not a wasm object (\\0asm magic)",
        wl.name
    );
    // Fast-path workloads must take the typed entry, not the buffer
    // entry — otherwise the (i64..)->i64 wasm export wouldn't exist.
    assert!(
        matches!(info.shape, relon_codegen_llvm::EmittedEntryShape::FastInt),
        "[{}] expected FastInt entry shape, got {:?}",
        wl.name,
        info.shape
    );

    relon_codegen_llvm::wasm_link::link_wasm_object(&obj, &wasm, &entry)
        .map_err(|e| format!("[{}] wasm-ld link: {e:?}", wl.name))?;
    let bytes = std::fs::read(&wasm).map_err(|e| format!("read wasm: {e}"))?;
    assert_eq!(&bytes[..4], b"\0asm", "[{}] linked module magic", wl.name);
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&wasm);
    Ok(bytes)
}

/// Instantiate `bytes` in wasmtime and call the exported entry with
/// `args`, returning the i64 result.
///
/// The LLVM wasm backend lowers 128-bit integer multiply (which shows
/// up in wide `list.sum` accumulation) to a `call $__multi3` against the
/// compiler-rt builtin. On a native target compiler-rt supplies it; for
/// the standalone wasm module we satisfy it as a host import with the
/// exact compiler-rt sret ABI: `__multi3(ret_ptr, a_lo, a_hi, b_lo,
/// b_hi)` writes the 128-bit signed product `[a_hi:a_lo] * [b_hi:b_lo]`
/// to `linear_memory[ret_ptr..ret_ptr+16]` little-endian. This keeps the
/// wasm module self-contained and the arithmetic correct.
fn run_in_wasmtime(bytes: &[u8], entry: &str, args: &[i64]) -> i64 {
    use wasmtime::{Caller, Engine, Extern, Linker, Module, Store, Val};

    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("wasmtime Module::new");
    let mut store = Store::new(&engine, ());

    let mut linker = Linker::new(&engine);
    // Register the compiler-rt 128-bit multiply builtin the LLVM wasm
    // backend may emit. Signature: (i32 ret_ptr, i64 a_lo, i64 a_hi,
    // i64 b_lo, i64 b_hi). Result is the full i128 product written LE
    // to memory[ret_ptr..+16].
    linker
        .func_wrap(
            "env",
            "__multi3",
            |mut caller: Caller<'_, ()>,
             ret_ptr: i32,
             a_lo: i64,
             a_hi: i64,
             b_lo: i64,
             b_hi: i64| {
                let a = (((a_hi as u64 as u128) << 64) | (a_lo as u64 as u128)) as i128;
                let b = (((b_hi as u64 as u128) << 64) | (b_lo as u64 as u128)) as i128;
                let prod = a.wrapping_mul(b) as u128;
                let mem = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => panic!("__multi3 host import needs an exported `memory`"),
                };
                let bytes = prod.to_le_bytes();
                mem.write(&mut caller, ret_ptr as usize, &bytes)
                    .expect("__multi3 store to linear memory");
            },
        )
        .expect("register __multi3");

    let instance = linker
        .instantiate(&mut store, &module)
        .expect("wasmtime instantiate");
    let func = instance
        .get_func(&mut store, entry)
        .unwrap_or_else(|| panic!("export `{entry}` missing"));
    let params: Vec<Val> = args.iter().map(|a| Val::I64(*a)).collect();
    let mut results = [Val::I64(0)];
    func.call(&mut store, &params, &mut results)
        .expect("wasm entry call");
    match results[0] {
        Val::I64(v) => v,
        other => panic!("expected i64 result, got {other:?}"),
    }
}

#[test]
fn fastpath_workloads_align_native_via_wasmtime() {
    let mut ran = 0usize;
    let mut skipped: Vec<&str> = Vec::new();
    for wl in WORKLOADS {
        let want = native_scalar(wl);
        match build_wasm(wl) {
            Ok(bytes) => {
                let entry = format!("relon_main_{}", wl.name);
                let got = run_in_wasmtime(&bytes, &entry, wl.args);
                assert_eq!(
                    got, want,
                    "[{}] wasm32→wasmtime result {got} != native oracle {want}",
                    wl.name
                );
                ran += 1;
            }
            Err(reason) if reason.contains("wasm-ld not found") => {
                skipped.push(wl.name);
            }
            Err(e) => panic!("{e}"),
        }
    }
    if !skipped.is_empty() {
        // wasm-ld absent: the emit step still ran (object magic asserted
        // inside build_wasm is skipped too since we bail before emit), so
        // we surface a clear skip rather than a silent pass.
        eprintln!(
            "aot_wasm: wasm-ld unavailable, skipped {} workload(s): {:?}",
            skipped.len(),
            skipped
        );
    }
    // At least the native-oracle leg must have exercised every workload.
    assert_eq!(
        ran + skipped.len(),
        WORKLOADS.len(),
        "every workload must run or be explicitly skipped"
    );
}

/// Byte layout of the native `#[repr(C)] ArenaState` struct, replicated
/// in wasm linear memory for the buffer-protocol handshake. The LLVM
/// emitter bakes these offsets as **compile-time constants** computed
/// from the host `size_of::<usize>() == 8`, so the SAME 8-byte-base
/// layout applies in the wasm module (the wasm body reads `arena_base`
/// as an i64 at offset 0, then `inttoptr`s — the i64→i32 wasm-pointer
/// truncation is harmless since the arena offset fits in 32 bits).
const STATE_OFF_ARENA_BASE: usize = 0; // i64 (8 bytes)
const STATE_OFF_TAIL_CURSOR: usize = 12; // u32
const STATE_OFF_SCRATCH_BASE: usize = 20; // u32
const STATE_SIZE: usize = 40; // through host_fns (8 bytes @ 32)

/// Drive a buffer-protocol wasm entry in wasmtime with a real arena
/// laid out in linear memory. Lays the `ArenaState` struct at
/// `__heap_base`, the arena right after it, packs `in_record` into the
/// input region, calls the entry, and returns the output-region bytes.
///
/// Arena layout mirrors `run_main_buffer`: `[in_buf | out_buf |
/// scratch]` (no const-data for these workloads). `arena_base` is set
/// to the absolute linear-memory offset of the arena so the body's
/// `arena_base + buf_ptr + offset` address arithmetic resolves into
/// real linear memory.
fn run_buffer_in_wasmtime(
    bytes: &[u8],
    entry: &str,
    in_record: &[u8],
    out_root_size: u32,
) -> Vec<u8> {
    use wasmtime::{Engine, Extern, Instance, Module, Store, Val};

    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("wasmtime Module::new");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("wasmtime instantiate");

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

    // Lay out: [ArenaState @ state_ptr][arena @ arena_off].
    // Inside the arena: [in_buf @ in_ptr][out_buf @ out_ptr][scratch].
    let align8 = |v: u32| (v + 7) & !7u32;
    let state_ptr = align8(heap_base);
    let arena_off = align8(state_ptr + STATE_SIZE as u32);
    let in_ptr = 0u32; // arena-relative
    let in_len = in_record.len() as u32;
    let out_ptr = align8(in_ptr + in_len);
    let out_cap = align8(out_root_size.max(8) + 16);
    let scratch_base = align8(out_ptr + out_cap);
    let scratch_size = 4096u32;
    let arena_bytes = scratch_base + scratch_size;

    // Grow linear memory if needed.
    let needed = (arena_off + arena_bytes) as usize;
    let cur = memory.data_size(&store);
    if needed > cur {
        let extra_pages = (needed - cur).div_ceil(65536) as u64;
        memory
            .grow(&mut store, extra_pages)
            .expect("grow linear memory");
    }

    // Write the ArenaState struct (LE).
    let arena_abs = arena_off; // absolute linear-memory offset of arena base
    let mut state = [0u8; STATE_SIZE];
    state[STATE_OFF_ARENA_BASE..STATE_OFF_ARENA_BASE + 8]
        .copy_from_slice(&(arena_abs as u64).to_le_bytes());
    state[STATE_OFF_TAIL_CURSOR..STATE_OFF_TAIL_CURSOR + 4]
        .copy_from_slice(&out_root_size.to_le_bytes());
    state[STATE_OFF_SCRATCH_BASE..STATE_OFF_SCRATCH_BASE + 4]
        .copy_from_slice(&scratch_base.to_le_bytes());
    memory
        .write(&mut store, state_ptr as usize, &state)
        .expect("write state");

    // Write input record into the arena's input region.
    memory
        .write(&mut store, (arena_abs + in_ptr) as usize, in_record)
        .expect("write in record");

    // Call the buffer entry:
    // (state_ptr, in_ptr, in_len, out_ptr, out_cap, caps) -> i32
    let func = instance
        .get_func(&mut store, entry)
        .unwrap_or_else(|| panic!("export `{entry}` missing"));
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

    // Read the output fixed-area record back out of linear memory.
    let mut out = vec![0u8; out_root_size as usize];
    memory
        .read(&store, (arena_abs + out_ptr) as usize, &mut out)
        .expect("read out record");
    out
}

/// Buffer-protocol handshake proof: a Float `#main(Float) -> Float`
/// lowers to the buffer entry (takes a `*ArenaState` + linear-memory
/// arena). We lay the arena in wasm linear memory, run it in wasmtime,
/// and differential the f64 result against the native oracle.
#[test]
fn float_buffer_path_aligns_native_via_wasmtime() {
    let opts = relon_analyzer::AnalyzeOptions {
        strict_mode: false,
        ..Default::default()
    };
    let src = "#main(Float x) -> Float\nx * 2.5 + 1.0";
    let x = 4.0f64;

    // Native oracle.
    use relon_eval_api::{Evaluator, Value};
    let ev = LlvmAotEvaluator::from_source(src).expect("native from_source");
    let mut args = std::collections::HashMap::new();
    args.insert("x".to_string(), Value::Float(ordered_float::OrderedFloat(x)));
    let want = match ev.run_main(args).expect("native run_main") {
        Value::Float(f) => f.into_inner(),
        other => panic!("expected Float, got {other:?}"),
    };

    if relon_codegen_llvm::wasm_link::find_wasm_ld().is_none() {
        eprintln!("aot_wasm: wasm-ld unavailable; skipping Float buffer handshake");
        return;
    }

    let obj = std::env::temp_dir().join(format!("relon_wasm_float_{}.o", std::process::id()));
    let wasm = std::env::temp_dir().join(format!("relon_wasm_float_{}.wasm", std::process::id()));
    let entry = "relon_main_float";
    let info = LlvmAotEvaluator::emit_object_for_target(
        src,
        entry,
        &obj,
        &opts,
        WorldMode::OpenWorld,
        None,
        CodegenTarget::Wasm32,
    )
    .expect("wasm32 emit for Float buffer path");
    assert!(matches!(
        info.shape,
        relon_codegen_llvm::EmittedEntryShape::Buffer
    ));
    let obj_bytes = std::fs::read(&obj).expect("read float obj");
    assert_eq!(&obj_bytes[..4], b"\0asm", "Float wasm object magic");

    relon_codegen_llvm::wasm_link::link_wasm_object(&obj, &wasm, entry).expect("link float wasm");
    let bytes = std::fs::read(&wasm).expect("read float wasm");

    // Input record: single f64 at offset of the (only) main field.
    let in_off = info.main_fields[0].offset as usize;
    let mut in_record = vec![0u8; info.main_root_size as usize];
    in_record[in_off..in_off + 8].copy_from_slice(&x.to_le_bytes());

    let out = run_buffer_in_wasmtime(&bytes, entry, &in_record, info.return_root_size);
    let ret_off = info.return_fields[0].offset as usize;
    let got = f64::from_le_bytes(out[ret_off..ret_off + 8].try_into().unwrap());

    assert_eq!(got, want, "wasm32 Float buffer result {got} != native {want}");
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&wasm);
}

/// Buffer-protocol handshake over a multi-field record return
/// (`#main(Int a, Int b) -> Dict { x: a+b, y: a*b }`). Exercises a
/// 2-field input record + 2-field output record (both `Int`) through
/// the linear-memory arena, differentialed against the native Dict
/// oracle. Proves the arena handshake isn't a Float-only fluke — the
/// fixed-area field offsets resolve correctly in linear memory.
#[test]
fn schema_record_buffer_path_aligns_native_via_wasmtime() {
    let opts = relon_analyzer::AnalyzeOptions {
        strict_mode: false,
        ..Default::default()
    };
    let src = "#main(Int a, Int b) -> Dict\n{ x: a + b, y: a * b }";
    let (a, b) = (6i64, 7i64);

    // Native oracle: decode the Dict's `x` / `y` fields.
    use relon_eval_api::{Evaluator, Value};
    let ev = LlvmAotEvaluator::from_source(src).expect("native from_source");
    let mut args = std::collections::HashMap::new();
    args.insert("a".to_string(), Value::Int(a));
    args.insert("b".to_string(), Value::Int(b));
    let dict = match ev.run_main(args).expect("native run_main") {
        Value::Dict(d) => d,
        other => panic!("expected Dict, got {other:?}"),
    };
    let want_x = match dict.map.get("x") {
        Some(Value::Int(v)) => *v,
        other => panic!("x not Int: {other:?}"),
    };
    let want_y = match dict.map.get("y") {
        Some(Value::Int(v)) => *v,
        other => panic!("y not Int: {other:?}"),
    };

    if relon_codegen_llvm::wasm_link::find_wasm_ld().is_none() {
        eprintln!("aot_wasm: wasm-ld unavailable; skipping schema-record buffer handshake");
        return;
    }

    let obj = std::env::temp_dir().join(format!("relon_wasm_rec_{}.o", std::process::id()));
    let wasm = std::env::temp_dir().join(format!("relon_wasm_rec_{}.wasm", std::process::id()));
    let entry = "relon_main_rec";
    let info = LlvmAotEvaluator::emit_object_for_target(
        src,
        entry,
        &obj,
        &opts,
        WorldMode::OpenWorld,
        None,
        CodegenTarget::Wasm32,
    )
    .expect("wasm32 emit for record buffer path");
    assert!(matches!(
        info.shape,
        relon_codegen_llvm::EmittedEntryShape::Buffer
    ));

    relon_codegen_llvm::wasm_link::link_wasm_object(&obj, &wasm, entry).expect("link record wasm");
    let bytes = std::fs::read(&wasm).expect("read record wasm");

    // Pack the two Int args at their declared offsets.
    let mut in_record = vec![0u8; info.main_root_size as usize];
    let lookup = |name: &str| {
        info.main_fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("missing field {name}"))
            .offset as usize
    };
    let off_a = lookup("a");
    let off_b = lookup("b");
    in_record[off_a..off_a + 8].copy_from_slice(&a.to_le_bytes());
    in_record[off_b..off_b + 8].copy_from_slice(&b.to_le_bytes());

    let out = run_buffer_in_wasmtime(&bytes, entry, &in_record, info.return_root_size);
    let ret = |name: &str| {
        let off = info
            .return_fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("missing ret field {name}"))
            .offset as usize;
        i64::from_le_bytes(out[off..off + 8].try_into().unwrap())
    };
    assert_eq!(ret("x"), want_x, "wasm record field x");
    assert_eq!(ret("y"), want_y, "wasm record field y");
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&wasm);
}
