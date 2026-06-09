//! Per-module const-data pool: bytes laid out at compile time for the
//! arena prefix; `idx -> offset` tables drive the `Op::ConstString` /
//! `Op::ConstList*` / `Op::*TableAddr` lowering paths in
//! [`super::Codegen::emit_op`].
//!
//! The pool is a single owned `Vec<u8>` plus per-record offset maps —
//! the host trampoline copies the bytes into the arena prefix before
//! every invocation. The codegen pass emits `iconst(I32, offset)` so
//! each runtime reference materialises an arena-relative pointer with
//! one instruction.
//!
//! Pool layout is stable across both compile paths (JIT + cranelift-
//! object) so the cache-emitted ET_REL and the live JIT produce
//! byte-identical const-data; the loader doesn't need to re-scan the
//! IR.
//!
//! ## OpVisitor wiring
//!
//! Collection drives through the [`relon_ir::OpVisitor`] dispatch
//! framework: [`ConstPool`] implements [`OpVisitor`] with one method
//! per [`Op`] variant. The driver [`relon_ir::walk_op`] guarantees
//! that adding a new [`Op`] variant to the IR forces a matching
//! method on [`ConstPool`] before the crate builds — the
//! `_ => {}` catch-all that the legacy hand-rolled `match` used would
//! silently skip new const-bearing variants.
//!
//! The byte-layout ordering remains identical to the pre-refactor
//! match: each variant calls into the same body, structured ops
//! (`If` / `Block` / `Loop` / `Call`) recurse via
//! [`relon_ir::walk_body`] in the same arm order, and the stdlib
//! inline-call path keeps reading
//! [`relon_ir::stdlib::builtin_stdlib`] indexed by `fn_index`.

use std::collections::HashMap;

use ordered_float::OrderedFloat;
use relon_ir::ir::{ClosureCapture, F64UnaryOp, IrType, Module as IrModule, TaggedOp, TrapKind};
use relon_ir::{walk_body, OpVisitor};

use crate::error::CraneliftError;

/// Per-module const-pool layout. Maps each IR-level `idx` referenced
/// by `Op::ConstString` / `Op::ConstList*` to its byte offset inside
/// the const-data blob shipped on the [`super::CompiledModule`].
#[derive(Debug, Default, Clone)]
pub(super) struct ConstPool {
    /// String pool: `idx -> byte offset within `bytes`.
    pub(super) string_offsets: HashMap<u32, u32>,
    /// List<Int> pool.
    pub(super) list_int_offsets: HashMap<u32, u32>,
    /// List<Float> pool.
    pub(super) list_float_offsets: HashMap<u32, u32>,
    /// List<Bool> pool.
    pub(super) list_bool_offsets: HashMap<u32, u32>,
    /// W5-P2: List<String> pointer-array pool. Maps each
    /// `Op::ConstListString` `idx` to the byte offset of its header
    /// record (`[len: u32][off_i: u32]...`).
    pub(super) list_string_offsets: HashMap<u32, u32>,
    /// W5-P1: `{String -> Int}` dict pool. Maps each `Op::ConstDict`
    /// `idx` to the byte offset of its arena dict record.
    pub(super) dict_offsets: HashMap<u32, u32>,
    /// Materialised bytes in record order. Cranelift code emits
    /// `i32.const <offset>` so the value at runtime is the buffer-
    /// relative address.
    pub(super) bytes: Vec<u8>,
    /// Lazily-laid-out Unicode case-fold tables. Each entry is set
    /// when the body references `Op::CaseFoldTableAddr { upper }`.
    pub(super) case_fold_upper_offset: Option<u32>,
    pub(super) case_fold_lower_offset: Option<u32>,
    /// Lazily-laid-out combining-mark + whitespace ranges tables.
    pub(super) combining_marks_offset: Option<u32>,
    pub(super) whitespace_offset: Option<u32>,
    /// Unicode normalization tables (NFD / NFKD decompositions,
    /// Canonical_Combining_Class, canonical composition pairs).
    pub(super) decomp_nfd_offset: Option<u32>,
    pub(super) decomp_nfkd_offset: Option<u32>,
    pub(super) ccc_offset: Option<u32>,
    pub(super) composition_offset: Option<u32>,
    /// Full multi-codepoint case-folding tables (UAX #21).
    pub(super) full_case_fold_upper_offset: Option<u32>,
    pub(super) full_case_fold_lower_offset: Option<u32>,
    pub(super) cased_ranges_offset: Option<u32>,
    pub(super) case_ignorable_ranges_offset: Option<u32>,
    /// Locale-aware Turkish / Azerbaijani override tables.
    pub(super) turkish_upper_offset: Option<u32>,
    pub(super) turkish_lower_offset: Option<u32>,
}

