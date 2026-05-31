//! [`relon_ir::OpVisitor`] impl for [`super::Codegen`].
//!
//! Each method maps to one [`relon_ir::ir::Op`] variant. The trait
//! body is intentionally narrow — most arms delegate to an existing
//! `emit_*` helper on `Codegen`, mirroring the pre-refactor
//! `emit_op` match. Variants the cranelift backend does not (yet)
//! lower keep returning [`crate::CraneliftError::Codegen`] through
//! [`unsupported`], preserving the v5-β-2 fallback path that lets
//! the auto-tier router downgrade to the wasm-AOT / tree-walking
//! backend without crashing the host.
//!
//! Switching `Codegen::emit_op` from a 78-arm hand-rolled `match` to
//! [`relon_ir::walk_op`] dispatched against this impl gives the
//! cranelift backend the same compile-time exhaustiveness guarantee
//! the bytecode + ConstPool backends already enjoy: adding a new
//! [`Op`] variant fails the build until a matching `visit_*` method
//! lands.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::InstBuilder;
use ordered_float::OrderedFloat;

use relon_ir::ir::{ClosureCapture, IrType, TaggedOp, TrapKind as IrTrapKind};
use relon_ir::OpVisitor;

use crate::error::CraneliftError;
use crate::sandbox::TrapKind;

use super::const_pool_emit::ConstValueKind;
use super::Codegen;

/// Standard "this op isn't supported by the cranelift backend yet"
/// failure. Mirrors the body of the pre-refactor `other =>` arm.
fn unsupported(name: &str) -> Result<(), CraneliftError> {
    Err(CraneliftError::Codegen(format!(
        "unsupported op in v5-beta-2 stage 3: {name}"
    )))
}

impl<'a, 'b> OpVisitor for Codegen<'a, 'b> {
    type Output = ();
    type Error = CraneliftError;

    // Constants.
    fn visit_const_bool(&mut self, value: bool) -> Result<(), CraneliftError> {
        let val = self.builder.ins().iconst(I32, i64::from(value as i32));
        self.push(val);
        Ok(())
    }

    fn visit_const_i32(&mut self, value: i32) -> Result<(), CraneliftError> {
        let val = self.builder.ins().iconst(I32, i64::from(value));
        self.push(val);
        Ok(())
    }

    fn visit_const_i64(&mut self, value: i64) -> Result<(), CraneliftError> {
        let val = self.builder.ins().iconst(I64, value);
        self.push(val);
        Ok(())
    }

    fn visit_const_f64(&mut self, v: OrderedFloat<f64>) -> Result<(), CraneliftError> {
        let val = self.builder.ins().f64const(v.into_inner());
        self.push(val);
        Ok(())
    }

    fn visit_const_string(&mut self, idx: u32, _value: &str) -> Result<(), CraneliftError> {
        self.emit_const_value(idx, ConstValueKind::String)?;
        Ok(())
    }

    fn visit_const_list_int(&mut self, idx: u32, _elements: &[i64]) -> Result<(), CraneliftError> {
        self.emit_const_value(idx, ConstValueKind::ListInt)?;
        Ok(())
    }

    fn visit_const_list_float(
        &mut self,
        idx: u32,
        _elements: &[u64],
    ) -> Result<(), CraneliftError> {
        self.emit_const_value(idx, ConstValueKind::ListFloat)?;
        Ok(())
    }

    fn visit_const_list_bool(
        &mut self,
        idx: u32,
        _elements: &[bool],
    ) -> Result<(), CraneliftError> {
        self.emit_const_value(idx, ConstValueKind::ListBool)?;
        Ok(())
    }

    fn visit_const_list_string(
        &mut self,
        _idx: u32,
        _elements: &[String],
    ) -> Result<(), CraneliftError> {
        unsupported("ConstListString")
    }

    // Locals + let-locals.
    fn visit_local_get(&mut self, idx: u32) -> Result<(), CraneliftError> {
        let v = self.get_local(idx)?;
        self.push(v);
        Ok(())
    }

    fn visit_let_get(&mut self, idx: u32, ty: IrType) -> Result<(), CraneliftError> {
        let mapped = self.remap_let_idx(idx);
        let v = self.get_let(mapped, ty)?;
        self.push(v);
        Ok(())
    }

