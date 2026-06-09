//! `Op`-family: first-class closures.
//!
//! MakeClosure / CallClosure (incl. the devirtualised direct-call path).

use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum, FunctionValue, IntValue, PointerValue,
};
use inkwell::AddressSpace;

use relon_ir::ir::IrType;

use crate::error::LlvmError;

use super::*;

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Phase F.W7: lower `Op::MakeClosure { fn_table_idx, captures,
    /// captures_size }`. Closure handle layout (8 bytes total):
    ///   `[fn_table_idx: u32 LE][captures_ptr: u32 LE]`
    ///
    /// Steps:
    ///   1. Alloc 8 bytes for the handle (arena-relative ptr ->
    ///      `handle_ptr`).
    ///   2. If `captures_size > 0`: alloc `captures_size` bytes for
    ///      the captures struct (-> `captures_ptr`). For each
    ///      capture, write the matching let-local value into the
    ///      struct at the declared offset. **Self-recursion** is
    ///      detected by a missing let_slot at MakeClosure time — the
    ///      lowering pass places the self-binding's `LetSet` *after*
    ///      MakeClosure, so the about-to-be-stored handle is the only
    ///      value the captured slot could hold. We seed it with
    ///      `handle_ptr` (the value the upcoming `LetSet` will stash)
    ///      so the recursive call site reads back a live handle
    ///      instead of zero (which would crash `CallClosure`'s
    ///      indirect dispatch).
    ///   3. Store `fn_table_idx` at `handle_ptr + 0`.
    ///   4. Store `captures_ptr` (or 0) at `handle_ptr + 4`.
    ///   5. Push `handle_ptr` as the i32 Closure handle.
    pub(crate) fn emit_make_closure(
        &mut self,
        ip_hint: &str,
        fn_table_idx: u32,
        captures: &[relon_ir::ir::ClosureCapture],
        captures_size: u32,
    ) -> Result<(), LlvmError> {
        let _ = ip_hint;
        // Validate fn_table_idx against the closure table the emit
        // pass seeded. The IR lowering numbers slots in source order;
        // a slot >= table length means the lowering pass and emit
        // pass disagree on closure count.
        if (fn_table_idx as usize) >= self.closure_fn_table.len() {
            return Err(LlvmError::Codegen(format!(
                "MakeClosure fn_table_idx={fn_table_idx} out of range (closure_fn_table.len()={})",
                self.closure_fn_table.len()
            )));
        }
        // Phase D.2: fast-path entry has no arena/state to bump the
        // 8-byte handle / captures-struct into. Virtualise the closure
        // — push a placeholder i32 tagged with `FastPathClosure` so the
        // downstream `LetSet/LetGet` chain keeps the `fn_table_idx`
        // available, and the matching `CallClosure` rewrites into a
        // direct call against `closure_fn_table[fn_table_idx]`. Sound
        // for the W7 anon-Dict shape because the lambda's post-O3
        // body drops state / captures_ptr (the inner self-recursion
        // fast path already side-stepped them); passing zero through
        // the direct-call ABI lets LLVM strip the dead args entirely.
        //
        // Captures are skipped — the only legal capture in this
        // shape is the self-handle (an `Op::LetSet { ty: Closure }`
        // immediately follows the `MakeClosure`) which the fast-path
        // closure tracker re-derives from `fn_table_idx`. Any other
        // capture surfaces as an emitter error so future widenings
        // (W3 fold, W11 multi-closure) explicitly opt in.
        if self.fast_path.is_some() {
            for cap in captures {
                if !matches!(cap.ty, IrType::Closure) {
                    return Err(LlvmError::Codegen(format!(
                        "fast-path MakeClosure: non-Closure capture (let_idx={}, ty={:?}) \
                         outside the W7 envelope",
                        cap.let_idx, cap.ty
                    )));
                }
            }
            let _ = captures_size;
            let placeholder = self.ctx.i32_type().const_zero();
            self.push_with_prov(
                placeholder,
                IrType::Closure,
                Provenance::FastPathClosure { fn_table_idx },
            );
            return Ok(());
        }
        // Step 1: alloc 8 bytes for the handle.
        let i32_t = self.ctx.i32_type();
        let eight = i32_t.const_int(8, false);
        self.emit_alloc_scratch_common(eight)?;
        let handle_ptr = self.pop_int("MakeClosure handle alloc")?;

        // Step 2: alloc + populate the captures struct.
        let captures_ptr = if captures_size > 0 {
            let cs = i32_t.const_int(u64::from(captures_size), false);
            self.emit_alloc_scratch_common(cs)?;
            self.pop_int("MakeClosure captures alloc")?
        } else {
            i32_t.const_zero()
        };

        // Step 3: store fn_table_idx at handle_ptr + 0.
        let fn_idx_v = i32_t.const_int(u64::from(fn_table_idx), false);
        let handle_addr = self.arena_addr_i32(handle_ptr)?;
        self.builder
            .build_store(handle_addr, fn_idx_v)
            .map_err(|e| LlvmError::Codegen(format!("MakeClosure fn_idx store: {e}")))?;

        // Step 4: store captures_ptr at handle_ptr + 4.
        let four = self.ctx.i32_type().const_int(4, false);
        let handle_plus_4 = self
            .builder
            .build_int_add(handle_ptr, four, "handle_plus_4")
            .map_err(|e| LlvmError::Codegen(format!("MakeClosure handle+4: {e}")))?;
        let captures_slot_addr = self.arena_addr_i32(handle_plus_4)?;
        self.builder
            .build_store(captures_slot_addr, captures_ptr)
            .map_err(|e| LlvmError::Codegen(format!("MakeClosure captures store: {e}")))?;

        // Step 5: write each capture into the captures struct.
        if captures_size > 0 {
            for cap in captures {
                // Determine the value to stash. If a let-slot exists
                // for `cap.let_idx`, read it. Otherwise treat it as a
                // self-recursive capture and use the handle_ptr we
                // just allocated (matches what the immediately-
                // following `LetSet { idx: cap.let_idx, ty: Closure }`
                // will store).
                let mapped_idx = self.remap_let_idx(cap.let_idx);
                let cap_offset = self.ctx.i32_type().const_int(u64::from(cap.offset), false);
                let cap_addr_i32 = self
                    .builder
                    .build_int_add(captures_ptr, cap_offset, "cap_off")
                    .map_err(|e| LlvmError::Codegen(format!("MakeClosure cap off: {e}")))?;
                let cap_addr = self.arena_addr_i32(cap_addr_i32)?;
                let value: BasicValueEnum<'ctx> = if let Some((slot, slot_ty)) =
                    self.let_slots.get(&mapped_idx).copied()
                {
                    let load_name = self.next_name("cap_load");
                    let raw = self
                        .builder
                        .build_load(self.ir_ty_to_llvm_int(slot_ty)?, slot, &load_name)
                        .map_err(|e| LlvmError::Codegen(format!("MakeClosure cap let load: {e}")))?
                        .into_int_value();
                    // Coerce to the capture's declared IR type
                    // width — the let-slot may have stored a
                    // wider value (e.g. i32 Closure stashed as
                    // i32 already matches; widen-and-truncate is
                    // a no-op).
                    match cap.ty {
                        IrType::I64 => {
                            if raw.get_type().get_bit_width() < 64 {
                                self.builder
                                    .build_int_z_extend(raw, self.ctx.i64_type(), "cap_zext")
                                    .map_err(|e| {
                                        LlvmError::Codegen(format!("MakeClosure cap zext: {e}"))
                                    })?
                                    .into()
                            } else {
                                raw.into()
                            }
                        }
                        IrType::F64 => {
                            // Stack carries f64 bit-cast to i64;
                            // store the bit pattern verbatim
                            // (the load on the read side bit-
                            // casts back).
                            raw.into()
                        }
                        _ => {
                            // Narrow to i32 if the let-slot
                            // carries i64; cap.ty is one of the
                            // 4-byte-wide variants.
                            if raw.get_type().get_bit_width() > 32 {
                                self.builder
                                    .build_int_truncate(raw, self.ctx.i32_type(), "cap_trunc")
                                    .map_err(|e| {
                                        LlvmError::Codegen(format!("MakeClosure cap trunc: {e}"))
                                    })?
                                    .into()
                            } else {
                                raw.into()
                            }
                        }
                    }
                } else {
                    // Self-recursive capture: the let-slot for
                    // `mapped_idx` isn't initialised yet because
                    // the lowering pass emits MakeClosure before
                    // the matching `LetSet`. The captured value
                    // is the closure handle itself — the same
                    // value the upcoming `LetSet` will store —
                    // so we stamp `handle_ptr` here. Only legal
                    // when the capture's IR type is `Closure`
                    // (anything else can't refer to a
                    // not-yet-bound let-local in source).
                    if cap.ty != IrType::Closure {
                        return Err(LlvmError::Codegen(format!(
                                "MakeClosure capture `let_idx={mapped_idx}` not yet bound but ty={:?} (expected Closure for self-recursion)",
                                cap.ty
                            )));
                    }
                    handle_ptr.into()
                };
                self.builder
                    .build_store(cap_addr, value)
                    .map_err(|e| LlvmError::Codegen(format!("MakeClosure cap store: {e}")))?;
            }
        }

        // Step 6: push the handle_ptr, tagged with the compile-time
        // `fn_table_idx` we just stored at `handle_ptr + 0`. The handle
        // is a real, fully-populated arena record (captures_ptr live at
        // `+4`), so a later `CallClosure` that consumes *this exact
        // value* can skip the runtime `switch i32 %cc_fn_idx` — its
        // selector is provably this constant — and emit a direct call
        // while still loading the real captures_ptr. Devirtualisation
        // fires only when the value reaches the call site unmodified
        // (tracked through `LetSet`/`LetGet` + inline-frame param binds);
        // any reassignment drops the provenance and the slow-path switch
        // returns. See [`Provenance::KnownClosure`].
        self.push_with_prov(
            handle_ptr,
            IrType::Closure,
            Provenance::KnownClosure { fn_table_idx },
        );
        Ok(())
    }

    /// Phase F.W7: lower `Op::CallClosure { param_tys, ret_ty }`.
    /// Stack discipline: `[Closure, arg0, arg1, ...] -> [ret_ty]`.
    ///
    /// Pops user args (in reverse), pops the closure handle,
    /// materialises `fn_table_idx` + `captures_ptr` from the handle,
    /// looks up the matching `FunctionValue` through
    /// `closure_fn_table[fn_table_idx]` via a switch, and invokes
    /// the resolved function indirectly with
    /// `(state, captures_ptr, args...)`.
    pub(crate) fn emit_call_closure(
        &mut self,
        ip_hint: &str,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        if self.closure_fn_table.is_empty() {
            return Err(LlvmError::Codegen(
                "Op::CallClosure but closure_fn_table is empty — module declared no lambdas".into(),
            ));
        }
        // Phase D.2: fast-path entry routes `CallClosure` through a
        // direct call to `closure_fn_table[fn_table_idx]` when the
        // popped handle was produced by an in-body `MakeClosure`
        // (virtualised closure carrying `FastPathClosure` provenance).
        // The lambda's `(state, captures_ptr, args...)` signature is
        // satisfied with a null pointer + i32 zero — sound for the W7
        // anon-Dict shape because the lambda's post-O3 body has
        // already dropped both args (the inner self-recursion fast
        // path side-stepped them). LLVM strips the dead loads when
        // it inlines the call.
        if self.fast_path.is_some() {
            // Pop user args in reverse.
            let mut user_args: Vec<IntValue<'ctx>> = Vec::with_capacity(param_tys.len());
            for _ in 0..param_tys.len() {
                user_args.push(self.pop_int(ip_hint)?);
            }
            user_args.reverse();
            let handle_tv = self.pop(ip_hint)?;
            let fn_table_idx = match handle_tv.prov {
                Provenance::FastPathClosure { fn_table_idx } => fn_table_idx,
                other => {
                    return Err(LlvmError::Codegen(format!(
                        "fast-path CallClosure: handle has provenance {other:?} (expected \
                         FastPathClosure — the call site reads a closure not constructed \
                         in this entry's body, outside the W7 envelope)"
                    )));
                }
            };
            let slot = fn_table_idx as usize;
            if slot >= self.closure_fn_table.len() {
                return Err(LlvmError::Codegen(format!(
                    "fast-path CallClosure: fn_table_idx={fn_table_idx} out of range \
                     (closure_fn_table.len()={})",
                    self.closure_fn_table.len()
                )));
            }
            let callee = self.closure_fn_table[slot];
            let null_state = self.ctx.ptr_type(AddressSpace::default()).const_null();
            let null_captures = self.ctx.i32_type().const_zero();
            return self.emit_call_closure_direct(
                callee,
                null_state,
                null_captures,
                user_args,
                param_tys,
                ret_ty,
            );
        }
        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen("CallClosure outside buffer-protocol entry (no state)".into())
        })?;
        // Pop user args in reverse.
        let mut user_args: Vec<IntValue<'ctx>> = Vec::with_capacity(param_tys.len());
        for _ in 0..param_tys.len() {
            user_args.push(self.pop_int(ip_hint)?);
        }
        user_args.reverse();

        // Pop closure handle (i32 arena-relative offset). Capture
        // provenance up-front so the self-recursion fast path can
        // route around the handle deref / switch entirely.
        let handle_tv = self.pop(ip_hint)?;
        let handle_ptr = handle_tv.val;
        let handle_prov = handle_tv.prov;

        // Phase F.W7 self-recursion fast path: when the handle came
        // from the lambda's own self-capture chain we know
        //  * `handle.fn_table_idx == self_fn_table_idx` (stamped by
        //    the outer `MakeClosure`); and
        //  * `handle.captures_ptr == captures_ptr_arg` (the lambda's
        //    LLVM param 1 — same value `MakeClosure` stashed into the
        //    handle's `+4` slot because the captured pointer is the
        //    captures struct the host built for this very lambda).
        // Skip the handle deref + switch dispatch and emit a direct
        // call to the matching `FunctionValue`. Cuts ~3 loads + a
        // conditional branch off every recursion, closing the gap
        // versus the equivalent Rust direct-recursive call on W7
        // (recursive `fib(k - 1) + fib(k - 2)`).
        if let (
            Provenance::OwnCaptureHandle {
                self_fn_table_idx, ..
            },
            Some(captures_ptr_arg),
        ) = (handle_prov, self.captures_ptr_param)
        {
            let slot = self_fn_table_idx as usize;
            if slot < self.closure_fn_table.len() {
                return self.emit_call_closure_direct(
                    self.closure_fn_table[slot],
                    state_ptr,
                    captures_ptr_arg,
                    user_args,
                    param_tys,
                    ret_ty,
                );
            }
        }

        // Devirtualisation (W18): the handle came from a literal
        // `MakeClosure` whose `fn_table_idx` is a compile-time constant
        // and the value reached this call site unmodified (tracked
        // through the `KnownClosure` provenance across `LetSet`/`LetGet`
        // and inline-frame argument binds). The runtime
        // `switch i32 %cc_fn_idx` would therefore *always* select
        // `closure_fn_table[fn_table_idx]`, so emit a direct call to it
        // — LLVM then inlines the callee, folding the per-element
        // dispatch out of the hot loop. We still load the *real*
        // captures_ptr from `handle + 4` (this closure may capture
        // free variables — e.g. the W18 predicate captures `is_prime`),
        // so capture semantics are byte-identical to the switch path;
        // only the dead `fn_idx` load + switch are removed.
        //
        // Correctness guard: devirtualise ONLY when the resolved
        // callee's signature (arity + return width) matches this call
        // site, exactly as the switch's per-case `signature_compatible`
        // check requires. A module may host several lambdas; if the
        // statically-resolved target somehow disagrees with the call
        // shape we keep the switch (its matching case fires at runtime),
        // never emitting an ill-typed direct call.
        if let Provenance::KnownClosure { fn_table_idx } = handle_prov {
            let slot = fn_table_idx as usize;
            if slot < self.closure_fn_table.len() {
                let callee = self.closure_fn_table[slot];
                let want_arity = 2 + user_args.len();
                let want_ret_llvm = match ret_ty {
                    IrType::Unit => None,
                    other => Some(self.ir_ty_to_llvm_int(other)?.get_bit_width()),
                };
                let have_ret_llvm = callee.get_type().get_return_type().and_then(|t| match t {
                    inkwell::types::BasicTypeEnum::IntType(it) => Some(it.get_bit_width()),
                    _ => None,
                });
                let signature_compatible =
                    callee.count_params() as usize == want_arity && have_ret_llvm == want_ret_llvm;
                if signature_compatible {
                    // Load the real captures_ptr from `handle + 4`; the
                    // closure's captured environment must be passed
                    // verbatim (unchanged from the slow path).
                    let i32_t = self.ctx.i32_type();
                    let four = i32_t.const_int(4, false);
                    let handle_plus_4 = self
                        .builder
                        .build_int_add(handle_ptr, four, "ccd_handle_plus_4")
                        .map_err(|e| {
                            LlvmError::Codegen(format!("CallClosure(known) handle+4: {e}"))
                        })?;
                    let cap_ptr_addr = self.arena_addr_i32(handle_plus_4)?;
                    let captures_ptr_name = self.next_name("ccd_captures_ptr");
                    let captures_ptr = self
                        .builder
                        .build_load(i32_t, cap_ptr_addr, &captures_ptr_name)
                        .map_err(|e| {
                            LlvmError::Codegen(format!("CallClosure(known) captures load: {e}"))
                        })?
                        .into_int_value();
                    return self.emit_call_closure_direct(
                        callee,
                        state_ptr,
                        captures_ptr,
                        user_args,
                        param_tys,
                        ret_ty,
                    );
                }
            }
        }

        // Load fn_table_idx (handle+0) and captures_ptr (handle+4).
        let handle_addr = self.arena_addr_i32(handle_ptr)?;
        let i32_t = self.ctx.i32_type();
        let fn_idx_name = self.next_name("cc_fn_idx");
        let fn_idx = self
            .builder
            .build_load(i32_t, handle_addr, &fn_idx_name)
            .map_err(|e| LlvmError::Codegen(format!("CallClosure fn_idx load: {e}")))?
            .into_int_value();
        let four = i32_t.const_int(4, false);
        let handle_plus_4 = self
            .builder
            .build_int_add(handle_ptr, four, "cc_handle_plus_4")
            .map_err(|e| LlvmError::Codegen(format!("CallClosure handle+4: {e}")))?;
        let cap_ptr_addr = self.arena_addr_i32(handle_plus_4)?;
        let captures_ptr_name = self.next_name("cc_captures_ptr");
        let captures_ptr = self
            .builder
            .build_load(i32_t, cap_ptr_addr, &captures_ptr_name)
            .map_err(|e| LlvmError::Codegen(format!("CallClosure captures load: {e}")))?
            .into_int_value();

        // NOTE: per-arg width coercion is deferred into each switch
        // case below. A module may host several same-arity lambdas with
        // *different* param widths (AOT-4 W16 binds a `List<Int>`-taking
        // recursive `sum_qs` (i32 handle) alongside a 1-arg `(x: Int)`
        // filter predicate (i64) — both arity 1). Coercing once here to
        // this call site's `param_tys` and then reusing the coerced
        // values across every switch case would emit a wrong-width arg
        // into the sibling lambda's case (which is statically present
        // but dynamically dead) and the LLVM verifier rejects the whole
        // module. Coercing per-case against the *callee's* declared LLVM
        // param type keeps each case well-typed; the runtime
        // `fn_table_idx` only ever selects the case whose lambda matches
        // the handle, so the sibling cases are never executed.

        // Dispatch through a switch over fn_table_idx → one direct
        // call per lambda. This avoids needing a runtime function-
        // pointer table at module scope (LLVM 18 + opaque pointers
        // makes that doable but adds the burden of seeding the
        // global at JIT-resolve time). The switch IR is tiny and
        // LLVM's selectoptimize pass collapses it to a jump table /
        // computed call when profitable.
        let cur_bb = self
            .builder
            .get_insert_block()
            .ok_or_else(|| LlvmError::Codegen("CallClosure: builder has no insert block".into()))?;
        let post_bb = self.ctx.append_basic_block(self.func, "cc_post");
        // Pre-allocate the ret slot in the entry block so mem2reg can
        // promote it across the switch joins.
        let ret_slot = if !matches!(ret_ty, IrType::Unit) {
            let ret_llvm_ty = self.ir_ty_to_llvm_int(ret_ty)?;
            let cur = self.builder.get_insert_block();
            // Position at entry block start to place the alloca
            // there; restore afterwards.
            let entry_first = self.func.get_first_basic_block().ok_or_else(|| {
                LlvmError::Codegen("CallClosure: function missing entry block".into())
            })?;
            // Insert before the first non-alloca instr — close enough
            // for mem2reg.
            if let Some(first) = entry_first.get_first_instruction() {
                self.builder.position_before(&first);
            } else {
                self.builder.position_at_end(entry_first);
            }
            let slot = self
                .builder
                .build_alloca(ret_llvm_ty, "cc_ret_slot")
                .map_err(|e| LlvmError::Codegen(format!("CallClosure ret_slot alloca: {e}")))?;
            // Restore builder position.
            if let Some(bb) = cur {
                self.builder.position_at_end(bb);
            }
            Some(slot)
        } else {
            None
        };

        // Build cases: one BB per lambda.
        let mut case_bbs: Vec<inkwell::basic_block::BasicBlock<'ctx>> =
            Vec::with_capacity(self.closure_fn_table.len());
        for slot in 0..self.closure_fn_table.len() {
            let bb = self
                .ctx
                .append_basic_block(self.func, &format!("cc_case_{slot}"));
            case_bbs.push(bb);
        }
        // Default trap block — execution reaches it only if the
        // handle's fn_table_idx is out of range, which would mean
        // memory corruption.
        let default_bb = self.ctx.append_basic_block(self.func, "cc_default_trap");

        // Position at the switch's current block and emit it.
        //
        // The switch's jump-table lowering is JIT-safe only because the
        // MCJIT memory manager allocates every code / data section
        // (including the `.rodata` jump table the `switch` lowers to) in
        // the low 2 GiB (`MAP_32BIT`) — the Small code model addresses
        // the table through a 32-bit *absolute* reference. See
        // `mcjit_mm::ContiguousCodeMemoryManager` for the EMIT-INLINE
        // fix that closed the SIGSEGV on >= 4-closure dispatch tables.
        self.builder.position_at_end(cur_bb);
        let cases: Vec<(IntValue<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> = case_bbs
            .iter()
            .enumerate()
            .map(|(i, bb)| (i32_t.const_int(i as u64, false), *bb))
            .collect();
        self.builder
            .build_switch(fn_idx, default_bb, &cases)
            .map_err(|e| LlvmError::Codegen(format!("CallClosure switch: {e}")))?;

        // Per-case body: direct call to the matching lambda fn.
        for (slot, case_bb) in case_bbs.iter().enumerate() {
            self.builder.position_at_end(*case_bb);
            let callee = self.closure_fn_table[slot];
            // AOT-4: a module may host lambdas of *different* arities
            // (W18 binds a 1-arg filter predicate alongside a 2-arg
            // recursive `is_prime`). The `fn_idx` switch enumerates
            // every lambda slot, but only the slot whose arity matches
            // this call site's `param_tys` can be selected at runtime
            // (the handle's `fn_table_idx` always points at the lambda
            // the predicate / recursion actually targets). Statically
            // emitting a call into a wrong-arity callee would make the
            // LLVM verifier reject the module ("Incorrect number of
            // arguments"). Guard each case: when the callee's arity
            // (state + captures_ptr + user params) disagrees with this
            // call's shape, the case is dead — emit `llvm.trap` +
            // `unreachable` instead of the ill-typed call.
            //
            // EMIT-INLINE fix: arity alone is *not* enough to decide a
            // case is live. A module can host several same-arity lambdas
            // whose *signatures* still differ — most importantly in their
            // return type. The W16 3-partition kernel binds a recursive
            // `List<Int> -> Int` where-helper (`closure_fn_table[0]`,
            // returns i64) *alongside* the `(x: Int) -> Bool` filter
            // predicates (return i32); both have arity 3 (`state`,
            // `captures_ptr`, one user param). A predicate call site's
            // `ret_slot` is i32, so emitting a real `call i64 @helper`
            // in that case would `store i64 <result>, ptr <i32 slot>` —
            // an 8-byte write into a 4-byte entry-block alloca that
            // clobbers the adjacent slot, plus a recursive call into the
            // helper with the predicate's i64 element coerced into the
            // helper's `List<Int>` handle param (a wild arena offset).
            // The case is dynamically dead (the handle's `fn_table_idx`
            // only ever selects a lambda whose signature matches the call
            // site) but its *static* presence is memory-unsafe and gives
            // the optimiser license to miscompile. Trap the case as dead
            // whenever the callee's return-type width disagrees with this
            // call site's expected return width, not just on arity.
            let want_arity = 2 + user_args.len();
            let want_ret_llvm = match ret_ty {
                IrType::Unit => None,
                other => Some(self.ir_ty_to_llvm_int(other)?.get_bit_width()),
            };
            let have_ret_llvm = callee.get_type().get_return_type().and_then(|t| match t {
                inkwell::types::BasicTypeEnum::IntType(it) => Some(it.get_bit_width()),
                _ => None,
            });
            let signature_compatible =
                callee.count_params() as usize == want_arity && have_ret_llvm == want_ret_llvm;
            if !signature_compatible {
                let trap = self.llvm_trap_fn.ok_or_else(|| {
                    LlvmError::Codegen(
                        "CallClosure incompatible-signature case: llvm.trap not declared".into(),
                    )
                })?;
                self.builder
                    .build_call(trap, &[], "cc_sig_trap")
                    .map_err(|e| LlvmError::Codegen(format!("CallClosure sig trap call: {e}")))?;
                self.builder
                    .build_unreachable()
                    .map_err(|e| LlvmError::Codegen(format!("CallClosure sig unreachable: {e}")))?;
                continue;
            }
            // Build args: (state, captures_ptr, user_args...). Coerce
            // each user arg to *this callee's* declared LLVM param width
            // (param 0 = state ptr, param 1 = captures_ptr, params 2.. =
            // the user args) so a same-arity sibling lambda with a
            // different param width still type-checks in its (dead) case.
            let callee_param_tys = callee.get_type().get_param_types();
            let mut call_args: Vec<BasicMetadataValueEnum<'ctx>> =
                Vec::with_capacity(2 + user_args.len());
            call_args.push(state_ptr.into());
            call_args.push(captures_ptr.into());
            for (i, v) in user_args.iter().enumerate() {
                let want_width = match callee_param_tys.get(2 + i) {
                    Some(inkwell::types::BasicMetadataTypeEnum::IntType(t)) => t.get_bit_width(),
                    _ => v.get_type().get_bit_width(),
                };
                let have_width = v.get_type().get_bit_width();
                let coerced = if have_width == want_width {
                    *v
                } else {
                    let target_ty = if want_width == 64 {
                        self.ctx.i64_type()
                    } else {
                        self.ctx.i32_type()
                    };
                    if have_width < want_width {
                        self.builder
                            .build_int_z_extend(*v, target_ty, "cc_arg_zext")
                            .map_err(|e| {
                                LlvmError::Codegen(format!("CallClosure arg #{i} zext: {e}"))
                            })?
                    } else {
                        self.builder
                            .build_int_truncate(*v, target_ty, "cc_arg_trunc")
                            .map_err(|e| {
                                LlvmError::Codegen(format!("CallClosure arg #{i} trunc: {e}"))
                            })?
                    }
                };
                call_args.push(coerced.into());
            }
            let name = self.next_name("cc_call");
            let call_site = self
                .builder
                .build_call(callee, &call_args, &name)
                .map_err(|e| LlvmError::Codegen(format!("CallClosure call: {e}")))?;
            if let Some(slot) = ret_slot {
                let v = match call_site.try_as_basic_value() {
                    inkwell::values::ValueKind::Basic(v) => v,
                    inkwell::values::ValueKind::Instruction(_) => {
                        return Err(LlvmError::Codegen(
                            "CallClosure: callee returned void but ret_ty != Unit".into(),
                        ));
                    }
                };
                self.builder
                    .build_store(slot, v)
                    .map_err(|e| LlvmError::Codegen(format!("CallClosure ret store: {e}")))?;
            }
            self.builder
                .build_unconditional_branch(post_bb)
                .map_err(|e| LlvmError::Codegen(format!("CallClosure case br: {e}")))?;
        }

        // Default block: invoke llvm.trap and fall through to an
        // `unreachable` so the verifier accepts the terminator.
        self.builder.position_at_end(default_bb);
        let trap = self.llvm_trap_fn.ok_or_else(|| {
            LlvmError::Codegen("CallClosure default trap: llvm.trap not declared".into())
        })?;
        self.builder
            .build_call(trap, &[], "cc_trap")
            .map_err(|e| LlvmError::Codegen(format!("CallClosure trap call: {e}")))?;
        self.builder
            .build_unreachable()
            .map_err(|e| LlvmError::Codegen(format!("CallClosure unreachable: {e}")))?;

        // Continue with the post block; pop the result slot into the
        // operand stack.
        self.builder.position_at_end(post_bb);
        if let Some(slot) = ret_slot {
            let llvm_ty = self.ir_ty_to_llvm_int(ret_ty)?;
            let name = self.next_name("cc_ret_load");
            let v = self
                .builder
                .build_load(llvm_ty, slot, &name)
                .map_err(|e| LlvmError::Codegen(format!("CallClosure ret load: {e}")))?
                .into_int_value();
            self.push(v, ret_ty);
        }
        Ok(())
    }

    /// Phase F.W7 self-recursion fast path companion to
    /// [`Self::emit_call_closure`]. Emits a single `call` instruction
    /// straight against `callee` with `(state, captures_ptr_arg,
    /// args...)` — no handle deref, no switch, no trap branch. The
    /// caller has already proven (via [`Provenance::OwnCaptureHandle`])
    /// that the runtime handle's fields satisfy the call ABI.
    ///
    /// Width-coerces each user arg the same way the slow-path
    /// dispatcher does, then pushes the call result back onto the
    /// operand stack (when the callee's return type isn't `Unit`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn emit_call_closure_direct(
        &mut self,
        callee: FunctionValue<'ctx>,
        state_ptr: PointerValue<'ctx>,
        captures_ptr: IntValue<'ctx>,
        mut user_args: Vec<IntValue<'ctx>>,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        // Width-coerce each user arg to the callee's declared shape
        // (mirrors the slow-path dispatcher's `cc_arg_zext` /
        // `cc_arg_trunc` pass).
        for (i, (slot, want_ty)) in user_args.iter_mut().zip(param_tys.iter()).enumerate() {
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
                IrType::F64 => 64,
            };
            let have_width = slot.get_type().get_bit_width();
            if have_width != want_width {
                let target_ty = if want_width == 64 {
                    self.ctx.i64_type()
                } else {
                    self.ctx.i32_type()
                };
                let coerced = if have_width < want_width {
                    self.builder
                        .build_int_z_extend(*slot, target_ty, "ccd_arg_zext")
                        .map_err(|e| {
                            LlvmError::Codegen(format!("CallClosure(direct) arg #{i} zext: {e}"))
                        })?
                } else {
                    self.builder
                        .build_int_truncate(*slot, target_ty, "ccd_arg_trunc")
                        .map_err(|e| {
                            LlvmError::Codegen(format!("CallClosure(direct) arg #{i} trunc: {e}"))
                        })?
                };
                *slot = coerced;
            }
        }

        // Build the LLVM call arg list `(state, captures_ptr_arg,
        // user_args...)` matching `declare_lambda_function`'s signature.
        let mut call_args: Vec<BasicMetadataValueEnum<'ctx>> =
            Vec::with_capacity(2 + user_args.len());
        call_args.push(state_ptr.into());
        call_args.push(captures_ptr.into());
        for v in &user_args {
            call_args.push((*v).into());
        }
        let name = self.next_name("ccd_call");
        let call_site = self
            .builder
            .build_call(callee, &call_args, &name)
            .map_err(|e| LlvmError::Codegen(format!("CallClosure(direct) call: {e}")))?;
        if !matches!(ret_ty, IrType::Unit) {
            let v = match call_site.try_as_basic_value() {
                inkwell::values::ValueKind::Basic(v) => v,
                inkwell::values::ValueKind::Instruction(_) => {
                    return Err(LlvmError::Codegen(
                        "CallClosure(direct): callee returned void but ret_ty != Unit".into(),
                    ));
                }
            };
            let v_int = match v {
                BasicValueEnum::IntValue(i) => i,
                BasicValueEnum::FloatValue(f) => self
                    .builder
                    .build_bit_cast(f, self.ctx.i64_type(), "ccd_ret_bitcast")
                    .map_err(|e| {
                        LlvmError::Codegen(format!("CallClosure(direct) ret bitcast: {e}"))
                    })?
                    .into_int_value(),
                other => {
                    return Err(LlvmError::Codegen(format!(
                        "CallClosure(direct): callee returned unsupported BasicValue {other:?}"
                    )));
                }
            };
            self.push(v_int, ret_ty);
        }
        Ok(())
    }
}
