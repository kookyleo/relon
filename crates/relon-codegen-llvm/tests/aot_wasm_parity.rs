//! P3 parity — the **new** LLVM→wasm32 object path
//! (`emit_object_for_target(.., CodegenTarget::Wasm32)` → `wasm-ld` →
//! wasmtime) re-run against the *full* old hand-written-wasm corpus
//! (`relon-codegen-wasm` / `relon-wasm-evaluator` smokes). This file
//! extends `aot_wasm.rs`: where `aot_wasm.rs` proved a curated handful,
//! this drives every workload the old `WasmEvaluator` smokes covered
//! that the new path can emit, and differentials each against the
//! **native** `run_main` oracle (native is bit-aligned to tree-walk +
//! cranelift from P2).
//!
//! Coverage classes exercised here, mirroring the old corpus:
//!   - scalar arithmetic / control flow / modulo (w12, z4_walker)
//!   - `list.sum(range(n))` and `.map(...)`-closure sums (w1/w2/w5
//!     inline/w6/w8/w10)
//!   - `range(n).reduce(...)` single + nested + factorial (z4_list,
//!     w9 inline)
//!   - String return via the buffer/tail protocol (w3, const-string)
//!   - `List<Int>` const return via the buffer/tail protocol (z4_list)
//!   - multi-field Dict return via the fixed-area buffer protocol
//!     (z4_dict_return)
//!   - `.filter(s.contains("x"))` string-literal body returning Int via
//!     the buffer protocol (w4 — its `Int -> Int` schema is fast-
//!     eligible, but the `Op::ConstString` literals force it onto the
//!     buffer entry so the const-pool resolves)
//!
//! **Honest gaps not asserted as green** (recorded in the agent report,
//! not faked): the W5 production Dict source with nested-Dict fields is
//! rejected **before any backend codegen** by the shared `relon-ir`
//! lowering layer (`AnonDictReturn(... unsupported value shape 'Dict')`)
//! — the same verdict for native object-emit, wasm32, AND cranelift, so
//! widening it is an IR-layer change shared with the cranelift backend,
//! out of scope here. (The sibling W7 `#internal`-recursive-closure Dict
//! now lowers four ways — see
//! `w7_recursive_closure_dict_aligns_four_ways_via_wasmtime`; its old
//! "rejected" verdict was a fast-path mis-route in
//! `emit_object_for_target`, not an IR / cranelift gap.) Also the
//! `relon-wasm-bindings` browser/LSP surface (a wasm-bindgen tree-walk
//! interpreter, an orthogonal mechanism the AOT object path does not and
//! is not meant to replace).
//!
//! **No fake green**: every assertion runs the value out of wasmtime and
//! compares to the native oracle.

use std::collections::HashMap;
use std::path::PathBuf;

use relon_codegen_llvm::{
    CodegenTarget, EmitObjectInfo, EmittedFieldType, LlvmAotEvaluator, WorldMode,
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

/// Emit + link `src` into a `.wasm`, returning the linked bytes plus the
/// `EmitObjectInfo` (entry shape, field offsets, const-data, tail flag).
fn build(name: &str, src: &str) -> (Vec<u8>, EmitObjectInfo) {
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let obj: PathBuf = tmp.join(format!("relon_parity_{name}_{pid}.o"));
    let wasm: PathBuf = tmp.join(format!("relon_parity_{name}_{pid}.wasm"));
    let entry = format!("relon_parity_{name}");

    let info = LlvmAotEvaluator::emit_object_for_target(
        src,
        &entry,
        &obj,
        &opts(),
        WorldMode::OpenWorld,
        None,
        CodegenTarget::Wasm32,
    )
    .unwrap_or_else(|e| panic!("[{name}] wasm32 emit_object: {e:?}"));

    let obj_bytes = std::fs::read(&obj).expect("read obj");
    assert_eq!(&obj_bytes[..4], b"\0asm", "[{name}] object \\0asm magic");

    relon_codegen_llvm::wasm_link::link_wasm_object(&obj, &wasm, &entry)
        .unwrap_or_else(|e| panic!("[{name}] wasm-ld link: {e:?}"));
    let bytes = std::fs::read(&wasm).expect("read wasm");
    assert_eq!(&bytes[..4], b"\0asm", "[{name}] linked module magic");
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&wasm);
    (bytes, info)
}

