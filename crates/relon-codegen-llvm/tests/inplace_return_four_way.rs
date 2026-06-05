//! S6 four-way gate for the in-place region-walk return ABI.
//!
//! S1–S4 proved the parameter-identity pointer-array list returns
//! (`List<List<scalar>>`, `List<String>`, `List<Schema>`) bit-equal across
//! tree-walk (the golden oracle), cranelift-AOT, and llvm-AOT-native. This
//! file carries the **same three shapes onto wasm32** and asserts a true
//! four-way bit-equal: tree-walk == llvm-native == wasm, end-to-end through
//! a real `LLVM→wasm32 object → wasm-ld → wasmtime` pipeline.
//!
//! Why this matters: pure-AOT / wasm deployment targets have **no
//! interpreter fallback**. For those targets these shapes must execute on
//! the metal, not fall back to tree-walk. The motivation for the whole
//! return-side line lands here.
//!
//! Mechanism — the wasm host does **not** carry its own decode or its own
//! verifier. The wasm module is the same LLVM IR retargeted to wasm32, so
//! it emits the **same** negative in-place sentinel `-(root_abs + 1)` the
//! native body does (the sentinel is built purely at the IR level in
//! `codegen/control.rs`, target-independent). After the call the host:
//!   1. takes a `&[u8]` view of wasm linear memory rebased to the arena
//!      origin (`&memory[arena_abs .. arena_abs + arena_size]`),
//!   2. hands it to `LlvmAotEvaluator::wasm_buffer_decode`, which routes
//!      through the **same** `decode_buffer_return` the host JIT path uses,
//!      which on a negative sentinel runs the backend-shared
//!      `relon_eval_api::inplace_return` pipeline:
//!      region-select → **verifier** → in-place decode.
//!
//! The verifier runs over the linear-memory slice exactly as it does over
//! the host arena — an unverified buffer is never decoded, and an
//! out-of-region pointer is a loud `Err`, never a wild read.
//!
//! Three layers per shape: hand-written edge cases (empty / single / many,
//! empty string / CJK / very long, mixed fields), a proptest differential,
//! and verifier adversarial probes on the linear-memory slice. Plus the
//! still-capped-shape guard: a shape the in-place ABI does not lift must
//! make the wasm32 emit decline loudly, never silently miscompile.
//!
//! When `wasm-ld` is unavailable the wasm legs are recorded skips (the
//! native three-way still runs) so the suite stays green on a host without
//! the LLVM linker. The proof is never faked: every asserted wasm leg runs
//! the value out of wasmtime and compares to the tree-walk oracle.

use std::collections::HashMap;
use std::sync::Arc;

use ordered_float::OrderedFloat;
use proptest::prelude::*;
use relon_codegen_llvm::{
    ArenaRegions, CodegenTarget, EmittedEntryShape, LlvmAotEvaluator, WorldMode,
};
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

// ---- oracle: tree-walk -----------------------------------------------

/// Tree-walk golden-oracle evaluation. This is the bit-exact yardstick;
/// no expected value is hand-written, the oracle defines truth.
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

/// Linear-memory-backed libc shims the standalone wasm module imports
/// (the native target gets these from libc / compiler-rt). The String /
/// List tail copies lower to `memcpy` / `memmove`; the arena zeroing the
/// host does up front is not present in the module, so the wasm body that
/// bumps the tail relies on these. Each returns `dest` (libc contract).
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

// ---- wasm four-way driver --------------------------------------------

