//! `Op`-family: schema pointer / method dispatch.
//!
//! `LoadSchemaPtr` (and schema-method dispatch) are routed to the
//! Phase 0b seam below from `super::lower_op`. Phase 0b fills the body
//! here, porting from `relon-codegen-cranelift` and aligning three-way
//! (tree-walk / cranelift / llvm).

use relon_ir::ir::Op;

use crate::error::LlvmError;

use super::*;

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Phase 0b seam: schema pointer / method dispatch (`LoadSchemaPtr`).
    /// Dispatched from `super::lower_op`.
    pub(crate) fn lower_schema_rest(
        &mut self,
        ip: usize,
        _ip_hint: &str,
        op: &Op,
    ) -> Result<(), LlvmError> {
        Err(LlvmError::Codegen(format!(
            "unsupported op (Phase 0b schema seam): {op:?} at ip={ip}"
        )))
    }
}