/// Build a wasmtime `Linker` carrying the compiler-rt `__multi3`
/// 128-bit-multiply builtin the LLVM wasm backend emits for wide
/// `list.sum` accumulation. Writes the i128 product LE to
/// `memory[ret_ptr..+16]`. (Identical to `aot_wasm.rs`'s helper; the
/// wasm module stays self-contained.)
fn linker_with_multi3(engine: &wasmtime::Engine) -> wasmtime::Linker<()> {
    use wasmtime::{Caller, Extern, Linker};
    let mut linker = Linker::new(engine);
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
                    _ => panic!("__multi3 needs an exported `memory`"),
                };
                mem.write(&mut caller, ret_ptr as usize, &prod.to_le_bytes())
                    .expect("__multi3 store");
            },
        )
        .expect("register __multi3");

    // String / List tail-payload copies lower to libc `memcpy` /
    // `memmove` / `memset` against linear memory. The native target gets
    // these from compiler-rt/libc; for the standalone wasm module we
    // satisfy them as host imports operating on the exported `memory`.
    // Each returns its `dest` pointer (libc contract).
    linker
        .func_wrap(
            "env",
            "memcpy",
            |mut caller: Caller<'_, ()>, dest: i32, src: i32, n: i32| -> i32 {
                let mem = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => panic!("memcpy needs an exported `memory`"),
                };
                let n = n as usize;
                let mut tmp = vec![0u8; n];
                mem.read(&caller, src as usize, &mut tmp)
                    .expect("memcpy read");
                mem.write(&mut caller, dest as usize, &tmp)
                    .expect("memcpy write");
                dest
            },
        )
        .expect("register memcpy");
    linker
        .func_wrap(
            "env",
            "memmove",
            |mut caller: Caller<'_, ()>, dest: i32, src: i32, n: i32| -> i32 {
                let mem = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => panic!("memmove needs an exported `memory`"),
                };
                let n = n as usize;
                let mut tmp = vec![0u8; n];
                mem.read(&caller, src as usize, &mut tmp)
                    .expect("memmove read");
                mem.write(&mut caller, dest as usize, &tmp)
                    .expect("memmove write");
                dest
            },
        )
        .expect("register memmove");
    linker
        .func_wrap(
            "env",
            "memset",
            |mut caller: Caller<'_, ()>, dest: i32, c: i32, n: i32| -> i32 {
                let mem = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => panic!("memset needs an exported `memory`"),
                };
                let fill = vec![c as u8; n as usize];
                mem.write(&mut caller, dest as usize, &fill)
                    .expect("memset write");
                dest
            },
        )
        .expect("register memset");

    // The W4 `s.contains("x")` const-needle inline lowers its byte-scan
    // to libc `memchr` (find byte `c` in the first `n` bytes at `s`).
    // Native gets it from libc; the standalone wasm module imports it.
    // Returns the absolute pointer to the first match, or 0 (NULL) when
    // the byte is absent — the libc contract the inline scan relies on.
    linker
        .func_wrap(
            "env",
            "memchr",
            |mut caller: Caller<'_, ()>, s: i32, c: i32, n: i64| -> i32 {
                let mem = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => panic!("memchr needs an exported `memory`"),
                };
                let n = n as usize;
                let mut buf = vec![0u8; n];
                mem.read(&caller, s as usize, &mut buf)
                    .expect("memchr read");
                let needle = c as u8;
                match buf.iter().position(|&b| b == needle) {
                    Some(off) => s + off as i32,
                    None => 0,
                }
            },
        )
        .expect("register memchr");
    linker
}

/// Run a `FastInt` `(i64..) -> i64` export in wasmtime.
fn run_fast(bytes: &[u8], entry: &str, args: &[i64]) -> i64 {
    use wasmtime::{Engine, Module, Store, Val};
    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("Module::new");
    let mut store = Store::new(&engine, ());
    let linker = linker_with_multi3(&engine);
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate");
    let func = instance
        .get_func(&mut store, entry)
        .unwrap_or_else(|| panic!("export `{entry}` missing"));
    let params: Vec<Val> = args.iter().map(|a| Val::I64(*a)).collect();
    let mut results = [Val::I64(0)];
    func.call(&mut store, &params, &mut results)
        .expect("fast entry call");
    match results[0] {
        Val::I64(v) => v,
        other => panic!("expected i64, got {other:?}"),
    }
}

/// Decoded value read out of a buffer-protocol wasm return region. Only
/// the leaf shapes the old corpus returns are modelled.
#[derive(Debug, PartialEq)]
enum Decoded {
    Int(i64),
    Float(f64),
    Str(String),
    ListInt(Vec<i64>),
}

