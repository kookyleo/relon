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
//! Phase 1.beta lands [`compile_module`] — the first real lowering
//! from `relon-ir`'s scalar IR to a wasm module. The Phase 1.alpha
//! [`compile_hardcoded_double`] helper stays in place as a regression
//! reference: it sidesteps the IR entirely and exercises the
//! `wasm-encoder` + `wasmtime` link in isolation.

pub mod error;

pub use error::CodegenError;

use relon_ir::{IrType, Module as IrModule, Op};
use wasm_encoder::{
    CodeSection, ExportKind, ExportSection, Function, FunctionSection, Ieee64, Instruction, Module,
    TypeSection, ValType,
};

// `relon-parser` participates in the codegen surface via `TokenRange`
// (carried by every `TaggedOp` for the Phase 1.gamma srcmap pass);
// stay imported so the unused-crate-dependencies lint doesn't trip
// before that pass lands.
#[allow(unused_imports)]
use relon_eval_api as _;
#[allow(unused_imports)]
use relon_parser as _;

/// Lower an IR [`Module`] to a wasm binary.
///
/// v1.beta scope:
///
/// * Every function's signature is recorded in a single TypeSection,
///   deduplicated by `(params, ret)` so two `(i64) -> i64` functions
///   share a type index.
/// * Each function is exported under its `Func::name` iff it is the
///   entry. Non-entry functions stay unexported (no caller exists
///   for them in Phase 1.beta).
/// * Bodies emit one wasm instruction per IR `Op`, plus the trailing
///   `end` that wasm requires.
///
/// Source map / ABI custom sections are deferred — Phase 1.gamma
/// adds `relon.srcmap`, Phase 2 adds `relon.abi`.
pub fn compile_module(ir: &IrModule) -> Result<Vec<u8>, CodegenError> {
    if ir.funcs.is_empty() {
        return Err(CodegenError::EmptyModule);
    }

    let mut module = Module::new();
    let mut types = TypeSection::new();
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut codes = CodeSection::new();

    // Type dedup table: `(param valtypes, return valtype) -> type index`.
    // Kept as a Vec because v1.beta only has a single function — O(n²)
    // is irrelevant and avoids dragging a hashing dep in.
    let mut type_table: Vec<(Vec<ValType>, ValType, u32)> = Vec::new();

    for (func_index, func) in ir.funcs.iter().enumerate() {
        let params_vt: Vec<ValType> = func.params.iter().map(ir_to_val_type).collect();
        let ret_vt = ir_to_val_type(&func.ret);

        // Reuse an existing TypeSection entry when the signature
        // already appeared, otherwise append a new one.
        let type_index = match type_table
            .iter()
            .find(|(p, r, _)| p == &params_vt && *r == ret_vt)
        {
            Some(&(_, _, idx)) => idx,
            None => {
                let idx = type_table.len() as u32;
                types.ty().function(params_vt.clone(), vec![ret_vt]);
                type_table.push((params_vt, ret_vt, idx));
                idx
            }
        };
        functions.function(type_index);

        if Some(func_index) == ir.entry_func_index {
            exports.export(func.name.as_str(), ExportKind::Func, func_index as u32);
        }

        codes.function(&emit_function_body(func)?);
    }

    module.section(&types);
    module.section(&functions);
    module.section(&exports);
    module.section(&codes);
    Ok(module.finish())
}