impl ConstPool {
    /// Build the pool from a scan of every function body in `module`.
    /// Each unique `idx` ends up with a `[len:u32 LE][payload]` record
    /// laid out in declaration order, aligned to 8 to match the wasm
    /// side. Driven through the shared [`OpVisitor`] dispatch.
    pub(super) fn from_module(module: &IrModule) -> Result<Self, CraneliftError> {
        let mut pool = ConstPool::default();
        for func in &module.funcs {
            pool.collect_body(&func.body)?;
        }
        Ok(pool)
    }

    /// Walk `body` and forward each op through [`OpVisitor`] dispatch.
    /// Kept on `ConstPool` (rather than inlining `walk_body`) because
    /// `collect_body` is reused by the `visit_if` / `visit_block` /
    /// `visit_loop_` / `visit_call` recursion paths and the call sites
    /// read more clearly than repeating `walk_body(&body, self)?;`.
    fn collect_body(&mut self, body: &[TaggedOp]) -> Result<(), CraneliftError> {
        walk_body(body, self)?;
        Ok(())
    }

    fn align_to(&mut self, align: usize) {
        let rem = self.bytes.len() % align;
        if rem != 0 {
            self.bytes.resize(self.bytes.len() + (align - rem), 0);
        }
    }
}

impl OpVisitor for ConstPool {
    type Output = ();
    type Error = CraneliftError;

    // --- Const-data records: the four "interesting" variants. ---

    fn visit_const_string(&mut self, idx: u32, value: &str) -> Result<(), CraneliftError> {
        if self.string_offsets.contains_key(&idx) {
            return Ok(());
        }
        self.align_to(4);
        let off = u32::try_from(self.bytes.len())
            .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
        let len = u32::try_from(value.len())
            .map_err(|_| CraneliftError::Codegen("ConstString length exceeds u32 range".into()))?;
        self.bytes.extend_from_slice(&len.to_le_bytes());
        self.bytes.extend_from_slice(value.as_bytes());
        self.string_offsets.insert(idx, off);
        Ok(())
    }

    fn visit_const_list_int(&mut self, idx: u32, elements: &[i64]) -> Result<(), CraneliftError> {
        if self.list_int_offsets.contains_key(&idx) {
            return Ok(());
        }
        self.align_to(8);
        let off = u32::try_from(self.bytes.len())
            .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
        let len = u32::try_from(elements.len())
            .map_err(|_| CraneliftError::Codegen("ConstListInt length exceeds u32 range".into()))?;
        self.bytes.extend_from_slice(&len.to_le_bytes());
        self.bytes.extend_from_slice(&[0u8; 4]); // pad to 8
        for e in elements {
            self.bytes.extend_from_slice(&e.to_le_bytes());
        }
        self.list_int_offsets.insert(idx, off);
        Ok(())
    }

    fn visit_const_list_float(&mut self, idx: u32, elements: &[u64]) -> Result<(), CraneliftError> {
        if self.list_float_offsets.contains_key(&idx) {
            return Ok(());
        }
        self.align_to(8);
        let off = u32::try_from(self.bytes.len())
            .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
        let len = u32::try_from(elements.len()).map_err(|_| {
            CraneliftError::Codegen("ConstListFloat length exceeds u32 range".into())
        })?;
        self.bytes.extend_from_slice(&len.to_le_bytes());
        self.bytes.extend_from_slice(&[0u8; 4]); // pad to 8
        for e in elements {
            self.bytes.extend_from_slice(&e.to_le_bytes());
        }
        self.list_float_offsets.insert(idx, off);
        Ok(())
    }

    fn visit_const_list_bool(&mut self, idx: u32, elements: &[bool]) -> Result<(), CraneliftError> {
        if self.list_bool_offsets.contains_key(&idx) {
            return Ok(());
        }
        self.align_to(4);
        let off = u32::try_from(self.bytes.len())
            .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
        let len = u32::try_from(elements.len()).map_err(|_| {
            CraneliftError::Codegen("ConstListBool length exceeds u32 range".into())
        })?;
        self.bytes.extend_from_slice(&len.to_le_bytes());
        for e in elements {
            self.bytes.push(if *e { 1 } else { 0 });
        }
        self.list_bool_offsets.insert(idx, off);
        Ok(())
    }

    // --- Unicode bundled-stdlib table addresses: each first hit
    //     materialises the table into the pool. ---

