//! Compile [`relon_ir::Func`] into a flat [`crate::BcFunction`].
//!
//! Two-pass walk: pass 1 emits ops with **placeholder** branch
//! targets (`usize::MAX`) and records each pending fixup; pass 2
//! patches the targets once every IR-level `block` / `loop` has a
//! resolved bytecode index. The pattern mirrors the wasm verifier's
//! `pop_label` discipline so a hand-built IR module that wouldn't
//! verify on wasm bounces out here as `BcCompileError::BranchTarget`.
//!
//! ## Buffer-protocol lowering
//!
//! `lower_workspace_single` emits the buffer-protocol entry shape:
//! `params = [I32 in_ptr, I32 in_len, I32 out_ptr, I32 out_cap,
//! I64 caps]` and the body uses `LoadField { offset }` /
//! `StoreField { offset }` against the in/out buffers. The bytecode
//! VM doesn't talk arenas; instead, [`compile_buffer_protocol`]
//! synthesises a virtual-local layout where each `#main` parameter
//! occupies one slot and every `LoadField` / `StoreField` is
//! rewritten to the matching `LocalGet` / `LocalSet`. The arg
//! packing (in [`crate::evaluator::BytecodeEvaluator`]) writes args
//! into those virtual slots directly.

use std::collections::BTreeMap;

use thiserror::Error;

use relon_eval_api::layout::OffsetTable;
use relon_ir::{Func, IrType, Op, TaggedOp};

use crate::op::{BcFunction, BcOp, BcTrapKind, ExternalPc};

/// Compile errors. Each variant pins one structural mismatch so the
/// caller can decide whether to fall back to the cranelift / tree-
/// walker path or to log + bail.
#[derive(Debug, Clone, Error)]
pub enum BcCompileError {
    /// The IR contains an op the bytecode VM has no lowering for.
    /// Today the legacy-i64 entry shape covers arith + control flow
    /// + locals + return; everything else (Call / CallNative / record
    ///   construction / list ops / stdlib indirection) surfaces here.
    #[error("unsupported IR op for bytecode VM: {0}")]
    UnsupportedOp(String),
    /// A branch target depth exceeded the resolved label stack.
    /// Symptom of either a malformed IR module or a compiler bug.
    #[error("branch target depth {depth} out of range (label stack height {stack})")]
    BranchTarget {
        /// Label depth requested by the branch op.
        depth: u32,
        /// Active label-stack height at the branch site.
        stack: u32,
    },
    /// `If` arm produced no ops. The IR shouldn't ever emit an empty
    /// arm, so flag it loudly rather than silently dropping the
    /// fixup.
    #[error("empty If arm at IR PC {pc}")]
    EmptyArm {
        /// IR-level PC of the offending `If` op.
        pc: u64,
    },
    /// A `LoadField` / `StoreField` op references an offset that
    /// doesn't appear in the schema's offset table. Indicates the
    /// IR doesn't match the schema the caller passed in — symptom
    /// of cross-module lowering drift.
    #[error("LoadField/StoreField offset {offset} has no matching schema field")]
    UnknownFieldOffset {
        /// The unresolved offset value.
        offset: u32,
    },
}

/// Compile one IR function to bytecode. The op stream and the
/// `ir_pc_map` come back paired so the partial-resume routing in
/// M2-B can walk both in lockstep.
///
/// `field_offset_to_local` maps each `LoadField` / `StoreField`
/// offset to the local slot the bytecode VM should read / write.
/// `BytecodeEvaluator::from_source` builds this from the main +
/// return schemas; legacy-i64 direct-IR callers can pass an empty
/// map (their IR uses `LocalGet(idx)` directly).
pub fn compile_function(
    func: &Func,
    field_offset_to_local: &BTreeMap<u32, u32>,
    return_field_offset_to_local: &BTreeMap<u32, u32>,
) -> Result<BcFunction, BcCompileError> {
    let mut state = CompileState {
        ops: Vec::new(),
        ir_pc_map: Vec::new(),
        ir_pc_next: 0,
        labels: Vec::new(),
        field_offset_to_local,
        return_field_offset_to_local,
    };
    state.compile_seq(&func.body, /*depth_base=*/ 0)?;
    state.emit_ret_if_missing();

    // Locals count: derive from the maximum LocalGet/LocalSet index
    // observed plus the buffer-protocol epilogue slot for the return
    // value placeholder (so StoreField → LocalSet can land cleanly).
    let max_local = state
        .ops
        .iter()
        .filter_map(|op| match op {
            BcOp::LocalGet(i) | BcOp::LocalSet(i) => Some(*i),
            _ => None,
        })
        .max();
    let locals_used = max_local.map(|m| m + 1).unwrap_or(0);
    let locals = (func.params.len() as u32).max(locals_used);

    Ok(BcFunction {
        ops: state.ops,
        locals,
        ir_pc_map: state.ir_pc_map,
    })
}