/// Drive a buffer-protocol entry in wasmtime with a full arena laid out
/// exactly like the native `dispatch_with_arena`:
/// `[const_data | in_buf | out_buf(root + tail_cap) | scratch]`.
/// `arena_base` is set to the arena's absolute linear-memory offset so
/// the body's `arena_base + buf_ptr + offset` arithmetic resolves into
/// real linear memory. Returns the decoded return fields by name.
///
/// The tail wire format (documented on `EmittedFieldType::String` /
/// `ListInt` and implemented by `BufferReader`) is **out_ptr-relative**:
/// the fixed-area `u32` slot holds a buffer-relative `record_start`; at
/// `record_start` sits a `u32` len, then the payload (lists pad the
/// payload start up to 8).
fn run_buffer(
    bytes: &[u8],
    entry: &str,
    info: &EmitObjectInfo,
    in_record: &[u8],
) -> HashMap<String, Decoded> {
    use wasmtime::{Engine, Extern, Module, Store, Val};

    // ArenaState layout (mirrors aot_wasm.rs): arena_base i64 @0,
    // tail_cursor u32 @12, scratch_base u32 @20, size 40.
    const STATE_OFF_ARENA_BASE: usize = 0;
    const STATE_OFF_TAIL_CURSOR: usize = 12;
    const STATE_OFF_SCRATCH_BASE: usize = 20;
    const STATE_SIZE: usize = 40;

    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("Module::new");
    let mut store = Store::new(&engine, ());
    let linker = linker_with_multi3(&engine);
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

    // Arena-relative layout: [const_data | in_buf | out_buf | scratch].
    let const_len = info.const_data.len() as u32;
    let in_ptr = align8(const_len);
    let in_len = in_record.len() as u32;
    let out_root = info.return_root_size.max(8);
    // Tail cushion for String / List returns (native reserves 64 KiB).
    let tail_cap = if info.return_has_tail { 65_536u32 } else { 0 };
    let out_ptr = align8(in_ptr + in_len);
    let out_cap = align8(out_root + tail_cap + 16);
    let scratch_base = align8(out_ptr + out_cap);
    let scratch_size = 1_048_576u32;
    let arena_bytes = scratch_base + scratch_size;

    let needed = (arena_off + arena_bytes) as usize;
    let cur = memory.data_size(&store);
    if needed > cur {
        let extra_pages = (needed - cur).div_ceil(65536) as u64;
        memory.grow(&mut store, extra_pages).expect("grow memory");
    }

    let arena_abs = arena_off;

    // ArenaState. tail_cursor starts at out_root (pointer-indirect
    // StoreField bumps past the fixed area into the tail), matching the
    // native `ArenaState::new` + out-region convention.
    let mut state = [0u8; STATE_SIZE];
    state[STATE_OFF_ARENA_BASE..STATE_OFF_ARENA_BASE + 8]
        .copy_from_slice(&(arena_abs as u64).to_le_bytes());
    state[STATE_OFF_TAIL_CURSOR..STATE_OFF_TAIL_CURSOR + 4]
        .copy_from_slice(&out_root.to_le_bytes());
    state[STATE_OFF_SCRATCH_BASE..STATE_OFF_SCRATCH_BASE + 4]
        .copy_from_slice(&scratch_base.to_le_bytes());
    memory
        .write(&mut store, state_ptr as usize, &state)
        .expect("write state");

    // const_data pool at arena offset 0.
    if !info.const_data.is_empty() {
        memory
            .write(&mut store, arena_abs as usize, &info.const_data)
            .expect("write const_data");
    }
    // Input record.
    memory
        .write(&mut store, (arena_abs + in_ptr) as usize, in_record)
        .expect("write in record");

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
    let bytes_written = match results[0] {
        Val::I32(v) => v,
        other => panic!("expected i32 bytes_written, got {other:?}"),
    };
    assert!(bytes_written >= 0, "negative bytes_written {bytes_written}");

    // Read the whole out region (root + tail) so tail records resolve.
    let read_len = out_cap as usize;
    let mut out = vec![0u8; read_len];
    memory
        .read(&store, (arena_abs + out_ptr) as usize, &mut out)
        .expect("read out region");

    // Decode each return field at its fixed-area offset. F1 stores every
    // pointer slot as an **arena-absolute** offset (the unified slot
    // convention `BufferReader` now walks over the whole arena); the tail
    // records still live in out_buf, so we rebase an absolute slot value
    // by `- out_ptr` to index into the `out` (out_buf) slice.
    let read_u32 = |buf: &[u8], at: usize| -> usize {
        u32::from_le_bytes(buf[at..at + 4].try_into().unwrap()) as usize
    };
    let out_ptr_us = out_ptr as usize;
    let mut decoded = HashMap::new();
    for f in &info.return_fields {
        let off = f.offset as usize;
        let val = match f.ty {
            EmittedFieldType::Int => {
                Decoded::Int(i64::from_le_bytes(out[off..off + 8].try_into().unwrap()))
            }
            EmittedFieldType::Float => {
                Decoded::Float(f64::from_le_bytes(out[off..off + 8].try_into().unwrap()))
            }
            EmittedFieldType::Bool => Decoded::Int(if out[off] != 0 { 1 } else { 0 }),
            EmittedFieldType::Null => Decoded::Int(0),
            EmittedFieldType::String => {
                let record_start = read_u32(&out, off) - out_ptr_us;
                let len = read_u32(&out, record_start);
                let payload = record_start + 4;
                let s = std::str::from_utf8(&out[payload..payload + len])
                    .expect("utf8 tail payload")
                    .to_string();
                Decoded::Str(s)
            }
            EmittedFieldType::ListInt => {
                let record_start = read_u32(&out, off) - out_ptr_us;
                let count = read_u32(&out, record_start);
                // List payload pads the start up to 8 (tail_alignment).
                let raw = record_start + 4;
                let payload = (raw + 7) & !7usize;
                let mut v = Vec::with_capacity(count);
                let mut cur = payload;
                for _ in 0..count {
                    v.push(i64::from_le_bytes(out[cur..cur + 8].try_into().unwrap()));
                    cur += 8;
                }
                Decoded::ListInt(v)
            }
        };
        decoded.insert(f.name.clone(), val);
    }
    decoded
}

