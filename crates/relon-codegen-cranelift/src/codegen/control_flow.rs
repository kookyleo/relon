//! Control-flow lowering helpers for [`super::Codegen`]:
//! `Op::If` / `Op::Block` / `Op::Loop` / `Op::Br` / `Op::BrIf` /
//! `Op::BrTable` / `Op::Return` / `Op::Trap` plus the
//! [`Codegen::emit_loop_back_resource_check`] cadence helper and
//! [`Codegen::placeholder_for`] zero-typed-undef helper.
//!
//! Cranelift's IR is a flat-CFG basic-block form, while the IR's
//! shape mirrors wasm's structured `Block` / `Loop` / `If`. These
//! helpers reconcile the two: each structured op pushes a
//! [`super::LabelFrame`] onto `label_stack` so subsequent `Br` /
//! `BrIf` / `BrTable` ops can resolve to the matching cranelift
//! block. The yield-typed variants (loops + blocks that thread a
//! value through their break edge) wire a cranelift block-param
//! through the same label frame.
//!
//! The lifetime of stack discipline + register state stays on
//! [`super::Codegen`] — these helpers only own the structured
//! lowering decisions.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{
    BlockArg, BlockCall, InstBuilder, JumpTableData, MemFlags, Value as CValue,
};
use cranelift_frontend::Variable;

use relon_ir::ir::{IrType, TaggedOp};

use crate::error::CraneliftError;
use crate::sandbox::{TrapKind, STATE_OFFSET_TAIL_CURSOR};

use super::{ir_ty_to_cl, EntryShape, LabelFrame};

