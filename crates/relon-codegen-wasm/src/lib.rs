#![forbid(unsafe_code)]

//! Lower `relon-ir` to WebAssembly bytecode + runtime adapter (Phase 1+).
//!
//! Implements the four locked design decisions:
//!   1. Binary memory handshake for `#main` params + return
//!      (see `wasm-binary-layout-v1-2026-05-16.md`)
//!   2. Stdlib self-contained (bundled bytecode + check_cap opcode)
//!   3. Source map + ABI metadata in custom sections
//!      (see `wasm-srcmap-section-v1-2026-05-16.md`)
//!   4. Static topological eager evaluation for dict fields
//!
//! Skeleton only at this point — Phase 1 (smoke test) lands the first
//! real lowering for `#main(Int) -> Int : x * 2`.

// Phase 1 will introduce:
//   pub mod codegen;            // IR -> wasm bytecode emit
//   pub mod runtime;            // trait WasmRuntime + WasmtimeRuntime impl
//   pub mod srcmap;             // emit / parse relon.srcmap section
//   pub mod abi;                // emit / parse relon.abi section
//   pub mod host_fn_table;      // emit / parse relon.host_fns section
//   pub mod evaluator;          // impl Evaluator for WasmAotEvaluator

// Suppress unused-crate-dependencies lint while the skeleton is empty.
// Removed once Phase 1 starts pulling these in.
#[allow(unused_imports)]
use relon_eval_api as _;
#[allow(unused_imports)]
use relon_ir as _;
#[allow(unused_imports)]
use relon_parser as _;
#[allow(unused_imports)]
use thiserror as _;

use wasm_encoder::{
    CodeSection, ExportKind, ExportSection, Function, FunctionSection, Instruction, Module,
    TypeSection, ValType,
};

/// Phase 1.alpha smoke generator.
///
/// Emits a tiny, fully self-contained wasm module that exports a
/// single function `run_main : (i32) -> i32` whose body is the
/// hardcoded equivalent of `x * 2`. The goal is to prove the
/// `wasm-encoder` + `wasmtime` toolchain links end-to-end before
/// Phase 1.beta wires the real `relon-ir` lowering through it; this
/// function therefore intentionally ignores any IR input.
///
/// Body (wat-style):
/// ```text
/// (func (export "run_main") (param i32) (result i32)
///   local.get 0
///   i32.const 2
///   i32.mul)
/// ```
pub fn compile_hardcoded_double() -> Vec<u8> {
    let mut module = Module::new();

    // Type section: one signature, (i32) -> i32.
    let mut types = TypeSection::new();
    types.ty().function(vec![ValType::I32], vec![ValType::I32]);
    module.section(&types);

    // Function section: one function, type index 0.
    let mut functions = FunctionSection::new();
    let type_index = 0;
    functions.function(type_index);
    module.section(&functions);

    // Export section: function 0 exported as `run_main`.
    let mut exports = ExportSection::new();
    let func_index = 0;
    exports.export("run_main", ExportKind::Func, func_index);
    module.section(&exports);

    // Code section: body is `local.get 0; i32.const 2; i32.mul`.
    // Empty `locals` because the single i32 parameter is already
    // available as local 0.
    let mut codes = CodeSection::new();
    let locals: Vec<(u32, ValType)> = Vec::new();
    let mut f = Function::new(locals);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(2));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::End);
    codes.function(&f);
    module.section(&codes);

    module.finish()
}
