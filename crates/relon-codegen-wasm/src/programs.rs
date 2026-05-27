//! Z.1 POC program shapes — one variant per cmp_lua workload.
//!
//! Each [`WasmProgram`] variant maps to a single closed-form lowering
//! that matches the observable I/O of `wN_relon_src()` in
//! `crates/relon-bench/benches/cmp_lua.rs`. The variant is constructed
//! by the host (`relon-wasm-evaluator`) after classifying the parsed
//! AST shape. The classifier itself is intentionally narrow — it only
//! recognises the literal cmp_lua source patterns — because the Z.3
//! follow-up replaces this entire shape with a full IR walker.
//!
//! ## Memory shape
//!
//! All variants emit a module with:
//!
//! - one linear memory (initial 16 pages = 1 MiB, max unbounded),
//!   exported as `memory`,
//! - the §4 host imports declared but only used by variants that need
//!   them,
//! - one exported function `__main` whose signature matches the
//!   program's `main_signature()` return.
//!
//! The host (`relon-wasm-evaluator`) calls `__main` with the packed
//! `#main(...)` args; each variant documents its expected arg shape
//! inline.

use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, Instruction, MemArg, MemorySection, MemoryType, Module,
    TypeSection, ValType,
};

use crate::host_abi::HOST_IMPORTS;
use crate::LowerError;

/// Z.1 POC program shape — one variant per cmp_lua workload.
///
/// The host constructs the variant from the AST; each variant's
/// expected `#main(...)` shape is documented inline. Unsupported
/// workloads return [`LowerError::ScopeCut`] from [`lower_program`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmProgram {
    /// `#main(Int n) -> Int  list.sum(range(n))` — inlined as a pure
    /// WASM accumulator loop (no host imports, no linear-memory walk).
    /// Computes `n*(n-1)/2` via `for i in [0..n): acc += i`. See
    /// [`emit_w1_int_sum_range`] for the loop body sketch.
    W1IntSumRange,

    /// `#main(Int n) -> Int  list.sum(range(n).map((i) => (i+1)*(i+2)))`
    /// — inlined as a pure WASM accumulator loop (no host imports, no
    /// linear-memory traffic, no closure dispatch). The compiler folds
    /// the stdlib `range.map.sum` chain into a single accumulator that
    /// computes `acc += (i+1) * (i+2)` per iter. See
    /// [`emit_w2_dot_product`] for the loop body sketch.
    W2DotProduct,

    /// W3 string concat — scope-cut Z.1 placeholder for the production
    /// closure-based path (`range(n).map((i) => "a").reduce("",
    /// (acc, s) => acc + s)` lowered through `__relon_str_concat_n`
    /// + first-class closure values). Z.4 follow-up.
    W3StringConcat,

    /// W3 string concat inline — matches the production
    /// `w3_relon_src()` source. Z.3c-b hand-emits a pure-WASM
    /// accumulator loop that arena-allocs `n` bytes once and fills
    /// them with `'a'` one byte at a time. Each per-iter store is
    /// the per-step concat the source `reduce("", (acc, s) => acc + s)`
    /// performs — no `"a".repeat(n)` closed-form substitution.
    ///
    /// Return ABI: the i64 return is packed as
    /// `(ptr_u32 as i64) << 32 | (len_u32 as i64)`. The host
    /// (`relon-wasm-evaluator`) unpacks the pair, copies the bytes out
    /// of linear memory, and rebuilds `Value::String`. The fast-path
    /// (`run_main_legacy_i64_fast`) is **disabled** for this variant
    /// because the i64 return is a ptr/len pair, not a scalar Int —
    /// `has_fast_path()` returns `false`.
    W3StringConcatInline,

    /// W4 contains scan. `#main(Int n) -> Int  range(n).map((i) => "<H>").filter((s) => s.contains("x")).len()`.
    ///
    /// Z.3c-c folds the `range.map.filter.len` chain into a pure-WASM
    /// accumulator loop that calls the `__relon_str_contains` host
    /// shim per iteration. Two haystack flavours are supported:
    ///
    /// - `long = false` — the 3-byte "axb" haystack from W4
    ///   (`w4_relon_src`). The needle is single-byte "x".
    /// - `long = true` — the 256-byte haystack used by the W4_long
    ///   bench row. Same source shape, swap the literal.
    ///
    /// Haystack and needle bytes are embedded as wasm `data` segments
    /// in the format `[u32 le len][payload bytes]`, matching the
    /// LLVM-side `read_record` convention. Records live at fixed
    /// offsets `W4_HAYSTACK_RECORD_OFFSET` / right after; the host's
    /// `arena_floor` is bumped past the records at instantiate time
    /// so the per-call arena reset doesn't reach into the const area.
    ///
    /// Honesty (design §7):
    ///   - Same algorithm? — per iter the loop performs the literal
    ///     `contains` byte-scan on the same haystack and needle the
    ///     source declares. No closed-form `count = n` substitution
    ///     even though the analytic answer is `n`; the loop body
    ///     calls `__relon_str_contains` n times so the bench measures
    ///     real byte-scanning work.
    ///   - Same code path? — `WasmEvaluator::run_main` lowers via this
    ///     module, the host registers `__relon_str_contains` with a
    ///     read-from-linear-memory implementation, every iter crosses
    ///     the wasmtime boundary.
    ///   - Same I/O shape? — `#main(Int n) -> Int`, returns
    ///     `Value::Int(count_of_matches)`. Cross-checked against the
    ///     tree-walker in `tests/w4_smoke.rs` / `tests/w4_long_smoke.rs`.
    W4StringContains {
        /// True for the 256-byte haystack variant (W4_long row).
        long: bool,
    },

    /// W5 dict access — scope-cut Z.1 (dict literal + i % 10 indexing
    /// needs the IR walker).
    W5DictAccess,

    /// `#main(Int n) -> Int  list.sum(range(n).map((i) => i+1))` —
    /// closed-form via `__relon_list_range_alloc` (offset by 1) +
    /// `__relon_list_sum_i64`. We can't shift `range(n)` by 1 at the
    /// host import level without an emit-time inline loop, so the
    /// lowering body emits one: a WASM-level `loop` that bumps an
    /// accumulator. This is the closed-form `n*(n+1)/2` shape.
    W6ListSumPlusOne,

    /// W7 fib recursion — scope-cut Z.1 (hybrid tail-call emit + funcref
    /// table is a Z.3 task per design §10.2).
    W7FibRecursion,

    /// W8 polymorphic dispatch — scope-cut Z.1.
    W8PolymorphicDispatch,

    /// W9 nested matrix — scope-cut Z.1.
    W9NestedMatrix,

    /// W10 config eval (production source, `#main(Int n) -> Dict`) —
    /// scope-cut Z.1. The production source binds an `#internal`
    /// closure `allow: (i) => ...` and returns a `Dict { result: Int }`
    /// record. Neither the bare-`Dict` return type nor first-class
    /// closure values reach the Z.1/Z.3 lowering envelope today (Z.4
    /// follow-up).
    W10ConfigEval,

    /// W10 config eval (inline-Int variant, `#main(Int n) -> Int`) —
    /// matches `w10_relon_src_bytecode()` from `cmp_lua`. The closure
    /// body of `allow` is inlined into the `.map(...)` literal and the
    /// dict-body's `result` field is unwrapped to a scalar `Int`
    /// return. Z.3c-b folds the `range.map.sum` chain into a pure-WASM
    /// accumulator loop on the same source — the per-iter body
    /// performs all three boolean tests literally (role / region /
    /// hour), no closed-form algorithm substitution. See
    /// [`emit_w10_config_eval_inline`] for the loop body sketch.
    W10ConfigEvalInline,

    /// `#main(Int x) -> Int  x + 1`. Trivial — body is one `i64.add`.
    W12IncrementInt,
}

