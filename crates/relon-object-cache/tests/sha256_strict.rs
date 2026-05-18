//! Strict-integrity mode: any change to the object body must be
//! caught by the SHA-256 recompute, even when no HMAC key is in use.

use relon_object_cache::{
    load, store, CacheError, HostFnImport, IntegrityMode, Metadata, SignatureHash,
};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

fn meta() -> Metadata {
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
        generator_version: "t".to_owned(),
    }
}

fn sha(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

#[test]
fn strict_passes_on_unmodified_file() {
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let object = b"strict-mode-payload".to_vec();
    let src = sha(&object);
    store(dir.path(), src, triple, &object, &meta(), None).unwrap();

    let entry = load(dir.path(), src, triple, None, IntegrityMode::Strict)
        .unwrap()
        .unwrap();
    assert_eq!(entry.object_bytes, object);
}

#[test]
fn strict_rejects_mismatched_filename_hash() {
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let object = b"body-A".to_vec();
    // Use a key that intentionally does NOT match the object's
    // SHA-256 — simulates a writer that lied about the hash.
    let lied_key = sha(b"different-source");
    store(dir.path(), lied_key, triple, &object, &meta(), None).unwrap();

    let err = load(dir.path(), lied_key, triple, None, IntegrityMode::Strict)
        .expect_err("strict mode must catch hash mismatch");
    assert!(matches!(err, CacheError::Sha256Mismatch), "got {err:?}");
}

#[test]
fn trust_on_write_skips_recompute() {
    // Same setup as above; TrustOnWrite skips the recompute, so
    // the load must succeed even though the filename hash and the
    // object's hash disagree.
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let object = b"body-B".to_vec();
    let lied_key = sha(b"unrelated-source");
    store(dir.path(), lied_key, triple, &object, &meta(), None).unwrap();

    let entry = load(
        dir.path(),
        lied_key,
        triple,
        None,
        IntegrityMode::TrustOnWrite,
    )
    .unwrap()
    .unwrap();
    assert_eq!(entry.object_bytes, object);
}

#[test]
fn strict_catches_in_place_body_swap() {
    // Honest writer + tampered file: write a valid cache, then
    // swap the body bytes while keeping the filename and length
    // identical. Strict mode must catch.
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let object = b"body-len-12!".to_vec();
    assert_eq!(object.len(), 12);
    let src = sha(&object);
    let path = store(dir.path(), src, triple, &object, &meta(), None).unwrap();

    let mut bytes = std::fs::read(&path).unwrap();
    let obj_start = 4 + 4 + 1 + "x86_64-unknown-linux-gnu".len() + 4;
    for i in 0..object.len() {
        bytes[obj_start + i] = b'X';
    }
    std::fs::write(&path, &bytes).unwrap();

    let err = load(dir.path(), src, triple, None, IntegrityMode::Strict)
        .expect_err("strict mode must catch body swap");
    assert!(matches!(err, CacheError::Sha256Mismatch), "got {err:?}");
}
