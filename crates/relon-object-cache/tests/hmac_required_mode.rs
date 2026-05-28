//! #171 regression: the `IntegrityMode::HmacRequired` mode must
//! refuse a load when the caller passes `hmac_key = None`. This is
//! belt-and-braces against future drift where a higher layer (the
//! codegen-cranelift object-cache integration) might resolve a `None`
//! key by accident — without the storage-layer guard, the loader
//! would otherwise silently fall back to a no-authentication read.

use relon_object_cache::{
    load, store, CacheError, HostFnImport, IntegrityMode, Metadata, SignatureHash,
};
use tempfile::tempdir;

fn fixture_metadata() -> Metadata {
    Metadata {
        host_fn_imports: vec![HostFnImport {
            name: "h".to_owned(),
            cap_bit: 0,
            params_hash: [0u8; 32],
            returns_hash: [0u8; 32],
        }],
        cap_bitmap: 0,
        main_signature: SignatureHash([0u8; 32]),
        created_at_unix: 0,
        generator_version: "test".to_owned(),
    }
}

#[test]
fn hmac_required_refuses_none_key() {
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let key = [0x42u8; 32];
    let object = b"authenticated-payload".to_vec();
    // Caller hashes a source-derived key into the filename stem —
    // exactly the codegen-cranelift usage pattern.
    let src_key = [0xAAu8; 32];

    // Write with HMAC so the file is authenticated on disk.
    store(
        dir.path(),
        src_key,
        triple,
        &object,
        &fixture_metadata(),
        Some(&key),
    )
    .unwrap();

    // Loading without a key in HmacRequired mode must error before
    // touching the file — even if the file's HMAC slot is populated,
    // the layer must not silently downgrade to no-auth.
    let err = load(
        dir.path(),
        src_key,
        triple,
        None,
        IntegrityMode::HmacRequired,
    )
    .expect_err("HmacRequired with None key must fail");
    assert!(
        matches!(err, CacheError::HmacKeyRequired),
        "expected HmacKeyRequired, got {err:?}"
    );
}

#[test]
fn hmac_required_passes_with_matching_key() {
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let key = [0x42u8; 32];
    let object = b"authenticated-payload".to_vec();
    let src_key = [0xAAu8; 32];

    store(
        dir.path(),
        src_key,
        triple,
        &object,
        &fixture_metadata(),
        Some(&key),
    )
    .unwrap();

    let entry = load(
        dir.path(),
        src_key,
        triple,
        Some(&key),
        IntegrityMode::HmacRequired,
    )
    .expect("hmac required pass")
    .expect("file present");
    assert_eq!(entry.object_bytes, object);
}

#[test]
fn hmac_required_catches_body_tamper_via_hmac_tag() {
    // The HMAC tag already covers the object bytes; in HmacRequired
    // mode the loader skips the SHA-256 recompute (the filename stem
    // is a source-derived key, not the object body's hash), so the
    // HMAC layer is the sole tamper detector. Verify it still
    // catches an in-place body swap.
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let key = [0x42u8; 32];
    let object = b"hmac-covered-body!!".to_vec();
    let src_key = [0xAAu8; 32];

    let path = store(
        dir.path(),
        src_key,
        triple,
        &object,
        &fixture_metadata(),
        Some(&key),
    )
    .unwrap();

    let mut bytes = std::fs::read(&path).unwrap();
    let obj_start = 4 + 4 + 1 + "x86_64-unknown-linux-gnu".len() + 4;
    for i in 0..object.len() {
        bytes[obj_start + i] ^= 0xff;
    }
    std::fs::write(&path, &bytes).unwrap();

    let err = load(
        dir.path(),
        src_key,
        triple,
        Some(&key),
        IntegrityMode::HmacRequired,
    )
    .expect_err("body tamper must surface as HmacMismatch");
    assert!(
        matches!(err, CacheError::HmacMismatch),
        "expected HmacMismatch, got {err:?}"
    );
}