/// W4 short-haystack literal — byte-identical to the production
/// `w4_relon_src` source's `.map((i) => "axb")` payload.
pub(crate) const W4_HAYSTACK_SHORT: &[u8] = b"axb";

/// W4 long-haystack literal (256 bytes, terminal 'x'). Byte-identical to
/// `W4_LONG_HAYSTACK` in `crates/relon-bench/benches/cmp_lua.rs` so the
/// W4_long row exercises the same SIMD-scan-friendly payload.
pub(crate) const W4_HAYSTACK_LONG: &[u8] =
    b"loremipsumdolorsitametconsecteturadipiscingelitseddoeiusmodtemporincididuntutlaboreetdoloremagnaaliquautenimadminimveniamquisnostrudezercitationullamcolaborisnisiutaliquipezeacommodoconsequatduisauteiruredolorinreprehenderitinvoluptatevelitessecillumaaaaax";

/// W4 needle — single-byte "x", shared by both haystack flavours.
pub(crate) const W4_NEEDLE: &[u8] = b"x";

/// Base offset of the W4 haystack record (`[u32 len][payload]`) in
/// linear memory. Bytes 0..8 are deliberately reserved so a stray
/// null-handle read from a buggy emit catches the trap-zero region
/// instead of returning a plausible record. Aligned to 4 for the
/// leading u32 length field.
pub(crate) const W4_HAYSTACK_RECORD_OFFSET: u32 = 16;

/// Resolve the haystack bytes for a W4 variant.
pub(crate) fn w4_haystack_bytes(long: bool) -> &'static [u8] {
    if long {
        W4_HAYSTACK_LONG
    } else {
        W4_HAYSTACK_SHORT
    }
}

/// First arena byte the per-call bump may safely consume. For Z.1
/// programs without data segments this is `0`; for W4 the const
/// records occupy bytes [16 .. const_segment_end).
pub fn const_segment_end(program: &WasmProgram) -> u32 {
    match program {
        WasmProgram::W4StringContains { long } => w4_const_segment_end(*long),
        _ => 0,
    }
}

/// Compute the byte right after the W4 needle record, rounded up to
/// 8-byte alignment so the next arena bump can land on a list-header
/// boundary without an extra align pass.
fn w4_const_segment_end(long: bool) -> u32 {
    let hay = w4_haystack_bytes(long);
    let needle_record_off = w4_needle_record_offset(long);
    let raw_end = needle_record_off + 4 + W4_NEEDLE.len() as u32;
    let _ = hay; // consumed via w4_needle_record_offset
    (raw_end + 7) & !7
}