/// Build a `field_offset → local_idx` map from a schema offset
/// table. Each declared field gets the next sequential slot index;
/// the order is whatever `BTreeMap` iteration produces (offset-asc),
/// which matches the schema declaration order for v1 inline layouts.
pub fn build_offset_to_local(layout: &OffsetTable) -> BTreeMap<u32, u32> {
    let mut by_offset: BTreeMap<u32, u32> = BTreeMap::new();
    for (i, fo) in layout.fields.iter().enumerate() {
        by_offset.insert(fo.offset as u32, i as u32);
    }
    by_offset
}

struct CompileState<'a> {
    ops: Vec<BcOp>,
    ir_pc_map: Vec<ExternalPc>,
    ir_pc_next: ExternalPc,
    labels: Vec<LabelFrame>,
    /// Map LoadField offsets to virtual local slots; empty for
    /// legacy-i64 sources.
    field_offset_to_local: &'a BTreeMap<u32, u32>,
    /// Map StoreField offsets — same scheme as `field_offset_to_local`
    /// but indexed off the **return** schema. We dump StoreField into
    /// a per-return-field slot then the evaluator unpacks it back
    /// into a `Value` after the run.
    return_field_offset_to_local: &'a BTreeMap<u32, u32>,
}

#[derive(Debug)]
struct LabelFrame {
    kind: LabelKind,
    target: Option<usize>,
    pending_patches: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LabelKind {
    Block,
    Loop,
}

impl<'a> CompileState<'a> {
    fn next_pc(&mut self) -> ExternalPc {
        self.ir_pc_next += 1;
        self.ir_pc_next
    }

    fn emit(&mut self, op: BcOp, pc: ExternalPc) {
        self.ops.push(op);
        self.ir_pc_map.push(pc);
    }

    fn current_idx(&self) -> usize {
        self.ops.len()
    }

    fn compile_seq(&mut self, ops: &[TaggedOp], _depth_base: u32) -> Result<(), BcCompileError> {
        for tagged in ops {
            self.compile_one(&tagged.op)?;
        }
        Ok(())
    }

