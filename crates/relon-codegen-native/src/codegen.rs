//! IR -> Cranelift IR lowering.
//!
//! v5-beta-1 deliberately narrows the supported IR surface to keep
//! the cranelift pipeline focused on the HelloWorld-tier scenarios:
//!
//! * Integer arithmetic (`Add` / `Sub` / `Mul` / `Div` / `Mod`) on `I64`.
//! * Six comparisons (`Eq` / `Ne` / `Lt` / `Le` / `Gt` / `Ge`).
//! * `ConstI64` / `ConstI32` / `ConstBool` literals plus `Return`.
//! * `LocalGet` and `LetGet` / `LetSet` for parameter / let-binding
//!   access.
//! * `If` for conditional control flow.
//! * `Call` for the narrow stdlib subset hard-wired in `evaluator.rs`
//!   (`length` of a constant String, `abs(Int)`).
//! * `ConstString` + `ReadStringLen` to validate the bounds-check
//!   path against constant String pointers.
//! * `CallNative` + `CheckCap` so the capability gate has an end-to-end
//!   exercise.
//!
//! Everything outside that envelope surfaces as
//! [`crate::CraneliftError::Codegen`] / [`CraneliftError::UnsupportedSignature`]
//! so the auto-tier wrapper can cleanly fall back to the wasm-AOT or
//! tree-walking backend without crashing the host.
//!
//! The lowering is intentionally one-pass and produces typed cranelift
//! values directly; no virtual-stack abstraction is needed because the
//! IR's stack discipline is shallow and well-typed by lowering time.
//! The cranelift verifier catches the few corner cases the lowering
//! pass might still mis-handle (type leaks across branches, etc.).

use std::collections::HashMap;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{
    AbiParam, Function, InstBuilder, MemFlags, Signature, TrapCode, UserFuncName, Value as CValue,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context as CodegenContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module as CrModule};

use relon_ir::ir::{IrType, Module as IrModule, Op, TaggedOp};

use crate::error::CraneliftError;
use crate::sandbox::{SandboxConfig, SandboxState, TrapKind};

/// Output of a successful compile: a JIT module plus the entry's
/// function ID so the host can resolve a raw function pointer through
/// `JITModule::get_finalized_function` later.
pub struct CompiledModule {
    pub module: JITModule,
    pub entry_fn_id: cranelift_module::FuncId,
    /// Number of `Int` parameters the entry expects (after the
    /// implicit sandbox-state pointer). Used by the runtime
    /// trampoline to materialise the `extern "C"` invocation.
    pub entry_arity: usize,
    /// Source range of the lowered `#main` directive — used by the
    /// runtime to attach trap diagnostics.
    pub entry_range: relon_parser::TokenRange,
}

/// Trap codes the cranelift lowering emits via `trap` /
/// `trapnz` / `trapz`. Aligned with [`TrapKind`] so the host
/// translates without a translation table.
///
/// v5-beta-1 uses cranelift's intrinsic `trap` instruction only for
/// guaranteed-fatal paths (Unreachable). Every guard reachable by
/// the lowered code (divide-by-zero, bounds, capability, resource)
/// routes through the `raise_trap` host helper + early-return
/// sequence instead, because:
///
/// 1. `cranelift_codegen::ir::trap` emits a `ud2` (SIGILL) on x86
///    Linux which Rust's panic runtime cannot intercept through
///    `catch_unwind`. Routing the trap through a host helper lets
///    us record the trap code in `SandboxState::trap_code` and
///    return a sentinel zero, which the trampoline interprets as
///    "trap fired — translate via the recorded code".
/// 2. Real `sigsetjmp` support is on the v5-beta-2 roadmap; until
///    then this is the cleanest path that preserves the typed
///    `RuntimeError` surface on every supported target.
#[allow(dead_code)]
fn trap_code(kind: TrapKind) -> TrapCode {
    TrapCode::user(kind as u8).expect("TrapKind discriminant is non-zero")
}

