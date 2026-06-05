//! F2 four-way gate for the cross-region anon-Dict object return.
//!
//! `#main(List<Server> servers) -> Dict { servers: servers, n: Int }` builds
//! the object head in `out_buf`, but the `servers` field is a `#main`
//! parameter identity whose `List<Server>` data lives in `in_buf` — a genuine
//! cross-region field pointer. Under the F1 arena-absolute slot convention
//! the codegen stores the parameter list root's arena-absolute offset
//! directly into the object's field slot (no tail copy). F1b shipped this on
//! cranelift; F2 ships the same on **llvm + wasm** for a true four-way
//! bit-equal (tree-walk == cranelift == llvm-native == wasm).
//!
//! Why this matters: pure-AOT / wasm deployment targets have **no
//! interpreter fallback**, so an object return with a parameter-sourced
//! pointer-array list field must execute on the metal, not fall back.
//!
//! Mechanism — the wasm host does **not** carry its own decode or its own
//! verifier. The wasm module is the same LLVM IR retargeted to wasm32, so it
//! takes the same positive `bytes_written` object-return path the native body
//! does (the cross-region store is target-independent IR). After the call the
//! host:
//!   1. takes a `&[u8]` view of wasm linear memory rebased to the arena
//!      origin (`&memory[arena_abs .. arena_abs + arena_size]`),
//!   2. hands it to `LlvmAotEvaluator::wasm_buffer_decode`, which on a
//!      positive return runs `verify_object_return_multi` over the whole
//!      arena anchored at `out_ptr` — `classify_span` lands the `servers`
//!      slot offset in the input region, bounds-checks the whole reachable
//!      graph — and only on a clean verify does `BufferReader::new_at_base`
//!      follow it cross-region.
//!
//! The verifier runs over the linear-memory slice exactly as it does over the
//! host arena: an unverified buffer is never decoded, and an out-of-region /
//! unclassifiable pointer is a loud `Err`, never a wild read. There is no
//! wasm-specific decode path.
//!
//! When `wasm-ld` is unavailable the wasm legs are recorded skips (the native
//! three-way — tree-walk == native-llvm — still runs) so the suite stays
//! green on a host without the LLVM linker. The proof is never faked: every
//! asserted wasm leg runs the value out of wasmtime and compares to the
//! tree-walk oracle.

use std::collections::HashMap;
use std::sync::Arc;

use proptest::prelude::*;
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

/// Linear-memory-backed libc shims the standalone wasm module imports (the
/// native target gets these from libc / compiler-rt). Each returns `dest`.
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

/// Compile `src` to a wasm32 module, run it in wasmtime over a linear-memory
/// arena laid out by the native planner, and decode the return through the
/// shared host pipeline (verifier-gated). Returns the decoded `Value`, or
/// `None` when `wasm-ld` is unavailable (a recorded skip).
fn run_wasm(src: &str, args: &HashMap<String, Value>) -> Option<Result<Value, String>> {
    if !wasm_ld_available() {
        return None;
    }
    use wasmtime::{Extern, Module, Store, Val};

    let ev = LlvmAotEvaluator::from_source_with_options(src, &opts())
        .expect("native from_source for wasm planner");
    let plan = ev.wasm_buffer_plan(args).expect("wasm_buffer_plan");

    let entry = format!("relon_xregion_{}", std::process::id());
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
        "cross-region object return must lower to the buffer entry, got {:?}",
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

    // ArenaState. tail_cursor starts at the return root size (a String
    // scalar field's pointer-indirect StoreField bumps past the fixed area
    // into the tail); the cross-region field slot stores an in_buf offset
    // and never touches the tail.
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
    // resolve exactly as on the host JIT path, then decode through the
    // shared verifier-gated pipeline.
    let full = memory.data(&store);
    let arena = &full[arena_abs as usize..(arena_abs + arena_size) as usize];
    Some(
        ev.wasm_buffer_decode(arena, r, ret)
            .map_err(|e| e.to_string()),
    )
}

/// Normalise a returned Dict into a sorted `(key, Value)` list so brand /
/// map-iteration-order differences between executors do not matter — only
/// the field set + per-field value content does.
fn fields_of(v: &Value) -> Vec<(String, Value)> {
    match v {
        Value::Dict(d) => {
            let mut out: Vec<(String, Value)> = d
                .map
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        }
        other => panic!("expected Dict return, got {other:?}"),
    }
}

