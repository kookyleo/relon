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

/// Type-tag for [`super::Codegen::emit_const_value`]. Selects which of
/// the per-record offset maps on [`super::const_pool::ConstPool`] the
/// lookup hits.
#[derive(Debug, Clone, Copy)]
pub(super) enum ConstValueKind {
    String,
    ListInt,
    ListFloat,
    ListBool,
    /// W5-P1: arena `{String -> Int}` dict record.
    Dict,
}

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
        let off = offset
            .ok_or_else(|| CraneliftError::Codegen(format!("{label} missing from const pool")))?;
        let v = self.builder.ins().iconst(I32, i64::from(off));
        self.push(v);
        Ok(v)
    }

    /// Resolve a `ConstString` / `ConstList*` record offset by `idx`
    /// and push it as an `i32` operand-stack value. `kind` selects
    /// which per-record offset map drives the lookup and also names
    /// the originating IR op for the diagnostic on a missing entry.
    pub(super) fn emit_const_value(
        &mut self,
        idx: u32,
        kind: ConstValueKind,
    ) -> Result<CValue, CraneliftError> {
        let (offset, label) = match kind {
            ConstValueKind::String => (
                self.const_pool.string_offsets.get(&idx).copied(),
                "ConstString",
            ),
            ConstValueKind::ListInt => (
                self.const_pool.list_int_offsets.get(&idx).copied(),
                "ConstListInt",
            ),
            ConstValueKind::ListFloat => (
                self.const_pool.list_float_offsets.get(&idx).copied(),
                "ConstListFloat",
            ),
            ConstValueKind::ListBool => (
                self.const_pool.list_bool_offsets.get(&idx).copied(),
                "ConstListBool",
            ),
            ConstValueKind::Dict => {
                (self.const_pool.dict_offsets.get(&idx).copied(), "ConstDict")
            }
        };
        let off = offset.ok_or_else(|| {
            CraneliftError::Codegen(format!("{label} idx {idx} not in pre-computed pool"))
        })?;
        let v = self.builder.ins().iconst(I32, i64::from(off));
        self.push(v);
        Ok(v)
    }
}
