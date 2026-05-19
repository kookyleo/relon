//! Layout smoke tests for the shared trace ABI.
//!
//! These pins are **load-bearing**. The cranelift emitter in
//! `relon-trace-emitter` reads / writes every `#[repr(C)]` type
//! defined in `relon-trace-abi` by raw byte offset. Any change to
//! sizes, alignment, or field offset shifts those constants and
//! silently corrupts the trace deopt path / pending-write buffer /
//! result slot.
//!
//! ## What this file guarantees
//!
//! - `TraceContext` fields appear at fixed byte offsets the emitter
//!   relies on (`ssa_slots @ 0`, `result_slot @ 16`).
//! - `DeoptStateSnapshot` is at least as large as the sum of its
//!   field widths (no surprise tail padding shrinks the struct).
//! - `RecoverableWriteRecord` is exactly 16 bytes (the emitter
//!   strides this size when bulk-appending to
//!   `pending_recoverable_writes`).
//! - `ExternalPc`, `ExternalSlot`, `ExternalAddr` are zero-overhead
//!   newtypes (`#[repr(transparent)]` preserved at the size level).
//! - `AbiSignature` for the canonical `TRACE_ENTRY_SIG` matches the
//!   2-param / 1-return shape every trace obeys.
//!
//! If any of these assertions trip, **STOP** and audit every reader
//! of the same field in `relon-trace-emitter` and
//! `relon-trace-jit::runtime` before adjusting the constant — the
//! correct fix is usually to revert the ABI change, not to bump the
//! expected value here.

use std::mem::{align_of, size_of};

use relon_trace_abi::{
    AbiType, DeoptStateSnapshot, ExternalAddr, ExternalPc, ExternalSlot, RecoverableWriteRecord,
    TraceContext, TRACE_ENTRY_SIG,
};

/// `offset_of!`-style helper that avoids unstable `core::mem::offset_of`
/// on older toolchains. We materialise an instance, take a raw byte
/// reference to it and to the field, and subtract.
macro_rules! field_offset {
    ($ty:path, $instance:expr, $field:ident) => {{
        let base = &$instance as *const _ as usize;
        let f = &$instance.$field as *const _ as usize;
        f - base
    }};
}

#[test]
fn trace_context_alignment_is_pointer_width() {
    // `Box<[u64]>` (fat pointer) drags the struct alignment to 8 on
    // every supported target. If this drifts to 16, audit the
    // emitter's load instructions for missing alignment hints.
    assert_eq!(align_of::<TraceContext>(), 8);
}

#[test]
fn trace_context_ssa_slots_at_offset_zero() {
    let ctx = TraceContext::with_capacity(0);
    assert_eq!(field_offset!(TraceContext, ctx, ssa_slots), 0);
}

#[test]
fn trace_context_result_slot_after_box_fat_ptr() {
    // `Box<[T]>` is a (ptr, len) fat pointer == 16 bytes on every
    // 64-bit target. `result_slot: u64` must therefore sit at
    // offset 16 — and the cranelift emitter hard-codes that.
    let ctx = TraceContext::with_capacity(0);
    assert_eq!(field_offset!(TraceContext, ctx, result_slot), 16);
}

#[test]
fn trace_context_deopt_state_follows_result_slot() {
    // `result_slot` is a single u64 -> 8 bytes. Next field starts
    // at offset 24. The `Option<DeoptStateSnapshot>` discriminant +
    // payload starts here; we don't pin the exact offset of the
    // payload (compiler-chosen niche optimisations), only the field
    // itself.
    let ctx = TraceContext::with_capacity(0);
    let off = field_offset!(TraceContext, ctx, deopt_state);
    assert_eq!(off, 24);
}

