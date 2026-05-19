//! v6-γ M1 ABI compatibility — `relon_trace_jit` shares `TraceContext`
//! and `DeoptStateSnapshot` types with `relon_trace_abi`.
//!
//! These tests pin the invariant that the runtime crate's public
//! `TraceContext` / `DeoptStateSnapshot` / `RecoverableWriteRecord`
//! re-exports resolve to the exact same Rust types as the shared
//! ABI crate. `TypeId` equality is the strongest check the language
//! offers short of layout-byte assertions; if a future refactor
//! accidentally reintroduces a layout-compatible fork, these tests
//! will fail and reviewers will be forced to reconcile.

use std::any::TypeId;

#[test]
fn abi_types_are_now_shared() {
    use relon_trace_abi as abi;
    use relon_trace_jit::{DeoptStateSnapshot, RecoverableWriteRecord, TraceContext};

    assert_eq!(
        TypeId::of::<TraceContext>(),
        TypeId::of::<abi::TraceContext>()
    );
    assert_eq!(
        TypeId::of::<DeoptStateSnapshot>(),
        TypeId::of::<abi::DeoptStateSnapshot>()
    );
    assert_eq!(
        TypeId::of::<RecoverableWriteRecord>(),
        TypeId::of::<abi::RecoverableWriteRecord>()
    );
}

#[test]
fn external_handles_are_now_shared() {
    use relon_trace_abi as abi;
    use relon_trace_jit::{ExternalAddr, ExternalPc, ExternalSlot};

    assert_eq!(TypeId::of::<ExternalPc>(), TypeId::of::<abi::ExternalPc>());
    assert_eq!(
        TypeId::of::<ExternalSlot>(),
        TypeId::of::<abi::ExternalSlot>()
    );
    assert_eq!(
        TypeId::of::<ExternalAddr>(),
        TypeId::of::<abi::ExternalAddr>()
    );
}

#[test]
fn observed_type_and_effect_class_are_shared() {
    use relon_trace_abi as abi;
    use relon_trace_jit::{EffectClass, ObservedType};

    assert_eq!(
        TypeId::of::<ObservedType>(),
        TypeId::of::<abi::ObservedType>()
    );
    assert_eq!(
        TypeId::of::<EffectClass>(),
        TypeId::of::<abi::EffectClass>()
    );
}

#[test]
fn trace_context_layout_pins_ssa_slots_first() {
    use relon_trace_jit::TraceContext;

    // The emitter assumes `ssa_slots` is at byte offset 0 and
    // `result_slot` follows immediately after the Box fat pointer
    // (16 bytes on every supported target). v6-γ M2+ codegen relies
    // on this invariant; if a reviewer reorders the trace-abi struct
    // this test fires before any IR is emitted.
    let ctx = TraceContext::with_capacity(4);
    let base = (&ctx as *const TraceContext) as usize;
    let ssa_slots_addr = (&ctx.ssa_slots as *const _) as usize;
    let result_slot_addr = (&ctx.result_slot as *const _) as usize;
    assert_eq!(
        ssa_slots_addr - base,
        0,
        "ssa_slots must be at offset 0 (zero-offset load in hot path)"
    );
    assert_eq!(
        result_slot_addr - base,
        16,
        "result_slot must follow the Box<[u64]> fat pointer at byte 16"
    );
}

#[test]
fn deopt_snapshot_apply_round_trips_through_trace_abi() {
    // Exercise the shared `DeoptStateSnapshot::apply(&mut TraceContext)`
    // path that lives in `relon-trace-abi` now. This pins the API
    // surface so subsequent v6-γ milestones don't accidentally fork it
    // back into trace-jit-private code.
    use relon_trace_jit::{DeoptStateSnapshot, RecoverableWriteRecord, TraceContext};

    let mut ctx = TraceContext::with_capacity(3);
    ctx.ssa_slots[0] = 1;
    ctx.ssa_slots[1] = 2;
    ctx.ssa_slots[2] = 3;

    let snap = DeoptStateSnapshot {
        guard_pc: 4,
        external_pc: 0x5000,
        ssa_slots_copy: vec![100u64, 200, 300].into_boxed_slice(),
        recoverable_writes: Vec::<RecoverableWriteRecord>::new(),
        value_stack_copy: Vec::new().into_boxed_slice(),
    };
    // SAFETY: empty recoverable_writes => no raw memory writes.
    unsafe {
        snap.apply(&mut ctx);
    }
    assert_eq!(&*ctx.ssa_slots, &[100u64, 200, 300]);
}
