//! `relon.host_fns` custom section — Phase 6 emit + parse.
//!
//! Captures every `#native` function (host-provided import) the module
//! depends on so the host SDK can validate its registered bindings at
//! load time. See ADR-B (`wasm-adr-B-host-fn-schema-2026-05-16.md`)
//! for the rationale.

use relon_ir::IrType;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const SECTION_NAME: &str = "relon.host_fns";
pub const MAGIC: [u8; 4] = *b"RLNF";
pub const FORMAT_VERSION: u8 = 1;
pub const NO_CAPABILITY: u32 = u32::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFnEntry {
    pub name: String,
    pub params_canonical_hash: [u8; 32],
    pub ret_canonical_hash: [u8; 32],
    pub cap_bit: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostFnTable {
    pub entries: Vec<HostFnEntry>,
}

impl HostFnTable {
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn required_capabilities(&self) -> u64 {
        let mut bits: u64 = 0;
        for entry in &self.entries {
            if entry.cap_bit == NO_CAPABILITY {
                continue;
            }
            if entry.cap_bit < 64 {
                bits |= 1u64 << entry.cap_bit;
            }
        }
        bits
    }
}

/// Canonical 1-byte tag for an [`IrType`]. Stable across host SDK
/// versions because new `IrType` variants must append rather than
/// re-use existing bytes — `hash_params` / `hash_return` are part of
/// the wire format that flows through `relon.host_fns`.
fn ir_type_tag(ty: IrType) -> u8 {
    match ty {
        IrType::I32 => 0x01,
        IrType::I64 => 0x02,
        IrType::F64 => 0x03,
        IrType::Bool => 0x04,
        IrType::Null => 0x05,
        IrType::String => 0x06,
        IrType::ListInt => 0x07,
    }
}

/// Compute the canonical sha256 hash of a `#native` fn's parameter
/// list. Layout: `b"params" || u32 LE count || [u8 tag]*count`. The
/// fixed `"params"` prefix keeps the params + return hashes distinct
/// even when their tag bytes happen to coincide (an empty params
/// list versus an empty return — return is always one value, but the
/// prefix prevents future surprises).
pub fn hash_params(param_tys: &[IrType]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"params");
    hasher.update((param_tys.len() as u32).to_le_bytes());
    for ty in param_tys {
        hasher.update([ir_type_tag(*ty)]);
    }
    hasher.finalize().into()
}

/// Compute the canonical sha256 hash of a `#native` fn's return
/// type. Layout: `b"return" || [u8 tag]`. Mirrors [`hash_params`]
/// so drift on either side surfaces independently.
pub fn hash_return(ret_ty: IrType) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"return");
    hasher.update([ir_type_tag(ret_ty)]);
    hasher.finalize().into()
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum HostFnError {
    #[error("relon.host_fns section is corrupted")]
    Corrupted,
    #[error("relon.host_fns format_version {got} is newer than supported version 1")]
    FutureFormat { got: u8 },
    #[error("relon.host_fns payload truncated")]
    Truncated,
    #[error("relon.host_fns entry name is not valid utf-8")]
    InvalidUtf8,
}

pub fn encode(table: &HostFnTable) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + 80 * table.entries.len());
    out.extend_from_slice(&MAGIC);
    out.push(FORMAT_VERSION);
    write_varuint32(&mut out, table.entries.len() as u32);
    for entry in &table.entries {
        let name_bytes = entry.name.as_bytes();
        write_varuint32(&mut out, name_bytes.len() as u32);
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&entry.params_canonical_hash);
        out.extend_from_slice(&entry.ret_canonical_hash);
        write_varuint32(&mut out, entry.cap_bit);
    }
    out
}

pub fn decode(bytes: &[u8]) -> Result<HostFnTable, HostFnError> {
    if bytes.len() < 5 {
        return Err(HostFnError::Corrupted);
    }
    if bytes[0..4] != MAGIC {
        return Err(HostFnError::Corrupted);
    }
    let format_version = bytes[4];
    if format_version != FORMAT_VERSION {
        return Err(HostFnError::FutureFormat {
            got: format_version,
        });
    }
    let mut cur = 5usize;
    let entry_count = read_varuint32(bytes, &mut cur)?;
    let mut entries = Vec::with_capacity(entry_count as usize);
    for _ in 0..entry_count {
        let name_len = read_varuint32(bytes, &mut cur)? as usize;
        if cur
            .checked_add(name_len)
            .is_none_or(|end| end > bytes.len())
        {
            return Err(HostFnError::Truncated);
        }
        let name_slice = &bytes[cur..cur + name_len];
        let name = std::str::from_utf8(name_slice).map_err(|_| HostFnError::InvalidUtf8)?;
        cur += name_len;
        if cur + 64 > bytes.len() {
            return Err(HostFnError::Truncated);
        }
        let mut params_hash = [0u8; 32];
        params_hash.copy_from_slice(&bytes[cur..cur + 32]);
        cur += 32;
        let mut ret_hash = [0u8; 32];
        ret_hash.copy_from_slice(&bytes[cur..cur + 32]);
        cur += 32;
        let cap_bit = read_varuint32(bytes, &mut cur)?;
        entries.push(HostFnEntry {
            name: name.to_string(),
            params_canonical_hash: params_hash,
            ret_canonical_hash: ret_hash,
            cap_bit,
        });
    }
    Ok(HostFnTable { entries })
}

