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
    ListFloat(Vec<f64>),
    ListString(Vec<String>),
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
    let (out, out_ptr) = run_buffer_raw(bytes, entry, info, in_record);

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
            EmittedFieldType::Unit => Decoded::Int(0),
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

/// Run a buffer-protocol entry in wasmtime and return the raw out region
/// (`out_buf`: fixed root + tail) together with the arena-relative
/// `out_ptr`. Asserts a non-negative `bytes_written` (the fixed-area /
/// tail-cursor return ABI). This is the shared run core behind
/// [`run_buffer`]; tests that decode return shapes the binding-marshalling
/// table doesn't yet erase into [`EmittedFieldType`] (e.g. a `List<Float>`
/// or a static `List<String>` copied into out_buf, whose `return_fields`
/// come back empty on the wasm32 target) drive this directly and decode
/// the slot themselves via the `decode_list_*` helpers.
fn run_buffer_raw(
    bytes: &[u8],
    entry: &str,
    info: &EmitObjectInfo,
    in_record: &[u8],
) -> (Vec<u8>, u32) {
    let (arena, _arena_abs, out_ptr, bytes_written) =
        run_buffer_arena(bytes, entry, info, in_record);
    assert!(
        bytes_written >= 0,
        "run_buffer_raw: negative bytes_written {bytes_written} (in-place sentinel return; \
         use run_buffer_arena + the sentinel path)"
    );
    // `arena` is read from the arena base, so `arena[i]` is arena-relative
    // offset `i`. Slice the out region (arena-relative offset `out_ptr` to
    // the arena end) so existing decoders index it relative to its start,
    // rebasing arena-absolute slots by `- out_ptr`.
    let out = arena[out_ptr as usize..].to_vec();
    (out, out_ptr)
}

/// Run a buffer-protocol entry in wasmtime and return the WHOLE arena
/// (`[const | in | out | scratch]`), the arena's absolute linear-memory
/// base, the arena-relative `out_ptr`, and the raw `bytes_written` the
/// entry returned. A **negative** `bytes_written` is the in-place
/// region-walk sentinel `-(root_abs + 1)` (the entry returns a value that
/// lives in `scratch` / `in` rather than a copy in `out_buf`), so the
/// caller must read it from the full arena at `root_abs`; see
/// `decode_list_string_return` / `decode_list_float_return`.
fn run_buffer_arena(
    bytes: &[u8],
    entry: &str,
    info: &EmitObjectInfo,
    in_record: &[u8],
) -> (Vec<u8>, u32, u32, i32) {
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

    // Read the whole arena (`[const | in | out | scratch]`). The out-buf
    // (positive `bytes_written`) decoders slice from `arena_abs + out_ptr`;
    // the in-place-sentinel (negative) decoders index the whole arena at
    // the arena-absolute `root_abs`, exactly as the native host does.
    let mut arena = vec![0u8; arena_bytes as usize];
    memory
        .read(&store, arena_abs as usize, &mut arena)
        .expect("read arena");

    (arena, arena_abs, out_ptr, bytes_written)
}

/// Read a little-endian `u32` from `buf` at `at`, panicking (verifier-
/// style) if the 4-byte window runs past the decoded region.
fn read_u32_checked(buf: &[u8], at: usize, what: &str) -> usize {
    assert!(
        at + 4 <= buf.len(),
        "{what}: u32 read at {at} exceeds out region len {}",
        buf.len()
    );
    u32::from_le_bytes(buf[at..at + 4].try_into().unwrap()) as usize
}

/// Rebase an arena-absolute pointer `abs` into an index in a `buf` slice
/// whose index 0 sits at arena offset `base`. Verifier-style: panics if
/// the pointer falls below the slice start.
fn rebase(abs: usize, base: usize, what: &str) -> usize {
    assert!(abs >= base, "{what}: ptr {abs} below slice base {base}");
    abs - base
}

/// Decode a `List<Float>` pointer-array record whose header sits at
/// arena-absolute `header_abs`. Layout is byte-identical to `List<Int>`
/// (the `ListFloat` arm of `emit_tail_record_from_absolute` / the
/// const-pool `add_list_float` blob): a `[len: u32 LE][pad to 8][f64 LE …]`
/// record, with the f64 elements on an 8-byte boundary. `buf` is the
/// decoded region and `base` is the arena offset of `buf[0]` (so an
/// arena-absolute pointer `p` indexes at `p - base`). Verifier-style
/// bounds checks panic rather than read out of range.
fn decode_list_float_at(buf: &[u8], base: usize, header_abs: usize) -> Vec<f64> {
    let record_start = rebase(header_abs, base, "list<float> header");
    let count = read_u32_checked(buf, record_start, "list<float> count");
    // Payload pads the start up to 8 (tail_alignment), same as List<Int>.
    let payload = (record_start + 4 + 7) & !7usize;
    assert!(
        payload + count * 8 <= buf.len(),
        "list<float> payload [{payload}..{}] exceeds region {}",
        payload + count * 8,
        buf.len()
    );
    let mut v = Vec::with_capacity(count);
    let mut cur = payload;
    for _ in 0..count {
        v.push(f64::from_le_bytes(buf[cur..cur + 8].try_into().unwrap()));
        cur += 8;
    }
    v
}

/// Decode a `List<String>` pointer-array record whose header sits at
/// arena-absolute `header_abs`. The wire format (produced by
/// `copy_list_string_block` on the out-buf path and by the in-place
/// region-walk path, byte-identical to `BufferBuilder::write_list_string`)
/// is a header `[count: u32 LE][off_0 …off_(count-1)]`, where each `off_i`
/// is an **arena-absolute** offset to a `[len: u32 LE][utf8 bytes]` String
/// record.
/// `buf` is the decoded region and `base` is the arena offset of `buf[0]`.
/// Verifier-style bounds checks panic on any pointer / length that escapes
/// the region; UTF-8 is validated.
fn decode_list_string_at(buf: &[u8], base: usize, header_abs: usize) -> Vec<String> {
    let header_start = rebase(header_abs, base, "list<string> header");
    let count = read_u32_checked(buf, header_start, "list<string> count");
    let entries_start = header_start + 4;
    assert!(
        entries_start + count * 4 <= buf.len(),
        "list<string> entry array [{entries_start}..{}] exceeds region {}",
        entries_start + count * 4,
        buf.len()
    );
    let mut v = Vec::with_capacity(count);
    for i in 0..count {
        let entry_abs = read_u32_checked(buf, entries_start + i * 4, "list<string> entry ptr");
        let rec_start = rebase(entry_abs, base, "list<string> entry");
        let len = read_u32_checked(buf, rec_start, "list<string> entry len");
        let payload = rec_start + 4;
        assert!(
            payload + len <= buf.len(),
            "list<string> entry[{i}] payload [{payload}..{}] exceeds region {}",
            payload + len,
            buf.len()
        );
        let s = std::str::from_utf8(&buf[payload..payload + len])
            .expect("list<string> entry utf8")
            .to_string();
        v.push(s);
    }
    v
}

/// Drive a `List<Float>` return and decode it byte-equal off the wasm
/// leg, transparently handling both return ABIs: a **non-negative**
/// `bytes_written` (the value was copied into out_buf, so the fixed slot
/// at offset 0 points to the record there) and the **negative** in-place
/// sentinel `-(root_abs + 1)` (the record lives in `scratch` / `in`; we
/// read it from the whole arena at `root_abs`). Mirrors the native host's
/// `decode_buffer_return` split.
fn decode_list_float_return(
    bytes: &[u8],
    entry: &str,
    info: &EmitObjectInfo,
    in_record: &[u8],
) -> Vec<f64> {
    let (arena, _arena_abs, out_ptr, bw) = run_buffer_arena(bytes, entry, info, in_record);
    // Both ABIs resolve to an arena-absolute header offset, decoded against
    // the WHOLE arena (base 0). For a negative `bw` the header offset is
    // the `root_abs` recovered from the sentinel; for a non-negative `bw`
    // it is the arena-absolute pointer in the fixed-area return slot at
    // arena-relative offset `out_ptr`.
    let header_abs = if bw < 0 {
        (-(bw as i64) - 1) as usize
    } else {
        // `arena` is read from the arena base, so the fixed-area return
        // slot sits at arena-relative offset `out_ptr`.
        read_u32_checked(&arena, out_ptr as usize, "list<float> slot")
    };
    decode_list_float_at(&arena, 0, header_abs)
}

