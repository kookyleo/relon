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

use std::collections::HashMap;

use relon_ir::ir::{Module as IrModule, Op, TaggedOp};

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
    /// Build the pool from a scan of the entry's IR body. Each unique
    /// `idx` ends up with a `[len:u32 LE][payload]` record laid out
    /// in declaration order, aligned to 8 to match the wasm side.
    pub(super) fn from_module(module: &IrModule) -> Result<Self, CraneliftError> {
        let mut pool = ConstPool::default();
        for func in &module.funcs {
            pool.collect_body(&func.body)?;
        }
        Ok(pool)
    }

    fn collect_body(&mut self, body: &[TaggedOp]) -> Result<(), CraneliftError> {
        for tagged in body {
            self.collect_op(&tagged.op)?;
        }
        Ok(())
    }

    fn collect_op(&mut self, op: &Op) -> Result<(), CraneliftError> {
        match op {
            Op::ConstString { idx, value } => {
                if self.string_offsets.contains_key(idx) {
                    return Ok(());
                }
                self.align_to(4);
                let off = u32::try_from(self.bytes.len())
                    .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
                let len = u32::try_from(value.len()).map_err(|_| {
                    CraneliftError::Codegen("ConstString length exceeds u32 range".into())
                })?;
                self.bytes.extend_from_slice(&len.to_le_bytes());
                self.bytes.extend_from_slice(value.as_bytes());
                self.string_offsets.insert(*idx, off);
            }
            Op::ConstListInt { idx, elements } => {
                if self.list_int_offsets.contains_key(idx) {
                    return Ok(());
                }
                self.align_to(8);
                let off = u32::try_from(self.bytes.len())
                    .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
                let len = u32::try_from(elements.len()).map_err(|_| {
                    CraneliftError::Codegen("ConstListInt length exceeds u32 range".into())
                })?;
                self.bytes.extend_from_slice(&len.to_le_bytes());
                self.bytes.extend_from_slice(&[0u8; 4]); // pad to 8
                for e in elements {
                    self.bytes.extend_from_slice(&e.to_le_bytes());
                }
                self.list_int_offsets.insert(*idx, off);
            }
            Op::ConstListFloat { idx, elements } => {
                if self.list_float_offsets.contains_key(idx) {
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
                self.list_float_offsets.insert(*idx, off);
            }
            Op::ConstListBool { idx, elements } => {
                if self.list_bool_offsets.contains_key(idx) {
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
                self.list_bool_offsets.insert(*idx, off);
            }
            Op::CaseFoldTableAddr { upper } => {
                let slot = if *upper {
                    &mut self.case_fold_upper_offset
                } else {
                    &mut self.case_fold_lower_offset
                };
                if slot.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let table: &[(u32, u32)] = if *upper {
                        relon_ir::case_folding::simple_upper_folding()
                    } else {
                        relon_ir::case_folding::simple_lower_folding()
                    };
                    let bytes = relon_ir::case_folding::encode_table_bytes(table);
                    self.bytes.extend_from_slice(&bytes);
                    if *upper {
                        self.case_fold_upper_offset = Some(off);
                    } else {
                        self.case_fold_lower_offset = Some(off);
                    }
                }
            }
            Op::CombiningMarkRangesAddr if self.combining_marks_offset.is_none() => {
                self.align_to(4);
                let off = u32::try_from(self.bytes.len())
                    .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
                let table = relon_ir::combining_marks::combining_mark_ranges();
                let bytes = relon_ir::combining_marks::encode_ranges_bytes(table);
                self.bytes.extend_from_slice(&bytes);
                self.combining_marks_offset = Some(off);
            }
            Op::WhitespaceRangesAddr if self.whitespace_offset.is_none() => {
                self.align_to(4);
                let off = u32::try_from(self.bytes.len())
                    .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
                let table = relon_ir::whitespace::non_ascii_whitespace_ranges();
                let bytes = relon_ir::whitespace::encode_ranges_bytes(table);
                self.bytes.extend_from_slice(&bytes);
                self.whitespace_offset = Some(off);
            }
            Op::DecompTableAddr { compatibility } => {
                let slot = if *compatibility {
                    &mut self.decomp_nfkd_offset
                } else {
                    &mut self.decomp_nfd_offset
                };
                if slot.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let (index, payload) = if *compatibility {
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
                    if *compatibility {
                        self.decomp_nfkd_offset = Some(off);
                    } else {
                        self.decomp_nfd_offset = Some(off);
                    }
                }
            }
            Op::CccTableAddr if self.ccc_offset.is_none() => {
                self.align_to(4);
                let off = u32::try_from(self.bytes.len())
                    .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
                let bytes = relon_ir::normalization::encode_ccc_table_bytes(
                    relon_ir::normalization_data::CCC_TABLE,
                );
                self.bytes.extend_from_slice(&bytes);
                self.ccc_offset = Some(off);
            }
            Op::CompositionTableAddr if self.composition_offset.is_none() => {
                self.align_to(4);
                let off = u32::try_from(self.bytes.len())
                    .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
                let bytes = relon_ir::normalization::encode_composition_table_bytes(
                    relon_ir::normalization_data::COMPOSITION_PAIRS,
                );
                self.bytes.extend_from_slice(&bytes);
                self.composition_offset = Some(off);
            }
            Op::FullCaseFoldTableAddr { upper } => {
                let slot = if *upper {
                    &mut self.full_case_fold_upper_offset
                } else {
                    &mut self.full_case_fold_lower_offset
                };
                if slot.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let table = if *upper {
                        relon_ir::full_case_folding::full_upper_folding()
                    } else {
                        relon_ir::full_case_folding::full_lower_folding()
                    };
                    let bytes = relon_ir::full_case_folding::encode_full_table_bytes(table);
                    self.bytes.extend_from_slice(&bytes);
                    if *upper {
                        self.full_case_fold_upper_offset = Some(off);
                    } else {
                        self.full_case_fold_lower_offset = Some(off);
                    }
                }
            }
            Op::CasedRangesAddr if self.cased_ranges_offset.is_none() => {
                self.align_to(4);
                let off = u32::try_from(self.bytes.len())
                    .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
                let table = relon_ir::full_case_folding::cased_ranges();
                let bytes = relon_ir::full_case_folding::encode_ranges_bytes(table);
                self.bytes.extend_from_slice(&bytes);
                self.cased_ranges_offset = Some(off);
            }
            Op::CaseIgnorableRangesAddr if self.case_ignorable_ranges_offset.is_none() => {
                self.align_to(4);
                let off = u32::try_from(self.bytes.len())
                    .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
                let table = relon_ir::full_case_folding::case_ignorable_ranges();
                let bytes = relon_ir::full_case_folding::encode_ranges_bytes(table);
                self.bytes.extend_from_slice(&bytes);
                self.case_ignorable_ranges_offset = Some(off);
            }
            Op::TurkishCaseFoldTableAddr { upper } => {
                let slot = if *upper {
                    &mut self.turkish_upper_offset
                } else {
                    &mut self.turkish_lower_offset
                };
                if slot.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let table = if *upper {
                        relon_ir::full_case_folding::turkish_upper_folding()
                    } else {
                        relon_ir::full_case_folding::turkish_lower_folding()
                    };
                    let bytes = relon_ir::full_case_folding::encode_simple_view_bytes(table);
                    self.bytes.extend_from_slice(&bytes);
                    if *upper {
                        self.turkish_upper_offset = Some(off);
                    } else {
                        self.turkish_lower_offset = Some(off);
                    }
                }
            }
            // Recurse into structured bodies so nested ConstStrings
            // (e.g. inside If arms or Block / Loop bodies) get
            // picked up too.
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                self.collect_body(then_body)?;
                self.collect_body(else_body)?;
            }
            Op::Block { body, .. } | Op::Loop { body, .. } => {
                self.collect_body(body)?;
            }
            Op::Call { fn_index, .. } => {
                // The cranelift backend inlines bundled stdlib bodies.
                // Recurse into the callee so its `ConstString` /
                // `CaseFoldTableAddr` references contribute to the
                // pool before the entry body is lowered. F-D2-G:
                // `.body()` lazily forces the op stream on first
                // touch — the same callee revisited later picks the
                // cached vector for free.
                let stdlib = relon_ir::stdlib::builtin_stdlib();
                if let Some(callee) = stdlib.get(*fn_index as usize) {
                    self.collect_body(callee.body())?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn align_to(&mut self, align: usize) {
        let rem = self.bytes.len() % align;
        if rem != 0 {
            self.bytes.resize(self.bytes.len() + (align - rem), 0);
        }
    }
}