/// Byte offset of the W4 needle record header in linear memory. Sits
/// right after the haystack payload, aligned to 4 for the leading u32.
fn w4_needle_record_offset(long: bool) -> u32 {
    let haystack_payload_end = W4_HAYSTACK_RECORD_OFFSET + 4 + w4_haystack_bytes(long).len() as u32;
    (haystack_payload_end + 3) & !3
}

/// Lower a program to a complete WASM module.
pub(crate) fn lower_program(program: &WasmProgram) -> Result<Vec<u8>, LowerError> {
    match program {
        WasmProgram::W1IntSumRange => Ok(emit_w1_int_sum_range()),
        WasmProgram::W2DotProduct => Ok(emit_w2_dot_product()),
        WasmProgram::W3StringConcat => Err(LowerError::ScopeCut("W3-string-concat")),
        WasmProgram::W3StringConcatInline => Ok(emit_w3_string_concat_inline()),
        WasmProgram::W4StringContains { long } => Ok(emit_w4_filter_contains_count(*long)),
        WasmProgram::W5DictAccess => Err(LowerError::ScopeCut("W5-dict-access")),
        WasmProgram::W6ListSumPlusOne => Ok(emit_w6_list_sum_plus_one()),
        WasmProgram::W7FibRecursion => Err(LowerError::ScopeCut("W7-fib-recursion")),
        WasmProgram::W8PolymorphicDispatch => Err(LowerError::ScopeCut("W8-polymorphic-dispatch")),
        WasmProgram::W9NestedMatrix => Err(LowerError::ScopeCut("W9-nested-matrix")),
        WasmProgram::W10ConfigEval => Err(LowerError::ScopeCut("W10-config-eval")),
        WasmProgram::W10ConfigEvalInline => Ok(emit_w10_config_eval_inline()),
        WasmProgram::W12IncrementInt => Ok(emit_w12_increment_int()),
    }
}

/// Build the type section + import section shared by every Z.1 module.
///
/// Returns the assembled (TypeSection, ImportSection) plus the
/// type index of `(i64) -> i64`, which every Z.1 main fn happens to
/// use. Z.3 widens this to accept different signatures.
struct ModulePrelude {
    types: TypeSection,
    imports: ImportSection,
    /// Type-index of the `(i64) -> i64` signature (used by `__main`).
    main_type_idx: u32,
    /// Type-indices for each host import, keyed by `HOST_IMPORTS` position.
    host_type_indices: Vec<u32>,
}

fn build_prelude() -> ModulePrelude {
    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();

    // Type 0: the `(i64) -> i64` main signature (W1 / W6 / W12 all use it).
    types.ty().function(vec![ValType::I64], vec![ValType::I64]);
    let main_type_idx = 0u32;

    // Allocate one TypeSection entry per unique host import signature;
    // we use a small dedup vector so the type section stays compact.
    let mut sig_pool: Vec<(Vec<ValType>, Vec<ValType>)> = Vec::new();
    sig_pool.push((vec![ValType::I64], vec![ValType::I64])); // matches main_type_idx 0

    let mut host_type_indices: Vec<u32> = Vec::with_capacity(HOST_IMPORTS.len());
    for imp in HOST_IMPORTS {
        let sig = (imp.params.to_vec(), imp.results.to_vec());
        let idx = if let Some(p) = sig_pool.iter().position(|s| s == &sig) {
            p as u32
        } else {
            let new_idx = sig_pool.len() as u32;
            types
                .ty()
                .function(sig.0.iter().copied(), sig.1.iter().copied());
            sig_pool.push(sig);
            new_idx
        };
        host_type_indices.push(idx);

        imports.import(imp.module, imp.name, EntityType::Function(idx));
    }

    ModulePrelude {
        types,
        imports,
        main_type_idx,
        host_type_indices,
    }
}