/// Drive a `List<String>` return and decode it byte-equal off the wasm
/// leg, handling both the out-buf (non-negative `bytes_written`) and
/// in-place-sentinel (negative) return ABIs, mirroring the native host.
fn decode_list_string_return(
    bytes: &[u8],
    entry: &str,
    info: &EmitObjectInfo,
    in_record: &[u8],
) -> Vec<String> {
    let (arena, _arena_abs, out_ptr, bw) = run_buffer_arena(bytes, entry, info, in_record);
    let header_abs = if bw < 0 {
        (-(bw as i64) - 1) as usize
    } else {
        read_u32_checked(&arena, out_ptr as usize, "list<string> slot")
    };
    decode_list_string_at(&arena, 0, header_abs)
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
    // Wave R2 — pipe operator. `range(n) | list.sum` is a pure static
    // desugar of `w1_listsum_range`; the pipe lowering prepends the LHS
    // as the call's first positional arg, folding into the same FastInt
    // accumulator loop. Validates the desugar on the LLVM native + wasm
    // legs against the native oracle.
    Fast {
        name: "r2_pipe_range_sum",
        src: "#import list from \"std/list\"\n#main(Int n) -> Int\nrange(n) | list.sum",
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
    // Wave R5 — static match arm selection with a NON-literal selected
    // body (`n * 2`). The `Int` scrutinee statically picks the first arm;
    // the wildcard never fires. The body lowers to real arithmetic IR
    // (not a folded constant), so the FastInt entry proves the selected
    // body is general codegen on the native + wasm legs.
    Fast {
        name: "r5_match_int_body_arith",
        src: "#main(Int n) -> Int\nn match { Int: n * 2, _: 0 }",
        args: &[7],
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

/// Wave R7: pack a single `Float` `#main` param into its declared
/// fixed-area slot as little-endian f64 bits (the buffer protocol rides
/// f64 as raw bits, same as the operand stack).
fn pack_single_float(info: &EmitObjectInfo, value: f64) -> Vec<u8> {
    let mut rec = vec![0u8; info.main_root_size as usize];
    let off = info.main_fields[0].offset as usize;
    rec[off..off + 8].copy_from_slice(&value.to_le_bytes());
    rec
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

/// Wave R4 — static const-fold of `type(n)` over an `Int` param →
/// the constant `"Int"` String. Proves the const-string the lowering
/// pushes (after evaluating + discarding the argument) round-trips
/// byte-equal on the wasm32 leg against the native LLVM oracle (itself
/// bit-aligned to tree-walk + cranelift).
#[test]
fn r4_type_int_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping r4 type(int)");
        return;
    }
    let src = "#main(Int n) -> String\ntype(n)";
    let n = 7i64;
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(n))])) {
        Value::String(s) => s,
        other => panic!("native expected String, got {other:?}"),
    };
    assert_eq!(want, "Int", "native type(int) oracle drifted");
    let (bytes, info) = build("r4_type_int", src);
    let in_record = pack_single_int(&info, n);
    let out = run_buffer(&bytes, "relon_parity_r4_type_int", &info, &in_record);
    match out.get("value") {
        Some(Decoded::Str(s)) => assert_eq!(*s, want, "r4 type(int) wasm != native"),
        other => panic!("r4 type(int) decoded {other:?}"),
    }
}

/// Wave R4 — coarsening: `type(range(n))` over a `List<Int>` argument
/// folds to the constant `"List"` (every concrete list element tag maps
/// to the same name). The `range(n)` is still materialised + discarded
/// for trap/ordering parity; only the constant name survives. Verified
/// byte-equal on the wasm32 leg against native LLVM.
#[test]
fn r4_type_list_coarsen_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping r4 type(list)");
        return;
    }
    let src = "#main(Int n) -> String\ntype(range(n))";
    let n = 3i64;
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(n))])) {
        Value::String(s) => s,
        other => panic!("native expected String, got {other:?}"),
    };
    assert_eq!(want, "List", "native type(list) coarsen oracle drifted");
    let (bytes, info) = build("r4_type_list", src);
    let in_record = pack_single_int(&info, n);
    let out = run_buffer(&bytes, "relon_parity_r4_type_list", &info, &in_record);
    match out.get("value") {
        Some(Decoded::Str(s)) => assert_eq!(*s, want, "r4 type(list) wasm != native"),
        other => panic!("r4 type(list) decoded {other:?}"),
    }
}

/// Wave R5 — static match arm selection. An `Int` scrutinee statically
/// satisfies the `Int` arm, so the lowering selects `"int"` at compile
/// time (the wildcard never fires). The scrutinee is still evaluated +
/// discarded for trap / ordering parity, then the selected arm's String
/// body is the result. Verified byte-equal on the wasm32 leg against the
/// native LLVM oracle (itself bit-aligned to tree-walk + cranelift).
#[test]
fn r5_match_int_arm_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping r5 match int arm");
        return;
    }
    let src = "#main(Int n) -> String\nn match { Int: \"int\", _: \"other\" }";
    let n = 5i64;
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(n))])) {
        Value::String(s) => s,
        other => panic!("native expected String, got {other:?}"),
    };
    assert_eq!(want, "int", "native r5 match int-arm oracle drifted");
    let (bytes, info) = build("r5_match_int_arm", src);
    let in_record = pack_single_int(&info, n);
    let out = run_buffer(&bytes, "relon_parity_r5_match_int_arm", &info, &in_record);
    match out.get("value") {
        Some(Decoded::Str(s)) => assert_eq!(*s, want, "r5 match int-arm wasm != native"),
        other => panic!("r5 match int-arm decoded {other:?}"),
    }
}

/// Wave R5 — a builtin-scalar pattern naming a DIFFERENT scalar than the
/// static type (`Float` arm vs an `Int` scrutinee) provably never
/// matches, so the wildcard wins and the lowering selects `"other"`.
/// Proves the "arm provably never matches → skip" decision agrees with
/// the runtime `check_type` on the native + wasm legs.
#[test]
fn r5_match_scalar_mismatch_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping r5 match scalar mismatch");
        return;
    }
    let src = "#main(Int n) -> String\nn match { Float: \"f\", _: \"other\" }";
    let n = 9i64;
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(n))])) {
        Value::String(s) => s,
        other => panic!("native expected String, got {other:?}"),
    };
    assert_eq!(
        want, "other",
        "native r5 match scalar-mismatch oracle drifted"
    );
    let (bytes, info) = build("r5_match_scalar_mismatch", src);
    let in_record = pack_single_int(&info, n);
    let out = run_buffer(
        &bytes,
        "relon_parity_r5_match_scalar_mismatch",
        &info,
        &in_record,
    );
    match out.get("value") {
        Some(Decoded::Str(s)) => assert_eq!(*s, want, "r5 match scalar-mismatch wasm != native"),
        other => panic!("r5 match scalar-mismatch decoded {other:?}"),
    }
}

/// Wave R2 — f-string with an Int interpolation (`f"n=${n}"`) → String.
/// Exercises `Op::IntToStr` + `Op::StrConcatN` on the LLVM native and
/// wasm32 legs, checked byte-exact against the native `run_main` oracle
/// (itself bit-aligned to tree-walk + cranelift).
#[test]
fn fstring_int_interp_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping f-string int interp");
        return;
    }
    let src = "#main(Int n) -> String\nf\"n=${n}\"";
    let n = 42i64;
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(n))])) {
        Value::String(s) => s,
        other => panic!("native expected String, got {other:?}"),
    };
    let (bytes, info) = build("fstring_int", src);
    let in_record = pack_single_int(&info, n);
    let out = run_buffer(&bytes, "relon_parity_fstring_int", &info, &in_record);
    match out.get("value") {
        Some(Decoded::Str(s)) => assert_eq!(*s, want, "f-string int wasm != native"),
        other => panic!("f-string int decoded {other:?}"),
    }
}