/// Compile `src` to a wasm32 module, run it in wasmtime over a linear-
/// memory arena laid out by the native planner, and decode the return
/// through the shared host pipeline (verifier-gated). Returns the decoded
/// `Value`, or `None` when `wasm-ld` is unavailable (a recorded skip).
fn run_wasm(src: &str, args: &HashMap<String, Value>) -> Option<Result<Value, String>> {
    if !wasm_ld_available() {
        return None;
    }
    use wasmtime::{Extern, Module, Store, Val};

    // The native evaluator is both the layout planner (it packs the input
    // and computes the arena layout the wasm body was emitted against) and
    // the decode owner (`wasm_buffer_decode`).
    let ev = LlvmAotEvaluator::from_source_with_options(src, &opts())
        .expect("native from_source for wasm planner");
    let plan = ev.wasm_buffer_plan(args).expect("wasm_buffer_plan");

    let entry = format!("relon_inplace_{}", std::process::id());
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
        "in-place return must lower to the buffer entry, got {:?}",
        info.shape
    );
    relon_codegen_llvm::wasm_link::link_wasm_object(&obj, &wasm, &entry).expect("link wasm");
    let bytes = std::fs::read(&wasm).expect("read wasm");
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&wasm);

    // ArenaState layout mirrors the native `#[repr(C)] ArenaState`:
    // arena_base i64 @0, tail_cursor u32 @12, scratch_base u32 @20.
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

    // ArenaState. tail_cursor starts at the return root size (pointer-
    // indirect StoreField bumps past the fixed area into the tail),
    // matching the native `ArenaState::new` convention.
    let mut state = [0u8; STATE_SIZE];
    state[STATE_OFF_ARENA_BASE..STATE_OFF_ARENA_BASE + 8]
        .copy_from_slice(&(arena_abs as u64).to_le_bytes());
    // tail cursor begins at the return root size offset inside out_buf.
    let return_root_size = info.return_root_size.max(8);
    state[STATE_OFF_TAIL_CURSOR..STATE_OFF_TAIL_CURSOR + 4]
        .copy_from_slice(&return_root_size.to_le_bytes());
    state[STATE_OFF_SCRATCH_BASE..STATE_OFF_SCRATCH_BASE + 4]
        .copy_from_slice(&r.scratch_base.to_le_bytes());
    memory
        .write(&mut store, state_ptr as usize, &state)
        .expect("write state");

    // const_data at arena offset 0, input record at in_ptr.
    if !plan.const_data.is_empty() {
        memory
            .write(&mut store, arena_abs as usize, &plan.const_data)
            .expect("write const_data");
    }
    memory
        .write(&mut store, (arena_abs + r.in_ptr) as usize, &plan.in_bytes)
        .expect("write in record");

    // Invoke the buffer entry.
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

    // Slice linear memory to the arena origin so arena-relative offsets
    // and the arena-relative sentinel root resolve exactly as on the host
    // JIT path, then decode through the shared verifier-gated pipeline.
    let full = memory.data(&store);
    let arena = &full[arena_abs as usize..(arena_abs + arena_size) as usize];
    Some(
        ev.wasm_buffer_decode(arena, r, ret)
            .map_err(|e| e.to_string()),
    )
}

/// Four-way bit-equal: tree-walk == native llvm == wasm. The wasm leg is
/// asserted when `wasm-ld` is present, otherwise it is a recorded skip and
/// the three-way (tree-walk == native) still runs.
fn assert_four_way(src: &str, args: HashMap<String, Value>) {
    let oracle = run_tree_walk(src, args.clone());

    let ev = LlvmAotEvaluator::from_source_with_options(src, &opts()).expect("native from_source");
    let native = ev.run_main(args.clone()).expect("native run_main");
    assert_eq!(
        native, oracle,
        "native llvm != tree-walk oracle for `{src}`"
    );

    match run_wasm(src, &args) {
        Some(Ok(wasm)) => assert_eq!(
            wasm, oracle,
            "wasm != tree-walk oracle for `{src}` (four-way bit-equal failed)"
        ),
        Some(Err(e)) => panic!("wasm decode failed for `{src}`: {e}"),
        None => eprintln!("inplace_return_four_way: wasm-ld unavailable; wasm leg skipped"),
    }
}

// ---- builders ---------------------------------------------------------

fn args1(name: &str, v: Value) -> HashMap<String, Value> {
    HashMap::from([(name.to_string(), v)])
}