fn write_varuint32(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
            out.push(byte);
        } else {
            out.push(byte);
            return;
        }
    }
}

fn read_varuint32(bytes: &[u8], cur: &mut usize) -> Result<u32, HostFnError> {
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    loop {
        if *cur >= bytes.len() {
            return Err(HostFnError::Truncated);
        }
        let byte = bytes[*cur];
        *cur += 1;
        if shift >= 32 {
            return Err(HostFnError::Corrupted);
        }
        result |= ((byte & 0x7f) as u32)
            .checked_shl(shift)
            .ok_or(HostFnError::Corrupted)?;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(name: &str, seed: u8, cap_bit: u32) -> HostFnEntry {
        HostFnEntry {
            name: name.to_string(),
            params_canonical_hash: [seed; 32],
            ret_canonical_hash: [seed.wrapping_add(1); 32],
            cap_bit,
        }
    }

    #[test]
    fn empty_table_roundtrips() {
        let table = HostFnTable::empty();
        let bytes = encode(&table);
        assert_eq!(bytes.len(), 6);
        let decoded = decode(&bytes).expect("decode empty");
        assert_eq!(decoded, table);
    }

    #[test]
    fn single_entry_roundtrips() {
        let table = HostFnTable {
            entries: vec![sample_entry("echo", 0xAA, NO_CAPABILITY)],
        };
        let bytes = encode(&table);
        let decoded = decode(&bytes).expect("decode single");
        assert_eq!(decoded, table);
    }

    #[test]
    fn multi_entry_preserves_order() {
        let table = HostFnTable {
            entries: vec![
                sample_entry("alpha", 0x11, 0),
                sample_entry("beta", 0x22, 3),
                sample_entry("gamma_long_name_with_underscores", 0x33, NO_CAPABILITY),
            ],
        };
        let bytes = encode(&table);
        let decoded = decode(&bytes).expect("decode multi");
        assert_eq!(decoded, table);
        assert_eq!(decoded.entries[0].name, "alpha");
        assert_eq!(decoded.entries[2].name, "gamma_long_name_with_underscores");
    }

    #[test]
    fn corrupted_magic_is_rejected() {
        let table = HostFnTable {
            entries: vec![sample_entry("x", 0, NO_CAPABILITY)],
        };
        let mut bytes = encode(&table);
        bytes[0] = b'Z';
        let err = decode(&bytes).expect_err("must reject");
        assert!(matches!(err, HostFnError::Corrupted));
    }

    #[test]
    fn future_format_is_rejected() {
        let table = HostFnTable {
            entries: vec![sample_entry("x", 0, NO_CAPABILITY)],
        };
        let mut bytes = encode(&table);
        bytes[4] = 2;
        let err = decode(&bytes).expect_err("must reject");
        assert!(matches!(err, HostFnError::FutureFormat { got: 2 }));
    }

    #[test]
    fn truncated_payload_is_rejected() {
        let table = HostFnTable {
            entries: vec![sample_entry("echo", 0x55, 1)],
        };
        let bytes = encode(&table);
        let err = decode(&bytes[..bytes.len() - 1]).expect_err("must reject");
        assert!(
            matches!(err, HostFnError::Truncated | HostFnError::Corrupted),
            "got: {err:?}"
        );
    }

    #[test]
    fn empty_payload_is_corrupted() {
        let err = decode(&[]).expect_err("must reject");
        assert!(matches!(err, HostFnError::Corrupted));
    }

    #[test]
    fn required_capabilities_collects_bits() {
        let table = HostFnTable {
            entries: vec![
                sample_entry("a", 0, 0),
                sample_entry("b", 0, 2),
                sample_entry("c", 0, NO_CAPABILITY),
                sample_entry("d", 0, 5),
            ],
        };
        assert_eq!(table.required_capabilities(), 0b0010_0101);
    }

    #[test]
    fn varuint32_roundtrips_edge_values() {
        for value in [0u32, 1, 127, 128, 16383, 16384, u32::MAX] {
            let mut buf = Vec::new();
            write_varuint32(&mut buf, value);
            let mut cur = 0usize;
            let decoded = read_varuint32(&buf, &mut cur).expect("decode varuint");
            assert_eq!(decoded, value);
            assert_eq!(cur, buf.len());
        }
    }
}
