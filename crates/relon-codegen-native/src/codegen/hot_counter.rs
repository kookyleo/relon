//! v6-γ M2/M5: HotCounter prologue injection for the cranelift JIT
//! entry function.
//!
//! The lowering pipeline (`compile_module_with`) calls
//! [`emit_hot_counter_inject`] right after the entry block is
//! materialised and its function parameters have been extracted. The
//! helper creates two new blocks (`hot_block` / `normal_block`),
//! branches between them, fills the hot path with a
//! `__relon_jump_to_recorder` call + sentinel return, and leaves the
//! builder positioned on `normal_block` so the rest of the entry
//! codegen flows unchanged.
//!
//! The prologue is intentionally non-atomic: the counter store / load
//! pair is `MemFlags::trusted()` and the threshold check uses
//! `icmp_imm`. Races on the counter can over-count by a small bounded
//! amount, which is acceptable for the recorder kick-off heuristic.
//!
//! This module is a sibling of the other codegen sub-files
//! (`arith` / `call` / `closure` / `control_flow` / `field` /
//! `record`) and follows the same convention: it owns its own
//! cranelift imports and the lowering helper, while the surrounding
//! `Codegen` state stays in [`super`].

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{
    AbiParam, BlockArg, InstBuilder, MemFlags, Signature, StackSlotData, StackSlotKind,
    Value as CValue,
};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::FunctionBuilder;

use super::EntryShape;

/// v6-γ M2: emit a HotCounter prologue at the current entry block.
///
/// On entry the builder must already be positioned at a freshly-built
/// entry block whose function-param values have been extracted. On
/// return the builder is positioned at a sealed `normal_block` that
/// continues the original entry-block control flow; the hot path
/// branches to a sealed `hot_block` that calls
/// `__relon_jump_to_recorder` and returns a sentinel zero.
///
/// IR shape (`pointer_ty == I64`):
///
/// ```text
/// entry_block:
///     %base    = iconst.i64 <hot_counters_base()>
///     %slot    = iadd_imm %base, fn_id * 4
///     %v       = load.i32 trusted %slot
///     %v1      = iadd_imm.i32 %v, 1
///                store.i32 trusted %v1, %slot
///     %hot     = icmp_imm.i32 uge %v1, RELON_HOT_THRESHOLD
///     brif %hot, hot_block, normal_block
///
/// hot_block:
///     %fn_id_const = iconst.i32 fn_id
///     %args_ptr    = iconst.i64 0    ; v6-γ M2: helper ignores arg ptr
///     call_indirect (sig=jump_sig) %jump_ptr (%fn_id_const, %args_ptr)
///     return  <zero of entry return ty>
///
/// normal_block:
///     ;; existing entry-block continuation
/// ```
pub(super) fn emit_hot_counter_inject(
    builder: &mut FunctionBuilder<'_>,
    pointer_ty: cranelift_codegen::ir::Type,
    entry_shape: EntryShape,
    fn_id: u32,
    arg_values: &[CValue],
) {
    let hot_block = builder.create_block();
    let normal_block = builder.create_block();

    // Counter slot address = base + fn_id * sizeof(u32).
    let base_addr = crate::trace_install::hot_counters_base() as i64;
    let slot_offset = (fn_id as i64) * 4;
    let counter_addr = base_addr.wrapping_add(slot_offset);
    let counter_ptr = builder.ins().iconst(pointer_ty, counter_addr);

    // load.i32 / iadd_imm.i32 / store.i32 (non-atomic per design).
    let cur = builder.ins().load(I32, MemFlags::trusted(), counter_ptr, 0);
    let inc = builder.ins().iadd_imm(cur, 1);
    builder
        .ins()
        .store(MemFlags::trusted(), inc, counter_ptr, 0);

    // icmp uge against the threshold; branch on the result.
    let hot = builder.ins().icmp_imm(
        IntCC::UnsignedGreaterThanOrEqual,
        inc,
        crate::trace_install::RELON_HOT_THRESHOLD as i64,
    );
    let empty: [BlockArg; 0] = [];
    builder
        .ins()
        .brif(hot, hot_block, empty.iter(), normal_block, empty.iter());

    // Fill the hot block: call the recorder jump helper, then return a
    // sentinel zero of the entry's return type. The helper is invoked
    // by raw fn pointer (iconst -> call_indirect) so we don't have to
    // declare an external symbol on the per-fn cranelift module.
    builder.switch_to_block(hot_block);
    builder.seal_block(hot_block);
    let fn_id_val = builder.ins().iconst(I32, fn_id as i64);

    // v6-γ M5: pack the entry's runtime arg values into a
    // stack-allocated `u64[]` and pass the address to the helper.
    // Earlier stages passed `null` here; the recorder then drove the
    // walker with zeroed slots, which made guard-laden ops abort
    // immediately because the IR walker had no real type
    // observations to feed the recorder. With real args the walker
    // pulls the live values via `LocalGet(idx)` and the recorder
    // sees concrete types, which is what M5's corpus harness depends
    // on. Each arg is widened to i64 — narrower args (i32 / bool /
    // ptr) are extended with `uextend` or stored at i32 width and
    // then i64-zeroed by the slot's prior init.
    let args_ptr_val = if arg_values.is_empty() {
        builder.ins().iconst(pointer_ty, 0)
    } else {
        let slot_count = arg_values.len();
        let slot_bytes = (slot_count as u32) * 8;
        let stack_slot = builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            slot_bytes,
            3, // 8-byte align (2^3); i64 store needs it.
        ));
        for (i, v) in arg_values.iter().enumerate() {
            // Widen any narrower-than-i64 arg to i64. cranelift's
            // `uextend` accepts the actual underlying width without
            // needing us to plumb the IR type here.
            let widened = match builder.func.dfg.value_type(*v) {
                t if t == I64 => *v,
                t if t == I32 => builder.ins().uextend(I64, *v),
                t => {
                    // Floats / bool / pointer: bitcast through an
                    // ireduce/uextend chain into i64. For F64 we
                    // bitcast to I64 directly; for boolean / i32 we
                    // uextend. Anything else is unexpected for the
                    // entry shapes we support today, so we
                    // conservatively spill a zero so the slot stays
                    // a valid u64.
                    if t == cranelift_codegen::ir::types::F64 {
                        builder.ins().bitcast(I64, MemFlags::trusted(), *v)
                    } else {
                        builder.ins().iconst(I64, 0)
                    }
                }
            };
            builder
                .ins()
                .stack_store(widened, stack_slot, (i as i32) * 8);
        }
        builder.ins().stack_addr(pointer_ty, stack_slot, 0)
    };

    let mut jump_sig = Signature::new(CallConv::SystemV);
    jump_sig.params.push(AbiParam::new(I32));
    jump_sig.params.push(AbiParam::new(pointer_ty));
    let jump_sig_ref = builder.import_signature(jump_sig);
    let jump_target = builder.ins().iconst(
        pointer_ty,
        crate::trace_install::__relon_jump_to_recorder as *const () as i64,
    );
    builder
        .ins()
        .call_indirect(jump_sig_ref, jump_target, &[fn_id_val, args_ptr_val]);
    let zero = match entry_shape {
        EntryShape::LegacyI64Args => builder.ins().iconst(I64, 0),
        EntryShape::BufferProtocol => builder.ins().iconst(I32, 0),
    };
    builder.ins().return_(&[zero]);

    // Continue with the normal block.
    builder.switch_to_block(normal_block);
    builder.seal_block(normal_block);
}