    fn visit_let_set(&mut self, idx: u32, ty: IrType) -> Result<(), CraneliftError> {
        let mapped = self.remap_let_idx(idx);
        let v = self.pop()?;
        self.set_let(mapped, ty, v);
        Ok(())
    }

    // Field load / store on the implicit `in_buf` / `out_buf` slots.
    fn visit_load_field(&mut self, offset: u32, ty: IrType) -> Result<(), CraneliftError> {
        self.emit_load_field(offset, ty)
    }

    fn visit_store_field(&mut self, offset: u32, ty: IrType) -> Result<(), CraneliftError> {
        self.emit_store_field(offset, ty)
    }

    // Subscript ops — not lowered by cranelift today.
    fn visit_dict_get_by_string_key(
        &mut self,
        _shape_hash: u64,
        _value_ty: IrType,
        _entry_count_hint: Option<u32>,
        _record_len_hint: Option<u32>,
    ) -> Result<(), CraneliftError> {
        unsupported("DictGetByStringKey")
    }

    fn visit_list_get_by_int_idx(&mut self, _element_ty: IrType) -> Result<(), CraneliftError> {
        unsupported("ListGetByIntIdx")
    }

    // Arithmetic.
    fn visit_add(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        match ty {
            IrType::I64 => self.emit_add_i64(),
            IrType::I32 => self.emit_add_i32(),
            IrType::F64 => self.emit_add_f64(),
            // F-D7-D: `Op::Add(IrType::String)` routes through the
            // bundled stdlib `concat` body via the existing
            // emit_call_stdlib inlining path — same operand-stack
            // discipline (`[.., lhs, rhs] -> [.., result]`).
            IrType::String => {
                let concat_idx =
                    relon_ir::stdlib::stdlib_function_index("concat").ok_or_else(|| {
                        CraneliftError::Codegen("stdlib `concat` slot not found".to_string())
                    })?;
                let param_tys = [IrType::String, IrType::String];
                self.emit_call_stdlib(concat_idx, 2, &param_tys, IrType::String)
            }
            _ => unsupported("Add"),
        }
    }

    // #165 — `Op::StrConcatN { operand_count }` is the IR-level fold of
    // a left-leaning source chain `(((a + b) + c) + d)` for `String +
    // String + ... + String`. Cranelift emits an inline single-
    // allocation join: it walks the N operand `StringRef`s already on
    // the operand stack, sums their `len` fields once, allocates one
    // scratch record sized `total + 4`, stamps the header, and copies
    // each payload at the running cursor — replacing the N - 1
    // pairwise `concat` allocations the unfolded path used to emit.
    fn visit_str_concat_n(&mut self, operand_count: u32) -> Result<(), CraneliftError> {
        self.emit_str_concat_n(operand_count)
    }

    fn visit_sub(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        match ty {
            IrType::I64 => self.emit_sub_i64(),
            IrType::I32 => self.emit_sub_i32(),
            IrType::F64 => self.emit_sub_f64(),
            _ => unsupported("Sub"),
        }
    }

    fn visit_mul(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        match ty {
            IrType::I64 => self.emit_mul_i64(),
            IrType::I32 => self.emit_mul_i32(),
            IrType::F64 => self.emit_mul_f64(),
            _ => unsupported("Mul"),
        }
    }

    fn visit_div(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        match ty {
            IrType::I64 => self.emit_div_i64(),
            IrType::I32 => self.emit_div_i32(),
            IrType::F64 => self.emit_div_f64(),
            _ => unsupported("Div"),
        }
    }

    fn visit_mod_(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        match ty {
            IrType::I64 => self.emit_mod_i64(),
            IrType::I32 => self.emit_mod_i32(),
            // Cranelift has no native `frem` instruction (x86 has no
            // hardware float-remainder; LLVM lowers `frem` to an
            // `fmod` libcall too). The #362 graceful reject is now a
            // real call against the module-declared external `fmod`,
            // guarded by the same Float zero-divisor trap as Float
            // `/`. See `emit_mod_f64`.
            IrType::F64 => self.emit_mod_f64(),
            _ => unsupported("Mod"),
        }
    }

    fn visit_bit_and(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        match ty {
            IrType::I64 => self.emit_bitand_i64(),
            IrType::I32 => self.emit_bitand_i32(),
            _ => unsupported("BitAnd"),
        }
    }

    fn visit_convert_i64_to_f64(&mut self) -> Result<(), CraneliftError> {
        self.emit_convert_i64_to_f64()
    }

