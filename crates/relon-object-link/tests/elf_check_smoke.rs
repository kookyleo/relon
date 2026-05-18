//! Unit-level smoke for the hand-rolled ELF header classifier.
//!
//! Each test crafts the minimum bytes needed to drive a single branch
//! of `parse_elf_type` — no real ELF compilation required, so this
//! file stays runnable on any host platform.

use relon_object_link::{is_et_dyn, is_et_rel, parse_elf_type, ElfType, LinkError};

/// Build a 64-bit LE ELF header with the given `e_type`. Everything
/// after offset 18 is zero — `parse_elf_type` only reads up to that
/// point, so a real `e_machine` is not needed.
fn elf_header(e_type: u16) -> Vec<u8> {
    let mut bytes = vec![0u8; 64];
    bytes[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    bytes[4] = 2; // EI_CLASS = 64-bit
    bytes[5] = 1; // EI_DATA  = LE
    bytes[6] = 1; // EI_VERSION = current
    bytes[16..18].copy_from_slice(&e_type.to_le_bytes());
    bytes
}

#[test]
fn parses_et_rel() {
    let bytes = elf_header(1);
    assert_eq!(parse_elf_type(&bytes).unwrap(), ElfType::Rel);
    assert!(is_et_rel(&bytes));
    assert!(!is_et_dyn(&bytes));
}

#[test]
fn parses_et_exec() {
    let bytes = elf_header(2);
    assert_eq!(parse_elf_type(&bytes).unwrap(), ElfType::Exec);
    assert!(!is_et_rel(&bytes));
    assert!(!is_et_dyn(&bytes));
}

#[test]
fn parses_et_dyn() {
    let bytes = elf_header(3);
    assert_eq!(parse_elf_type(&bytes).unwrap(), ElfType::Dyn);
    assert!(!is_et_rel(&bytes));
    assert!(is_et_dyn(&bytes));
}

#[test]
fn parses_et_other() {
    let bytes = elf_header(4); // ET_CORE
    assert_eq!(parse_elf_type(&bytes).unwrap(), ElfType::Other);
}

#[test]
fn rejects_non_elf_magic() {
    let mut bytes = elf_header(1);
    bytes[0] = b'M'; // break magic
    let err = parse_elf_type(&bytes).unwrap_err();
    assert!(
        matches!(err, LinkError::InvalidElf(ref m) if m.contains("magic")),
        "got {err:?}"
    );
}

#[test]
fn rejects_32_bit_elf() {
    let mut bytes = elf_header(1);
    bytes[4] = 1; // EI_CLASS = 32-bit
    let err = parse_elf_type(&bytes).unwrap_err();
    assert!(
        matches!(err, LinkError::InvalidElf(ref m) if m.contains("64-bit")),
        "got {err:?}"
    );
}

#[test]
fn rejects_big_endian_elf() {
    let mut bytes = elf_header(1);
    bytes[5] = 2; // EI_DATA = BE
    let err = parse_elf_type(&bytes).unwrap_err();
    assert!(
        matches!(err, LinkError::InvalidElf(ref m) if m.contains("little-endian")),
        "got {err:?}"
    );
}

#[test]
fn rejects_short_buffer() {
    let bytes = vec![0x7f, b'E', b'L', b'F']; // 4 bytes only
    let err = parse_elf_type(&bytes).unwrap_err();
    assert!(
        matches!(err, LinkError::InvalidElf(ref m) if m.contains("too short")),
        "got {err:?}"
    );
    assert!(!is_et_rel(&bytes));
    assert!(!is_et_dyn(&bytes));
}

#[test]
fn rejects_unknown_version() {
    let mut bytes = elf_header(1);
    bytes[6] = 7; // bogus EI_VERSION
    let err = parse_elf_type(&bytes).unwrap_err();
    assert!(
        matches!(err, LinkError::InvalidElf(ref m) if m.contains("EI_VERSION")),
        "got {err:?}"
    );
}