/// Wave R2 — f-string mixing literal parts and an Int interpolation
/// (`f"a${n}b${n}c"`) → String. Two `Op::IntToStr` records feeding a
/// 5-operand `Op::StrConcatN`; the record alignment fix matters here.
#[test]
fn fstring_mixed_parts_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping f-string mixed parts");
        return;
    }
    let src = "#main(Int n) -> String\nf\"a${n}b${n}c\"";
    let n = 7i64;
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(n))])) {
        Value::String(s) => s,
        other => panic!("native expected String, got {other:?}"),
    };
    let (bytes, info) = build("fstring_mixed", src);
    let in_record = pack_single_int(&info, n);
    let out = run_buffer(&bytes, "relon_parity_fstring_mixed", &info, &in_record);
    match out.get("value") {
        Some(Decoded::Str(s)) => assert_eq!(*s, want, "f-string mixed wasm != native"),
        other => panic!("f-string mixed decoded {other:?}"),
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

/// Wave R3 helper: build a `#main(Int n) -> List<Int>` source, take the
/// native LLVM `run_main` as the oracle (itself bit-aligned to the
/// tree-walk and cranelift backends), then assert the wasm32 leg decodes
/// byte-equal. When
/// `wasm-ld` is unavailable the wasm leg is a recorded skip, but the
/// native LLVM compile + run still executes so the LLVM-native lowering
/// of the construct is proven on every invocation.
fn r3_list_parity(test_name: &str, entry: &str, src: &str, n: i64) {
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(n))])) {
        Value::List(items) => items
            .iter()
            .map(|v| match v {
                Value::Int(i) => *i,
                other => panic!("[{test_name}] non-int list element {other:?}"),
            })
            .collect::<Vec<_>>(),
        other => panic!("[{test_name}] native expected List, got {other:?}"),
    };
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping {test_name} wasm leg");
        return;
    }
    let (bytes, info) = build(test_name, src);
    assert!(matches!(
        info.shape,
        relon_codegen_llvm::EmittedEntryShape::Buffer
    ));
    let in_record = pack_single_int(&info, n);
    let out = run_buffer(&bytes, entry, &info, &in_record);
    match out.get("value") {
        Some(Decoded::ListInt(v)) => assert_eq!(*v, want, "[{test_name}] wasm List<Int> != native"),
        other => panic!("[{test_name}] decoded {other:?}"),
    }
}

/// Wave R3 — `range(n)` as a materialised `List<Int>` value.
#[test]
fn r3_range_value_aligns_native_via_wasmtime() {
    r3_list_parity(
        "r3_range_value",
        "relon_parity_r3_range_value",
        "#main(Int n) -> List<Int>\nrange(n)",
        5,
    );
}

/// Wave R3 — `range(n).map((x) => x*x)` (general map closure → List<Int>).
#[test]
fn r3_range_map_aligns_native_via_wasmtime() {
    r3_list_parity(
        "r3_range_map",
        "relon_parity_r3_range_map",
        "#main(Int n) -> List<Int>\nrange(n).map((Int x) => x * x)",
        5,
    );
}

/// Wave R12 — list spread over a single RUNTIME source `[100, ...range(n), 200]`:
/// the source length is only known at runtime, so the materialiser allocs a
/// scratch record, stores the static scalars inline, and `memory.copy`-s the
/// source payload in place. Proves the llvm-native == wasm leg for the
/// runtime-source spread (tree-walk == cranelift proven in the corpus).
#[test]
fn r12_list_spread_runtime_src_aligns_native_via_wasmtime() {
    r3_list_parity(
        "r12_list_spread_runtime_src",
        "relon_parity_r12_list_spread_runtime_src",
        "#main(Int n) -> List<Int>\n[100, ...range(n), 200]",
        3,
    );
}

/// Wave R12 — runtime-source spread with a leading scalar only `[7, ...range(n)]`.
#[test]
fn r12_list_spread_runtime_src_prefix_aligns_native_via_wasmtime() {
    r3_list_parity(
        "r12_list_spread_runtime_src_prefix",
        "relon_parity_r12_list_spread_runtime_src_prefix",
        "#main(Int n) -> List<Int>\n[7, ...range(n)]",
        4,
    );
}

/// Wave R12 — runtime-source spread, empty source edge `[100, ...range(0), 200]`
/// (memory.copy of 0 bytes). Exercised at `n = 0`.
#[test]
fn r12_list_spread_runtime_src_empty_aligns_native_via_wasmtime() {
    r3_list_parity(
        "r12_list_spread_runtime_src_empty",
        "relon_parity_r12_list_spread_runtime_src_empty",
        "#main(Int n) -> List<Int>\n[100, ...range(n), 200]",
        0,
    );
}

// ---------------------------------------------------------------------
// `List<Float>` / `List<String>` element-list returns. Until now the
// wasm parity harness only decoded `List<Int>`; every feature that
// produces a `List<Float>` (Float math, comprehensions, Int->Float map,
// Float spread) or a `List<String>` (`split`, String-map, String
// comprehension) was only verified on the `List<Int>`-shaped corpus, so
// the wasm leg of these returns was never byte-decoded. The
// `decode_list_float` / `decode_list_string` helpers close that gap.
//
// These returns map to `None` in `emitted_field_type_for` (the
// binding-marshalling table the `Native` target enforces), so on the
// wasm32 target `return_fields` comes back empty (the wasm host walks the
// full `BufferSchema`, not these erased descriptors — see the
// `descriptors_strict` comment in `evaluator.rs`). The single
// `Ret { value: T }` slot therefore sits at fixed-area offset 0, which we
// decode directly out of the raw out region. Native LLVM-AOT `run_main`
// is the oracle (itself bit-aligned to tree-walk + cranelift).
// ---------------------------------------------------------------------

/// Build `src` (single `Int n` param returning `List<Float>`), take the
/// native LLVM `run_main` list as the oracle, then assert the wasm32 leg
/// decodes byte-equal (IEEE-754 bit pattern per element, so NaN / ±0.0
/// edges stay exact). The return slot is the single `value` field at
/// fixed-area offset 0.
fn list_float_parity(name: &str, src: &str, n: i64) {
    let want: Vec<f64> = match native_run(src, HashMap::from([("n".to_string(), Value::Int(n))])) {
        Value::List(items) => items
            .iter()
            .map(|v| match v {
                Value::Float(f) => f.into_inner(),
                Value::Int(i) => *i as f64,
                other => panic!("[{name}] non-float list element {other:?}"),
            })
            .collect(),
        other => panic!("[{name}] native expected List, got {other:?}"),
    };
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping {name} wasm leg");
        return;
    }
    let (bytes, info) = build(name, src);
    assert!(matches!(
        info.shape,
        relon_codegen_llvm::EmittedEntryShape::Buffer
    ));
    assert!(
        info.return_has_tail,
        "[{name}] List<Float> return needs tail"
    );
    let in_record = pack_single_int(&info, n);
    let got = decode_list_float_return(&bytes, &format!("relon_parity_{name}"), &info, &in_record);
    let decoded = Decoded::ListFloat(got);
    let Decoded::ListFloat(got) = &decoded else {
        unreachable!()
    };
    assert_eq!(
        got.len(),
        want.len(),
        "[{name}] List<Float> length wasm != native"
    );
    // Compare the IEEE-754 bit pattern per element so NaN / ±0.0 edges
    // stay exact (plain f64 `PartialEq` would gloss over them).
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        assert_eq!(
            g.to_bits(),
            w.to_bits(),
            "[{name}] List<Float>[{i}] wasm {g} bits != native {w}"
        );
    }
}

/// Build `src` returning `List<String>`, take the native LLVM `run_main`
/// list as the oracle, then assert the wasm32 leg decodes byte-equal. The
/// `#main` param defaults to `Int n`; a const driver passes `n` straight
/// through. The return slot is the single `value` field at offset 0.
fn list_string_parity(name: &str, src: &str, n: i64) {
    let want: Vec<String> = match native_run(src, HashMap::from([("n".to_string(), Value::Int(n))]))
    {
        Value::List(items) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.to_string(),
                other => panic!("[{name}] non-string list element {other:?}"),
            })
            .collect(),
        other => panic!("[{name}] native expected List, got {other:?}"),
    };
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping {name} wasm leg");
        return;
    }
    let (bytes, info) = build(name, src);
    assert!(matches!(
        info.shape,
        relon_codegen_llvm::EmittedEntryShape::Buffer
    ));
    assert!(
        info.return_has_tail,
        "[{name}] List<String> return needs tail"
    );
    let in_record = pack_single_int(&info, n);
    let got = decode_list_string_return(&bytes, &format!("relon_parity_{name}"), &info, &in_record);
    assert_eq!(
        Decoded::ListString(got),
        Decoded::ListString(want),
        "[{name}] List<String> wasm != native"
    );
}

// ----- List<Float> features -----

/// Const `List<Float>` literal return.
#[test]
fn lf_const_list_aligns_native_via_wasmtime() {
    list_float_parity(
        "lf_const",
        "#main(Int n) -> List<Float>\n[1.5, 2.5, 3.5]",
        1,
    );
}