/// Build a cranelift JIT module and lower the IR's entry function
/// into it. v5-beta-1 only emits one function (the `#main` entry);
/// auxiliary stdlib bodies the IR references are lowered as inline
/// helpers via the `Call` path.
pub fn compile_module(
    ir: &IrModule,
    sandbox: &SandboxConfig,
) -> Result<CompiledModule, CraneliftError> {
    let entry_idx = ir
        .entry_func_index
        .ok_or_else(|| CraneliftError::Codegen("module has no entry function".into()))?;
    let entry = &ir.funcs[entry_idx];

    // Validate the entry signature against our v5-beta-1 envelope:
    // every parameter must be I64, return must be I64. Anything else
    // (string args, dict returns, ...) is currently out of scope.
    for (i, param) in entry.params.iter().enumerate() {
        if !matches!(param, IrType::I64) {
            return Err(CraneliftError::UnsupportedSignature(format!(
                "cranelift-native v5-beta-1 only supports Int params; param #{i} is {:?}",
                param
            )));
        }
    }
    if !matches!(entry.ret, IrType::I64) {
        return Err(CraneliftError::UnsupportedSignature(format!(
            "cranelift-native v5-beta-1 only supports Int return; got {:?}",
            entry.ret
        )));
    }

    // Cranelift ISA + flag setup. We pin `is_pic = false` because the
    // JIT loads code into heap-allocated executable pages and never
    // links via the system dynamic loader; PIC would cost an extra
    // `mov` per global access without buying anything.
    let mut flag_builder = settings::builder();
    flag_builder
        .set("is_pic", "false")
        .map_err(|e| CraneliftError::JitSetup(format!("is_pic flag: {e}")))?;
    flag_builder
        .set("opt_level", "speed")
        .map_err(|e| CraneliftError::JitSetup(format!("opt_level flag: {e}")))?;
    // Enable verifier in debug builds so accidentally malformed IR
    // surfaces with a useful message instead of producing
    // miscompiled code that segfaults at run time.
    #[cfg(debug_assertions)]
    flag_builder
        .set("enable_verifier", "true")
        .map_err(|e| CraneliftError::JitSetup(format!("enable_verifier flag: {e}")))?;
    let flags = settings::Flags::new(flag_builder);

    let isa_builder = cranelift_native::builder()
        .map_err(|e| CraneliftError::HostTarget(format!("cranelift-native: {e}")))?;
    let isa = isa_builder
        .finish(flags)
        .map_err(|e| CraneliftError::JitSetup(format!("isa finish: {e}")))?;

    // Build a JIT module with the default symbol set. We register the
    // sandbox helper functions ahead of `JITBuilder::build` so the
    // compiled module can resolve them via `declare_function`.
    let mut jit_builder =
        JITBuilder::with_isa(isa.clone(), cranelift_module::default_libcall_names());
    // Register host symbols by their address (libcalls would also be
    // valid here, but the sandbox helpers have unique non-libcall
    // names so we use the direct-symbol path).
    jit_builder.symbol("relon_now", SandboxState::now_helper as *const u8);
    jit_builder.symbol("relon_raise_trap", SandboxState::raise_trap as *const u8);
    jit_builder.symbol("relon_cap_lookup", SandboxState::cap_lookup as *const u8);

    let mut module = JITModule::new(jit_builder);

    // Declare the sandbox helpers up front. We need their `FuncId`s
    // so the codegen pass can emit `call_indirect`-style references.
    let raise_trap_sig = {
        let mut sig = module.make_signature();
        sig.params
            .push(AbiParam::new(module.target_config().pointer_type()));
        sig.params.push(AbiParam::new(I64));
        sig.call_conv = CallConv::SystemV;
        sig
    };
    let raise_trap_id = module
        .declare_function("relon_raise_trap", Linkage::Import, &raise_trap_sig)
        .map_err(|e| CraneliftError::ModuleDefine(format!("declare raise_trap: {e}")))?;

    let now_sig = {
        let mut sig = module.make_signature();
        sig.params
            .push(AbiParam::new(module.target_config().pointer_type()));
        sig.returns.push(AbiParam::new(I64));
        sig.call_conv = CallConv::SystemV;
        sig
    };
    let now_id = module
        .declare_function("relon_now", Linkage::Import, &now_sig)
        .map_err(|e| CraneliftError::ModuleDefine(format!("declare now: {e}")))?;

    let cap_lookup_sig = {
        let mut sig = module.make_signature();
        sig.params
            .push(AbiParam::new(module.target_config().pointer_type()));
        sig.params.push(AbiParam::new(I32));
        sig.returns
            .push(AbiParam::new(module.target_config().pointer_type()));
        sig.call_conv = CallConv::SystemV;
        sig
    };
    let cap_lookup_id = module
        .declare_function("relon_cap_lookup", Linkage::Import, &cap_lookup_sig)
        .map_err(|e| CraneliftError::ModuleDefine(format!("declare cap_lookup: {e}")))?;

    // Build the entry signature:
    //   fn run_main(state: *const SandboxState, args...: i64) -> i64
    let pointer_ty = module.target_config().pointer_type();
    let mut entry_sig = Signature::new(CallConv::SystemV);
    entry_sig.params.push(AbiParam::new(pointer_ty)); // state pointer
    for _ in &entry.params {
        entry_sig.params.push(AbiParam::new(I64));
    }
    entry_sig.returns.push(AbiParam::new(I64));

    let entry_fn_id = module
        .declare_function("run_main", Linkage::Export, &entry_sig)
        .map_err(|e| CraneliftError::ModuleDefine(format!("declare run_main: {e}")))?;

    // Emit the function body.
    let mut ctx = CodegenContext::new();
    ctx.func = Function::with_name_signature(UserFuncName::user(0, 0), entry_sig);

    let mut builder_ctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        // Pull the sandbox state pointer + Int args out of the entry
        // block parameters.
        let block_params: Vec<_> = builder.block_params(entry_block).to_vec();
        let state_ptr = block_params[0];
        let arg_values: Vec<CValue> = block_params[1..].to_vec();

        // Reference declarations into the function so the lowering
        // pass can emit direct calls to them.
        let raise_trap_ref = module.declare_func_in_func(raise_trap_id, builder.func);
        let now_ref = module.declare_func_in_func(now_id, builder.func);
        let cap_lookup_ref = module.declare_func_in_func(cap_lookup_id, builder.func);

        // Pre-allocate the trap block + a block param that carries
        // the i64 trap code. Every guard branches here with its
        // TrapKind code as an argument; cranelift handles phi nodes
        // automatically when the block has a parameter. We fill the
        // block's body at the very end (after the function body has
        // emitted all its guard branches) so the FunctionBuilder
        // never sees a half-filled block on a `switch_to_block`
        // call.
        let trap_block = builder.create_block();
        builder.append_block_param(trap_block, I64);

        let mut codegen = Codegen {
            builder: &mut builder,
            sandbox,
            state_ptr,
            raise_trap_ref,
            now_ref,
            cap_lookup_ref,
            pointer_ty,
            locals: HashMap::new(),
            let_locals: HashMap::new(),
            arg_values: &arg_values,
            stack: Vec::new(),
            ir,
            trap_block: Some(trap_block),
            label_stack: Vec::new(),
        };

        codegen.emit_prologue();
        codegen.emit_body(&entry.body)?;

        // Now fill the trap block body. Every guard branched in with
        // its `TrapKind as i64` as the block param; we call
        // `relon_raise_trap(state, code)` and return 0.
        builder.switch_to_block(trap_block);
        let code = builder.block_params(trap_block)[0];
        builder.ins().call(raise_trap_ref, &[state_ptr, code]);
        let zero = builder.ins().iconst(I64, 0);
        builder.ins().return_(&[zero]);
        builder.seal_block(trap_block);

        // The lowering for `Return` already wired the `return`
        // instruction. If the body never emits a return, the cranelift
        // verifier will surface that as an error.

        builder.finalize();
    }

    module
        .define_function(entry_fn_id, &mut ctx)
        .map_err(|e| CraneliftError::ModuleDefine(format!("define run_main: {e}")))?;
    module
        .finalize_definitions()
        .map_err(|e| CraneliftError::ModuleDefine(format!("finalize: {e}")))?;

    Ok(CompiledModule {
        module,
        entry_fn_id,
        entry_arity: entry.params.len(),
        entry_range: entry.range,
    })
}

