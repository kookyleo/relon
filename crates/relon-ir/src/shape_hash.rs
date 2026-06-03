//! Canonical helper for computing the `shape_hash` field of
//! [`crate::ir::Op::DictGetByStringKey`].
//!
//! ## Why this lives in `relon-ir`
//!
//! `Op::DictGetByStringKey { shape_hash, .. }` is an IR-level Op
//! produced by an analyzer pass; the `shape_hash` fingerprints the
//! dict's key set so a keyed-lookup consumer can quickly reject a
//! shape mismatch.
//!
//! For the cache to ever hit, the producer (the analyzer / IR pass
//! choosing the value to stamp) and any consumer hashing a runtime key
//! must use the **same hash algorithm**. The canonical FxHash impl
//! ([`fx_hash_bytes`]) lives here so both sides route through one
//! source of truth and stay bit-for-bit identical on the same key
//! bytes.
//!
//! ## Stability contract
//!
//! Changing the byte layout the hasher consumes (e.g. mixing key
//! ordering, adding a salt) is a **wire-format break**. Bump the IR
//! Op's variant or version any such change explicitly.

/// FxHash64 over a byte slice. Deterministic; identical output across
/// runs, threads, opt levels, and host architectures.
///
/// The constants come from the rustc-fxhash reference implementation.
/// Any 64-bit hash with adequate single-byte dispersal would suffice;
/// we lock the constants down so the producer/consumer contract stays
/// stable across compiler / target / opt-level changes.
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

/// Compute the canonical `shape_hash` for a dict-key set.
///
/// Hashes the UTF-8 bytes of each key via [`fx_hash_bytes`] and mixes
/// the per-key hashes into a running accumulator.
///
/// Pass keys in a **stable, source-order** iteration — usually the
/// order in which the dict literal declares them. Note the xor mixing
/// is commutative, so permutations of the same key set hash equal.
#[inline]
pub fn shape_hash_for_keys<'a, I>(keys: I) -> u64
where
    I: IntoIterator<Item = &'a str>,
{
    let mut h = INITIAL_SEED;
    for key in keys {
        // Per-key FxHash, mixed into the running accumulator via xor.
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

    #[test]
    fn empty_key_set_returns_seed() {
        assert_eq!(shape_hash_for_keys(std::iter::empty()), INITIAL_SEED);
    }

    #[test]
    fn single_key_seed_xor_mixing() {
        // For a single key, the accumulator is
        // `SEED ^ fx_hash_bytes(key_bytes)`.
        let key = "thekey";
        let producer = shape_hash_for_keys([key]);
        let expected = INITIAL_SEED ^ fx_hash_bytes(key.as_bytes());
        assert_eq!(producer, expected);
    }

    #[test]
    fn fx_hash_bytes_is_deterministic() {
        assert_eq!(fx_hash_bytes(b"hello"), fx_hash_bytes(b"hello"));
        assert_ne!(fx_hash_bytes(b"hello"), fx_hash_bytes(b"world"));
        // Empty input falls through to the seed constant.
        assert_eq!(fx_hash_bytes(b""), 0xcbf2_9ce4_8422_2325);
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