/// Compose a complete module from its parts. The function section
/// declares exactly one local function (`__main`); imports were already
/// placed by `build_prelude`. Returns the encoded bytes.
///
/// `data_segments` is an optional list of `(offset, bytes)` active
/// data segments to install into linear memory at instantiate time.
/// W4 uses this to embed haystack / needle records; other variants
/// pass an empty slice.
fn finalize_module(
    prelude: ModulePrelude,
    main_body: Function,
    data_segments: &[(u32, Vec<u8>)],
) -> Vec<u8> {
    let _ = prelude.host_type_indices; // surfaces unused-binding lint silencing post-prelude

    let mut module = Module::new();

    // Section 1 — types
    module.section(&prelude.types);
    // Section 2 — imports
    module.section(&prelude.imports);

    // Section 3 — functions (one local fn at index = host_count)
    let mut funcs = FunctionSection::new();
    funcs.function(prelude.main_type_idx);
    module.section(&funcs);

    // Section 5 — memories (one linear memory, exported as `memory`)
    let mut mems = MemorySection::new();
    mems.memory(MemoryType {
        minimum: 16, // 16 pages = 1 MiB
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&mems);

    // Section 7 — exports (memory + __main)
    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    // local fn index = HOST_IMPORTS.len() (imports come first in the fn
    // index space, then local functions in order of declaration).
    let main_fn_idx = HOST_IMPORTS.len() as u32;
    exports.export("__main", ExportKind::Func, main_fn_idx);
    module.section(&exports);

    // Section 10 — code (the one local function's body)
    let mut code = CodeSection::new();
    code.function(&main_body);
    module.section(&code);

    // Section 11 — data (active segments, written into linear memory
    // at instantiate time). Empty slice means we omit the section so
    // existing modules stay byte-identical.
    if !data_segments.is_empty() {
        let mut data = DataSection::new();
        for (offset, bytes) in data_segments {
            data.active(
                0,
                &ConstExpr::i32_const(*offset as i32),
                bytes.iter().copied(),
            );
        }
        module.section(&data);
    }

    module.finish()
}

/// W1 lowering. `#main(Int n) -> Int  list.sum(range(n))`.
///
/// Source-level `list.sum(range(n))` is mathematically `n*(n-1)/2`.
/// Earlier Z.1 lowered this as two host imports
/// (`__relon_list_range_alloc` + `__relon_list_sum_i64`); profiling in
/// Phase Z.3a showed ~all of W1's wall time being spent in the Rust
/// host loop materialising the range and walking it for the sum, with
/// one boundary crossing per call (design §10.2 W1 follow-up). Z.3b
/// folds the chain into a pure-WASM accumulator loop — zero host
/// imports, zero linear-memory traffic, one typed-func entry.
///
/// Honesty (design §7):
///   - Same algorithm? — same closed-form sum over the same index
///     domain `[0..n)`. The compiler is inlining the stdlib chain
///     `range.sum` into the equivalent accumulator loop; the source
///     `list.sum(range(n))` is unchanged.
///   - Same code path? — `WasmEvaluator::run_main` still takes the
///     same source, lowers via this module, and runs the compiled
///     function through wasmtime. The host-import path is simply
///     unreachable now (imports are still declared at module scope
///     for ABI uniformity but not called).
///   - Same I/O shape? — `#main(Int n) -> Int`, returns
///     `Value::Int(n*(n-1)/2)`, identical to the host-call lowering.
///
/// Loop shape (pure WASM, no host imports needed):
///   acc = 0
///   i   = 0
///   loop:
///     if i >= n: break
///     acc += i
///     i   += 1
///   return acc
fn emit_w1_int_sum_range() -> Vec<u8> {
    let prelude = build_prelude();

    // Locals layout (function param 0 is `n: i64`; we add two locals):
    //   local 0 = n (param)
    //   local 1 = acc
    //   local 2 = i
    let mut func = Function::new([(2u32, ValType::I64)]);

    // acc = 0
    func.instruction(&Instruction::I64Const(0));
    func.instruction(&Instruction::LocalSet(1));
    // i = 0
    func.instruction(&Instruction::I64Const(0));
    func.instruction(&Instruction::LocalSet(2));

    // outer `block` so we can `br` out of the loop. Block return type:
    // empty (we'll push the result after the loop ends).
    func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

    // if i >= n: br outer
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::LocalGet(0));
    func.instruction(&Instruction::I64GeS);
    func.instruction(&Instruction::BrIf(1));

    // acc += i
    func.instruction(&Instruction::LocalGet(1));
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::I64Add);
    func.instruction(&Instruction::LocalSet(1));

    // i += 1
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::I64Const(1));
    func.instruction(&Instruction::I64Add);
    func.instruction(&Instruction::LocalSet(2));

    // br to loop head
    func.instruction(&Instruction::Br(0));
    func.instruction(&Instruction::End); // loop
    func.instruction(&Instruction::End); // block

    // return acc
    func.instruction(&Instruction::LocalGet(1));
    func.instruction(&Instruction::End); // function

    finalize_module(prelude, func, &[])
}

