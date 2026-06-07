//! Wave R14 four-way gate for the Unicode case-fold / normalization seam.
//!
//! The bundled stdlib bodies for `upper` / `lower` / `title` / `nfd`
//! decode each input codepoint from UTF-8, look it up in a shared
//! `relon-unicode` table (simple + full case folding, combining-mark and
//! whitespace ranges, NFD decomposition), and re-encode the result. They
//! lowered + ran on the tree-walk evaluator and the cranelift-AOT backend
//! since v3++, but on the LLVM backend they SEGFAULTed (native) and
//! STACK-/arena-OVERFLOWed (wasm): the LLVM `*TableAddr` lowering copied
//! the whole table into arena scratch **at the op site**, and because the
//! op lives inside the per-codepoint decode loop (via the inlined
//! `__casefold_lookup` / `__is_combining_mark` helpers), it re-copied the
//! table every iteration until the scratch cursor overran the arena.
//!
//! R14 moves the tables into the const-data prefix — the exact mechanism
//! cranelift uses — so a `*TableAddr` is a single compile-time-constant
//! offset push with no runtime cost. Removing that overrun then exposed a
//! second latent bug: the LLVM `emit_if` did not push a structured-control
//! label frame, so a `Br { depth }` that breaks out of the search loop
//! from inside an `If` resolved one level too shallow and jumped into the
//! caller's per-codepoint loop (an infinite loop). `emit_if` now pushes
//! the frame, matching cranelift's wasm structured-control-flow model.
//!
//! This file asserts a true four-way bit-equal — tree-walk == cranelift ==
//! llvm-native == llvm-wasm — over a multibyte battery (ASCII, accented
//! Latin, NBSP / em-space, CJK, combining marks, empty, mixed) plus a
//! large input that previously overran the arena. The wasm leg runs the
//! value out of wasmtime through the shared verifier-gated decode; when
//! `wasm-ld` is unavailable it is a recorded skip and the three-way
//! (tree-walk == cranelift == native) still runs. No expected values are
//! hand-written: tree-walk is the oracle.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::{CodegenTarget, EmittedEntryShape, LlvmAotEvaluator, WorldMode};
use relon_eval_api::{Evaluator, Value};

fn opts() -> relon_analyzer::AnalyzeOptions {
    relon_analyzer::AnalyzeOptions {
        strict_mode: false,
        ..Default::default()
    }
}

fn wasm_ld_available() -> bool {
    relon_codegen_llvm::wasm_link::find_wasm_ld().is_some()
}

fn args1(input: &str) -> HashMap<String, Value> {
    HashMap::from([("s".to_string(), Value::String(input.into()))])
}

// ---- oracle: tree-walk -----------------------------------------------

fn run_tree_walk(src: &str, args: HashMap<String, Value>) -> Value {
    use relon_evaluator::{Context, TreeWalkEvaluator};
    use relon_parser::parse_document;
    let node = parse_document(src).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    let ctx = Arc::new({
        let mut ctx = ctx;
        TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    });
    TreeWalkEvaluator::new(Arc::clone(&ctx))
        .run_main(&Arc::new(relon_eval_api::scope::Scope::default()), args)
        .expect("tree-walk run_main")
}

// ---- wasm linker imports (memcpy/memmove/memset/multi3) --------------

fn linker_with_libc(engine: &wasmtime::Engine) -> wasmtime::Linker<()> {
    use wasmtime::{Caller, Extern, Linker};
    let mut linker = Linker::new(engine);
    let mem = |caller: &mut Caller<'_, ()>| match caller.get_export("memory") {
        Some(Extern::Memory(m)) => m,
        _ => panic!("libc shim needs an exported `memory`"),
    };
    linker
        .func_wrap(
            "env",
            "__multi3",
            |mut caller: Caller<'_, ()>, ret: i32, a_lo: i64, a_hi: i64, b_lo: i64, b_hi: i64| {
                let a = (((a_hi as u64 as u128) << 64) | (a_lo as u64 as u128)) as i128;
                let b = (((b_hi as u64 as u128) << 64) | (b_lo as u64 as u128)) as i128;
                let prod = a.wrapping_mul(b) as u128;
                let m = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => panic!("__multi3 needs memory"),
                };
                m.write(&mut caller, ret as usize, &prod.to_le_bytes())
                    .expect("__multi3 store");
            },
        )
        .expect("multi3");
    for name in ["memcpy", "memmove"] {
        linker
            .func_wrap(
                "env",
                name,
                move |mut caller: Caller<'_, ()>, dest: i32, src: i32, n: i32| -> i32 {
                    let m = mem(&mut caller);
                    let n = n as usize;
                    let mut tmp = vec![0u8; n];
                    m.read(&caller, src as usize, &mut tmp).expect("copy read");
                    m.write(&mut caller, dest as usize, &tmp)
                        .expect("copy write");
                    dest
                },
            )
            .unwrap_or_else(|_| panic!("register {name}"));
    }
    linker
        .func_wrap(
            "env",
            "memset",
            move |mut caller: Caller<'_, ()>, dest: i32, c: i32, n: i32| -> i32 {
                let m = mem(&mut caller);
                let fill = vec![c as u8; n as usize];
                m.write(&mut caller, dest as usize, &fill).expect("memset");
                dest
            },
        )
        .expect("memset");
    linker
}