#[test]
fn trace_context_size_is_stable() {
    // Pinned size = sum of field widths:
    //   ssa_slots                       16 (Box fat ptr)
    //   result_slot                      8
    //   deopt_state (Option<DSS>)       72 (rustc niches the None
    //                                       discriminant into the Box
    //                                       inside DSS so the option
    //                                       is the same width as DSS;
    //                                       v6-δ M2-B widened DSS to
    //                                       carry `value_stack_copy`)
    //   host_hooks                      32 (4 x Option<fn ptr>)
    //   pending_recoverable_writes      24 (Vec<T> = (ptr, len, cap))
    // Total: 152 bytes on 64-bit targets.
    //
    // If this assertion trips, *figure out which field grew* and
    // bump the expected size here AND update every emitter constant
    // that reads past the growing field.
    assert_eq!(size_of::<TraceContext>(), 152);
}

#[test]
fn deopt_state_snapshot_size_is_stable() {
    // Pinned size = sum of field widths:
    //   guard_pc      4
    //   (padding)     4
    //   external_pc   8
    //   ssa_slots_copy     16 (Box fat ptr)
    //   recoverable_writes 24 (Vec fat ptr)
    //   value_stack_copy   16 (Box fat ptr; v6-δ M2-B widening)
    // Total: 72 bytes on 64-bit targets.
    assert_eq!(size_of::<DeoptStateSnapshot>(), 72);
}

#[test]
fn recoverable_write_record_is_16_bytes() {
    // Two u64 fields, `#[repr(C)]`, no padding -> exactly 16.
    // The emitter strides this width when bulk-appending records to
    // `pending_recoverable_writes`.
    assert_eq!(size_of::<RecoverableWriteRecord>(), 16);
    assert_eq!(align_of::<RecoverableWriteRecord>(), 8);
}

#[test]
fn recoverable_write_record_field_offsets() {
    let r = RecoverableWriteRecord {
        addr: 0,
        before_value: 0,
    };
    assert_eq!(field_offset!(RecoverableWriteRecord, r, addr), 0);
    assert_eq!(field_offset!(RecoverableWriteRecord, r, before_value), 8);
}

#[test]
fn external_pc_is_transparent_u64() {
    // `#[repr(transparent)] u64` => size of u64.
    assert_eq!(size_of::<ExternalPc>(), 8);
    assert_eq!(align_of::<ExternalPc>(), 8);
}

#[test]
fn external_slot_is_transparent_u32() {
    assert_eq!(size_of::<ExternalSlot>(), 4);
    assert_eq!(align_of::<ExternalSlot>(), 4);
}

#[test]
fn external_addr_is_transparent_u64() {
    assert_eq!(size_of::<ExternalAddr>(), 8);
    assert_eq!(align_of::<ExternalAddr>(), 8);
}

#[test]
fn external_pc_pointer_roundtrip_smoke() {
    let backing = [0u8; 1];
    let p = backing.as_ptr();
    let pc = ExternalPc::from_ptr(p);
    assert_eq!(pc.as_ptr(), p);
    // The reverse direction (constructed from a literal u64) must
    // also round-trip — synthetic addresses are used in golden
    // tests where no real backing memory exists.
    let synthetic = ExternalPc(0xfeed_face_dead_beef);
    assert_eq!(synthetic.0, 0xfeed_face_dead_beef);
}

#[test]
fn abi_signature_trace_entry_shape() {
    assert_eq!(TRACE_ENTRY_SIG.params.len(), 2);
    assert_eq!(TRACE_ENTRY_SIG.returns.len(), 1);
    assert_eq!(TRACE_ENTRY_SIG.params[0], AbiType::Ptr);
    assert_eq!(TRACE_ENTRY_SIG.params[1], AbiType::Ptr);
    assert_eq!(TRACE_ENTRY_SIG.returns[0], AbiType::I32);
}

#[test]
fn option_deopt_state_niches_into_box_pointer() {
    // The deopt_state slot is `Option<DeoptStateSnapshot>` and we
    // rely on rustc's niche optimisation to keep it the same width
    // as `DeoptStateSnapshot` itself (the `Box` inside DSS provides
    // a non-null niche). If this assertion ever trips, the emitter
    // can no longer read `deopt_state` through a single qword load
    // and we'd need to either widen the load sequence or add a
    // surrounding wrapper to restore the niche.
    use std::mem::size_of;
    assert_eq!(
        size_of::<Option<DeoptStateSnapshot>>(),
        size_of::<DeoptStateSnapshot>()
    );
}
