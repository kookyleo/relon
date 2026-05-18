//! `__relon_trace_inline_cache_lookup` + IC cardinality dispatch.
//!
//! The emitter places type-check guards behind an inline cache (IC)
//! so the slow `observed_type_of(var)` host lookup is skipped on the
//! steady-state hit path. Each IC is sized by a compile-time
//! `N: usize` const generic (`InlineCache<1>` mono, `<2>` bi, `<4>`
//! mega — see `inline_cache.rs`).
//!
//! At runtime cranelift IR can only emit a C-ABI call with a fixed
//! signature. To bridge the const-generic IC to that uniform call we
//! adopt a **header-byte cardinality** layout:
//!
//! ```text
//! IC storage = [cardinality: u8][InlineCache<N>: variable]
//! ```
//!
//! The helper reads the leading byte to dispatch into the correct
//! `InlineCache<N>::check`. Host-side allocation must call
//! [`ic_storage_size`] to size the buffer and [`write_ic_header`] to
//! prime the cardinality byte.
//!
//! ## Why not a vtable?
//!
//! A vtable pointer would cost an extra cache line + indirection on
//! the fast path. The cardinality byte is read on the same line as
//! the IC slots (`InlineCache<N>` is 24-40 bytes for our N choices)
//! so the dispatch is essentially free in the hot case.
//!
//! ## Why not stash the cardinality in a static at the call site?
//!
//! Cranelift IR can emit immediate `i32` operands per call site, so
//! a per-call-site static is feasible — but it forces the helper to
//! grow a 4th argument. The header byte keeps the helper signature
//! identical to what the emitter already emits (`(ic_ptr, observed)`)
//! and centralises the dispatch decision in one place.

use std::mem::{align_of, size_of};

use crate::inline_cache::{CacheResult, InlineCache};
use crate::trace_ir::ObservedType;

impl CacheResult {
    /// Lower [`CacheResult`] to the i32 the cranelift IR branches on.
    /// Mirrors the doc-comment on `__relon_trace_inline_cache_lookup`:
    /// `0 = Hit`, `1 = Miss`.
    pub fn into_i32(self) -> i32 {
        match self {
            CacheResult::Hit => 0,
            CacheResult::Miss => 1,
        }
    }
}

/// Bridge: convert the raw `u8` observed-type byte the emitter passes
/// into a typed [`ObservedType`]. Out-of-range bytes fall through to
/// `ObservedType::Ptr` (the opaque tag), which lets the IC treat
/// junk values as "non-matching" rather than panicking.
fn observed_type_from_raw(raw: u8) -> ObservedType {
    match raw {
        0 => ObservedType::I32,
        1 => ObservedType::I64,
        2 => ObservedType::F64,
        3 => ObservedType::Bool,
        _ => ObservedType::Ptr,
    }
}

/// Stable little-endian encoding of `ObservedType` the emitter must
/// use when calling the IC helper. Kept here (rather than on
/// `ObservedType` itself) to avoid leaking ABI choices into the
/// optimizer-facing IR.
pub fn observed_type_as_raw(ty: ObservedType) -> u8 {
    match ty {
        ObservedType::I32 => 0,
        ObservedType::I64 => 1,
        ObservedType::F64 => 2,
        ObservedType::Bool => 3,
        ObservedType::Ptr => 4,
    }
}

/// Byte size of an `InlineCache<N>` plus its leading cardinality
/// header. The host uses this to size the allocation backing each
/// type-check guard.
pub fn ic_storage_size<const N: usize>() -> usize {
    // The cardinality byte sits at offset 0; the InlineCache<N>
    // structure starts at offset 1. To keep alignment correct
    // (InlineCache embeds `Cell<[Option<ObservedType>; N]>` whose
    // alignment is u8 today but may grow) we reserve the natural
    // alignment of `InlineCache<N>`.
    let header = 1usize;
    let align = align_of::<InlineCache<N>>();
    let padded_header = (header + align - 1) & !(align - 1);
    padded_header + size_of::<InlineCache<N>>()
}

/// Byte offset of the `InlineCache<N>` payload within the IC
/// storage. Mirrors the header-padding in [`ic_storage_size`].
fn ic_payload_offset<const N: usize>() -> usize {
    let header = 1usize;
    let align = align_of::<InlineCache<N>>();
    (header + align - 1) & !(align - 1)
}