/// Translate an IR function body into a `wasm_encoder::Function`,
/// emitting one wasm instruction per `Op` and validating the
/// per-op type tag against a virtual value stack.
fn emit_function_body(func: &relon_ir::Func) -> Result<Function, CodegenError> {
    // No locals beyond the parameters — v1.beta has no let / where /
    // closure body, so the wasm `locals` vector stays empty.
    let mut f = Function::new(Vec::<(u32, ValType)>::new());

    // Virtual stack used to validate arithmetic type tags. Each entry
    // records the IR type the corresponding wasm op left on the
    // operand stack. Mismatches between the tag carried on `Op::Add`
    // / `Sub` / `Mul` / `Div` / `Mod` and what the stack actually
    // holds surface as `MixedNumericTypes`.
    let mut vstack: Vec<IrType> = Vec::new();
    let param_types = &func.params;

    for tagged in &func.body {
        match &tagged.op {
            Op::ConstI64(v) => {
                f.instruction(&Instruction::I64Const(*v));
                vstack.push(IrType::I64);
            }
            Op::ConstF64(v) => {
                f.instruction(&Instruction::F64Const(Ieee64::from(v.into_inner())));
                vstack.push(IrType::F64);
            }
            Op::LocalGet(idx) => {
                let ty = *param_types
                    .get(*idx as usize)
                    .ok_or(CodegenError::MixedNumericTypes)?;
                f.instruction(&Instruction::LocalGet(*idx));
                vstack.push(ty);
            }
            Op::Add(tag) => emit_arith(&mut f, &mut vstack, *tag, ArithOp::Add)?,
            Op::Sub(tag) => emit_arith(&mut f, &mut vstack, *tag, ArithOp::Sub)?,
            Op::Mul(tag) => emit_arith(&mut f, &mut vstack, *tag, ArithOp::Mul)?,
            Op::Div(tag) => emit_arith(&mut f, &mut vstack, *tag, ArithOp::Div)?,
            Op::Mod(tag) => emit_arith(&mut f, &mut vstack, *tag, ArithOp::Mod)?,
            Op::Return => {
                // Wasm encodes "return at end" as a bare `end` —
                // the last value on the stack is the function's
                // result. We just emit `end` after every op stream
                // outside this match (see below).
            }
        }
    }

    // Every wasm function body ends with `end`. The IR's `Op::Return`
    // is a no-op at emit time (the trailing `end` does the work);
    // we still want it in the IR so srcmap can pin "return" to a
    // source range later.
    f.instruction(&Instruction::End);
    Ok(f)
}

enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// Emit one binary-arithmetic instruction. Pops two operands of
/// `tag` off the virtual stack, validates the tag against what's
/// actually there, and pushes the result type back on.
fn emit_arith(
    f: &mut Function,
    vstack: &mut Vec<IrType>,
    tag: IrType,
    op: ArithOp,
) -> Result<(), CodegenError> {
    let rhs = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
    let lhs = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
    if lhs != tag || rhs != tag {
        return Err(CodegenError::MixedNumericTypes);
    }
    let instr = match (tag, op) {
        (IrType::I64, ArithOp::Add) => Instruction::I64Add,
        (IrType::I64, ArithOp::Sub) => Instruction::I64Sub,
        (IrType::I64, ArithOp::Mul) => Instruction::I64Mul,
        // Signed division/remainder — `Int` is signed in Relon.
        (IrType::I64, ArithOp::Div) => Instruction::I64DivS,
        (IrType::I64, ArithOp::Mod) => Instruction::I64RemS,
        (IrType::F64, ArithOp::Add) => Instruction::F64Add,
        (IrType::F64, ArithOp::Sub) => Instruction::F64Sub,
        (IrType::F64, ArithOp::Mul) => Instruction::F64Mul,
        (IrType::F64, ArithOp::Div) => Instruction::F64Div,
        // Wasm has no `f64.rem`. Lowering already rejects this
        // shape, but we double-check here so a hand-crafted IR
        // can't bypass it silently.
        (IrType::F64, ArithOp::Mod) => return Err(CodegenError::MixedNumericTypes),
    };
    f.instruction(&instr);
    vstack.push(tag);
    Ok(())
}

/// Map an [`IrType`] to its wasm value type.
fn ir_to_val_type(t: &IrType) -> ValType {
    match t {
        IrType::I64 => ValType::I64,
        IrType::F64 => ValType::F64,
    }
}

/// Phase 1.alpha smoke generator. Retained as a regression reference
/// so the encoder + engine smoke test survives the Phase 1.beta
/// rewrite of the rest of the file. **Not part of the v1.beta
/// pipeline** — exists solely to prove `wasm-encoder` + `wasmtime`
/// keep linking after dependency bumps.
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

    let mut types = TypeSection::new();
    types.ty().function(vec![ValType::I32], vec![ValType::I32]);
    module.section(&types);

    let mut functions = FunctionSection::new();
    functions.function(0);
    module.section(&functions);

    let mut exports = ExportSection::new();
    exports.export("run_main", ExportKind::Func, 0);
    module.section(&exports);

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
