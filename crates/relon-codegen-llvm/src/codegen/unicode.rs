//! `Op`-family: Unicode table-address ops.
//!
//! The `*TableAddr` ops (`CaseFoldTableAddr`, `CombiningMarkRangesAddr`,
//! `WhitespaceRangesAddr`, `DecompTableAddr`, `CccTableAddr`,
//! `CompositionTableAddr`, `FullCaseFoldTableAddr`, `CasedRangesAddr`,
//! `CaseIgnorableRangesAddr`, `TurkishCaseFoldTableAddr`) are routed to
//! the lowering below from `super::lower_op`.
//!
//! # Lowering model (Wave R14)
//!
//! Each `*TableAddr` op resolves to an arena-relative `i32` offset into
//! the const-data prefix — the **exact same** mechanism cranelift uses.
//! [`super::ConstPool::collect_op`] walks every body (incl. inlined
//! bundled-stdlib helpers), encodes each referenced table once with the
//! byte-for-byte-identical `relon_ir::{case_folding, combining_marks,
//! whitespace, normalization, full_case_folding}` encoders (the same
//! functions cranelift's `ConstPool` calls), and lays the bytes into the
//! per-module const-data blob. The host copies that blob to the arena
//! prefix before every dispatch, so the recorded offset is the
//! arena-relative address the `__casefold_lookup` / `__is_combining_mark`
//! / ... helpers read through the same `Load*AtAbsolute` path every other
//! arena reference uses. The op lowering is therefore a pure
//! compile-time-constant offset push — no global, no runtime copy.
//!
//! ## Why this replaced the Phase-0b per-call scratch copy
//!
//! Phase 0b materialised each table by bump-allocating arena scratch and
//! `memcpy`ing a module global into it *at the op site*. Because these
//! ops live inside the case-fold / normalization helper bodies that are
//! inlined into the per-codepoint decode loop, that re-copied every table
//! (e.g. the 2404-byte combining-marks table) into fresh scratch on every
//! loop iteration; the scratch cursor overran the arena within a few
//! codepoints, so even `"hello".upper()` stored/loaded out of bounds —
//! SIGSEGV on native, OOB on wasm. Laying the bytes into the const prefix
//! once removes the per-iteration cost entirely and matches cranelift, so
//! the result is byte-identical four-way (see `tests/unicode_four_way.rs`).

use relon_ir::ir::{IrType, Op};

use crate::error::LlvmError;

use super::*;

/// Identifies which encoded Unicode table a `*TableAddr` op refers to.
/// Drives both the deterministic global symbol name (so repeated uses
/// in one module share a single global) and the byte encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum UnicodeTable {
    CaseFold { upper: bool },
    FullCaseFold { upper: bool },
    Turkish { upper: bool },
    CombiningMarks,
    Whitespace,
    Cased,
    CaseIgnorable,
    Decomp { compatibility: bool },
    Ccc,
    Composition,
}

