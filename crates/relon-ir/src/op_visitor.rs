//! Backend-dispatch visitor over the canonical [`Op`] enum.
//!
//! Each codegen / interpretation backend (bytecode VM,
//! cranelift-native, future targets) historically maintained its own
//! `match op { Op::Add(..) => ..., Op::Sub(..) => ..., ... }` block.
//! With ~70 variants and three backends the duplication risked
//! silent drift: a new variant might be wired in one backend and
//! forgotten in another.
//!
//! [`OpVisitor`] consolidates the surface into a single trait whose
//! methods cover every [`Op`] variant. The trait deliberately ships
//! **without default bodies** — adding a new [`Op`] variant forces
//! every visitor impl to either define the lowering or explicitly
//! mark it `unsupported`. The compiler catches the omission instead
//! of waiting for a differential corpus run to surface it.
//!
//! The driver [`walk_op`] takes a borrowed [`Op`] reference and a
//! mutable visitor, splits the variant once, and forwards each
//! variant's payload to the matching trait method. Backends rarely
//! call [`walk_op`] directly — they wrap their compile / emit state
//! in a struct that implements [`OpVisitor`] and dispatch from their
//! existing per-op driver.
//!
//! ## Design notes
//!
//! * One method per variant. `match Op` style catch-all visitors
//!   would defeat the "force impl on add" point.
//! * `Result<Self::Output, Self::Error>` return type — backends pick
//!   their own emit-value + error types.
//! * Payload fields are passed by reference for owned data (Vec /
//!   String) and by value for `Copy` payloads — no extra boxing or
//!   `dyn` indirection on hot paths.
//! * Nested op-stream payloads (`If` / `Block` / `Loop`) are passed
//!   verbatim; the visitor decides whether to recurse via
//!   [`walk_body`] or interpret the body itself.
//!
//! The trait is intentionally not `Send + Sync`; concrete
//! implementations may capture non-thread-safe builder state.

use ordered_float::OrderedFloat;

use crate::ir::{ClosureCapture, IrType, Op, TaggedOp, TrapKind};

/// Backend-dispatch visitor over [`Op`]. Each method matches exactly
/// one [`Op`] variant; the trait carries no default bodies so adding
/// a variant fails compilation on every impl until it lands a
/// matching method.
///
/// Backends typically wrap their per-function compile / emit state in
/// a struct that implements this trait and rely on [`walk_op`] /
/// [`walk_body`] to drive the dispatch.
pub trait OpVisitor {
    /// Per-method success type. Backends usually pick `()` and
    /// accumulate side-effects against captured state.
    type Output;
    /// Per-method failure type. Backends usually pick their own
    /// `CompileError` / `CodegenError` enum.
    type Error;

    // Constants.
    fn visit_const_bool(&mut self, value: bool) -> Result<Self::Output, Self::Error>;
    fn visit_const_i32(&mut self, value: i32) -> Result<Self::Output, Self::Error>;
    fn visit_const_i64(&mut self, value: i64) -> Result<Self::Output, Self::Error>;
    fn visit_const_f64(&mut self, value: OrderedFloat<f64>) -> Result<Self::Output, Self::Error>;
    fn visit_const_string(&mut self, idx: u32, value: &str) -> Result<Self::Output, Self::Error>;
    fn visit_const_list_int(
        &mut self,
        idx: u32,
        elements: &[i64],
    ) -> Result<Self::Output, Self::Error>;
    fn visit_const_list_float(
        &mut self,
        idx: u32,
        elements: &[u64],
    ) -> Result<Self::Output, Self::Error>;
    fn visit_const_list_bool(
        &mut self,
        idx: u32,
        elements: &[bool],
    ) -> Result<Self::Output, Self::Error>;
    fn visit_const_list_string(
        &mut self,
        idx: u32,
        elements: &[String],
    ) -> Result<Self::Output, Self::Error>;
    /// W5-P1: const `{String -> Int}` dict record. `entries` is in
    /// source declaration order; the layout pass sorts by key bytes.
    fn visit_const_dict(
        &mut self,
        idx: u32,
        entries: &[(String, i64)],
    ) -> Result<Self::Output, Self::Error>;

