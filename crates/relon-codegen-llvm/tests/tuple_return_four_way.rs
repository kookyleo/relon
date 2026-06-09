//! Wave T2 four-way gate for the `#main(...) -> Tuple<...>` return.
//!
//! A tuple lowers to an **anonymous positional record** (`Schema` with the
//! synthetic field names `"0"`, `"1"`, ... and `is_tuple == true`). It reuses
//! the whole record / return ABI unchanged — `AllocRootRecord` + per-element
//! `StoreFieldAtRecord`, the positive `bytes_written` object-return path, and
//! the multi-region bounds verifier — so the native + wasm legs take exactly
//! the same path a branded-dict return does. The only behavioural fork is the
//! host decode: a tuple schema drains its slots **positionally** into a
//! `Value::Tuple` whose JSON projection is a positional array.
//!
//! This file proves tree-walk == native-llvm == wasm for tuples of scalars
//! (`Int` / `Float` / `Bool` / `String`, heterogeneous), including a String
//! element (pointer-indirect tail record) so the slot-encoding parity is
//! exercised, not just the inline scalars. The cranelift leg of the same
//! shapes is covered by `relon-test-harness` (`assert_all_backends_bit_equal`
//! reaches cranelift) and the manual CLI dual-run; this file owns the wasm +
//! llvm-native halves the harness defers here.
//!
//! When `wasm-ld` is unavailable the wasm legs are recorded skips (the native
//! three-way — tree-walk == native-llvm — still runs). The proof is never
//! faked: every asserted wasm leg runs the value out of wasmtime and compares
//! to the tree-walk oracle.

use std::collections::HashMap;
use std::sync::Arc;

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

    let entry = format!("relon_tuple_{}", std::process::id());
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
        "tuple return must lower to the buffer entry, got {:?}",
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
    // tuple slot's pointer-indirect StoreField bumps past the fixed area into
    // the tail).
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

/// A tuple decodes to a positional `Value::Tuple`. Normalise to the element
/// vector so the four-way compare is element-by-element (a tuple has no field
/// names — order is the identity).
fn elems_of(v: &Value) -> Vec<Value> {
    match v {
        Value::Tuple(items) => items.as_ref().clone(),
        other => panic!("expected Tuple return, got {other:?}"),
    }
}

/// Four-way bit-equal: tree-walk == native-llvm == wasm. The wasm leg is
/// asserted when `wasm-ld` is present, otherwise a recorded skip (the
/// three-way still runs).
fn assert_value_four_way(src: &str, args: HashMap<String, Value>) {
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
        None => eprintln!("tuple_return_four_way: wasm-ld unavailable; wasm leg skipped"),
    }
}

/// Tuple-return specialised wrapper that also asserts the oracle's
/// container shape is positional.
fn assert_four_way(src: &str, args: HashMap<String, Value>) {
    let oracle = run_tree_walk(src, args.clone());
    let oracle_elems = elems_of(&oracle);

    let ev = LlvmAotEvaluator::from_source_with_options(src, &opts()).expect("native from_source");
    let native = ev.run_main(args.clone()).expect("native run_main");
    assert_eq!(
        elems_of(&native),
        oracle_elems,
        "native llvm != tree-walk oracle for `{src}`"
    );

    match run_wasm(src, &args) {
        Some(Ok(wasm)) => assert_eq!(
            elems_of(&wasm),
            oracle_elems,
            "wasm != tree-walk oracle for `{src}` (four-way bit-equal failed)"
        ),
        Some(Err(e)) => panic!("wasm decode failed for `{src}`: {e}"),
        None => eprintln!("tuple_return_four_way: wasm-ld unavailable; wasm leg skipped"),
    }
}

// ---- builders ---------------------------------------------------------

fn args1(name: &str, v: Value) -> HashMap<String, Value> {
    HashMap::from([(name.to_string(), v)])
}