/// Float math producing a list: `range(n).map((Int x) => x * 2.0)`
/// (Int->Float map, F64 multiply per element).
#[test]
fn lf_range_map_scale_aligns_native_via_wasmtime() {
    list_float_parity(
        "lf_range_map_scale",
        "#import list from \"std/list\"\n#main(Int n) -> List<Float>\n\
         range(n).map((Int x) => x * 2.0)",
        5,
    );
}

/// Int->Float map via the free `_list_map` intrinsic form.
#[test]
fn lf_list_map_free_aligns_native_via_wasmtime() {
    list_float_parity(
        "lf_list_map_free",
        "#main(Int n) -> List<Float>\n_list_map(range(n), (Int x) => x * 1.0)",
        4,
    );
}

/// Float comprehension `[x * 0.5 for x in range(n)]` (desugars onto map).
#[test]
fn lf_comprehension_aligns_native_via_wasmtime() {
    list_float_parity(
        "lf_comprehension",
        "#main(Int n) -> List<Float>\n[x * 0.5 for x in range(n)]",
        4,
    );
}

/// Float spread `[...xs, 1.5]` over a const `List<Float>` source.
#[test]
fn lf_spread_aligns_native_via_wasmtime() {
    list_float_parity(
        "lf_spread",
        "#main(Int n) -> List<Float>\n[...[10.0, 20.0], 1.5]",
        1,
    );
}

/// Empty Float list edge (`range(0).map(...)` → `[]`).
#[test]
fn lf_empty_aligns_native_via_wasmtime() {
    list_float_parity(
        "lf_empty",
        "#main(Int n) -> List<Float>\nrange(n).map((Int x) => x * 2.0)",
        0,
    );
}

/// Single-element Float list edge.
#[test]
fn lf_single_aligns_native_via_wasmtime() {
    list_float_parity(
        "lf_single",
        "#main(Int n) -> List<Float>\nrange(n).map((Int x) => x * 2.0)",
        1,
    );
}

// ----- List<String> features -----

/// Const `List<String>` literal return.
#[test]
fn ls_const_list_aligns_native_via_wasmtime() {
    list_string_parity(
        "ls_const",
        "#main(Int n) -> List<String>\n[\"a\", \"bb\", \"ccc\"]",
        1,
    );
}

/// `split` producing a `List<String>`.
#[test]
fn ls_split_aligns_native_via_wasmtime() {
    list_string_parity(
        "ls_split",
        "#main(Int n) -> List<String>\n\"a,b,c,dd\".split(\",\")",
        1,
    );
}

/// `split` with a trailing empty segment (`"a,,c,"` → `["a","","c",""]`).
#[test]
fn ls_split_empty_segments_aligns_native_via_wasmtime() {
    list_string_parity(
        "ls_split_empty",
        "#main(Int n) -> List<String>\n\"a,,c,\".split(\",\")",
        1,
    );
}

/// `List<String>`-producing map: each element maps to a const String.
#[test]
fn ls_map_const_aligns_native_via_wasmtime() {
    list_string_parity(
        "ls_map_const",
        "#import list from \"std/list\"\n#main(Int n) -> List<String>\n\
         range(n).map((Int x) => \"item\")",
        3,
    );
}

/// String comprehension `["s" for x in range(n)]` (desugars onto map).
#[test]
fn ls_comprehension_aligns_native_via_wasmtime() {
    list_string_parity(
        "ls_comprehension",
        "#main(Int n) -> List<String>\n[\"s\" for x in range(n)]",
        4,
    );
}

/// `List<String>` carrying an empty string, a multi-byte (CJK) string,
/// and a long string — exercises per-entry length / UTF-8 / alignment
/// edges in the pointer-array decode. The CJK literal is written as
/// escapes (U+4E2D U+6587) so the source stays ASCII while the runtime
/// String is multi-byte UTF-8.
#[test]
fn ls_mixed_widths_aligns_native_via_wasmtime() {
    list_string_parity(
        "ls_mixed_widths",
        "#main(Int n) -> List<String>\n\
         [\"\", \"\u{4e2d}\u{6587}\", \"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG\", \"x\"]",
        1,
    );
}

/// Single-element `List<String>` edge.
#[test]
fn ls_single_aligns_native_via_wasmtime() {
    list_string_parity("ls_single", "#main(Int n) -> List<String>\n[\"only\"]", 1);
}

/// Empty `List<String>` edge (`range(0)`-driven map → `[]`).
#[test]
fn ls_empty_aligns_native_via_wasmtime() {
    list_string_parity(
        "ls_empty",
        "#import list from \"std/list\"\n#main(Int n) -> List<String>\n\
         range(n).map((Int x) => \"item\")",
        0,
    );
}

/// Wave R3 — `range(n).filter((x) => x > 1)` (general filter predicate).
#[test]
fn r3_range_filter_aligns_native_via_wasmtime() {
    r3_list_parity(
        "r3_range_filter",
        "relon_parity_r3_range_filter",
        "#main(Int n) -> List<Int>\nrange(n).filter((Int x) => x > 1)",
        5,
    );
}

/// Wave R3 — `_list_map(range(n), f)` free-function intrinsic form.
#[test]
fn r3_list_map_free_aligns_native_via_wasmtime() {
    r3_list_parity(
        "r3_list_map_free",
        "relon_parity_r3_list_map_free",
        "#main(Int n) -> List<Int>\n_list_map(range(n), (Int x) => x + 100)",
        4,
    );
}

/// Wave R3 — comprehension `[x*2 for x in range(n)]` desugared onto map.
#[test]
fn r3_comprehension_aligns_native_via_wasmtime() {
    r3_list_parity(
        "r3_comprehension",
        "relon_parity_r3_comprehension",
        "#main(Int n) -> List<Int>\n[x * 2 for x in range(n)]",
        4,
    );
}

/// Wave R3 — comprehension with a guard `[x*10 for x in range(n) if x>1]`
/// desugared onto filter-then-map.
#[test]
fn r3_comprehension_if_aligns_native_via_wasmtime() {
    r3_list_parity(
        "r3_comprehension_if",
        "relon_parity_r3_comprehension_if",
        "#main(Int n) -> List<Int>\n[x * 10 for x in range(n) if x > 1]",
        5,
    );
}

/// Wave R3 — `_list_reduce(range(n), 0, (a, x) => a + x)` (fold to Int).
/// Returns a scalar `Int`, so the wasm leg decodes the fixed-area `value`
/// field directly.
#[test]
fn r3_list_reduce_free_aligns_native_via_wasmtime() {
    let src = "#import list from \"std/list\"\n#main(Int n) -> Int\n\
               _list_reduce(range(n), 0, (Int a, Int x) => a + x)";
    let n = 5i64;
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(n))])) {
        Value::Int(i) => i,
        other => panic!("[r3_list_reduce_free] native expected Int, got {other:?}"),
    };
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping r3_list_reduce_free wasm leg");
        return;
    }
    let (bytes, info) = build("r3_list_reduce_free", src);
    let in_record = pack_single_int(&info, n);
    let out = run_buffer(
        &bytes,
        "relon_parity_r3_list_reduce_free",
        &info,
        &in_record,
    );
    match out.get("value") {
        Some(Decoded::Int(v)) => assert_eq!(*v, want, "[r3_list_reduce_free] wasm Int != native"),
        other => panic!("[r3_list_reduce_free] decoded {other:?}"),
    }
}

