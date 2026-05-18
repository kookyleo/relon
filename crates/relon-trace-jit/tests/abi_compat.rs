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

    assert_eq!(TypeId::of::<TraceContext>(), TypeId::of::<abi::TraceContext>());
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
    assert_eq!(TypeId::of::<EffectClass>(), TypeId::of::<abi::EffectClass>());
}
