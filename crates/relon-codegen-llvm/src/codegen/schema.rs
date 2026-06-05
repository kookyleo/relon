//! `Op`-family: schema pointer / method dispatch.
//!
//! `LoadSchemaPtr` (and schema-method dispatch) are routed to the
//! Phase 0b seam below from `super::lower_op`. Phase 0b fills the body
//! here, aligning three-way (tree-walk / cranelift / llvm).

use relon_ir::ir::Op;

use crate::error::LlvmError;

use super::*;

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Phase 0b seam: schema pointer / method dispatch (`LoadSchemaPtr`).
    /// Dispatched from `super::lower_op`.
    pub(crate) fn lower_schema_rest(
        &mut self,
        ip: usize,
        ip_hint: &str,
        op: &Op,
    ) -> Result<(), LlvmError> {
        match op {
            Op::LoadSchemaPtr { offset } => self.emit_load_schema_ptr(ip_hint, *offset),
            other => Err(LlvmError::Codegen(format!(
                "unsupported op (Phase 0b schema seam): {other:?} at ip={ip}"
            ))),
        }
    }

    /// Lower `Op::LoadSchemaPtr { offset }`.
    ///
    /// A schema-typed `#main` parameter arrives in the input buffer as
    /// a 4-byte buffer-relative offset stored at `in_ptr + offset`.
    /// This op lifts that slot to the **absolute** (arena-relative)
    /// address of the schema instance's fixed area: it reads the 4-byte
    /// slot, then adds `in_ptr` so downstream `LoadFieldAtAbsolute`
    /// (which composes `arena_base + base + field_offset`) lands on the
    /// matching field.
    ///
    /// The pushed value is the absolute i32 address tagged `IrType::I32`
    /// — the IR-level schema brand is tracked by the lowering pass, not
    /// by an operand-stack tag. Mirrors the IR doc on
    /// [`relon_ir::ir::Op::LoadSchemaPtr`]: `local.get $in_ptr;
    /// i32.load offset=N; <add in_ptr>`.
    ///
    /// No bounds check (Phase B/C/D LLVM emitter trusts the IR + relies
    /// on the host trap for UB, matching the other `Load*Ptr` paths).
    fn emit_load_schema_ptr(&mut self, _ip_hint: &str, offset: u32) -> Result<(), LlvmError> {
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("Op::LoadSchemaPtr outside buffer-protocol entry shape".into())
        })?;
        // IR LocalGet(0) == in_ptr (the buffer-protocol entry's input
        // record base).
        let in_ptr_i32 = self.lookup_param(0)?;
        // Read the 4-byte slot at `in_ptr + offset`. F1: the input
        // marshaller baked `in_ptr` into the slot (`finish_arena_absolute`),
        // so the loaded value is the schema instance's arena-relative base
        // directly — no `+ in_ptr` rebase. LoadFieldAtAbsolute adds
        // `arena_base` itself.
        let slot_addr = self.compute_buffer_addr(arena_base_ptr, in_ptr_i32, offset)?;
        let name = self.next_name("schemaptr_abs");
        let abs = self
            .builder
            .build_load(self.ctx.i32_type(), slot_addr, &name)
            .map_err(|e| LlvmError::Codegen(format!("LoadSchemaPtr slot load: {e}")))?
            .into_int_value();
        self.push(abs, IrType::I32);
        Ok(())
    }
}
