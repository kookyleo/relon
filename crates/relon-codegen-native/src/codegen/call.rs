//! Call-dispatch helpers for [`super::Codegen`]:
//! `Op::Call` (stdlib inline), `Op::CallNative` (capability-gated
//! indirect dispatch), and the `Op::CheckCap` capability gate.
//!
//! The cranelift backend has no separate callee compilation unit
//! yet — `Op::Call` inlines the bundled stdlib body in place by
//! pushing a [`super::InlineFrame`] onto `inline_frames` and walking
//! the callee's `body` against the caller's emit state. `LocalGet` /
//! `LetGet/LetSet` / `Return` honour the topmost inline frame.
//!
//! `Op::CallNative` materialises the host fn pointer through
//! `relon_cap_lookup` (a host helper exposed via the capability
//! vtable), trap-checks the result against null, and emits a
//! cranelift `call_indirect` against an ad-hoc signature derived from
//! the IR-declared `(param_tys) -> ret_ty` shape.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::I32;
use cranelift_codegen::ir::{AbiParam, InstBuilder, Signature, Value as CValue};
use cranelift_codegen::isa::CallConv;

use relon_ir::ir::IrType;

use crate::error::CraneliftError;
use crate::sandbox::TrapKind;
use crate::vtable::VtableSlot;

use super::{ir_ty_to_cl, InlineFrame};

impl<'a, 'b> super::Codegen<'a, 'b> {
    /// Translate a stdlib `Op::Call` by inlining the callee's body.
    ///
    /// The IR's `Op::Call { fn_index, arg_count, param_tys, ret_ty }`
    /// is the surface for stdlib dispatch (and, in the future,
    /// user-function dispatch). The wasm backend resolves `fn_index`
    /// against the bundled stdlib + user functions and emits a wasm
    /// `call` instruction. The cranelift backend has no separate
    /// callee compilation unit yet, so v5-β-2 inlines the body in
    /// place: pop `arg_count` cranelift values off the operand
    /// stack, bind them to the callee's `params` slots, lower the
    /// callee body with an active `InlineFrame`, and continue at the
    /// exit block carrying the typed return value.
    pub(super) fn emit_call_stdlib(
        &mut self,
        fn_index: u32,
        arg_count: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), CraneliftError> {
        // Resolve the callee. The IR pass uses `fn_index = stdlib idx`
        // for bundled stdlib calls and `fn_index = N + user_fn_idx`
        // for user-defined. v5-β-2 only inlines bundled stdlib bodies
        // — fn_index that exceeds the bundled stdlib's length surfaces
        // as Codegen failure so the harness routes the case to
        // `CraneliftUnsupported`.
        let stdlib = relon_ir::stdlib::builtin_stdlib();
        let callee = stdlib.get(fn_index as usize).ok_or_else(|| {
            CraneliftError::Codegen(format!(
                "Op::Call fn_index {fn_index} outside bundled stdlib (max {})",
                stdlib.len()
            ))
        })?;

        // Sanity-check arity + param shapes against the IR's tag.
        if callee.params.len() != arg_count as usize {
            return Err(CraneliftError::Codegen(format!(
                "Op::Call to `{}` declares {} args but callee has {}",
                callee.name,
                arg_count,
                callee.params.len()
            )));
        }
        for (i, (declared, expected)) in callee.params.iter().zip(param_tys.iter()).enumerate() {
            if declared != expected {
                return Err(CraneliftError::Codegen(format!(
                    "Op::Call to `{}` arg #{i}: callee expects {declared:?}, IR tags {expected:?}",
                    callee.name
                )));
            }
        }

        // Pop the arguments off the operand stack. The IR pushes
        // them in declaration order, so the last-pushed value is the
        // last param.
        let mut args = Vec::with_capacity(arg_count as usize);
        for _ in 0..arg_count {
            args.push(self.pop()?);
        }
        args.reverse();

        // Allocate the exit block + result-carrier param.
        let exit_block = self.builder.create_block();
        let exit_ty = ir_ty_to_cl(ret_ty)?;
        self.builder.append_block_param(exit_block, exit_ty);

        // Capture the let_locals "next free slot" snapshot. Stdlib
        // bodies don't typically declare let bindings, but the
        // namespace separation is cheap and future-proofs the
        // inlining once larger callees come online. We use the max
        // currently-used index + 1; if the caller has no let
        // bindings yet, the offset is 0 and the callee's `LetSet 0`
        // maps to caller slot 0 — collision-free because no caller
        // op has run yet that touches let_locals at this nesting.
        let let_offset = self
            .let_locals
            .keys()
            .copied()
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);

