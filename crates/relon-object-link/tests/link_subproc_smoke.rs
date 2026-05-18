//! Subprocess linker tests that do not need a real cranelift fixture.
//!
//! We use the platform `ld` (or `cc`) to verify failure-path error
//! mapping, plus a tiny synthetic input case to exercise the
//! `NotEtRel` guard before we ever fork the linker.

#![cfg(unix)]

use std::path::PathBuf;

use relon_object_link::{LinkError, SubprocLinker};

#[test]
fn nonexistent_linker_path_reports_linker_failed_or_not_found() {
    // `from_path_for_tests` deliberately skips the existence check so
    // we can drive the `Command::output()` failure branch.
    let linker = SubprocLinker::from_path_for_tests(PathBuf::from("/nonexistent/ld-totally-fake"));
    // Build a syntactically valid 64-bit LE ELF header with e_type =
    // ET_REL so the early-validation gate passes and we actually try
    // to fork the missing linker.
    let bytes = synthetic_et_rel_header();
    let err = linker
        .link(&bytes, "x86_64-unknown-linux-gnu")
        .expect_err("missing linker must surface as an error");
    assert!(
        matches!(err, LinkError::LinkerNotFound | LinkError::Io(_)),
        "expected LinkerNotFound or Io, got {err:?}"
    );
}

#[test]
fn rejects_non_elf_input_before_forking() {
    let linker = match SubprocLinker::new() {
        Ok(l) => l,
        Err(LinkError::LinkerNotFound) => {
            eprintln!("skipping: no system linker available");
            return;
        }
        Err(e) => panic!("unexpected linker discovery error: {e:?}"),
    };
    let bytes = b"definitely not an elf file at all";
    let err = linker
        .link(bytes, "x86_64-unknown-linux-gnu")
        .expect_err("non-elf input must fail validation");
    assert!(matches!(err, LinkError::InvalidElf(_)), "got {err:?}");
}

#[test]
fn rejects_et_dyn_input() {
    let linker = match SubprocLinker::new() {
        Ok(l) => l,
        Err(LinkError::LinkerNotFound) => return,
        Err(e) => panic!("{e:?}"),
    };
    // Build an ET_DYN-typed header and confirm `link` refuses to
    // re-link an already-linked output.
    let mut bytes = synthetic_et_rel_header();
    bytes[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    let err = linker
        .link(&bytes, "x86_64-unknown-linux-gnu")
        .expect_err("ET_DYN input must surface NotEtRel");
    assert!(
        matches!(err, LinkError::NotEtRel(relon_object_link::ElfType::Dyn)),
        "got {err:?}"
    );
}

#[test]
fn rejects_unsupported_triple() {
    let linker = match SubprocLinker::new() {
        Ok(l) => l,
        Err(LinkError::LinkerNotFound) => return,
        Err(e) => panic!("{e:?}"),
    };
    let bytes = synthetic_et_rel_header();
    let err = linker
        .link(&bytes, "aarch64-apple-darwin")
        .expect_err("non x86_64-linux triple must be rejected");
    assert!(
        matches!(err, LinkError::UnsupportedTriple(_)),
        "got {err:?}"
    );
}

#[test]
fn linker_failure_carries_stderr() {
    let linker = match SubprocLinker::new() {
        Ok(l) => l,
        Err(LinkError::LinkerNotFound) => return,
        Err(e) => panic!("{e:?}"),
    };
    // A syntactically valid ELF header with no section table is what
    // `ld` rejects with a recognisable error message. We then assert
    // the error variant + a substring of the captured stderr.
    let bytes = synthetic_et_rel_header();
    let err = linker
        .link(&bytes, "x86_64-unknown-linux-gnu")
        .expect_err("malformed object must surface LinkerFailed");
    match err {
        LinkError::LinkerFailed(msg) => {
            // Distros differ on exact wording; both binutils and lld
            // mention either "file format" or "input file" in the
            // failure message for a truncated object.
            let lower = msg.to_ascii_lowercase();
            assert!(
                lower.contains("file") || lower.contains("error") || lower.contains("relon"),
                "stderr did not surface a recognisable diagnostic: {msg}"
            );
        }
        // On some sandboxed builders `ld` itself is missing — accept
        // that as a skip rather than a hard failure.
        LinkError::LinkerNotFound => {
            eprintln!("skipping: linker disappeared between discovery and exec");
        }
        other => panic!("expected LinkerFailed, got {other:?}"),
    }
}

#[test]
fn ld_path_accessor_returns_resolved_path() {
    let linker = match SubprocLinker::new() {
        Ok(l) => l,
        Err(LinkError::LinkerNotFound) => return,
        Err(e) => panic!("{e:?}"),
    };
    let p = linker.ld_path();
    assert!(
        p.is_file(),
        "resolved ld_path should be a real binary: {}",
        p.display()
    );
}

/// 64-bit LE ELF header with `e_type = ET_REL`, padded to 64 bytes.
/// Insufficient to actually link (no section table), but enough to
/// satisfy the front-of-pipeline validation in `SubprocLinker::link`.
fn synthetic_et_rel_header() -> Vec<u8> {
    let mut bytes = vec![0u8; 64];
    bytes[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    bytes[4] = 2; // 64-bit
    bytes[5] = 1; // LE
    bytes[6] = 1; // EI_VERSION
    bytes[16..18].copy_from_slice(&1u16.to_le_bytes()); // ET_REL
    bytes
}
