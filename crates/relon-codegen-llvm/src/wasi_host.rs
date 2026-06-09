//! P3 §2.2 — effectful `#native` host fn → wasm import lowering.
//!
//! On the **native** target an open-world `Op::CallNative` lowers to the
//! dynamic `relon_llvm_call_native` helper (resolved at MCJIT link time
//! via `engine.add_global_mapping`), and a closed-world co-compile can
//! inline a *pure* host fn body straight into the `.o`. Neither mechanism
//! is reachable inside a sandbox wasm module:
//!
//! - `relon_llvm_call_native` is a native function pointer the MCJIT
//!   engine patches in — there is no MCJIT engine behind a `wasm-ld`
//!   linked module, so the symbol would be an unresolved native import
//!   wasmtime cannot satisfy with the right `(state, idx, args, n)` ABI.
//! - host-bitcode co-compile (closed-world) now spans both targets
//!   (P3 §2.2): the wasm32 object-emit path co-compiles + inlines a
//!   *pure-compute* host fn into the unit (see
//!   `cocompile::link_and_inline_host_shim_wasm_pure_only`). But an
//!   **effectful** host fn (IO / clock / side effect) must *not* be
//!   inlined into the sandbox — it has to cross the sandbox boundary back
//!   out to the trusted host, so it keeps this WASI-import lowering even
//!   under closed-world. The pure/effectful split is keyed off the IR's
//!   capability-gate (`Op::CheckCap`) shape (`compute_effectful_imports`).
//!
//! The sandbox-correct lowering for an effectful host fn on wasm32 is a
//! **wasm import**: the module declares the host fn as an `extern`
//! (undefined) symbol and emits a plain `call @<import.name>`. The LLVM
//! WebAssembly backend lowers an undefined external call to an
//! `(import "env" "<name>" (func ...))` entry; `wasm-ld --allow-undefined`
//! (already wired in `crate::wasm_link`) keeps the import unresolved in
//! the linked module. wasmtime's `Linker::func_wrap("env", "<name>", ..)`
//! then supplies the trusted host implementation at instantiation time —
//! the call leaves the sandbox, runs host code, and returns the scalar
//! result back across the boundary.
//!
//! ## ABI across the import boundary
//!
//! The buffer-protocol operand stack carries every scalar as its 64-bit
//! bit pattern (Int / Bool ride the i64 lane; see `codegen::call`). The
//! import is therefore declared `(i64 ...n) -> i64` — identical to the
//! closed-world `declare_host_fn_direct` shape — so the host fn observes
//! the same i64-bits ABI on both targets and the wasm result is
//! bit-for-bit equal to the native dispatch. A `Unit`-returning host fn
//! declares a `void` wasm import (no result lane).
//!
//! ## What this is *not*
//!
//! - No `*state` pointer is threaded across the import boundary — the
//!   host implementation lives outside the wasm linear memory and owns
//!   its own side effects, so it needs no arena handle. (The capability
//!   gate still rides the preceding `Op::CheckCap`, lowered against the
//!   in-module `caps` bitmask before the call leaves the sandbox.)
//! - No `state.trap_code` probe: dispatch cannot "fail to resolve" the
//!   way the dynamic native helper can — an unregistered import fails at
//!   wasmtime *instantiation*, not per-call. A host fn that wants to
//!   signal a runtime error does so through its return contract, exactly
//!   as the inlined native path would.

use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum};

use relon_ir::ir::IrType;