/// Four-way bit-equal: tree-walk == native-llvm == wasm. The wasm leg is
/// asserted when `wasm-ld` is present, otherwise a recorded skip (the
/// three-way still runs).
fn assert_four_way(src: &str, args: HashMap<String, Value>) {
    let oracle = run_tree_walk(src, args.clone());

    let ev = LlvmAotEvaluator::from_source_with_options(src, &opts()).expect("native from_source");
    let native = ev.run_main(args.clone()).expect("native run_main");
    assert_eq!(
        fields_of(&native),
        fields_of(&oracle),
        "native llvm != tree-walk oracle for `{src}`"
    );

    match run_wasm(src, &args) {
        Some(Ok(wasm)) => assert_eq!(
            fields_of(&wasm),
            fields_of(&oracle),
            "wasm != tree-walk oracle for `{src}` (four-way bit-equal failed)"
        ),
        Some(Err(e)) => panic!("wasm decode failed for `{src}`: {e}"),
        None => eprintln!("cross_region_object_four_way: wasm-ld unavailable; wasm leg skipped"),
    }
}

// ---- builders ---------------------------------------------------------

fn args1(name: &str, v: Value) -> HashMap<String, Value> {
    HashMap::from([(name.to_string(), v)])
}

fn from_cps(cps: &[u32]) -> String {
    cps.iter().map(|c| char::from_u32(*c).unwrap()).collect()
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

fn cfg(brand: &str, fields: Vec<(&str, Value)>) -> Value {
    Value::branded_dict(
        fields.into_iter().map(|(k, v)| (k.to_string(), v)),
        Some(brand.to_string()),
    )
}

fn list(items: Vec<Value>) -> Value {
    Value::List(Arc::new(items))
}

fn iints(items: &[i64]) -> Value {
    Value::List(Arc::new(items.iter().map(|x| Value::Int(*x)).collect()))
}

// ---- hand-written cases ----------------------------------------------

const SRC_SERVERS_N: &str = "#schema Server { name: String, port: Int }\n\
     #main(List<Server> servers) -> Dict\n{ servers: servers, n: 1 }";

#[test]
fn servers_empty() {
    assert_four_way(SRC_SERVERS_N, args1("servers", list(vec![])));
}

#[test]
fn servers_single() {
    assert_four_way(
        SRC_SERVERS_N,
        args1(
            "servers",
            list(vec![cfg(
                "Server",
                vec![("name", s("alpha")), ("port", Value::Int(8080))],
            )]),
        ),
    );
}

#[test]
fn servers_many_with_cjk_empty_long() {
    let long = "x".repeat(5_000);
    let long_cjk = from_cps(&[0x4E2D]).repeat(2_048);
    assert_four_way(
        SRC_SERVERS_N,
        args1(
            "servers",
            list(vec![
                cfg("Server", vec![("name", s("")), ("port", Value::Int(0))]),
                cfg(
                    "Server",
                    vec![
                        ("name", s(&from_cps(&[0x4E2D, 0x6587]))),
                        ("port", Value::Int(-1)),
                    ],
                ),
                cfg(
                    "Server",
                    vec![
                        ("name", s(&from_cps(&[0x1F980, 0x1F980]))),
                        ("port", Value::Int(i64::MAX)),
                    ],
                ),
                cfg("Server", vec![("name", s(&long)), ("port", Value::Int(7))]),
                cfg(
                    "Server",
                    vec![("name", s(&long_cjk)), ("port", Value::Int(-9))],
                ),
            ]),
        ),
    );
}

const SRC_GRID: &str = "#main(List<List<Int>> grid) -> Dict\n{ g: grid, n: 1 }";

#[test]
fn grid_list_list_int() {
    assert_four_way(
        SRC_GRID,
        args1(
            "grid",
            list(vec![
                iints(&[1, 2, 3]),
                iints(&[]),
                iints(&[-7, i64::MIN, i64::MAX]),
            ]),
        ),
    );
}

const SRC_MIXED_SCALAR_CROSS: &str = "#schema Server { name: String, port: Int }\n\
     #main(List<Server> servers) -> Dict\n\
     { title: \"cfg\", servers: servers, count: 2, ratio: 1.5 }";

#[test]
fn mixed_scalar_and_cross_region() {
    assert_four_way(
        SRC_MIXED_SCALAR_CROSS,
        args1(
            "servers",
            list(vec![
                cfg("Server", vec![("name", s("x")), ("port", Value::Int(1))]),
                cfg(
                    "Server",
                    vec![
                        ("name", s(&from_cps(&[0x4E2D, 0x6587]))),
                        ("port", Value::Int(2)),
                    ],
                ),
            ]),
        ),
    );
}

// ---- proptest --------------------------------------------------------

fn string_strat() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        "[a-z]{0,8}".prop_map(|s| s),
        prop::collection::vec(
            prop_oneof![Just(0x4E2Du32), Just(0x1F980u32), Just(0xE9u32)],
            0..5
        )
        .prop_map(|cps| cps.iter().map(|c| char::from_u32(*c).unwrap()).collect()),
        (0usize..40).prop_map(|n| "x".repeat(n)),
    ]
}