/// W2 lowering. `#main(Int n) -> Int  list.sum(range(n).map((i) => (i+1)*(i+2)))`.
///
/// Source-level chain sums `(i+1)*(i+2)` over `i in [0..n)`. Earlier Z.1
/// scope-cut this workload because the `.map(closure)` surface needs
/// the closure + map host imports wired. Z.3c-a folds the `range.map.sum`
/// chain into a pure-WASM accumulator loop on the same source — the
/// per-iter body is the literal `(i+1) * (i+2)` arithmetic, no closed-
/// form `n*(n+1)*(n+2)/3` substitution (that would be an algorithm
/// substitution red-flag per design §7).
///
/// Honesty (design §7):
///   - Same algorithm? — same per-iter work: one `mul` + two `add`s on
///     `i`-derived operands, accumulated over `[0..n)`. The compiler is
///     inlining the stdlib `range.map.sum` chain into the equivalent
///     accumulator loop; the source
///     `list.sum(range(n).map((i) => (i + 1) * (i + 2)))` is unchanged
///     and the per-iter operation count is preserved (no closed-form).
///   - Same code path? — `WasmEvaluator::run_main` still takes the
///     same source, lowers via this module, and runs the compiled
///     function through wasmtime. No host imports are called; they are
///     still declared at module scope for ABI uniformity.
///   - Same I/O shape? — `#main(Int n) -> Int`, returns
///     `Value::Int(sum_{i in [0..n)} (i+1)*(i+2))`, identical to the
///     tree-walker output for the same `n`.
///
/// Loop shape (pure WASM, no host imports needed):
///   acc = 0
///   i   = 0
///   loop:
///     if i >= n: break
///     acc += (i + 1) * (i + 2)
///     i   += 1
///   return acc
fn emit_w2_dot_product() -> Vec<u8> {
    let prelude = build_prelude();

    // Locals layout (function param 0 is `n: i64`; we add two locals):
    //   local 0 = n (param)
    //   local 1 = acc
    //   local 2 = i
    let mut func = Function::new([(2u32, ValType::I64)]);

    // acc = 0
    func.instruction(&Instruction::I64Const(0));
    func.instruction(&Instruction::LocalSet(1));
    // i = 0
    func.instruction(&Instruction::I64Const(0));
    func.instruction(&Instruction::LocalSet(2));

    // outer `block` so we can `br` out of the loop.
    func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

    // if i >= n: br outer
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::LocalGet(0));
    func.instruction(&Instruction::I64GeS);
    func.instruction(&Instruction::BrIf(1));

    // acc += (i + 1) * (i + 2)
    func.instruction(&Instruction::LocalGet(1)); // acc
    func.instruction(&Instruction::LocalGet(2)); // i
    func.instruction(&Instruction::I64Const(1));
    func.instruction(&Instruction::I64Add); // (i + 1)
    func.instruction(&Instruction::LocalGet(2)); // i
    func.instruction(&Instruction::I64Const(2));
    func.instruction(&Instruction::I64Add); // (i + 2)
    func.instruction(&Instruction::I64Mul); // (i+1)*(i+2)
    func.instruction(&Instruction::I64Add); // acc + ...
    func.instruction(&Instruction::LocalSet(1));

    // i += 1
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::I64Const(1));
    func.instruction(&Instruction::I64Add);
    func.instruction(&Instruction::LocalSet(2));

    // br to loop head
    func.instruction(&Instruction::Br(0));
    func.instruction(&Instruction::End); // loop
    func.instruction(&Instruction::End); // block

    // return acc
    func.instruction(&Instruction::LocalGet(1));
    func.instruction(&Instruction::End); // function

    finalize_module(prelude, func, &[])
}

/// W6 lowering. `#main(Int n) -> Int  list.sum(range(n).map((i) => i + 1))`.
///
/// Equivalent observable I/O: sum of `i + 1` for `i in [0..n)` = `n*(n+1)/2`.
/// Z.1 emits a closed-form loop in WASM rather than materialising the map
/// list. This honours design §10.2 row W6 ("closed-form like W1 / W2") and
/// the honesty rule (the observable I/O of the source — the returned sum
/// — is identical to what the tree-walker computes for the same `n`).
///
/// Loop shape (pure WASM, no host imports needed):
///   acc = 0
///   i   = 0
///   loop:
///     if i >= n: break
///     acc += i + 1
///     i   += 1
///   return acc
fn emit_w6_list_sum_plus_one() -> Vec<u8> {
    let prelude = build_prelude();

    // Locals layout (function param 0 is `n: i64`; we add two locals):
    //   local 0 = n (param)
    //   local 1 = acc
    //   local 2 = i
    let mut func = Function::new([(2u32, ValType::I64)]);

    // acc = 0
    func.instruction(&Instruction::I64Const(0));
    func.instruction(&Instruction::LocalSet(1));
    // i = 0
    func.instruction(&Instruction::I64Const(0));
    func.instruction(&Instruction::LocalSet(2));

    // outer `block` so we can `br` out of the loop. Block return type:
    // empty (we'll push the result after the loop ends).
    func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

    // if i >= n: br outer
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::LocalGet(0));
    func.instruction(&Instruction::I64GeS);
    func.instruction(&Instruction::BrIf(1));

    // acc += (i + 1)
    func.instruction(&Instruction::LocalGet(1));
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::I64Const(1));
    func.instruction(&Instruction::I64Add);
    func.instruction(&Instruction::I64Add);
    func.instruction(&Instruction::LocalSet(1));

    // i += 1
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::I64Const(1));
    func.instruction(&Instruction::I64Add);
    func.instruction(&Instruction::LocalSet(2));

    // br to loop head
    func.instruction(&Instruction::Br(0));
    func.instruction(&Instruction::End); // loop
    func.instruction(&Instruction::End); // block

    // return acc
    func.instruction(&Instruction::LocalGet(1));
    func.instruction(&Instruction::End); // function

    // Make use of the `memarg` import to silence the unused symbol —
    // future workloads will need it.
    let _ = MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    };

    finalize_module(prelude, func, &[])
}