    fn visit_case_fold_table_addr(&mut self, upper: bool) -> Result<(), CraneliftError> {
        let slot = if upper {
            &mut self.case_fold_upper_offset
        } else {
            &mut self.case_fold_lower_offset
        };
        if slot.is_none() {
            self.align_to(4);
            let off = u32::try_from(self.bytes.len())
                .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
            let table: &[(u32, u32)] = if upper {
                relon_ir::case_folding::simple_upper_folding()
            } else {
                relon_ir::case_folding::simple_lower_folding()
            };
            let bytes = relon_ir::case_folding::encode_table_bytes(table);
            self.bytes.extend_from_slice(&bytes);
            if upper {
                self.case_fold_upper_offset = Some(off);
            } else {
                self.case_fold_lower_offset = Some(off);
            }
        }
        Ok(())
    }

    fn visit_combining_mark_ranges_addr(&mut self) -> Result<(), CraneliftError> {
        if self.combining_marks_offset.is_none() {
            self.align_to(4);
            let off = u32::try_from(self.bytes.len())
                .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
            let table = relon_ir::combining_marks::combining_mark_ranges();
            let bytes = relon_ir::combining_marks::encode_ranges_bytes(table);
            self.bytes.extend_from_slice(&bytes);
            self.combining_marks_offset = Some(off);
        }
        Ok(())
    }

    fn visit_whitespace_ranges_addr(&mut self) -> Result<(), CraneliftError> {
        if self.whitespace_offset.is_none() {
            self.align_to(4);
            let off = u32::try_from(self.bytes.len())
                .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
            let table = relon_ir::whitespace::non_ascii_whitespace_ranges();
            let bytes = relon_ir::whitespace::encode_ranges_bytes(table);
            self.bytes.extend_from_slice(&bytes);
            self.whitespace_offset = Some(off);
        }
        Ok(())
    }

    fn visit_decomp_table_addr(&mut self, compatibility: bool) -> Result<(), CraneliftError> {
        let slot = if compatibility {
            &mut self.decomp_nfkd_offset
        } else {
            &mut self.decomp_nfd_offset
        };
        if slot.is_none() {
            self.align_to(4);
            let off = u32::try_from(self.bytes.len())
                .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
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
            let bytes = relon_ir::normalization::encode_decomp_table_bytes(index, payload);
            self.bytes.extend_from_slice(&bytes);
            if compatibility {
                self.decomp_nfkd_offset = Some(off);
            } else {
                self.decomp_nfd_offset = Some(off);
            }
        }
        Ok(())
    }

    fn visit_ccc_table_addr(&mut self) -> Result<(), CraneliftError> {
        if self.ccc_offset.is_none() {
            self.align_to(4);
            let off = u32::try_from(self.bytes.len())
                .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
            let bytes = relon_ir::normalization::encode_ccc_table_bytes(
                relon_ir::normalization_data::CCC_TABLE,
            );
            self.bytes.extend_from_slice(&bytes);
            self.ccc_offset = Some(off);
        }
        Ok(())
    }

    fn visit_composition_table_addr(&mut self) -> Result<(), CraneliftError> {
        if self.composition_offset.is_none() {
            self.align_to(4);
            let off = u32::try_from(self.bytes.len())
                .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
            let bytes = relon_ir::normalization::encode_composition_table_bytes(
                relon_ir::normalization_data::COMPOSITION_PAIRS,
            );
            self.bytes.extend_from_slice(&bytes);
            self.composition_offset = Some(off);
        }
        Ok(())
    }

    fn visit_full_case_fold_table_addr(&mut self, upper: bool) -> Result<(), CraneliftError> {
        let slot = if upper {
            &mut self.full_case_fold_upper_offset
        } else {
            &mut self.full_case_fold_lower_offset
        };
        if slot.is_none() {
            self.align_to(4);
            let off = u32::try_from(self.bytes.len())
                .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
            let table = if upper {
                relon_ir::full_case_folding::full_upper_folding()
            } else {
                relon_ir::full_case_folding::full_lower_folding()
            };
            let bytes = relon_ir::full_case_folding::encode_full_table_bytes(table);
            self.bytes.extend_from_slice(&bytes);
            if upper {
                self.full_case_fold_upper_offset = Some(off);
            } else {
                self.full_case_fold_lower_offset = Some(off);
            }
        }
        Ok(())
    }

    fn visit_cased_ranges_addr(&mut self) -> Result<(), CraneliftError> {
        if self.cased_ranges_offset.is_none() {
            self.align_to(4);
            let off = u32::try_from(self.bytes.len())
                .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
            let table = relon_ir::full_case_folding::cased_ranges();
            let bytes = relon_ir::full_case_folding::encode_ranges_bytes(table);
            self.bytes.extend_from_slice(&bytes);
            self.cased_ranges_offset = Some(off);
        }
        Ok(())
    }