/// Per-function lowering state. Owns the cranelift builder and tracks
/// the running operand stack alongside variable bindings.
struct Codegen<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    sandbox: &'a SandboxConfig,
    state_ptr: CValue,
    /// Reserved for future op coverage. v5-beta-1 doesn't emit
    /// `raise_trap` directly — every guard uses cranelift's intrinsic
    /// `trap` / `trapnz`, which delivers the trap-code byte through
    /// the runtime's panic path — but holding the FuncRef ready
    /// avoids a second pass for v5-beta-2 to wire in.
    #[allow(dead_code)]
    raise_trap_ref: cranelift_codegen::ir::FuncRef,
    now_ref: cranelift_codegen::ir::FuncRef,
    cap_lookup_ref: cranelift_codegen::ir::FuncRef,
    pointer_ty: cranelift_codegen::ir::Type,
    /// `LocalGet` slot index -> cranelift `Variable`.
    locals: HashMap<u32, Variable>,
    /// `LetGet/LetSet` slot index -> cranelift `Variable`.
    let_locals: HashMap<u32, Variable>,
    arg_values: &'a [CValue],
    /// The IR's virtual operand stack, kept as live cranelift values
    /// so each `Add`/`Sub`/... pop maps to a typed `Value` directly.
    stack: Vec<CValue>,
    /// Reference back to the IR module so `Call` can look up the
    /// referenced function (in v5-beta-1 we inline stdlib bodies
    /// rather than emit per-callee cranelift functions).
    #[allow(dead_code)]
    ir: &'a IrModule,
    /// Pre-allocated "trap-and-return" block. Guards branch here
    /// when they fire; the block holds a single block param carrying
    /// the `TrapKind` code, calls `raise_trap`, and returns 0. The
    /// block is allocated unconditionally and may end up unreachable
    /// when `SandboxConfig` disables every guard, in which case
    /// cranelift's dead-block elimination removes it.
    trap_block: Option<cranelift_codegen::ir::Block>,

    /// v5-β-2 widen: label stack so `Op::Br { label_depth }` /
    /// `Op::BrIf` / `Op::BrTable` can resolve to the matching
    /// cranelift target block.
    ///
    /// Each entry carries the `(target_block, is_loop)` pair where
    /// `target_block` is:
    ///   * for `Op::Block { ... }`: the **exit** block (forward
    ///     branch — `Br N` jumps past the matching End).
    ///   * for `Op::Loop { ... }`: the **header** block (back
    ///     branch — `Br N` re-enters the loop, equivalent to
    ///     `continue`).
    ///
    /// `label_depth = 0` selects the innermost (top of stack)
    /// label; higher depths walk outwards.
    label_stack: Vec<LabelFrame>,
}