    fn compile_one(&mut self, op: &Op) -> Result<(), BcCompileError> {
        let pc = self.next_pc();
        match op {
            Op::ConstI64(v) => self.emit(BcOp::ConstI64(*v), pc),
            Op::ConstI32(v) => self.emit(BcOp::ConstI32(*v), pc),
            Op::ConstBool(b) => self.emit(BcOp::ConstI32(if *b { 1 } else { 0 }), pc),
            Op::LocalGet(idx) => self.emit(BcOp::LocalGet(*idx), pc),
            Op::LetGet { idx, .. } => {
                // Let-locals sit past the buffer-protocol arg slots.
                // The buffer-protocol layout reserves the first
                // `main_schema.fields.len()` virtual slots for inputs
                // and the return-schema field slots after that; let
                // locals come **on top of** those reserved slots so
                // they don't collide. The offset is fixed by the
                // compile-time field maps.
                let base = self.input_arg_count() + self.return_field_count();
                self.emit(BcOp::LocalGet(base + *idx), pc);
            }
            Op::LetSet { idx, .. } => {
                let base = self.input_arg_count() + self.return_field_count();
                self.emit(BcOp::LocalSet(base + *idx), pc);
            }
            Op::Add(ty) => self.emit(BcOp::Add(*ty), pc),
            Op::Sub(ty) => self.emit(BcOp::Sub(*ty), pc),
            Op::Mul(ty) => self.emit(BcOp::Mul(*ty), pc),
            Op::Div(ty) => self.emit(BcOp::Div(*ty), pc),
            Op::Mod(ty) => self.emit(BcOp::Mod(*ty), pc),
            Op::Eq(ty) => self.emit(BcOp::Eq(*ty), pc),
            Op::Ne(ty) => self.emit(BcOp::Ne(*ty), pc),
            Op::Lt(ty) => self.emit(BcOp::Lt(*ty), pc),
            Op::Le(ty) => self.emit(BcOp::Le(*ty), pc),
            Op::Gt(ty) => self.emit(BcOp::Gt(*ty), pc),
            Op::Ge(ty) => self.emit(BcOp::Ge(*ty), pc),
            Op::Return => self.emit(BcOp::Return, pc),
            Op::Trap { kind, .. } => {
                let bc_kind = match kind {
                    relon_ir::ir::TrapKind::IndexOutOfBounds => BcTrapKind::IndexOutOfBounds,
                    relon_ir::ir::TrapKind::EmptyList => BcTrapKind::EmptyList,
                    relon_ir::ir::TrapKind::InvalidUtf8 => BcTrapKind::InvalidUtf8,
                };
                self.emit(BcOp::Trap(bc_kind), pc);
            }
            Op::LoadField { offset, ty: _ } => {
                let slot = self
                    .field_offset_to_local
                    .get(offset)
                    .copied()
                    .ok_or(BcCompileError::UnknownFieldOffset { offset: *offset })?;
                self.emit(BcOp::LocalGet(slot), pc);
            }
            Op::StoreField { offset, ty: _ } => {
                // Map onto a return-field virtual slot, positioned
                // **after** the input arg slots so the evaluator can
                // read them back as `locals[input_args + i]`.
                let return_slot = self
                    .return_field_offset_to_local
                    .get(offset)
                    .copied()
                    .ok_or(BcCompileError::UnknownFieldOffset { offset: *offset })?;
                let slot = self.input_arg_count() + return_slot;
                self.emit(BcOp::LocalSet(slot), pc);
            }
            Op::If {
                result_ty,
                then_body,
                else_body,
            } => self.compile_if(*result_ty, then_body, else_body, pc)?,
            Op::Block { body, .. } => self.compile_block(body)?,
            Op::Loop { body, .. } => self.compile_loop(body)?,
            Op::Br { label_depth } => self.compile_br(*label_depth, /*conditional=*/ false)?,
            Op::BrIf { label_depth } => {
                self.compile_br(*label_depth, /*conditional=*/ true)?
            }
            // Everything else — Call / CallNative / record / stdlib /
            // memory ops / closures / native — is outside the M2-A
            // envelope. Surface as a compile error so the caller can
            // route through cranelift / tree-walker.
            other => {
                return Err(BcCompileError::UnsupportedOp(format!("{other:?}")));
            }
        }
        Ok(())
    }

    fn input_arg_count(&self) -> u32 {
        self.field_offset_to_local.len() as u32
    }

    fn return_field_count(&self) -> u32 {
        self.return_field_offset_to_local.len() as u32
    }

    fn compile_if(
        &mut self,
        result_ty: IrType,
        then_body: &[TaggedOp],
        else_body: &[TaggedOp],
        if_pc: ExternalPc,
    ) -> Result<(), BcCompileError> {
        let _ = result_ty;
        if then_body.is_empty() && else_body.is_empty() {
            return Err(BcCompileError::EmptyArm { pc: if_pc });
        }
        let if_idx = self.current_idx();
        self.emit(BcOp::JumpIfFalse(usize::MAX), if_pc);

        self.compile_seq(then_body, /*depth_base=*/ 0)?;
        let after_then = self.current_idx();
        let then_jump_pc = self.next_pc();
        self.emit(BcOp::Jump(usize::MAX), then_jump_pc);

        let else_start = self.current_idx();
        self.compile_seq(else_body, /*depth_base=*/ 0)?;
        let join = self.current_idx();

        match &mut self.ops[if_idx] {
            BcOp::JumpIfFalse(t) => *t = else_start,
            _ => unreachable!("emitted JumpIfFalse above"),
        }
        match &mut self.ops[after_then] {
            BcOp::Jump(t) => *t = join,
            _ => unreachable!("emitted Jump above"),
        }
        Ok(())
    }

    fn compile_block(&mut self, body: &[TaggedOp]) -> Result<(), BcCompileError> {
        self.labels.push(LabelFrame {
            kind: LabelKind::Block,
            target: None,
            pending_patches: Vec::new(),
        });
        self.compile_seq(body, /*depth_base=*/ 0)?;
        let frame = self.labels.pop().expect("balanced push/pop");
        let target = self.current_idx();
        for patch in frame.pending_patches {
            match &mut self.ops[patch] {
                BcOp::Jump(t) | BcOp::JumpIfTrue(t) | BcOp::JumpIfFalse(t) => *t = target,
                _ => unreachable!("Block fixup pointed at a non-jump op"),
            }
        }
        let _ = frame.kind;
        let _ = frame.target;
        Ok(())
    }

