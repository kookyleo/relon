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
    CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection, ImportSection,
    Instruction, MemArg, MemorySection, MemoryType, Module, TypeSection, ValType,
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
    /// — scope-cut Z.1 (needs closure + map host import).
    W2DotProduct,

    /// W3 string concat — scope-cut Z.1 (string concat surface needs
    /// the `__relon_str_concat_n` plumbing wired).
    W3StringConcat,

    /// W4 contains scan — scope-cut Z.1 (filter / list-len-of-filter
    /// composition needs IR walker).
    W4StringContains {
        /// True for the 256-byte haystack variant.
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

    /// W10 config eval — scope-cut Z.1.
    W10ConfigEval,

    /// `#main(Int x) -> Int  x + 1`. Trivial — body is one `i64.add`.
    W12IncrementInt,
}

/// Lower a program to a complete WASM module.
pub(crate) fn lower_program(program: &WasmProgram) -> Result<Vec<u8>, LowerError> {
    match program {
        WasmProgram::W1IntSumRange => Ok(emit_w1_int_sum_range()),
        WasmProgram::W2DotProduct => Err(LowerError::ScopeCut("W2-dot-product")),
        WasmProgram::W3StringConcat => Err(LowerError::ScopeCut("W3-string-concat")),
        WasmProgram::W4StringContains { .. } => Err(LowerError::ScopeCut("W4-string-contains")),
        WasmProgram::W5DictAccess => Err(LowerError::ScopeCut("W5-dict-access")),
        WasmProgram::W6ListSumPlusOne => Ok(emit_w6_list_sum_plus_one()),
        WasmProgram::W7FibRecursion => Err(LowerError::ScopeCut("W7-fib-recursion")),
        WasmProgram::W8PolymorphicDispatch => Err(LowerError::ScopeCut("W8-polymorphic-dispatch")),
        WasmProgram::W9NestedMatrix => Err(LowerError::ScopeCut("W9-nested-matrix")),
        WasmProgram::W10ConfigEval => Err(LowerError::ScopeCut("W10-config-eval")),
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
fn finalize_module(prelude: ModulePrelude, main_body: Function) -> Vec<u8> {
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

    finalize_module(prelude, func)
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

    finalize_module(prelude, func)
}

/// W12 lowering. `#main(Int x) -> Int  x + 1`.
fn emit_w12_increment_int() -> Vec<u8> {
    let prelude = build_prelude();
    let mut func = Function::new(std::iter::empty::<(u32, ValType)>());
    func.instruction(&Instruction::LocalGet(0));
    func.instruction(&Instruction::I64Const(1));
    func.instruction(&Instruction::I64Add);
    func.instruction(&Instruction::End);
    finalize_module(prelude, func)
}