fn int_rows(rows: &[&[i64]]) -> Value {
    Value::List(Arc::new(
        rows.iter()
            .map(|r| Value::List(Arc::new(r.iter().copied().map(Value::Int).collect())))
            .collect(),
    ))
}

fn float_rows(rows: &[&[f64]]) -> Value {
    Value::List(Arc::new(
        rows.iter()
            .map(|r| {
                Value::List(Arc::new(
                    r.iter()
                        .copied()
                        .map(|f| Value::Float(OrderedFloat(f)))
                        .collect(),
                ))
            })
            .collect(),
    ))
}

fn str_list(items: &[&str]) -> Value {
    Value::List(Arc::new(
        items.iter().map(|s| Value::String((*s).into())).collect(),
    ))
}

// ====================================================================
// Shape 1: List<List<scalar>> parameter-identity
// ====================================================================

const SRC_LL_INT: &str = "#main(List<List<Int>> xss) -> List<List<Int>>\nxss";
const SRC_LL_FLOAT: &str = "#main(List<List<Float>> xss) -> List<List<Float>>\nxss";

#[test]
fn ll_int_empty_outer() {
    assert_four_way(SRC_LL_INT, args1("xss", int_rows(&[])));
}

#[test]
fn ll_int_empty_rows() {
    assert_four_way(SRC_LL_INT, args1("xss", int_rows(&[&[], &[]])));
}

#[test]
fn ll_int_single_element() {
    assert_four_way(SRC_LL_INT, args1("xss", int_rows(&[&[42]])));
}

#[test]
fn ll_int_mixed_lengths_with_blank() {
    assert_four_way(
        SRC_LL_INT,
        args1("xss", int_rows(&[&[1, 2, 3], &[], &[4], &[5, 6]])),
    );
}

#[test]
fn ll_int_extreme_values() {
    assert_four_way(
        SRC_LL_INT,
        args1("xss", int_rows(&[&[i64::MIN, i64::MAX, 0, -1], &[1]])),
    );
}

#[test]
fn ll_float_extremes_and_signed_zero() {
    assert_four_way(
        SRC_LL_FLOAT,
        args1(
            "xss",
            float_rows(&[&[0.0, -0.0, f64::MIN, f64::MAX], &[3.5], &[]]),
        ),
    );
}

// ====================================================================
// Shape 2: List<String> parameter-identity
// ====================================================================

const SRC_LS: &str = "#main(List<String> xs) -> List<String>\nxs";

#[test]
fn ls_empty() {
    assert_four_way(SRC_LS, args1("xs", str_list(&[])));
}

#[test]
fn ls_single() {
    assert_four_way(SRC_LS, args1("xs", str_list(&["hello"])));
}

#[test]
fn ls_empty_string_and_cjk() {
    // CJK / emoji via unicode escapes (source stays ASCII): U+4E2D U+6587
    // ("zhongwen"), U+65E5 U+672C U+8A9E ("nihongo"), U+1F980 (crab).
    let cjk_a = "\u{4E2D}\u{6587}";
    let cjk_b = "\u{65E5}\u{672C}\u{8A9E}";
    let emoji = "\u{1F980}x";
    assert_four_way(
        SRC_LS,
        args1("xs", str_list(&["", cjk_a, cjk_b, "", emoji])),
    );
}

#[test]
fn ls_very_long() {
    let long = "a".repeat(4096);
    // U+5B57 ("zi") repeated — a long multi-byte UTF-8 run.
    let cjk_long = "\u{5B57}".repeat(2000);
    assert_four_way(
        SRC_LS,
        args1("xs", str_list(&[long.as_str(), "x", cjk_long.as_str(), ""])),
    );
}

// ====================================================================
// Shape 3: List<Schema> parameter-identity
// ====================================================================

const SRC_LSCHEMA: &str = "#schema Server { String host: *, Int port: * }\n\
                           #main(List<Server> servers) -> List<Server>\nservers";