fn args_tuple(name: &str, items: Vec<Value>) -> HashMap<String, Value> {
    HashMap::from([(name.to_string(), Value::Tuple(Arc::new(items)))])
}

// ---- hand-written cases ----------------------------------------------

/// The headline shape from the task brief: `(n, "x", true)`.
#[test]
fn tuple_int_string_bool() {
    assert_four_way(
        "#main(Int n) -> Tuple<Int, String, Bool>\n(n, \"x\", true)",
        args1("n", Value::Int(7)),
    );
}

/// Pure-scalar all-inline tuple, computed elements.
#[test]
fn tuple_int_int_computed() {
    assert_four_way(
        "#main(Int n) -> Tuple<Int, Int>\n(n, n + 1)",
        args1("n", Value::Int(41)),
    );
}

/// Float + Bool + String, mixed slot widths and a pointer-indirect tail.
#[test]
fn tuple_float_bool_string() {
    assert_four_way(
        "#main(Float f) -> Tuple<Float, Bool, String>\n(f, false, \"hi\")",
        args1("f", Value::Float(ordered_float::OrderedFloat(2.5))),
    );
}

/// A two-String tuple — two pointer-indirect tail records in one record.
#[test]
fn tuple_string_string() {
    assert_four_way(
        "#main(Int n) -> Tuple<String, String>\n(\"left\", \"right\")",
        args1("n", Value::Int(0)),
    );
}

/// A 1-tuple of Int with an Int param: this is the shape that could have
/// wrongly taken the typed-i64 fast path (single-Int-field record + Int
/// param). The `is_tuple` guard on `is_single_int_field_record` forces it onto
/// the buffer path so it decodes to `[n]` (a positional list), not a scalar /
/// branded dict.
#[test]
fn tuple_single_int_avoids_fast_path() {
    assert_four_way(
        "#main(Int n) -> Tuple<Int>\n(n,)",
        args1("n", Value::Int(99)),
    );
}

/// A long String element exercises the tail-area copy past the fixed area.
#[test]
fn tuple_int_long_string() {
    let long = "z".repeat(4096);
    assert_four_way(
        &format!("#main(Int n) -> Tuple<Int, String>\n(n, \"{long}\")"),
        args1("n", Value::Int(-3)),
    );
}

/// T3: tuple input + compiled `.N` access, with an arithmetic scalar return.
#[test]
fn tuple_param_index_arith_return() {
    assert_value_four_way(
        "#main(Tuple<Int, Int> pair) -> Int\npair.0 * 10 + pair.1",
        args_tuple("pair", vec![Value::Int(4), Value::Int(2)]),
    );
}

/// T3: tuple input + `.N` access to a pointer-indirect String field.
#[test]
fn tuple_param_string_index_return() {
    assert_value_four_way(
        "#main(Tuple<Int, String> pair) -> String\npair.1",
        args_tuple(
            "pair",
            vec![Value::Int(1), Value::String("from-input".into())],
        ),
    );
}

/// T3: tuple input projected back into a tuple return, proving positional
/// input marshalling, compiled `.N`, and tuple-return array decode together.
#[test]
fn tuple_param_project_tuple_return() {
    assert_four_way(
        "#main(Tuple<Int, String, Bool> pair) -> Tuple<Int, String, Bool>\n(pair.0, pair.1, pair.2)",
        args_tuple(
            "pair",
            vec![
                Value::Int(7),
                Value::String("x".into()),
                Value::Bool(true),
            ],
        ),
    );
}

#[test]
fn tuple_param_rejects_list_payload() {
    let src = "#main(Tuple<Int, String> pair) -> String\npair.1";
    let ev = LlvmAotEvaluator::from_source_with_options(src, &opts()).expect("native from_source");
    let err = ev
        .run_main(HashMap::from([(
            "pair".to_string(),
            Value::list(vec![Value::Int(7), Value::String("x".into())]),
        )]))
        .expect_err("Value::List must not satisfy Tuple<...> host input");
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("got List"),
        "expected tuple arg rejection to report List payload, got {rendered}"
    );
}