/// Initialise a freshly allocated IC storage region. Writes the
/// cardinality header byte and constructs a default-empty
/// `InlineCache<N>` at the payload offset.
///
/// ## Safety
///
/// - `storage` must point to a buffer of at least `ic_storage_size::<N>()`
///   bytes, properly aligned for `InlineCache<N>`.
/// - `storage` must not currently hold a live `InlineCache<N>` (or
///   any other value with a Drop impl) — this function overwrites
///   the region without dropping.
/// - `N` must match the cardinality the caller intends to dispatch
///   with later (1, 2, or 4 are the supported choices today; other
///   values still work but the runtime helper will treat unknown
///   cardinalities as misses).
pub unsafe fn write_ic_header<const N: usize>(storage: *mut u8) {
    // 1. Cardinality byte at offset 0.
    debug_assert!(N <= u8::MAX as usize, "cardinality must fit in u8");
    *storage = N as u8;
    // 2. InlineCache<N> at the aligned payload offset.
    let payload = storage.add(ic_payload_offset::<N>()) as *mut InlineCache<N>;
    std::ptr::write(payload, InlineCache::<N>::new());
}

/// Drop the `InlineCache<N>` payload at the given storage. Mirrors
/// [`write_ic_header`]; call before freeing the backing memory if
/// the IC owns any non-trivial Drop state.
///
/// ## Safety
///
/// `storage` must point to a region previously initialised by
/// `write_ic_header::<N>(storage)`. Calling with a different `N`
/// is undefined behaviour.
pub unsafe fn drop_ic_payload<const N: usize>(storage: *mut u8) {
    let payload = storage.add(ic_payload_offset::<N>()) as *mut InlineCache<N>;
    std::ptr::drop_in_place(payload);
}

