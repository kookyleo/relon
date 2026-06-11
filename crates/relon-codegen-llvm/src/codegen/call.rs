//! `Op`-family: call dispatch.
//!
//! User-defined `Call` (direct LLVM call), bundled-stdlib inline `Call`,
//! and the `llvm.trap` guard call.
//!
//! Phase 0b widens the family with two source-trap / capability ops:
//!
//! - [`Op::CheckCap`] — the capability gate. The buffer-protocol entry
//!   carries the host-granted capability set as its trailing `i64 caps`
//!   bitmask param (IR `LocalGet(4)` / LLVM param 5). `Op::CheckCap {
//!   cap_bit }` tests bit `cap_bit` of that mask and traps when it is
//!   clear. This mirrors the wasm backend's bitmask convention — the
//!   `cap_bit` value is a `CapabilityBit::bit_index()` (0..=5), the same
//!   numeric bit the cranelift backend reuses as a `cap_lookup` vtable
//!   slot key, so all three backends gate on the same numeric bit.
//! - [`Op::Trap`] — an unconditional `llvm.trap` + `unreachable`,
//!   mirroring cranelift's `emit_trap` (unconditional `cond_trap`).
//!
//! `Op::CallNative` (Phase 0b) lowers to open-world dynamic dispatch:
//! the source-lowered call (`cap_bit == NO_CAPABILITY_BIT`) spills its
//! scalar args into an `alloca` and calls the externally-mapped
//! `relon_llvm_call_native` helper, which resolves the
//! `import_idx`-keyed `Arc<dyn RelonFunction>` on the per-call
//! `ArenaState`'s host-fn registry and invokes it. A non-zero
//! `state.trap_code` after the call routes to `llvm.trap`. This mirrors
//! cranelift's `emit_call_native_dynamic` / `RelonCallNative` vtable
//! slot. The capability gate rides on the preceding `Op::CheckCap`. A
//! hand-built `cap_bit != NO_CAPABILITY_BIT` direct-vtable call stays a
//! precise `Codegen` error (the legacy raw-`HostFnPtr` path is not
//! wired on this backend yet).

use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum};
use inkwell::{AddressSpace, IntPredicate};

use relon_ir::ir::{IrType, Op, TrapKind, NO_CAPABILITY_BIT};

use crate::error::LlvmError;
use crate::state::{NativeTrap, ARENA_STATE_OFFSET_TRAP_CODE};