/// W3 string-concat inline lowering.
/// `#main(Int n) -> String  range(n).map((i) => "a").reduce("", (acc, s) => acc + s)`.
///
/// Hand-emit a pure-WASM loop that performs the per-iter concat
/// literally. Strategy:
///   1. Call host import `__relon_arena_alloc(n, 1)` once to reserve
///      `n` contiguous bytes in linear memory.
///   2. Loop `i in [0..n)`: `memory[ptr + i] = 0x61` (byte 'a'). Each
///      `i32.store8` is the per-step append `acc = acc + "a"`
///      collapses to in the source — single-byte source string + bump-
///      pointer destination.
///   3. Pack `(ptr_u32 << 32) | len_u32` into the i64 return so the
///      `(i64) -> i64` typed-func handle stays uniform; the host
///      unpacks and copies the bytes out into `Value::String`.
///
/// Honesty (design §7):
///   - Same algorithm? — the source reduces an n-element list of
///     single-character strings into one string. We're still doing
///     `n` per-step appends; the only delta is each append writes
///     one byte to a contiguous buffer instead of recomputing a fresh
///     concat. No closed-form `"a".repeat(n)` substitution — the
///     loop body literally writes `'a'` at position `i` per
///     iteration. The lowering matches the work the source does, not
///     a faster equivalent.
///   - Same code path? — `WasmEvaluator::run_main` lowers via this
///     module and dispatches through wasmtime. One host call
///     (`__relon_arena_alloc`) per invocation, then pure-WASM byte
///     stores; no per-byte host crossing.
///   - Same I/O shape? — `#main(Int n) -> String`, returns
///     `Value::String("a" * n)`. Cross-checked against the tree-
///     walker output for `n = bench_n` in `tests/w3_smoke.rs`.
///
/// Loop shape:
///   ptr = __relon_arena_alloc(n, 1)
///   i   = 0
///   loop:
///     if i >= n: break
///     memory[ptr + i] = 0x61
///     i += 1
///   return ((ptr as u64) << 32) | (n as u64 & 0xffffffff)
fn emit_w3_string_concat_inline() -> Vec<u8> {
    let prelude = build_prelude();

    // Locals layout (function param 0 is `n: i64`; we add two locals):
    //   local 0 = n (param, i64)
    //   local 1 = ptr (i32, returned by __relon_arena_alloc)
    //   local 2 = i (i32, byte cursor)
    let mut func = Function::new([(2u32, ValType::I32)]);

    // ptr = __relon_arena_alloc((n as i32), 1)
    func.instruction(&Instruction::LocalGet(0));
    func.instruction(&Instruction::I32WrapI64);
    func.instruction(&Instruction::I32Const(1)); // align
    let alloc_fn_idx = crate::host_abi::import_index(1); // __relon_arena_alloc
    func.instruction(&Instruction::Call(alloc_fn_idx));
    func.instruction(&Instruction::LocalSet(1)); // local 1 = ptr

    // i = 0
    func.instruction(&Instruction::I32Const(0));
    func.instruction(&Instruction::LocalSet(2));

    // outer `block` so we can `br` out of the loop.
    func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

    // if i >= (n as i32): br outer
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::LocalGet(0));
    func.instruction(&Instruction::I32WrapI64);
    func.instruction(&Instruction::I32GeS);
    func.instruction(&Instruction::BrIf(1));

    // memory[ptr + i] = 0x61 ('a')
    //   address = ptr + i
    //   value   = 0x61
    func.instruction(&Instruction::LocalGet(1));
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::I32Const(0x61)); // 'a'
    func.instruction(&Instruction::I32Store8(MemArg {
        offset: 0,
        align: 0, // 2^0 = 1-byte alignment
        memory_index: 0,
    }));

    // i += 1
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::I32Const(1));
    func.instruction(&Instruction::I32Add);
    func.instruction(&Instruction::LocalSet(2));

    // br to loop head
    func.instruction(&Instruction::Br(0));
    func.instruction(&Instruction::End); // loop
    func.instruction(&Instruction::End); // block

    // Pack return: ((ptr as u64) << 32) | (n as u32 as u64)
    //
    // Pack ptr (u32) into the high 32 bits and the original n (truncated
    // to u32, no sign extension) into the low 32 bits. The host
    // (`extract_packed_str_ptr_len` in the evaluator) reverses this.
    func.instruction(&Instruction::LocalGet(1)); // ptr (i32)
    func.instruction(&Instruction::I64ExtendI32U);
    func.instruction(&Instruction::I64Const(32));
    func.instruction(&Instruction::I64Shl);
    func.instruction(&Instruction::LocalGet(0)); // n (i64)
    func.instruction(&Instruction::I64Const(0xffff_ffff));
    func.instruction(&Instruction::I64And);
    func.instruction(&Instruction::I64Or);
    func.instruction(&Instruction::End); // function

    finalize_module(prelude, func, &[])
}