    fn compile_loop(&mut self, body: &[TaggedOp]) -> Result<(), BcCompileError> {
        let header = self.current_idx();
        self.labels.push(LabelFrame {
            kind: LabelKind::Loop,
            target: Some(header),
            pending_patches: Vec::new(),
        });
        self.compile_seq(body, /*depth_base=*/ 0)?;
        let frame = self.labels.pop().expect("balanced push/pop");
        for patch in frame.pending_patches {
            match &mut self.ops[patch] {
                BcOp::Jump(t) | BcOp::JumpIfTrue(t) | BcOp::JumpIfFalse(t) => *t = header,
                _ => unreachable!("Loop fixup pointed at a non-jump op"),
            }
        }
        Ok(())
    }

    fn compile_br(&mut self, depth: u32, conditional: bool) -> Result<(), BcCompileError> {
        let stack_height = self.labels.len() as u32;
        if depth >= stack_height {
            return Err(BcCompileError::BranchTarget {
                depth,
                stack: stack_height,
            });
        }
        let frame_idx = (stack_height - 1 - depth) as usize;
        let placeholder_pc = self.next_pc();
        let bc_idx = self.current_idx();
        let placeholder = if conditional {
            BcOp::JumpIfTrue(usize::MAX)
        } else {
            BcOp::Jump(usize::MAX)
        };
        self.emit(placeholder, placeholder_pc);
        let frame = &mut self.labels[frame_idx];
        match frame.kind {
            LabelKind::Loop => {
                let header = frame
                    .target
                    .expect("loop target seeded at compile_loop entry");
                match &mut self.ops[bc_idx] {
                    BcOp::Jump(t) | BcOp::JumpIfTrue(t) => *t = header,
                    _ => unreachable!("emitted Jump/JumpIfTrue above"),
                }
            }
            LabelKind::Block => {
                frame.pending_patches.push(bc_idx);
            }
        }
        Ok(())
    }

    fn emit_ret_if_missing(&mut self) {
        if !matches!(self.ops.last(), Some(BcOp::Return)) {
            let pc = self.next_pc();
            self.emit(BcOp::Return, pc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_ir::IrType;
    use relon_parser::TokenRange;

    fn tagged(op: Op) -> TaggedOp {
        TaggedOp {
            op,
            range: TokenRange::default(),
        }
    }

    #[test]
    fn compiles_simple_add() {
        let func = Func {
            name: "f".into(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body: vec![
                tagged(Op::LocalGet(0)),
                tagged(Op::LocalGet(1)),
                tagged(Op::Add(IrType::I64)),
                tagged(Op::Return),
            ],
            range: TokenRange::default(),
        };
        let empty = BTreeMap::new();
        let bc = compile_function(&func, &empty, &empty).unwrap();
        assert_eq!(bc.locals, 2);
        assert_eq!(bc.ops.len(), 4);
        assert_eq!(bc.ir_pc_map.len(), 4);
        for w in bc.ir_pc_map.windows(2) {
            assert!(w[1] > w[0]);
        }
    }

    #[test]
    fn compiles_if_expression() {
        let func = Func {
            name: "f".into(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body: vec![
                tagged(Op::LocalGet(0)),
                tagged(Op::LocalGet(1)),
                tagged(Op::Gt(IrType::I64)),
                tagged(Op::If {
                    result_ty: IrType::I64,
                    then_body: vec![tagged(Op::LocalGet(0))],
                    else_body: vec![tagged(Op::LocalGet(1))],
                }),
                tagged(Op::Return),
            ],
            range: TokenRange::default(),
        };
        let empty = BTreeMap::new();
        let bc = compile_function(&func, &empty, &empty).unwrap();
        for op in &bc.ops {
            match op {
                BcOp::Jump(t) | BcOp::JumpIfTrue(t) | BcOp::JumpIfFalse(t) => {
                    assert!(*t <= bc.ops.len(), "branch target {t} > op count");
                    assert_ne!(*t, usize::MAX, "branch target left as placeholder");
                }
                _ => {}
            }
        }
    }

    #[test]
    fn unsupported_op_surfaces_compile_error() {
        let func = Func {
            name: "f".into(),
            params: vec![IrType::I64],
            ret: IrType::I64,
            body: vec![tagged(Op::Call {
                fn_index: 0,
                arg_count: 0,
                param_tys: vec![],
                ret_ty: IrType::I64,
            })],
            range: TokenRange::default(),
        };
        let empty = BTreeMap::new();
        let err = compile_function(&func, &empty, &empty).unwrap_err();
        assert!(matches!(err, BcCompileError::UnsupportedOp(_)));
    }
}