    // Locals + let-locals.
    fn visit_local_get(&mut self, idx: u32) -> Result<Self::Output, Self::Error>;
    fn visit_let_get(&mut self, idx: u32, ty: IrType) -> Result<Self::Output, Self::Error>;
    fn visit_let_set(&mut self, idx: u32, ty: IrType) -> Result<Self::Output, Self::Error>;

    // Field load / store on the implicit `in_buf` / `out_buf` slots.
    fn visit_load_field(&mut self, offset: u32, ty: IrType) -> Result<Self::Output, Self::Error>;
    fn visit_store_field(&mut self, offset: u32, ty: IrType) -> Result<Self::Output, Self::Error>;

    // Subscript ops (F-D8-B).
    fn visit_dict_get_by_string_key(
        &mut self,
        shape_hash: u64,
        value_ty: IrType,
        entry_count_hint: Option<u32>,
        record_len_hint: Option<u32>,
    ) -> Result<Self::Output, Self::Error>;
    fn visit_list_get_by_int_idx(
        &mut self,
        element_ty: IrType,
    ) -> Result<Self::Output, Self::Error>;

    // Arithmetic / bitwise.
    fn visit_add(&mut self, ty: IrType) -> Result<Self::Output, Self::Error>;
    /// #165 — single-allocation N-operand `String` concat. Pops
    /// `operand_count` String values off the operand stack (deepest
    /// leaf first), pushes one concatenated String. Backends decide
    /// how to fulfil "single allocation"; the tree-walker delegates to
    /// `SmolStr::concat_many`, the bytecode VM materialises a single
    /// fresh arena slot, and cranelift sums the lengths once before
    /// the alloc.
    fn visit_str_concat_n(&mut self, operand_count: u32) -> Result<Self::Output, Self::Error>;
    fn visit_sub(&mut self, ty: IrType) -> Result<Self::Output, Self::Error>;
    fn visit_mul(&mut self, ty: IrType) -> Result<Self::Output, Self::Error>;
    fn visit_div(&mut self, ty: IrType) -> Result<Self::Output, Self::Error>;
    fn visit_mod_(&mut self, ty: IrType) -> Result<Self::Output, Self::Error>;
    fn visit_bit_and(&mut self, ty: IrType) -> Result<Self::Output, Self::Error>;
    /// #359: signed-int → float promotion (`Op::ConvertI64ToF64`). Pop
    /// one `I64`-typed value, push its `F64`-typed `sitofp` widening.
    /// Mirrors the tree-walker's `as_f64()` Int promotion so mixed
    /// `Int`/`Float` arithmetic compiles bit-identically.
    fn visit_convert_i64_to_f64(&mut self) -> Result<Self::Output, Self::Error>;

    // Comparison.
    fn visit_eq(&mut self, ty: IrType) -> Result<Self::Output, Self::Error>;
    fn visit_ne(&mut self, ty: IrType) -> Result<Self::Output, Self::Error>;
    fn visit_lt(&mut self, ty: IrType) -> Result<Self::Output, Self::Error>;
    fn visit_le(&mut self, ty: IrType) -> Result<Self::Output, Self::Error>;
    fn visit_gt(&mut self, ty: IrType) -> Result<Self::Output, Self::Error>;
    fn visit_ge(&mut self, ty: IrType) -> Result<Self::Output, Self::Error>;

    // Control flow.
    fn visit_if(
        &mut self,
        result_ty: IrType,
        then_body: &[TaggedOp],
        else_body: &[TaggedOp],
    ) -> Result<Self::Output, Self::Error>;
    fn visit_block(
        &mut self,
        result_ty: Option<IrType>,
        body: &[TaggedOp],
    ) -> Result<Self::Output, Self::Error>;
    fn visit_loop_(
        &mut self,
        result_ty: Option<IrType>,
        body: &[TaggedOp],
    ) -> Result<Self::Output, Self::Error>;
    fn visit_br(&mut self, label_depth: u32) -> Result<Self::Output, Self::Error>;
    fn visit_br_if(&mut self, label_depth: u32) -> Result<Self::Output, Self::Error>;
    fn visit_br_table(
        &mut self,
        default: u32,
        targets: &[u32],
    ) -> Result<Self::Output, Self::Error>;
    fn visit_return(&mut self) -> Result<Self::Output, Self::Error>;
    fn visit_select(&mut self, ty: IrType) -> Result<Self::Output, Self::Error>;
    fn visit_trap(&mut self, kind: TrapKind) -> Result<Self::Output, Self::Error>;