    // Comparison.
    fn visit_eq(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        match ty {
            IrType::I64 => self.emit_cmp(IntCC::Equal),
            IrType::I32 | IrType::Bool => self.emit_cmp_i32(IntCC::Equal),
            // F64 `==` follows `OrderedFloat` (NaN == NaN is true), not
            // raw ordered IEEE — see `emit_fcmp_eq`.
            IrType::F64 => self.emit_fcmp_eq(/*negate=*/ false),
            _ => unsupported("Eq"),
        }
    }

    fn visit_ne(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        match ty {
            IrType::I64 => self.emit_cmp(IntCC::NotEqual),
            IrType::I32 | IrType::Bool => self.emit_cmp_i32(IntCC::NotEqual),
            // F64 `!=` is the negation of the NaN-aware `==` (so
            // NaN != NaN is false) — see `emit_fcmp_eq`.
            IrType::F64 => self.emit_fcmp_eq(/*negate=*/ true),
            _ => unsupported("Ne"),
        }
    }

    fn visit_lt(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        match ty {
            IrType::I64 => self.emit_cmp(IntCC::SignedLessThan),
            IrType::I32 => self.emit_cmp_i32(IntCC::SignedLessThan),
            IrType::F64 => self.emit_fcmp(FloatCC::LessThan),
            _ => unsupported("Lt"),
        }
    }

    fn visit_le(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        match ty {
            IrType::I64 => self.emit_cmp(IntCC::SignedLessThanOrEqual),
            IrType::I32 => self.emit_cmp_i32(IntCC::SignedLessThanOrEqual),
            IrType::F64 => self.emit_fcmp(FloatCC::LessThanOrEqual),
            _ => unsupported("Le"),
        }
    }

    fn visit_gt(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        match ty {
            IrType::I64 => self.emit_cmp(IntCC::SignedGreaterThan),
            IrType::I32 => self.emit_cmp_i32(IntCC::SignedGreaterThan),
            IrType::F64 => self.emit_fcmp(FloatCC::GreaterThan),
            _ => unsupported("Gt"),
        }
    }

    fn visit_ge(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        match ty {
            IrType::I64 => self.emit_cmp(IntCC::SignedGreaterThanOrEqual),
            IrType::I32 => self.emit_cmp_i32(IntCC::SignedGreaterThanOrEqual),
            IrType::F64 => self.emit_fcmp(FloatCC::GreaterThanOrEqual),
            _ => unsupported("Ge"),
        }
    }

    // Control flow.
    fn visit_if(
        &mut self,
        result_ty: IrType,
        then_body: &[TaggedOp],
        else_body: &[TaggedOp],
    ) -> Result<(), CraneliftError> {
        self.emit_if(result_ty, then_body, else_body)
    }

    fn visit_block(
        &mut self,
        result_ty: Option<IrType>,
        body: &[TaggedOp],
    ) -> Result<(), CraneliftError> {
        self.emit_block(result_ty, body, false)
    }

    fn visit_loop_(
        &mut self,
        result_ty: Option<IrType>,
        body: &[TaggedOp],
    ) -> Result<(), CraneliftError> {
        self.emit_block(result_ty, body, true)
    }

    fn visit_br(&mut self, label_depth: u32) -> Result<(), CraneliftError> {
        self.emit_br(label_depth, /*conditional=*/ false)
    }

    fn visit_br_if(&mut self, label_depth: u32) -> Result<(), CraneliftError> {
        self.emit_br(label_depth, /*conditional=*/ true)
    }

    fn visit_br_table(&mut self, default: u32, targets: &[u32]) -> Result<(), CraneliftError> {
        self.emit_br_table(default, targets)
    }

    fn visit_return(&mut self) -> Result<(), CraneliftError> {
        self.emit_return()
    }

    fn visit_select(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        // v5-β-2 widen: stack discipline mirrors wasm —
        // pop `[val_true, val_false, cond]`, push `val_true` when
        // `cond` is non-zero, else `val_false`. cranelift's `select`
        // takes `(cond, val_if_true, val_if_false)`.
        let cond = self.pop()?;
        let val_false = self.pop()?;
        let val_true = self.pop()?;
        // The IR pass guarantees both arms share the same wasm slot;
        // we don't inspect the tag beyond holding it for the future
        // mismatched-width sanity-check trap.
        let _ = ty;
        let r = self.builder.ins().select(cond, val_true, val_false);
        self.push(r);
        Ok(())
    }

