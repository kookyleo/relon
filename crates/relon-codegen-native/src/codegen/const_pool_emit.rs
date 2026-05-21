//! Cranelift-side emit helpers that translate const-pool offsets into
//! pushed `iconst(I32, ..)` operand-stack values.
//!
//! The companion [`super::const_pool::ConstPool`] owns the bytes and
//! the `idx -> offset` (or `slot -> offset`) maps. This file owns the
//! lowering arm: each helper resolves one logical const-pool record
//! and synthesises the matching cranelift instruction.
//!
//! Splitting layout (`const_pool.rs`) from emit (this file) keeps the
//! [`super::Codegen::emit_op`] match one-line-per-variant — every
//! `Op::*Addr` / `Op::Const*` arm now reads as a thin delegate which
//! drops the previous 6-9 lines of `ok_or_else` / `iconst` / `push`
//! boilerplate. The original byte-layout invariants stay in
//! `const_pool.rs` where the IR walk lives.

use cranelift_codegen::ir::types::I32;
use cranelift_codegen::ir::{InstBuilder, Value as CValue};

use crate::error::CraneliftError;

impl<'a, 'b> super::Codegen<'a, 'b> {
    /// Resolve a const-pool table offset and push it as an `i32`
    /// operand-stack value. The `offset` is the pre-computed
    /// `Option<u32>` slot on [`super::const_pool::ConstPool`]; the
    /// `label` names the originating IR op for diagnostics.
    ///
    /// Used by every `Op::*TableAddr` arm — `CaseFoldTableAddr`,
    /// `CombiningMarkRangesAddr`, `WhitespaceRangesAddr`,
    /// `DecompTableAddr`, `CccTableAddr`, `CompositionTableAddr`,
    /// `FullCaseFoldTableAddr`, `CasedRangesAddr`,
    /// `CaseIgnorableRangesAddr`, `TurkishCaseFoldTableAddr`.
    pub(super) fn emit_const_pool_address(
        &mut self,
        offset: Option<u32>,
        label: &str,
    ) -> Result<CValue, CraneliftError> {
        let off = offset.ok_or_else(|| {
            CraneliftError::Codegen(format!("{label} missing from const pool"))
        })?;
        let v = self.builder.ins().iconst(I32, i64::from(off));
        self.push(v);
        Ok(v)
    }
}