/// Wave R3b — `List<Float>` fold to a scalar `Float`. The source is an
/// element-type-changing `_list_map(range(n), (Int x) => x * 1.0)`
/// (Int -> Float) reduced with an F64 accumulator through the bundled
/// `list_float_fold` body. A scalar `Float` return rides the buffer
/// protocol's fixed-area `value` slot, so the wasm leg decodes it
/// directly (the List<Float> return shape is not yet wasm-marshallable).
/// Native LLVM-AOT is the oracle.
#[test]
fn r3b_float_reduce_aligns_native_via_wasmtime() {
    let src = "#import list from \"std/list\"\n#main(Int n) -> Float\n\
               _list_reduce(_list_map(range(n), (Int x) => x * 1.0), 0.0, \
               (Float a, Float x) => a + x)";
    let n = 5i64;
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(n))])) {
        Value::Float(f) => f.0,
        other => panic!("[r3b_float_reduce] native expected Float, got {other:?}"),
    };
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping r3b_float_reduce wasm leg");
        return;
    }
    let (bytes, info) = build("r3b_float_reduce", src);
    let in_record = pack_single_int(&info, n);
    let out = run_buffer(&bytes, "relon_parity_r3b_float_reduce", &info, &in_record);
    match out.get("value") {
        Some(Decoded::Float(v)) => {
            assert_eq!(
                v.to_bits(),
                want.to_bits(),
                "[r3b_float_reduce] wasm Float != native"
            )
        }
        other => panic!("[r3b_float_reduce] decoded {other:?}"),
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

/// Wave R10 — backward static `&sibling` / `&root` field references in an
/// anon-Dict-return body run on wasm32. Later fields read earlier ones
/// via `&sibling.x` / `&root.x` (the entry dict IS the document root, so
/// both bases resolve to the same field); the reference lowers to the
/// same `Op::LetGet` over the source-ordered field-let graph that a bare
/// let read uses. Verified byte-equal on the wasm32 leg against the LLVM
/// native oracle (itself bit-aligned to tree-walk + cranelift via the
/// `r10_*` corpus). The reference-in-dict-field surface is `#relaxed`
/// (matching `examples/pricing.relon`).
#[test]
fn r10_sibling_root_backward_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping r10 sibling/root refs");
        return;
    }
    let src = "#relaxed\n#main(Int a, Int b) -> Dict\n\
               { x: a + b, y: &sibling.x * 2, z: &root.x + &sibling.y }";
    let (a, b) = (17i64, 5i64);
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
    let want = |k: &str| match dict.map.get(k) {
        Some(Value::Int(v)) => *v,
        other => panic!("{k} not Int: {other:?}"),
    };
    let (want_x, want_y, want_z) = (want("x"), want("y"), want("z"));
    // Oracle sanity: x = a+b = 22, y = x*2 = 44, z = x + y = 66.
    assert_eq!((want_x, want_y, want_z), (22, 44, 66), "r10 oracle drifted");

    let (bytes, info) = build("r10_sibling_root", src);
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
    let out = run_buffer(&bytes, "relon_parity_r10_sibling_root", &info, &in_record);
    assert_eq!(out.get("x"), Some(&Decoded::Int(want_x)), "r10 field x");
    assert_eq!(out.get("y"), Some(&Decoded::Int(want_y)), "r10 field y");
    assert_eq!(out.get("z"), Some(&Decoded::Int(want_z)), "r10 field z");
}

/// Wave R10b — the SAME backward `&sibling` / `&root` field-reference
/// program as `r10_sibling_root_backward_aligns_native_via_wasmtime`,
/// but in STRICT mode (no `#relaxed`). R10b taught the strict-mode
/// analyzer to derive a single-segment, backward `&sibling.<name>` /
/// entry-level `&root.<name>` reference's type from the target field's
/// static type, so the program now passes strict analysis. Lowering is
/// unchanged, so the wasm32 leg stays byte-equal to the LLVM native
/// oracle — proving the strict reference path runs four-way.
#[test]
fn r10b_strict_sibling_root_backward_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping r10b strict sibling/root refs");
        return;
    }
    let src = "#main(Int a, Int b) -> Dict\n\
               { x: a + b, y: &sibling.x * 2, z: &root.x + &sibling.y }";
    let (a, b) = (17i64, 5i64);
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
    let want = |k: &str| match dict.map.get(k) {
        Some(Value::Int(v)) => *v,
        other => panic!("{k} not Int: {other:?}"),
    };
    let (want_x, want_y, want_z) = (want("x"), want("y"), want("z"));
    // Oracle sanity: x = a+b = 22, y = x*2 = 44, z = x + y = 66.
    assert_eq!(
        (want_x, want_y, want_z),
        (22, 44, 66),
        "r10b oracle drifted"
    );

    let (bytes, info) = build("r10b_strict_sibling_root", src);
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
    let out = run_buffer(
        &bytes,
        "relon_parity_r10b_strict_sibling_root",
        &info,
        &in_record,
    );
    assert_eq!(out.get("x"), Some(&Decoded::Int(want_x)), "r10b field x");
    assert_eq!(out.get("y"), Some(&Decoded::Int(want_y)), "r10b field y");
    assert_eq!(out.get("z"), Some(&Decoded::Int(want_z)), "r10b field z");
}

/// Wave R13 — FORWARD `&sibling` / `&root` field references on the
/// anon-Dict-return path run on wasm32. R13 emits the dict fields in
/// topological order over their reference edges, so a field reading a
/// *later*-declared sibling (`y: &sibling.x` with `x` declared after)
/// has its target's let bound before the reference lowers. Covers the
/// scalar-Int forward-to-leaf shape plus a transitive forward chain over
/// a param-free component, a forward String reference, and a forward
/// List<Int> reference. Verified byte-equal on the wasm32 leg against the
/// LLVM native oracle (itself bit-aligned to tree-walk + cranelift via
/// the `r13_*` corpus). The reference-in-dict-field surface is `#relaxed`.
#[test]
fn r13_forward_ref_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping r13 forward refs");
        return;
    }
    // Forward-to-leaf: `y` reads later `x`, `x: a + b` is a non-ref leaf.
    let src = "#relaxed\n#main(Int a, Int b) -> Dict\n{ y: &sibling.x * 2, x: a + b }";
    let (a, b) = (40i64, 2i64);
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
    let want = |k: &str| match dict.map.get(k) {
        Some(Value::Int(v)) => *v,
        other => panic!("{k} not Int: {other:?}"),
    };
    let (want_x, want_y) = (want("x"), want("y"));
    // Oracle sanity: x = a+b = 42, y = x*2 = 84.
    assert_eq!((want_x, want_y), (42, 84), "r13 oracle drifted");

    let (bytes, info) = build("r13_forward_ref", src);
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
    let out = run_buffer(&bytes, "relon_parity_r13_forward_ref", &info, &in_record);
    assert_eq!(out.get("x"), Some(&Decoded::Int(want_x)), "r13 field x");
    assert_eq!(out.get("y"), Some(&Decoded::Int(want_y)), "r13 field y");
}

/// Wave R13 — forward String reference: `greeting: &sibling.name` reads a
/// later-declared `name: "hello"`. Confirms the topological emit order
/// binds the pointer-indirect String field's let before the forward
/// reference re-loads and tail-copies it, byte-equal wasm vs native.
#[test]
fn r13_forward_string_ref_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping r13 forward String ref");
        return;
    }
    let src = "#relaxed\n#main(Int a) -> Dict\n{ greeting: &sibling.name, name: \"hello\" }";
    let dict = match native_run(src, HashMap::from([("a".to_string(), Value::Int(1))])) {
        Value::Dict(d) => d,
        other => panic!("native expected Dict, got {other:?}"),
    };
    let want = |k: &str| match dict.map.get(k) {
        Some(Value::String(s)) => s.to_string(),
        other => panic!("{k} not String: {other:?}"),
    };
    let (want_g, want_n) = (want("greeting"), want("name"));
    assert_eq!((want_g.as_str(), want_n.as_str()), ("hello", "hello"));

    let (bytes, info) = build("r13_forward_string_ref", src);
    let mut in_record = vec![0u8; info.main_root_size as usize];
    for f in &info.main_fields {
        let off = f.offset as usize;
        if f.name == "a" {
            in_record[off..off + 8].copy_from_slice(&1i64.to_le_bytes());
        }
    }
    let out = run_buffer(
        &bytes,
        "relon_parity_r13_forward_string_ref",
        &info,
        &in_record,
    );
    assert_eq!(
        out.get("greeting"),
        Some(&Decoded::Str(want_g)),
        "r13 field greeting"
    );
    assert_eq!(
        out.get("name"),
        Some(&Decoded::Str(want_n)),
        "r13 field name"
    );
}

/// Wave R13 — forward List<Int> reference: `alias: &sibling.items` reads
/// a later-declared `items: [1, 2, 3]`. Confirms the topological emit
/// order binds the const-pool list field's let before the forward
/// reference re-loads its address and tail-copies the block, byte-equal
/// wasm vs native.
#[test]
fn r13_forward_list_ref_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping r13 forward List ref");
        return;
    }
    let src = "#relaxed\n#main(Int a) -> Dict\n{ alias: &sibling.items, items: [1, 2, 3] }";
    let dict = match native_run(src, HashMap::from([("a".to_string(), Value::Int(1))])) {
        Value::Dict(d) => d,
        other => panic!("native expected Dict, got {other:?}"),
    };
    let want = |k: &str| match dict.map.get(k) {
        Some(Value::List(items)) => items
            .iter()
            .map(|v| match v {
                Value::Int(i) => *i,
                other => panic!("list elem not Int: {other:?}"),
            })
            .collect::<Vec<_>>(),
        other => panic!("{k} not List: {other:?}"),
    };
    let (want_alias, want_items) = (want("alias"), want("items"));
    assert_eq!(want_alias, vec![1, 2, 3]);
    assert_eq!(want_items, vec![1, 2, 3]);

    let (bytes, info) = build("r13_forward_list_ref", src);
    let mut in_record = vec![0u8; info.main_root_size as usize];
    for f in &info.main_fields {
        let off = f.offset as usize;
        if f.name == "a" {
            in_record[off..off + 8].copy_from_slice(&1i64.to_le_bytes());
        }
    }
    let out = run_buffer(
        &bytes,
        "relon_parity_r13_forward_list_ref",
        &info,
        &in_record,
    );
    assert_eq!(
        out.get("alias"),
        Some(&Decoded::ListInt(want_alias)),
        "r13 field alias"
    );
    assert_eq!(
        out.get("items"),
        Some(&Decoded::ListInt(want_items)),
        "r13 field items"
    );
}