    fn visit_trap(&mut self, kind: IrTrapKind) -> Result<(), CraneliftError> {
        // `relon_ir::TrapKind` covers stdlib-domain failures
        // (IndexOutOfBounds / EmptyList / InvalidUtf8). Map them
        // onto the sandbox-side BoundsViolation / Unreachable surface
        // until v6-γ widens the trap taxonomy.
        let mapped = match kind {
            IrTrapKind::IndexOutOfBounds | IrTrapKind::EmptyList => TrapKind::BoundsViolation,
            IrTrapKind::InvalidUtf8 => TrapKind::Unreachable,
        };
        self.emit_trap(mapped)
    }

    // Pointer / list loads from the in_buf — not lowered yet.
    fn visit_load_string_ptr(&mut self, _offset: u32) -> Result<(), CraneliftError> {
        unsupported("LoadStringPtr")
    }

    fn visit_load_list_int_ptr(&mut self, _offset: u32) -> Result<(), CraneliftError> {
        unsupported("LoadListIntPtr")
    }

    fn visit_load_list_float_ptr(&mut self, _offset: u32) -> Result<(), CraneliftError> {
        unsupported("LoadListFloatPtr")
    }

    fn visit_load_list_bool_ptr(&mut self, _offset: u32) -> Result<(), CraneliftError> {
        unsupported("LoadListBoolPtr")
    }

    fn visit_load_list_string_ptr(&mut self, _offset: u32) -> Result<(), CraneliftError> {
        unsupported("LoadListStringPtr")
    }

    fn visit_load_list_schema_ptr(&mut self, _offset: u32) -> Result<(), CraneliftError> {
        unsupported("LoadListSchemaPtr")
    }

    fn visit_load_schema_ptr(&mut self, _offset: u32) -> Result<(), CraneliftError> {
        unsupported("LoadSchemaPtr")
    }

    fn visit_load_field_at_absolute(
        &mut self,
        _offset: u32,
        _ty: IrType,
    ) -> Result<(), CraneliftError> {
        unsupported("LoadFieldAtAbsolute")
    }

    fn visit_read_string_len(&mut self) -> Result<(), CraneliftError> {
        self.emit_read_string_len()
    }

    // Record construction.
    fn visit_alloc_root_record(&mut self, record_local_idx: u32) -> Result<(), CraneliftError> {
        self.emit_alloc_root_record(record_local_idx);
        Ok(())
    }

    fn visit_alloc_sub_record(
        &mut self,
        record_local_idx: u32,
        root_size: u32,
        root_align: u32,
    ) -> Result<(), CraneliftError> {
        self.emit_alloc_sub_record(record_local_idx, root_size, root_align)
    }

    fn visit_store_field_at_record(
        &mut self,
        record_local_idx: u32,
        offset: u32,
        ty: IrType,
    ) -> Result<(), CraneliftError> {
        self.emit_store_field_at_record(record_local_idx, offset, ty)
    }

    fn visit_push_record_base(&mut self, record_local_idx: u32) -> Result<(), CraneliftError> {
        self.emit_push_record_base(record_local_idx)
    }

    fn visit_emit_tail_record_from_absolute_addr(
        &mut self,
        ty: IrType,
    ) -> Result<(), CraneliftError> {
        self.emit_tail_record_from_absolute(ty)
    }

    // Calls.
    fn visit_call(
        &mut self,
        fn_index: u32,
        arg_count: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), CraneliftError> {
        self.emit_call_stdlib(fn_index, arg_count, param_tys, ret_ty)
    }

    fn visit_call_native(
        &mut self,
        import_idx: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
        cap_bit: u32,
    ) -> Result<(), CraneliftError> {
        self.emit_call_native(import_idx, param_tys, ret_ty, cap_bit)
    }

    fn visit_check_cap(&mut self, cap_bit: u32) -> Result<(), CraneliftError> {
        self.emit_check_cap(cap_bit)
    }

    fn visit_make_closure(
        &mut self,
        fn_table_idx: u32,
        captures: &[ClosureCapture],
        captures_size: u32,
    ) -> Result<(), CraneliftError> {
        self.emit_make_closure(fn_table_idx, captures, captures_size)
    }