    // Pointer / list loads from the in_buf.
    fn visit_load_string_ptr(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_load_list_int_ptr(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_load_list_float_ptr(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_load_list_bool_ptr(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_load_list_string_ptr(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_load_list_schema_ptr(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_load_schema_ptr(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_load_field_at_absolute(
        &mut self,
        offset: u32,
        ty: IrType,
    ) -> Result<Self::Output, Self::Error>;
    fn visit_read_string_len(&mut self) -> Result<Self::Output, Self::Error>;

    // Record construction.
    fn visit_alloc_root_record(
        &mut self,
        record_local_idx: u32,
    ) -> Result<Self::Output, Self::Error>;
    fn visit_alloc_sub_record(
        &mut self,
        record_local_idx: u32,
        root_size: u32,
        root_align: u32,
    ) -> Result<Self::Output, Self::Error>;
    fn visit_store_field_at_record(
        &mut self,
        record_local_idx: u32,
        offset: u32,
        ty: IrType,
    ) -> Result<Self::Output, Self::Error>;
    fn visit_push_record_base(
        &mut self,
        record_local_idx: u32,
    ) -> Result<Self::Output, Self::Error>;
    fn visit_emit_tail_record_from_absolute_addr(
        &mut self,
        ty: IrType,
    ) -> Result<Self::Output, Self::Error>;

    // Calls.
    fn visit_call(
        &mut self,
        fn_index: u32,
        arg_count: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<Self::Output, Self::Error>;
    fn visit_call_native(
        &mut self,
        import_idx: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
        cap_bit: u32,
    ) -> Result<Self::Output, Self::Error>;
    fn visit_check_cap(&mut self, cap_bit: u32) -> Result<Self::Output, Self::Error>;
    /// Built-in clock primitive (`Op::ReadClock`). Pushes one `I64`
    /// nanosecond wall-clock reading. See the op doc for the per-target
    /// lowering (native host helper vs standard WASI `clock_time_get`).
    fn visit_read_clock(&mut self) -> Result<Self::Output, Self::Error>;
    /// Built-in random primitive (`Op::ReadRandom`). Pushes one `I64`
    /// of fresh random bytes. See the op doc for the per-target
    /// lowering (native host helper vs standard WASI `random_get`).
    fn visit_read_random(&mut self) -> Result<Self::Output, Self::Error>;
    /// Built-in filesystem read primitive (`Op::ReadFile`). Pops one
    /// `String` (the path) and pushes one `String` (the file contents).
    /// See the op doc for the per-target lowering (native host helper
    /// vs standard WASI `path_open`/`fd_read`/`fd_close`).
    fn visit_read_file(&mut self) -> Result<Self::Output, Self::Error>;
    /// Built-in directory listing primitive (`Op::ReadDir`). Pops one
    /// `String` (the path) and pushes one `List<String>` (the sorted
    /// entry file names). See the op doc for the per-target lowering
    /// (native host helper; wasm32 not yet implemented).
    fn visit_read_dir(&mut self) -> Result<Self::Output, Self::Error>;
    fn visit_make_closure(
        &mut self,
        fn_table_idx: u32,
        captures: &[ClosureCapture],
        captures_size: u32,
    ) -> Result<Self::Output, Self::Error>;
    fn visit_call_closure(
        &mut self,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<Self::Output, Self::Error>;

    // Scratch alloc / raw memory.
    fn visit_alloc_scratch(&mut self, size_bytes: u32) -> Result<Self::Output, Self::Error>;
    fn visit_alloc_scratch_dyn(&mut self) -> Result<Self::Output, Self::Error>;
    fn visit_load_i32_at_absolute(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_load_i64_at_absolute(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_load_i8u_at_absolute(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_load_f64_at_absolute(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_store_i32_at_absolute(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_store_i64_at_absolute(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_store_i8_at_absolute(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_store_f64_at_absolute(&mut self, offset: u32) -> Result<Self::Output, Self::Error>;
    fn visit_memcpy_at_absolute(&mut self) -> Result<Self::Output, Self::Error>;

    // Unicode-aware bundled-stdlib table addresses.
    fn visit_case_fold_table_addr(&mut self, upper: bool) -> Result<Self::Output, Self::Error>;
    fn visit_combining_mark_ranges_addr(&mut self) -> Result<Self::Output, Self::Error>;
    fn visit_whitespace_ranges_addr(&mut self) -> Result<Self::Output, Self::Error>;
    fn visit_decomp_table_addr(&mut self, compatibility: bool)
        -> Result<Self::Output, Self::Error>;
    fn visit_ccc_table_addr(&mut self) -> Result<Self::Output, Self::Error>;
    fn visit_composition_table_addr(&mut self) -> Result<Self::Output, Self::Error>;
    fn visit_full_case_fold_table_addr(&mut self, upper: bool)
        -> Result<Self::Output, Self::Error>;
    fn visit_cased_ranges_addr(&mut self) -> Result<Self::Output, Self::Error>;
    fn visit_case_ignorable_ranges_addr(&mut self) -> Result<Self::Output, Self::Error>;
    fn visit_turkish_case_fold_table_addr(
        &mut self,
        upper: bool,
    ) -> Result<Self::Output, Self::Error>;
}

/// Dispatch a single [`Op`] through the visitor. Exhaustive match —
/// the compiler refuses to build this function when [`Op`] gains a
/// new variant without a matching method on [`OpVisitor`].
///
/// Backends typically wrap this in a thin per-op driver that also
/// records source positions, label bookkeeping, etc.
pub fn walk_op<V: OpVisitor>(op: &Op, visitor: &mut V) -> Result<V::Output, V::Error> {
    match op {
        Op::ConstBool(v) => visitor.visit_const_bool(*v),
        Op::ConstI32(v) => visitor.visit_const_i32(*v),
        Op::ConstI64(v) => visitor.visit_const_i64(*v),
        Op::ConstF64(v) => visitor.visit_const_f64(*v),
        Op::ConstString { idx, value } => visitor.visit_const_string(*idx, value),
        Op::ConstListInt { idx, elements } => visitor.visit_const_list_int(*idx, elements),
        Op::ConstListFloat { idx, elements } => visitor.visit_const_list_float(*idx, elements),
        Op::ConstListBool { idx, elements } => visitor.visit_const_list_bool(*idx, elements),
        Op::ConstListString { idx, elements } => visitor.visit_const_list_string(*idx, elements),
        Op::ConstDict { idx, entries } => visitor.visit_const_dict(*idx, entries),
        Op::LocalGet(idx) => visitor.visit_local_get(*idx),
        Op::LetGet { idx, ty } => visitor.visit_let_get(*idx, *ty),
        Op::LetSet { idx, ty } => visitor.visit_let_set(*idx, *ty),
        Op::LoadField { offset, ty } => visitor.visit_load_field(*offset, *ty),
        Op::StoreField { offset, ty } => visitor.visit_store_field(*offset, *ty),
        Op::DictGetByStringKey {
            shape_hash,
            value_ty,
            entry_count_hint,
            record_len_hint,
        } => visitor.visit_dict_get_by_string_key(
            *shape_hash,
            *value_ty,
            *entry_count_hint,
            *record_len_hint,
        ),
        Op::ListGetByIntIdx { element_ty } => visitor.visit_list_get_by_int_idx(*element_ty),
        Op::Add(ty) => visitor.visit_add(*ty),
        Op::StrConcatN { operand_count } => visitor.visit_str_concat_n(*operand_count),
        Op::Sub(ty) => visitor.visit_sub(*ty),
        Op::Mul(ty) => visitor.visit_mul(*ty),
        Op::Div(ty) => visitor.visit_div(*ty),
        Op::Mod(ty) => visitor.visit_mod_(*ty),
        Op::BitAnd(ty) => visitor.visit_bit_and(*ty),
        Op::ConvertI64ToF64 => visitor.visit_convert_i64_to_f64(),
        Op::Eq(ty) => visitor.visit_eq(*ty),
        Op::Ne(ty) => visitor.visit_ne(*ty),
        Op::Lt(ty) => visitor.visit_lt(*ty),
        Op::Le(ty) => visitor.visit_le(*ty),
        Op::Gt(ty) => visitor.visit_gt(*ty),
        Op::Ge(ty) => visitor.visit_ge(*ty),
        Op::If {
            result_ty,
            then_body,
            else_body,
        } => visitor.visit_if(*result_ty, then_body, else_body),
        Op::LoadStringPtr { offset } => visitor.visit_load_string_ptr(*offset),
        Op::LoadListIntPtr { offset } => visitor.visit_load_list_int_ptr(*offset),
        Op::LoadListFloatPtr { offset } => visitor.visit_load_list_float_ptr(*offset),
        Op::LoadListBoolPtr { offset } => visitor.visit_load_list_bool_ptr(*offset),
        Op::LoadListStringPtr { offset } => visitor.visit_load_list_string_ptr(*offset),
        Op::LoadListSchemaPtr { offset } => visitor.visit_load_list_schema_ptr(*offset),
        Op::Return => visitor.visit_return(),
        Op::AllocRootRecord { record_local_idx } => {
            visitor.visit_alloc_root_record(*record_local_idx)
        }
        Op::AllocSubRecord {
            record_local_idx,
            root_size,
            root_align,
        } => visitor.visit_alloc_sub_record(*record_local_idx, *root_size, *root_align),
        Op::StoreFieldAtRecord {
            record_local_idx,
            offset,
            ty,
        } => visitor.visit_store_field_at_record(*record_local_idx, *offset, *ty),
        Op::PushRecordBase { record_local_idx } => {
            visitor.visit_push_record_base(*record_local_idx)
        }
        Op::EmitTailRecordFromAbsoluteAddr { ty } => {
            visitor.visit_emit_tail_record_from_absolute_addr(*ty)
        }
        Op::Call {
            fn_index,
            arg_count,
            param_tys,
            ret_ty,
        } => visitor.visit_call(*fn_index, *arg_count, param_tys, *ret_ty),
        Op::ReadStringLen => visitor.visit_read_string_len(),
        Op::Select { ty } => visitor.visit_select(*ty),
        Op::LoadFieldAtAbsolute { offset, ty } => {
            visitor.visit_load_field_at_absolute(*offset, *ty)
        }
        Op::LoadSchemaPtr { offset } => visitor.visit_load_schema_ptr(*offset),
        Op::CallNative {
            import_idx,
            param_tys,
            ret_ty,
            cap_bit,
        } => visitor.visit_call_native(*import_idx, param_tys, *ret_ty, *cap_bit),
        Op::CheckCap { cap_bit } => visitor.visit_check_cap(*cap_bit),
        Op::ReadClock => visitor.visit_read_clock(),
        Op::ReadRandom => visitor.visit_read_random(),
        Op::ReadFile => visitor.visit_read_file(),
        Op::ReadDir => visitor.visit_read_dir(),
        Op::Block { result_ty, body } => visitor.visit_block(*result_ty, body),
        Op::Loop { result_ty, body } => visitor.visit_loop_(*result_ty, body),
        Op::Br { label_depth } => visitor.visit_br(*label_depth),
        Op::BrIf { label_depth } => visitor.visit_br_if(*label_depth),
        Op::BrTable { default, targets } => visitor.visit_br_table(*default, targets),
        Op::AllocScratch { size_bytes } => visitor.visit_alloc_scratch(*size_bytes),
        Op::AllocScratchDyn => visitor.visit_alloc_scratch_dyn(),
        Op::LoadI32AtAbsolute { offset } => visitor.visit_load_i32_at_absolute(*offset),
        Op::LoadI64AtAbsolute { offset } => visitor.visit_load_i64_at_absolute(*offset),
        Op::StoreI32AtAbsolute { offset } => visitor.visit_store_i32_at_absolute(*offset),
        Op::StoreI64AtAbsolute { offset } => visitor.visit_store_i64_at_absolute(*offset),
        Op::MemcpyAtAbsolute => visitor.visit_memcpy_at_absolute(),
        Op::LoadI8UAtAbsolute { offset } => visitor.visit_load_i8u_at_absolute(*offset),
        Op::StoreI8AtAbsolute { offset } => visitor.visit_store_i8_at_absolute(*offset),
        Op::LoadF64AtAbsolute { offset } => visitor.visit_load_f64_at_absolute(*offset),
        Op::StoreF64AtAbsolute { offset } => visitor.visit_store_f64_at_absolute(*offset),
        Op::Trap { kind } => visitor.visit_trap(*kind),
        Op::MakeClosure {
            fn_table_idx,
            captures,
            captures_size,
        } => visitor.visit_make_closure(*fn_table_idx, captures, *captures_size),
        Op::CallClosure { param_tys, ret_ty } => visitor.visit_call_closure(param_tys, *ret_ty),
        Op::CaseFoldTableAddr { upper } => visitor.visit_case_fold_table_addr(*upper),
        Op::CombiningMarkRangesAddr => visitor.visit_combining_mark_ranges_addr(),
        Op::WhitespaceRangesAddr => visitor.visit_whitespace_ranges_addr(),
        Op::DecompTableAddr { compatibility } => visitor.visit_decomp_table_addr(*compatibility),
        Op::CccTableAddr => visitor.visit_ccc_table_addr(),
        Op::CompositionTableAddr => visitor.visit_composition_table_addr(),
        Op::FullCaseFoldTableAddr { upper } => visitor.visit_full_case_fold_table_addr(*upper),
        Op::CasedRangesAddr => visitor.visit_cased_ranges_addr(),
        Op::CaseIgnorableRangesAddr => visitor.visit_case_ignorable_ranges_addr(),
        Op::TurkishCaseFoldTableAddr { upper } => {
            visitor.visit_turkish_case_fold_table_addr(*upper)
        }
    }
}

/// Helper: drive [`walk_op`] over each [`TaggedOp`] in `body`, in
/// order. Stops at the first error. Visitors that need to track the
/// per-op `TokenRange` should walk the body themselves and pull the
/// `range` field before invoking [`walk_op`].
pub fn walk_body<V: OpVisitor>(
    body: &[TaggedOp],
    visitor: &mut V,
) -> Result<Vec<V::Output>, V::Error> {
    let mut out = Vec::with_capacity(body.len());
    for tagged in body {
        out.push(walk_op(&tagged.op, visitor)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trivial counting visitor used to exercise the driver dispatch
    /// over every variant. The unit test below builds an `Op` of each
    /// variant and asserts the visitor recorded a call.
    #[derive(Default)]
    struct CountingVisitor {
        calls: u32,
        last: &'static str,
    }

    impl OpVisitor for CountingVisitor {
        type Output = ();
        type Error = ();

        fn visit_const_bool(&mut self, _v: bool) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ConstBool";
            Ok(())
        }
        fn visit_const_i32(&mut self, _v: i32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ConstI32";
            Ok(())
        }
        fn visit_const_i64(&mut self, _v: i64) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ConstI64";
            Ok(())
        }
        fn visit_const_f64(&mut self, _v: OrderedFloat<f64>) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ConstF64";
            Ok(())
        }
        fn visit_const_string(&mut self, _: u32, _: &str) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ConstString";
            Ok(())
        }
        fn visit_const_list_int(&mut self, _: u32, _: &[i64]) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ConstListInt";
            Ok(())
        }
        fn visit_const_list_float(&mut self, _: u32, _: &[u64]) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ConstListFloat";
            Ok(())
        }
        fn visit_const_list_bool(&mut self, _: u32, _: &[bool]) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ConstListBool";
            Ok(())
        }
        fn visit_const_list_string(&mut self, _: u32, _: &[String]) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ConstListString";
            Ok(())
        }
        fn visit_const_dict(&mut self, _: u32, _: &[(String, i64)]) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ConstDict";
            Ok(())
        }
        fn visit_local_get(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LocalGet";
            Ok(())
        }
        fn visit_let_get(&mut self, _: u32, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LetGet";
            Ok(())
        }
        fn visit_let_set(&mut self, _: u32, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LetSet";
            Ok(())
        }
        fn visit_load_field(&mut self, _: u32, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LoadField";
            Ok(())
        }
        fn visit_store_field(&mut self, _: u32, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "StoreField";
            Ok(())
        }
        fn visit_dict_get_by_string_key(
            &mut self,
            _: u64,
            _: IrType,
            _: Option<u32>,
            _: Option<u32>,
        ) -> Result<(), ()> {
            self.calls += 1;
            self.last = "DictGetByStringKey";
            Ok(())
        }
        fn visit_list_get_by_int_idx(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ListGetByIntIdx";
            Ok(())
        }
        fn visit_add(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Add";
            Ok(())
        }
        fn visit_str_concat_n(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "StrConcatN";
            Ok(())
        }
        fn visit_sub(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Sub";
            Ok(())
        }
        fn visit_mul(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Mul";
            Ok(())
        }
        fn visit_div(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Div";
            Ok(())
        }
        fn visit_mod_(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Mod";
            Ok(())
        }
        fn visit_bit_and(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "BitAnd";
            Ok(())
        }
        fn visit_convert_i64_to_f64(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ConvertI64ToF64";
            Ok(())
        }
        fn visit_eq(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Eq";
            Ok(())
        }
        fn visit_ne(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Ne";
            Ok(())
        }
        fn visit_lt(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Lt";
            Ok(())
        }
        fn visit_le(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Le";
            Ok(())
        }
        fn visit_gt(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Gt";
            Ok(())
        }
        fn visit_ge(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Ge";
            Ok(())
        }
        fn visit_if(&mut self, _: IrType, _: &[TaggedOp], _: &[TaggedOp]) -> Result<(), ()> {
            self.calls += 1;
            self.last = "If";
            Ok(())
        }
        fn visit_block(&mut self, _: Option<IrType>, _: &[TaggedOp]) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Block";
            Ok(())
        }
        fn visit_loop_(&mut self, _: Option<IrType>, _: &[TaggedOp]) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Loop";
            Ok(())
        }
        fn visit_br(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Br";
            Ok(())
        }
        fn visit_br_if(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "BrIf";
            Ok(())
        }
        fn visit_br_table(&mut self, _: u32, _: &[u32]) -> Result<(), ()> {
            self.calls += 1;
            self.last = "BrTable";
            Ok(())
        }
        fn visit_return(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Return";
            Ok(())
        }
        fn visit_select(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Select";
            Ok(())
        }
        fn visit_trap(&mut self, _: TrapKind) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Trap";
            Ok(())
        }
        fn visit_load_string_ptr(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LoadStringPtr";
            Ok(())
        }
        fn visit_load_list_int_ptr(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LoadListIntPtr";
            Ok(())
        }
        fn visit_load_list_float_ptr(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LoadListFloatPtr";
            Ok(())
        }
        fn visit_load_list_bool_ptr(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LoadListBoolPtr";
            Ok(())
        }
        fn visit_load_list_string_ptr(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LoadListStringPtr";
            Ok(())
        }
        fn visit_load_list_schema_ptr(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LoadListSchemaPtr";
            Ok(())
        }
        fn visit_load_schema_ptr(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LoadSchemaPtr";
            Ok(())
        }
        fn visit_load_field_at_absolute(&mut self, _: u32, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LoadFieldAtAbsolute";
            Ok(())
        }
        fn visit_read_string_len(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ReadStringLen";
            Ok(())
        }
        fn visit_alloc_root_record(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "AllocRootRecord";
            Ok(())
        }
        fn visit_alloc_sub_record(&mut self, _: u32, _: u32, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "AllocSubRecord";
            Ok(())
        }
        fn visit_store_field_at_record(&mut self, _: u32, _: u32, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "StoreFieldAtRecord";
            Ok(())
        }
        fn visit_push_record_base(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "PushRecordBase";
            Ok(())
        }
        fn visit_emit_tail_record_from_absolute_addr(&mut self, _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "EmitTailRecordFromAbsoluteAddr";
            Ok(())
        }
        fn visit_call(&mut self, _: u32, _: u32, _: &[IrType], _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "Call";
            Ok(())
        }
        fn visit_call_native(&mut self, _: u32, _: &[IrType], _: IrType, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "CallNative";
            Ok(())
        }
        fn visit_check_cap(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "CheckCap";
            Ok(())
        }
        fn visit_read_clock(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ReadClock";
            Ok(())
        }
        fn visit_read_random(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ReadRandom";
            Ok(())
        }
        fn visit_read_file(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ReadFile";
            Ok(())
        }
        fn visit_read_dir(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "ReadDir";
            Ok(())
        }
        fn visit_make_closure(&mut self, _: u32, _: &[ClosureCapture], _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "MakeClosure";
            Ok(())
        }
        fn visit_call_closure(&mut self, _: &[IrType], _: IrType) -> Result<(), ()> {
            self.calls += 1;
            self.last = "CallClosure";
            Ok(())
        }
        fn visit_alloc_scratch(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "AllocScratch";
            Ok(())
        }
        fn visit_alloc_scratch_dyn(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "AllocScratchDyn";
            Ok(())
        }
        fn visit_load_i32_at_absolute(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LoadI32AtAbsolute";
            Ok(())
        }
        fn visit_load_i64_at_absolute(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LoadI64AtAbsolute";
            Ok(())
        }
        fn visit_load_i8u_at_absolute(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LoadI8UAtAbsolute";
            Ok(())
        }
        fn visit_load_f64_at_absolute(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "LoadF64AtAbsolute";
            Ok(())
        }
        fn visit_store_i32_at_absolute(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "StoreI32AtAbsolute";
            Ok(())
        }
        fn visit_store_i64_at_absolute(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "StoreI64AtAbsolute";
            Ok(())
        }
        fn visit_store_i8_at_absolute(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "StoreI8AtAbsolute";
            Ok(())
        }
        fn visit_store_f64_at_absolute(&mut self, _: u32) -> Result<(), ()> {
            self.calls += 1;
            self.last = "StoreF64AtAbsolute";
            Ok(())
        }
        fn visit_memcpy_at_absolute(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "MemcpyAtAbsolute";
            Ok(())
        }
        fn visit_case_fold_table_addr(&mut self, _: bool) -> Result<(), ()> {
            self.calls += 1;
            self.last = "CaseFoldTableAddr";
            Ok(())
        }
        fn visit_combining_mark_ranges_addr(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "CombiningMarkRangesAddr";
            Ok(())
        }
        fn visit_whitespace_ranges_addr(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "WhitespaceRangesAddr";
            Ok(())
        }
        fn visit_decomp_table_addr(&mut self, _: bool) -> Result<(), ()> {
            self.calls += 1;
            self.last = "DecompTableAddr";
            Ok(())
        }
        fn visit_ccc_table_addr(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "CccTableAddr";
            Ok(())
        }
        fn visit_composition_table_addr(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "CompositionTableAddr";
            Ok(())
        }
        fn visit_full_case_fold_table_addr(&mut self, _: bool) -> Result<(), ()> {
            self.calls += 1;
            self.last = "FullCaseFoldTableAddr";
            Ok(())
        }
        fn visit_cased_ranges_addr(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "CasedRangesAddr";
            Ok(())
        }
        fn visit_case_ignorable_ranges_addr(&mut self) -> Result<(), ()> {
            self.calls += 1;
            self.last = "CaseIgnorableRangesAddr";
            Ok(())
        }
        fn visit_turkish_case_fold_table_addr(&mut self, _: bool) -> Result<(), ()> {
            self.calls += 1;
            self.last = "TurkishCaseFoldTableAddr";
            Ok(())
        }
    }

    #[test]
    fn dispatch_routes_const_i64_to_matching_method() {
        let op = Op::ConstI64(42);
        let mut v = CountingVisitor::default();
        walk_op(&op, &mut v).unwrap();
        assert_eq!(v.calls, 1);
        assert_eq!(v.last, "ConstI64");
    }

    #[test]
    fn dispatch_routes_add_to_visit_add() {
        let op = Op::Add(IrType::I64);
        let mut v = CountingVisitor::default();
        walk_op(&op, &mut v).unwrap();
        assert_eq!(v.last, "Add");
    }

    #[test]
    fn dispatch_routes_str_concat_n_to_matching_method() {
        let op = Op::StrConcatN { operand_count: 4 };
        let mut v = CountingVisitor::default();
        walk_op(&op, &mut v).unwrap();
        assert_eq!(v.last, "StrConcatN");
    }

    #[test]
    fn dispatch_routes_call_native_payload() {
        let op = Op::CallNative {
            import_idx: 3,
            param_tys: vec![IrType::I64],
            ret_ty: IrType::I64,
            cap_bit: crate::ir::NO_CAPABILITY_BIT,
        };
        let mut v = CountingVisitor::default();
        walk_op(&op, &mut v).unwrap();
        assert_eq!(v.last, "CallNative");
    }
}