        // Push the inline frame and lower the callee body. We clone
        // the body out of the stdlib vector because `emit_body`
        // takes &self mut and we can't simultaneously hold a borrow
        // into stdlib. F-D2-G: `body()` lazily forces the op stream
        // on first touch and caches it for the rest of the process —
        // the JIT pulls the cache on subsequent inlines for free.
        let body = callee.body_owned();
        self.inline_frames.push(InlineFrame {
            params: args,
            exit_block,
            ret_ty,
            let_offset,
        });
        let result = self.emit_body(&body);
        let frame = self.inline_frames.pop().expect("we just pushed one");
        result?;

        // Switch to the exit block; its block-param is the typed
        // return value, push it onto the caller's stack.
        self.builder.seal_block(frame.exit_block);
        self.builder.switch_to_block(frame.exit_block);
        let ret_val = self.builder.block_params(frame.exit_block)[0];
        self.push(ret_val);
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
    ///
    /// Policy boundary: the populated-vs-null slot decision the IR
    /// reads here is made up-stack by
    /// [`crate::sandbox::CapabilityVtable::register_via_gate`], which
    /// consults the shared [`relon_eval_api::CapabilityGate`] — the
    /// same trait the tree-walker's `check_native_fn_capability`
    /// invokes at dispatch time. Single source of policy; two
    /// enforcement-timing surfaces.
    pub(super) fn emit_check_cap(&mut self, cap_bit: u32) -> Result<(), CraneliftError> {
        if !self.sandbox.capability_check {
            return Ok(());
        }
        if cap_bit == relon_ir::ir::NO_CAPABILITY_BIT {
            return Ok(());
        }
        let cap_bit_v = self.builder.ins().iconst(I32, i64::from(cap_bit));
        let inst = self.emit_host_fn_call(VtableSlot::RelonCapLookup, &[self.state_ptr, cap_bit_v]);
        let fn_ptr = self.builder.inst_results(inst)[0];
        let zero = self.builder.ins().iconst(self.pointer_ty, 0);
        let cmp = self.builder.ins().icmp(IntCC::Equal, fn_ptr, zero);
        self.cond_trap(cmp, TrapKind::CapabilityDenied);
        Ok(())
    }

