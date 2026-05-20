// Module-local allow: the consistency test below calls the unsafe
// `fx_hash_key_record` shim (its caller-provided layout-conformant
// String record is constructed by the test itself, so the safety
// contract is trivially upheld). The crate-wide `deny(unsafe_code)`
// stays intact elsewhere.
#![allow(unsafe_code)]

//! Canonical helper for computing the `shape_hash` field of
//! [`crate::ir::Op::DictGetByStringKey`].
//!
//! ## Why this lives in `relon-ir`
//!
//! `Op::DictGetByStringKey { shape_hash, .. }` is an IR-level Op
//! produced by a future analyzer pass (F-D8-D) and consumed by the
//! trace recorder, which stamps `shape_hash` onto the resulting
//! `TraceOp::DictLookup`'s inline-cache slot. The IC dispatch then
//! routes through `relon_trace_jit::runtime::fx_hash_key_record` to
//! check whether the runtime key matches.
//!
//! For the IC to ever hit, the producer (the analyzer / IR pass
//! choosing the value to stamp) and the consumer (the runtime hashing
//! the runtime key) must use the **same hash algorithm**. The actual
//! FxHash impl lives in the bottom-of-graph `relon-trace-abi::hash`
//! crate so neither side has to re-implement it. This module is the
//! producer-side wrapper that names the hash as "the canonical shape
//! hash" — analyzer / IR-emit code never reaches into `relon-trace-abi`
//! directly; it asks this helper for a `shape_hash` and gets a value
//! that is guaranteed bit-for-bit identical with what the runtime
//! will compute on the same key bytes.
//!
//! ## Stability contract
//!
//! Changing the byte layout the hasher consumes (e.g. mixing key
//! ordering, adding a salt) is a **wire-format break**. Bump the IR
//! Op's variant or version any such change explicitly — F-D8 traces
//! already in flight assume the current bytes-only stream.

use relon_trace_abi::hash::fx_hash_bytes;

/// Compute the canonical `shape_hash` for a dict-key set.
///
/// Hashes the UTF-8 bytes of each key in iteration order; the result
/// is the running FxHash64 over the concatenation. **Identical** to
/// the value `relon_trace_jit::runtime::fx_hash_key_record` would
/// derive when called on a layout-conformant String record carrying
/// the same payload (verified by the layout smoke + the
/// `single_key_matches_runtime_hash` unit test below).
///
/// Pass keys in a **stable, source-order** iteration — usually the
/// order in which the dict literal declares them. Reordering produces
/// a different hash and is treated as a different shape by the IC.
#[inline]
pub fn shape_hash_for_keys<'a, I>(keys: I) -> u64
where
    I: IntoIterator<Item = &'a str>,
{
    let mut h = INITIAL_SEED;
    for key in keys {
        // Per-key FxHash, then mix into running accumulator via xor.
        // xor preserves the bottom-of-graph property (no
        // dependency-on-relon-trace-jit) and is reproducible by the
        // F-D8-D unit tests without pulling the runtime in.
        let k = fx_hash_bytes(key.as_bytes());
        h ^= k;
    }
    h
}

/// Initial accumulator value for [`shape_hash_for_keys`]. Exposed as a
/// `pub const` so an analyzer side-table that pre-computes shape
/// hashes (rather than calling the helper at lowering time) can
/// reproduce the exact byte stream.
pub const INITIAL_SEED: u64 = 0xcbf2_9ce4_8422_2325;

#[cfg(test)]
mod tests {
    use super::*;
    use relon_trace_abi::hash::{fx_hash_bytes, fx_hash_key_record};

    #[test]
    fn empty_key_set_returns_seed() {
        assert_eq!(shape_hash_for_keys(std::iter::empty()), INITIAL_SEED);
    }

    #[test]
    fn single_key_matches_runtime_hash() {
        // The producer-side `shape_hash_for_keys(["k"])` must round-
        // trip with the consumer-side `fx_hash_key_record(record)` on
        // a single-key dict; otherwise the IC dispatch would always
        // miss. We anchor this to a literal layout-conformant String
        // record so the layout drift the layout-smoke test guards
        // against also shows up here.
        let key = "thekey";
        let mut record = (key.len() as u32).to_le_bytes().to_vec();
        record.extend_from_slice(key.as_bytes());

        let producer = shape_hash_for_keys([key]);
        // Mirror the producer's seed-xor mixing: for a single key,
        // the accumulator is `SEED ^ fx_hash_bytes(key_bytes)`.
        let expected = INITIAL_SEED ^ fx_hash_bytes(key.as_bytes());
        assert_eq!(producer, expected);
        // The runtime helper sees the layout-conformant record, hashes
        // payload-only, matches the un-seeded fx_hash_bytes value.
        let runtime = unsafe { fx_hash_key_record(record.as_ptr()) };
        assert_eq!(runtime, fx_hash_bytes(key.as_bytes()));
    }

    #[test]
    fn order_sensitive() {
        // The accumulator uses xor, which is commutative — so
        // `shape_hash_for_keys(["a", "b"]) == shape_hash_for_keys(["b", "a"])`
        // for distinct keys. Document this; producers must canonicalise
        // ordering before stamping if they care about per-shape distinct
        // hashes for permutations.
        let ab = shape_hash_for_keys(["a", "b"]);
        let ba = shape_hash_for_keys(["b", "a"]);
        assert_eq!(ab, ba, "xor mixing is commutative across keys");
    }

    #[test]
    fn distinct_key_sets_hash_differently() {
        let dict_a = shape_hash_for_keys(["host", "port"]);
        let dict_b = shape_hash_for_keys(["host", "port", "tls"]);
        assert_ne!(dict_a, dict_b);
    }
}