/// Cardinality dispatch entry point. The emitter passes the raw
/// storage pointer (header byte + InlineCache payload) and the raw
/// observed-type byte; we read the header byte to pick the right
/// `InlineCache<N>::check` instantiation.
///
/// Returns:
/// - `0` on cache hit (no slow-path needed)
/// - `1` on cache miss (caller must run slow path / deopt)
///
/// Unknown cardinalities always return `1` (Miss), letting the
/// emitter keep its branch shape uniform.
///
/// ## Safety
///
/// - `ic_ptr` must point to an IC storage region of the layout
///   produced by [`write_ic_header`]. The first byte is read as the
///   cardinality; the payload (at the aligned offset) is reinterpreted
///   as an `InlineCache<N>` whose lifetime is tied to the caller.
/// - `observed_type_raw` must be a valid `ObservedType` discriminant
///   (0..=4); out-of-range bytes are coerced to `ObservedType::Ptr`.
/// - The function is `unsafe extern "C"` so its signature is callable
///   from cranelift IR.
#[no_mangle]
pub unsafe extern "C" fn __relon_trace_inline_cache_lookup(
    ic_ptr: *mut u8,
    observed_type_raw: u8,
) -> i32 {
    if ic_ptr.is_null() {
        return CacheResult::Miss.into_i32();
    }
    let cardinality = *ic_ptr;
    let observed = observed_type_from_raw(observed_type_raw);

    match cardinality {
        1 => {
            let payload = ic_ptr.add(ic_payload_offset::<1>()) as *const InlineCache<1>;
            (*payload).check(observed).into_i32()
        }
        2 => {
            let payload = ic_ptr.add(ic_payload_offset::<2>()) as *const InlineCache<2>;
            (*payload).check(observed).into_i32()
        }
        4 => {
            let payload = ic_ptr.add(ic_payload_offset::<4>()) as *const InlineCache<4>;
            (*payload).check(observed).into_i32()
        }
        _ => CacheResult::Miss.into_i32(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{alloc, alloc_zeroed, dealloc, Layout};

    /// Allocate an IC storage region for cardinality `N`, run `f` with
    /// the storage pointer, then drop + free. Keeps the unsafe layout
    /// dance in one place.
    fn with_ic_storage<const N: usize, R>(f: impl FnOnce(*mut u8) -> R) -> R {
        let size = ic_storage_size::<N>();
        let align = align_of::<InlineCache<N>>().max(1);
        let layout = Layout::from_size_align(size, align).expect("valid layout");
        unsafe {
            let storage = alloc_zeroed(layout);
            assert!(!storage.is_null(), "alloc failed");
            write_ic_header::<N>(storage);
            let result = f(storage);
            drop_ic_payload::<N>(storage);
            dealloc(storage, layout);
            result
        }
    }

    #[test]
    fn cache_result_into_i32_mapping() {
        assert_eq!(CacheResult::Hit.into_i32(), 0);
        assert_eq!(CacheResult::Miss.into_i32(), 1);
    }

    #[test]
    fn observed_type_raw_roundtrip() {
        for ty in [
            ObservedType::I32,
            ObservedType::I64,
            ObservedType::F64,
            ObservedType::Bool,
            ObservedType::Ptr,
        ] {
            let raw = observed_type_as_raw(ty);
            let back = observed_type_from_raw(raw);
            assert_eq!(back, ty);
        }
    }

    #[test]
    fn observed_type_from_raw_clamps_invalid() {
        // Bytes >= 5 fall through to the opaque Ptr tag.
        assert_eq!(observed_type_from_raw(5), ObservedType::Ptr);
        assert_eq!(observed_type_from_raw(0xff), ObservedType::Ptr);
    }

    #[test]
    fn ic_storage_size_includes_header() {
        // Header is >= 1 byte; total must exceed the bare InlineCache size.
        assert!(ic_storage_size::<1>() > size_of::<InlineCache<1>>());
        assert!(ic_storage_size::<2>() > size_of::<InlineCache<2>>());
        assert!(ic_storage_size::<4>() > size_of::<InlineCache<4>>());
    }

    #[test]
    fn ic_lookup_n1_monomorphic_hit_after_miss() {
        with_ic_storage::<1, ()>(|storage| {
            unsafe {
                // First observation: miss (cache empty).
                let r0 = __relon_trace_inline_cache_lookup(
                    storage,
                    observed_type_as_raw(ObservedType::I32),
                );
                assert_eq!(r0, CacheResult::Miss.into_i32());
                // Same observation again: hit.
                for _ in 0..3 {
                    let r = __relon_trace_inline_cache_lookup(
                        storage,
                        observed_type_as_raw(ObservedType::I32),
                    );
                    assert_eq!(r, CacheResult::Hit.into_i32());
                }
            }
        });
    }

    #[test]
    fn ic_lookup_n2_polymorphic_hit() {
        with_ic_storage::<2, ()>(|storage| {
            unsafe {
                // Seed both slots.
                __relon_trace_inline_cache_lookup(storage, observed_type_as_raw(ObservedType::I32));
                __relon_trace_inline_cache_lookup(storage, observed_type_as_raw(ObservedType::I64));
                // Now both types should hit.
                let r_i32 = __relon_trace_inline_cache_lookup(
                    storage,
                    observed_type_as_raw(ObservedType::I32),
                );
                let r_i64 = __relon_trace_inline_cache_lookup(
                    storage,
                    observed_type_as_raw(ObservedType::I64),
                );
                assert_eq!(r_i32, CacheResult::Hit.into_i32());
                assert_eq!(r_i64, CacheResult::Hit.into_i32());
            }
        });
    }

    #[test]
    fn ic_lookup_n4_megamorphic_hit() {
        with_ic_storage::<4, ()>(|storage| {
            unsafe {
                // Seed four distinct types.
                for ty in [
                    ObservedType::I32,
                    ObservedType::I64,
                    ObservedType::F64,
                    ObservedType::Bool,
                ] {
                    __relon_trace_inline_cache_lookup(storage, observed_type_as_raw(ty));
                }
                // All four hit.
                for ty in [
                    ObservedType::I32,
                    ObservedType::I64,
                    ObservedType::F64,
                    ObservedType::Bool,
                ] {
                    let r = __relon_trace_inline_cache_lookup(storage, observed_type_as_raw(ty));
                    assert_eq!(r, CacheResult::Hit.into_i32(), "expected hit for {:?}", ty);
                }
            }
        });
    }

    #[test]
    fn ic_lookup_miss_on_new_type() {
        with_ic_storage::<2, ()>(|storage| {
            unsafe {
                // Seed with I32 only.
                __relon_trace_inline_cache_lookup(storage, observed_type_as_raw(ObservedType::I32));
                // F64 is uncached -> miss.
                let r = __relon_trace_inline_cache_lookup(
                    storage,
                    observed_type_as_raw(ObservedType::F64),
                );
                assert_eq!(r, CacheResult::Miss.into_i32());
            }
        });
    }

    #[test]
    fn ic_lookup_unknown_cardinality_returns_miss() {
        // Manually allocate a storage whose header byte is an unknown
        // cardinality (3, 7). The helper must classify it as a Miss
        // rather than dispatch.
        let layout = Layout::from_size_align(16, 8).unwrap();
        unsafe {
            let storage = alloc(layout);
            *storage = 7; // unknown cardinality
            let r =
                __relon_trace_inline_cache_lookup(storage, observed_type_as_raw(ObservedType::I32));
            assert_eq!(r, CacheResult::Miss.into_i32());
            dealloc(storage, layout);
        }
    }

    #[test]
    fn ic_lookup_null_ptr_returns_miss() {
        let r = unsafe {
            __relon_trace_inline_cache_lookup(
                std::ptr::null_mut(),
                observed_type_as_raw(ObservedType::I32),
            )
        };
        assert_eq!(r, CacheResult::Miss.into_i32());
    }
}
