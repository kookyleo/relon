//! `Op`-family: Unicode table-address ops.
//!
//! The `*TableAddr` ops (`CaseFoldTableAddr`, `CombiningMarkRangesAddr`,
//! `WhitespaceRangesAddr`, `DecompTableAddr`, `CccTableAddr`,
//! `CompositionTableAddr`, `FullCaseFoldTableAddr`, `CasedRangesAddr`,
//! `CaseIgnorableRangesAddr`, `TurkishCaseFoldTableAddr`) are routed to
//! the Phase 0b seam below from `super::lower_op`. Phase 0b fills the
//! body here, porting from `relon-codegen-cranelift` and aligning
//! three-way (tree-walk / cranelift / llvm).

use relon_ir::ir::Op;

use crate::error::LlvmError;

use super::*;

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Phase 0b seam: Unicode `*TableAddr` long tail. Dispatched from
    /// `super::lower_op`.
    pub(crate) fn lower_unicode_rest(
        &mut self,
        ip: usize,
        _ip_hint: &str,
        op: &Op,
    ) -> Result<(), LlvmError> {
        Err(LlvmError::Codegen(format!(
            "unsupported op (Phase 0b unicode seam): {op:?} at ip={ip}"
        )))
    }
}
