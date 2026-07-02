//! Security regression: in `Strict` mode (no HMAC key) the loader
//! must authenticate the security-relevant metadata trailer, not just
//! the object body. A local attacker with write access to the cache
//! directory could otherwise keep the object body byte-for-byte
//! identical and flip `cap_bitmap` in place to widen the granted
//! capabilities, and the load would still succeed.
//!
//! The fix folds `cap_bitmap` / `host_fn_imports` / `main_signature`
//! into the content-addressing key (`content_key`) that `Strict`
//! compares against the caller-supplied `source_sha256`. See
//! `storage::content_key`.

use relon_object_cache::{
    content_key, load, store, CacheError, HostFnImport, IntegrityMode, Metadata, SignatureHash,
};
use tempfile::tempdir;

const TRIPLE: &str = "x86_64-unknown-linux-gnu";

fn meta() -> Metadata {
    Metadata {
        host_fn_imports: vec![HostFnImport {
            name: "relon_host_log".to_owned(),
            cap_bit: 3,
            params_hash: [1u8; 32],
            returns_hash: [2u8; 32],
        }],
        cap_bitmap: 0b0000_0001,
        main_signature: SignatureHash([0xABu8; 32]),
        created_at_unix: 1_700_000_000,
        generator_version: "regression".to_owned(),
    }
}

/// Splice a tampered metadata blob back into an on-disk cache file,
/// keeping the object body and every length field byte-identical.
/// Returns the mutated `cap_bitmap` for assertions. Requires the
/// re-serialized metadata to have the same length (`cap_bitmap` is a
/// fixed-width `u64` in bincode, so a value-only edit preserves it).
fn tamper_cap_bitmap_on_disk(path: &std::path::Path, new_caps: u64) {
    let mut bytes = std::fs::read(path).unwrap();
    let triple_end = 9 + TRIPLE.len();
    let obj_size =
        u32::from_le_bytes(bytes[triple_end..triple_end + 4].try_into().unwrap()) as usize;
    let obj_start = triple_end + 4;
    let obj_before = bytes[obj_start..obj_start + obj_size].to_vec();

    let meta_start = obj_start + obj_size + 4;
    let meta_end = bytes.len() - 32;

    let mut m: Metadata = bincode::deserialize(&bytes[meta_start..meta_end]).unwrap();
    m.cap_bitmap = new_caps;
    let reser = bincode::serialize(&m).unwrap();
    assert_eq!(
        reser.len(),
        meta_end - meta_start,
        "cap_bitmap edit must not change the metadata length"
    );
    bytes[meta_start..meta_end].copy_from_slice(&reser);
    std::fs::write(path, &bytes).unwrap();

    // Object body untouched — proves the attack keeps the body intact.
    let after = std::fs::read(path).unwrap();
    assert_eq!(&after[obj_start..obj_start + obj_size], &obj_before[..]);
}

#[test]
fn strict_loads_untampered_entry() {
    let dir = tempdir().unwrap();
    let object = b"legitimate-object-body".to_vec();
    let m = meta();
    // Strict callers derive the filename stem from `content_key`.
    let src = content_key(&object, &m);
    store(dir.path(), src, TRIPLE, &object, &m, None).unwrap();

    let entry = load(dir.path(), src, TRIPLE, None, IntegrityMode::Strict)
        .expect("untampered load must succeed")
        .expect("cache hit expected");
    assert_eq!(entry.object_bytes, object);
    assert_eq!(entry.metadata, m);
}

#[test]
fn strict_rejects_in_place_cap_bitmap_tamper() {
    let dir = tempdir().unwrap();
    let object = b"body-stays-identical".to_vec();
    let m = meta();
    let src = content_key(&object, &m);
    let path = store(dir.path(), src, TRIPLE, &object, &m, None).unwrap();

    // The PoC: keep the object body untouched, flip cap_bitmap to
    // "all capabilities granted".
    tamper_cap_bitmap_on_disk(&path, 0xFFFF_FFFF_FFFF_FFFF);

    let err = load(dir.path(), src, TRIPLE, None, IntegrityMode::Strict)
        .expect_err("Strict must reject a forged cap_bitmap");
    assert!(matches!(err, CacheError::Sha256Mismatch), "got {err:?}");
}

#[test]
fn hmac_required_still_rejects_metadata_tamper() {
    // Belt-and-braces: the HMAC layer already covered header + object
    // + metadata; confirm the same in-place cap_bitmap tamper is still
    // caught in the production mode.
    let dir = tempdir().unwrap();
    let object = b"body-for-hmac-mode".to_vec();
    let m = meta();
    let hmac_key = [0x5Au8; 32];
    // HmacRequired routes a source-derived key through the stem.
    let src = [0xC3u8; 32];
    let path = store(dir.path(), src, TRIPLE, &object, &m, Some(&hmac_key)).unwrap();

    tamper_cap_bitmap_on_disk(&path, 0xFFFF_FFFF_FFFF_FFFF);

    let err = load(
        dir.path(),
        src,
        TRIPLE,
        Some(&hmac_key),
        IntegrityMode::HmacRequired,
    )
    .expect_err("HmacRequired must reject a forged cap_bitmap");
    assert!(matches!(err, CacheError::HmacMismatch), "got {err:?}");
}