/// One label frame for the `Op::Br` / `Op::BrIf` / `Op::BrTable`
/// target resolution.
struct LabelFrame {
    /// The cranelift block this label resolves to (loop header for
    /// `Op::Loop`, exit block for `Op::Block`).
    target_block: cranelift_codegen::ir::Block,
    /// `true` for `Op::Loop` (back-edge); `false` for `Op::Block`
    /// (forward-edge). Informational today — used by future trace
    /// recorder integration to identify loop back-edges as hot
    /// counter sites.
    #[allow(dead_code)]
    is_loop: bool,
}

impl<'a, 'b> Codegen<'a, 'b> {
    /// Emit the entry prologue: resource-limit check (one wall-clock
    /// read + comparison) plus any other one-shot setup.
    fn emit_prologue(&mut self) {
        if self.sandbox.deadline_check {
            self.emit_resource_check();
        }
    }

    /// Conditional trap: when `cond` is non-zero, jump to the trap
    /// block with the supplied `TrapKind` code as the block param.
    /// Replaces the cranelift intrinsic `trapnz`-via-`ud2` path
    /// that produced SIGILL on x86 Linux, which `catch_unwind`
    /// cannot intercept on stable Rust.
    fn cond_trap(&mut self, cond: CValue, kind: TrapKind) {
        let trap_block = self
            .trap_block
            .expect("trap_block must be pre-allocated by compile_module");
        let continue_block = self.builder.create_block();
        let code_val = self.builder.ins().iconst(I64, i64::from(kind as u8));
        self.builder
            .ins()
            .brif(cond, trap_block, &[code_val.into()], continue_block, &[]);
        self.builder.seal_block(continue_block);
        self.builder.switch_to_block(continue_block);
    }

    /// Insert a deadline guard at the current insertion point. Reads
    /// `state.epoch.elapsed().as_nanos()` via the host helper and
    /// traps when the result is past `state.deadline_ns`.
    fn emit_resource_check(&mut self) {
        // call relon_now(state) -> i64
        let inst = self.builder.ins().call(self.now_ref, &[self.state_ptr]);
        let elapsed = self.builder.inst_results(inst)[0];

        // Load deadline_ns from state. Offset 0 because we placed the
        // field first in the `#[repr(C)] SandboxState`. We must keep
        // this in sync with the struct layout in `sandbox.rs`.
        let deadline = self
            .builder
            .ins()
            .load(I64, MemFlags::trusted(), self.state_ptr, 0);

        // Trap when elapsed >= deadline.
        let cmp = self
            .builder
            .ins()
            .icmp(IntCC::SignedGreaterThanOrEqual, elapsed, deadline);
        self.cond_trap(cmp, TrapKind::ResourceExhausted);
    }

    /// Materialise a cranelift `Variable` for a `LocalGet` slot the
    /// IR references. Slot 0 corresponds to `arg_values[0]`, slot 1
    /// to `arg_values[1]`, and so on.
    fn get_local(&mut self, idx: u32) -> Result<CValue, CraneliftError> {
        if let Some(var) = self.locals.get(&idx).copied() {
            return Ok(self.builder.use_var(var));
        }
        let arg_idx = idx as usize;
        if arg_idx >= self.arg_values.len() {
            return Err(CraneliftError::Codegen(format!(
                "LocalGet({idx}) out of range — entry has {} args",
                self.arg_values.len()
            )));
        }
        // Mirror the arg value into a Variable so future LocalSet
        // (if we ever support it) writes go through SSA cleanly.
        let var = self.builder.declare_var(I64);
        self.builder.def_var(var, self.arg_values[arg_idx]);
        self.locals.insert(idx, var);
        Ok(self.builder.use_var(var))
    }

