//! `Op`-family: Unicode table-address ops.
//!
//! The `*TableAddr` ops (`CaseFoldTableAddr`, `CombiningMarkRangesAddr`,
//! `WhitespaceRangesAddr`, `DecompTableAddr`, `CccTableAddr`,
//! `CompositionTableAddr`, `FullCaseFoldTableAddr`, `CasedRangesAddr`,
//! `CaseIgnorableRangesAddr`, `TurkishCaseFoldTableAddr`) are routed to
//! the Phase 0b seam below from `super::lower_op`.
//!
//! # Lowering model
//!
//! On the cranelift side each `*TableAddr` op resolves to an
//! arena-relative `i32` offset: `ConstPool` lays the encoded table
//! bytes into the per-module const-data blob (which the host copies to
//! the arena prefix before every dispatch) and the op pushes the byte
//! offset of that record. Downstream the `__casefold_lookup` /
//! `__is_combining_mark` / ... helpers add the arena base and read the
//! table through the same `Load*AtAbsolute` path every other arena
//! reference uses.
//!
//! The LLVM `ConstPool` (see `super::ConstPool`) only collects
//! `Op::ConstString` records — it does not carry the Unicode tables,
//! and Phase 0b is not allowed to widen it (it lives in the shared
//! `mod.rs`). To keep the *exact same* arena-relative-offset contract
//! without touching the const pool, each `*TableAddr` op here:
//!
//!   1. Encodes the table with the byte-for-byte-identical encoders in
//!      `relon_ir::{case_folding, combining_marks, whitespace,
//!      normalization, full_case_folding}` (the same functions
//!      `relon-codegen-cranelift`'s `ConstPool` calls), so the bytes a
//!      lookup helper sees are identical across both backends.
//!   2. Materialises those bytes as a private, `module`-deduplicated
//!      `[N x i8]` LLVM global constant.
//!   3. At runtime bump-allocates `N` bytes of arena scratch
//!      (`emit_alloc_scratch_static`, identical mechanism to
//!      `Op::AllocScratch`), `memcpy`s the global into it, and pushes
//!      the resulting arena-relative `i32` offset.
//!
//! The pushed value therefore has the same type, the same units
//! (arena-relative bytes) and the same downstream usage as the
//! cranelift offset — only the placement differs (per-call scratch vs
//! const-data prefix). The table contents are read-only and the global
//! is shared per module, so the per-call copy is the only divergence
//! and it is invisible to every consumer.

use inkwell::module::Linkage;
use inkwell::values::BasicValue;

use relon_ir::ir::Op;

use crate::error::LlvmError;

use super::*;

/// Identifies which encoded Unicode table a `*TableAddr` op refers to.
/// Drives both the deterministic global symbol name (so repeated uses
/// in one module share a single global) and the byte encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnicodeTable {
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
            UnicodeTable::Decomp { compatibility: true } => "relon_uni_decomp_nfkd",
            UnicodeTable::Decomp { compatibility: false } => "relon_uni_decomp_nfd",
            UnicodeTable::Ccc => "relon_uni_ccc",
            UnicodeTable::Composition => "relon_uni_composition",
        }
    }

    /// Encode the table into the exact byte layout the lookup helpers
    /// expect. Each arm calls the same `relon_ir` encoder
    /// `relon-codegen-cranelift`'s `ConstPool` uses, so the bytes are
    /// identical across backends.
    fn encode_bytes(self) -> Vec<u8> {
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
    /// Phase 0b seam: Unicode `*TableAddr` long tail. Dispatched from
    /// `super::lower_op`.
    pub(crate) fn lower_unicode_rest(
        &mut self,
        ip: usize,
        _ip_hint: &str,
        op: &Op,
    ) -> Result<(), LlvmError> {
        let table = match op {
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
            other => {
                return Err(LlvmError::Codegen(format!(
                    "lower_unicode_rest: non-unicode op routed here: {other:?} at ip={ip}"
                )));
            }
        };
        self.emit_unicode_table_addr(table)
    }

    /// Materialise `table` into the arena and push its arena-relative
    /// `i32` offset. See the module-level doc for why this copies into
    /// per-call scratch rather than the const-data prefix.
    fn emit_unicode_table_addr(&mut self, table: UnicodeTable) -> Result<(), LlvmError> {
        let bytes = table.encode_bytes();
        let len = u32::try_from(bytes.len()).map_err(|_| {
            LlvmError::Codegen(format!(
                "unicode table `{}` exceeds u32 byte range",
                table.global_symbol()
            ))
        })?;

        // 1. Source global: a private, module-deduplicated `[N x i8]`
        //    constant holding the encoded table bytes.
        let global = self.unicode_table_global(table, &bytes)?;
        let src_ptr = global.as_pointer_value();

        // 2. Reserve `len` bytes of arena scratch; pushes the
        //    arena-relative offset onto the virtual stack.
        self.emit_alloc_scratch_static(len)?;
        // Peek (don't pop) the freshly-pushed offset so we can compute
        // the destination pointer; the offset stays on the stack as the
        // op's result.
        let off = self
            .stack
            .last()
            .ok_or_else(|| {
                LlvmError::Codegen("unicode table addr: scratch offset missing from stack".into())
            })?
            .val;
        let dst_ptr = self.arena_addr_i32(off)?;

        // 3. memcpy the global into the arena slot. The table is
        //    read-only so a single copy per dispatch is correct; the
        //    bytes are byte-identical to cranelift's const-data record.
        let i64_t = self.ctx.i64_type();
        let len64 = i64_t.const_int(u64::from(len), false);
        self.builder
            .build_memcpy(dst_ptr, 1, src_ptr, 1, len64)
            .map_err(|e| {
                LlvmError::Codegen(format!(
                    "unicode table `{}` memcpy: {e}",
                    table.global_symbol()
                ))
            })?;

        Ok(())
    }

    /// Return the private `[N x i8]` global holding `bytes`, creating
    /// it on first reference and reusing it on subsequent ones (keyed
    /// by the table's stable symbol name).
    fn unicode_table_global(
        &self,
        table: UnicodeTable,
        bytes: &[u8],
    ) -> Result<inkwell::values::GlobalValue<'ctx>, LlvmError> {
        let sym = table.global_symbol();
        if let Some(existing) = self.module.get_global(sym) {
            return Ok(existing);
        }
        let i8_t = self.ctx.i8_type();
        let arr_t = i8_t.array_type(u32::try_from(bytes.len()).map_err(|_| {
            LlvmError::Codegen(format!("unicode table `{sym}` exceeds u32 byte range"))
        })?);
        let global = self.module.add_global(arr_t, None, sym);
        let init: Vec<_> = bytes
            .iter()
            .map(|&b| i8_t.const_int(u64::from(b), false))
            .collect();
        let init_arr = i8_t.const_array(&init);
        global.set_initializer(&init_arr.as_basic_value_enum());
        global.set_constant(true);
        // Internal linkage: the table never escapes the module; the
        // MCJIT engine resolves the pointer locally with no host-side
        // `add_global_mapping`.
        global.set_linkage(Linkage::Internal);
        Ok(global)
    }
}

