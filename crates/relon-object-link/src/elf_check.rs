//! Tiny hand-rolled 64-bit little-endian ELF header classifier.
//!
//! We only ever need to know:
//!
//! - Is this really an ELF file?
//! - Is it 64-bit + little-endian (the only thing v5-gamma supports)?
//! - Is `e_type` one of `ET_REL` / `ET_EXEC` / `ET_DYN`?
//!
//! Pulling `object` or `goblin` to read ~20 bytes would bloat the
//! dep graph for no benefit, so we parse the fields by hand. The
//! relevant offsets are fixed by the ELF spec (ELF-64 layout, see
//! `man 5 elf`):
//!
//! ```text
//! ofs  size  field        meaning
//!   0    4   e_ident[0..4] magic "\x7fELF"
//!   4    1   EI_CLASS     1 = 32-bit, 2 = 64-bit
//!   5    1   EI_DATA      1 = LE,     2 = BE
//!   6    1   EI_VERSION   always 1
//!  16    2   e_type       ET_REL=1 / ET_EXEC=2 / ET_DYN=3 / ET_CORE=4
//! ```

use crate::error::LinkError;

/// Subset of `e_type` values we care about. Everything we do not
/// recognise collapses into [`ElfType::Other`] so the caller can
/// surface a meaningful diagnostic without us re-encoding the full
/// ELF spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ElfType {
    /// `ET_REL` — relocatable object, what cranelift-object emits.
    Rel = 1,
    /// `ET_EXEC` — fully linked executable. Not loadable by dlopen.
    Exec = 2,
    /// `ET_DYN` — shared object, the one dlopen actually accepts.
    Dyn = 3,
    /// Anything else we currently do not classify (core dumps,
    /// vendor-specific OS types, …).
    Other,
}

/// Magic bytes that start every ELF file.
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
/// `EI_CLASS = 2` — 64-bit objects, the only thing we support.
const EI_CLASS_64: u8 = 2;
/// `EI_DATA = 1` — little-endian, fixed for x86_64 / aarch64.
const EI_DATA_LE: u8 = 1;
/// `EI_VERSION` is always 1 in any ELF the kernel will load.
const EI_VERSION_CURRENT: u8 = 1;
/// Minimum number of bytes we need to inspect — through `e_type`
/// which ends at offset 18.
const MIN_HEADER_LEN: usize = 18;

/// Parse the ELF type of `bytes` after validating the header is a
/// 64-bit little-endian ELF the rest of the pipeline can handle.
pub fn parse_elf_type(bytes: &[u8]) -> Result<ElfType, LinkError> {
    if bytes.len() < MIN_HEADER_LEN {
        return Err(LinkError::InvalidElf(format!(
            "buffer too short for elf header: {} < {}",
            bytes.len(),
            MIN_HEADER_LEN
        )));
    }
    if bytes[0..4] != ELF_MAGIC {
        return Err(LinkError::InvalidElf(format!(
            "missing ELF magic, got {:02x?}",
            &bytes[0..4]
        )));
    }
    if bytes[4] != EI_CLASS_64 {
        return Err(LinkError::InvalidElf(format!(
            "only 64-bit ELF supported, EI_CLASS={}",
            bytes[4]
        )));
    }
    if bytes[5] != EI_DATA_LE {
        return Err(LinkError::InvalidElf(format!(
            "only little-endian ELF supported, EI_DATA={}",
            bytes[5]
        )));
    }
    if bytes[6] != EI_VERSION_CURRENT {
        return Err(LinkError::InvalidElf(format!(
            "unexpected EI_VERSION={}",
            bytes[6]
        )));
    }
    // `e_type` is a little-endian u16 starting at offset 16.
    let raw = u16::from_le_bytes([bytes[16], bytes[17]]);
    Ok(match raw {
        1 => ElfType::Rel,
        2 => ElfType::Exec,
        3 => ElfType::Dyn,
        _ => ElfType::Other,
    })
}

/// Convenience predicate — `true` iff `bytes` is a valid 64-bit LE
/// ELF whose `e_type` is `ET_REL`. Returns `false` on any parse
/// failure rather than propagating; intended for cheap gating where
/// the caller does not care which validation step rejected the
/// bytes.
pub fn is_et_rel(bytes: &[u8]) -> bool {
    matches!(parse_elf_type(bytes), Ok(ElfType::Rel))
}

/// Convenience predicate — `true` iff `bytes` is a valid 64-bit LE
/// ELF whose `e_type` is `ET_DYN`.
pub fn is_et_dyn(bytes: &[u8]) -> bool {
    matches!(parse_elf_type(bytes), Ok(ElfType::Dyn))
}