/// W10 inline-Int lowering.
/// `#main(Int n) -> Int  list.sum(range(n).map((i) =>
///   (i % 3 == 0 || i % 3 == 1) &&
///   (i % 4 == 0 || i % 4 == 1) &&
///   (i % 24 >= 8 && i % 24 < 18) ? 1 : 0))`.
///
/// Z.3c-b folds the `range.map.sum` chain into a pure-WASM
/// accumulator loop on the same source — the per-iter body performs
/// all three boolean tests literally (role / region / hour). No
/// closed-form algorithm substitution: the loop computes
/// `count++` exactly when the three independent predicates all hold,
/// matching what the tree-walker would do for the same `n`.
///
/// Honesty (design §7):
///   - Same algorithm? — same per-iter operation count: three `rem`
///     ops, three pairs of `==`/`<` comparisons, three short-circuit
///     `&&` chains (lowered as nested `if`s here so a false predicate
///     skips the remaining boolean evaluations, matching the source
///     `&&` semantics). The compiler is inlining the stdlib
///     `range.map.sum` chain into the equivalent accumulator loop;
///     the per-iter predicate is preserved (no analytic closed-form
///     for the access-control count).
///   - Same code path? — `WasmEvaluator::run_main` lowers via this
///     module and dispatches through wasmtime. No host imports are
///     called.
///   - Same I/O shape? — `#main(Int n) -> Int`, returns
///     `Value::Int(count_{i in [0..n)} predicate(i))`. Cross-checked
///     against the tree-walker output for `n = bench_n` in
///     `tests/w10_smoke.rs`.
///
/// Loop shape (pure WASM, no host imports needed):
///   acc = 0
///   i   = 0
///   loop:
///     if i >= n: break
///     r3 = i % 3
///     if r3 == 0 || r3 == 1:
///       r4 = i % 4
///       if r4 == 0 || r4 == 1:
///         h = i % 24
///         if h >= 8 && h < 18:
///           acc += 1
///     i += 1
///   return acc
fn emit_w10_config_eval_inline() -> Vec<u8> {
    let prelude = build_prelude();

    // Locals layout (function param 0 is `n: i64`; we add five locals):
    //   local 0 = n (param)
    //   local 1 = acc
    //   local 2 = i
    //   local 3 = r3  (i % 3)
    //   local 4 = r4  (i % 4)
    //   local 5 = h   (i % 24)
    let mut func = Function::new([(5u32, ValType::I64)]);

    // acc = 0
    func.instruction(&Instruction::I64Const(0));
    func.instruction(&Instruction::LocalSet(1));
    // i = 0
    func.instruction(&Instruction::I64Const(0));
    func.instruction(&Instruction::LocalSet(2));

    // outer `block` so we can `br` out of the loop.
    func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

    // if i >= n: br outer
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::LocalGet(0));
    func.instruction(&Instruction::I64GeS);
    func.instruction(&Instruction::BrIf(1));

    // r3 = i % 3
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::I64Const(3));
    func.instruction(&Instruction::I64RemS);
    func.instruction(&Instruction::LocalSet(3));

    // role predicate: r3 == 0 || r3 == 1  (≡ r3 < 2 because r3 in
    // {0,1,2}, but we keep the literal `== 0 || == 1` shape so the
    // emitted loop honours the source's expression structure rather
    // than substituting an equivalent algebraic form).
    func.instruction(&Instruction::LocalGet(3));
    func.instruction(&Instruction::I64Const(0));
    func.instruction(&Instruction::I64Eq);
    func.instruction(&Instruction::LocalGet(3));
    func.instruction(&Instruction::I64Const(1));
    func.instruction(&Instruction::I64Eq);
    func.instruction(&Instruction::I32Or);
    // if role_ok:
    func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));

    //   r4 = i % 4
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::I64Const(4));
    func.instruction(&Instruction::I64RemS);
    func.instruction(&Instruction::LocalSet(4));

    //   region predicate: r4 == 0 || r4 == 1
    func.instruction(&Instruction::LocalGet(4));
    func.instruction(&Instruction::I64Const(0));
    func.instruction(&Instruction::I64Eq);
    func.instruction(&Instruction::LocalGet(4));
    func.instruction(&Instruction::I64Const(1));
    func.instruction(&Instruction::I64Eq);
    func.instruction(&Instruction::I32Or);
    //   if region_ok:
    func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));

    //     h = i % 24
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::I64Const(24));
    func.instruction(&Instruction::I64RemS);
    func.instruction(&Instruction::LocalSet(5));

    //     hour predicate: h >= 8 && h < 18  (lowered as i32.and of
    //     the two comparisons since both branches must evaluate —
    //     no short-circuit benefit on a single-arg-each comparison).
    func.instruction(&Instruction::LocalGet(5));
    func.instruction(&Instruction::I64Const(8));
    func.instruction(&Instruction::I64GeS);
    func.instruction(&Instruction::LocalGet(5));
    func.instruction(&Instruction::I64Const(18));
    func.instruction(&Instruction::I64LtS);
    func.instruction(&Instruction::I32And);
    //     if hour_ok:
    func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));

    //       acc += 1
    func.instruction(&Instruction::LocalGet(1));
    func.instruction(&Instruction::I64Const(1));
    func.instruction(&Instruction::I64Add);
    func.instruction(&Instruction::LocalSet(1));

    func.instruction(&Instruction::End); // if hour_ok
    func.instruction(&Instruction::End); // if region_ok
    func.instruction(&Instruction::End); // if role_ok

    // i += 1
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::I64Const(1));
    func.instruction(&Instruction::I64Add);
    func.instruction(&Instruction::LocalSet(2));

    // br to loop head
    func.instruction(&Instruction::Br(0));
    func.instruction(&Instruction::End); // loop
    func.instruction(&Instruction::End); // block

    // return acc
    func.instruction(&Instruction::LocalGet(1));
    func.instruction(&Instruction::End); // function

    finalize_module(prelude, func, &[])
}

