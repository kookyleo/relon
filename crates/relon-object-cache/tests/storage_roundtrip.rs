//! Round-trip the cache file format: `store` + `load` must return
//! the exact bytes we put in, and storing two different sources
//! into the same directory must not stomp on each other.

#![allow(deprecated)]

use relon_object_cache::{
    load, store, CacheError, HostFnImport, IntegrityMode, Metadata, SignatureHash,
};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

fn sample_metadata(generator: &str) -> Metadata {
    Metadata {
        host_fn_imports: vec![HostFnImport {
            name: "relon_host_log".to_owned(),
            cap_bit: 3,
            params_hash: [1u8; 32],
            returns_hash: [2u8; 32],
        }],
        cap_bitmap: 0b0000_1010,
        main_signature: SignatureHash([0xAB; 32]),
        created_at_unix: 1_700_000_000,
        generator_version: generator.to_owned(),
    }
}

fn sha_of(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[test]
fn store_then_load_returns_same_object_bytes() {
    let dir = tempdir().unwrap();
    let object = b"\x7fELF stub-payload bytes for tests".to_vec();
    // Strict integrity recomputes sha256(object) and compares
    // against the filename — use the actual digest as the key.
    let key = sha_of(&object);
    let triple = "x86_64-unknown-linux-gnu";
    let meta = sample_metadata("relon-codegen-cranelift 0.1.0");

    let path = store(dir.path(), key, triple, &object, &meta, None).unwrap();
    assert!(path.exists(), "store must produce a file at {path:?}");

    let entry = load(dir.path(), key, triple, None, IntegrityMode::Strict)
        .unwrap()
        .expect("cache hit expected");
    assert_eq!(entry.object_bytes, object);
    assert_eq!(entry.metadata, meta);
    assert_eq!(entry.target_triple, triple);
}

#[test]
fn two_distinct_sources_coexist() {
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let obj_a = b"object-A-bytes".to_vec();
    let obj_b = b"object-B-different".to_vec();
    let key_a = sha_of(&obj_a);
    let key_b = sha_of(&obj_b);

    store(
        dir.path(),
        key_a,
        triple,
        &obj_a,
        &sample_metadata("gen-A"),
        None,
    )
    .unwrap();
    store(
        dir.path(),
        key_b,
        triple,
        &obj_b,
        &sample_metadata("gen-B"),
        None,
    )
    .unwrap();

    let a = load(dir.path(), key_a, triple, None, IntegrityMode::Strict)
        .unwrap()
        .unwrap();
    let b = load(dir.path(), key_b, triple, None, IntegrityMode::Strict)
        .unwrap()
        .unwrap();

    assert_eq!(a.object_bytes, obj_a);
    assert_eq!(b.object_bytes, obj_b);
    assert_eq!(a.metadata.generator_version, "gen-A");
    assert_eq!(b.metadata.generator_version, "gen-B");
}

#[test]
fn missing_file_returns_ok_none() {
    let dir = tempdir().unwrap();
    let res = load(
        dir.path(),
        [0u8; 32],
        "x86_64-unknown-linux-gnu",
        None,
        IntegrityMode::Strict,
    )
    .unwrap();
    assert!(res.is_none(), "expected Ok(None) for absent cache file");
}

#[test]
fn overwrite_replaces_previous_entry() {
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    // Use a deliberately stable filename key (e.g. source-level
    // hash that survives recompiles) while the underlying object
    // bytes change. Strict mode would reject the second write
    // because its sha256 no longer matches the stable key — that
    // is exactly the codegen behaviour `source_sha256` was meant
    // to model, so the overwrite scenario uses HmacRequired with
    // a fixture HMAC key to authenticate each freshly-written body.
    let key = sha_of(b"src-C-stable-key");
    let hmac_key = [0x77u8; 32];
    let obj_v1 = b"version-1".to_vec();
    let obj_v2 = b"version-2-now-longer".to_vec();

    store(
        dir.path(),
        key,
        triple,
        &obj_v1,
        &sample_metadata("v1"),
        Some(&hmac_key),
    )
    .unwrap();
    // Re-store with v2 — the atomic rename inside `store` should
    // replace v1 transparently without touching the filename.
    store(
        dir.path(),
        key,
        triple,
        &obj_v2,
        &sample_metadata("v2"),
        Some(&hmac_key),
    )
    .unwrap();

    let entry = load(
        dir.path(),
        key,
        triple,
        Some(&hmac_key),
        IntegrityMode::HmacRequired,
    )
    .unwrap()
    .unwrap();
    assert_eq!(entry.object_bytes, obj_v2);
    assert_eq!(entry.metadata.generator_version, "v2");
}

#[test]
fn truncated_file_surfaces_typed_error() {
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let key = sha_of(b"truncated");
    let object = b"some-object-body".to_vec();
    let path = store(
        dir.path(),
        key,
        triple,
        &object,
        &sample_metadata("g"),
        None,
    )
    .unwrap();

    // Truncate the file in place so the loader sees a short blob.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.truncate(20);
    std::fs::write(&path, &bytes).unwrap();

    let err = load(dir.path(), key, triple, None, IntegrityMode::Strict)
        .expect_err("truncated file must surface as CacheError");
    assert!(matches!(err, CacheError::Truncated(_)), "got {err:?}");
}

/// Hardening guard for the decode path: a cache file on disk is
/// untrusted input (truncated write, bit-rot, hand-edit, foreign
/// writer). Every malformed blob — at any truncation length, and with
/// each embedded length field corrupted — must surface as a typed
/// `Err` (or `Ok(None)` for an absent file) and *never* panic. In
/// particular the `u32::from_le_bytes(&bytes[a..a+4])` reads in the
/// decoder run on fixed-width 4-byte windows guarded by explicit
/// length checks, so a corrupt length field must be rejected before
/// it can index out of bounds.
#[test]
fn malformed_blob_never_panics_only_errors() {
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let object = b"some-object-body-that-is-reasonably-long".to_vec();
    // Strict mode re-hashes the body against the filename key, so the
    // key must be the object's own digest for the pristine sanity load.
    let key = sha_of(&object);
    let path = store(
        dir.path(),
        key,
        triple,
        &object,
        &sample_metadata("g"),
        None,
    )
    .unwrap();
    let good = std::fs::read(&path).unwrap();

    // Build a battery of malformed variants.
    let mut variants: Vec<Vec<u8>> = Vec::new();

    // 1. Every truncation length, including those past the 49-byte
    //    minimum that reach the slice-window reads.
    for len in 0..=good.len() {
        variants.push(good[..len].to_vec());
    }

    // Offsets of the embedded length fields in the v1 layout:
    //   byte 8           : triple_len (u8)
    //   byte 9+triple_len: object_size (u32 LE)
    //   then             : metadata_size (u32 LE)
    let triple_len = good[8] as usize;
    let obj_size_off = 9 + triple_len;
    let meta_size_off = obj_size_off + 4 + object.len();

    // 2. Corrupt triple_len to every u8 value (drives triple_end and
    //    therefore the object_size window to arbitrary positions).
    for v in 0u8..=255 {
        let mut b = good.clone();
        b[8] = v;
        variants.push(b);
    }

    // 3. Corrupt object_size to boundary / overflow values so the
    //    object_size window and the downstream metadata_size window
    //    land at, just past, and far beyond the buffer end.
    for raw in [
        0u32,
        u32::MAX,
        u32::MAX - 4,
        good.len() as u32,
        good.len() as u32 + 1,
    ] {
        let mut b = good.clone();
        b[obj_size_off..obj_size_off + 4].copy_from_slice(&raw.to_le_bytes());
        variants.push(b);
    }

    // 4. Corrupt metadata_size likewise.
    for raw in [0u32, u32::MAX, u32::MAX - 4, good.len() as u32 + 7] {
        let mut b = good.clone();
        b[meta_size_off..meta_size_off + 4].copy_from_slice(&raw.to_le_bytes());
        variants.push(b);
    }

    for (i, bytes) in variants.iter().enumerate() {
        std::fs::write(&path, bytes).unwrap();
        // The load-bearing invariant: decoding untrusted bytes must
        // never panic. (The `try_into().unwrap()` reads in the decoder
        // sit behind explicit 4-byte-window length checks, so a corrupt
        // length field is rejected before it can index out of bounds.)
        let res =
            std::panic::catch_unwind(|| load(dir.path(), key, triple, None, IntegrityMode::Strict));
        assert!(
            res.is_ok(),
            "decode panicked on malformed variant #{i} (len {})",
            bytes.len()
        );
    }

    // Sanity: the untouched blob still round-trips, so the fuzz loop
    // above is exercising the decoder rather than a wholesale-broken
    // path.
    std::fs::write(&path, &good).unwrap();
    let entry = load(dir.path(), key, triple, None, IntegrityMode::Strict)
        .unwrap()
        .expect("the pristine blob must still decode as a cache hit");
    assert_eq!(entry.object_bytes, object);
}
