//! Cache invalidation: a version-bumped or wrong-triple file must
//! surface a typed error so the host can regenerate.

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
fn wrong_triple_returns_triple_mismatch() {
    let dir = tempdir().unwrap();
    let object = b"obj".to_vec();
    let src = sha(&object);
    store(
        dir.path(),
        src,
        "x86_64-unknown-linux-gnu",
        &object,
        &meta(),
        None,
    )
    .unwrap();

    let err = load(
        dir.path(),
        src,
        "aarch64-unknown-linux-gnu",
        None,
        IntegrityMode::Strict,
    )
    .expect_err("triple mismatch must surface a typed error");
    match err {
        CacheError::TripleMismatch { file, runtime } => {
            assert_eq!(file, "x86_64-unknown-linux-gnu");
            assert_eq!(runtime, "aarch64-unknown-linux-gnu");
        }
        other => panic!("expected TripleMismatch, got {other:?}"),
    }
}

#[test]
fn bumped_version_returns_version_mismatch() {
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let object = b"obj".to_vec();
    let src = sha(&object);
    let path = store(dir.path(), src, triple, &object, &meta(), None).unwrap();

    // Rewrite the version u32 to something the runtime does not
    // understand. Offset 4..8.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let err = load(dir.path(), src, triple, None, IntegrityMode::Strict)
        .expect_err("version skew must surface typed error");
    match err {
        CacheError::VersionMismatch { file, runtime } => {
            assert_eq!(file, 99);
            assert_eq!(runtime, 1);
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

#[test]
fn bad_magic_returns_magic_mismatch() {
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let object = b"obj".to_vec();
    let src = sha(&object);
    let path = store(dir.path(), src, triple, &object, &meta(), None).unwrap();

    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0..4].copy_from_slice(b"XXXX");
    std::fs::write(&path, &bytes).unwrap();

    let err = load(dir.path(), src, triple, None, IntegrityMode::Strict)
        .expect_err("magic mismatch must surface typed error");
    assert!(matches!(err, CacheError::MagicMismatch), "got {err:?}");
}

#[test]
fn metadata_corruption_returns_metadata_error() {
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let object = b"obj-for-meta-test".to_vec();
    let src = sha(&object);
    let path = store(dir.path(), src, triple, &object, &meta(), None).unwrap();

    // Clobber the metadata section with garbage. Layout: magic(4)
    // + ver(4) + triple_len(1) + triple + obj_size(4) + obj +
    // meta_size(4) + meta + hmac(32). Find the metadata start.
    let mut bytes = std::fs::read(&path).unwrap();
    let triple_end = 9 + triple.len();
    let obj_size =
        u32::from_le_bytes(bytes[triple_end..triple_end + 4].try_into().unwrap()) as usize;
    let meta_start = triple_end + 4 + obj_size + 4;
    let hmac_start = bytes.len() - 32;
    for b in &mut bytes[meta_start..hmac_start] {
        *b = 0xFF;
    }
    std::fs::write(&path, &bytes).unwrap();

    let err = load(dir.path(), src, triple, None, IntegrityMode::Strict)
        .expect_err("garbage metadata must surface typed error");
    assert!(matches!(err, CacheError::Metadata(_)), "got {err:?}");
}

#[test]
fn small_garbage_file_returns_truncated() {
    let dir = tempdir().unwrap();
    // Write 5 bytes directly — way below the minimum layout.
    let path = relon_object_cache::storage::cache_path_for(dir.path(), [0u8; 32]);
    std::fs::create_dir_all(dir.path()).unwrap();
    std::fs::write(&path, b"hello").unwrap();

    let err = load(
        dir.path(),
        [0u8; 32],
        "x86_64-unknown-linux-gnu",
        None,
        IntegrityMode::Strict,
    )
    .expect_err("garbage file must surface typed error");
    assert!(matches!(err, CacheError::Truncated(_)), "got {err:?}");
}