fn server(host: &str, port: i64) -> Value {
    let map = std::collections::BTreeMap::from([
        (
            relon_eval_api::smol_str::SmolStr::from("host"),
            Value::String(host.into()),
        ),
        (
            relon_eval_api::smol_str::SmolStr::from("port"),
            Value::Int(port),
        ),
    ]);
    Value::branded_dict(map, Some("Server".into()))
}

fn servers(items: Vec<Value>) -> Value {
    Value::List(Arc::new(items))
}

#[test]
fn lschema_empty() {
    assert_four_way(SRC_LSCHEMA, args1("servers", servers(vec![])));
}

#[test]
fn lschema_single() {
    assert_four_way(
        SRC_LSCHEMA,
        args1("servers", servers(vec![server("localhost", 8080)])),
    );
}

#[test]
fn lschema_many_with_cjk_and_empty_host() {
    assert_four_way(
        SRC_LSCHEMA,
        args1(
            "servers",
            servers(vec![
                server("", 0),
                // U+6570 U+636E U+5E93 ("shujuku") — CJK host via escapes.
                server("\u{6570}\u{636E}\u{5E93}", 5432),
                server("a.very.long.hostname.example.internal", i64::MAX),
                server("x", i64::MIN),
            ]),
        ),
    );
}

// ====================================================================
// proptest differential — the "shapes you didn't think of" net.
// Smaller case count than the native three-way: each wasm case shells out
// to wasm-ld + spins up a wasmtime instance.
// ====================================================================

fn int_strat() -> impl Strategy<Value = i64> {
    prop_oneof![Just(i64::MIN), Just(i64::MAX), Just(0i64), any::<i64>()]
}

fn list_list_int_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(prop::collection::vec(int_strat(), 0..4), 0..4).prop_map(|rows| {
        Value::List(Arc::new(
            rows.into_iter()
                .map(|r| Value::List(Arc::new(r.into_iter().map(Value::Int).collect())))
                .collect(),
        ))
    })
}

fn string_strat() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("\u{4E2D}\u{6587}".to_string()), // CJK via escapes
        Just("\u{1F980}".to_string()),        // crab emoji
        "[a-z]{0,32}",
        "\\PC{0,16}", // any non-control codepoints (incl. multi-byte)
    ]
}

fn list_string_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(string_strat(), 0..6).prop_map(|v| {
        Value::List(Arc::new(
            v.into_iter().map(|s| Value::String(s.into())).collect(),
        ))
    })
}