impl UnicodeTable {
    /// Stable per-table global symbol. Used to dedup repeated `*TableAddr`
    /// references inside one module — the second reference reuses the
    /// global emitted by the first.
    fn global_symbol(self) -> &'static str {
        match self {
            UnicodeTable::CaseFold { upper: true } => "relon_uni_casefold_upper",
            UnicodeTable::CaseFold { upper: false } => "relon_uni_casefold_lower",
            UnicodeTable::FullCaseFold { upper: true } => "relon_uni_full_casefold_upper",
            UnicodeTable::FullCaseFold { upper: false } => "relon_uni_full_casefold_lower",
            UnicodeTable::Turkish { upper: true } => "relon_uni_turkish_upper",
            UnicodeTable::Turkish { upper: false } => "relon_uni_turkish_lower",
            UnicodeTable::CombiningMarks => "relon_uni_combining_marks",
            UnicodeTable::Whitespace => "relon_uni_whitespace",
            UnicodeTable::Cased => "relon_uni_cased_ranges",
            UnicodeTable::CaseIgnorable => "relon_uni_case_ignorable_ranges",
            UnicodeTable::Decomp {
                compatibility: true,
            } => "relon_uni_decomp_nfkd",
            UnicodeTable::Decomp {
                compatibility: false,
            } => "relon_uni_decomp_nfd",
            UnicodeTable::Ccc => "relon_uni_ccc",
            UnicodeTable::Composition => "relon_uni_composition",
        }
    }

    /// Map a `*TableAddr` op to the table it references, or `None` when
    /// `op` is not a Unicode table-address op. Shared by the const-pool
    /// collector (which lays the bytes once) and the lowering site
    /// (which pushes the resulting offset).
    pub(crate) fn from_op(op: &Op) -> Option<UnicodeTable> {
        Some(match op {
            Op::CaseFoldTableAddr { upper } => UnicodeTable::CaseFold { upper: *upper },
            Op::FullCaseFoldTableAddr { upper } => UnicodeTable::FullCaseFold { upper: *upper },
            Op::TurkishCaseFoldTableAddr { upper } => UnicodeTable::Turkish { upper: *upper },
            Op::CombiningMarkRangesAddr => UnicodeTable::CombiningMarks,
            Op::WhitespaceRangesAddr => UnicodeTable::Whitespace,
            Op::CasedRangesAddr => UnicodeTable::Cased,
            Op::CaseIgnorableRangesAddr => UnicodeTable::CaseIgnorable,
            Op::DecompTableAddr { compatibility } => UnicodeTable::Decomp {
                compatibility: *compatibility,
            },
            Op::CccTableAddr => UnicodeTable::Ccc,
            Op::CompositionTableAddr => UnicodeTable::Composition,
            _ => return None,
        })
    }

    /// Encode the table into the exact byte layout the lookup helpers
    /// expect. Each arm calls the same `relon_ir` encoder
    /// `relon-codegen-cranelift`'s `ConstPool` uses, so the bytes are
    /// identical across backends.
    pub(crate) fn encode_bytes(self) -> Vec<u8> {
        match self {
            UnicodeTable::CaseFold { upper } => {
                let table = if upper {
                    relon_ir::case_folding::simple_upper_folding()
                } else {
                    relon_ir::case_folding::simple_lower_folding()
                };
                relon_ir::case_folding::encode_table_bytes(table)
            }
            UnicodeTable::FullCaseFold { upper } => {
                let table = if upper {
                    relon_ir::full_case_folding::full_upper_folding()
                } else {
                    relon_ir::full_case_folding::full_lower_folding()
                };
                relon_ir::full_case_folding::encode_full_table_bytes(table)
            }
            UnicodeTable::Turkish { upper } => {
                let table = if upper {
                    relon_ir::full_case_folding::turkish_upper_folding()
                } else {
                    relon_ir::full_case_folding::turkish_lower_folding()
                };
                relon_ir::full_case_folding::encode_simple_view_bytes(table)
            }
            UnicodeTable::CombiningMarks => {
                let table = relon_ir::combining_marks::combining_mark_ranges();
                relon_ir::combining_marks::encode_ranges_bytes(table)
            }
            UnicodeTable::Whitespace => {
                let table = relon_ir::whitespace::non_ascii_whitespace_ranges();
                relon_ir::whitespace::encode_ranges_bytes(table)
            }
            UnicodeTable::Cased => {
                let table = relon_ir::full_case_folding::cased_ranges();
                relon_ir::full_case_folding::encode_ranges_bytes(table)
            }
            UnicodeTable::CaseIgnorable => {
                let table = relon_ir::full_case_folding::case_ignorable_ranges();
                relon_ir::full_case_folding::encode_ranges_bytes(table)
            }
            UnicodeTable::Decomp { compatibility } => {
                let (index, payload) = if compatibility {
                    (
                        relon_ir::normalization_data::NFKD_INDEX,
                        relon_ir::normalization_data::NFKD_POOL,
                    )
                } else {
                    (
                        relon_ir::normalization_data::NFD_INDEX,
                        relon_ir::normalization_data::NFD_POOL,
                    )
                };
                relon_ir::normalization::encode_decomp_table_bytes(index, payload)
            }
            UnicodeTable::Ccc => relon_ir::normalization::encode_ccc_table_bytes(
                relon_ir::normalization_data::CCC_TABLE,
            ),
            UnicodeTable::Composition => relon_ir::normalization::encode_composition_table_bytes(
                relon_ir::normalization_data::COMPOSITION_PAIRS,
            ),
        }
    }
}

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Wave R14: Unicode `*TableAddr` long tail. Dispatched from
    /// `super::lower_op`.
    ///
    /// Resolves the op to the const-data-prefix offset its encoded table
    /// bytes were laid into by [`super::ConstPool::collect_op`] and
    /// pushes that arena-relative `i32` offset — exactly the contract
    /// `Op::ConstString` uses, and byte-for-byte identical to cranelift's
    /// `ConstPool` (same `relon_ir` encoders, same const-data prefix the
    /// host copies into the arena before every dispatch).
    ///
    /// # Why not per-call scratch (the pre-R14 approach)
    ///
    /// The Phase-0b lowering materialised each table by bump-allocating
    /// arena scratch and `memcpy`ing a module global into it **at the op
    /// site**. Because these ops live inside the bundled case-fold /
    /// normalization helper bodies (`__casefold_lookup`,
    /// `__is_combining_mark`, ...) which are *inlined into the
    /// per-codepoint decode loop*, that copied every table (e.g. the
    /// 2404-byte combining-marks range table) into fresh scratch on
    /// **every loop iteration**. The scratch cursor ran off the end of
    /// the arena within a few codepoints, so even `"hello".upper()`
    /// stored/loaded out of bounds — SIGSEGV on native, OOB on wasm
    /// (the R8 failure mode). Laying the bytes into the const prefix once
    /// makes the op a pure compile-time-constant offset push with zero
    /// runtime cost, which is also what cranelift does.
    pub(crate) fn lower_unicode_rest(
        &mut self,
        ip: usize,
        _ip_hint: &str,
        op: &Op,
    ) -> Result<(), LlvmError> {
        let table = UnicodeTable::from_op(op).ok_or_else(|| {
            LlvmError::Codegen(format!(
                "lower_unicode_rest: non-unicode op routed here: {op:?} at ip={ip}"
            ))
        })?;
        let off = self
            .const_pool
            .unicode_table_offsets
            .get(&table)
            .copied()
            .ok_or_else(|| {
                LlvmError::Codegen(format!(
                    "Unicode table `{}` missing from const pool — \
                     `ConstPool::collect_op` must have laid it out",
                    table.global_symbol()
                ))
            })?;
        let c = self.ctx.i32_type().const_int(u64::from(off), false);
        self.push(c, IrType::I32);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Byte-alignment differential for the `*TableAddr` long tail.
    //!
    //! Wave R14 moved the Unicode tables off the per-call scratch copy
    //! and into the const-data prefix (the exact mechanism cranelift's
    //! `ConstPool` uses), so the alignment we pin is now twofold:
    //!
    //!   * `encode_bytes_match_shared_encoders` — each table's encoded
    //!     bytes are byte-for-byte identical to the shared `relon_ir`
    //!     encoder output cranelift's `ConstPool` consumes.
    //!   * `const_pool_lays_tables_at_aligned_offsets` /
    //!     `lower_pushes_const_pool_offset` — `ConstPool::collect_op`
    //!     lays each referenced table once at a 4-aligned offset holding
    //!     exactly those bytes, and `lower_unicode_rest` pushes that
    //!     offset as a single i32 (no scratch alloc, no per-call copy).
    //!
    //! End-to-end value differentials across all four legs live in
    //! `tests/unicode_four_way.rs`.

    use super::*;
    use crate::codegen::{ConstPool, EntryShape};
    use inkwell::context::Context;
    use inkwell::AddressSpace;
    use relon_ir::ir::IrType;

    /// Build the full `(op, global symbol, expected encoder bytes)`
    /// matrix once — the expected bytes come from the *same* `relon_ir`
    /// encoders `relon-codegen-cranelift`'s `ConstPool` calls, so this
    /// is the cranelift gold-standard table data restated.
    fn table_cases() -> Vec<(Op, &'static str, Vec<u8>)> {
        vec![
            (
                Op::CaseFoldTableAddr { upper: true },
                "relon_uni_casefold_upper",
                relon_ir::case_folding::encode_table_bytes(
                    relon_ir::case_folding::simple_upper_folding(),
                ),
            ),
            (
                Op::CaseFoldTableAddr { upper: false },
                "relon_uni_casefold_lower",
                relon_ir::case_folding::encode_table_bytes(
                    relon_ir::case_folding::simple_lower_folding(),
                ),
            ),
            (
                Op::FullCaseFoldTableAddr { upper: true },
                "relon_uni_full_casefold_upper",
                relon_ir::full_case_folding::encode_full_table_bytes(
                    relon_ir::full_case_folding::full_upper_folding(),
                ),
            ),
            (
                Op::FullCaseFoldTableAddr { upper: false },
                "relon_uni_full_casefold_lower",
                relon_ir::full_case_folding::encode_full_table_bytes(
                    relon_ir::full_case_folding::full_lower_folding(),
                ),
            ),
            (
                Op::TurkishCaseFoldTableAddr { upper: true },
                "relon_uni_turkish_upper",
                relon_ir::full_case_folding::encode_simple_view_bytes(
                    relon_ir::full_case_folding::turkish_upper_folding(),
                ),
            ),
            (
                Op::TurkishCaseFoldTableAddr { upper: false },
                "relon_uni_turkish_lower",
                relon_ir::full_case_folding::encode_simple_view_bytes(
                    relon_ir::full_case_folding::turkish_lower_folding(),
                ),
            ),
            (
                Op::CombiningMarkRangesAddr,
                "relon_uni_combining_marks",
                relon_ir::combining_marks::encode_ranges_bytes(
                    relon_ir::combining_marks::combining_mark_ranges(),
                ),
            ),
            (
                Op::WhitespaceRangesAddr,
                "relon_uni_whitespace",
                relon_ir::whitespace::encode_ranges_bytes(
                    relon_ir::whitespace::non_ascii_whitespace_ranges(),
                ),
            ),
            (
                Op::CasedRangesAddr,
                "relon_uni_cased_ranges",
                relon_ir::full_case_folding::encode_ranges_bytes(
                    relon_ir::full_case_folding::cased_ranges(),
                ),
            ),
            (
                Op::CaseIgnorableRangesAddr,
                "relon_uni_case_ignorable_ranges",
                relon_ir::full_case_folding::encode_ranges_bytes(
                    relon_ir::full_case_folding::case_ignorable_ranges(),
                ),
            ),
            (
                Op::DecompTableAddr {
                    compatibility: false,
                },
                "relon_uni_decomp_nfd",
                relon_ir::normalization::encode_decomp_table_bytes(
                    relon_ir::normalization_data::NFD_INDEX,
                    relon_ir::normalization_data::NFD_POOL,
                ),
            ),
            (
                Op::DecompTableAddr {
                    compatibility: true,
                },
                "relon_uni_decomp_nfkd",
                relon_ir::normalization::encode_decomp_table_bytes(
                    relon_ir::normalization_data::NFKD_INDEX,
                    relon_ir::normalization_data::NFKD_POOL,
                ),
            ),
            (
                Op::CccTableAddr,
                "relon_uni_ccc",
                relon_ir::normalization::encode_ccc_table_bytes(
                    relon_ir::normalization_data::CCC_TABLE,
                ),
            ),
            (
                Op::CompositionTableAddr,
                "relon_uni_composition",
                relon_ir::normalization::encode_composition_table_bytes(
                    relon_ir::normalization_data::COMPOSITION_PAIRS,
                ),
            ),
        ]
    }

    /// Gold-standard byte-identity: the `Vec<u8>` each `*TableAddr` arm
    /// feeds into `set_initializer` (via `UnicodeTable::encode_bytes`)
    /// must be byte-for-byte identical to the shared `relon_ir` encoder
    /// output cranelift's `ConstPool` consumes — so a downstream lookup
    /// helper reads the same table on both backends.
    #[test]
    fn encode_bytes_match_shared_encoders() {
        for (op, sym, expected) in table_cases() {
            let table = match &op {
                Op::CaseFoldTableAddr { upper } => UnicodeTable::CaseFold { upper: *upper },
                Op::FullCaseFoldTableAddr { upper } => UnicodeTable::FullCaseFold { upper: *upper },
                Op::TurkishCaseFoldTableAddr { upper } => UnicodeTable::Turkish { upper: *upper },
                Op::CombiningMarkRangesAddr => UnicodeTable::CombiningMarks,
                Op::WhitespaceRangesAddr => UnicodeTable::Whitespace,
                Op::CasedRangesAddr => UnicodeTable::Cased,
                Op::CaseIgnorableRangesAddr => UnicodeTable::CaseIgnorable,
                Op::DecompTableAddr { compatibility } => UnicodeTable::Decomp {
                    compatibility: *compatibility,
                },
                Op::CccTableAddr => UnicodeTable::Ccc,
                Op::CompositionTableAddr => UnicodeTable::Composition,
                other => panic!("unexpected op in matrix: {other:?}"),
            };
            assert_eq!(table.global_symbol(), sym);
            let got = table.encode_bytes();
            assert_eq!(
                got, expected,
                "{op:?}: encode_bytes diverges from the shared `relon_ir` \
                 encoder that cranelift's ConstPool consumes"
            );
            assert!(
                got.len() >= 4,
                "{op:?}: table must carry at least its u32 count header"
            );
        }
    }

    /// Lay every `*TableAddr` op into a fresh `ConstPool` via
    /// `collect_op` and check that each table lands at a 4-aligned
    /// offset holding exactly its shared-encoder bytes. This is the
    /// data half of the cranelift contract: the host copies
    /// `ConstPool::bytes` to the arena prefix, so the recorded offset is
    /// the arena-relative address a lookup helper reads from.
    #[test]
    fn const_pool_lays_tables_at_aligned_offsets() {
        let mut pool = ConstPool::default();
        for (op, _sym, _expected) in table_cases() {
            pool.collect_op(&op).expect("collect_op");
        }
        for (op, _sym, expected) in table_cases() {
            let table = UnicodeTable::from_op(&op).expect("from_op");
            let off = *pool
                .unicode_table_offsets
                .get(&table)
                .unwrap_or_else(|| panic!("{op:?}: missing const-pool offset"));
            assert_eq!(off % 4, 0, "{op:?}: table offset must be 4-aligned");
            let slice = &pool.bytes[off as usize..off as usize + expected.len()];
            assert_eq!(
                slice,
                expected.as_slice(),
                "{op:?}: const-pool bytes diverge from the shared encoder cranelift consumes"
            );
        }
    }

    /// `lower_unicode_rest` pushes exactly one i32 equal to the
    /// const-pool offset for the table — no scratch alloc, no per-call
    /// memcpy, no backing global. Repeated references resolve to the
    /// SAME offset (dedup happens in `collect_op`).
    #[test]
    fn lower_pushes_const_pool_offset() {
        let mut pool = ConstPool::default();
        let op = Op::CccTableAddr;
        pool.collect_op(&op).expect("collect");
        let want_off = *pool
            .unicode_table_offsets
            .get(&UnicodeTable::Ccc)
            .expect("ccc offset");

        let ctx = Context::create();
        let module = ctx.create_module("offset_test");
        let ptr_t = ctx.ptr_type(AddressSpace::default());
        let void_t = ctx.void_type();
        let fn_ty = void_t.fn_type(&[ptr_t.into()], false);
        let func = module.add_function("t", fn_ty, None);
        let entry_bb = ctx.append_basic_block(func, "entry");
        let builder = ctx.create_builder();
        builder.position_at_end(entry_bb);
        let state_ptr = func.get_nth_param(0).unwrap().into_pointer_value();

        let mut emit = Emit::new(
            &ctx,
            &builder,
            &module,
            func,
            EntryShape::Buffer,
            None,
            Some(state_ptr),
            0,
            &pool,
        );
        emit.lower_unicode_rest(0, "a", &op).unwrap();
        emit.lower_unicode_rest(1, "b", &op).unwrap();
        // Two ops → two identical offset constants on the stack, both i32.
        assert_eq!(emit.stack.len(), 2);
        for tv in &emit.stack {
            assert_eq!(tv.ty, IrType::I32, "table addr result must be i32");
            let c = tv.val;
            assert!(c.is_const(), "table addr must be a compile-time constant");
            assert_eq!(
                c.get_zero_extended_constant(),
                Some(u64::from(want_off)),
                "lowered offset must equal the const-pool offset"
            );
        }
        // No table global is emitted any more (the bytes live in the
        // const-data prefix, not a module global).
        assert!(
            module.get_global("relon_uni_ccc").is_none(),
            "R14 must not emit a per-table module global"
        );
    }
}