/// Named tuple schema parameters use the same positional ABI as
/// `Tuple<...>` parameters while preserving the user-facing schema name.
#[test]
fn named_tuple_schema_param_index_return() {
    assert_value_four_way(
        "#schema IPv4 (Int, Int, Int, Int)\n#main(IPv4 ip) -> Int\nip.0 * 1000000 + ip.1 * 10000 + ip.2 * 100 + ip.3",
        args_tuple(
            "ip",
            vec![Value::Int(127), Value::Int(0), Value::Int(0), Value::Int(1)],
        ),
    );
}

/// Named tuple schema returns decode as JSON arrays, not branded objects.
#[test]
fn named_tuple_schema_return() {
    assert_four_way(
        "#schema Packet (Int, String, Bool)\n#main(Int n) -> Packet\n(n, \"ok\", true)",
        args1("n", Value::Int(5)),
    );
}

// ---- custom enum buffer-path coverage --------------------------------

fn enum_value(variant: &str, enum_name: &str, fields: Vec<(&str, Value)>) -> Value {
    Value::variant_dict(
        fields.into_iter().map(|(k, v)| (k.to_string(), v)),
        variant.to_string(),
        enum_name.to_string(),
    )
}

#[test]
fn custom_enum_unit_return_four_way() {
    assert_value_four_way(
        "#enum Stat { Up, Down }\n#main() -> Stat\nStat.Up",
        HashMap::new(),
    );
}

#[test]
fn custom_enum_struct_return_four_way() {
    assert_value_four_way(
        "#enum Notification { Email { address: String, subject: String }, Push }\n\
         #main() -> Notification\n\
         Notification.Email { address: \"a@b.c\", subject: \"hi\" }",
        HashMap::new(),
    );
}

#[test]
fn custom_enum_tuple_return_four_way() {
    assert_value_four_way(
        "#enum Packet { Pair(Int, String), Empty }\n#main() -> Packet\nPacket.Pair(7, \"x\")",
        HashMap::new(),
    );
}

#[test]
fn custom_enum_unit_ternary_return_four_way() {
    assert_value_four_way(
        "#enum Stat { Up, Down }
#main(Bool b) -> Stat
b ? Stat.Up : Stat.Down",
        args1("b", Value::Bool(true)),
    );
    assert_value_four_way(
        "#enum Stat { Up, Down }
#main(Bool b) -> Stat
b ? Stat.Up : Stat.Down",
        args1("b", Value::Bool(false)),
    );
}

#[test]
fn custom_enum_payload_ternary_return_four_way() {
    assert_value_four_way(
        r#"#enum Packet { Pair(Int, String), Empty }
#main(Bool b) -> Packet
b ? Packet.Pair(7, "x") : Packet.Empty"#,
        args1("b", Value::Bool(true)),
    );
    assert_value_four_way(
        r#"#enum Packet { Pair(Int, String), Empty }
#main(Bool b) -> Packet
b ? Packet.Pair(7, "x") : Packet.Empty"#,
        args1("b", Value::Bool(false)),
    );
}

#[test]
fn custom_enum_unit_param_identity_four_way() {
    assert_value_four_way(
        "#enum Stat { Up, Down }\n#main(Stat s) -> Stat\ns",
        args1("s", enum_value("Down", "Stat", vec![])),
    );
}

#[test]
fn custom_enum_tuple_param_identity_four_way() {
    assert_value_four_way(
        "#enum Packet { Pair(Int, String), Empty }\n#main(Packet p) -> Packet\np",
        args1(
            "p",
            enum_value(
                "Pair",
                "Packet",
                vec![("0", Value::Int(7)), ("1", Value::String("x".into()))],
            ),
        ),
    );
}

