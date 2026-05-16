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

pub mod abi;
pub mod error;
pub mod srcmap;

pub use abi::{AbiError, AbiMetadata};
pub use error::CodegenError;
pub use srcmap::{Entry as SrcMapEntry, SrcMap, SrcMapError};

use relon_ir::{IrType, Module as IrModule, Op};
use relon_parser::TokenRange;
use wasm_encoder::{
    CodeSection, CustomSection, ExportKind, ExportSection, Function, FunctionSection, Ieee64,
    Instruction, Module, TypeSection, ValType,
};

// `relon-eval-api` participates in the codegen surface via the
// `Evaluator` trait wiring (see `relon-host`). Keep imported as `_`
// so the unused-crate-dependencies lint doesn't trip before the
// wasm-backed adapter lands.
#[allow(unused_imports)]
use relon_eval_api as _;

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
/// Phase 1.gamma additionally appends a `relon.srcmap` custom section
/// after the code section. The section maps every emitted wasm
/// instruction's module-absolute byte offset to the source
/// [`TokenRange`] it lowered from, plus a leading entry per function
/// pinning the function prologue to the `#main(...)` declaration
/// range. The `relon.abi` section is still deferred to Phase 2.
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

    // Per-function: the source ranges that produced each emitted wasm
    // instruction, in emit order. Length equals the number of wasm ops
    // wasmparser will read out of the function body (final `End`
    // included). We keep `Func::range` separately so the prologue
    // (locals header) maps to the function declaration.
    let mut per_func_ranges: Vec<(TokenRange, Vec<TokenRange>)> =
        Vec::with_capacity(ir.funcs.len());

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

        let (body, ranges) = emit_function_body(func)?;
        codes.function(&body);
        per_func_ranges.push((func.range, ranges));
    }

    module.section(&types);
    module.section(&functions);
    module.section(&exports);
    module.section(&codes);

    // Snapshot the module-so-far so wasmparser can resolve module-
    // absolute byte offsets for every emitted wasm instruction. The
    // custom section appended below sits after the code section, so
    // none of the offsets shift on the second pass.
    let bytes_so_far = module.as_slice().to_vec();
    let srcmap = build_srcmap(&bytes_so_far, &per_func_ranges)?;
    let srcmap_bytes = srcmap::encode_to_bytes(&srcmap);
    module.section(&CustomSection {
        name: srcmap::SECTION_NAME.into(),
        data: (&srcmap_bytes[..]).into(),
    });

    // Phase 2.a: append `relon.abi`. The codegen pipeline doesn't yet
    // accept a schema as input, so both hashes go in as 32-byte zero
    // placeholders. Phase 2.b broadens `compile_module` to take the
    // schema; until then the host loader treats zero-hash modules as
    // "ABI shape locked, schema not yet validated".
    let abi_bytes = abi::encode(&AbiMetadata::placeholder());
    module.section(&CustomSection {
        name: abi::SECTION_NAME.into(),
        data: (&abi_bytes[..]).into(),
    });

    Ok(module.finish())
}

/// Walk the emitted module's code section with `wasmparser`, line up
/// each wasm instruction with the [`TokenRange`] it lowered from, and
/// produce a sorted [`SrcMap`] ready for encoding.
///
/// The file table holds a single placeholder entry — v1.beta doesn't
/// thread source paths through IR lowering yet (the lowering pass
/// uses a hard-coded `"main"` module name for diagnostics; see
/// [`relon_ir::lower_workspace_single`]). Phase 2+ widens the
/// pipeline to multi-file workspaces, at which point this becomes a
/// real path table.
fn build_srcmap(
    module_bytes: &[u8],
    per_func: &[(TokenRange, Vec<TokenRange>)],
) -> Result<SrcMap, CodegenError> {
    let mut entries: Vec<SrcMapEntry> = Vec::new();
    let mut func_iter = per_func.iter();

    for payload in wasmparser::Parser::new(0).parse_all(module_bytes) {
        let payload =
            payload.map_err(|e| CodegenError::SrcMapEncode(format!("wasmparser error: {e}")))?;
        if let wasmparser::Payload::CodeSectionEntry(body) = payload {
            let (func_range, op_ranges) = func_iter.next().ok_or_else(|| {
                CodegenError::SrcMapEncode("more wasm function bodies than IR funcs".into())
            })?;

            // The function body's `range()` starts at the byte just
            // after the body's length prefix and covers locals + ops.
            // Pin the function declaration range there so any trap
            // landing inside the prologue resolves to the `#main(...)`
            // line rather than to the first user op.
            let body_start = body.range().start as u32;
            entries.push(token_range_to_entry(body_start, *func_range));

            let ops_reader = body
                .get_operators_reader()
                .map_err(|e| CodegenError::SrcMapEncode(format!("operators reader: {e}")))?;
            let mut op_idx = 0usize;
            for item in ops_reader.into_iter_with_offsets() {
                let (_op, offset) =
                    item.map_err(|e| CodegenError::SrcMapEncode(format!("op decode: {e}")))?;
                let range = op_ranges.get(op_idx).copied().ok_or_else(|| {
                    CodegenError::SrcMapEncode(format!(
                        "wasm body has more operators ({}) than IR op-ranges ({}) recorded",
                        op_idx + 1,
                        op_ranges.len()
                    ))
                })?;
                entries.push(token_range_to_entry(offset as u32, range));
                op_idx += 1;
            }
            if op_idx != op_ranges.len() {
                return Err(CodegenError::SrcMapEncode(format!(
                    "wasm body produced {} operators but IR recorded {} ranges",
                    op_idx,
                    op_ranges.len()
                )));
            }
        }
    }

    if func_iter.next().is_some() {
        return Err(CodegenError::SrcMapEncode(
            "fewer wasm function bodies than IR funcs".into(),
        ));
    }

    // Entries arrive already in pc-ascending order (wasmparser walks
    // the code section in declaration order, and within a body in
    // offset order). Belt-and-braces sort here so a future emitter
    // change can't silently break delta encoding.
    entries.sort_by_key(|e| e.pc);

    Ok(SrcMap {
        files: vec![SRCMAP_PLACEHOLDER_FILE.to_string()],
        entries,
    })
}