fn list_schema_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec((string_strat(), int_strat()), 0..5).prop_map(|rows| {
        Value::List(Arc::new(
            rows.into_iter()
                .map(|(h, p)| {
                    let map = std::collections::BTreeMap::from([
                        (
                            relon_eval_api::smol_str::SmolStr::from("host"),
                            Value::String(h.into()),
                        ),
                        (
                            relon_eval_api::smol_str::SmolStr::from("port"),
                            Value::Int(p),
                        ),
                    ]);
                    Value::branded_dict(map, Some("Server".into()))
                })
                .collect(),
        ))
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn diff_list_list_int(val in list_list_int_strat()) {
        // tree-walk vs wasm directly (native is covered by the native
        // three-way suite; here the wasm leg is the new ground).
        let oracle = run_tree_walk(SRC_LL_INT, args1("xss", val.clone()));
        if let Some(res) = run_wasm(SRC_LL_INT, &args1("xss", val)) {
            prop_assert_eq!(res.map_err(TestCaseError::fail)?, oracle);
        }
    }

    #[test]
    fn diff_list_string(val in list_string_strat()) {
        let oracle = run_tree_walk(SRC_LS, args1("xs", val.clone()));
        if let Some(res) = run_wasm(SRC_LS, &args1("xs", val)) {
            prop_assert_eq!(res.map_err(TestCaseError::fail)?, oracle);
        }
    }

    #[test]
    fn diff_list_schema(val in list_schema_strat()) {
        let oracle = run_tree_walk(SRC_LSCHEMA, args1("servers", val.clone()));
        if let Some(res) = run_wasm(SRC_LSCHEMA, &args1("servers", val)) {
            prop_assert_eq!(res.map_err(TestCaseError::fail)?, oracle);
        }
    }
}

// ====================================================================
// verifier adversarial — a corrupted sentinel root must be a loud Err on
// the linear-memory slice, never a wild read / silent wrong value.
// ====================================================================

/// Decode a deliberately out-of-range in-place sentinel against a real
/// arena slice: the region-select must reject a root outside every region,
/// and a root pointing at non-list bytes must fail the verifier. Either way
/// the result is `Err`, never a decoded value.
#[test]
fn verifier_rejects_out_of_region_root_loudly() {
    // Build a real plan/arena for a List<String> identity so the regions
    // are realistic, then hand the decode a corrupt sentinel.
    let src = SRC_LS;
    let args = args1("xs", str_list(&["a", "b"]));
    let ev = LlvmAotEvaluator::from_source_with_options(src, &opts()).expect("from_source");
    let plan = ev.wasm_buffer_plan(&args).expect("plan");
    let r: ArenaRegions = plan.regions;
    let arena = vec![0u8; r.arena_size];

    // A sentinel whose decoded root lands past the arena entirely.
    let bogus_root = (r.arena_size as i64) + 4096;
    let sentinel = -(bogus_root) - 1;
    let sentinel = sentinel as i32;
    let res = ev.wasm_buffer_decode(&arena, r, sentinel);
    assert!(
        res.is_err(),
        "out-of-region in-place root must be a loud Err, got {res:?}"
    );

    // A sentinel pointing into the (all-zero) scratch region: the root
    // header reads len=0 but any non-trivial graph would over-read; the
    // verifier must still gate, and a zero-len list decodes to empty — but
    // a root at an unaligned/garbage offset inside the region with a huge
    // length must be rejected. Plant a bogus huge length at scratch_base.
    let mut arena2 = vec![0u8; r.arena_size];
    let sb = r.scratch_base as usize;
    // header: [len: u32][off_0..]; plant len = 0xFFFF_FFFF (over-reads).
    arena2[sb..sb + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let root_in_scratch = r.scratch_base as i64;
    let sentinel2 = (-(root_in_scratch) - 1) as i32;
    let res2 = ev.wasm_buffer_decode(&arena2, r, sentinel2);
    assert!(
        res2.is_err(),
        "in-place root with an over-reading length must be a loud Err, got {res2:?}"
    );
}

// ====================================================================
// loud-cap guard — a shape the in-place ABI does NOT lift must make the
// wasm32 emit decline loudly on the shared IR lowering, never miscompile.
// ====================================================================

/// `List<List<String>>` parameter identity (inner pointer-array element)
/// and a parameter-*field* `List<List<Int>>` are still capped. The wasm32
/// emit path shares the `relon-ir` lowering with the native backends, so
/// the decline is symmetric — assert wasm32 emit returns `Err`.
#[test]
fn capped_shapes_decline_on_wasm_emit_not_silently() {
    let cap_cases = [
        "#main(List<List<String>> xss) -> List<List<String>>\nxss",
        "#schema W { List<List<Int>> rows: * }\n#main(W w) -> List<List<Int>>\nw.rows",
    ];
    let dir = std::env::temp_dir();
    for src in cap_cases {
        let obj = dir.join(format!("relon_cap_{}_{:p}.o", std::process::id(), &src));
        let res = LlvmAotEvaluator::emit_object_for_target(
            src,
            "relon_cap_entry",
            &obj,
            &opts(),
            WorldMode::OpenWorld,
            None,
            CodegenTarget::Wasm32,
        );
        let _ = std::fs::remove_file(&obj);
        assert!(
            res.is_err(),
            "wasm32 emit must decline capped in-place shape `{src}`, but it was accepted — \
             a silent-miscompile path may have opened"
        );
    }
}