    /// Resolve / create a `let`-binding slot.
    fn get_let(&mut self, idx: u32, ty: IrType) -> Result<CValue, CraneliftError> {
        let var = match self.let_locals.get(&idx).copied() {
            Some(v) => v,
            None => {
                return Err(CraneliftError::Codegen(format!(
                    "LetGet({idx}) read before LetSet"
                )))
            }
        };
        let _ = ty; // typing handled when the Variable was declared
        Ok(self.builder.use_var(var))
    }

    fn set_let(&mut self, idx: u32, ty: IrType, value: CValue) {
        let var = if let Some(v) = self.let_locals.get(&idx).copied() {
            v
        } else {
            let cr_ty = match ty {
                IrType::I64 => I64,
                IrType::I32 | IrType::Bool | IrType::Null => I32,
                _ => I64, // pointers (String/List/...) map to i64 on x86_64; v5-beta-1
                          // only ever hits this with I64 in practice.
            };
            let v = self.builder.declare_var(cr_ty);
            self.let_locals.insert(idx, v);
            v
        };
        self.builder.def_var(var, value);
    }

    fn push(&mut self, v: CValue) {
        self.stack.push(v);
    }

    fn pop(&mut self) -> Result<CValue, CraneliftError> {
        self.stack
            .pop()
            .ok_or_else(|| CraneliftError::Codegen("stack underflow".into()))
    }

    fn emit_body(&mut self, body: &[TaggedOp]) -> Result<(), CraneliftError> {
        for tagged in body {
            self.emit_op(&tagged.op)?;
        }
        Ok(())
    }