// ---- wasm leg --------------------------------------------------------

/// Compile `src` to wasm32, run it in wasmtime over a native-planned
/// linear-memory arena, and decode the return through the shared
/// verifier-gated pipeline. `None` when `wasm-ld` is unavailable.
fn run_wasm(src: &str, args: &HashMap<String, Value>) -> Option<Result<Value, String>> {
    if !wasm_ld_available() {
        return None;
    }
    use wasmtime::{Extern, Module, Store, Val};

    let ev = LlvmAotEvaluator::from_source_with_options(src, &opts())
        .expect("native from_source for wasm planner");
    let plan = ev.wasm_buffer_plan(args).expect("wasm_buffer_plan");

    let entry = format!("relon_uni_{}", std::process::id());
    let dir = std::env::temp_dir();
    let obj = dir.join(format!("{entry}_{:p}.o", &plan));
    let wasm = dir.join(format!("{entry}_{:p}.wasm", &plan));
    let info = LlvmAotEvaluator::emit_object_for_target(
        src,
        &entry,
        &obj,
        &opts(),
        WorldMode::OpenWorld,
        None,
        CodegenTarget::Wasm32,
    )
    .expect("wasm32 emit");
    assert!(
        matches!(info.shape, EmittedEntryShape::Buffer),
        "string return must lower to the buffer entry, got {:?}",
        info.shape
    );
    relon_codegen_llvm::wasm_link::link_wasm_object(&obj, &wasm, &entry).expect("link wasm");
    let bytes = std::fs::read(&wasm).expect("read wasm");
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&wasm);

    const STATE_OFF_ARENA_BASE: usize = 0;
    const STATE_OFF_TAIL_CURSOR: usize = 12;
    const STATE_OFF_SCRATCH_BASE: usize = 20;
    const STATE_SIZE: usize = 40;

    let engine = wasmtime::Engine::default();
    let module = Module::new(&engine, &bytes).expect("Module::new");
    let mut store = Store::new(&engine, ());
    let linker = linker_with_libc(&engine);
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate");
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
    let r = plan.regions;
    let arena_size = r.arena_size as u32;

    let needed = (arena_off + arena_size) as usize;
    let cur = memory.data_size(&store);
    if needed > cur {
        let extra_pages = (needed - cur).div_ceil(65536) as u64;
        memory.grow(&mut store, extra_pages).expect("grow memory");
    }
    let arena_abs = arena_off;

    let mut state = [0u8; STATE_SIZE];
    state[STATE_OFF_ARENA_BASE..STATE_OFF_ARENA_BASE + 8]
        .copy_from_slice(&(arena_abs as u64).to_le_bytes());
    let return_root_size = info.return_root_size.max(8);
    state[STATE_OFF_TAIL_CURSOR..STATE_OFF_TAIL_CURSOR + 4]
        .copy_from_slice(&return_root_size.to_le_bytes());
    state[STATE_OFF_SCRATCH_BASE..STATE_OFF_SCRATCH_BASE + 4]
        .copy_from_slice(&r.scratch_base.to_le_bytes());
    memory
        .write(&mut store, state_ptr as usize, &state)
        .expect("write state");

    if !plan.const_data.is_empty() {
        memory
            .write(&mut store, arena_abs as usize, &plan.const_data)
            .expect("write const_data");
    }
    memory
        .write(&mut store, (arena_abs + r.in_ptr) as usize, &plan.in_bytes)
        .expect("write in record");

    let func = instance
        .get_func(&mut store, &entry)
        .unwrap_or_else(|| panic!("export `{entry}` missing"));
    let params = [
        Val::I32(state_ptr as i32),
        Val::I32(r.in_ptr as i32),
        Val::I32(r.in_len as i32),
        Val::I32(r.out_ptr as i32),
        Val::I32(r.out_cap as i32),
        Val::I64(0),
    ];
    let mut results = [Val::I32(0)];
    func.call(&mut store, &params, &mut results)
        .expect("buffer entry call");
    let ret = match results[0] {
        Val::I32(v) => v,
        other => panic!("expected i32 return, got {other:?}"),
    };

    let full = memory.data(&store);
    let arena = &full[arena_abs as usize..(arena_abs + arena_size) as usize];
    Some(
        ev.wasm_buffer_decode(arena, r, ret)
            .map_err(|e| e.to_string()),
    )
}

// ---- four-way driver -------------------------------------------------

