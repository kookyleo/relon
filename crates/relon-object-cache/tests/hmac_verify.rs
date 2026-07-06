//! HMAC verification: a tampered byte must be rejected, an
//! unmodified file must round-trip, and a no-HMAC cache must still
//! load without a key.

#![allow(deprecated)]

use relon_object_cache::{
    compute_hmac, content_key, ensure_key, hmac_key_path, load, store, verify_hmac, CacheError,
    HmacError, HostFnImport, IntegrityMode, Metadata, SignatureHash,
};
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use tempfile::tempdir;

/// Serializes the tests that mutate `XDG_DATA_HOME` / `HOME`.
/// `std::env::set_var` is process-global while the test harness runs
/// `#[test]` fns on parallel threads, so every env-touching test must
/// hold this lock for its whole body.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn sample_meta() -> Metadata {
    Metadata {
        host_fn_imports: vec![HostFnImport {
            name: "relon_host_log".to_owned(),
            cap_bit: 0,
            params_hash: [9u8; 32],
            returns_hash: [10u8; 32],
        }],
        cap_bitmap: 1,
        main_signature: SignatureHash([0xCD; 32]),
        created_at_unix: 1_700_000_001,
        generator_version: "test".to_owned(),
    }
}

fn sha(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

#[test]
fn hmac_round_trips_when_key_matches() {
    let dir = tempdir().unwrap();
    let key = [0x42u8; 32];
    let triple = "x86_64-unknown-linux-gnu";
    let object = b"object-body-for-hmac-test".to_vec();
    // Strict mode also runs after the HMAC check, so the stem must be
    // the `content_key`, not the bare object digest.
    let src = content_key(&object, &sample_meta());

    store(dir.path(), src, triple, &object, &sample_meta(), Some(&key)).unwrap();
    let entry = load(dir.path(), src, triple, Some(&key), IntegrityMode::Strict)
        .unwrap()
        .unwrap();
    assert_eq!(entry.object_bytes, object);
}

#[test]
fn hmac_rejects_tampered_object_byte() {
    let dir = tempdir().unwrap();
    let key = [0x55u8; 32];
    let triple = "x86_64-unknown-linux-gnu";
    let object = b"original-payload-here".to_vec();
    let src = sha(&object);

    let path = store(dir.path(), src, triple, &object, &sample_meta(), Some(&key)).unwrap();

    // Flip exactly one byte deep in the object body so the SHA-256
    // check does not catch us first — HMAC must catch the change
    // regardless. `HmacRequired` skips the recompute (the filename
    // stem is still the object's own digest here, but the test is
    // about the HMAC layer's tamper detection in isolation).
    let mut bytes = std::fs::read(&path).unwrap();
    let target = 4 + 4 + 1 + "x86_64-unknown-linux-gnu".len() + 4 + 3;
    bytes[target] ^= 0x01;
    std::fs::write(&path, &bytes).unwrap();

    let err = load(
        dir.path(),
        src,
        triple,
        Some(&key),
        IntegrityMode::HmacRequired,
    )
    .expect_err("tampered byte must surface a HMAC mismatch");
    assert!(matches!(err, CacheError::HmacMismatch), "got {err:?}");
}

#[test]
fn no_hmac_key_skips_verification() {
    // A file written without a key must load without a key — and
    // also load with a key, because the HMAC slot stays zero and
    // the caller chose not to provide one for verification.
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let object = b"unsigned-cache".to_vec();
    let src = content_key(&object, &sample_meta());

    store(dir.path(), src, triple, &object, &sample_meta(), None).unwrap();
    let entry = load(dir.path(), src, triple, None, IntegrityMode::Strict)
        .unwrap()
        .unwrap();
    assert_eq!(entry.object_bytes, object);
}

#[test]
fn hmac_required_but_file_unsigned_fails() {
    // File was written without HMAC (tag is all zeros) but the
    // loader insists on one. Verification must fail.
    let dir = tempdir().unwrap();
    let key = [0x77u8; 32];
    let triple = "x86_64-unknown-linux-gnu";
    let object = b"unsigned-but-checked".to_vec();
    let src = sha(&object);

    store(dir.path(), src, triple, &object, &sample_meta(), None).unwrap();
    let err = load(dir.path(), src, triple, Some(&key), IntegrityMode::Strict)
        .expect_err("unsigned file must fail HMAC when key supplied");
    assert!(matches!(err, CacheError::HmacMismatch), "got {err:?}");
}

#[test]
fn compute_and_verify_helper_round_trips() {
    let key = [0x99u8; 32];
    let msg = b"some-payload";
    let tag = compute_hmac(msg, &key);
    assert!(verify_hmac(msg, &key, &tag));
    // Wrong key
    let other = [0x00u8; 32];
    assert!(!verify_hmac(msg, &other, &tag));
}

#[test]
fn ensure_key_creates_then_loads_idempotently() {
    let _guard = ENV_LOCK.lock().unwrap();
    // Isolate XDG_DATA_HOME to a tempdir so we do not touch the
    // user's real key.
    let dir = tempdir().unwrap();
    let saved = std::env::var_os("XDG_DATA_HOME");
    // SAFETY: ENV_LOCK serializes every env-mutating test in this
    // binary; we restore the env var before returning.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", dir.path());
    }

    let first = ensure_key().unwrap();
    let second = ensure_key().unwrap();
    assert_eq!(first, second, "ensure_key must be idempotent");

    let path = hmac_key_path().unwrap();
    assert!(path.starts_with(dir.path()));
    assert!(path.ends_with("cache-key"));

    // Restore the env var.
    unsafe {
        match saved {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }
}

#[test]
fn no_key_location_fails_closed() {
    let _guard = ENV_LOCK.lock().unwrap();
    // With neither XDG_DATA_HOME nor HOME set there is no trusted
    // directory for the key. The old behaviour fell back to a
    // cwd-relative `relon-cache-key` — which would let whoever
    // controls the working directory inject a key — so both the
    // path resolution and key generation must refuse instead.
    let saved_xdg = std::env::var_os("XDG_DATA_HOME");
    let saved_home = std::env::var_os("HOME");
    // SAFETY: ENV_LOCK serializes every env-mutating test in this
    // binary; both vars are restored before returning.
    unsafe {
        std::env::remove_var("XDG_DATA_HOME");
        std::env::remove_var("HOME");
    }

    let path_err = hmac_key_path().expect_err("no env → key path must fail");
    let key_err = ensure_key().expect_err("no env → key generation must fail");

    unsafe {
        match saved_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        match saved_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    assert!(matches!(path_err, HmacError::NoKeyLocation), "{path_err:?}");
    assert!(matches!(key_err, HmacError::NoKeyLocation), "{key_err:?}");
    // And nothing may have been dropped into the working directory.
    assert!(
        !std::path::Path::new("relon-cache-key").exists(),
        "cwd fallback key file must never be created"
    );
}
