//! `Op`-family: structured control flow + return.
//!
//! Block/Loop/Br/BrIf/If and Return. `Select` / `BrTable` are still in
//! the `unsupported` set in `super::lower_op` (Phase 0b fills them here).

use inkwell::values::{
    BasicValueEnum, IntValue,
};
use inkwell::IntPredicate;

use relon_ir::ir::{IrType, Op, TaggedOp};

use crate::error::LlvmError;
use crate::state::ARENA_STATE_OFFSET_TAIL_CURSOR;

use super::*;

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Phase 0b seam: multi-way / select control flow (`Select`,
    /// `BrTable`). Dispatched from `super::lower_op`. Ported from
    /// `relon-codegen-cranelift`'s `op_visitor::visit_select` and
    /// `control_flow::emit_br_table`; three-way aligned in
    /// `tests/phase0b_control.rs`.
    pub(crate) fn lower_control_rest(
        &mut self,
        ip: usize,
        ip_hint: &str,
        op: &Op,
    ) -> Result<(), LlvmError> {
        match op {
            Op::Select { ty } => self.emit_select(ip_hint, *ty),
            Op::BrTable { default, targets } => self.emit_br_table(ip_hint, *default, targets),
            other => Err(LlvmError::Codegen(format!(
                "unsupported op (Phase 0b control seam): {other:?} at ip={ip}"
            ))),
        }
    }

    /// Lower `Op::Select { ty }`. Wasm `select` semantics: pop
    /// `[val_true, val_false, cond]`, push `val_true` when `cond`
    /// is non-zero, else `val_false`.
    ///
    /// Mirrors the cranelift backend's `visit_select`: cranelift's
    /// `ins().select(cond, val_true, val_false)` maps onto LLVM's
    /// `build_select`, which takes `(cond_i1, then, else)`. The IR
    /// pass guarantees both arms share the same wasm slot, so the
    /// two popped values carry the same LLVM int type; `ty` is the
    /// IR tag re-stamped onto the pushed result.
    fn emit_select(&mut self, ip_hint: &str, ty: IrType) -> Result<(), LlvmError> {
        let cond = self.pop_int(ip_hint)?;
        let val_false = self.pop_int(ip_hint)?;
        let val_true = self.pop_int(ip_hint)?;
        // Narrow the i32 / i64 condition to i1 (non-zero test),
        // matching `emit_br_if` / `emit_if`.
        let name = self.next_name("select_cond");
        let cond_i1 = self
            .builder
            .build_int_compare(IntPredicate::NE, cond, cond.get_type().const_zero(), &name)
            .map_err(|e| LlvmError::Codegen(format!("Select cmp: {e}")))?;
        let sel_name = self.next_name("select");
        let selected = self
            .builder
            .build_select(cond_i1, val_true, val_false, &sel_name)
            .map_err(|e| LlvmError::Codegen(format!("Select: {e}")))?
            .into_int_value();
        self.push(selected, ty);
        Ok(())
    }

    /// Lower `Op::BrTable { default, targets }`. Pops one `i32`
    /// discriminant; when `index < targets.len()` jumps to
    /// `targets[index]`, otherwise jumps to `default`. Label depths
    /// resolve against the same `label_stack` as `Br` / `BrIf`.
    ///
    /// Ported from the cranelift backend's `emit_br_table`: cranelift's
    /// `br_table` + `JumpTable` maps onto LLVM's `build_switch`, with
    /// one `(i32 case_value, target_bb)` entry per `targets[i]` and the
    /// default arm pointing at `default`'s resolved block. The Phase B
    /// envelope's `Block`/`Loop` carry no result phi (`result_ty:
    /// None`), so — like `emit_br` — `BrTable` is value-less: the
    /// stack effect is `[i32] -> []` and no yield value rides the edges.
    /// After the switch the rest of the current block is unreachable,
    /// so we seal a dead block and open a fresh continuation, exactly
    /// as `emit_br` does.
    fn emit_br_table(
        &mut self,
        ip_hint: &str,
        default: u32,
        targets: &[u32],
    ) -> Result<(), LlvmError> {
        let idx = self.pop_int(ip_hint)?;
        // The discriminant must be i32-wide for the switch case
        // constants. The IR contract feeds an i32; if a wider value
        // arrived, truncate to keep the switch operand / case widths
        // in lockstep.
        let i32_t = self.ctx.i32_type();
        let idx_i32 = if idx.get_type().get_bit_width() == 32 {
            idx
        } else {
            self.builder
                .build_int_truncate(idx, i32_t, &self.next_name("brtable_idx"))
                .map_err(|e| LlvmError::Codegen(format!("BrTable idx trunc: {e}")))?
        };

        // Resolve the default target block by label depth.
        let default_bb = self.br_table_target_bb(default)?;

        // Resolve each per-index target and build the switch cases.
        let mut cases: Vec<(IntValue<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> =
            Vec::with_capacity(targets.len());
        for (i, depth) in targets.iter().enumerate() {
            let tgt_bb = self.br_table_target_bb(*depth)?;
            cases.push((i32_t.const_int(i as u64, false), tgt_bb));
        }

        self.builder
            .build_switch(idx_i32, default_bb, &cases)
            .map_err(|e| LlvmError::Codegen(format!("BrTable switch: {e}")))?;

        // After the switch the surrounding body is unreachable. Open a
        // fresh dead block sealed with `unreachable`, then a cont block
        // so subsequent ops have somewhere to land — mirrors `emit_br`.
        let dead_bb = self
            .ctx
            .append_basic_block(self.func, "unreachable_after_br_table");
        self.builder.position_at_end(dead_bb);
        self.builder
            .build_unreachable()
            .map_err(|e| LlvmError::Codegen(format!("BrTable dead-block unreachable: {e}")))?;
        let cont_bb = self
            .ctx
            .append_basic_block(self.func, "after_br_table_cont");
        self.builder.position_at_end(cont_bb);
        Ok(())
    }

    /// Resolve a `BrTable` arm's label depth to its branch target
    /// basic block. A `Block` frame's branch target is its `tail_bb`
    /// (forward exit); a `Loop` frame's is its `header_bb` (back-edge
    /// continue). Mirrors the `Br` / `BrIf` resolution in `emit_br` /
    /// `emit_br_if`.
    fn br_table_target_bb(
        &self,
        depth: u32,
    ) -> Result<inkwell::basic_block::BasicBlock<'ctx>, LlvmError> {
        let frame = self.label_target(depth)?;
        Ok(match frame.kind {
            LabelKind::Block => frame.tail_bb,
            LabelKind::Loop => frame.header_bb,
        })
    }

    /// Lower `Op::Return`. The shape decides what flows back:
    ///
    /// - Legacy-i64: pop the top of the operand stack and `ret v`.
    /// - Buffer-protocol: return a hard-coded i32 `return_root_size`
    ///   so the host trampoline reads back the full fixed area.
    ///   Phase B doesn't emit pointer-indirect StoreField, so the
    ///   tail-cursor path is dead — `return_root_size` is enough.
    ///
    /// Mirrors the cranelift backend's `emit_return` for the same
    /// shapes.
    pub(crate) fn emit_return(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        // Phase E.1: inline-frame return. The callee body pops the
        // typed return value, stores it into the frame's ret_slot,
        // then unconditionally jumps to exit_bb. The caller side picks
        // up from there in `emit_call_stdlib`.
        if let Some((ret_slot, exit_bb, ret_ty)) = self
            .inline_frames
            .last()
            .map(|f| (f.ret_slot, f.exit_bb, f.ret_ty))
        {
            let v = self.pop(ip_hint)?;
            // Coerce the popped value's width to the slot type if
            // needed (Bool / Null on an i32 stack but stored as i32
            // already — no coercion. String / ListInt on i32 — same.
            // I64 on i64 — same. We rely on the caller's typing
            // contract.)
            let stored = self.coerce_to_let_ty(v, ret_ty)?;
            self.builder
                .build_store(ret_slot, stored)
                .map_err(|e| LlvmError::Codegen(format!("inline Return store: {e}")))?;
            self.builder
                .build_unconditional_branch(exit_bb)
                .map_err(|e| LlvmError::Codegen(format!("inline Return br: {e}")))?;
            // Open a fresh dummy block so any subsequent ops the body
            // emits (e.g. dead trailing ConstBool after Trap) have
            // somewhere to land. LLVM's verifier prunes the dead chain.
            let dummy = self.ctx.append_basic_block(self.func, "after_inline_ret");
            self.builder.position_at_end(dummy);
            return Ok(());
        }
        // Phase D.1 fast path: the trailing buffer-protocol `Op::Return`
        // doesn't carry a value on the stack (the IR producer already
        // emitted a `StoreField` into the output buffer that the fast
        // emitter redirected into `ret_slot`). Load + `ret` from the
        // slot to produce the typed i64 result.
        if let Some(fast) = self.fast_path.as_ref() {
            let i64_t = self.ctx.i64_type();
            let v = self
                .builder
                .build_load(i64_t, fast.ret_slot, "fast_ret_load")
                .map_err(|e| LlvmError::Codegen(format!("fast Return load: {e}")))?
                .into_int_value();
            self.builder
                .build_return(Some(&v))
                .map_err(|e| LlvmError::Codegen(format!("fast Return: {e}")))?;
            // Open a dead continuation block so downstream ops have
            // somewhere to land — matches the buffer/legacy branches
            // below. The block stays dead; the verifier accepts it
            // once we seal with `unreachable` in `lower_body`'s
            // trailing branch.
            let cont = self.ctx.append_basic_block(self.func, "after_return_cont");
            self.builder.position_at_end(cont);
            // Suppress the `_` warning on ip_hint when this branch
            // runs.
            let _ = ip_hint;
            return Ok(());
        }
        // Phase E.2 helper-body return: when lowering a sibling
        // function rather than the entry, pop the operand and emit a
        // typed return matching the helper's declared IR return type.
        // Widens / truncates the popped i32 / i64 to the declared LLVM
        // ret slot when the two widths disagree.
        if let Some(ret_ty) = self.helper_ret_ty {
            let v = self.pop_int(ip_hint)?;
            // #359 (W20): `F64` joins `I64` on the 64-bit return slot —
            // it rides as its bit pattern in an i64 register (see
            // `ir_ty_to_llvm_abi`), so the helper / lambda LLVM ret type
            // is `i64` and the popped operand is already the f64 bits.
            let want_width = match ret_ty {
                IrType::I64 | IrType::F64 => 64,
                IrType::I32
                | IrType::Bool
                | IrType::Null
                | IrType::String
                | IrType::ListInt
                | IrType::ListFloat
                | IrType::ListBool
                | IrType::ListString
                | IrType::ListSchema
                | IrType::Closure
                | IrType::Dict => 32,
            };
            let have_width = v.get_type().get_bit_width();
            let final_v = if have_width == want_width {
                v
            } else if have_width < want_width {
                let target_ty = if want_width == 64 {
                    self.ctx.i64_type()
                } else {
                    self.ctx.i32_type()
                };
                self.builder
                    .build_int_z_extend(v, target_ty, "helper_ret_zext")
                    .map_err(|e| LlvmError::Codegen(format!("helper Return zext: {e}")))?
            } else {
                let target_ty = if want_width == 64 {
                    self.ctx.i64_type()
                } else {
                    self.ctx.i32_type()
                };
                self.builder
                    .build_int_truncate(v, target_ty, "helper_ret_trunc")
                    .map_err(|e| LlvmError::Codegen(format!("helper Return trunc: {e}")))?
            };
            self.builder
                .build_return(Some(&final_v))
                .map_err(|e| LlvmError::Codegen(format!("helper Return: {e}")))?;
            let cont = self.ctx.append_basic_block(self.func, "after_return_cont");
            self.builder.position_at_end(cont);
            return Ok(());
        }
        match self.shape {
            EntryShape::LegacyI64 => {
                let v = self.pop_int(ip_hint)?;
                self.builder
                    .build_return(Some(&v))
                    .map_err(|e| LlvmError::Codegen(format!("Return (legacy): {e}")))?;
            }
            EntryShape::Buffer => {
                let i32_t = self.ctx.i32_type();
                // Phase E.1: when the body emitted a pointer-indirect
                // StoreField (String / List* return) the trampoline
                // needs to know how many bytes past `out_ptr` the tail
                // cursor advanced to. Read it back from the state slot
                // so the host can decode the variable-length payload.
                // Bodies that only wrote into the fixed area keep the
                // historical "return root_size" path so a trampoline
                // that doesn't bother to consult `tail_cursor` still
                // works.
                let v: IntValue<'ctx> = if self.needs_tail_cursor {
                    let state_ptr = self.state_ptr.ok_or_else(|| {
                        LlvmError::Codegen(
                            "buffer Return needs tail_cursor but state ptr unavailable".into(),
                        )
                    })?;
                    let i8_t = self.ctx.i8_type();
                    let tail_gep = unsafe {
                        self.builder
                            .build_in_bounds_gep(
                                i8_t,
                                state_ptr,
                                &[i32_t
                                    .const_int(u64::from(ARENA_STATE_OFFSET_TAIL_CURSOR), false)],
                                "tail_cursor_gep",
                            )
                            .map_err(|e| LlvmError::Codegen(format!("tail_cursor GEP: {e}")))?
                    };
                    self.builder
                        .build_load(i32_t, tail_gep, "tail_cursor")
                        .map_err(|e| LlvmError::Codegen(format!("tail_cursor load: {e}")))?
                        .into_int_value()
                } else {
                    i32_t.const_int(u64::from(self.buffer_return_size), false)
                };
                self.builder
                    .build_return(Some(&v))
                    .map_err(|e| LlvmError::Codegen(format!("Return (buffer): {e}")))?;
            }
        }
        // After the explicit return, the rest of the surrounding
        // body is unreachable. Open a fresh continuation block so
        // any subsequent ops (a stray `LetGet` after a Br-tail
        // Return, etc.) emit somewhere valid. The block is dead;
        // LLVM's verifier accepts it as long as it ends with a
        // terminator — we seal it with `unreachable` lazily when
        // the next terminator-emitting op needs to bind it.
        let cont = self.ctx.append_basic_block(self.func, "after_return_cont");
        self.builder.position_at_end(cont);
        Ok(())
    }

    pub(crate) fn emit_block(
        &mut self,
        result_ty: Option<IrType>,
        body: &[TaggedOp],
    ) -> Result<(), LlvmError> {
        if result_ty.is_some() {
            return Err(LlvmError::Codegen(
                "Block with result_ty: Phase B envelope does not carry block-result phis".into(),
            ));
        }
        let header_bb = self.ctx.append_basic_block(self.func, "block_head");
        let tail_bb = self.ctx.append_basic_block(self.func, "block_tail");

        // Fallthrough from the current insertion point into the
        // block's header.
        self.builder
            .build_unconditional_branch(header_bb)
            .map_err(|e| LlvmError::Codegen(format!("Block fallthrough: {e}")))?;
        self.builder.position_at_end(header_bb);

        self.label_stack.push(LabelFrame {
            header_bb,
            tail_bb,
            kind: LabelKind::Block,
        });
        // Devirtualisation (W18) correctness: a `Br` can exit the block
        // early, skipping a later `LetSet { Closure }`; a `LetGet` after
        // the block (the `Br` target) would then read a slot the emitter
        // believes holds a known closure (because emission walks the
        // body linearly) but a runtime early-exit path never set. Drop,
        // around the block, every closure slot the body reassigns — the
        // post-block `LetGet` then falls back to the runtime switch on
        // the early-exit path. Straight-line uses inside the block still
        // devirtualise.
        let mut body_closure_setslots: Vec<u32> = Vec::new();
        collect_closure_letset_slots(body, &mut body_closure_setslots);
        for s in &body_closure_setslots {
            self.known_closure_let_slots.remove(&self.remap_let_idx(*s));
        }
        for (ip, tagged) in body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        for s in &body_closure_setslots {
            self.known_closure_let_slots.remove(&self.remap_let_idx(*s));
        }
        // If the body ran without an explicit `Br`, fall through to
        // `tail_bb`. A `Br` that fired already terminated the current
        // block via `build_unconditional_branch`; in that case the
        // builder's current block is already terminated and we must
        // not emit another branch.
        let cur_terminated = self
            .builder
            .get_insert_block()
            .and_then(|bb| bb.get_terminator())
            .is_some();
        if !cur_terminated {
            self.builder
                .build_unconditional_branch(tail_bb)
                .map_err(|e| LlvmError::Codegen(format!("Block tail fallthrough: {e}")))?;
        }
        self.builder.position_at_end(tail_bb);
        self.label_stack.pop();
        Ok(())
    }

    pub(crate) fn emit_loop(&mut self, result_ty: Option<IrType>, body: &[TaggedOp]) -> Result<(), LlvmError> {
        if result_ty.is_some() {
            return Err(LlvmError::Codegen(
                "Loop with result_ty: Phase B envelope does not carry loop-result phis".into(),
            ));
        }
        let header_bb = self.ctx.append_basic_block(self.func, "loop_head");
        let tail_bb = self.ctx.append_basic_block(self.func, "loop_tail");

        self.builder
            .build_unconditional_branch(header_bb)
            .map_err(|e| LlvmError::Codegen(format!("Loop fallthrough: {e}")))?;
        self.builder.position_at_end(header_bb);

        self.label_stack.push(LabelFrame {
            header_bb,
            tail_bb,
            kind: LabelKind::Loop,
        });
        // Devirtualisation (W18) correctness: a loop body runs 0+ times
        // and a `LetGet` at the top of the body re-executes every
        // iteration, so a `KnownClosure` let-slot the body *reassigns*
        // cannot be trusted on a path that reads it before that
        // reassignment ran (iteration 1's top, or after a 0-trip loop).
        // Conservatively drop, both before emitting the body and on loop
        // exit, every slot the body contains a `LetSet { Closure }` for
        // (at any nesting depth). A within-body `MakeClosure; LetSet`
        // still re-establishes the entry in source order for the reads
        // that follow it in the same iteration (its `fn_table_idx` is a
        // compile-time constant, identical every iteration), so
        // straight-line uses inside the body keep devirtualising; only
        // cross-iteration / loop-carried reads fall back to the switch.
        // W18's filter loop reads its predicate via the inline-frame
        // param (not a body-bound let), so it is unaffected.
        let mut body_closure_setslots: Vec<u32> = Vec::new();
        collect_closure_letset_slots(body, &mut body_closure_setslots);
        for s in &body_closure_setslots {
            self.known_closure_let_slots.remove(&self.remap_let_idx(*s));
        }
        for (ip, tagged) in body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        for s in &body_closure_setslots {
            self.known_closure_let_slots.remove(&self.remap_let_idx(*s));
        }
        // If the body fell through without an explicit `Br`, that's
        // an implicit "exit the loop" in wasm semantics — the loop
        // body executed once and the loop terminates. Emit a branch
        // to `tail_bb`.
        let cur_terminated = self
            .builder
            .get_insert_block()
            .and_then(|bb| bb.get_terminator())
            .is_some();
        if !cur_terminated {
            self.builder
                .build_unconditional_branch(tail_bb)
                .map_err(|e| LlvmError::Codegen(format!("Loop implicit exit: {e}")))?;
        }
        self.builder.position_at_end(tail_bb);
        self.label_stack.pop();
        Ok(())
    }

    pub(crate) fn label_target(&self, depth: u32) -> Result<&LabelFrame<'ctx>, LlvmError> {
        let len = self.label_stack.len();
        let idx = len
            .checked_sub(1 + depth as usize)
            .ok_or_else(|| LlvmError::Codegen(format!("label_depth {depth} out of range")))?;
        Ok(&self.label_stack[idx])
    }

    pub(crate) fn emit_br(&mut self, label_depth: u32) -> Result<(), LlvmError> {
        let target = self.label_target(label_depth)?;
        let bb = match target.kind {
            LabelKind::Block => target.tail_bb,
            LabelKind::Loop => target.header_bb,
        };
        self.builder
            .build_unconditional_branch(bb)
            .map_err(|e| LlvmError::Codegen(format!("Br: {e}")))?;
        // After a `Br`, the rest of the surrounding body is
        // unreachable in wasm semantics. LLVM does not allow
        // emitting more instructions into a terminated block — we
        // open a fresh `unreachable_after_br` block so the
        // emitter's invariants stay satisfied. The block stays
        // dead; LLVM's verifier and -O2 prune it.
        let dead_bb = self
            .ctx
            .append_basic_block(self.func, "unreachable_after_br");
        self.builder.position_at_end(dead_bb);
        // Seal it with an `unreachable` so the verifier accepts the
        // dead block before -O2 cleans it up.
        self.builder
            .build_unreachable()
            .map_err(|e| LlvmError::Codegen(format!("dead-block unreachable: {e}")))?;
        // Reposition to a fresh successor so subsequent ops have an
        // open block to emit into. The successor will itself become
        // dead, but the verifier is happy with the chain.
        let cont_bb = self.ctx.append_basic_block(self.func, "after_br_cont");
        self.builder.position_at_end(cont_bb);
        Ok(())
    }

    pub(crate) fn emit_br_if(&mut self, ip_hint: &str, label_depth: u32) -> Result<(), LlvmError> {
        let cond = self.pop_int(ip_hint)?;
        // Narrow the i32 / i64 condition to i1.
        let zero = cond.get_type().const_zero();
        let name = self.next_name("br_cond");
        let cond_i1 = self
            .builder
            .build_int_compare(IntPredicate::NE, cond, zero, &name)
            .map_err(|e| LlvmError::Codegen(format!("BrIf cmp: {e}")))?;
        let target = self.label_target(label_depth)?;
        let take_bb = match target.kind {
            LabelKind::Block => target.tail_bb,
            LabelKind::Loop => target.header_bb,
        };
        // Fall-through path stays in the surrounding body.
        let fallthru_bb = self.ctx.append_basic_block(self.func, "br_if_fallthru");
        self.builder
            .build_conditional_branch(cond_i1, take_bb, fallthru_bb)
            .map_err(|e| LlvmError::Codegen(format!("BrIf: {e}")))?;
        self.builder.position_at_end(fallthru_bb);
        Ok(())
    }

    pub(crate) fn emit_if(
        &mut self,
        ip_hint: &str,
        result_ty: IrType,
        then_body: &[TaggedOp],
        else_body: &[TaggedOp],
    ) -> Result<(), LlvmError> {
        let cond = self.pop_int(ip_hint)?;
        let name = self.next_name("if_cond");
        let cond_i1 = self
            .builder
            .build_int_compare(IntPredicate::NE, cond, cond.get_type().const_zero(), &name)
            .map_err(|e| LlvmError::Codegen(format!("If cmp: {e}")))?;
        let then_bb = self.ctx.append_basic_block(self.func, "if_then");
        let else_bb = self.ctx.append_basic_block(self.func, "if_else");
        let merge_bb = self.ctx.append_basic_block(self.func, "if_merge");
        self.builder
            .build_conditional_branch(cond_i1, then_bb, else_bb)
            .map_err(|e| LlvmError::Codegen(format!("If branch: {e}")))?;

        // Devirtualisation (W18) correctness: the `KnownClosure`
        // let-slot tracker is path-insensitive (it mutates a flat map as
        // the emitter walks ops). Across an `If`, the two arms may bind
        // the *same* let-slot to *different* known closures; a flat
        // last-write would let a post-merge `LetGet` devirtualise to the
        // wrong target on the path that didn't run — a miscompile. Take
        // the dataflow *meet*: snapshot the map, emit each arm against
        // its own copy, then keep only entries both arms agree on (and
        // entries neither touched). Disagreements are dropped, so the
        // post-merge `LetGet` falls back to the runtime switch (always
        // correct). Straight-line shapes like W18 are unaffected.
        let known_closure_snapshot = self.known_closure_let_slots.clone();

        // Then arm.
        self.builder.position_at_end(then_bb);
        for (ip, tagged) in then_body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        let then_result = self.pop(ip_hint).ok();
        let then_known_closures = std::mem::replace(
            &mut self.known_closure_let_slots,
            known_closure_snapshot.clone(),
        );
        let then_end_bb = self.builder.get_insert_block().unwrap();
        let then_terminated = then_end_bb.get_terminator().is_some();
        if !then_terminated {
            self.builder
                .build_unconditional_branch(merge_bb)
                .map_err(|e| LlvmError::Codegen(format!("If then->merge: {e}")))?;
        }

        // Else arm (starts from the pre-If snapshot, restored above).
        self.builder.position_at_end(else_bb);
        for (ip, tagged) in else_body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        let else_result = self.pop(ip_hint).ok();
        let else_known_closures =
            std::mem::replace(&mut self.known_closure_let_slots, known_closure_snapshot);
        let else_end_bb = self.builder.get_insert_block().unwrap();
        let else_terminated = else_end_bb.get_terminator().is_some();
        if !else_terminated {
            self.builder
                .build_unconditional_branch(merge_bb)
                .map_err(|e| LlvmError::Codegen(format!("If else->merge: {e}")))?;
        }
        // Meet: keep a `KnownClosure` slot only when both arms reached
        // the merge with the SAME known target. A slot only one arm
        // touched, or that the arms disagree on, is dropped. An arm that
        // terminated (e.g. `Return`) cannot reach the merge, so the
        // surviving arm's view governs the slots it owns.
        self.known_closure_let_slots = match (then_terminated, else_terminated) {
            (true, true) => std::collections::HashMap::new(),
            (true, false) => else_known_closures,
            (false, true) => then_known_closures,
            (false, false) => then_known_closures
                .iter()
                .filter_map(|(&k, &v)| (else_known_closures.get(&k) == Some(&v)).then_some((k, v)))
                .collect(),
        };

        // Merge phi if both arms terminated normally.
        self.builder.position_at_end(merge_bb);
        match (then_result, else_result) {
            (Some(t), Some(e)) => {
                let phi_ty: inkwell::types::BasicTypeEnum<'ctx> = match result_ty {
                    // AOT-1: F64 rides as i64 bits, so both arms feed an
                    // i64-typed phi (the bit pattern, never a `double`).
                    IrType::I64 | IrType::F64 => self.ctx.i64_type().into(),
                    IrType::I32 | IrType::Bool | IrType::Null => self.ctx.i32_type().into(),
                    other => {
                        return Err(LlvmError::Codegen(format!(
                            "If result_ty {other:?} unsupported"
                        )));
                    }
                };
                let phi = self
                    .builder
                    .build_phi(phi_ty, "if_phi")
                    .map_err(|e| LlvmError::Codegen(format!("If phi: {e}")))?;
                let then_val: BasicValueEnum<'ctx> = t.val.into();
                let else_val: BasicValueEnum<'ctx> = e.val.into();
                if !then_terminated {
                    phi.add_incoming(&[(&then_val, then_end_bb)]);
                }
                if !else_terminated {
                    phi.add_incoming(&[(&else_val, else_end_bb)]);
                }
                let v = phi.as_basic_value().into_int_value();
                self.push(v, result_ty);
            }
            _ => {
                // One arm didn't push (e.g. ended with Return).
                // Phase B's W1/W2 path doesn't exercise this — surface
                // an error so a future shape doesn't silently miscompile.
                if !then_terminated || !else_terminated {
                    return Err(LlvmError::Codegen(
                        "If arms produced no value but did not terminate".into(),
                    ));
                }
                // Both arms terminated (e.g. both Return). Surface
                // `merge_bb` as unreachable.
                self.builder
                    .build_unreachable()
                    .map_err(|e| LlvmError::Codegen(format!("If merge unreachable: {e}")))?;
            }
        }

        Ok(())
    }
}