    fn visit_call_closure(
        &mut self,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), CraneliftError> {
        self.emit_call_closure(param_tys, ret_ty)
    }

    // Scratch alloc / raw memory.
    fn visit_alloc_scratch(&mut self, size_bytes: u32) -> Result<(), CraneliftError> {
        self.emit_alloc_scratch_static(size_bytes)
    }

    fn visit_alloc_scratch_dyn(&mut self) -> Result<(), CraneliftError> {
        self.emit_alloc_scratch_dyn()
    }

    fn visit_load_i32_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        self.emit_load_i32_at_absolute(offset)
    }

    fn visit_load_i64_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        self.emit_load_i64_at_absolute(offset)
    }

    fn visit_load_i8u_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        self.emit_load_i8u_at_absolute(offset)
    }

    fn visit_load_f64_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        self.emit_load_f64_at_absolute(offset)
    }

    fn visit_store_i32_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        self.emit_store_i32_at_absolute(offset)
    }

    fn visit_store_i64_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        self.emit_store_i64_at_absolute(offset)
    }

    fn visit_store_i8_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        self.emit_store_i8_at_absolute(offset)
    }

    fn visit_store_f64_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        self.emit_store_f64_at_absolute(offset)
    }

    fn visit_memcpy_at_absolute(&mut self) -> Result<(), CraneliftError> {
        self.emit_memcpy_at_absolute()
    }

    // Unicode-aware bundled-stdlib table addresses.
    fn visit_case_fold_table_addr(&mut self, upper: bool) -> Result<(), CraneliftError> {
        let off = if upper {
            self.const_pool.case_fold_upper_offset
        } else {
            self.const_pool.case_fold_lower_offset
        };
        self.emit_const_pool_address(off, "CaseFoldTableAddr")?;
        Ok(())
    }

    fn visit_combining_mark_ranges_addr(&mut self) -> Result<(), CraneliftError> {
        let off = self.const_pool.combining_marks_offset;
        self.emit_const_pool_address(off, "CombiningMarkRangesAddr")?;
        Ok(())
    }

    fn visit_whitespace_ranges_addr(&mut self) -> Result<(), CraneliftError> {
        let off = self.const_pool.whitespace_offset;
        self.emit_const_pool_address(off, "WhitespaceRangesAddr")?;
        Ok(())
    }

    fn visit_decomp_table_addr(&mut self, compatibility: bool) -> Result<(), CraneliftError> {
        let off = if compatibility {
            self.const_pool.decomp_nfkd_offset
        } else {
            self.const_pool.decomp_nfd_offset
        };
        self.emit_const_pool_address(off, "DecompTableAddr")?;
        Ok(())
    }

    fn visit_ccc_table_addr(&mut self) -> Result<(), CraneliftError> {
        let off = self.const_pool.ccc_offset;
        self.emit_const_pool_address(off, "CccTableAddr")?;
        Ok(())
    }

    fn visit_composition_table_addr(&mut self) -> Result<(), CraneliftError> {
        let off = self.const_pool.composition_offset;
        self.emit_const_pool_address(off, "CompositionTableAddr")?;
        Ok(())
    }

    fn visit_full_case_fold_table_addr(&mut self, upper: bool) -> Result<(), CraneliftError> {
        let off = if upper {
            self.const_pool.full_case_fold_upper_offset
        } else {
            self.const_pool.full_case_fold_lower_offset
        };
        self.emit_const_pool_address(off, "FullCaseFoldTableAddr")?;
        Ok(())
    }

    fn visit_cased_ranges_addr(&mut self) -> Result<(), CraneliftError> {
        let off = self.const_pool.cased_ranges_offset;
        self.emit_const_pool_address(off, "CasedRangesAddr")?;
        Ok(())
    }

    fn visit_case_ignorable_ranges_addr(&mut self) -> Result<(), CraneliftError> {
        let off = self.const_pool.case_ignorable_ranges_offset;
        self.emit_const_pool_address(off, "CaseIgnorableRangesAddr")?;
        Ok(())
    }

    fn visit_turkish_case_fold_table_addr(&mut self, upper: bool) -> Result<(), CraneliftError> {
        let off = if upper {
            self.const_pool.turkish_upper_offset
        } else {
            self.const_pool.turkish_lower_offset
        };
        self.emit_const_pool_address(off, "TurkishCaseFoldTableAddr")?;
        Ok(())
    }
}
