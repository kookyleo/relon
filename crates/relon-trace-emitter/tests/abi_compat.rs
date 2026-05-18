//! v6-γ M1 ABI compatibility — `relon_trace_emitter` shares its
//! `TraceContext` and `DeoptStateSnapshot` with `relon_trace_abi`.
//!
//! See the matching test in `relon-trace-jit` for the rationale. The
//! emitter half pins the invariant that cranelift IR generated against
//! `relon_trace_emitter::TraceContext` is reading / writing the same
//! struct the runtime helpers in `relon-trace-jit` materialise on
//! guard failure.

use std::any::TypeId;

#[test]
fn abi_types_match_trace_abi() {
    use relon_trace_abi as abi;
    use relon_trace_emitter::{
        DeoptStateSnapshot, EffectClass, ExternalAddr, ExternalPc, ExternalSlot, ObservedType,
        RecoverableWriteRecord, TraceContext,
    };

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
    assert_eq!(TypeId::of::<ExternalPc>(), TypeId::of::<abi::ExternalPc>());
    assert_eq!(
        TypeId::of::<ExternalSlot>(),
        TypeId::of::<abi::ExternalSlot>()
    );
    assert_eq!(
        TypeId::of::<ExternalAddr>(),
        TypeId::of::<abi::ExternalAddr>()
    );
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
fn emitter_and_jit_trace_context_are_the_same_type() {
    // Both `relon_trace_emitter::TraceContext` and
    // `relon_trace_jit::TraceContext` re-export the same
    // `relon_trace_abi::TraceContext`. If a future patch forks the
    // definition this assertion fires.
    assert_eq!(
        TypeId::of::<relon_trace_emitter::TraceContext>(),
        TypeId::of::<relon_trace_jit::TraceContext>(),
    );
}
