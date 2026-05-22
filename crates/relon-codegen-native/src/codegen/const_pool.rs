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
use relon_ir::ir::{ClosureCapture, IrType, Module as IrModule, TaggedOp, TrapKind};
use relon_ir::{walk_body, OpVisitor};

use crate::error::CraneliftError;

/// Per-module const-pool layout. Maps each IR-level `idx` referenced
/// by `Op::ConstString` / `Op::ConstList*` to its byte offset inside
/// the const-data blob shipped on the [`super::CompiledModule`].
#[derive(Debug, Default, Clone)]
pub(super) struct ConstPool {
    /// String pool: `idx -> byte offset within `bytes`.
    pub(super) string_offsets: HashMap<u32, u32>,
    /// Tier 1b cached `fx_hash_bytes(payload)` for each
    /// `Op::ConstString` payload. Populated in parallel with
    /// [`Self::string_offsets`] at IR-walk time so a future trace-JIT
    /// consumer that wants to cross a const string into a dict key
    /// can read the digest off the pool without re-running the
    /// byte-wise hash loop. Kept as a side table (rather than packed
    /// into the wire-layout header) so the existing stdlib body
    /// `+4`-offset payload reads (concat / substring / upper / …)
    /// keep working byte-for-byte against the legacy
    /// `[len:u32][payload]` const-record shape. The widened header
    /// layout used on dict-key records (#149) is documented as the
    /// Tier 1b target for the cranelift AOT path, but the wire
    /// migration has to land alongside a coordinated rewrite of the
    /// stdlib bodies' payload offsets and the
    /// `EmitTailRecordFromAbsoluteAddr` strip-header copy — see the
    /// follow-up tracked in the #164 stage report.
    pub(super) string_hashes: HashMap<u32, u64>,
    /// List<Int> pool.
    pub(super) list_int_offsets: HashMap<u32, u32>,
    /// List<Float> pool.
    pub(super) list_float_offsets: HashMap<u32, u32>,
    /// List<Bool> pool.
    pub(super) list_bool_offsets: HashMap<u32, u32>,
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
        // Tier 1b: stamp `fx_hash_bytes(payload)` into the parallel
        // side table at IR-walk time. The wire-layout side stays on
        // the legacy `[len:u32][payload]` shape (see the
        // `string_hashes` doc for why); the cached digest sits next to
        // the offset so a future consumer that wants to cross a const
        // string into a dict key reads it for free.
        self.string_hashes
            .insert(idx, relon_trace_abi::hash::fx_hash_bytes(value.as_bytes()));
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
    fn visit_const_list_string(&mut self, _: u32, _: &[String]) -> Result<(), CraneliftError> {
        // The IR carries this variant but the cranelift backend has
        // not yet wired pointer-array materialisation into the const
        // pool. Stays a no-op until pointer-array support widens
        // (mirrors the pre-OpVisitor behaviour where the legacy
        // `match` skipped the variant via the catch-all arm).
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
    fn visit_store_field(&mut self, _: u32, _: IrType) -> Result<(), CraneliftError> {
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
    fn visit_store_field_at_record(
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
    fn visit_emit_tail_record_from_absolute_addr(
        &mut self,
        _: IrType,
    ) -> Result<(), CraneliftError> {
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

    /// Sanity: visit_const_string / visit_const_list_int / visit_const_list_bool
    /// land in the pool in declaration order with the same `[len:u32 LE]`
    /// prefix the legacy match emitted. Tests the OpVisitor dispatch end-
    /// to-end through `from_module` (which calls `walk_body` internally).
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

    /// Tier 1b: visit_const_string pre-stamps `fx_hash_bytes(payload)`
    /// into the side table for every unique idx. Round-trips against
    /// the canonical `relon-trace-abi` reference so producer + consumer
    /// can never drift apart.
    #[test]
    fn opvisitor_caches_fx_hash_for_each_const_string() {
        let module = synth_module(vec![
            tagged(Op::ConstString {
                idx: 0,
                value: "alpha".into(),
            }),
            tagged(Op::ConstString {
                idx: 1,
                value: "caf\u{00E9}".into(),
            }),
        ]);
        let pool = ConstPool::from_module(&module).unwrap();
        assert_eq!(
            pool.string_hashes.get(&0).copied(),
            Some(relon_trace_abi::hash::fx_hash_bytes(b"alpha")),
            "ASCII payload hash must match fx_hash_bytes reference"
        );
        assert_eq!(
            pool.string_hashes.get(&1).copied(),
            Some(relon_trace_abi::hash::fx_hash_bytes(
                "caf\u{00E9}".as_bytes()
            )),
            "non-ASCII payload hash must match fx_hash_bytes reference"
        );
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
}