#[cfg(test)]
mod tests {
    //! Byte-alignment differential for the `*TableAddr` long tail.
    //!
    //! An end-to-end `from_source` three-way comparison is NOT possible
    //! at this phase: the Unicode stdlib bodies that carry these ops
    //! (`upper` / `lower` / `nfd` / ...) also reference ops neither
    //! Phase-0b backend lowers yet (`Op::LoadStringPtr` on cranelift,
    //! `Op::Trap { InvalidUtf8 }` etc. on LLVM), so `from_source` of a
    //! `s.upper()` workload fails to *build* on both sides — long before
    //! any value could be observed (see `tests/phase0b_unicode.rs`).
    //!
    //! The meaningful alignment we CAN pin now is the table data itself:
    //! cranelift's `ConstPool` materialises each table by calling the
    //! shared `relon_ir::{case_folding, full_case_folding,
    //! combining_marks, whitespace, normalization}` encoders, then a
    //! `*TableAddr` op pushes the arena-relative offset of those bytes.
    //! This LLVM backend emits a global initialised from the *same*
    //! encoders and pushes an arena-relative offset to a per-call copy.
    //! These tests build a real LLVM module, lower every `*TableAddr`
    //! op, and assert the emitted global's bytes are byte-for-byte
    //! identical to the encoder output cranelift consumes — so the bytes
    //! a downstream lookup helper reads are guaranteed identical across
    //! both backends.

    use super::*;
    use crate::codegen::{ConstPool, EntryShape};
    use inkwell::context::Context;
    use inkwell::AddressSpace;
    use relon_ir::ir::IrType;

