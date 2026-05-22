//! Many threads loading the same entry simultaneously. All must
//! succeed and observe identical bytes — there is no internal
//! mutable state, so this is mostly a smoke test against future
//! regressions that introduce a non-thread-safe cache.

#![allow(deprecated)]

use relon_object_cache::{load, store, HostFnImport, IntegrityMode, Metadata, SignatureHash};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::thread;
use tempfile::tempdir;

fn meta() -> Metadata {
    Metadata {
        host_fn_imports: vec![HostFnImport {
            name: "concur".to_owned(),
            cap_bit: 0,
            params_hash: [3u8; 32],
            returns_hash: [4u8; 32],
        }],
        cap_bitmap: 1,
        main_signature: SignatureHash([0x77; 32]),
        created_at_unix: 12345,
        generator_version: "concurrent-test".to_owned(),
    }
}

fn sha(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

#[test]
fn eight_threads_load_same_entry() {
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let object: Vec<u8> = (0..1024).map(|i| (i & 0xFF) as u8).collect();
    let src = sha(&object);
    store(dir.path(), src, triple, &object, &meta(), None).unwrap();

    let dir = Arc::new(dir);
    let object = Arc::new(object);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let dir = dir.clone();
        let object = object.clone();
        handles.push(thread::spawn(move || {
            let entry = load(dir.path(), src, triple, None, IntegrityMode::Strict)
                .unwrap()
                .unwrap();
            assert_eq!(entry.object_bytes, *object);
            assert_eq!(entry.metadata.generator_version, "concurrent-test");
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn concurrent_writers_atomic_rename() {
    // Two threads write the same source hash concurrently. Both
    // must produce a syntactically valid file; the loader must
    // observe one of the two payloads (the last writer to rename
    // wins) and never a partial blob.
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let key = sha(b"contended-source");

    let dir1 = dir.path().to_path_buf();
    let dir2 = dir.path().to_path_buf();

    let t1 = thread::spawn(move || {
        let obj_a = vec![0xAAu8; 2048];
        store(&dir1, key, triple, &obj_a, &meta(), None).unwrap();
    });
    let t2 = thread::spawn(move || {
        let obj_b = vec![0xBBu8; 2048];
        store(&dir2, key, triple, &obj_b, &meta(), None).unwrap();
    });
    t1.join().unwrap();
    t2.join().unwrap();

    let entry = load(dir.path(), key, triple, None, IntegrityMode::TrustOnWrite)
        .unwrap()
        .unwrap();
    assert_eq!(entry.object_bytes.len(), 2048);
    assert!(
        entry.object_bytes.iter().all(|&b| b == 0xAA)
            || entry.object_bytes.iter().all(|&b| b == 0xBB),
        "loader saw a mix of both writers — atomic rename broken"
    );
}

#[test]
fn reads_during_overwrite_never_observe_partial_blob() {
    // One writer rewrites the entry repeatedly while N readers
    // hammer the same key. Readers either see the prior valid
    // version (Ok(Some)) or, on Linux, may transiently see NotFound
    // between unlink and rename — either is acceptable.
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    let key = sha(b"churn");
    let initial = vec![0xCCu8; 512];
    store(dir.path(), key, triple, &initial, &meta(), None).unwrap();

    let dir_path = dir.path().to_path_buf();
    let writer_path = dir_path.clone();
    let writer = thread::spawn(move || {
        for i in 0..20 {
            let body = vec![(0xD0 | (i & 0x0F)) as u8; 512];
            store(&writer_path, key, triple, &body, &meta(), None).unwrap();
        }
    });

    let mut readers = Vec::new();
    for _ in 0..4 {
        let dir_path = dir_path.clone();
        readers.push(thread::spawn(move || {
            for _ in 0..50 {
                // TrustOnWrite avoids the sha256 mismatch we would
                // hit since the writer uses a static `key` rather
                // than per-version hashes.
                match load(&dir_path, key, triple, None, IntegrityMode::TrustOnWrite) {
                    Ok(Some(entry)) => {
                        assert_eq!(entry.object_bytes.len(), 512);
                    }
                    Ok(None) => { /* transient between rename / unlink */ }
                    Err(e) => panic!("unexpected error during concurrent read: {e:?}"),
                }
            }
        }));
    }
    writer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }
}

#[test]
fn parallel_distinct_keys_dont_interfere() {
    let dir = tempdir().unwrap();
    let triple = "x86_64-unknown-linux-gnu";

    let dir1 = dir.path().to_path_buf();
    let dir2 = dir.path().to_path_buf();

    let h1 = thread::spawn(move || {
        for i in 0..50u32 {
            let body = format!("body-A-{}", i).into_bytes();
            let key = sha(&body);
            store(&dir1, key, triple, &body, &meta(), None).unwrap();
            let entry = load(&dir1, key, triple, None, IntegrityMode::Strict)
                .unwrap()
                .unwrap();
            assert_eq!(entry.object_bytes, body);
        }
    });
    let h2 = thread::spawn(move || {
        for i in 0..50u32 {
            let body = format!("body-B-{}", i).into_bytes();
            let key = sha(&body);
            store(&dir2, key, triple, &body, &meta(), None).unwrap();
            let entry = load(&dir2, key, triple, None, IntegrityMode::Strict)
                .unwrap()
                .unwrap();
            assert_eq!(entry.object_bytes, body);
        }
    });
    h1.join().unwrap();
    h2.join().unwrap();
}
