//! Deterministic, dependency-free hash routines shared across the
//! trace JIT pipeline.
//!
//! Both the **producer** side (an analyzer / IR pass deciding what
//! `shape_hash` to stamp into an `Op::DictGetByStringKey`) and the
//! **consumer** side (`relon_trace_jit::runtime::dict_list` reading
//! the per-key hash off a `*const StringRef` at dispatch time) must
//! agree byte-for-byte on the hash function — any drift would silently
//! turn every inline-cache lookup into a miss.
//!
//! Centralising the algorithm here keeps the canonical source in the
//! same crate that already pins every wire-format type (see the
//! crate-level "ABI invariant" doc). A `relon-ir` pass that wants to
//! pre-compute `shape_hash` calls into [`fx_hash_bytes`] via
//! `relon-trace-abi`; the runtime calls into the same fn via
//! `relon-trace-jit::runtime`. There is no second implementation
//! anywhere in the workspace.
//!
//! ## Why FxHash64
//!
//! - **Deterministic across runs / threads**: no random seeding, so a
//!   producer running at compile time and a consumer running at JIT
//!   time always derive the same `u64` for the same byte stream.
//! - **Cheap on short keys**: dict-key sets are typically a handful of
//!   short identifiers; FxHash bottoms out at one xor+mul per byte.
//! - **No external dependency**: pulling a hashing crate into the
//!   bottom of the dep graph (where this crate lives) would risk
//!   feature-flag explosion. The full reference impl is ~12 lines and
//!   lives below.
//!
//! The exact constants here come from the rustc-fxhash reference
//! implementation. Any 64-bit hash with adequate single-byte
//! dispersal would suffice for the inline-cache tagging use case;
//! we lock the constants down so the producer/consumer contract
//! stays stable across compiler / target / opt-level changes.

/// FxHash64 over a byte slice. Deterministic; identical output across
/// runs, threads, opt levels, and host architectures.
///
/// **The canonical reference implementation** for the trace JIT
/// pipeline. Producers (`relon-ir` / `relon-analyzer` pre-stamping
/// `Op::DictGetByStringKey::shape_hash`) and consumers
/// (`relon-trace-jit::runtime::dict_list` IC dispatch) must both
/// route through this fn — re-implementing the algorithm elsewhere
/// is forbidden by the layout-smoke + `hash_consistency` test.
#[inline]
pub fn fx_hash_bytes(bytes: &[u8]) -> u64 {
    const SEED: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let mut h: u64 = SEED;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Hash a `[len: u32 LE][utf8...]` String record with [`fx_hash_bytes`].
///
/// Pulled out so bench fixtures can pre-compute the per-key hash at
/// fixture-build time and stamp it into the dict's entry table.
///
/// # Safety
///
/// `key_ptr` must point at a layout-conformant String record with
/// `len + 4` valid bytes (4-byte little-endian length header followed
/// by exactly `len` UTF-8 payload bytes). The trace JIT runtime
/// holds these records on a stable arena; callers outside that
/// arena ownership boundary must keep the backing memory alive for
/// the duration of this call.
#[inline]
pub unsafe fn fx_hash_key_record(key_ptr: *const u8) -> u64 {
    // SAFETY: caller contract — `key_ptr` is a layout-conformant
    // String record.
    let len = unsafe { (key_ptr as *const u32).read_unaligned() } as usize;
    let bytes = unsafe { std::slice::from_raw_parts(key_ptr.add(4), len) };
    fx_hash_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_across_calls() {
        assert_eq!(fx_hash_bytes(b"hello"), fx_hash_bytes(b"hello"));
    }

    #[test]
    fn different_inputs_hash_differently() {
        // Not a collision-free guarantee — just a sanity check that
        // the constants aren't degenerately mapping every input to
        // the same seed.
        assert_ne!(fx_hash_bytes(b"hello"), fx_hash_bytes(b"world"));
    }

    #[test]
    fn empty_input_returns_seed() {
        // The loop never enters when `bytes` is empty, so the
        // hash falls through to the seed constant. Locking this
        // behaviour down because the dict IC may stamp empty-key
        // entries (`d[""]`) and the consumer expects this exact
        // value.
        assert_eq!(fx_hash_bytes(b""), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn key_record_matches_payload_only() {
        let payload = b"thekey";
        let mut record = (payload.len() as u32).to_le_bytes().to_vec();
        record.extend_from_slice(payload);
        let via_record = unsafe { fx_hash_key_record(record.as_ptr()) };
        let via_bytes = fx_hash_bytes(payload);
        assert_eq!(via_record, via_bytes);
    }
}