impl<'a, 'b> super::Codegen<'a, 'b> {
    /// Lower a wasm `Block` (forward exit) or `Loop` (back edge) into
    /// cranelift's flat-CFG block form.
    ///
    /// For both shapes we create a cranelift block ahead of the body
    /// and push a `LabelFrame` onto `label_stack`; `Op::Br` /
    /// `Op::BrIf` / `Op::BrTable` resolve to that block by depth-
    /// counting from the top of the stack.
    ///
    /// * `is_loop = false` (wasm `Block`): the `target_block` is the
    ///   **continuation** block reached after the body terminates;
    ///   `Br 0` jumps forward past the body's End. When `result_ty =
    ///   Some(t)`, the continuation has one block-param of type `t`;
    ///   fallthrough at body end pops the top of the operand stack
    ///   and forwards it as the continuation arg.
    /// * `is_loop = true` (wasm `Loop`): the `target_block` is the
    ///   loop **header**; `Br 0` jumps back to re-enter the loop
    ///   (equivalent to `continue`). When `result_ty = Some(t)`, the
    ///   header has one block-param of type `t` representing the
    ///   loop-carried accumulator. Each back-edge re-supplies the
    ///   next iteration's value; the loop "exits" through fall-
    ///   through to a continuation block which inherits the final
    ///   value. The loop's seed value is popped off the operand
    ///   stack before entering the header (wasm semantics).
    ///
    /// v5-β-2 stage 5 widens this to handle `result_ty != None` via
    /// cranelift block-parameter threading. Stdlib bodies in practice
    /// still use `result_ty = None`, but the yield-shape variant
    /// surfaces clean via `Op::Loop { result_ty: Some(_) }` for hand-
    /// rolled IR + the BrTable test suite.
    pub(super) fn emit_block(
        &mut self,
        result_ty: Option<IrType>,
        body: &[TaggedOp],
        is_loop: bool,
    ) -> Result<(), CraneliftError> {
        let result_cl_ty = match result_ty {
            None => None,
            Some(ty) => Some(ir_ty_to_cl(ty)?),
        };

        if is_loop {
            // Loop: branch into a fresh header block, lower the
            // body inside it. The body's terminator (Br / fallthrough
            // / Return) decides whether the loop exits or re-enters.
            let header = self.builder.create_block();
            // Loop header carries the loop-carried accumulator as a
            // block parameter when the loop yields a value. The seed
            // value is the top of the operand stack at loop entry.
            let seed = if let Some(cl_ty) = result_cl_ty {
                self.builder.append_block_param(header, cl_ty);
                Some(self.pop()?)
            } else {
                None
            };
            let seed_args: Vec<BlockArg> = seed.into_iter().map(BlockArg::from).collect();
            self.builder.ins().jump(header, &seed_args);
            self.builder.switch_to_block(header);
            // Push the header block-param onto the operand stack so
            // the body's first op consumes the loop-carried value
            // (wasm-Loop semantics: the yield value re-enters the
            // operand stack each iteration). The body is responsible
            // for stashing / using / re-yielding it before the back-
            // edge.
            if result_cl_ty.is_some() {
                let v = self.builder.block_params(header)[0];
                self.push(v);
            }
            // For yielding loops, prepare a continuation block. The
            // loop's normal fallthrough lands there carrying the
            // final accumulator as a block-param. The frame remembers
            // the continuation so back-edges can re-enter while non-
            // looping `Br N` past the loop's enclosing label still
            // lands at the right place.
            let loop_cont_block = if result_cl_ty.is_some() {
                Some(self.builder.create_block())
            } else {
                None
            };
            if let (Some(cl_ty), Some(cont)) = (result_cl_ty, loop_cont_block) {
                self.builder.append_block_param(cont, cl_ty);
            }
            // Allocate a back-edge counter if the sandbox deadline
            // check is on. Initialised to 0 here; each back-edge
            // bumps + checks at the `RESOURCE_CHECK_INTERVAL` cadence.
            let back_edge_counter = if self.sandbox.deadline_check {
                let var = self.builder.declare_var(I64);
                let zero = self.builder.ins().iconst(I64, 0);
                self.builder.def_var(var, zero);
                Some(var)
            } else {
                None
            };
            // Loops with no other entry edge get sealed once the body
            // lowers — cranelift seals retroactively for blocks with
            // forward branches, so we leave it unsealed during the
            // body walk and seal at the end.
            self.label_stack.push(LabelFrame {
                target_block: header,
                is_loop: true,
                result_cl_ty,
                loop_cont_block,
                back_edge_counter,
            });
            self.emit_body(body)?;
            let frame = self.label_stack.pop().expect("just pushed");
            self.builder.seal_block(header);
            if let Some(cont) = frame.loop_cont_block {
                // Fall through to cont with the current top-of-stack
                // as the final loop value. Skip the fall-through
                // jump when the body already terminated (the body
                // always Br-back-edged); cranelift's DCE handles
                // the dead exit path.
                if !self.builder.is_unreachable() {
                    let cont_arg = if let Some(cl_ty) = result_cl_ty {
                        if self.stack.is_empty() {
                            self.placeholder_for(cl_ty)
                        } else {
                            self.pop()?
                        }
                    } else {
                        self.builder.ins().iconst(I32, 0)
                    };
                    self.builder.ins().jump(cont, &[cont_arg.into()]);
                }
                self.builder.seal_block(cont);
                self.builder.switch_to_block(cont);
                // The continuation block-param is the loop's result;
                // push it onto the operand stack.
                let v = self.builder.block_params(cont)[0];
                self.push(v);
            }
        } else {
            // Block (forward exit): allocate a continuation block,
            // lower the body, then switch to the continuation. A
            // `Br 0` inside the body jumps forward to `cont`.
            let cont = self.builder.create_block();
            if let Some(cl_ty) = result_cl_ty {
                self.builder.append_block_param(cont, cl_ty);
            }
            self.label_stack.push(LabelFrame {
                target_block: cont,
                is_loop: false,
                result_cl_ty,
                loop_cont_block: None,
                back_edge_counter: None,
            });
            self.emit_body(body)?;
            self.label_stack.pop();
            // Fallthrough to cont when the body doesn't explicitly
            // branch out. We forward the top-of-stack value as the
            // continuation block-param when the block yields. Skip
            // the jump entirely if the body already terminated (the
            // current block is unreachable / already filled by a Br
            // / BrTable / Return / Trap).
            if !self.builder.is_unreachable() {
                let fall_args: Vec<BlockArg> = if let Some(cl_ty) = result_cl_ty {
                    let v = if !self.stack.is_empty() {
                        self.pop()?
                    } else {
                        self.placeholder_for(cl_ty)
                    };
                    vec![v.into()]
                } else {
                    Vec::new()
                };
                self.builder.ins().jump(cont, &fall_args);
            }
            self.builder.seal_block(cont);
            self.builder.switch_to_block(cont);
            // When the block yields, expose the block-param to the
            // surrounding code via the operand stack.
            if result_cl_ty.is_some() {
                let v = self.builder.block_params(cont)[0];
                self.push(v);
            }
        }
        Ok(())
    }