    /// Lower `Op::CallNative { import_idx, param_tys, ret_ty, cap_bit }`.
    /// Stage 5 Phase C.1: full indirect dispatch via the capability
    /// vtable.
    ///
    /// Sequence:
    ///   1. (cap_bit != NO_CAPABILITY_BIT, capability_check on)
    ///      call `cap_lookup(state, cap_bit)` to materialise the host
    ///      fn pointer.
    ///   2. Trap with `CapabilityDenied` when the returned pointer is
    ///      null (slot not registered or denied by the host posture).
    ///   3. Build a cranelift `Signature` matching the IR-declared
    ///      `(param_tys) -> ret_ty` shape; install it as a SigRef on
    ///      the current function.
    ///   4. Pop `param_tys.len()` operands off the virtual stack and
    ///      `call_indirect(sig_ref, fn_ptr, args)`.
    ///   5. Push the (single) return value if `ret_ty != Null`.
    ///
    /// ABI: every host fn is exposed as `extern "C"` (`SystemV` calling
    /// convention) — host SDKs that register fns must transmute their
    /// concrete signature to [`crate::sandbox::HostFnPtr`] (a type-
    /// erased pointer); the cranelift call-site re-shapes the slot
    /// signature based on the IR's `param_tys + ret_ty` tag. Pointer-
    /// indirect arg types (String / List*) flow through as i32 arena
    /// offsets — the host fn is responsible for re-deriving the
    /// arena base via the sandbox state pointer if it needs the raw
    /// buffer.
    pub(super) fn emit_call_native(
        &mut self,
        import_idx: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
        cap_bit: u32,
    ) -> Result<(), CraneliftError> {
        // Validate the import index. Helps surface IR-pass bugs early.
        let import = self.ir.imports.get(import_idx as usize).ok_or_else(|| {
            CraneliftError::Codegen(format!(
                "CallNative import_idx {import_idx} out of range (module has {} imports)",
                self.ir.imports.len()
            ))
        })?;
        if import.param_tys != param_tys {
            return Err(CraneliftError::Codegen(format!(
                "CallNative import #{import_idx} param shape disagreement: IR call has {:?}, import declares {:?}",
                param_tys, import.param_tys
            )));
        }
        if import.ret_ty != ret_ty {
            return Err(CraneliftError::Codegen(format!(
                "CallNative import #{import_idx} ret_ty disagreement: IR call has {:?}, import declares {:?}",
                ret_ty, import.ret_ty
            )));
        }

        // 1. cap_lookup -> fn_ptr (or null when the slot is empty).
        // Even when capability_check is OFF on the sandbox config, we
        // still need the fn pointer for the indirect call, so the
        // lookup always runs; only the null-check is gated.
        let effective_cap_bit = if cap_bit == relon_ir::ir::NO_CAPABILITY_BIT {
            // The host SDK convention is to register host fns at the
            // import's `import_idx` when no capability is required.
            // Mirror that: use `import_idx` as the lookup key so an
            // unguarded `#native` resolves to the same slot the SDK
            // populated. The vtable's `register(import_idx, fn_ptr)`
            // path is the canonical call-shape today; future host
            // SDKs may grow a separate "default cap" slot system.
            import_idx
        } else {
            cap_bit
        };
        let cap_bit_v = self.builder.ins().iconst(I32, i64::from(effective_cap_bit));
        let inst = self.emit_host_fn_call(VtableSlot::RelonCapLookup, &[self.state_ptr, cap_bit_v]);
        let fn_ptr = self.builder.inst_results(inst)[0];

        // 2. Null-check (always emitted: even with capability_check off
        //    we still need to refuse the call when the host never
        //    registered any fn at this slot; a null `call_indirect`
        //    would segfault).
        let zero = self.builder.ins().iconst(self.pointer_ty, 0);
        let cmp = self.builder.ins().icmp(IntCC::Equal, fn_ptr, zero);
        self.cond_trap(cmp, TrapKind::CapabilityDenied);

        // 3. Build the call signature mirroring (param_tys) -> ret_ty.
        let mut sig = Signature::new(CallConv::SystemV);
        for ty in param_tys {
            let cl_ty = ir_ty_to_cl(*ty)?;
            sig.params.push(AbiParam::new(cl_ty));
        }
        // Null return type means "no return value"; everything else
        // gets one return slot.
        if !matches!(ret_ty, IrType::Null) {
            let cl_ret = ir_ty_to_cl(ret_ty)?;
            sig.returns.push(AbiParam::new(cl_ret));
        }
        let sig_ref = self.builder.import_signature(sig);

        // 4. Pop args off the virtual stack (last-pushed = last arg).
        let mut args: Vec<CValue> = Vec::with_capacity(param_tys.len());
        for _ in 0..param_tys.len() {
            args.push(self.pop()?);
        }
        args.reverse();

        let call_inst = self.builder.ins().call_indirect(sig_ref, fn_ptr, &args);

        // 5. Push the return value (if any).
        if !matches!(ret_ty, IrType::Null) {
            let result = self.builder.inst_results(call_inst)[0];
            self.push(result);
        }

        Ok(())
    }
}