// ---------------------------------------------------------------------
// FastInt corpus — old `WasmEvaluator` Int-return smokes whose new path
// lowers to the typed `(i64..) -> i64` entry. Native fast-dispatch is
// the oracle.
// ---------------------------------------------------------------------

struct Fast {
    name: &'static str,
    src: &'static str,
    args: &'static [i64],
}

const FAST: &[Fast] = &[
    // w12 — increment Int.
    Fast {
        name: "w12_increment",
        src: "#main(Int x) -> Int\nx + 1",
        args: &[41],
    },
    // z4_walker — arithmetic chain / ternary / modulo.
    Fast {
        name: "arith_chain",
        src: "#main(Int n) -> Int\n(n + 1) * (n + 2) - n",
        args: &[10],
    },
    Fast {
        name: "ternary",
        src: "#main(Int n) -> Int\nn < 0 ? 0 : n",
        args: &[-5],
    },
    Fast {
        name: "modulo",
        src: "#main(Int n) -> Int\nn % 7",
        args: &[100],
    },
    // w1 — list.sum(range(n)).
    Fast {
        name: "w1_listsum_range",
        src: "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))",
        args: &[1000],
    },
    // w2 — map closure (i+1)*(i+2) sum.
    Fast {
        name: "w2_dot",
        src: "#import list from \"std/list\"\n#main(Int n) -> Int\n\
               list.sum(range(n).map((i) => (i + 1) * (i + 2)))",
        args: &[100],
    },
    // w5 inline — (i%10)+1 map sum.
    Fast {
        name: "w5_inline",
        src: "#import list from \"std/list\"\n#main(Int n) -> Int\n\
               list.sum(range(n).map((i) => (i % 10) + 1))",
        args: &[200],
    },
    // w6 — i+1 map sum.
    Fast {
        name: "w6_listsum_plus1",
        src: "#import list from \"std/list\"\n#main(Int n) -> Int\n\
               list.sum(range(n).map((i) => i + 1))",
        args: &[300],
    },
    // w8 inline — polymorphic dispatch (nested ternary) sum.
    Fast {
        name: "w8_dispatch",
        src: "#import list from \"std/list\"\n#main(Int n) -> Int\n\
               list.sum(range(n).map((i) => \
               (i % 4) == 0 ? 1 : (i % 4) == 1 ? 2 : (i % 4) == 2 ? 3 : 4))",
        args: &[97],
    },
    // w9 inline — nested range.reduce.
    Fast {
        name: "w9_nested_reduce",
        src: "#main(Int n) -> Int\n\
               range(n).reduce(0, (acc, j) => \
               acc + range(n).reduce(0, (inner, i) => inner + (i * n + j)))",
        args: &[20],
    },
    // w10 inline — config-eval predicate count.
    Fast {
        name: "w10_config_eval",
        src: "#import list from \"std/list\"\n#main(Int n) -> Int\n\
               list.sum(range(n).map((i) => \
               (i % 3 == 0 || i % 3 == 1) && \
               (i % 4 == 0 || i % 4 == 1) && \
               (i % 24 >= 8 && i % 24 < 18) ? 1 : 0))",
        args: &[240],
    },
    // z4_list — single range.reduce sum.
    Fast {
        name: "range_reduce_sum",
        src: "#main(Int n) -> Int\nrange(n).reduce(0, (acc, i) => acc + i)",
        args: &[50],
    },
    // z4_list — factorial-style reduce.
    Fast {
        name: "factorial_reduce",
        src: "#main(Int n) -> Int\nrange(n).reduce(1, (acc, i) => acc * (i + 1))",
        args: &[8],
    },
];