    /// Emit the periodic deadline guard at a loop back-edge. Bumps
    /// the frame's per-loop counter; when `(counter &
    /// (RESOURCE_CHECK_INTERVAL - 1)) == 0`, emits a resource-check
    /// guard (one host clock read + comparison). `RESOURCE_CHECK_INTERVAL`
    /// is a power of two so the modulus is cheap.
    pub(super) fn emit_loop_back_resource_check(&mut self, counter_var: Variable) {
        let cur = self.builder.use_var(counter_var);
        let one = self.builder.ins().iconst(I64, 1);
        let next = self.builder.ins().iadd(cur, one);
        self.builder.def_var(counter_var, next);
        let mask = self
            .builder
            .ins()
            .iconst(I64, (crate::sandbox::RESOURCE_CHECK_INTERVAL as i64) - 1);
        let masked = self.builder.ins().band(next, mask);
        // Branch to a fresh deadline-check block when masked == 0;
        // otherwise just skip. Use brif + a tiny block layout so the
        // common (non-zero) case stays branch-predicted.
        let check_block = self.builder.create_block();
        let after_block = self.builder.create_block();
        let zero = self.builder.ins().iconst(I64, 0);
        let cmp = self.builder.ins().icmp(IntCC::Equal, masked, zero);
        self.builder
            .ins()
            .brif(cmp, check_block, &[], after_block, &[]);
        self.builder.seal_block(check_block);
        self.builder.switch_to_block(check_block);
        self.emit_resource_check();
        self.builder.ins().jump(after_block, &[]);
        self.builder.seal_block(after_block);
        self.builder.switch_to_block(after_block);
    }