    /// Run `op` through `lower_unicode_rest` inside a freshly-built
    /// buffer-shaped LLVM function and return `(emitted global array
    /// length, stack depth after lowering)`. The function takes a single
    /// `ptr` param used as the arena-state pointer (its `[base]` word is
    /// the arena base) — enough to satisfy `emit_alloc_scratch_static` +
    /// `arena_addr_i32` + the memcpy. We never execute the function; we
    /// only inspect the module the emit pass produced.
    ///
    /// Note we read back the *length* of the emitted `[N x i8]` global,
    /// not its bytes: inkwell 0.9 has no per-element constant reader and
    /// `get_string_constant` truncates at the embedded NULs every table
    /// header carries. The byte-for-byte identity is pinned separately
    /// by `encode_bytes_match_shared_encoders`, which checks the exact
    /// `Vec<u8>` this function feeds into `set_initializer`.
    fn lower_and_global_len(op: &Op, sym: &str) -> (usize, usize) {
        let ctx = Context::create();
        let module = ctx.create_module("unicode_test");
        let ptr_t = ctx.ptr_type(AddressSpace::default());
        let void_t = ctx.void_type();
        let fn_ty = void_t.fn_type(&[ptr_t.into()], false);
        let func = module.add_function("t", fn_ty, None);
        let entry_bb = ctx.append_basic_block(func, "entry");
        let builder = ctx.create_builder();
        builder.position_at_end(entry_bb);

        let state_ptr = func.get_nth_param(0).unwrap().into_pointer_value();
        // arena_base = *(state + ARENA_STATE_OFFSET_BASE) as ptr.
        let i8_t = ctx.i8_type();
        let i32_t = ctx.i32_type();
        let i64_t = ctx.i64_type();
        let base_gep = unsafe {
            builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(
                        crate::state::ARENA_STATE_OFFSET_BASE as u64,
                        false,
                    )],
                    "base_gep",
                )
                .unwrap()
        };
        let base_int = builder
            .build_load(i64_t, base_gep, "base")
            .unwrap()
            .into_int_value();
        let arena_base_ptr = builder
            .build_int_to_ptr(base_int, ptr_t, "arena_base_ptr")
            .unwrap();

        let const_pool = ConstPool::default();
        let mut emit = Emit::new(
            &ctx,
            &builder,
            &module,
            func,
            EntryShape::Buffer,
            Some(arena_base_ptr),
            Some(state_ptr),
            /*buffer_return_size=*/ 0,
            &const_pool,
        );
        emit.lower_unicode_rest(0, "test", op).expect("lower op");
        let depth = emit.stack.len();
        // The lowered op leaves an i32 (the arena offset) on the stack.
        if let Some(top) = emit.stack.last() {
            assert_eq!(top.ty, IrType::I32, "table addr result must be i32");
        }

        let global = module.get_global(sym).expect("global emitted");
        let init = global.get_initializer().expect("initializer set");
        let arr = init.into_array_value();
        let n = arr.get_type().len() as usize;
        (n, depth)
    }

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
                Op::DecompTableAddr { compatibility: false },
                "relon_uni_decomp_nfd",
                relon_ir::normalization::encode_decomp_table_bytes(
                    relon_ir::normalization_data::NFD_INDEX,
                    relon_ir::normalization_data::NFD_POOL,
                ),
            ),
            (
                Op::DecompTableAddr { compatibility: true },
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

    /// Wiring check: lowering each `*TableAddr` op emits a module global
    /// whose `[N x i8]` length equals the encoder byte length and
    /// pushes exactly one arena-offset value. Confirms the lowering
    /// feeds the verified `encode_bytes` vector straight into the
    /// global with no truncation / re-shaping.
    #[test]
    fn table_addr_emits_global_of_encoder_length() {
        for (op, sym, expected) in table_cases() {
            let (global_len, depth) = lower_and_global_len(&op, sym);
            assert_eq!(
                depth, 1,
                "{op:?} must push exactly one (arena-offset) value, got depth {depth}"
            );
            assert_eq!(
                global_len,
                expected.len(),
                "{op:?}: emitted global length differs from shared encoder length"
            );
        }
    }

    /// Repeated references to the same table inside one module must
    /// share a single global (dedup by symbol) rather than re-emitting
    /// the bytes per use — mirrors cranelift's "lay out once, reuse the
    /// offset" const-pool contract.
    #[test]
    fn repeated_table_addr_dedups_global() {
        let ctx = Context::create();
        let module = ctx.create_module("dedup_test");
        let ptr_t = ctx.ptr_type(AddressSpace::default());
        let void_t = ctx.void_type();
        let fn_ty = void_t.fn_type(&[ptr_t.into()], false);
        let func = module.add_function("t", fn_ty, None);
        let entry_bb = ctx.append_basic_block(func, "entry");
        let builder = ctx.create_builder();
        builder.position_at_end(entry_bb);
        let state_ptr = func.get_nth_param(0).unwrap().into_pointer_value();
        let i8_t = ctx.i8_type();
        let i32_t = ctx.i32_type();
        let i64_t = ctx.i64_type();
        let base_gep = unsafe {
            builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(crate::state::ARENA_STATE_OFFSET_BASE as u64, false)],
                    "base_gep",
                )
                .unwrap()
        };
        let base_int = builder
            .build_load(i64_t, base_gep, "base")
            .unwrap()
            .into_int_value();
        let arena_base_ptr = builder
            .build_int_to_ptr(base_int, ptr_t, "arena_base_ptr")
            .unwrap();
        let const_pool = ConstPool::default();
        let mut emit = Emit::new(
            &ctx,
            &builder,
            &module,
            func,
            EntryShape::Buffer,
            Some(arena_base_ptr),
            Some(state_ptr),
            0,
            &const_pool,
        );
        let op = Op::CccTableAddr;
        emit.lower_unicode_rest(0, "a", &op).unwrap();
        emit.lower_unicode_rest(1, "b", &op).unwrap();
        // Two ops → two arena offsets on the stack, but only ONE backing
        // global.
        assert_eq!(emit.stack.len(), 2);
        let mut count = 0;
        let mut g = module.get_first_global();
        while let Some(global) = g {
            if global.get_name().to_string_lossy() == "relon_uni_ccc" {
                count += 1;
            }
            g = global.get_next_global();
        }
        assert_eq!(count, 1, "repeated CccTableAddr must reuse one global");
    }
}