    fn visit_case_ignorable_ranges_addr(&mut self) -> Result<(), CraneliftError> {
        if self.case_ignorable_ranges_offset.is_none() {
            self.align_to(4);
            let off = u32::try_from(self.bytes.len())
                .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
            let table = relon_ir::full_case_folding::case_ignorable_ranges();
            let bytes = relon_ir::full_case_folding::encode_ranges_bytes(table);
            self.bytes.extend_from_slice(&bytes);
            self.case_ignorable_ranges_offset = Some(off);
        }
        Ok(())
    }

    fn visit_turkish_case_fold_table_addr(&mut self, upper: bool) -> Result<(), CraneliftError> {
        let slot = if upper {
            &mut self.turkish_upper_offset
        } else {
            &mut self.turkish_lower_offset
        };
        if slot.is_none() {
            self.align_to(4);
            let off = u32::try_from(self.bytes.len())
                .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
            let table = if upper {
                relon_ir::full_case_folding::turkish_upper_folding()
            } else {
                relon_ir::full_case_folding::turkish_lower_folding()
            };
            let bytes = relon_ir::full_case_folding::encode_simple_view_bytes(table);
            self.bytes.extend_from_slice(&bytes);
            if upper {
                self.turkish_upper_offset = Some(off);
            } else {
                self.turkish_lower_offset = Some(off);
            }
        }
        Ok(())
    }

    // --- Structured ops: recurse into nested bodies so nested
    //     const-data references contribute to the pool. ---

    fn visit_if(
        &mut self,
        _result_ty: IrType,
        then_body: &[TaggedOp],
        else_body: &[TaggedOp],
    ) -> Result<(), CraneliftError> {
        self.collect_body(then_body)?;
        self.collect_body(else_body)?;
        Ok(())
    }

    fn visit_block(
        &mut self,
        _result_ty: Option<IrType>,
        body: &[TaggedOp],
    ) -> Result<(), CraneliftError> {
        self.collect_body(body)
    }

    fn visit_loop_(
        &mut self,
        _result_ty: Option<IrType>,
        body: &[TaggedOp],
    ) -> Result<(), CraneliftError> {
        self.collect_body(body)
    }

    fn visit_call(
        &mut self,
        fn_index: u32,
        _arg_count: u32,
        _param_tys: &[IrType],
        _ret_ty: IrType,
    ) -> Result<(), CraneliftError> {
        // The cranelift backend inlines bundled stdlib bodies. Recurse
        // into the callee so its `ConstString` / `CaseFoldTableAddr`
        // references contribute to the pool before the entry body is
        // lowered. F-D2-G: `.body()` lazily forces the op stream on
        // first touch — the same callee revisited later picks the
        // cached vector for free.
        let stdlib = relon_ir::stdlib::builtin_stdlib();
        if let Some(callee) = stdlib.get(fn_index as usize) {
            self.collect_body(callee.body())?;
        }
        Ok(())
    }

    // --- All remaining ops contribute nothing to the const pool. ---
    //     Kept as explicit no-ops so adding a new const-bearing
    //     variant to `Op` forces a compile error here instead of
    //     silently slipping past.