#[test]
fn fastint_corpus_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping FastInt corpus");
        return;
    }
    for wl in FAST {
        // Native oracle via the typed fast entry.
        let ev = LlvmAotEvaluator::from_source(wl.src)
            .unwrap_or_else(|e| panic!("[{}] native from_source: {e:?}", wl.name));
        assert!(
            ev.has_fast_path(),
            "[{}] expected fast-path eligibility",
            wl.name
        );
        let want = ev
            .run_main_legacy_i64_fast(wl.args)
            .unwrap_or_else(|e| panic!("[{}] native fast dispatch: {e:?}", wl.name));

        let (bytes, info) = build(wl.name, wl.src);
        assert!(
            matches!(info.shape, relon_codegen_llvm::EmittedEntryShape::FastInt),
            "[{}] expected FastInt shape, got {:?}",
            wl.name,
            info.shape
        );
        let entry = format!("relon_parity_{}", wl.name);
        let got = run_fast(&bytes, &entry, wl.args);
        assert_eq!(
            got, want,
            "[{}] wasm result {got} != native oracle {want}",
            wl.name
        );
    }
}

// ---------------------------------------------------------------------
// Buffer corpus — old smokes with String / List / multi-field-Dict
// returns. Native `run_main` is the oracle. The wasm arena driver lays
// const_data + tail and decodes via the documented wire format.
// ---------------------------------------------------------------------

/// Pack a single-Int `#main(Int <name>)` arg into the input record.
fn pack_single_int(info: &EmitObjectInfo, value: i64) -> Vec<u8> {
    let mut rec = vec![0u8; info.main_root_size as usize];
    let off = info.main_fields[0].offset as usize;
    rec[off..off + 8].copy_from_slice(&value.to_le_bytes());
    rec
}

fn native_run(src: &str, args: HashMap<String, Value>) -> Value {
    let ev = LlvmAotEvaluator::from_source(src).expect("native from_source");
    ev.run_main(args).expect("native run_main")
}

/// w3 — `range(n).map(i => "a").reduce("", (acc, s) => acc + s)` → String.
#[test]
fn w3_string_return_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping w3 string return");
        return;
    }
    let src = "#import list from \"std/list\"\n#main(Int n) -> String\n\
               range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)";
    let n = 5i64;
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(n))])) {
        Value::String(s) => s,
        other => panic!("native expected String, got {other:?}"),
    };

    let (bytes, info) = build("w3_string", src);
    assert!(matches!(
        info.shape,
        relon_codegen_llvm::EmittedEntryShape::Buffer
    ));
    let in_record = pack_single_int(&info, n);
    let out = run_buffer(&bytes, "relon_parity_w3_string", &info, &in_record);
    match out.get("value") {
        Some(Decoded::Str(s)) => assert_eq!(*s, want, "w3 wasm String != native"),
        other => panic!("w3 decoded {other:?}"),
    }
}

/// const-string return (`#main(Int n) -> String "hello"`) → String.
#[test]
fn const_string_return_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping const string return");
        return;
    }
    let src = "#main(Int n) -> String\n\"hello\"";
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(1))])) {
        Value::String(s) => s,
        other => panic!("native expected String, got {other:?}"),
    };
    let (bytes, info) = build("const_string", src);
    let in_record = pack_single_int(&info, 1);
    let out = run_buffer(&bytes, "relon_parity_const_string", &info, &in_record);
    match out.get("value") {
        Some(Decoded::Str(s)) => assert_eq!(*s, want, "const-string wasm != native"),
        other => panic!("const-string decoded {other:?}"),
    }
}