/// Placeholder file path used in the srcmap file table while the IR
/// doesn't yet thread real source paths through to codegen. Replaced
/// in Phase 2 once workspace lowering carries file ids.
const SRCMAP_PLACEHOLDER_FILE: &str = "<entry>";

/// Project a parser [`TokenRange`] to a srcmap [`SrcMapEntry`] anchored
/// at `pc`. Uses the range's start position; the closing position is
/// summarised as a character count in `range_len`.
fn token_range_to_entry(pc: u32, range: TokenRange) -> SrcMapEntry {
    let line = range.start.line;
    // `column` is `usize` in the parser; wasm srcmap stores u32. Cast
    // is safe in practice (line widths well below `u32::MAX`) and
    // matches what the parser already enforces in its own diagnostics.
    let col = range.start.column as u32;
    let range_len = range
        .end
        .offset
        .saturating_sub(range.start.offset)
        .min(u32::MAX as usize) as u32;
    SrcMapEntry {
        pc,
        // v1.beta has a single placeholder file entry; multi-file
        // support is Phase 2.
        file_idx: 0,
        line,
        col,
        range_len,
    }
}

/// Translate an IR function body into a `wasm_encoder::Function`,
/// emitting one wasm instruction per `Op` and validating the
/// per-op type tag against a virtual value stack.
///
/// Returns the encoded body plus the parallel vector of source
/// [`TokenRange`]s — one per emitted wasm instruction in emit order,
/// including the trailing `End`. The srcmap pass zips this vector
/// against `wasmparser`'s post-finish offset stream to build the
/// per-instruction `pc → range` table.
fn emit_function_body(func: &relon_ir::Func) -> Result<(Function, Vec<TokenRange>), CodegenError> {
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

    // Per-emitted-instruction source ranges, lock-step with the
    // wasm op stream the encoder builds. `Op::Return` emits no wasm
    // op so we deliberately skip it here; the trailing implicit
    // `End` records its own entry below.
    let mut ranges: Vec<TokenRange> = Vec::with_capacity(func.body.len() + 1);

    for tagged in &func.body {
        match &tagged.op {
            Op::ConstI64(v) => {
                f.instruction(&Instruction::I64Const(*v));
                vstack.push(IrType::I64);
                ranges.push(tagged.range);
            }
            Op::ConstF64(v) => {
                f.instruction(&Instruction::F64Const(Ieee64::from(v.into_inner())));
                vstack.push(IrType::F64);
                ranges.push(tagged.range);
            }
            Op::LocalGet(idx) => {
                let ty = *param_types
                    .get(*idx as usize)
                    .ok_or(CodegenError::MixedNumericTypes)?;
                f.instruction(&Instruction::LocalGet(*idx));
                vstack.push(ty);
                ranges.push(tagged.range);
            }
            Op::Add(tag) => {
                emit_arith(&mut f, &mut vstack, *tag, ArithOp::Add)?;
                ranges.push(tagged.range);
            }
            Op::Sub(tag) => {
                emit_arith(&mut f, &mut vstack, *tag, ArithOp::Sub)?;
                ranges.push(tagged.range);
            }
            Op::Mul(tag) => {
                emit_arith(&mut f, &mut vstack, *tag, ArithOp::Mul)?;
                ranges.push(tagged.range);
            }
            Op::Div(tag) => {
                emit_arith(&mut f, &mut vstack, *tag, ArithOp::Div)?;
                ranges.push(tagged.range);
            }
            Op::Mod(tag) => {
                emit_arith(&mut f, &mut vstack, *tag, ArithOp::Mod)?;
                ranges.push(tagged.range);
            }
            Op::Return => {
                // Wasm encodes "return at end" as a bare `end` —
                // the last value on the stack is the function's
                // result. The implicit final `End` below carries
                // the function's declaration range so srcmap can
                // pin a trap-at-return to the right source span.
            }
        }
    }

    // Every wasm function body ends with `end`. We tag it with the
    // function's declaration range (rather than the last op's range)
    // so a trap that lands on the terminator resolves to "the
    // function's exit", not to whatever the last computed value was.
    f.instruction(&Instruction::End);
    ranges.push(func.range);

    Ok((f, ranges))
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