    fn visit_const_bool(&mut self, _: bool) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_const_i32(&mut self, _: i32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_const_i64(&mut self, _: i64) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_const_f64(&mut self, _: OrderedFloat<f64>) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_const_list_string(
        &mut self,
        idx: u32,
        elements: &[String],
    ) -> Result<(), CraneliftError> {
        if self.list_string_offsets.contains_key(&idx) {
            return Ok(());
        }
        // W5-P2 pointer-array record. Two-part layout, both 4-aligned:
        //
        //   1. Each element's String record `[slen: u32 LE][utf8]` is
        //      emitted first, in element order. Its arena-relative byte
        //      offset is captured.
        //   2. The header `[len: u32 LE][off_0: u32 LE]...[off_{N-1}:
        //      u32 LE]` is emitted afterwards; each `off_i` is the
        //      arena-relative offset of String record `i` captured in
        //      step 1 (the same handle representation `ConstString`
        //      pushes). `idx -> header offset` is the value the
        //      `Op::ConstListString` push resolves to.
        //
        // Indexing (`keys[i]`) loads `off_i` from `header + 4 + i*4` —
        // a ready-made String handle. The String records sit BEFORE the
        // header so a `keys[i]` consumer never reads past the header
        // into payload it didn't ask for.
        self.align_to(4);
        let mut str_offsets: Vec<u32> = Vec::with_capacity(elements.len());
        for s in elements {
            self.align_to(4);
            let s_off = u32::try_from(self.bytes.len()).map_err(|_| {
                CraneliftError::Codegen("ConstListString string offset exceeds u32".into())
            })?;
            let slen = u32::try_from(s.len()).map_err(|_| {
                CraneliftError::Codegen("ConstListString element length exceeds u32".into())
            })?;
            self.bytes.extend_from_slice(&slen.to_le_bytes());
            self.bytes.extend_from_slice(s.as_bytes());
            str_offsets.push(s_off);
        }
        self.align_to(4);
        let header_off = u32::try_from(self.bytes.len()).map_err(|_| {
            CraneliftError::Codegen("ConstListString header offset exceeds u32".into())
        })?;
        let len = u32::try_from(elements.len())
            .map_err(|_| CraneliftError::Codegen("ConstListString length exceeds u32".into()))?;
        self.bytes.extend_from_slice(&len.to_le_bytes());
        for off in &str_offsets {
            self.bytes.extend_from_slice(&off.to_le_bytes());
        }
        self.list_string_offsets.insert(idx, header_off);
        Ok(())
    }
    fn visit_const_dict(
        &mut self,
        idx: u32,
        entries: &[(String, i64)],
    ) -> Result<(), CraneliftError> {
        if self.dict_offsets.contains_key(&idx) {
            return Ok(());
        }
        // W5-P1 arena dict record. Layout (record-relative offsets):
        //   [entry_count: u32 LE]
        //   [shape_hash:  u64 LE]   (canonical over the sorted key set)
        //   entry_count × [key_off: u32 LE][key_len: u32 LE][value: i64 LE]
        //   concatenated UTF-8 key bytes
        // The entry table is sorted by key bytes so the record is
        // deterministic and the W5-P3 static probe can binary-search.
        // The record start is 8-aligned so the i64 values + u64
        // shape_hash land on natural boundaries.
        self.align_to(8);
        let off = u32::try_from(self.bytes.len())
            .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;

        // Sort a copy by key bytes for a deterministic, probe-friendly
        // table. The source order is preserved on the IR op itself.
        let mut sorted: Vec<&(String, i64)> = entries.iter().collect();
        sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

        let entry_count = u32::try_from(sorted.len())
            .map_err(|_| CraneliftError::Codegen("ConstDict entry count exceeds u32".into()))?;
        let shape_hash =
            relon_ir::shape_hash::shape_hash_for_keys(sorted.iter().map(|(k, _)| k.as_str()));

        // Header.
        self.bytes.extend_from_slice(&entry_count.to_le_bytes());
        self.bytes.extend_from_slice(&[0u8; 4]); // pad: keep shape_hash 8-aligned
        self.bytes.extend_from_slice(&shape_hash.to_le_bytes());

        // The key payload begins right after the entry table. Each
        // entry is 16 bytes (u32 off + u32 len + i64 value); the header
        // is 16 bytes (u32 count + u32 pad + u64 hash).
        const HEADER_BYTES: u32 = 16;
        const ENTRY_BYTES: u32 = 16;
        let table_bytes = entry_count
            .checked_mul(ENTRY_BYTES)
            .ok_or_else(|| CraneliftError::Codegen("ConstDict table size overflow".into()))?;
        let key_payload_base = HEADER_BYTES
            .checked_add(table_bytes)
            .ok_or_else(|| CraneliftError::Codegen("ConstDict key base overflow".into()))?;

        // Entry table. key_off is record-relative; accumulate as we go.
        let mut running_key_off = key_payload_base;
        for (key, value) in &sorted {
            let key_len = u32::try_from(key.len())
                .map_err(|_| CraneliftError::Codegen("ConstDict key length exceeds u32".into()))?;
            self.bytes.extend_from_slice(&running_key_off.to_le_bytes());
            self.bytes.extend_from_slice(&key_len.to_le_bytes());
            self.bytes.extend_from_slice(&value.to_le_bytes());
            running_key_off = running_key_off
                .checked_add(key_len)
                .ok_or_else(|| CraneliftError::Codegen("ConstDict key offset overflow".into()))?;
        }

        // Key payload.
        for (key, _) in &sorted {
            self.bytes.extend_from_slice(key.as_bytes());
        }

        self.dict_offsets.insert(idx, off);
        Ok(())
    }
    fn visit_local_get(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_let_get(&mut self, _: u32, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_let_set(&mut self, _: u32, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_field(&mut self, _: u32, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_store_field(&mut self, _: u32, _: IrType, _: bool) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_dict_get_by_string_key(
        &mut self,
        _: u64,
        _: IrType,
        _: Option<u32>,
        _: Option<u32>,
    ) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_list_get_by_int_idx(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_add(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_str_concat_n(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_int_to_str(&mut self) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_sub(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_mul(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_div(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_mod_(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_bit_and(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_convert_i64_to_f64(&mut self) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_f64_to_i64_sat(&mut self) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_f64_unary(&mut self, _: F64UnaryOp) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_eq(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_ne(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_lt(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_le(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_gt(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_ge(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_br(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_br_if(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_br_table(&mut self, _: u32, _: &[u32]) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_return(&mut self) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_select(&mut self, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_trap(&mut self, _: TrapKind) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_string_ptr(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_list_int_ptr(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_list_float_ptr(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_list_bool_ptr(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_list_string_ptr(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_list_schema_ptr(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_list_list_ptr(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_schema_ptr(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_field_at_absolute(&mut self, _: u32, _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_read_string_len(&mut self) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_alloc_root_record(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_alloc_sub_record(&mut self, _: u32, _: u32, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_alloc_scratch_record(&mut self, _: u32, _: u32, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_store_field_at_record(
        &mut self,
        _: u32,
        _: u32,
        _: IrType,
    ) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_store_field_at_record_absolute(
        &mut self,
        _: u32,
        _: u32,
        _: IrType,
    ) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_push_record_base(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_push_record_base_absolute(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_emit_tail_record_from_absolute_addr(
        &mut self,
        _: IrType,
    ) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_build_variant_record(
        &mut self,
        _: u8,
        _: u32,
        _: u32,
        _: Option<u32>,
        _: Option<IrType>,
    ) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_build_variant_record_scratch(
        &mut self,
        _: u8,
        _: u32,
        _: u32,
        _: Option<u32>,
        _: Option<IrType>,
    ) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_build_pointer_list(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_call_native(
        &mut self,
        _: u32,
        _: &[IrType],
        _: IrType,
        _: u32,
    ) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_check_cap(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_make_closure(
        &mut self,
        _: u32,
        _: &[ClosureCapture],
        _: u32,
    ) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_call_closure(&mut self, _: &[IrType], _: IrType) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_alloc_scratch(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_alloc_scratch_dyn(&mut self) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_i32_at_absolute(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_i64_at_absolute(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_i8u_at_absolute(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_load_f64_at_absolute(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_store_i32_at_absolute(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_store_i64_at_absolute(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_store_i8_at_absolute(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_store_f64_at_absolute(&mut self, _: u32) -> Result<(), CraneliftError> {
        Ok(())
    }
    fn visit_memcpy_at_absolute(&mut self) -> Result<(), CraneliftError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_ir::ir::{Func, Module as IrModule, Op, TaggedOp};
    use relon_parser::TokenRange;

    fn tagged(op: Op) -> TaggedOp {
        TaggedOp {
            op,
            range: TokenRange::default(),
        }
    }

    fn synth_module(body: Vec<TaggedOp>) -> IrModule {
        IrModule {
            funcs: vec![Func {
                name: "main".into(),
                params: vec![],
                ret: IrType::I64,
                body,
                range: TokenRange::default(),
            }],
            entry_func_index: Some(0),
            imports: vec![],
            closure_table: vec![],
        }
    }

    /// F-5 wire-format smoke gate (cranelift const-pool side): pin the
    /// exact `[len: u32 LE][payload]` shape of every const-string the
    /// pool emits. `visit_const_string` plus the 5 stdlib bodies that
    /// index payload at `s + 4` (see `crates/relon-ir/src/stdlib/`)
    /// plus `emit_read_string_len` in `codegen/field.rs` plus
    /// `emit_tail_record_from_absolute` in `codegen/record.rs` all
    /// hard-code the 4-byte header. Migration to the 12-byte
    /// `[len_with_ascii_flag][hash:u64][payload]` plan documented in
    /// `docs/internal/review-improvement-169-conststring-wire-full-2026-05-22.md`
    /// must flip every site atomically — this test fires the second
    /// the producer drifts so the partial-coverage silent-corruption
    /// regression (#164 baseline) cannot recur.
    ///
    /// Counterpart producer test:
    /// `relon-eval-api::buffer::write_string_wire_format_smoke_gate`
    /// pins the buffer-protocol writer's matching shape.
    ///
    /// Tests the OpVisitor dispatch end-to-end through `from_module`
    /// (which calls `walk_body` internally).
    #[test]
    fn opvisitor_emits_const_string_record_in_declaration_order() {
        let module = synth_module(vec![
            tagged(Op::ConstString {
                idx: 0,
                value: "hi".into(),
            }),
            tagged(Op::ConstString {
                idx: 1,
                value: "world".into(),
            }),
        ]);
        let pool = ConstPool::from_module(&module).unwrap();
        assert_eq!(pool.string_offsets.get(&0).copied(), Some(0));
        // 4-byte len + 2-byte "hi", aligned to 4 → next record at 8.
        assert_eq!(pool.string_offsets.get(&1).copied(), Some(8));
        // Validate the raw bytes match the wasm-side encoding.
        assert_eq!(&pool.bytes[0..4], &2u32.to_le_bytes());
        assert_eq!(&pool.bytes[4..6], b"hi");
        assert_eq!(&pool.bytes[8..12], &5u32.to_le_bytes());
        assert_eq!(&pool.bytes[12..17], b"world");
    }

    /// Duplicate `ConstString` idx is a no-op (mirrors the legacy
    /// `if self.string_offsets.contains_key(idx)` guard).
    #[test]
    fn duplicate_idx_does_not_grow_pool() {
        let module = synth_module(vec![
            tagged(Op::ConstString {
                idx: 0,
                value: "hi".into(),
            }),
            tagged(Op::ConstString {
                idx: 0,
                value: "hi".into(),
            }),
        ]);
        let pool = ConstPool::from_module(&module).unwrap();
        // Only one record laid down.
        assert_eq!(pool.bytes.len(), 4 + 2);
    }

    /// Structured-op recursion still picks up nested const-data. The
    /// `If` arms each emit one `ConstString`; both must land in the
    /// pool in the order the legacy `match` would have visited them
    /// (then-arm before else-arm).
    #[test]
    fn opvisitor_recurses_into_if_arms() {
        let module = synth_module(vec![tagged(Op::If {
            result_ty: IrType::I64,
            then_body: vec![tagged(Op::ConstString {
                idx: 7,
                value: "t".into(),
            })],
            else_body: vec![tagged(Op::ConstString {
                idx: 8,
                value: "ee".into(),
            })],
        })]);
        let pool = ConstPool::from_module(&module).unwrap();
        // then-arm first, then else-arm.
        assert_eq!(pool.string_offsets.get(&7).copied(), Some(0));
        // First record: 4 + 1 = 5 bytes, aligned to 4 → 8.
        assert_eq!(pool.string_offsets.get(&8).copied(), Some(8));
    }

    /// W5-P1 wire-format gate: pin the exact arena byte layout of a
    /// `{String -> Int}` dict record. The entries are supplied out of
    /// key order; the pool must sort by key bytes, lay down the
    /// `[entry_count][pad][shape_hash]` header, the sorted
    /// `[key_off][key_len][value]` entry table, then the concatenated
    /// key payload — all record-relative. This layout is what the
    /// W5-P3 static dict-probe will binary-search, so any drift in the
    /// header / entry-stride / key-offset math must fire here.
    #[test]
    fn const_dict_arena_layout_is_sorted_and_byte_exact() {
        // Supplied out of order to prove the pool sorts by key bytes.
        let entries = vec![
            ("c".to_string(), 30i64),
            ("a".to_string(), 10i64),
            ("b".to_string(), 20i64),
        ];
        let module = synth_module(vec![tagged(Op::ConstDict {
            idx: 0,
            entries: entries.clone(),
        })]);
        let pool = ConstPool::from_module(&module).unwrap();
        assert_eq!(pool.dict_offsets.get(&0).copied(), Some(0));

        let b = &pool.bytes;
        // Header: entry_count=3, 4-byte pad, shape_hash over sorted keys.
        assert_eq!(&b[0..4], &3u32.to_le_bytes(), "entry_count");
        assert_eq!(&b[4..8], &[0u8; 4], "header pad");
        let expected_hash =
            relon_ir::shape_hash::shape_hash_for_keys(["a", "b", "c"]).to_le_bytes();
        assert_eq!(&b[8..16], &expected_hash, "shape_hash over sorted keys");

        // Entry table starts at 16; each entry is 16 bytes. Key payload
        // base = 16 (header) + 3*16 (table) = 64. Keys "a","b","c" are
        // 1 byte each → key_off 64, 65, 66.
        // Entry 0 = key "a", value 10.
        assert_eq!(&b[16..20], &64u32.to_le_bytes(), "entry0 key_off");
        assert_eq!(&b[20..24], &1u32.to_le_bytes(), "entry0 key_len");
        assert_eq!(&b[24..32], &10i64.to_le_bytes(), "entry0 value");
        // Entry 1 = key "b", value 20.
        assert_eq!(&b[32..36], &65u32.to_le_bytes(), "entry1 key_off");
        assert_eq!(&b[36..40], &1u32.to_le_bytes(), "entry1 key_len");
        assert_eq!(&b[40..48], &20i64.to_le_bytes(), "entry1 value");
        // Entry 2 = key "c", value 30.
        assert_eq!(&b[48..52], &66u32.to_le_bytes(), "entry2 key_off");
        assert_eq!(&b[52..56], &1u32.to_le_bytes(), "entry2 key_len");
        assert_eq!(&b[56..64], &30i64.to_le_bytes(), "entry2 value");
        // Key payload: sorted keys concatenated.
        assert_eq!(&b[64..67], b"abc", "key payload");
        assert_eq!(b.len(), 67, "total record length");

        // Each entry's key_off/key_len slices the right key bytes.
        for (i, (k, _)) in [("a", 10i64), ("b", 20), ("c", 30)].iter().enumerate() {
            let entry_base = 16 + i * 16;
            let off =
                u32::from_le_bytes(b[entry_base..entry_base + 4].try_into().unwrap()) as usize;
            let len =
                u32::from_le_bytes(b[entry_base + 4..entry_base + 8].try_into().unwrap()) as usize;
            assert_eq!(&b[off..off + len], k.as_bytes(), "key {i} payload slice");
        }
    }

    /// W5-P2 wire-format gate: pin the exact arena byte layout of a
    /// `List<String>` pointer-array record. MUST stay byte-identical to
    /// the LLVM side's `const_list_string_byte_layout` (cross-backend
    /// arena data contract — both pools copy the same blob into the
    /// arena prefix; a drift on either side silently corrupts the
    /// other's cached ET_REL). The element String records sit first
    /// (4-aligned), then the `[len][off_i...]` header whose `off_i` is
    /// the arena-relative offset of String record `i`.
    #[test]
    fn const_list_string_pointer_array_is_byte_exact() {
        let module = synth_module(vec![tagged(Op::ConstListString {
            idx: 0,
            elements: vec!["a".into(), "bb".into(), "ccc".into()],
        })]);
        let pool = ConstPool::from_module(&module).unwrap();
        let b = &pool.bytes;
        // String record "a" at offset 0: [slen=1]["a"], pad to 8.
        assert_eq!(&b[0..4], &1u32.to_le_bytes());
        assert_eq!(&b[4..5], b"a");
        // "bb" at offset 8: [slen=2]["bb"], pad to 16.
        assert_eq!(&b[8..12], &2u32.to_le_bytes());
        assert_eq!(&b[12..14], b"bb");
        // "ccc" at offset 16: [slen=3]["ccc"], pad to 24.
        assert_eq!(&b[16..20], &3u32.to_le_bytes());
        assert_eq!(&b[20..23], b"ccc");
        // Header at offset 24: [len=3][off_0=0][off_1=8][off_2=16].
        assert_eq!(pool.list_string_offsets.get(&0).copied(), Some(24));
        assert_eq!(&b[24..28], &3u32.to_le_bytes());
        assert_eq!(&b[28..32], &0u32.to_le_bytes());
        assert_eq!(&b[32..36], &8u32.to_le_bytes());
        assert_eq!(&b[36..40], &16u32.to_le_bytes());
        assert_eq!(b.len(), 40);
    }

    /// W5-P2: duplicate `ConstListString` idx is a no-op (idempotent
    /// layout, mirroring the other const-list guards).
    #[test]
    fn duplicate_const_list_string_idx_does_not_grow_pool() {
        let module = synth_module(vec![
            tagged(Op::ConstListString {
                idx: 0,
                elements: vec!["x".into()],
            }),
            tagged(Op::ConstListString {
                idx: 0,
                elements: vec!["x".into()],
            }),
        ]);
        let pool = ConstPool::from_module(&module).unwrap();
        // String record "x": 4 + 1 = 5, pad to 8; header [len=1][off=0]
        // = 8 bytes → total 16, laid once.
        assert_eq!(pool.bytes.len(), 16);
    }

    /// W5-P1: duplicate `ConstDict` idx is a no-op (idempotent layout,
    /// mirroring the const-string / const-list guards).
    #[test]
    fn duplicate_const_dict_idx_does_not_grow_pool() {
        let entries = vec![("a".to_string(), 1i64)];
        let module = synth_module(vec![
            tagged(Op::ConstDict {
                idx: 0,
                entries: entries.clone(),
            }),
            tagged(Op::ConstDict { idx: 0, entries }),
        ]);
        let pool = ConstPool::from_module(&module).unwrap();
        // Header 16 + entry 16 + key "a" 1 = 33 bytes, laid once.
        assert_eq!(pool.bytes.len(), 33);
    }
}
