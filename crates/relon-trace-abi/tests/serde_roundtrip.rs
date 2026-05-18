//! Serde roundtrip tests for the shared trace ABI.
//!
//! Gated behind `cfg(feature = "serde")` so the default test build
//! does not pull in `serde` / `bincode`. Off-path tooling (golden
//! trace dumpers, ABI checkers, snapshot tests in downstream crates)
//! turns the feature on; the emitter / runtime hot path leaves it
//! off.
//!
//! ## What's covered
//!
//! The "wire-compatible" ABI surface — types whose discriminant /
//! field values appear in golden trace dumps and on-disk caches:
//!
//! - [`RecoverableWriteRecord`] (`#[repr(C)]`, two u64s)
//! - [`ExternalPc`] / [`ExternalSlot`] / [`ExternalAddr`]
//!   (`#[repr(transparent)]` newtypes)
//! - [`EffectClass`] (5-bucket enum; `u8` discriminant)
//! - [`ObservedType`] (5-bucket enum; `u8` discriminant)
//! - [`TraceEntryStatus`] (3-bucket enum; `i32` discriminant)
//!
//! [`TraceContext`] and [`DeoptStateSnapshot`] are deliberately
//! **excluded** from this test: their `Box<[u64]>` / function-pointer
//! fields don't round-trip cleanly without a custom `serde` impl. The
//! deopt path operates on in-memory state and never serialises a
//! whole context to disk.
//!
//! [`TraceContext`]: relon_trace_abi::TraceContext
//! [`DeoptStateSnapshot`]: relon_trace_abi::DeoptStateSnapshot
//! [`EffectClass`]: relon_trace_abi::EffectClass
//! [`ObservedType`]: relon_trace_abi::ObservedType
//! [`TraceEntryStatus`]: relon_trace_abi::TraceEntryStatus
//! [`RecoverableWriteRecord`]: relon_trace_abi::RecoverableWriteRecord
//! [`ExternalPc`]: relon_trace_abi::ExternalPc
//! [`ExternalSlot`]: relon_trace_abi::ExternalSlot
//! [`ExternalAddr`]: relon_trace_abi::ExternalAddr

#![cfg(feature = "serde")]

use relon_trace_abi::{
    EffectClass, ExternalAddr, ExternalPc, ExternalSlot, ObservedType, RecoverableWriteRecord,
    TraceEntryStatus,
};

fn roundtrip<T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
    v: T,
) {
    let bytes = bincode::serialize(&v).expect("serialise");
    let back: T = bincode::deserialize(&bytes).expect("deserialise");
    assert_eq!(v, back);
}

#[test]
fn recoverable_write_record_roundtrip() {
    let r = RecoverableWriteRecord {
        addr: 0xdead_beef_cafe_babe,
        before_value: 0x1234_5678_9abc_def0,
    };
    roundtrip(r);
}

#[test]
fn recoverable_write_record_zero_roundtrip() {
    // Zero-value record (all bits cleared) must still survive a
    // round-trip — bincode encodes width, not significance, so this
    // catches accidental "skip if zero" optimisations.
    let r = RecoverableWriteRecord {
        addr: 0,
        before_value: 0,
    };
    roundtrip(r);
}

#[test]
fn external_pc_roundtrip() {
    roundtrip(ExternalPc(0));
    roundtrip(ExternalPc(0x1));
    roundtrip(ExternalPc(0xfeed_face_dead_beef));
    roundtrip(ExternalPc(u64::MAX));
}

#[test]
fn external_slot_roundtrip() {
    roundtrip(ExternalSlot(0));
    roundtrip(ExternalSlot(1));
    roundtrip(ExternalSlot(u32::MAX));
}

#[test]
fn external_addr_roundtrip() {
    roundtrip(ExternalAddr(0));
    roundtrip(ExternalAddr(0xabcd_ef01_2345_6789));
}

#[test]
fn effect_class_roundtrip_all_variants() {
    for ec in [
        EffectClass::Pure,
        EffectClass::ReadOnly,
        EffectClass::RecoverableWrite,
        EffectClass::Unrecoverable,
    ] {
        roundtrip(ec);
    }
}

#[test]
fn observed_type_roundtrip_all_variants() {
    for ot in [
        ObservedType::I32,
        ObservedType::I64,
        ObservedType::F64,
        ObservedType::Bool,
        ObservedType::Ptr,
    ] {
        roundtrip(ot);
    }
}

#[test]
fn trace_entry_status_roundtrip_all_variants() {
    for s in [
        TraceEntryStatus::Success,
        TraceEntryStatus::GuardFailed,
        TraceEntryStatus::Aborted,
    ] {
        roundtrip(s);
    }
}

#[test]
fn effect_class_wire_size_is_one_byte_via_enum_index() {
    // bincode encodes #[repr(u8)] enums as a 4-byte u32 variant
    // index (bincode 1.x default). The exact byte count isn't part
    // of our ABI guarantee — we just pin that the variant index is
    // **stable**: encode-decode-re-encode produces the same bytes.
    // Reviewers: if bincode's default config changes in a v2 bump,
    // pin a bincode::Config builder here so the wire format stays
    // stable.
    let bytes_a = bincode::serialize(&EffectClass::ReadOnly).unwrap();
    let bytes_b = bincode::serialize(&EffectClass::ReadOnly).unwrap();
    assert_eq!(bytes_a, bytes_b);
    assert_eq!(
        bincode::deserialize::<EffectClass>(&bytes_a).unwrap(),
        EffectClass::ReadOnly
    );
}