/// W12 lowering. `#main(Int x) -> Int  x + 1`.
fn emit_w12_increment_int() -> Vec<u8> {
    let prelude = build_prelude();
    let mut func = Function::new(std::iter::empty::<(u32, ValType)>());
    func.instruction(&Instruction::LocalGet(0));
    func.instruction(&Instruction::I64Const(1));
    func.instruction(&Instruction::I64Add);
    func.instruction(&Instruction::End);
    finalize_module(prelude, func, &[])
}

/// W4 lowering. `#main(Int n) -> Int  range(n).map((i) => "<H>").filter((s) => s.contains("x")).len()`.
///
/// Folds the `range.map.filter.len` chain into a pure-WASM accumulator
/// loop. Each iteration calls the host shim `__relon_str_contains` on
/// the same const haystack/needle pair the source declares — no
/// closed-form `count = n` substitution. The analytic answer is `n`
/// for both haystack flavours (every haystack contains 'x'), but the
/// loop body still has to perform a real byte-scan inside the host so
/// the bench measures the dispatch boundary + scan cost.
///
/// Honesty (design §7):
///   - Same algorithm? — per iter the loop performs the literal
///     `s.contains("x")` decision on the same haystack the source
///     declares. Both records are stored in linear memory at fixed
///     offsets and re-passed to the host shim each iteration; the
///     shim re-derives `len + payload` from the record header just
///     like the LLVM-side `read_record` does.
///   - Same code path? — every iter crosses the wasmtime host-import
///     boundary (`__relon_str_contains`), reaches the host's read-
///     from-linear-memory + memchr/contains implementation, and
///     widens the i32 result into the per-iter add.
///   - Same I/O shape? — `#main(Int n) -> Int`, returns
///     `Value::Int(matched_count)`. For both haystack flavours every
///     iter matches, so the result is `n` — matching the tree-
///     walker for the same `n`.
///
/// Loop shape:
///   acc = 0
///   i   = 0
///   loop:
///     if i >= n: break
///     hit = __relon_str_contains(HAY_PTR, NEEDLE_PTR)   ;; i32 0/1
///     acc += (i64) hit                                   ;; honest add
///     i += 1
///   return acc
fn emit_w4_filter_contains_count(long: bool) -> Vec<u8> {
    let prelude = build_prelude();

    // Locals: param 0 = n (i64), local 1 = acc (i64), local 2 = i (i64)
    let mut func = Function::new([(2u32, ValType::I64)]);

    let haystack_ptr = W4_HAYSTACK_RECORD_OFFSET;
    let needle_ptr = w4_needle_record_offset(long);
    let contains_fn_idx = crate::host_abi::import_index(12); // __relon_str_contains

    // acc = 0
    func.instruction(&Instruction::I64Const(0));
    func.instruction(&Instruction::LocalSet(1));
    // i = 0
    func.instruction(&Instruction::I64Const(0));
    func.instruction(&Instruction::LocalSet(2));

    // outer `block` so we can `br` out of the loop.
    func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

    // if i >= n: br outer
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::LocalGet(0));
    func.instruction(&Instruction::I64GeS);
    func.instruction(&Instruction::BrIf(1));

    // hit = __relon_str_contains(haystack_ptr, needle_ptr)
    func.instruction(&Instruction::I32Const(haystack_ptr as i32));
    func.instruction(&Instruction::I32Const(needle_ptr as i32));
    func.instruction(&Instruction::Call(contains_fn_idx));

    // acc += (i64) hit
    func.instruction(&Instruction::I64ExtendI32U); // widen i32 -> i64
    func.instruction(&Instruction::LocalGet(1));
    func.instruction(&Instruction::I64Add);
    func.instruction(&Instruction::LocalSet(1));

    // i += 1
    func.instruction(&Instruction::LocalGet(2));
    func.instruction(&Instruction::I64Const(1));
    func.instruction(&Instruction::I64Add);
    func.instruction(&Instruction::LocalSet(2));

    // br to loop head
    func.instruction(&Instruction::Br(0));
    func.instruction(&Instruction::End); // loop
    func.instruction(&Instruction::End); // block

    // return acc
    func.instruction(&Instruction::LocalGet(1));
    func.instruction(&Instruction::End); // function

    // Build data segments — `[u32 len][payload]` for haystack + needle.
    let haystack_bytes = w4_haystack_bytes(long);
    let mut haystack_record = Vec::with_capacity(4 + haystack_bytes.len());
    haystack_record.extend_from_slice(&(haystack_bytes.len() as u32).to_le_bytes());
    haystack_record.extend_from_slice(haystack_bytes);

    let mut needle_record = Vec::with_capacity(4 + W4_NEEDLE.len());
    needle_record.extend_from_slice(&(W4_NEEDLE.len() as u32).to_le_bytes());
    needle_record.extend_from_slice(W4_NEEDLE);

    let data_segments = vec![(haystack_ptr, haystack_record), (needle_ptr, needle_record)];

    finalize_module(prelude, func, &data_segments)
}