/// Wave R11 — field decorators on the anon-Dict-return path run on
/// wasm32. A decorated field `@deco(args) k: v` desugars to the call
/// `deco(v, args)` (value-first), and stacked decorators apply bottom-up
/// (`@a @b v ≡ a(b(v))`). The decorator resolves to an `#internal`
/// field-form function lifted to a closure let, so the desugared call
/// lowers through `Op::CallClosure`. Verified byte-equal on the wasm32
/// leg against the LLVM native oracle (itself bit-aligned to tree-walk +
/// cranelift via the `r11_*` corpus).
#[test]
fn r11_field_decorator_aligns_native_via_wasmtime() {
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping r11 field decorator");
        return;
    }
    // Stacked: @add(1) @mul(10) x: p ⇒ add(mul(p,10),1).
    let src = "#relaxed\n#main(Int p) -> Dict\n\
               { #internal\n add(v, n): v + n,\n #internal\n mul(v, n): v * n,\n \
               @add(1) @mul(10)\n x: p }";
    let p = 5i64;
    let dict = match native_run(src, HashMap::from([("p".to_string(), Value::Int(p))])) {
        Value::Dict(d) => d,
        other => panic!("native expected Dict, got {other:?}"),
    };
    let want_x = match dict.map.get("x") {
        Some(Value::Int(v)) => *v,
        other => panic!("x not Int: {other:?}"),
    };
    // Oracle sanity: add(mul(5,10),1) = 51.
    assert_eq!(want_x, 51, "r11 oracle drifted");

    let (bytes, info) = build("r11_field_decorator", src);
    let in_record = pack_single_int(&info, p);
    let out = run_buffer(
        &bytes,
        "relon_parity_r11_field_decorator",
        &info,
        &in_record,
    );
    assert_eq!(out.get("x"), Some(&Decoded::Int(want_x)), "r11 field x");
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

// ===========================================================
// Wave R7 — scalar-returning Float math stdlib on wasm32.
//
// `floor` / `ceil` / `round` (Float -> Int via the new
// `Op::F64Unary` + `Op::F64ToI64Sat` intrinsics) and `sqrt` / `abs`
// (Float -> Float) ride the buffer protocol's fixed-area scalar slot,
// so the wasm leg decodes the result directly (the same path the R3b
// `r3b_float_reduce` Float-scalar return uses). Native LLVM-AOT is the
// oracle; the corresponding tree-walk == cranelift legs are proven in
// the `relon-test-harness` corpus (`r7_*`). `pow` is intentionally not
// here — it needs a `pow` libcall with no native wasm instruction.
// ===========================================================

/// Shared driver: build `src` (single `Float x` param returning an Int
/// scalar), run the wasm32 leg, and assert the decoded `value` matches
/// the native Int oracle.
fn r7_check_int(name: &str, src: &str, x: f64) {
    let want = match native_run(
        src,
        HashMap::from([("x".to_string(), Value::Float(x.into()))]),
    ) {
        Value::Int(v) => v,
        other => panic!("[{name}] native expected Int, got {other:?}"),
    };
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping {name}");
        return;
    }
    let (bytes, info) = build(name, src);
    let in_record = pack_single_float(&info, x);
    let out = run_buffer(&bytes, &format!("relon_parity_{name}"), &info, &in_record);
    match out.get("value") {
        Some(Decoded::Int(v)) => assert_eq!(*v, want, "[{name}] wasm Int != native"),
        other => panic!("[{name}] decoded {other:?}"),
    }
}

/// Shared driver for a Float-scalar result: compares the IEEE-754 bit
/// pattern so `NaN` / `±0.0` edges stay exact.
fn r7_check_float(name: &str, src: &str, x: f64) {
    let want = match native_run(
        src,
        HashMap::from([("x".to_string(), Value::Float(x.into()))]),
    ) {
        Value::Float(f) => f.into_inner(),
        other => panic!("[{name}] native expected Float, got {other:?}"),
    };
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping {name}");
        return;
    }
    let (bytes, info) = build(name, src);
    let in_record = pack_single_float(&info, x);
    let out = run_buffer(&bytes, &format!("relon_parity_{name}"), &info, &in_record);
    match out.get("value") {
        Some(Decoded::Float(v)) => assert_eq!(
            v.to_bits(),
            want.to_bits(),
            "[{name}] wasm Float bits != native"
        ),
        other => panic!("[{name}] decoded {other:?}"),
    }
}

#[test]
fn r7_floor_aligns_native_via_wasmtime() {
    r7_check_int("r7_floor", "#main(Float x) -> Int\nfloor(x)", 3.7);
}

#[test]
fn r7_floor_neg_aligns_native_via_wasmtime() {
    r7_check_int("r7_floor_neg", "#main(Float x) -> Int\nfloor(x)", -3.2);
}

#[test]
fn r7_ceil_aligns_native_via_wasmtime() {
    r7_check_int("r7_ceil", "#main(Float x) -> Int\nceil(x)", 3.2);
}

#[test]
fn r7_round_ties_even_down_aligns_native_via_wasmtime() {
    // 2.5 rounds to 2 under ties-to-even (the oracle's
    // `round_ties_even`), NOT 3 (C `round` ties-away).
    r7_check_int("r7_round_even_down", "#main(Float x) -> Int\nround(x)", 2.5);
}

#[test]
fn r7_round_ties_even_up_aligns_native_via_wasmtime() {
    // 3.5 rounds to 4 under ties-to-even.
    r7_check_int("r7_round_even_up", "#main(Float x) -> Int\nround(x)", 3.5);
}

#[test]
fn r7_sqrt_aligns_native_via_wasmtime() {
    r7_check_float("r7_sqrt", "#main(Float x) -> Float\nsqrt(x)", 9.0);
}

#[test]
fn r7_sqrt_negative_is_nan_aligns_native_via_wasmtime() {
    // sqrt of a negative is NaN per IEEE-754 (oracle does not error);
    // the bit-pattern comparison in `r7_check_float` pins the NaN.
    r7_check_float("r7_sqrt_neg", "#main(Float x) -> Float\nsqrt(x)", -1.0);
}

#[test]
fn r7_abs_float_aligns_native_via_wasmtime() {
    r7_check_float("r7_abs_float", "#main(Float x) -> Float\nabs(x)", -5.5);
}

// ===========================================================
// Wave R8 — byte-level string stdlib on wasm32.
//
// `len` (String -> Int), `ends_with` (String, String -> Bool), and
// `replace` (String, String, String -> String). Each bundled body is
// purely byte-level (loads / stores / `BitAnd` for the char-boundary
// test, no UTF-8 decode or `Op::Trap`), exercised with constant String
// operands inside an `Int n` param entry (the arg is consumed and
// discarded by the literals). The String result rides the buffer
// protocol's tail-area record exactly like the w3 / r4 String returns;
// Bool / Int ride the fixed-area scalar slot. Native LLVM-AOT is the
// oracle; the tree-walk == cranelift legs are proven in
// `relon-test-harness` corpus `r8_*` + the per-fn probes.
//
// `trim` / `trim_start` / `trim_end` are now compiled four-way (the
// UTF-8 decode seam + `__is_whitespace` helper + `Op::Trap { InvalidUtf8 }`
// they need landed with R14 — the same seam `upper` / `lower` / `title` /
// `nfd` ride; see `unicode_four_way.rs`). Their wasm legs live in the
// `js_trim_*` tests below. `matches` (regex engine) and `split`
// (List<String> result) stay capped — no wasm-portable body.
// ===========================================================