/// tree-walk (oracle) == cranelift == llvm-native == llvm-wasm.
fn assert_four_way(src: &str, input: &str) {
    let oracle = run_tree_walk(src, args1(input));

    let cl = AotEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("cranelift from_source `{src}` / {input:?}: {e:?}"));
    let cl_val = cl
        .run_main(args1(input))
        .unwrap_or_else(|e| panic!("cranelift run `{src}` / {input:?}: {e:?}"));
    assert_eq!(
        cl_val, oracle,
        "cranelift != tree-walk for `{src}` / {input:?}"
    );

    let na = LlvmAotEvaluator::from_source_with_options(src, &opts())
        .unwrap_or_else(|e| panic!("native from_source `{src}` / {input:?}: {e:?}"));
    let na_val = na
        .run_main(args1(input))
        .unwrap_or_else(|e| panic!("native run `{src}` / {input:?}: {e:?}"));
    assert_eq!(
        na_val, oracle,
        "llvm-native != tree-walk for `{src}` / {input:?}"
    );

    match run_wasm(src, &args1(input)) {
        Some(Ok(w)) => assert_eq!(
            w, oracle,
            "llvm-wasm != tree-walk for `{src}` / {input:?} (four-way bit-equal failed)"
        ),
        Some(Err(e)) => panic!("wasm decode failed for `{src}` / {input:?}: {e}"),
        None => eprintln!("unicode_four_way: wasm-ld unavailable; wasm leg skipped for `{src}`"),
    }
}

/// The multibyte battery every op runs against. Codepoints are written as
/// `\u{..}` escapes so the source file stays ASCII-only.
fn battery() -> Vec<&'static str> {
    vec![
        "",                          // empty
        "hello",                     // ASCII fast path
        "HELLO",                     // already-upper ASCII
        "Hello World",               // mixed-case, spaces (title boundaries)
        "caf\u{00E9}",               // accented Latin (precomposed e-acute)
        "H\u{00E9}llo",              // accent mid-word
        "STRASSE \u{00DF}",          // German eszett (multi-cp upper stays simple-folded)
        "\u{00A0}x\u{2003}y",        // NBSP + em-space (whitespace ranges)
        "  spaced  out  ",           // ASCII whitespace, leading/trailing
        "\u{65E5}\u{672C}\u{8A9E}",  // CJK (no case, no decomposition)
        "e\u{0301}",                 // base + combining acute (already-decomposed)
        "a\u{0301}e\u{0300}i",       // multiple combining marks
        "Mixed \u{00C9}\u{65E5} 12", // Latin + accent + CJK + digits
    ]
}

const UPPER: &str = "#main(String s) -> String\ns.upper()";
const LOWER: &str = "#main(String s) -> String\ns.lower()";
const TITLE: &str = "#main(String s) -> String\ns.title()";
const NFD: &str = "#main(String s) -> String\ns.nfd()";

#[test]
fn upper_four_way_battery() {
    for inp in battery() {
        assert_four_way(UPPER, inp);
    }
}

#[test]
fn lower_four_way_battery() {
    for inp in battery() {
        assert_four_way(LOWER, inp);
    }
}

#[test]
fn title_four_way_battery() {
    for inp in battery() {
        assert_four_way(TITLE, inp);
    }
}

#[test]
fn nfd_four_way_battery() {
    for inp in battery() {
        assert_four_way(NFD, inp);
    }
}

/// Large input: the prior LLVM/wasm failure mode was an arena overrun
/// within a *few* codepoints (the per-op scratch-table copy that R14
/// removed). A string of hundreds of codepoints drives that many
/// decode-loop iterations and must complete (no overflow / no infinite
/// loop) and stay bit-equal across tree-walk == llvm-native == llvm-wasm.
///
/// Cranelift is intentionally excluded here: a large String return trips
/// cranelift's fixed cross-region-return scratch window (a separate,
/// pre-existing arena-sizing limit, orthogonal to the R14 table-placement
/// fix), so its small-input four-way coverage stays in the batteries
/// above while this probe targets exactly the LLVM/wasm legs that the
/// overrun used to crash.
#[test]
fn large_input_completes_native_and_wasm() {
    let unit = "Hello \u{00E9}\u{65E5}a\u{0301} World \u{2003}";
    let big: String = unit.repeat(200);
    for src in [UPPER, LOWER, TITLE, NFD] {
        let oracle = run_tree_walk(src, args1(&big));

        let na = LlvmAotEvaluator::from_source_with_options(src, &opts())
            .unwrap_or_else(|e| panic!("native from_source `{src}`: {e:?}"));
        let na_val = na
            .run_main(args1(&big))
            .unwrap_or_else(|e| panic!("native run `{src}` (large): {e:?}"));
        assert_eq!(
            na_val, oracle,
            "llvm-native != tree-walk for `{src}` (large)"
        );

        match run_wasm(src, &args1(&big)) {
            Some(Ok(w)) => assert_eq!(w, oracle, "llvm-wasm != tree-walk for `{src}` (large)"),
            Some(Err(e)) => panic!("wasm decode failed for `{src}` (large): {e}"),
            None => eprintln!("unicode_four_way: wasm-ld unavailable; large wasm leg skipped"),
        }
    }
}