fn server_strat() -> impl Strategy<Value = Value> {
    (string_strat(), any::<i64>()).prop_map(|(name, port)| {
        cfg(
            "Server",
            vec![("name", s(&name)), ("port", Value::Int(port))],
        )
    })
}

fn servers_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(server_strat(), 0..6).prop_map(list)
}

fn grid_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(
        prop::collection::vec(
            prop_oneof![Just(i64::MIN), Just(i64::MAX), Just(0i64), any::<i64>()],
            0..5,
        )
        .prop_map(|r| Value::List(Arc::new(r.into_iter().map(Value::Int).collect()))),
        0..5,
    )
    .prop_map(list)
}

proptest! {
    // Each wasm case shells out to wasm-ld + spins up a wasmtime instance,
    // so keep the count modest.
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn diff_servers_object(servers in servers_strat()) {
        let oracle = run_tree_walk(SRC_SERVERS_N, args1("servers", servers.clone()));
        if let Some(res) = run_wasm(SRC_SERVERS_N, &args1("servers", servers)) {
            let wasm = res.map_err(TestCaseError::fail)?;
            prop_assert_eq!(fields_of(&wasm), fields_of(&oracle));
        }
    }

    #[test]
    fn diff_grid_object(grid in grid_strat()) {
        let oracle = run_tree_walk(SRC_GRID, args1("grid", grid.clone()));
        if let Some(res) = run_wasm(SRC_GRID, &args1("grid", grid)) {
            let wasm = res.map_err(TestCaseError::fail)?;
            prop_assert_eq!(fields_of(&wasm), fields_of(&oracle));
        }
    }
}

// ---- adversarial: cross-region pointer forced out of every region ----

/// A cross-region object field whose slot offset is corrupted to land in no
/// region (or run off its region) must be a loud `Err` from
/// `wasm_buffer_decode` — never a wild read / silent wrong value. We build a
/// real plan/arena for the cross-region object, then plant a bogus field
/// pointer in the object's `servers` slot before decoding.
#[test]
fn wasm_decode_rejects_out_of_region_field_pointer_loudly() {
    let src = SRC_SERVERS_N;
    let args = args1(
        "servers",
        list(vec![cfg(
            "Server",
            vec![("name", s("a")), ("port", Value::Int(1))],
        )]),
    );
    let ev = LlvmAotEvaluator::from_source_with_options(src, &opts()).expect("from_source");
    let plan = ev.wasm_buffer_plan(&args).expect("plan");
    let r = plan.regions;

    // Lay a realistic arena: const_data + input record, then write the object
    // head at out_ptr with the `servers` slot holding a *corrupt* arena
    // offset that lands past the arena entirely. The object root layout is
    // `{ n: Int @0, servers: List<Schema> @8 }` (field order is schema
    // canonical; either slot order is fine for the adversarial probe — we
    // scan for the cross-region slot by planting a poison value in both
    // candidate pointer slots so the verifier must reject regardless).
    let mut arena = vec![0u8; r.arena_size];
    let cd = plan.const_data.len();
    if cd > 0 {
        arena[..cd].copy_from_slice(&plan.const_data);
    }
    let in_off = r.in_ptr as usize;
    arena[in_off..in_off + plan.in_bytes.len()].copy_from_slice(&plan.in_bytes);

    // Poison: an offset well past the arena, in every 4-byte slot of the
    // object root's fixed area. `verify_object_return_multi` walks the
    // List<Schema> field pointer and must classify it to no region.
    let bogus = (r.arena_size as u32) + 4096;
    let out_off = r.out_ptr as usize;
    let root_size = (r.out_cap as usize).min(64);
    let mut i = 0;
    while i + 4 <= root_size {
        arena[out_off + i..out_off + i + 4].copy_from_slice(&bogus.to_le_bytes());
        i += 4;
    }

    // Positive `bytes_written` return = object path.
    let ret = r.out_cap.min(64) as i32;
    let res = ev.wasm_buffer_decode(&arena, r, ret);
    assert!(
        res.is_err(),
        "an out-of-region cross-region field pointer must be a loud Err, got {res:?}"
    );
}