/// Shared driver: build `src` (single `Int n` param returning a String
/// scalar), run the wasm32 leg, assert the decoded tail-record String
/// matches the native oracle (itself bit-aligned to tree-walk +
/// cranelift).
fn r8_check_str(name: &str, src: &str) {
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(1))])) {
        Value::String(s) => s,
        other => panic!("[{name}] native expected String, got {other:?}"),
    };
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping {name}");
        return;
    }
    let (bytes, info) = build(name, src);
    let in_record = pack_single_int(&info, 1);
    let out = run_buffer(&bytes, &format!("relon_parity_{name}"), &info, &in_record);
    match out.get("value") {
        Some(Decoded::Str(s)) => assert_eq!(*s, want, "[{name}] wasm String != native"),
        other => panic!("[{name}] decoded {other:?}"),
    }
}

/// Shared driver for an Int-scalar result (e.g. `len`).
fn r8_check_int(name: &str, src: &str, want_dbg: i64) {
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(1))])) {
        Value::Int(v) => v,
        other => panic!("[{name}] native expected Int, got {other:?}"),
    };
    assert_eq!(want, want_dbg, "[{name}] native Int oracle drifted");
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping {name}");
        return;
    }
    let (bytes, info) = build(name, src);
    let in_record = pack_single_int(&info, 1);
    let out = run_buffer(&bytes, &format!("relon_parity_{name}"), &info, &in_record);
    match out.get("value") {
        Some(Decoded::Int(v)) => assert_eq!(*v, want, "[{name}] wasm Int != native"),
        other => panic!("[{name}] decoded {other:?}"),
    }
}

/// Shared driver for a Bool-scalar result (e.g. `ends_with`). Bool is
/// decoded as `Decoded::Int(0/1)`.
fn r8_check_bool(name: &str, src: &str, want_dbg: bool) {
    let want = match native_run(src, HashMap::from([("n".to_string(), Value::Int(1))])) {
        Value::Bool(b) => b,
        other => panic!("[{name}] native expected Bool, got {other:?}"),
    };
    assert_eq!(want, want_dbg, "[{name}] native Bool oracle drifted");
    if !wasm_ld_available() {
        eprintln!("aot_wasm_parity: wasm-ld unavailable; skipping {name}");
        return;
    }
    let (bytes, info) = build(name, src);
    let in_record = pack_single_int(&info, 1);
    let out = run_buffer(&bytes, &format!("relon_parity_{name}"), &info, &in_record);
    match out.get("value") {
        Some(Decoded::Int(v)) => {
            assert_eq!(*v != 0, want, "[{name}] wasm Bool != native")
        }
        other => panic!("[{name}] decoded {other:?}"),
    }
}

#[test]
fn r8_len_aligns_native_via_wasmtime() {
    r8_check_int("r8_len", "#main(Int n) -> Int\nlen(\"hello\")", 5);
}

#[test]
fn r8_len_unicode_aligns_native_via_wasmtime() {
    // "café" — 5 UTF-8 bytes (len() is byte length, matching the oracle).
    r8_check_int("r8_len_unicode", "#main(Int n) -> Int\nlen(\"café\")", 5);
}

#[test]
fn r8_ends_with_true_aligns_native_via_wasmtime() {
    r8_check_bool(
        "r8_ends_with_t",
        "#main(Int n) -> Bool\nends_with(\"hello\", \"lo\")",
        true,
    );
}

#[test]
fn r8_ends_with_false_aligns_native_via_wasmtime() {
    r8_check_bool(
        "r8_ends_with_f",
        "#main(Int n) -> Bool\nends_with(\"hello\", \"xo\")",
        false,
    );
}

#[test]
fn r8_ends_with_empty_aligns_native_via_wasmtime() {
    r8_check_bool(
        "r8_ends_with_e",
        "#main(Int n) -> Bool\nends_with(\"hello\", \"\")",
        true,
    );
}

#[test]
fn r8_replace_all_aligns_native_via_wasmtime() {
    r8_check_str(
        "r8_replace_all",
        "#main(Int n) -> String\n\"aXbXc\".replace(\"X\", \"-\")",
    );
}

#[test]
fn r8_replace_grow_aligns_native_via_wasmtime() {
    r8_check_str(
        "r8_replace_grow",
        "#main(Int n) -> String\n\"a.b.c\".replace(\".\", \"__\")",
    );
}

#[test]
fn r8_replace_nomatch_aligns_native_via_wasmtime() {
    r8_check_str(
        "r8_replace_nomatch",
        "#main(Int n) -> String\n\"abc\".replace(\"X\", \"-\")",
    );
}

#[test]
fn r8_replace_empty_from_aligns_native_via_wasmtime() {
    r8_check_str(
        "r8_replace_empty",
        "#main(Int n) -> String\n\"ab\".replace(\"\", \"-\")",
    );
}

#[test]
fn r8_replace_empty_from_unicode_aligns_native_via_wasmtime() {
    r8_check_str(
        "r8_replace_empty_u",
        "#main(Int n) -> String\n\"café\".replace(\"\", \"X\")",
    );
}

// ===========================================================
// Wave R9: Bool-returning `is_uuid` validator. Reuses the
// `r8_check_bool` driver (Bool decodes as `Decoded::Int(0/1)`).
// Mirrors the `relon-test-harness` corpus `r9_*` cases. Sibling
// validators stay capped: `is_email` / `is_uri` walk `s.chars()`
// (UTF-8 decode seam — LLVM/wasm segfault), `is_ipv4` / `is_ipv6`
// route through `core::net` parsers (no wasm body), `is_iso_date`
// needs integer div/rem for the leap-year test (no `DivS` / `RemS`
// IR op).
// ===========================================================

#[test]
fn r9_is_uuid_valid_aligns_native_via_wasmtime() {
    r8_check_bool(
        "r9_is_uuid_valid",
        "#main(Int n) -> Bool\nis_uuid(\"12345678-1234-1234-1234-123456789012\")",
        true,
    );
}

#[test]
fn r9_is_uuid_upper_hex_aligns_native_via_wasmtime() {
    r8_check_bool(
        "r9_is_uuid_upper",
        "#main(Int n) -> Bool\nis_uuid(\"ABCDEF01-ABCD-ABCD-ABCD-ABCDEF012345\")",
        true,
    );
}

#[test]
fn r9_is_uuid_too_short_aligns_native_via_wasmtime() {
    r8_check_bool(
        "r9_is_uuid_short",
        "#main(Int n) -> Bool\nis_uuid(\"12345678-1234-1234-1234-12345678901\")",
        false,
    );
}

#[test]
fn r9_is_uuid_bad_dash_aligns_native_via_wasmtime() {
    r8_check_bool(
        "r9_is_uuid_dash",
        "#main(Int n) -> Bool\nis_uuid(\"12345678X1234-1234-1234-123456789012\")",
        false,
    );
}

#[test]
fn r9_is_uuid_nonhex_aligns_native_via_wasmtime() {
    r8_check_bool(
        "r9_is_uuid_nonhex",
        "#main(Int n) -> Bool\nis_uuid(\"1234567g-1234-1234-1234-123456789012\")",
        false,
    );
}

// ===========================================================
// JSON-Schema numeric / size predicates. Reuses the
// `r8_check_bool` Bool driver. Covers the four-way arms:
//   * `multiple_of(Int, Int)` — `d == 0 ? false : n % d == 0`
//     (the `d == 0` guard gates the `Op::Mod(I64)`, so a zero
//     divisor never traps).
//   * `in_range(n, lo, hi)` — all-`F64` inclusive bound check
//     (Int args widened to f64, matching the `to_f64_val` oracle).
//   * `size_in_range(List<_>, lo, hi)` — element count from the
//     `[len: u32 LE]` record header.
// Capped arms (NOT here): Float `multiple_of` (`Op::Mod(F64)` has
// no native cranelift / wasm remainder) and `size_in_range` on a
// String (Unicode code-point count needs the UTF-8 decode seam).
// ===========================================================

#[test]
fn js_multiple_of_true_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_mul_of_t",
        "#main(Int n) -> Bool\nmultiple_of(12, 4)",
        true,
    );
}

#[test]
fn js_multiple_of_false_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_mul_of_f",
        "#main(Int n) -> Bool\nmultiple_of(13, 4)",
        false,
    );
}

#[test]
fn js_multiple_of_zero_divisor_aligns_native_via_wasmtime() {
    // d == 0 short-circuits to false WITHOUT evaluating `n % d`
    // (which would trap on wasm / cranelift).
    r8_check_bool(
        "js_mul_of_z",
        "#main(Int n) -> Bool\nmultiple_of(7, 0)",
        false,
    );
}