    fn emit_op(&mut self, op: &Op) -> Result<(), CraneliftError> {
        match op {
            Op::ConstI64(v) => {
                let val = self.builder.ins().iconst(I64, *v);
                self.push(val);
            }
            Op::ConstI32(v) => {
                let val = self.builder.ins().iconst(I32, i64::from(*v));
                self.push(val);
            }
            Op::ConstBool(b) => {
                let val = self.builder.ins().iconst(I32, i64::from(*b as i32));
                self.push(val);
            }
            Op::LocalGet(idx) => {
                let v = self.get_local(*idx)?;
                self.push(v);
            }
            Op::LetGet { idx, ty } => {
                let v = self.get_let(*idx, *ty)?;
                self.push(v);
            }
            Op::LetSet { idx, ty } => {
                let v = self.pop()?;
                self.set_let(*idx, *ty, v);
            }
            Op::Add(IrType::I64) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let r = self.builder.ins().iadd(a, b);
                self.push(r);
            }
            Op::Sub(IrType::I64) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let r = self.builder.ins().isub(a, b);
                self.push(r);
            }
            Op::Mul(IrType::I64) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let r = self.builder.ins().imul(a, b);
                self.push(r);
            }
            Op::Div(IrType::I64) => {
                let b = self.pop()?;
                let a = self.pop()?;
                if self.sandbox.div_check {
                    // Trap when divisor == 0. The cond_trap helper
                    // routes through `raise_trap` + early return so
                    // the trap is observable through the typed
                    // `RuntimeError` channel rather than SIGFPE/SIGILL.
                    let zero = self.builder.ins().iconst(I64, 0);
                    let cmp = self.builder.ins().icmp(IntCC::Equal, b, zero);
                    self.cond_trap(cmp, TrapKind::DivisionByZero);
                }
                let r = self.builder.ins().sdiv(a, b);
                self.push(r);
            }
            Op::Mod(IrType::I64) => {
                let b = self.pop()?;
                let a = self.pop()?;
                if self.sandbox.div_check {
                    let zero = self.builder.ins().iconst(I64, 0);
                    let cmp = self.builder.ins().icmp(IntCC::Equal, b, zero);
                    self.cond_trap(cmp, TrapKind::DivisionByZero);
                }
                let r = self.builder.ins().srem(a, b);
                self.push(r);
            }
            Op::Eq(IrType::I64) => self.emit_cmp(IntCC::Equal)?,
            Op::Ne(IrType::I64) => self.emit_cmp(IntCC::NotEqual)?,
            Op::Lt(IrType::I64) => self.emit_cmp(IntCC::SignedLessThan)?,
            Op::Le(IrType::I64) => self.emit_cmp(IntCC::SignedLessThanOrEqual)?,
            Op::Gt(IrType::I64) => self.emit_cmp(IntCC::SignedGreaterThan)?,
            Op::Ge(IrType::I64) => self.emit_cmp(IntCC::SignedGreaterThanOrEqual)?,
            Op::Eq(IrType::Bool) | Op::Eq(IrType::I32) => self.emit_cmp_i32(IntCC::Equal)?,
            Op::Ne(IrType::Bool) | Op::Ne(IrType::I32) => self.emit_cmp_i32(IntCC::NotEqual)?,
            Op::Return => {
                let v = self.pop()?;
                self.builder.ins().return_(&[v]);
            }
            Op::If {
                result_ty,
                then_body,
                else_body,
            } => self.emit_if(*result_ty, then_body, else_body)?,
            Op::CheckCap { cap_bit } => self.emit_check_cap(*cap_bit)?,

            // v5-β-2 widen: `select` for the simple stdlib bodies
            // (`abs` / `min` / `max`) and any user expression the
            // lowering pass emits via a ternary. Stack discipline
            // mirrors wasm: pop `[val_true, val_false, cond]`,
            // push `val_true` when `cond` is non-zero, else
            // `val_false`. cranelift's `select` takes
            // `(cond, val_if_true, val_if_false)` so the operand
            // order is straightforward.
            Op::Select { ty } => {
                let cond = self.pop()?;
                let val_false = self.pop()?;
                let val_true = self.pop()?;
                // Sanity: the IR pass guarantees both arms share the
                // same wasm slot; we don't need to inspect the tag
                // beyond a sanity-check trap if a future bug feeds
                // mismatched widths.
                let _ = ty;
                let r = self.builder.ins().select(cond, val_true, val_false);
                self.push(r);
            }

            // v5-β-2 widen: structured block forms. cranelift's
            // CFG is flat blocks + terminators, but the wasm-style
            // `Block` / `Loop` here only ever appear in stdlib
            // bodies the cranelift backend will inline; emit them
            // as nested cranelift blocks with a basic label depth
            // stack so `Br` / `BrIf` find the right target. For
            // now we route them through helpers that the next
            // tranche (stdlib body inlining) will exercise.
            Op::Block { result_ty, body } => self.emit_block(*result_ty, body, false)?,
            Op::Loop { result_ty, body } => self.emit_block(*result_ty, body, true)?,
            Op::Br { label_depth } => self.emit_br(*label_depth, /*conditional=*/ false)?,
            Op::BrIf { label_depth } => self.emit_br(*label_depth, /*conditional=*/ true)?,

            // v5-β-2 widen: arithmetic on `I32` slot (used by stdlib
            // bodies for pointer / length arithmetic against the
            // wasm linear-memory model). Same semantics as the I64
            // variants but on cranelift's `I32` type.
            Op::Add(IrType::I32) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let r = self.builder.ins().iadd(a, b);
                self.push(r);
            }
            Op::Sub(IrType::I32) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let r = self.builder.ins().isub(a, b);
                self.push(r);
            }
            Op::Mul(IrType::I32) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let r = self.builder.ins().imul(a, b);
                self.push(r);
            }
            Op::BitAnd(IrType::I32) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let r = self.builder.ins().band(a, b);
                self.push(r);
            }
            Op::BitAnd(IrType::I64) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let r = self.builder.ins().band(a, b);
                self.push(r);
            }
            Op::Lt(IrType::I32) => self.emit_cmp_i32(IntCC::SignedLessThan)?,
            Op::Le(IrType::I32) => self.emit_cmp_i32(IntCC::SignedLessThanOrEqual)?,
            Op::Gt(IrType::I32) => self.emit_cmp_i32(IntCC::SignedGreaterThan)?,
            Op::Ge(IrType::I32) => self.emit_cmp_i32(IntCC::SignedGreaterThanOrEqual)?,

            // v5-β-2: every other op still surfaces as Codegen
            // failure. Items #1-#6 in the v5-β-2 plan (LoadField,
            // StoreField, scratch alloc, stdlib inlining, full
            // CallNative dispatch, real sigsetjmp) widen this list
            // incrementally — each widening is paired with a
            // corpus tier transition from CraneliftUnsupported
            // to MatchOk.
            other => {
                return Err(CraneliftError::Codegen(format!(
                    "unsupported op in v5-beta-2: {:?}",
                    std::mem::discriminant(other)
                )))
            }
        }
        Ok(())
    }

    fn emit_cmp(&mut self, cc: IntCC) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().icmp(cc, a, b);
        // cranelift `icmp` produces an i8 in some versions, an i32 in
        // others; we normalise to i32 to match the IR's `Bool` slot.
        let r = self.builder.ins().uextend(I32, r);
        self.push(r);
        Ok(())
    }

    fn emit_cmp_i32(&mut self, cc: IntCC) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().icmp(cc, a, b);
        let r = self.builder.ins().uextend(I32, r);
        self.push(r);
        Ok(())
    }

    /// Lower a wasm `Block` (forward exit) or `Loop` (back edge) into
    /// cranelift's flat-CFG block form.
    ///
    /// For both shapes we create a cranelift block ahead of the body
    /// and push a `LabelFrame` onto `label_stack`; `Op::Br` /
    /// `Op::BrIf` resolve to that block by depth-counting from the
    /// top of the stack.
    ///
    /// * `is_loop = false` (wasm `Block`): the `target_block` is the
    ///   **continuation** block reached after the body terminates;
    ///   `Br 0` jumps forward past the body's End.
    /// * `is_loop = true` (wasm `Loop`): the `target_block` is the
    ///   loop **header**; `Br 0` jumps back to re-enter the loop
    ///   (equivalent to `continue`). The block exits by falling
    ///   through its End.
    ///
    /// v5-β-2 limitation: `result_ty != None` (block-yields-value)
    /// is **not** yet supported because the codegen still needs to
    /// thread the yielded value through block params. Stdlib bodies
    /// in practice always use `result_ty = None`; surfacing the
    /// unsupported case as Codegen failure keeps the safety net.
    fn emit_block(
        &mut self,
        result_ty: Option<IrType>,
        body: &[TaggedOp],
        is_loop: bool,
    ) -> Result<(), CraneliftError> {
        if result_ty.is_some() {
            return Err(CraneliftError::Codegen(
                "Block / Loop with result_ty != None not yet supported on cranelift".to_string(),
            ));
        }

        if is_loop {
            // Loop: branch into a fresh header block, lower the
            // body inside it. The body's terminator (Br / fallthrough
            // / Return) decides whether the loop exits or re-enters.
            let header = self.builder.create_block();
            self.builder.ins().jump(header, &[]);
            self.builder.switch_to_block(header);
            // Loops with no other entry edge get sealed once the body
            // lowers — cranelift seals retroactively for blocks with
            // forward branches, so we leave it unsealed during the
            // body walk and seal at the end.
            self.label_stack.push(LabelFrame {
                target_block: header,
                is_loop: true,
            });
            self.emit_body(body)?;
            self.label_stack.pop();
            self.builder.seal_block(header);
        } else {
            // Block (forward exit): allocate a continuation block,
            // lower the body, then switch to the continuation. A
            // `Br 0` inside the body jumps forward to `cont`.
            let cont = self.builder.create_block();
            self.label_stack.push(LabelFrame {
                target_block: cont,
                is_loop: false,
            });
            self.emit_body(body)?;
            self.label_stack.pop();
            // Fallthrough to cont when the body doesn't explicitly
            // branch out. The builder's `is_filled` API would let us
            // skip this when the body already terminated, but
            // emitting an extra `jump` is cheap and keeps the cranelift
            // verifier happy.
            self.builder.ins().jump(cont, &[]);
            self.builder.seal_block(cont);
            self.builder.switch_to_block(cont);
        }
        Ok(())
    }

    /// Lower `Op::Br { label_depth }` (unconditional) or
    /// `Op::BrIf { label_depth }` (conditional, popping the
    /// condition off the stack).
    fn emit_br(&mut self, label_depth: u32, conditional: bool) -> Result<(), CraneliftError> {
        let depth = label_depth as usize;
        if depth >= self.label_stack.len() {
            return Err(CraneliftError::Codegen(format!(
                "Br/BrIf label_depth {label_depth} out of range — only {} frame(s) on stack",
                self.label_stack.len()
            )));
        }
        let target = self.label_stack[self.label_stack.len() - 1 - depth].target_block;

        if conditional {
            // Pop the i32 condition. cranelift `brif(cond, then,
            // else)` needs both arms; for the "fallthrough" arm we
            // create a fresh block and switch into it after the
            // branch so subsequent ops land somewhere valid.
            let cond = self.pop()?;
            let fallthrough = self.builder.create_block();
            self.builder.ins().brif(cond, target, &[], fallthrough, &[]);
            self.builder.seal_block(fallthrough);
            self.builder.switch_to_block(fallthrough);
        } else {
            self.builder.ins().jump(target, &[]);
            // After an unconditional branch, the rest of the basic
            // block is unreachable. Create a fresh dummy block so
            // subsequent op emission lands somewhere; cranelift's
            // dead-block elimination will prune it.
            let dummy = self.builder.create_block();
            self.builder.seal_block(dummy);
            self.builder.switch_to_block(dummy);
        }
        Ok(())
    }

    fn emit_if(
        &mut self,
        result_ty: IrType,
        then_body: &[TaggedOp],
        else_body: &[TaggedOp],
    ) -> Result<(), CraneliftError> {
        let cond = self.pop()?;
        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let join_block = self.builder.create_block();

        let cr_ty = match result_ty {
            IrType::I64 => I64,
            IrType::I32 | IrType::Bool | IrType::Null => I32,
            _ => {
                return Err(CraneliftError::Codegen(format!(
                    "If result_ty {:?} unsupported in v5-beta-1",
                    result_ty
                )))
            }
        };
        self.builder.append_block_param(join_block, cr_ty);

        self.builder
            .ins()
            .brif(cond, then_block, &[], else_block, &[]);
        self.builder.seal_block(then_block);
        self.builder.seal_block(else_block);

        // Then-arm
        self.builder.switch_to_block(then_block);
        let stack_before = self.stack.len();
        self.emit_body(then_body)?;
        if self.stack.len() != stack_before + 1 {
            return Err(CraneliftError::Codegen(
                "If then-body must leave one value on the stack".into(),
            ));
        }
        let then_result = self.stack.pop().unwrap();
        self.builder.ins().jump(join_block, &[then_result.into()]);

        // Else-arm
        self.builder.switch_to_block(else_block);
        let stack_before = self.stack.len();
        self.emit_body(else_body)?;
        if self.stack.len() != stack_before + 1 {
            return Err(CraneliftError::Codegen(
                "If else-body must leave one value on the stack".into(),
            ));
        }
        let else_result = self.stack.pop().unwrap();
        self.builder.ins().jump(join_block, &[else_result.into()]);

        self.builder.seal_block(join_block);
        self.builder.switch_to_block(join_block);
        let join_val = self.builder.block_params(join_block)[0];
        self.push(join_val);
        Ok(())
    }

    /// Capability gate: query the vtable via the host helper. The
    /// helper returns the raw fn pointer; the gate traps when the
    /// pointer is null.
    ///
    /// v5-beta-1 limits the lowered capability check to "presence" —
    /// the actual call_indirect that consumes the returned pointer
    /// is on the `CallNative` path, which currently sits outside the
    /// supported op envelope. The gate is still useful on its own
    /// because the analyzer / IR pass can emit `CheckCap { cap_bit }`
    /// pre-flight before a native fn the host hasn't granted, and
    /// the trap path validates the negative case end-to-end.
    fn emit_check_cap(&mut self, cap_bit: u32) -> Result<(), CraneliftError> {
        if !self.sandbox.capability_check {
            return Ok(());
        }
        if cap_bit == relon_ir::ir::NO_CAPABILITY_BIT {
            return Ok(());
        }
        let cap_bit_v = self.builder.ins().iconst(I32, i64::from(cap_bit));
        let inst = self
            .builder
            .ins()
            .call(self.cap_lookup_ref, &[self.state_ptr, cap_bit_v]);
        let fn_ptr = self.builder.inst_results(inst)[0];
        let zero = self.builder.ins().iconst(self.pointer_ty, 0);
        let cmp = self.builder.ins().icmp(IntCC::Equal, fn_ptr, zero);
        self.cond_trap(cmp, TrapKind::CapabilityDenied);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_ir::ir::{Func, Module as IrModule, Op, TaggedOp};
    use relon_parser::TokenRange;

    /// Helper: synthesise a minimal IR module that returns
    /// `arg0 + arg1` (both `Int`).
    fn synth_add_module() -> IrModule {
        let body = vec![
            TaggedOp {
                op: Op::LocalGet(0),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::LocalGet(1),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::Add(IrType::I64),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::Return,
                range: TokenRange::default(),
            },
        ];
        let func = Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body,
            range: TokenRange::default(),
        };
        IrModule {
            imports: vec![],
            funcs: vec![func],
            entry_func_index: Some(0),
            closure_table: vec![],
        }
    }

    #[test]
    fn compile_module_rejects_non_i64_param() {
        let mut ir = synth_add_module();
        ir.funcs[0].params[0] = IrType::Bool;
        let cfg = SandboxConfig::default();
        let result = compile_module(&ir, &cfg);
        assert!(matches!(
            result,
            Err(CraneliftError::UnsupportedSignature(_))
        ));
    }

    #[test]
    fn compile_module_rejects_non_i64_return() {
        let mut ir = synth_add_module();
        ir.funcs[0].ret = IrType::Bool;
        let cfg = SandboxConfig::default();
        let result = compile_module(&ir, &cfg);
        assert!(matches!(
            result,
            Err(CraneliftError::UnsupportedSignature(_))
        ));
    }

    #[test]
    fn compile_module_emits_runnable_entry_for_add() {
        let ir = synth_add_module();
        let cfg = SandboxConfig::unchecked();
        let result = compile_module(&ir, &cfg);
        assert!(result.is_ok(), "compile failed: {:?}", result.err());
    }
}