use crate::codegen::Emit;
use crate::error::LlvmError;

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Lower an open-world `Op::CallNative` on the **wasm32** target to a
    /// direct call against a wasm `import`.
    ///
    /// `import_idx` indexes the module's validated `#native` import
    /// table; the caller (`emit_call_native`) has already checked the
    /// param/ret shapes and the scalar return envelope (I64 / Bool /
    /// Unit). This routine:
    ///   1. declares the host fn as an undefined external `(i64..)->i64`
    ///      (or `->void`) symbol — the LLVM wasm backend emits it as an
    ///      `(import "env" "<name>")`, kept unresolved by
    ///      `wasm-ld --allow-undefined`;
    ///   2. pops `n` operands, widens each to the i64 lane;
    ///   3. emits `call @<import.name>` and pushes the result.
    ///
    /// No `*state` pointer and no `trap_code` probe — see the module
    /// docs for why the sandbox boundary needs neither.
    pub(crate) fn emit_call_native_wasi(
        &mut self,
        import_idx: u32,
        n: usize,
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        let import = self.imports.get(import_idx as usize).ok_or_else(|| {
            LlvmError::Codegen(format!(
                "CallNative (wasm import) import_idx {import_idx} out of range"
            ))
        })?;

        // Declare the host fn as an undefined external symbol. The LLVM
        // WebAssembly backend turns an undefined external call into an
        // `(import "env" "<name>")`; `wasm-ld --allow-undefined` keeps it
        // unresolved so wasmtime's `Linker` supplies the implementation.
        // Reuses the same `(i64..)->i64` / `->void` shape the closed-world
        // direct path declares, so the host fn sees an identical ABI.
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let import_fn = match self.module.get_function(&import.name) {
            Some(f) => f,
            None => {
                let params: Vec<inkwell::types::BasicMetadataTypeEnum<'ctx>> =
                    import.param_tys.iter().map(|_| i64_t.into()).collect();
                let fn_ty = match import.ret_ty {
                    IrType::Unit => self.ctx.void_type().fn_type(&params, false),
                    _ => i64_t.fn_type(&params, false),
                };
                self.module.add_function(
                    &import.name,
                    fn_ty,
                    Some(inkwell::module::Linkage::External),
                )
            }
        };

        // Pop the args (last-pushed = last declaration-order arg) and
        // widen each into the i64 lane the import declares.
        let mut args: Vec<inkwell::values::IntValue<'ctx>> = Vec::with_capacity(n);
        for _ in 0..n {
            args.push(self.pop_int("CallNative (wasm import) arg")?);
        }
        args.reverse();

        let mut call_args: Vec<BasicMetadataValueEnum<'ctx>> = Vec::with_capacity(n);
        for (i, v) in args.iter().enumerate() {
            let w = v.get_type().get_bit_width();
            let v64 = if w == 64 {
                *v
            } else if w < 64 {
                self.builder
                    .build_int_z_extend(*v, i64_t, &self.next_name("wasi_arg_zext"))
                    .map_err(|e| LlvmError::Codegen(format!("CallNative wasm arg{i} zext: {e}")))?
            } else {
                return Err(LlvmError::Codegen(format!(
                    "CallNative wasm arg{i} has i{w} width outside the i64 lane"
                )));
            };
            call_args.push(v64.into());
        }

        let call_site = self
            .builder
            .build_call(import_fn, &call_args, &self.next_name("wasi_call"))
            .map_err(|e| LlvmError::Codegen(format!("CallNative wasm build_call: {e}")))?;

        match ret_ty {
            IrType::Unit => {}
            IrType::I64 => {
                let result = match call_site.try_as_basic_value() {
                    inkwell::values::ValueKind::Basic(BasicValueEnum::IntValue(v)) => v,
                    other => {
                        return Err(LlvmError::Codegen(format!(
                            "CallNative wasm import returned {other:?}, expected i64"
                        )));
                    }
                };
                self.push(result, IrType::I64);
            }
            IrType::Bool => {
                let result = match call_site.try_as_basic_value() {
                    inkwell::values::ValueKind::Basic(BasicValueEnum::IntValue(v)) => v,
                    other => {
                        return Err(LlvmError::Codegen(format!(
                            "CallNative wasm import returned {other:?}, expected i64"
                        )));
                    }
                };
                let b = self
                    .builder
                    .build_int_truncate(result, i32_t, &self.next_name("wasi_ret_bool"))
                    .map_err(|e| LlvmError::Codegen(format!("CallNative wasm ret trunc: {e}")))?;
                self.push(b, IrType::Bool);
            }
            other => {
                return Err(LlvmError::Codegen(format!(
                    "CallNative wasm ret_ty {other:?} unreachable after envelope check"
                )));
            }
        }
        Ok(())
    }
}
