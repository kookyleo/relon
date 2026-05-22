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
    let meta = sample_metadata("relon-codegen-native 0.1.0");

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
