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

/// Byte offset of the cached payload hash inside a layout-conformant
/// dict key record. See [`fx_hash_key_record`] for the layout contract
/// and the rationale for caching the hash at fixture-build time.
///
/// Exposed as a `pub const` so the cranelift-side inline emitter
/// (`relon_trace_emitter::dict_inline`) can issue a single
/// `load.u64 [key_ptr + STRING_RECORD_HASH_OFFSET]` instead of running
/// the byte-wise hash loop on every dict lookup.
pub const STRING_RECORD_HASH_OFFSET: i32 = 4;

/// Byte offset of the payload bytes inside a layout-conformant dict
/// key record. Sits after the 4-byte `len` header and the 8-byte
/// cached `hash` field. Exposed for the same reason as
/// [`STRING_RECORD_HASH_OFFSET`].
pub const STRING_RECORD_PAYLOAD_OFFSET: i32 = 12;

/// Hash a `[len: u32 LE][hash: u64 LE][utf8...]` dict key record by
/// **reading the cached hash field** — the producer side
/// (`build_string_record` / the recorder driver) has already pre-computed
/// the payload hash at record-build time and stamped it into bytes
/// 4..12, so the consumer side just loads it back as a u64.
///
/// This is the Tier 1a "StringRef header caches u32 fx_hash" optimisation
/// (the cached field is widened to u64 here so it matches the dict's
/// entry-table hash width — see the dict_list module doc for why u32
/// would force an extra fold step on the IC hot path). Replacing the
/// byte-wise hash loop with a single load drops the W5 hot-path key
/// hashing cost from ~one xor+mul per key byte to a single 8-byte
/// load.
///
/// # Safety
///
/// `key_ptr` must point at a layout-conformant dict key record with at
/// least `12 + len` valid bytes:
///
/// - bytes 0..4   — `len: u32 LE` (payload byte count)
/// - bytes 4..12  — `hash: u64 LE` (pre-computed `fx_hash_bytes(payload)`)
/// - bytes 12..12+len — UTF-8 payload bytes
///
/// The trace JIT runtime holds these records on a stable arena;
/// callers outside that arena ownership boundary must keep the
/// backing memory alive for the duration of this call. The cached
/// hash MUST match `fx_hash_bytes(payload)` byte-for-byte — otherwise
/// the dict IC will silently turn every lookup into a deopt; producer
/// helpers like `build_string_record` enforce this at construction
/// time and tests in this module round-trip the invariant.
#[inline]
pub unsafe fn fx_hash_key_record(key_ptr: *const u8) -> u64 {
    // SAFETY: caller contract — `key_ptr` is a layout-conformant dict
    // key record whose cached hash field at offset 4 is the FxHash of
    // the payload bytes. Loading the cached u64 is byte-identical to
    // re-running the byte-wise hash loop on the payload, by the
    // construction invariant of `build_string_record`.
    unsafe {
        key_ptr
            .add(STRING_RECORD_HASH_OFFSET as usize)
            .cast::<u64>()
            .read_unaligned()
    }
}

/// Byte-wise FxHash over a dict key record's payload — the fallback
/// reference implementation used by tests + the producer side of the
/// pre-cache contract. Production hot paths route through
/// [`fx_hash_key_record`] (loads the cached u64) instead.
///
/// # Safety
///
/// Same layout contract as [`fx_hash_key_record`]; this variant simply
/// ignores the cached field and recomputes the hash from the payload
/// bytes.
#[inline]
pub unsafe fn fx_hash_key_record_payload(key_ptr: *const u8) -> u64 {
    // SAFETY: caller contract — see [`fx_hash_key_record`].
    let len = unsafe { (key_ptr as *const u32).read_unaligned() } as usize;
    let bytes = unsafe {
        std::slice::from_raw_parts(key_ptr.add(STRING_RECORD_PAYLOAD_OFFSET as usize), len)
    };
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
        // Producer-side construction: stamp the cached hash at
        // record-build time so the consumer-side `fx_hash_key_record`
        // load matches the byte-wise reference.
        let payload = b"thekey";
        let cached_hash = fx_hash_bytes(payload);
        let mut record = (payload.len() as u32).to_le_bytes().to_vec();
        record.extend_from_slice(&cached_hash.to_le_bytes());
        record.extend_from_slice(payload);
        let via_record = unsafe { fx_hash_key_record(record.as_ptr()) };
        let via_bytes = fx_hash_bytes(payload);
        assert_eq!(
            via_record, via_bytes,
            "cached hash must round-trip vs byte-wise reference"
        );
        // Belt-and-braces: the payload-only fallback must agree too.
        let via_payload = unsafe { fx_hash_key_record_payload(record.as_ptr()) };
        assert_eq!(
            via_payload, via_bytes,
            "payload-only fallback must agree with byte-wise reference"
        );
    }
}