/// z4_list — const `List<Int>` return (`#main(Int n) -> List<Int> [10,20,30]`).
#[test]
fn const_list_int_return_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping const list return");
        return;
    }
    let src = "#main(Int n) -> List<Int>\n[10, 20, 30]";
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(1))])) {
        Value::List(items) => items
            .iter()
            .map(|v| match v {
                Value::Int(i) => *i,
                other => panic!("non-int list element {other:?}"),
            })
            .collect::<Vec<_>>(),
        other => panic!("native expected List, got {other:?}"),
    };
    let (bytes, info) = build("const_list", src);
    let in_record = pack_single_int(&info, 1);
    let out = run_buffer(&bytes, "relon_parity_const_list", &info, &in_record);
    match out.get("value") {
        Some(Decoded::ListInt(v)) => assert_eq!(*v, want, "const-list wasm != native"),
        other => panic!("const-list decoded {other:?}"),
    }
}

/// z4_dict_return — multi-field Int Dict return through the fixed-area
/// buffer protocol (`#main(Int a, Int b) -> Dict { x: a+b, y: a*b }`).
#[test]
fn multi_field_dict_return_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping multi-field dict return");
        return;
    }
    let src = "#main(Int a, Int b) -> Dict\n{ x: a + b, y: a * b }";
    let (a, b) = (6i64, 7i64);
    let dict = match native_run(
        src,
        HashMap::from([
            ("a".to_string(), Value::Int(a)),
            ("b".to_string(), Value::Int(b)),
        ]),
    ) {
        Value::Dict(d) => d,
        other => panic!("native expected Dict, got {other:?}"),
    };
    let want_x = match dict.map.get("x") {
        Some(Value::Int(v)) => *v,
        other => panic!("x not Int: {other:?}"),
    };
    let want_y = match dict.map.get("y") {
        Some(Value::Int(v)) => *v,
        other => panic!("y not Int: {other:?}"),
    };

    let (bytes, info) = build("dict_xy", src);
    // Pack a + b at their declared offsets.
    let mut in_record = vec![0u8; info.main_root_size as usize];
    for f in &info.main_fields {
        let off = f.offset as usize;
        let v = match f.name.as_str() {
            "a" => a,
            "b" => b,
            other => panic!("unexpected main field {other}"),
        };
        in_record[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    let out = run_buffer(&bytes, "relon_parity_dict_xy", &info, &in_record);
    assert_eq!(out.get("x"), Some(&Decoded::Int(want_x)), "dict field x");
    assert_eq!(out.get("y"), Some(&Decoded::Int(want_y)), "dict field y");
}

/// W5-P3 — `d[k]` dict-get probe runs on wasm32. A `#main` dict body
/// binds `#internal d` (an `Op::ConstDict` arena record) and the
/// `result` Int field probes it with a `ConstString` key. This proves
/// the IR-lowered linear-scan + byte-compare probe lowers to wasm32
/// with NO unsatisfiable import (only the standard libc symbols the
/// `linker_with_multi3` harness already provides) and matches the
/// native LLVM oracle byte-for-byte. The full w5 (map-loop capture +
/// `#internal keys` list) stays scope-cut until P4 — see
/// `w5_nested_dict_field_is_unsupported_on_wasm32_emit`.
#[test]
fn w5_p3_dict_get_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping w5-p3 dict-get");
        return;
    }
    // "c" is the middle of the sorted 5-entry table → value 3; the
    // probe must scan past "a"/"b" before matching.
    let src = "#main(Int i) -> Dict\n\
               {\n\
                 #internal\n\
                 d: { a: 1, b: 2, c: 3, d: 4, e: 5 },\n\
                 result: d[\"c\"]\n\
               }";
    let want = match native_run(src, HashMap::from([("i".to_string(), Value::Int(0))])) {
        Value::Dict(d) => match d.map.get("result") {
            Some(Value::Int(v)) => *v,
            other => panic!("native result not Int: {other:?}"),
        },
        other => panic!("native expected Dict, got {other:?}"),
    };
    assert_eq!(want, 3, "native oracle: d[\"c\"] == 3");

    let (bytes, info) = build("w5_p3_dict_get", src);
    assert!(
        matches!(info.shape, relon_codegen_llvm::EmittedEntryShape::Buffer),
        "w5-p3 expected Buffer shape, got {:?}",
        info.shape
    );
    let in_record = pack_single_int(&info, 0);
    let out = run_buffer(&bytes, "relon_parity_w5_p3_dict_get", &info, &in_record);
    assert_eq!(
        out.get("result"),
        Some(&Decoded::Int(want)),
        "w5-p3 wasm dict-get != native oracle"
    );
}