#[test]
fn custom_enum_unit_list_param_identity_four_way() {
    assert_value_four_way(
        "#enum Stat { Up, Down }
#main(List<Stat> xs) -> List<Stat>
xs",
        args1(
            "xs",
            Value::List(Arc::new(vec![
                enum_value("Up", "Stat", vec![]),
                enum_value("Down", "Stat", vec![]),
            ])),
        ),
    );
}

#[test]
fn custom_enum_tuple_list_param_identity_four_way() {
    assert_value_four_way(
        "#enum Packet { Pair(Int, String), Empty }
#main(List<Packet> xs) -> List<Packet>
xs",
        args1(
            "xs",
            Value::List(Arc::new(vec![
                enum_value(
                    "Pair",
                    "Packet",
                    vec![("0", Value::Int(7)), ("1", Value::String("x".into()))],
                ),
                enum_value("Empty", "Packet", vec![]),
            ])),
        ),
    );
}

#[test]
fn custom_enum_unit_list_literal_return_four_way() {
    assert_value_four_way(
        "#enum Stat { Up, Down }
#main() -> List<Stat>
[Stat.Up, Stat.Down]",
        HashMap::new(),
    );
}

#[test]
fn custom_enum_tuple_list_literal_return_four_way() {
    assert_value_four_way(
        r#"#enum Packet { Pair(Int, String), Empty }
#main() -> List<Packet>
[Packet.Pair(7, "x"), Packet.Empty]"#,
        HashMap::new(),
    );
}

#[test]
fn custom_enum_empty_list_literal_return_four_way() {
    assert_value_four_way(
        "#enum Stat { Up, Down }
#main() -> List<Stat>
[]",
        HashMap::new(),
    );
}

#[test]
fn custom_enum_unit_list_map_return_four_way() {
    assert_value_four_way(
        "#enum Stat { Up, Down }
#main(List<Int> xs) -> List<Stat>
xs.map((Int x) => x > 0 ? Stat.Up : Stat.Down)",
        args1(
            "xs",
            Value::List(Arc::new(vec![Value::Int(1), Value::Int(-1), Value::Int(2)])),
        ),
    );
}

#[test]
fn option_list_map_return_four_way() {
    assert_value_four_way(
        "#main(List<Int> xs) -> List<Option<Int>>
xs.map((Int x) => x > 0 ? Some(x) : None)",
        args1(
            "xs",
            Value::List(Arc::new(vec![Value::Int(1), Value::Int(-1), Value::Int(2)])),
        ),
    );
}