#[test]
fn js_in_range_int_inside_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_inr_in",
        "#main(Int n) -> Bool\nin_range(5, 1, 10)",
        true,
    );
}

#[test]
fn js_in_range_int_edge_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_inr_edge",
        "#main(Int n) -> Bool\nin_range(10, 1, 10)",
        true,
    );
}

#[test]
fn js_in_range_int_outside_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_inr_out",
        "#main(Int n) -> Bool\nin_range(11, 1, 10)",
        false,
    );
}

#[test]
fn js_in_range_float_mix_aligns_native_via_wasmtime() {
    // Mixed Int / Float args: the oracle widens every arg to f64, so
    // the lowering peephole widens the Int bounds with ConvertI64ToF64.
    r8_check_bool(
        "js_inr_fmix",
        "#main(Int n) -> Bool\nin_range(2.5, 1, 3)",
        true,
    );
}

#[test]
fn js_size_in_range_list_inside_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_sir_in",
        "#main(Int n) -> Bool\nsize_in_range([1, 2, 3], 1, 5)",
        true,
    );
}

#[test]
fn js_size_in_range_list_outside_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_sir_out",
        "#main(Int n) -> Bool\nsize_in_range([1, 2, 3, 4, 5, 6], 1, 5)",
        false,
    );
}

// ===========================================================
// `trim` / `trim_start` / `trim_end` (Rust `str::trim*`) and the
// ASCII-structured validators `is_email` / `is_uri`, compiled
// four-way. The trim bodies forward-decode the input (trapping
// `InvalidUtf8`) and use the `__is_whitespace` helper (Unicode
// `White_Space`, i.e. `char::is_whitespace`) to bound the surviving
// slice, then memcpy it into a fresh record — the String result rides
// the tail-record protocol (`r8_check_str`). The validators are
// byte-level (a non-ASCII byte fails the char class exactly as the
// codepoint-level oracle rejects a non-ASCII codepoint). Native
// LLVM-AOT is the oracle; tree-walk == cranelift legs live in the
// `relon-test-harness` corpus `js_trim_*` / `js_is_email_*` /
// `js_is_uri_*`.
// ===========================================================

#[test]
fn js_trim_ascii_aligns_native_via_wasmtime() {
    r8_check_str("js_trim_ascii", "#main(Int n) -> String\ntrim(\"  hi  \")");
}

#[test]
fn js_trim_start_ascii_aligns_native_via_wasmtime() {
    r8_check_str(
        "js_trim_start_ascii",
        "#main(Int n) -> String\ntrim_start(\"  hi  \")",
    );
}

#[test]
fn js_trim_end_ascii_aligns_native_via_wasmtime() {
    r8_check_str(
        "js_trim_end_ascii",
        "#main(Int n) -> String\ntrim_end(\"  hi  \")",
    );
}

#[test]
fn js_trim_multibyte_ws_aligns_native_via_wasmtime() {
    // Leading NBSP (U+00A0) + trailing ideographic space (U+3000).
    r8_check_str(
        "js_trim_mb_ws",
        "#main(Int n) -> String\ntrim(\"\u{00A0}hi\u{3000}\")",
    );
}

#[test]
fn js_trim_keeps_inner_unicode_aligns_native_via_wasmtime() {
    r8_check_str(
        "js_trim_inner_u",
        "#main(Int n) -> String\ntrim(\"  a σ b  \")",
    );
}

#[test]
fn js_trim_all_whitespace_aligns_native_via_wasmtime() {
    r8_check_str(
        "js_trim_all_ws",
        "#main(Int n) -> String\ntrim(\" \u{00A0}\u{3000} \")",
    );
}

#[test]
fn js_trim_empty_aligns_native_via_wasmtime() {
    r8_check_str("js_trim_empty", "#main(Int n) -> String\ntrim(\"\")");
}

#[test]
fn js_is_email_valid_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_email_t",
        "#main(Int n) -> Bool\nis_email(\"a.b@example.com\")",
        true,
    );
}

#[test]
fn js_is_email_no_at_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_email_noat",
        "#main(Int n) -> Bool\nis_email(\"nope.example.com\")",
        false,
    );
}

#[test]
fn js_is_email_double_dot_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_email_dd",
        "#main(Int n) -> Bool\nis_email(\"a..b@example.com\")",
        false,
    );
}

#[test]
fn js_is_email_single_label_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_email_1lbl",
        "#main(Int n) -> Bool\nis_email(\"a@localhost\")",
        false,
    );
}

#[test]
fn js_is_email_label_dash_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_email_dash",
        "#main(Int n) -> Bool\nis_email(\"a@-bad.com\")",
        false,
    );
}

#[test]
fn js_is_email_unicode_local_aligns_native_via_wasmtime() {
    // Non-ASCII local part -> rejected (byte-level scan fails on the
    // multi-byte sequence, matching `.chars().all(...)`).
    r8_check_bool(
        "js_email_u",
        "#main(Int n) -> Bool\nis_email(\"résumé@example.com\")",
        false,
    );
}

#[test]
fn js_is_uri_valid_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_uri_t",
        "#main(Int n) -> Bool\nis_uri(\"https://example.com\")",
        true,
    );
}

#[test]
fn js_is_uri_no_scheme_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_uri_nos",
        "#main(Int n) -> Bool\nis_uri(\"no-scheme\")",
        false,
    );
}

#[test]
fn js_is_uri_empty_scheme_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_uri_es",
        "#main(Int n) -> Bool\nis_uri(\":empty-scheme\")",
        false,
    );
}

#[test]
fn js_is_uri_digit_first_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_uri_df",
        "#main(Int n) -> Bool\nis_uri(\"1http://x\")",
        false,
    );
}

#[test]
fn js_is_uri_mailto_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_uri_mailto",
        "#main(Int n) -> Bool\nis_uri(\"mailto:x@y.com\")",
        true,
    );
}

// ===========================================================
// `is_iso_date(String) -> Bool` (RFC 3339 `YYYY-MM-DD`). Reuses
// the `r8_check_bool` Bool driver. Mirrors the `relon-test-harness`
// corpus `js_is_iso_date_*` cases: valid date, invalid month / day,
// the four leap-year corners for 2/29 (2024 valid, 2023 / 1900
// invalid, 2000 valid), wrong separator, wrong length, non-digit.
// The body is byte-level shape + integer date arithmetic; the
// leap-year test uses `Op::Mod(I32)` against the non-zero constant
// divisors 4 / 100 / 400, so the divisor-zero guard never fires.
// ===========================================================

#[test]
fn js_is_iso_date_valid_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_isod_t",
        "#main(Int n) -> Bool\nis_iso_date(\"2020-01-15\")",
        true,
    );
}

#[test]
fn js_is_iso_date_bad_month_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_isod_mon",
        "#main(Int n) -> Bool\nis_iso_date(\"2020-13-01\")",
        false,
    );
}

#[test]
fn js_is_iso_date_bad_day_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_isod_day",
        "#main(Int n) -> Bool\nis_iso_date(\"2020-04-31\")",
        false,
    );
}

#[test]
fn js_is_iso_date_leap_2024_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_isod_l2024",
        "#main(Int n) -> Bool\nis_iso_date(\"2024-02-29\")",
        true,
    );
}

#[test]
fn js_is_iso_date_nonleap_2023_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_isod_2023",
        "#main(Int n) -> Bool\nis_iso_date(\"2023-02-29\")",
        false,
    );
}

#[test]
fn js_is_iso_date_century_1900_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_isod_1900",
        "#main(Int n) -> Bool\nis_iso_date(\"1900-02-29\")",
        false,
    );
}

#[test]
fn js_is_iso_date_century_2000_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_isod_2000",
        "#main(Int n) -> Bool\nis_iso_date(\"2000-02-29\")",
        true,
    );
}

#[test]
fn js_is_iso_date_bad_sep_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_isod_sep",
        "#main(Int n) -> Bool\nis_iso_date(\"2020/01/15\")",
        false,
    );
}

#[test]
fn js_is_iso_date_bad_len_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_isod_len",
        "#main(Int n) -> Bool\nis_iso_date(\"2020-1-15\")",
        false,
    );
}

#[test]
fn js_is_iso_date_nondigit_aligns_native_via_wasmtime() {
    r8_check_bool(
        "js_isod_nd",
        "#main(Int n) -> Bool\nis_iso_date(\"20x0-01-15\")",
        false,
    );
}