// ---------------------------------------------------------------------
// Honest ❌ gaps — assert the new wasm32 object-emit path *rejects* these
// old corpus shapes (so a future widening of the emitter that silently
// changes the verdict trips this test, prompting a parity re-eval). We
// assert the *emit* outcome, not a faked run.
// ---------------------------------------------------------------------

/// W4 `range(n).map(=>"axb").filter(s.contains("x")).len()` — the
/// string-literal map body used to hard-fail the wasm32 object-emit
/// path with `Op::ConstString { idx: 0 }: missing const-pool entry`.
/// Root cause: the `Int -> Int` schema matched the fast-entry profile,
/// but the body carries `Op::ConstString` literals the typed
/// `(i64..) -> i64` fast entry (empty const-pool, no `*state`) can't
/// lower. The object-emit path now routes a const-pool-touching body to
/// the buffer entry (mirroring MCJIT's emit-then-roll-back tolerance),
/// so the ConstString resolves against the real const-pool blob. This
/// asserts the wasm value out of wasmtime against the native oracle.
#[test]
fn w4_filter_contains_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping w4 contains-filter");
        return;
    }
    let src = "#import list from \"std/list\"\n#main(Int n) -> Int\n\
               range(n).map((i) => \"axb\").filter((s) => s.contains(\"x\")).len()";
    let n = 10i64;
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(n))])) {
        Value::Int(v) => v,
        other => panic!("native expected Int, got {other:?}"),
    };

    let (bytes, info) = build("w4_contains", src);
    assert!(
        matches!(info.shape, relon_codegen_llvm::EmittedEntryShape::Buffer),
        "w4 expected Buffer shape (const-pool body off the fast entry), got {:?}",
        info.shape
    );
    let in_record = pack_single_int(&info, n);
    let out = run_buffer(&bytes, "relon_parity_w4_contains", &info, &in_record);
    match out.get("value") {
        Some(Decoded::Int(v)) => assert_eq!(*v, want, "w4 wasm Int {v} != native oracle {want}"),
        other => panic!("w4 decoded {other:?}"),
    }
}

/// W7 production Dict — `#internal fib: (k) => ... fib(...)` first-class
/// **recursive** closure. The body lifts `fib` to an internal let-bound
/// closure handle that captures itself (`fib(k-1) + fib(k-2)`) and the
/// host-visible `result` field calls it. The IR lowering populates the
/// module's `closure_table` with the single `fib` lambda; the object-emit
/// path routes through `emit_module_funcs`, which declares every lambda
/// up-front (forward reference for the self-call) and emits each lambda
/// body — so the recursive closure lowers correctly for static wasm32
/// emit and runs in wasmtime to the same value as the native LLVM, the
/// cranelift JIT, and the tree-walk oracle (four-way bit-equal).
///
/// This was the P1-P3 honest-gap guard
/// (`w7_recursive_closure_dict_is_unsupported_on_wasm32_emit`): the
/// failure was a fast-path mis-route in `emit_object_for_target` — a
/// closure module whose `#main(Int n) -> { result: Int }` schema matched
/// the fast `(i64..) -> i64` envelope was emitted as a fast-only entry
/// with an empty `closure_fn_table` (the fast-only branch never declares
/// nor emits the lambda bodies). The fix forces the buffer entry whenever
/// the module declares any lambda, mirroring how the in-process MCJIT
/// path already emits the buffer module first. fib(13) = 233.
#[test]
fn w7_recursive_closure_dict_aligns_four_ways_via_wasmtime() {
    let src = "#main(Int n) -> Dict\n{\n#internal\n\
               fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\nresult: fib(n)\n}";
    let n = 13i64;
    let want = 233i64; // fib(13)

    // Tree-walk gold standard.
    {
        use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
        use std::sync::Arc;
        let node = relon_parser::parse_document(src).expect("parse w7");
        let analyzed = Arc::new(relon_analyzer::analyze(&node));
        let mut ctx = Context::new()
            .with_root(node)
            .with_analyzed(Arc::clone(&analyzed));
        TreeWalkEvaluator::prepare_in_place(&mut ctx);
        let walker = TreeWalkEvaluator::new(Arc::new(ctx));
        let scope = Arc::new(Scope::default());
        let out = walker
            .run_main(&scope, HashMap::from([("n".to_string(), Value::Int(n))]))
            .expect("tree-walk run_main");
        assert_eq!(w7_result(&out), want, "tree-walk fib(13) != 233");
    }

    // Native LLVM oracle (in-process MCJIT).
    let native = w7_result(&native_run(
        src,
        HashMap::from([("n".to_string(), Value::Int(n))]),
    ));
    assert_eq!(native, want, "native LLVM fib(13) != 233");

    // Cranelift JIT.
    {
        use relon_codegen_cranelift::AotEvaluator;
        let cl = AotEvaluator::from_source_with_options(src, &opts()).expect("cranelift compiles");
        let out = cl
            .run_main(HashMap::from([("n".to_string(), Value::Int(n))]))
            .expect("cranelift run_main");
        assert_eq!(w7_result(&out), want, "cranelift fib(13) != 233");
    }

    // wasm32 object-emit → wasm-ld → wasmtime.
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping w7 wasm leg");
        return;
    }
    let (bytes, info) = build("w7", src);
    assert!(
        matches!(info.shape, relon_codegen_llvm::EmittedEntryShape::Buffer),
        "w7 expected Buffer shape (closure module forced off the fast path), got {:?}",
        info.shape
    );
    let in_record = pack_single_int(&info, n);
    let out = run_buffer(&bytes, "relon_parity_w7", &info, &in_record);
    assert_eq!(
        out.get("result"),
        Some(&Decoded::Int(want)),
        "w7 wasm recursive-closure fib(13) != native oracle"
    );
}