#[test]
fn result_list_map_return_four_way() {
    assert_value_four_way(
        "#main(List<Int> xs) -> List<Result<Int, String>>
xs.map((Int x) => x > 0 ? Ok(x) : Err(\"bad\"))",
        args1(
            "xs",
            Value::List(Arc::new(vec![Value::Int(1), Value::Int(-1), Value::Int(2)])),
        ),
    );
}

#[test]
fn custom_enum_tuple_list_map_return_four_way() {
    assert_value_four_way(
        r#"#enum Packet { Pair(Int, String), Empty }
#main(List<Int> xs) -> List<Packet>
xs.map((Int x) => x > 0 ? Packet.Pair(x, "x") : Packet.Empty)"#,
        args1(
            "xs",
            Value::List(Arc::new(vec![Value::Int(1), Value::Int(-1), Value::Int(2)])),
        ),
    );
}

#[test]
fn custom_enum_struct_list_map_return_four_way() {
    assert_value_four_way(
        r#"#enum Msg { Email { code: Int, subject: String }, Push }
#main(List<Int> xs) -> List<Msg>
xs.map((Int x) => x > 0 ? Msg.Email { code: x, subject: "hi" } : Msg.Push)"#,
        args1(
            "xs",
            Value::List(Arc::new(vec![Value::Int(1), Value::Int(-1), Value::Int(2)])),
        ),
    );
}

#[test]
fn custom_enum_tuple_comprehension_return_four_way() {
    assert_value_four_way(
        r#"#enum Packet { Pair(Int, String), Empty }
#main(List<Int> xs) -> List<Packet>
[x > 0 ? Packet.Pair(x, "x") : Packet.Empty for x in xs]"#,
        args1(
            "xs",
            Value::List(Arc::new(vec![Value::Int(1), Value::Int(-1), Value::Int(2)])),
        ),
    );
}

#[test]
fn option_list_filter_return_four_way() {
    assert_value_four_way(
        "#main(List<Option<Int>> xs) -> List<Option<Int>>
xs.filter((Option<Int> x) => true)",
        args1(
            "xs",
            Value::List(Arc::new(vec![
                Value::option_some(Value::Int(1)),
                Value::option_none(),
                Value::option_some(Value::Int(2)),
            ])),
        ),
    );
}

#[test]
fn result_list_filter_return_four_way() {
    assert_value_four_way(
        "#main(List<Result<Int, String>> xs) -> List<Result<Int, String>>
xs.filter((Result<Int, String> x) => true)",
        args1(
            "xs",
            Value::List(Arc::new(vec![
                enum_value("Ok", "Result", vec![("value", Value::Int(1))]),
                enum_value(
                    "Err",
                    "Result",
                    vec![("error", Value::String("bad".into()))],
                ),
            ])),
        ),
    );
}

#[test]
fn custom_enum_unit_list_filter_return_four_way() {
    let input = Value::List(Arc::new(vec![
        enum_value("Up", "Stat", vec![]),
        enum_value("Down", "Stat", vec![]),
        enum_value("Up", "Stat", vec![]),
    ]));
    assert_value_four_way(
        "#enum Stat { Up, Down }
#main(List<Stat> xs) -> List<Stat>
xs.filter((Stat s) => s match { Up: true, Down: false })",
        args1("xs", input.clone()),
    );
    assert_value_four_way(
        "#enum Stat { Up, Down }
#main(List<Stat> xs) -> List<Stat>
_list_filter(xs, (Stat s) => s match { Up: true, Down: false })",
        args1("xs", input),
    );
}

#[test]
fn custom_enum_tuple_list_filter_payload_pattern_four_way() {
    assert_value_four_way(
        r#"#enum Packet { Pair(Int, String), Empty }
#main(List<Packet> xs) -> List<Packet>
xs.filter((Packet p) => p match { Pair(n, _): n > 0, Empty: false })"#,
        args1(
            "xs",
            Value::List(Arc::new(vec![
                enum_value(
                    "Pair",
                    "Packet",
                    vec![("0", Value::Int(1)), ("1", Value::String("x".into()))],
                ),
                enum_value("Empty", "Packet", vec![]),
                enum_value(
                    "Pair",
                    "Packet",
                    vec![("0", Value::Int(-1)), ("1", Value::String("n".into()))],
                ),
            ])),
        ),
    );
}

#[test]
fn custom_enum_unit_comprehension_return_four_way() {
    assert_value_four_way(
        "#enum Stat { Up, Down }
#main(List<Int> xs) -> List<Stat>
[x > 0 ? Stat.Up : Stat.Down for x in xs]",
        args1(
            "xs",
            Value::List(Arc::new(vec![Value::Int(1), Value::Int(-1), Value::Int(2)])),
        ),
    );
}

#[test]
fn option_match_param_four_way() {
    assert_value_four_way(
        "#main(Option<Int> x) -> Int
x match { Some(v): v + 1, None: 0 }",
        args1("x", Value::option_some(Value::Int(41))),
    );
    assert_value_four_way(
        "#main(Option<Int> x) -> Int
x match { Some(v): v + 1, None: 0 }",
        args1("x", Value::option_none()),
    );
}

#[test]
fn option_match_direct_payload_access_four_way() {
    assert_value_four_way(
        "#main(Option<Int> x) -> Int
x match { Some: x.value, None: 0 }",
        args1("x", Value::option_some(Value::Int(7))),
    );
}

#[test]
fn result_match_param_four_way() {
    assert_value_four_way(
        "#main(Result<Int, String> r) -> Int
r match { Ok(v): v + 1, Err(e): 0 }",
        args1(
            "r",
            Value::variant_dict(
                [("value", Value::Int(41))],
                "Ok".to_string(),
                "Result".to_string(),
            ),
        ),
    );
    assert_value_four_way(
        "#main(Result<Int, String> r) -> Int
r match { Ok(v): v + 1, Err(e): 0 }",
        args1(
            "r",
            Value::variant_dict(
                [("error", Value::String("no".into()))],
                "Err".to_string(),
                "Result".to_string(),
            ),
        ),
    );
}

#[test]
fn custom_enum_unit_match_param_four_way() {
    assert_value_four_way(
        "#enum Stat { Up, Down }\n#main(Stat s) -> Int\ns match { Up: 1, Down: 0 }",
        args1("s", enum_value("Down", "Stat", vec![])),
    );
}

#[test]
fn custom_enum_struct_match_param_four_way() {
    assert_value_four_way(
        "#enum Notification { Email { address: String }, Push }\n\
         #main(Notification msg) -> Int\n\
         msg match { Push: 0, Email: 1 }",
        args1(
            "msg",
            enum_value(
                "Email",
                "Notification",
                vec![("address", Value::String("a@b.c".into()))],
            ),
        ),
    );
}

#[test]
fn custom_enum_tuple_match_param_with_wildcard_four_way() {
    assert_value_four_way(
        "#enum Packet { Pair(Int, String), Empty }\n\
         #main(Packet p) -> Int\n\
         p match { Empty: 0, _: 1 }",
        args1(
            "p",
            enum_value(
                "Pair",
                "Packet",
                vec![("0", Value::Int(7)), ("1", Value::String("x".into()))],
            ),
        ),
    );
}

#[test]
fn custom_enum_struct_match_payload_field_four_way() {
    assert_value_four_way(
        "#enum Notification { Email { address: String }, Push }\n\
         #main(Notification msg) -> String\n\
         msg match { Push: \"\", Email: msg.address }",
        args1(
            "msg",
            enum_value(
                "Email",
                "Notification",
                vec![("address", Value::String("a@b.c".into()))],
            ),
        ),
    );
}

#[test]
fn custom_enum_tuple_match_payload_index_four_way() {
    assert_value_four_way(
        "#enum Packet { Pair(Int, String), Empty }\n\
         #main(Packet p) -> Int\n\
         p match { Empty: 0, Pair: p.0 }",
        args1(
            "p",
            enum_value(
                "Pair",
                "Packet",
                vec![("0", Value::Int(7)), ("1", Value::String("x".into()))],
            ),
        ),
    );
}

#[test]
fn custom_enum_tuple_match_payload_pattern_four_way() {
    assert_value_four_way(
        "#enum Packet { Pair(Int, String), Empty }\n\
         #main(Packet p) -> Int\n\
         p match { Pair(n, _): n + 1, Empty: 0 }",
        args1(
            "p",
            enum_value(
                "Pair",
                "Packet",
                vec![("0", Value::Int(7)), ("1", Value::String("x".into()))],
            ),
        ),
    );
}

#[test]
fn custom_enum_struct_match_payload_pattern_four_way() {
    assert_value_four_way(
        "#enum Notification { Email { code: Int, subject: String }, Push }\n\
         #main(Notification msg) -> Int\n\
         msg match { Notification.Email { code, subject: _ }: code + 1, Push: 0 }",
        args1(
            "msg",
            enum_value(
                "Email",
                "Notification",
                vec![
                    ("code", Value::Int(41)),
                    ("subject", Value::String("hi".into())),
                ],
            ),
        ),
    );
}

#[test]
fn custom_generic_enum_input_identity_four_way() {
    assert_value_four_way(
        "#enum Box<T> { Some(T), None }\n#main(Box<Int> b) -> Box<Int>\nb",
        args1("b", enum_value("Some", "Box", vec![("0", Value::Int(7))])),
    );
}

#[test]
fn custom_generic_enum_match_payload_pattern_four_way() {
    assert_value_four_way(
        "#enum Box<T> { Some(T), None }\n\
         #main(Box<Int> b) -> Int\n\
         b match { Some(n): n + 1, None: 0 }",
        args1("b", enum_value("Some", "Box", vec![("0", Value::Int(7))])),
    );
}

#[test]
fn custom_enum_nested_tuple_payload_identity_four_way() {
    assert_value_four_way(
        "#enum Payload { Nested(Tuple<Int, String>), Empty }\n\
         #main(Payload p) -> Payload\n\
         p",
        args1(
            "p",
            enum_value(
                "Nested",
                "Payload",
                vec![(
                    "0",
                    Value::Tuple(Arc::new(vec![Value::Int(7), Value::String("x".into())])),
                )],
            ),
        ),
    );
}

#[test]
fn custom_enum_list_payload_identity_four_way() {
    assert_value_four_way(
        "#enum Payload { Numbers(List<Int>), Empty }\n\
         #main(Payload p) -> Payload\n\
         p",
        args1(
            "p",
            enum_value(
                "Numbers",
                "Payload",
                vec![(
                    "0",
                    Value::List(Arc::new(vec![Value::Int(1), Value::Int(2)])),
                )],
            ),
        ),
    );
}

#[test]
fn custom_enum_option_result_payload_identity_four_way() {
    assert_value_four_way(
        "#enum Payload { Maybe(Option<Int>), Outcome(Result<Int, String>), Empty }\n\
         #main(Payload p) -> Payload\n\
         p",
        args1(
            "p",
            enum_value(
                "Maybe",
                "Payload",
                vec![("0", Value::option_some(Value::Int(9)))],
            ),
        ),
    );
    assert_value_four_way(
        "#enum Payload { Maybe(Option<Int>), Outcome(Result<Int, String>), Empty }\n\
         #main(Payload p) -> Payload\n\
         p",
        args1(
            "p",
            enum_value(
                "Outcome",
                "Payload",
                vec![(
                    "0",
                    Value::variant_dict(
                        [("error", Value::String("no".into()))],
                        "Err".to_string(),
                        "Result".to_string(),
                    ),
                )],
            ),
        ),
    );
}

#[test]
fn custom_generic_enum_return_constructor_four_way() {
    assert_value_four_way(
        "#enum Box<T> { Some(T), None }\n#main() -> Box<Int>\nBox.Some(7)",
        HashMap::new(),
    );
}

#[test]
fn custom_generic_enum_struct_payload_constructor_four_way() {
    assert_value_four_way(
        "#enum Box<T> { Some { value: T }, None }
#main() -> Box<Int>
Box.Some { value: 7 }",
        HashMap::new(),
    );
}

#[test]
fn custom_enum_nested_payload_constructors_four_way() {
    assert_value_four_way(
        "#enum Payload { Nested(Tuple<Int, String>), Numbers(List<Int>), Maybe(Option<Int>), Empty }\n\
         #main() -> Payload\n\
         Payload.Nested((7, \"x\"))",
        HashMap::new(),
    );
    assert_value_four_way(
        "#enum Payload { Nested(Tuple<Int, String>), Numbers(List<Int>), Maybe(Option<Int>), Empty }\n\
         #main() -> Payload\n\
         Payload.Numbers([1, 2])",
        HashMap::new(),
    );
    assert_value_four_way(
        "#enum Payload { Nested(Tuple<Int, String>), Numbers(List<Int>), Maybe(Option<Int>), Empty }\n\
         #main() -> Payload\n\
         Payload.Maybe(Some(9))",
        HashMap::new(),
    );
}