use super::*;

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Phase 0b seam: native dispatch + capability gate + trap
    /// (`CallNative`, `CheckCap`, `Trap`). Dispatched from
    /// `super::lower_op`. All three are lowered here in full —
    /// `CallNative` via the dynamic-dispatch helper (see
    /// [`Self::emit_call_native`]).
    pub(crate) fn lower_call_rest(
        &mut self,
        ip: usize,
        _ip_hint: &str,
        op: &Op,
    ) -> Result<(), LlvmError> {
        match op {
            Op::CheckCap { cap_bit } => self.emit_check_cap(*cap_bit),
            Op::Trap { kind } => self.emit_trap(*kind),
            Op::CallNative {
                import_idx,
                param_tys,
                ret_ty,
                cap_bit,
            } => self.emit_call_native(*import_idx, param_tys, *ret_ty, *cap_bit),
            other => Err(LlvmError::Codegen(format!(
                "lower_call_rest reached non-call op {other:?} at ip={ip}"
            ))),
        }
    }

    /// Capability gate: `Op::CheckCap { cap_bit }`.
    ///
    /// The buffer-protocol entry's trailing `i64 caps` param (IR
    /// `LocalGet(4)`, LLVM param `param_base + 4`) is the host-granted
    /// capability bitmask. The gate tests bit `cap_bit`; a clear bit
    /// routes to an `llvm.trap` + `unreachable` trap block, mirroring
    /// cranelift's `cond_trap(fn_ptr == null, CapabilityDenied)`. The
    /// numeric `cap_bit` is a [`relon_cap::CapabilityBit::bit_index`]
    /// (0..=5) — identical across the three backends; only the lookup
    /// mechanism (bitmask here, `cap_lookup` vtable slot on cranelift)
    /// differs. (`bit_index` is `relon_cap::CapabilityBit::bit_index`.)
    ///
    /// `NO_CAPABILITY_BIT` elides the gate (cranelift / source lowering
    /// use the sentinel for "no capability required").
    ///
    /// The gate is only meaningful under the buffer-protocol entry shape
    /// — the legacy-i64 `(I64...) -> I64` entry has no `caps` slot, so a
    /// `CheckCap` there surfaces as a `Codegen` error rather than
    /// silently reading an out-of-range param.
    pub(crate) fn emit_check_cap(&mut self, cap_bit: u32) -> Result<(), LlvmError> {
        if cap_bit == NO_CAPABILITY_BIT {
            return Ok(());
        }
        if !matches!(self.shape, EntryShape::Buffer) {
            return Err(LlvmError::Codegen(format!(
                "Op::CheckCap {{ cap_bit: {cap_bit} }} requires the buffer-protocol entry \
                 (the legacy-i64 entry carries no `caps` slot)"
            )));
        }
        if cap_bit >= 64 {
            return Err(LlvmError::Codegen(format!(
                "Op::CheckCap cap_bit {cap_bit} out of range for the i64 caps bitmask"
            )));
        }

        // The `caps` bitmask is IR local 4 (buffer entry handshake slots
        // are LocalGet(0..=3); LocalGet(4) is the i64 caps word).
        let caps = self.lookup_param(4)?;
        if caps.get_type().get_bit_width() != 64 {
            return Err(LlvmError::Codegen(format!(
                "Op::CheckCap: caps param is i{} not i64; buffer entry shape changed?",
                caps.get_type().get_bit_width()
            )));
        }
        let i64_t = self.ctx.i64_type();
        // mask = 1 << cap_bit ; granted = (caps & mask) != 0.
        let mask = i64_t.const_int(1u64 << cap_bit, false);
        let masked = self
            .builder
            .build_and(caps, mask, &self.next_name("cap_mask"))
            .map_err(|e| LlvmError::Codegen(format!("CheckCap and: {e}")))?;
        let zero = i64_t.const_zero();
        let denied = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                masked,
                zero,
                &self.next_name("cap_denied"),
            )
            .map_err(|e| LlvmError::Codegen(format!("CheckCap cmp: {e}")))?;

        let trap_bb = self.ctx.append_basic_block(self.func, "cap_denied_trap");
        let cont_bb = self.ctx.append_basic_block(self.func, "cap_granted");
        self.builder
            .build_conditional_branch(denied, trap_bb, cont_bb)
            .map_err(|e| LlvmError::Codegen(format!("CheckCap branch: {e}")))?;

        // Trap arm: record `CapabilityDenied` in `state.trap_code` and
        // return the negative sentinel so the host surfaces a typed
        // `RuntimeError::CapabilityDenied` after the dispatch returns —
        // rather than an `llvm.trap` `ud2` (SIGILL) the host cannot
        // catch on stable Rust. Mirrors cranelift's
        // `cond_trap(CapabilityDenied)` outcome class. The host reads
        // `ArenaState::trap_code()` before decoding the buffer.
        self.builder.position_at_end(trap_bb);
        self.emit_state_trap(NativeTrap::CapabilityDenied, "CheckCap")?;

        // Continue codegen on the granted path.
        self.builder.position_at_end(cont_bb);
        Ok(())
    }

    /// Lower `Op::Trap { kind }`: an unconditional `llvm.trap` +
    /// `unreachable`. Mirrors cranelift's `emit_trap`
    /// (`cond_trap(iconst 1, kind)`) — the source lowering emits this
    /// for `IndexOutOfBounds` / `EmptyList` sentinels. The `kind` is
    /// diagnostic only on the LLVM path: `llvm.trap` lowers to a single
    /// `ud2`, so the host observes one undifferentiated trap regardless
    /// of `kind` (matching the divmod guard's behaviour).
    ///
    /// After the terminator we open a fresh dead continuation block so
    /// any trailing ops the body emits (the IR marks post-`Trap` ops
    /// unreachable, but a stray `ConstBool` for a typed `If` arm can
    /// still appear) have somewhere valid to land. Mirrors the
    /// post-`Return` / post-`Br` dummy-block pattern in `control.rs`.
    pub(crate) fn emit_trap(&mut self, kind: TrapKind) -> Result<(), LlvmError> {
        // The no-match `match` trap must surface a *typed*
        // `RuntimeError::TypeMismatch` (byte-aligned with the tree-walk
        // oracle), which `llvm.trap` / `ud2` cannot do — a SIGILL aborts
        // the process with no decodable cause. Route it through the same
        // `state.trap_code` + negative-sentinel epilogue the `CheckCap`
        // deny path uses; `run_buffer_main` lifts the recorded code via
        // `NativeTrap::runtime_error_from_code`. The checked-reduction
        // overflow (`list_int_sum`'s per-iteration guard) takes the same
        // route, lifting to `RuntimeError::NumericOverflow` byte-aligned
        // with the tree-walk oracle's checked `+` and cranelift's
        // `TrapKind::NumericOverflow`.
        //
        // The state-trap epilogue is `ret i32 -1` — the negative
        // bytes-written sentinel, which is only well-typed (and only
        // *means* "trap") in the buffer-protocol entry. Inside an
        // emitted helper / lambda body (`EntryShape::LegacyI64`, raw
        // scalar return) that return would either fail the LLVM
        // verifier (i64 return) or — worse — hand the caller `-1` as a
        // legitimate value (i32 Bool/handle return). There these kinds
        // keep the unconditional `llvm.trap` every other stdlib-domain
        // trap (`EmptyList` / `IndexOutOfBounds`) emits in any position:
        // a loud process abort, never a silently wrong value. Threading
        // a typed trap channel through helper returns is the recorded
        // follow-up alongside the stdlib-domain traps.
        if self.shape == EntryShape::Buffer {
            let typed = match kind {
                TrapKind::NoMatch => Some((NativeTrap::NoMatch, "Trap(NoMatch)")),
                TrapKind::NumericOverflow => {
                    Some((NativeTrap::NumericOverflow, "Trap(NumericOverflow)"))
                }
                _ => None,
            };
            if let Some((code, hint)) = typed {
                self.emit_state_trap(code, hint)?;
                let cont = self.ctx.append_basic_block(self.func, "after_trap_cont");
                self.builder.position_at_end(cont);
                return Ok(());
            }
        }
        self.emit_llvm_trap_call("Trap")?;
        self.builder
            .build_unreachable()
            .map_err(|e| LlvmError::Codegen(format!("Op::Trap unreachable: {e}")))?;
        let cont = self.ctx.append_basic_block(self.func, "after_trap_cont");
        self.builder.position_at_end(cont);
        Ok(())
    }

    /// Lower `Op::CallNative { import_idx, param_tys, ret_ty, cap_bit }`
    /// via open-world dynamic dispatch — the LLVM mirror of cranelift's
    /// `emit_call_native_dynamic`.
    ///
    /// Sequence (matches cranelift):
    ///   1. validate `import_idx` / param shape / ret-ty against the
    ///      module's `#native` import table;
    ///   2. pop `param_tys.len()` operands, widen each to i64, spill
    ///      them into an `alloca` block (the i64 lane the helper decodes);
    ///   3. `call @relon_llvm_call_native(state, import_idx, args_ptr,
    ///      count)`;
    ///   4. load `state.trap_code`; a non-zero value branches to an
    ///      `llvm.trap` (surfaces as a typed host error);
    ///   5. push the i64 result if `ret_ty != Unit`.
    ///
    /// Scope (phase-0b parity with the bytecode VM + the cranelift
    /// dynamic path): scalar args ride the i64 lane; `ret_ty` must be
    /// `I64` / `Bool` / `Unit`. The capability gate is enforced
    /// independently by the preceding `Op::CheckCap` op (which the
    /// source lowering always prepends for a gated call); a
    /// source-lowered `Op::CallNative` carries `cap_bit ==
    /// NO_CAPABILITY_BIT` and dispatches dynamically. A hand-built
    /// `cap_bit != NO_CAPABILITY_BIT` (the cranelift legacy raw-`HostFnPtr`
    /// direct path) is not wired on the LLVM backend yet — it surfaces a
    /// precise `Codegen` error so the host falls back rather than
    /// miscompiling.
    pub(crate) fn emit_call_native(
        &mut self,
        import_idx: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
        cap_bit: u32,
    ) -> Result<(), LlvmError> {
        // Open-world dynamic dispatch only makes sense under the
        // buffer-protocol entry — that's the only shape carrying the
        // `*state` pointer the `relon_llvm_call_native` helper needs.
        // The closed-world direct path (Stage 1.B) needs no state
        // pointer (it emits a plain `call @<host_symbol>`), so it is
        // accepted on the legacy-i64 entry too — the spike fixture rides
        // the legacy shape so the linked + inlined module JITs without
        // the buffer arena handshake.
        if matches!(self.world_mode, super::WorldMode::OpenWorld)
            && !matches!(self.shape, EntryShape::Buffer)
        {
            return Err(LlvmError::Codegen(format!(
                "Op::CallNative (import_idx={import_idx}) requires the buffer-protocol entry \
                 (only it threads the `*state` pointer the host-dispatch helper needs)"
            )));
        }

        // The hand-built direct-`cap_bit` path (cranelift's legacy
        // raw-`HostFnPtr` `cap_lookup` + `call_indirect`) is not wired
        // on the LLVM backend. Source-lowered calls always carry
        // `NO_CAPABILITY_BIT` and ride the dynamic helper below.
        if cap_bit != NO_CAPABILITY_BIT {
            return Err(LlvmError::Codegen(format!(
                "Op::CallNative (import_idx={import_idx}) with cap_bit={cap_bit} (direct \
                 capability-vtable dispatch) is unsupported on the LLVM AOT backend; only the \
                 source-lowered NO_CAPABILITY_BIT dynamic-dispatch path is wired"
            )));
        }

        // 1. Validate the import index + shapes against the module's
        //    `#native` table (mirrors cranelift's `self.ir.imports`
        //    check). Surfaces IR-pass bugs early.
        let import = self.imports.get(import_idx as usize).ok_or_else(|| {
            LlvmError::Codegen(format!(
                "CallNative import_idx {import_idx} out of range (module has {} imports)",
                self.imports.len()
            ))
        })?;
        if import.param_tys != param_tys {
            return Err(LlvmError::Codegen(format!(
                "CallNative import #{import_idx} param shape disagreement: IR call has {:?}, import declares {:?}",
                param_tys, import.param_tys
            )));
        }
        if import.ret_ty != ret_ty {
            return Err(LlvmError::Codegen(format!(
                "CallNative import #{import_idx} ret_ty disagreement: IR call has {:?}, import declares {:?}",
                ret_ty, import.ret_ty
            )));
        }
        if !matches!(ret_ty, IrType::I64 | IrType::Bool | IrType::Unit) {
            return Err(LlvmError::Codegen(format!(
                "CallNative dynamic dispatch (import #{import_idx}) ret_ty {ret_ty:?} outside the \
                 phase-0b scalar envelope (I64 / Bool / Unit only)"
            )));
        }

        // Stage 1.B: closed-world co-compile lowers `Op::CallNative` to
        // a static `call @<host_symbol>` (cranelift's *static* cap_lookup
        // -> fn_ptr arm), not the open-world dynamic helper. The host
        // bitcode is linked in + inlined by `crate::cocompile`. Splits
        // here so the open-world helper path below is untouched.
        if matches!(self.world_mode, super::WorldMode::ClosedWorld) {
            // P3 §2.2 wasm closed-world: route per-import. An **effectful**
            // host fn (capability-gated — `effectful_imports[idx] == true`)
            // must NOT be inlined into the sandbox; it crosses the boundary
            // as a **wasm import** (`emit_call_native_wasi`). A pure-compute
            // host fn co-compiles + inlines via the direct path, mirroring
            // the native closed-world inline. On the native closed-world
            // path `effectful_imports` is empty, so every call takes the
            // direct path exactly as before.
            let effectful = self
                .effectful_imports
                .get(import_idx as usize)
                .copied()
                .unwrap_or(false);
            if matches!(self.target, crate::CodegenTarget::Wasm32) && effectful {
                return self.emit_call_native_wasi(import_idx, param_tys.len(), ret_ty);
            }
            return self.emit_call_native_direct(import_idx, param_tys.len(), ret_ty);
        }

        // P3 §2.2: on wasm32 the open-world dynamic helper
        // (`relon_llvm_call_native`) is unreachable — there is no MCJIT
        // engine behind a `wasm-ld` linked module to patch the symbol in.
        // An **effectful** host fn must cross the sandbox boundary back
        // out to the trusted host, so we lower the call to a **wasm
        // import** (declared external, kept unresolved by
        // `wasm-ld --allow-undefined`, satisfied by wasmtime's `Linker`).
        // See `crate::wasi_host`.
        if matches!(self.target, crate::CodegenTarget::Wasm32) {
            return self.emit_call_native_wasi(import_idx, param_tys.len(), ret_ty);
        }

        let call_native_fn = self.call_native_fn.ok_or_else(|| {
            LlvmError::Codegen(
                "Op::CallNative: relon_llvm_call_native helper not declared (emit_module_funcs \
                 forgot to wire it for the buffer entry)"
                    .into(),
            )
        })?;
        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen("Op::CallNative: buffer entry missing state pointer".into())
        })?;

        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());
        let i8_t = self.ctx.i8_type();
        let n = param_tys.len();

        // 2. Pop the args (last-pushed = last declaration-order arg) and
        //    widen each to i64 (the lane the helper decodes).
        let mut args: Vec<IntValue<'ctx>> = Vec::with_capacity(n);
        for _ in 0..n {
            args.push(self.pop_int("CallNative arg")?);
        }
        args.reverse();

        let mut widened: Vec<IntValue<'ctx>> = Vec::with_capacity(n);
        for (i, v) in args.iter().enumerate() {
            let w = v.get_type().get_bit_width();
            let v64 = if w == 64 {
                *v
            } else if w < 64 {
                // Scalar IR args ride as i32 (Bool / I32) — zero-extend
                // into the i64 lane. The host fn re-wraps as Value::Int.
                self.builder
                    .build_int_z_extend(*v, i64_t, &self.next_name("cn_arg_zext"))
                    .map_err(|e| LlvmError::Codegen(format!("CallNative arg{i} zext: {e}")))?
            } else {
                return Err(LlvmError::Codegen(format!(
                    "CallNative arg{i} has i{w} width outside the phase-0b i64 lane"
                )));
            };
            widened.push(v64);
        }

        // Spill the widened args into an alloca block (8-byte-aligned
        // i64 slots) — the contiguous layout `relon_llvm_call_native`
        // decodes. `args_ptr = null` for the nullary case. The alloca is
        // placed in the entry block so mem2reg / SROA can promote it.
        let args_ptr = if n == 0 {
            ptr_t.const_null()
        } else {
            let arr_ty = i64_t.array_type(n as u32);
            let entry_bb = self.func.get_first_basic_block().ok_or_else(|| {
                LlvmError::Codegen("CallNative: function has no entry block".into())
            })?;
            let cur = self.builder.get_insert_block();
            if let Some(first) = entry_bb.get_first_instruction() {
                self.builder.position_before(&first);
            } else {
                self.builder.position_at_end(entry_bb);
            }
            let slot = self
                .builder
                .build_alloca(arr_ty, &self.next_name("cn_args"))
                .map_err(|e| LlvmError::Codegen(format!("CallNative args alloca: {e}")))?;
            if let Some(bb) = cur {
                self.builder.position_at_end(bb);
            }
            for (i, v) in widened.iter().enumerate() {
                let gep = unsafe {
                    self.builder
                        .build_in_bounds_gep(
                            i64_t,
                            slot,
                            &[i32_t.const_int(i as u64, false)],
                            &self.next_name("cn_arg_gep"),
                        )
                        .map_err(|e| LlvmError::Codegen(format!("CallNative arg{i} gep: {e}")))?
                };
                self.builder
                    .build_store(gep, *v)
                    .map_err(|e| LlvmError::Codegen(format!("CallNative arg{i} store: {e}")))?;
            }
            slot
        };

        // 3. call @relon_llvm_call_native(state, import_idx, args_ptr, count)
        let import_v = i32_t.const_int(u64::from(import_idx), false);
        let count_v = i32_t.const_int(n as u64, false);
        let call_args: [BasicMetadataValueEnum<'ctx>; 4] = [
            state_ptr.into(),
            import_v.into(),
            args_ptr.into(),
            count_v.into(),
        ];
        let call_site = self
            .builder
            .build_call(call_native_fn, &call_args, &self.next_name("cn_call"))
            .map_err(|e| LlvmError::Codegen(format!("CallNative build_call: {e}")))?;
        let result = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(BasicValueEnum::IntValue(v)) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "CallNative: helper returned {other:?}, expected i64"
                )));
            }
        };

        // 4. load `state.trap_code` (the helper records a NativeTrap
        //    code on failure); a non-zero value means the dispatch
        //    failed. Return the negative sentinel so the host surfaces a
        //    typed `RuntimeError` after the dispatch (the helper already
        //    stored the precise code) — no `llvm.trap` / SIGILL.
        let trap_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_TRAP_CODE), false)],
                    &self.next_name("cn_trap_gep"),
                )
                .map_err(|e| LlvmError::Codegen(format!("CallNative trap_code gep: {e}")))?
        };
        let trap_code = self
            .builder
            .build_load(i64_t, trap_gep, &self.next_name("cn_trap_code"))
            .map_err(|e| LlvmError::Codegen(format!("CallNative trap_code load: {e}")))?
            .into_int_value();
        let zero = i64_t.const_zero();
        let trapped = self
            .builder
            .build_int_compare(
                IntPredicate::NE,
                trap_code,
                zero,
                &self.next_name("cn_trapped"),
            )
            .map_err(|e| LlvmError::Codegen(format!("CallNative trap cmp: {e}")))?;
        let trap_bb = self.ctx.append_basic_block(self.func, "cn_trap");
        let cont_bb = self.ctx.append_basic_block(self.func, "cn_cont");
        self.builder
            .build_conditional_branch(trapped, trap_bb, cont_bb)
            .map_err(|e| LlvmError::Codegen(format!("CallNative trap branch: {e}")))?;
        self.builder.position_at_end(trap_bb);
        // The helper already wrote the precise code; just return the
        // negative sentinel without overwriting it.
        self.emit_trap_sentinel_return("CallNative")?;
        self.builder.position_at_end(cont_bb);

        // 5. Push the result. The IR's scalar return types (I64 / Bool)
        //    all ride the i64 lane; truncate Bool back to i32 to match
        //    the operand-stack width convention.
        match ret_ty {
            IrType::Unit => {}
            IrType::I64 => self.push(result, IrType::I64),
            IrType::Bool => {
                let b = self
                    .builder
                    .build_int_truncate(result, i32_t, &self.next_name("cn_ret_bool"))
                    .map_err(|e| LlvmError::Codegen(format!("CallNative ret trunc: {e}")))?;
                self.push(b, IrType::Bool);
            }
            other => {
                return Err(LlvmError::Codegen(format!(
                    "CallNative ret_ty {other:?} unreachable after envelope check"
                )));
            }
        }
        Ok(())
    }

    /// Stage 1.B closed-world direct dispatch for `Op::CallNative`.
    ///
    /// Emits a static `call @<host_symbol>` against the `extern`
    /// declaration `emit_module_funcs_closed_world` pre-declared from
    /// the `#native` import table (mirrors cranelift's *static*
    /// `cap_lookup -> fn_ptr -> call_indirect` arm, but resolved fully
    /// statically since the host-fn set is closed at emit time). The
    /// host bitcode is linked in + inlined by [`crate::cocompile`], so
    /// after the O3 / LTO pass this site collapses to the host fn body
    /// (zero residual `call`).
    ///
    /// All scalar args / returns ride the i64 lane (`Bool` / `I32`
    /// zero-extend in; the i64-bits convention `relon_llvm_call_native`
    /// also decodes — so the closed-world result is bit-for-bit equal to
    /// the open-world path). No `state.trap_code` probe: a co-compiled
    /// host fn is trusted (capability gating, if any, rides on the
    /// preceding `Op::CheckCap`), so there is no dynamic dispatch failure
    /// to surface.
    fn emit_call_native_direct(
        &mut self,
        import_idx: u32,
        n: usize,
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        let import = self.imports.get(import_idx as usize).ok_or_else(|| {
            LlvmError::Codegen(format!(
                "CallNative (closed-world) import_idx {import_idx} out of range"
            ))
        })?;
        let host_fn = self.module.get_function(&import.name).ok_or_else(|| {
            LlvmError::Codegen(format!(
                "Op::CallNative (closed-world): host fn `{}` not declared \
                 (emit_module_funcs_closed_world forgot to declare it)",
                import.name
            ))
        })?;

        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();

        // Pop the args (last-pushed = last declaration-order arg) and
        // widen each to the i64 lane, matching the host shim ABI.
        let mut args: Vec<IntValue<'ctx>> = Vec::with_capacity(n);
        for _ in 0..n {
            args.push(self.pop_int("CallNative arg")?);
        }
        args.reverse();

        let mut call_args: Vec<BasicMetadataValueEnum<'ctx>> = Vec::with_capacity(n);
        for (i, v) in args.iter().enumerate() {
            let w = v.get_type().get_bit_width();
            let v64 = if w == 64 {
                *v
            } else if w < 64 {
                self.builder
                    .build_int_z_extend(*v, i64_t, &self.next_name("cn_direct_zext"))
                    .map_err(|e| LlvmError::Codegen(format!("CallNative arg{i} zext: {e}")))?
            } else {
                return Err(LlvmError::Codegen(format!(
                    "CallNative arg{i} has i{w} width outside the i64 lane"
                )));
            };
            call_args.push(v64.into());
        }

        let call_site = self
            .builder
            .build_call(host_fn, &call_args, &self.next_name("cn_direct_call"))
            .map_err(|e| LlvmError::Codegen(format!("CallNative direct build_call: {e}")))?;

        // Push the result (if any). I64 / Bool both ride the i64 lane;
        // Bool truncates back to the i32 operand-stack width.
        match ret_ty {
            IrType::Unit => {}
            IrType::I64 => {
                let result = match call_site.try_as_basic_value() {
                    inkwell::values::ValueKind::Basic(BasicValueEnum::IntValue(v)) => v,
                    other => {
                        return Err(LlvmError::Codegen(format!(
                            "CallNative direct: host fn returned {other:?}, expected i64"
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
                            "CallNative direct: host fn returned {other:?}, expected i64"
                        )));
                    }
                };
                let b = self
                    .builder
                    .build_int_truncate(result, i32_t, &self.next_name("cn_direct_bool"))
                    .map_err(|e| LlvmError::Codegen(format!("CallNative direct ret trunc: {e}")))?;
                self.push(b, IrType::Bool);
            }
            other => {
                return Err(LlvmError::Codegen(format!(
                    "CallNative direct ret_ty {other:?} unreachable after envelope check"
                )));
            }
        }
        Ok(())
    }

    /// Phase 0b shared trap epilogue: store `code` into
    /// `state.trap_code`, then `ret i32 -1`. Used by the `Op::CheckCap`
    /// deny path. The negative bytes-written sentinel signals the host
    /// to read `ArenaState::trap_code()` and lift it to a typed
    /// `RuntimeError` instead of decoding the (unwritten) output buffer.
    /// Requires the buffer-protocol entry (caller asserts `state_ptr`).
    /// After the terminator the caller positions at a fresh continuation
    /// block.
    fn emit_state_trap(&mut self, code: NativeTrap, op_hint: &str) -> Result<(), LlvmError> {
        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen(format!("{op_hint}: state trap requires the buffer entry"))
        })?;
        let i8_t = self.ctx.i8_type();
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_TRAP_CODE), false)],
                    &self.next_name("trap_store_gep"),
                )
                .map_err(|e| LlvmError::Codegen(format!("{op_hint} trap_code gep: {e}")))?
        };
        self.builder
            .build_store(gep, i64_t.const_int(code as u64, false))
            .map_err(|e| LlvmError::Codegen(format!("{op_hint} trap_code store: {e}")))?;
        self.emit_trap_sentinel_return(op_hint)
    }

    /// Emit `ret i32 -1` — the negative bytes-written sentinel the host
    /// reads as "a trap fired; decode `ArenaState::trap_code()`". Used
    /// by both [`Self::emit_state_trap`] (which writes the code first)
    /// and the `Op::CallNative` trap branch (where the dispatch helper
    /// already wrote the code). Must be called on a fresh block; the
    /// caller positions at a continuation block afterwards.
    pub(crate) fn emit_trap_sentinel_return(&mut self, op_hint: &str) -> Result<(), LlvmError> {
        let i32_t = self.ctx.i32_type();
        // -1 as i32 — `const_int` takes the two's-complement bit pattern.
        let neg_one = i32_t.const_int(u64::from(u32::MAX), false);
        self.builder
            .build_return(Some(&neg_one))
            .map_err(|e| LlvmError::Codegen(format!("{op_hint} trap sentinel ret: {e}")))?;
        Ok(())
    }

    /// Phase E.2 multi-function dispatch: lower `Op::Call`.
    ///
    /// The IR's `fn_index` is split as `[0..stdlib_count) = bundled
    /// stdlib body` / `[stdlib_count..) = user-defined sibling`. The
    /// LLVM emitter currently only routes the sibling slice — stdlib
    /// inlining stays parked on the cranelift backend. A stdlib call
    /// surfaces `LlvmError::Codegen` so the host can fall back.
    pub(crate) fn emit_call(
        &mut self,
        ip_hint: &str,
        fn_index: u32,
        arg_count: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        let stdlib_count = relon_ir::stdlib::stdlib_function_count();
        if fn_index < stdlib_count {
            return Err(LlvmError::Codegen(format!(
                "Op::Call to stdlib fn_index={fn_index} not yet supported in LLVM AOT \
                 (cranelift inlines bundled stdlib bodies; LLVM path widens with #278)"
            )));
        }
        let helper_idx = fn_index - stdlib_count;
        let callee = match self.helper_table.as_ref().and_then(|t| t.get(&helper_idx)) {
            Some(fv) => *fv,
            None => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call helper_idx={helper_idx} (fn_index={fn_index}, stdlib_count={stdlib_count}) \
                     not in helper_table — module may be missing the function"
                )));
            }
        };

        // Sanity check arity against the declared signature.
        if callee.count_params() as usize != param_tys.len() {
            return Err(LlvmError::Codegen(format!(
                "Op::Call helper_idx={helper_idx}: callee has {} LLVM params, IR declares {}",
                callee.count_params(),
                param_tys.len()
            )));
        }
        if arg_count as usize != param_tys.len() {
            return Err(LlvmError::Codegen(format!(
                "Op::Call helper_idx={helper_idx}: arg_count={arg_count} != param_tys.len()={}",
                param_tys.len()
            )));
        }

        // Pop the arguments off the operand stack — last-pushed value
        // is the last param.
        let mut args: Vec<BasicMetadataValueEnum<'ctx>> = Vec::with_capacity(arg_count as usize);
        for _ in 0..arg_count {
            args.push(self.pop_int(ip_hint)?.into());
        }
        args.reverse();

        // Adjust each arg's LLVM type to match the callee's declared
        // param: widen / truncate i32 <-> i64 as needed. The IR's
        // stack-machine semantics keep types tagged but the wasm slot
        // widening can leave a Bool-as-i32 in front of an I64 callee
        // param. We re-coerce here to match the helper's signature.
        for (i, (slot, want_ty)) in args.iter_mut().zip(param_tys.iter()).enumerate() {
            let arg_val = match slot {
                BasicMetadataValueEnum::IntValue(v) => *v,
                other => {
                    return Err(LlvmError::Codegen(format!(
                        "Op::Call arg #{i}: expected IntValue, got {other:?}"
                    )));
                }
            };
            let want_width = match *want_ty {
                IrType::I64 => 64,
                IrType::I32
                | IrType::Bool
                | IrType::Unit
                | IrType::String
                | IrType::ListInt
                | IrType::ListFloat
                | IrType::ListBool
                | IrType::ListString
                | IrType::ListSchema
                | IrType::ListList
                | IrType::Closure
                | IrType::Dict => 32,
                IrType::F64 => {
                    return Err(LlvmError::Codegen(format!(
                        "Op::Call arg #{i}: F64 param not yet supported in Phase E.2"
                    )));
                }
            };
            let have_width = arg_val.get_type().get_bit_width();
            if have_width != want_width {
                let target_ty = if want_width == 64 {
                    self.ctx.i64_type()
                } else {
                    self.ctx.i32_type()
                };
                let coerced = if have_width < want_width {
                    self.builder
                        .build_int_z_extend(arg_val, target_ty, "call_arg_zext")
                        .map_err(|e| LlvmError::Codegen(format!("call arg zext: {e}")))?
                } else {
                    self.builder
                        .build_int_truncate(arg_val, target_ty, "call_arg_trunc")
                        .map_err(|e| LlvmError::Codegen(format!("call arg trunc: {e}")))?
                };
                *slot = coerced.into();
            }
        }

        let name = self.next_name("call_ret");
        let call_site = self
            .builder
            .build_call(callee, &args, &name)
            .map_err(|e| LlvmError::Codegen(format!("Op::Call build_call: {e}")))?;
        let ret_val = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(v) => v,
            inkwell::values::ValueKind::Instruction(_) => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call helper_idx={helper_idx}: callee returned void; Phase E.2 envelope expects a typed return"
                )));
            }
        };
        let ret_int = match ret_val {
            BasicValueEnum::IntValue(v) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call helper_idx={helper_idx}: callee returned {other:?}, expected IntValue"
                )));
            }
        };
        self.push(ret_int, ret_ty);
        Ok(())
    }

    /// Phase E.2: emit a call to the `llvm.trap` intrinsic. The
    /// intrinsic must be pre-declared on the module via
    /// [`declare_llvm_trap`] before the first guard fires; the
    /// declaration is cached on the `Emit` so repeated div / mod
    /// guards share one `FunctionValue`. The `op_hint` is used only
    /// for diagnostic naming on the build_call site.
    pub(crate) fn emit_llvm_trap_call(&mut self, op_hint: &str) -> Result<(), LlvmError> {
        let trap_fn = self.llvm_trap_fn.ok_or_else(|| {
            LlvmError::Codegen(format!(
                "{op_hint}: llvm.trap intrinsic missing — emit_module_funcs forgot to declare it"
            ))
        })?;
        let name = self.next_name("trap_call");
        self.builder
            .build_call(trap_fn, &[], &name)
            .map_err(|e| LlvmError::Codegen(format!("{op_hint} llvm.trap build_call: {e}")))?;
        Ok(())
    }

    /// Lower `Op::Call { fn_index, ... }` by inlining the bundled
    /// stdlib body. Mirrors cranelift's `emit_call_stdlib` — pop the
    /// args, set up an inline frame with an exit-block target, lower
    /// the callee body recursively against the inline frame, then
    /// continue at the exit block with the loaded result on the stack.
    pub(crate) fn emit_call_stdlib(
        &mut self,
        ip_hint: &str,
        fn_index: u32,
        arg_count: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        let stdlib = relon_ir::stdlib::builtin_stdlib();
        let callee = stdlib.get(fn_index as usize).ok_or_else(|| {
            LlvmError::Codegen(format!(
                "Op::Call fn_index {fn_index} outside bundled stdlib (max {})",
                stdlib.len()
            ))
        })?;
        if callee.params.len() != arg_count as usize {
            return Err(LlvmError::Codegen(format!(
                "Op::Call to `{}` declares {arg_count} args but callee has {}",
                callee.name,
                callee.params.len()
            )));
        }
        for (i, (declared, expected)) in callee.params.iter().zip(param_tys.iter()).enumerate() {
            if declared != expected {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call to `{}` arg #{i}: callee expects {declared:?}, IR tags {expected:?}",
                    callee.name
                )));
            }
        }
        // Pop args in reverse so `params[i]` is the i-th declared arg.
        let mut args: Vec<TypedValue<'ctx>> = Vec::with_capacity(arg_count as usize);
        for _ in 0..arg_count {
            args.push(self.pop(ip_hint)?);
        }
        args.reverse();

        // Pick a let_offset window past any caller let slot — both
        // the ones already declared (lazy `let_slots` max) AND the
        // static `let_floor` watermark covering lets the caller body
        // binds only *after* this call. Inspecting `let_slots` alone
        // is unsound: e.g. the runtime list-spread materialiser binds
        // its source-handle / cursor lets after lowering a
        // `range(n).map(...)` source, and the callee window would
        // land on those late slots ("let-slot N aliased" error).
        let let_offset = self
            .let_slots
            .keys()
            .copied()
            .max()
            .map(|m| m + 1)
            .unwrap_or(0)
            .max(self.let_floor);

        // Alloca for the callee's return value. The callee's
        // `Op::Return` stores into this slot then jumps to `exit_bb`;
        // the caller-side load below pushes the value back on the
        // virtual stack.
        let ret_llvm_ty: inkwell::types::BasicTypeEnum<'ctx> = match ret_ty {
            // AOT-1: F64 rides as i64 bits on the operand stack, so its
            // inline-frame ret slot is i64-wide (same as I64). The
            // callee's `Op::Return` stores the raw bit pattern via
            // `coerce_to_let_ty`; the caller-side load reads i64 bits and
            // re-tags the pushed value `F64`. Used by the Wave R3b
            // `list_float_fold` (`-> Float`) bundled body.
            IrType::I64 | IrType::F64 => self.ctx.i64_type().into(),
            IrType::I32
            | IrType::Bool
            | IrType::Unit
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            // Wave R3c: a stdlib body returning a pointer-array list
            // (`list_string_map` / `list_*_map_to_string` -> `ListString`,
            // variant-list map -> `ListList`) hands back an i32
            // arena-relative handle, same as `ListInt`.
            | IrType::ListString
            | IrType::ListList => self.ctx.i32_type().into(),
            other => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call ret_ty {other:?} unsupported in inline frame"
                )));
            }
        };
        // Allocate the ret slot in the function's entry block so it
        // stays out of any loop body; mem2reg promotes it on -O2/-O3.
        let entry_bb = self.func.get_first_basic_block().ok_or_else(|| {
            LlvmError::Codegen("emit_call_stdlib: function has no entry block".into())
        })?;
        let cur = self.builder.get_insert_block();
        if let Some(first_instr) = entry_bb.get_first_instruction() {
            self.builder.position_before(&first_instr);
        } else {
            self.builder.position_at_end(entry_bb);
        }
        let ret_slot = self
            .builder
            .build_alloca(ret_llvm_ty, "call_ret_slot")
            .map_err(|e| LlvmError::Codegen(format!("call ret_slot alloca: {e}")))?;
        if let Some(bb) = cur {
            self.builder.position_at_end(bb);
        }

        let exit_bb = self.ctx.append_basic_block(self.func, "call_exit");
        let frame = InlineFrame {
            params: args,
            let_offset,
            ret_slot,
            ret_ty,
            exit_bb,
        };
        self.inline_frames.push(frame);
        let body = callee.body_owned();
        // Raise the floor past the callee window for the duration of
        // the inline emission so a nested stdlib inline (or any let
        // the callee binds late) allocates its own window above this
        // one; restore the caller's floor on frame pop.
        let saved_floor = self.let_floor;
        self.let_floor = let_offset + relon_ir::ir::body_let_watermark(&body);
        let result = self.lower_body(&body);
        // Always pop the frame before returning the error so the emit
        // state stays consistent on failure.
        self.inline_frames.pop();
        self.let_floor = saved_floor;
        result?;

        // After the inline body finishes the current block has either
        // hit `Op::Return` (which terminated via `br exit_bb`) or fell
        // through. If it fell through, branch to exit_bb so the
        // load + push below has a single in-edge.
        let cur_terminated = self
            .builder
            .get_insert_block()
            .and_then(|bb| bb.get_terminator())
            .is_some();
        if !cur_terminated {
            self.builder
                .build_unconditional_branch(exit_bb)
                .map_err(|e| LlvmError::Codegen(format!("inline call fallthrough: {e}")))?;
        }
        // Position at the exit block and load the result.
        self.builder.position_at_end(exit_bb);
        let name = self.next_name("call_ret_load");
        let v = self
            .builder
            .build_load(ret_llvm_ty, ret_slot, &name)
            .map_err(|e| LlvmError::Codegen(format!("inline call ret load: {e}")))?
            .into_int_value();
        self.push(v, ret_ty);
        Ok(())
    }
}