/// Pull the `result` Int out of a tree-walk / cranelift / native Dict.
fn w7_result(v: &Value) -> i64 {
    match v {
        Value::Dict(d) => match d.map.get("result") {
            Some(Value::Int(n)) => *n,
            other => panic!("w7 result not Int: {other:?}"),
        },
        other => panic!("w7 expected Dict, got {other:?}"),
    }
}

/// W5-P4 — the full production w5 Dict now compiles end-to-end to wasm32
/// and matches the native LLVM oracle. The body binds `#internal d` (an
/// `Op::ConstDict` arena record), `#internal keys` (an
/// `Op::ConstListString` arena record), and a host-visible
/// `result: list.sum(range(n).map((i) => d[keys[i % 10]]))`. The map loop
/// is inlined (`emit_range_pipeline_loop`); its body resolves `keys[i%10]`
/// (a `ListString` int-index → String handle) then `d[<String>]` (the
/// IR-lowered dict-probe linear scan + byte compare) entirely through the
/// captured `d` / `keys` let-bindings — no new wasm import beyond the
/// standard libc symbols the harness already provides. n=10 sums
/// `d["a"]..d["j"]` = 1+2+…+10 = 55. This was the P1-P3 scope-cut guard
/// (`w5_nested_dict_field_is_unsupported_on_wasm32_emit`); P4 flips it to
/// a real value assertion against the native oracle.
#[test]
fn w5_full_dict_probe_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping w5-full dict-probe");
        return;
    }
    let src = "#import list from \"std/list\"\n#main(Int n) -> Dict\n{\n#internal\n\
               d: { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 },\n#internal\n\
               keys: [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\", \"j\"],\n\
               result: list.sum(range(n).map((i) => d[keys[i % 10]]))\n}";
    let n = 10i64;
    // Native LLVM oracle (non-strict opts — the inline map body's
    // `d[keys[i%10]]` is not statically derivable by the strict analyzer,
    // matching how w2 / w5_inline run through `opts()`).
    let want = match LlvmAotEvaluator::from_source_with_options(src, &opts())
        .expect("native w5 from_source")
        .run_main(HashMap::from([("n".to_string(), Value::Int(n))]))
        .expect("native w5 run_main")
    {
        Value::Dict(d) => match d.map.get("result") {
            Some(Value::Int(v)) => *v,
            other => panic!("native result not Int: {other:?}"),
        },
        other => panic!("native expected Dict, got {other:?}"),
    };
    assert_eq!(want, 55, "native oracle: full w5 sum == 55");

    let (bytes, info) = build("w5_full", src);
    assert!(
        matches!(info.shape, relon_codegen_llvm::EmittedEntryShape::Buffer),
        "w5-full expected Buffer shape, got {:?}",
        info.shape
    );
    let in_record = pack_single_int(&info, n);
    let out = run_buffer(&bytes, "relon_parity_w5_full", &info, &in_record);
    assert_eq!(
        out.get("result"),
        Some(&Decoded::Int(want)),
        "w5-full wasm dict-probe != native oracle"
    );
}