    /// Lower `Op::Br { label_depth }` (unconditional) or
    /// `Op::BrIf { label_depth }` (conditional, popping the
    /// condition off the stack).
    pub(super) fn emit_br(
        &mut self,
        label_depth: u32,
        conditional: bool,
    ) -> Result<(), CraneliftError> {
        let depth = label_depth as usize;
        if depth >= self.label_stack.len() {
            return Err(CraneliftError::Codegen(format!(
                "Br/BrIf label_depth {label_depth} out of range — only {} frame(s) on stack",
                self.label_stack.len()
            )));
        }
        let frame_idx = self.label_stack.len() - 1 - depth;
        let target = self.label_stack[frame_idx].target_block;
        let result_cl_ty = self.label_stack[frame_idx].result_cl_ty;
        let is_loop = self.label_stack[frame_idx].is_loop;
        let back_edge_counter = self.label_stack[frame_idx].back_edge_counter;

        // For yielded targets, pop the top-of-stack and forward as
        // the block-arg. We do this once for both branch shapes.
        let block_args: Vec<BlockArg> = if let Some(cl_ty) = result_cl_ty {
            let v = if !self.builder.is_unreachable() && !self.stack.is_empty() {
                self.pop()?
            } else {
                self.placeholder_for(cl_ty)
            };
            vec![v.into()]
        } else {
            Vec::new()
        };

        if conditional {
            // Pop the i32 condition. cranelift `brif(cond, then,
            // else)` needs both arms; for the "fallthrough" arm we
            // create a fresh block and switch into it after the
            // branch so subsequent ops land somewhere valid.
            let cond = self.pop()?;
            let fallthrough = self.builder.create_block();
            // RESOURCE_CHECK_INTERVAL cadence: if this branch is a
            // loop back-edge, emit the periodic deadline check
            // before the brif. We use the "then" arm (jump-to-loop-
            // header) as the loop continuation, so the check fires
            // only when the loop actually iterates.
            if is_loop {
                if let Some(counter_var) = back_edge_counter {
                    // The check needs to run conditionally — we need
                    // a separate then-arm block that holds the
                    // counter bump + check, then jumps to the loop
                    // header. The else-arm falls through unchanged.
                    let take_branch = self.builder.create_block();
                    self.builder
                        .ins()
                        .brif(cond, take_branch, &[], fallthrough, &[]);
                    self.builder.seal_block(take_branch);
                    self.builder.switch_to_block(take_branch);
                    self.emit_loop_back_resource_check(counter_var);
                    self.builder.ins().jump(target, &block_args);
                    self.builder.seal_block(fallthrough);
                    self.builder.switch_to_block(fallthrough);
                    return Ok(());
                }
            }
            self.builder
                .ins()
                .brif(cond, target, &block_args, fallthrough, &[]);
            self.builder.seal_block(fallthrough);
            self.builder.switch_to_block(fallthrough);
        } else {
            // Unconditional back-edge: also gets the cadence guard.
            if is_loop {
                if let Some(counter_var) = back_edge_counter {
                    self.emit_loop_back_resource_check(counter_var);
                }
            }
            self.builder.ins().jump(target, &block_args);
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

    /// Lower `Op::BrTable { default, targets }`. Pops one `i32` index
    /// from the stack; when `index < targets.len()` jumps to
    /// `targets[index]`, otherwise jumps to `default`. The label
    /// depths resolve against the same `label_stack` as `Br` / `BrIf`.
    ///
    /// Yield-typed targets are supported only when every target (incl.
    /// default) shares the same `result_cl_ty`. The IR shape
    /// guarantees this: `Op::BrTable` is a single discriminant
    /// dispatch where every arm produces a value of the same type.
    /// Mismatch surfaces as Codegen.
    pub(super) fn emit_br_table(
        &mut self,
        default: u32,
        targets: &[u32],
    ) -> Result<(), CraneliftError> {
        // Pop the discriminant; we'll feed it directly to br_table.
        let idx_val = self.pop()?;
        let default_depth = default as usize;
        if default_depth >= self.label_stack.len() {
            return Err(CraneliftError::Codegen(format!(
                "BrTable default depth {default} out of range — only {} frame(s) on stack",
                self.label_stack.len()
            )));
        }
        let default_frame_idx = self.label_stack.len() - 1 - default_depth;
        let default_target = self.label_stack[default_frame_idx].target_block;
        let yield_ty = self.label_stack[default_frame_idx].result_cl_ty;
        // Validate every target depth + cross-check yield types.
        for (i, depth) in targets.iter().enumerate() {
            let d = *depth as usize;
            if d >= self.label_stack.len() {
                return Err(CraneliftError::Codegen(format!(
                    "BrTable target #{i} depth {depth} out of range — only {} frame(s) on stack",
                    self.label_stack.len()
                )));
            }
            let frame = &self.label_stack[self.label_stack.len() - 1 - d];
            if frame.result_cl_ty != yield_ty {
                return Err(CraneliftError::Codegen(format!(
                    "BrTable target #{i} yield type {:?} disagrees with default {:?}",
                    frame.result_cl_ty, yield_ty
                )));
            }
        }
        // If any of the targets is a loop back-edge with a back-edge
        // counter, we can't directly weave the resource check into
        // br_table — the cadence guard belongs on the actual taken
        // arm. v5-β-2 stage 5 takes the safe approach: emit the
        // br_table without per-arm cadence, and rely on the prologue
        // + outer-loop guard for the bound. (Inner `BrIf` back-edges
        // still benefit from the cadence.) The single-deadline
        // safety net stays intact because the prologue's check
        // runs on every entry call.
        // Pop the yield value (if any) — every arm receives the same
        // value (wasm semantics: the operand stack at the BrTable
        // point is shared by every arm).
        let yield_arg: Option<CValue> = if let Some(cl_ty) = yield_ty {
            Some(
                if !self.builder.is_unreachable() && !self.stack.is_empty() {
                    self.pop()?
                } else {
                    self.placeholder_for(cl_ty)
                },
            )
        } else {
            None
        };

        // Build the BlockCalls + JumpTable. Each call carries the
        // optional yield value as its block-arg.
        let yield_args_slice: Vec<BlockArg> = yield_arg.iter().map(|v| (*v).into()).collect();
        let default_call = self
            .builder
            .func
            .dfg
            .block_call(default_target, &yield_args_slice);
        let target_calls: Vec<BlockCall> = targets
            .iter()
            .map(|depth| {
                let d = *depth as usize;
                let tgt = self.label_stack[self.label_stack.len() - 1 - d].target_block;
                self.builder.func.dfg.block_call(tgt, &yield_args_slice)
            })
            .collect();
        let jt_data = JumpTableData::new(default_call, &target_calls);
        let jt = self.builder.create_jump_table(jt_data);
        self.builder.ins().br_table(idx_val, jt);
        // After br_table the rest of the block is unreachable. Create
        // a dummy fallthrough so subsequent op emission lands somewhere.
        let dummy = self.builder.create_block();
        self.builder.seal_block(dummy);
        self.builder.switch_to_block(dummy);
        Ok(())
    }

    /// Lower `Op::If { result_ty, then_body, else_body }`. Pops the
    /// `i32` condition, lowers each arm into its own cranelift block,
    /// and joins on a typed block-param so the surrounding code reads
    /// the unified yield value.
    pub(super) fn emit_if(
        &mut self,
        result_ty: IrType,
        then_body: &[TaggedOp],
        else_body: &[TaggedOp],
    ) -> Result<(), CraneliftError> {
        let cond = self.pop()?;
        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let join_block = self.builder.create_block();

        // Mirror `emit_block`'s widened type map: native `F64` joins,
        // i64/i32 scalars, and the i32 arena-handle leaves (String /
        // List* / Closure). W20's `pair_force`/`accel` ternaries yield
        // native `F64`; the list-valued reduce body yields a `ListFloat`
        // i32 handle. The shared `ir_ty_to_cl` is the single source of
        // truth for the IR-type -> clif-type mapping.
        let cr_ty = ir_ty_to_cl(result_ty)?;
        self.builder.append_block_param(join_block, cr_ty);

        self.builder
            .ins()
            .brif(cond, then_block, &[], else_block, &[]);
        self.builder.seal_block(then_block);
        self.builder.seal_block(else_block);

        // Then-arm. Push the join block as a label frame so a nested
        // `Br 0` (or higher depths threading through `If`) finds the
        // right target — wasm semantics treat `If` as a labeled block
        // whose break target is the matching `End`.
        self.builder.switch_to_block(then_block);
        let stack_before_then = self.stack.len();
        // `If` is treated as a labelled block whose break target is the
        // matching End — but the result value is consumed via the
        // join-block phi (the explicit `If` lowering pattern) rather
        // than via the label-frame yield path. So we leave
        // `result_cl_ty = None` on the frame to avoid double-popping
        // the yield value.
        self.label_stack.push(LabelFrame {
            target_block: join_block,
            is_loop: false,
            result_cl_ty: None,
            loop_cont_block: None,
            back_edge_counter: None,
        });
        self.emit_body(then_body)?;
        self.label_stack.pop();
        // The arm may have terminated early (Br / Trap) and switched
        // to a dummy unreachable block. In that case any "value left
        // on the stack" is stale — we ignore the stack-discipline
        // check and feed cranelift a placeholder undef-like value so
        // the unreachable block still jumps to join_block with a
        // typed arg. The DCE pass drops the dummy on the floor.
        let then_result = if self.stack.len() == stack_before_then + 1 {
            self.stack.pop().unwrap()
        } else if self.stack.len() < stack_before_then {
            return Err(CraneliftError::Codegen(
                "If then-body underflowed the stack".into(),
            ));
        } else {
            // Stack drifted (e.g. Br/Trap terminated early without
            // pushing); emit an iconst placeholder so the join_block
            // edge stays typed. Codegen of subsequent ops uses the
            // join_block param, never this placeholder.
            self.placeholder_for(cr_ty)
        };
        self.builder.ins().jump(join_block, &[then_result.into()]);
        // Drop anything else the arm leaked.
        self.stack.truncate(stack_before_then);

        // Else-arm.
        self.builder.switch_to_block(else_block);
        let stack_before_else = self.stack.len();
        self.label_stack.push(LabelFrame {
            target_block: join_block,
            is_loop: false,
            result_cl_ty: None,
            loop_cont_block: None,
            back_edge_counter: None,
        });
        self.emit_body(else_body)?;
        self.label_stack.pop();
        let else_result = if self.stack.len() == stack_before_else + 1 {
            self.stack.pop().unwrap()
        } else if self.stack.len() < stack_before_else {
            return Err(CraneliftError::Codegen(
                "If else-body underflowed the stack".into(),
            ));
        } else {
            self.placeholder_for(cr_ty)
        };
        self.builder.ins().jump(join_block, &[else_result.into()]);
        self.stack.truncate(stack_before_else);

        self.builder.seal_block(join_block);
        self.builder.switch_to_block(join_block);
        let join_val = self.builder.block_params(join_block)[0];
        self.push(join_val);
        Ok(())
    }

    /// Emit the function's `Return`:
    ///   * Inline frame active — pop the top of the virtual stack
    ///     and `jump exit_block(v)`, finishing the callee body.
    ///   * LegacyI64Args (no inline) — pop the top of the virtual
    ///     stack and emit `return v: i64`.
    ///   * BufferProtocol (no inline) — the wasm-side semantics
    ///     push `i32 bytes_written` (the tail cursor when the body
    ///     emitted pointer-indirect stores, else `return_root_size`)
    ///     and end the function.
    pub(super) fn emit_return(&mut self) -> Result<(), CraneliftError> {
        if let Some(exit) = self.inline_frames.last().map(|f| f.exit_block) {
            // Inline-frame return: jump to the exit block with the
            // popped value as the block param. The caller's
            // `emit_call_stdlib` continues from there.
            let v = self.pop()?;
            self.builder.ins().jump(exit, &[v.into()]);
            // After the unconditional jump, the rest of the basic
            // block is unreachable. Provide a dummy block so any
            // subsequent ops emitted before the inline frame is
            // popped land somewhere valid.
            let dummy = self.builder.create_block();
            self.builder.seal_block(dummy);
            self.builder.switch_to_block(dummy);
            return Ok(());
        }
        match self.entry_shape {
            EntryShape::LegacyI64Args => {
                // In lambda mode (lambda_param_tys is set) the return
                // value's IR type isn't always I64 — the lambda's
                // `ret` could be I64 / Bool / String / etc. We just
                // pop one operand and return it; cranelift's verifier
                // catches type-width mismatches before finalize.
                let v = self.pop()?;
                self.builder.ins().return_(&[v]);
            }
            EntryShape::BufferProtocol => {
                // Mirrors the wasm-side epilogue: for bodies that
                // touched the tail-cursor (pointer-indirect stores /
                // dict construction) return the post-bump cursor;
                // otherwise return the precomputed `return_root_size`
                // so the host trampoline reads the full fixed area.
                let value = if self.needs_tail_cursor {
                    self.builder.ins().load(
                        I32,
                        MemFlags::trusted(),
                        self.state_ptr,
                        STATE_OFFSET_TAIL_CURSOR,
                    )
                } else {
                    self.builder
                        .ins()
                        .iconst(I32, i64::from(self.return_root_size))
                };
                self.builder.ins().return_(&[value]);
            }
        }
        // After the explicit return, the current block is filled.
        // Switch to a fresh dummy block so any subsequent ops the
        // body emits land somewhere valid; cranelift's DCE prunes
        // the now-dead dummy. Mirrors the post-Br / post-BrTable
        // dummy pattern.
        let dummy = self.builder.create_block();
        self.builder.seal_block(dummy);
        self.builder.switch_to_block(dummy);
        Ok(())
    }

    /// Lower `Op::Trap { kind }`. Unconditional branch to the trap
    /// block with the supplied kind code.
    pub(super) fn emit_trap(&mut self, kind: TrapKind) -> Result<(), CraneliftError> {
        let one = self.builder.ins().iconst(I32, 1);
        self.cond_trap(one, kind);
        Ok(())
    }

    /// Emit a zero placeholder of the given cranelift type. Used to
    /// keep dead `If` arms typed when the body branched out early
    /// (Br / Trap) and didn't leave a real value on the stack.
    pub(super) fn placeholder_for(&mut self, ty: cranelift_codegen::ir::Type) -> CValue {
        if ty == I64 {
            self.builder.ins().iconst(I64, 0)
        } else if ty == cranelift_codegen::ir::types::F64 {
            self.builder.ins().f64const(0.0)
        } else {
            self.builder.ins().iconst(I32, 0)
        }
    }
}
