//! `relon.abi` custom section — Phase 2.a emit + parse.
//!
//! Binary layout matches `docs/internal/wasm-srcmap-section-v1-2026-05-16.md`,
//! Section 2:
//!
//! ```text
//! payload (84 bytes total):
//!   magic                  = b"RLNA"            (4 bytes)
//!   format_version         = u8 = 1             (1 byte)
//!   abi_version            : u16 LE             (2 bytes)
//!   codegen_version        : u32 LE             (4 bytes)
//!   main_schema_hash       : [u8; 32]           (32 bytes)
//!   return_schema_hash     : [u8; 32]           (32 bytes)
//!   flags                  : u8                  (1 byte)
//!   required_capabilities  : u64 LE             (8 bytes)
//! ```
//!
//! Host SDK instantiation flow (per the spec):
//!
//! 1. Read the `relon.abi` section. If absent, refuse-to-load with
//!    [`AbiError::AbiSectionMissing`].
//! 2. Validate the magic prefix; mismatch becomes [`AbiError::Corrupted`].
//! 3. Validate `format_version`; future versions become
//!    [`AbiError::FutureFormat`].
//! 4. Validate `abi_version` against [`CURRENT_ABI_VERSION`];
//!    mismatch becomes [`AbiError::AbiMismatch`].
//! 5. Compute the host-side schema hashes from the compile-time `#main`
//!    signature; mismatch becomes [`AbiError::SchemaDrift`]. (Schema
//!    hashing arrives in Phase 2.b — this phase emits zero placeholders.)
//!
//! Phase 2.a scope: the section is emitted with placeholder zeros for
//! both schema hashes. The shape is locked so the host loader machinery
//! can be written ahead of the codegen flip in Phase 2.b.

use thiserror::Error;

/// The `relon.abi` section name used when emitting / locating the
/// custom section in a wasm module.
pub const SECTION_NAME: &str = "relon.abi";

/// 4-byte magic prefix identifying a Relon ABI payload. Mismatches
/// during decode become [`AbiError::Corrupted`].
pub const MAGIC: [u8; 4] = *b"RLNA";

/// Current `format_version` byte. Distinct from `abi_version`: the
/// format version moves only when the binary shape of *this* section
/// changes, while `abi_version` bumps for any breaking change to the
/// wasm-side binary handshake layout.
pub const FORMAT_VERSION: u8 = 1;

/// Current ABI version expected by this codegen / host pair. Bumped
/// every time the binary handshake layout breaks (e.g. a new tag byte
/// in the `Option<T>` payload).
///
/// Phase 6 bumped from 1 to 2 because [`AbiMetadata`] gained the
/// `required_capabilities` slot — modules emitted by older codegen
/// versions can't carry the capability bitset, so host SDKs must
/// refuse-to-load them rather than silently treat the field as zero.
///
/// Phase 11 bumps from 2 to 3 because `run_main` gained a fifth
/// parameter (`caps_arg: i64`) and the imported `relon_caps_avail`
/// global was demoted to a module-internal mutable global. Old hosts
/// still call `run_main(in_ptr, in_len, out_ptr, out_cap)` — the
/// missing i64 would surface as a wasmtime arity mismatch, but
/// rejecting the load at the ABI gate gives a clean diagnostic.
pub const CURRENT_ABI_VERSION: u16 = 3;

/// Current codegen version. Advisory marker that bumps for any
/// observable codegen change so a host can include it in error
/// reports without affecting load-time refusal logic. Kept as a
/// placeholder semver-numeric value (`0x0001_0000` ~= 1.0.0) until
/// the codegen pipeline establishes a real release schedule.
pub const CURRENT_CODEGEN_VERSION: u32 = 0x0001_0000;

/// Total encoded size of an ABI payload in bytes. Matches the layout
/// laid out at the top of this module.
pub const PAYLOAD_SIZE: usize = 4 + 1 + 2 + 4 + 32 + 32 + 1 + 8;

/// Parsed / about-to-be-encoded `relon.abi` metadata.
///
/// The shape is intentionally narrow: every field corresponds 1:1 to
/// a byte slot in the encoded form. Schema hashes are stored as raw
/// 32-byte arrays so the host can `==` them against the result of
/// `crate::schema_canonical::schema_hash` without an allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbiMetadata {
    /// ABI version this module was compiled for. Host refuses to load
    /// when this disagrees with [`CURRENT_ABI_VERSION`].
    pub abi_version: u16,
    /// Codegen version stamp, advisory. Useful in error reports.
    pub codegen_version: u32,
    /// sha256 of the canonical `#main` schema. Placeholder zeros in
    /// Phase 2.a; populated by the codegen pipeline in Phase 2.b.
    pub main_schema_hash: [u8; 32],
    /// sha256 of the canonical return-type schema. Same placeholder
    /// rules as `main_schema_hash`.
    pub return_schema_hash: [u8; 32],
    /// Free-form flag bits. Bit 0 reserved for "sandboxed", bit 1
    /// reserved for "dhat-trace embedded" per the spec; v1 always
    /// emits `0` and a non-zero value here is a forward-compat
    /// marker the host should pass through, not refuse.
    pub flags: u8,
    /// Capability bitset the module requires before any `#native`
    /// import call. Codegen computes this as the OR of every
    /// `relon.host_fns` entry's `cap_bit`; bit `N` set means the
    /// module declared a `#native` fn that requires capability `N`.
    /// Host SDK refuses-to-load when the host's granted bitmap is
    /// not a superset (`cap_grants & required_capabilities ==
    /// required_capabilities`). Phase 6 lands the slot uninitialised
    /// to zero for callers that already build [`AbiMetadata`] by
    /// hand — `placeholder()` clears it; `compile_module` overwrites
    /// it with the host-fns table's collected bits.
    pub required_capabilities: u64,
}

impl AbiMetadata {
    /// Construct a Phase 2.a placeholder ABI record: current versions,
    /// zeroed schema hashes, cleared flags. Convenience for the
    /// `compile_module` emit site so callers do not have to repeat
    /// the same boilerplate at every codegen entry point.
    pub fn placeholder() -> Self {
        Self {
            abi_version: CURRENT_ABI_VERSION,
            codegen_version: CURRENT_CODEGEN_VERSION,
            main_schema_hash: [0u8; 32],
            return_schema_hash: [0u8; 32],
            flags: 0,
            required_capabilities: 0,
        }
    }
}

/// Reasons ABI parse / validate can fail.
///
/// `AbiMismatch`, `SchemaDrift`, and `AbiSectionMissing` are
/// load-time refusal classes the host SDK turns into
/// `RuntimeError::AbiSectionMissing` / `RuntimeError::AbiMismatch`
/// / `RuntimeError::SchemaDrift` at instantiation. `Corrupted` and
/// `FutureFormat` indicate the binary itself is wrong (truncated,
/// magic-mismatch, future-version) — always non-recoverable.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AbiError {
    /// Payload was shorter than [`PAYLOAD_SIZE`] or carried a magic
    /// prefix other than `b"RLNA"`. The decoder treats both as
    /// "wrong section" — host SDKs should propagate as a hard error
    /// rather than re-trying without ABI checks.
    #[error("relon.abi section is corrupted")]
    Corrupted,
    /// `format_version` byte is newer than this decoder knows about.
    /// Host SDK should refuse-to-load.
    #[error("relon.abi format_version {got} is newer than supported version 1")]
    FutureFormat {
        /// The format_version value observed in the binary.
        got: u8,
    },
    /// `abi_version` on the wire does not match [`CURRENT_ABI_VERSION`].
    /// Indicates the wasm module was compiled for an incompatible
    /// version of the binary handshake.
    #[error("relon.abi version mismatch: wanted {wanted}, got {got}")]
    AbiMismatch {
        /// Version the host SDK expects.
        wanted: u16,
        /// Version the wasm module declares.
        got: u16,
    },
    /// One of the two schema hashes (`main_schema_hash` or
    /// `return_schema_hash`) doesn't match what the host SDK derived
    /// from its compile-time schema. Surfaces a schema-drift bug —
    /// e.g. the host was rebuilt against a new `#main` signature but
    /// the wasm module is stale.
    #[error("relon.abi schema drift detected on {which} hash")]
    SchemaDrift {
        /// `"main"` or `"return"` depending on which hash mismatched.
        which: &'static str,
    },
    /// `relon.abi` section is absent from the module entirely. Always
    /// a producer bug — `compile_module` emits the section
    /// unconditionally.
    #[error("relon.abi section is missing")]
    AbiSectionMissing,
}

/// Encode an [`AbiMetadata`] to the raw custom-section payload bytes.
///
/// Output is exactly [`PAYLOAD_SIZE`] bytes long; the caller wraps
/// these bytes in a `wasm_encoder::CustomSection` named
/// [`SECTION_NAME`].
pub fn encode(meta: &AbiMetadata) -> Vec<u8> {
    let mut out = Vec::with_capacity(PAYLOAD_SIZE);
    out.extend_from_slice(&MAGIC);
    out.push(FORMAT_VERSION);
    out.extend_from_slice(&meta.abi_version.to_le_bytes());
    out.extend_from_slice(&meta.codegen_version.to_le_bytes());
    out.extend_from_slice(&meta.main_schema_hash);
    out.extend_from_slice(&meta.return_schema_hash);
    out.push(meta.flags);
    out.extend_from_slice(&meta.required_capabilities.to_le_bytes());
    debug_assert_eq!(out.len(), PAYLOAD_SIZE);
    out
}

/// Decode a `relon.abi` payload (without the outer wasm custom-section
/// header) into an [`AbiMetadata`].
///
/// Validates only the binary shape — `abi_version` / schema hash
/// matching against host expectations is the caller's job (host SDK
/// in Phase 2.b; the loader in [`crate::WasmModule::from_bytes`]
/// already wraps the abi-mismatch part for the `WasmModule` surface).
pub fn decode(bytes: &[u8]) -> Result<AbiMetadata, AbiError> {
    if bytes.len() < PAYLOAD_SIZE {
        return Err(AbiError::Corrupted);
    }

    // Magic prefix.
    if bytes[0..4] != MAGIC {
        return Err(AbiError::Corrupted);
    }

    // Format version.
    let format_version = bytes[4];
    if format_version != FORMAT_VERSION {
        return Err(AbiError::FutureFormat {
            got: format_version,
        });
    }

    let abi_version = u16::from_le_bytes([bytes[5], bytes[6]]);
    let codegen_version = u32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]);

    let mut main_schema_hash = [0u8; 32];
    main_schema_hash.copy_from_slice(&bytes[11..43]);

    let mut return_schema_hash = [0u8; 32];
    return_schema_hash.copy_from_slice(&bytes[43..75]);

    let flags = bytes[75];

    let required_capabilities = u64::from_le_bytes([
        bytes[76], bytes[77], bytes[78], bytes[79], bytes[80], bytes[81], bytes[82], bytes[83],
    ]);

    Ok(AbiMetadata {
        abi_version,
        codegen_version,
        main_schema_hash,
        return_schema_hash,
        flags,
        required_capabilities,
    })
}

/// Validate that an [`AbiMetadata`] matches the running SDK's
/// expectations. Catches `abi_version` drift here so the loader path
/// doesn't have to repeat the comparison.
///
/// Schema hash matching is *not* checked here in Phase 2.a — both
/// hashes are zeros at the moment, so any drift check would compare
/// zero against zero (or against a real host-side hash, which would
/// always disagree). Phase 2.b plumbs the host-side schema in and
/// turns this into the full validate.
pub fn check_versions(meta: &AbiMetadata) -> Result<(), AbiError> {
    if meta.abi_version != CURRENT_ABI_VERSION {
        return Err(AbiError::AbiMismatch {
            wanted: CURRENT_ABI_VERSION,
            got: meta.abi_version,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_metadata() -> AbiMetadata {
        AbiMetadata {
            abi_version: CURRENT_ABI_VERSION,
            codegen_version: 0x0001_2345,
            main_schema_hash: [0x11u8; 32],
            return_schema_hash: [0x22u8; 32],
            flags: 0,
            required_capabilities: 0x0000_0000_DEAD_BEEF,
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let original = sample_metadata();
        let bytes = encode(&original);
        assert_eq!(bytes.len(), PAYLOAD_SIZE);
        let decoded = decode(&bytes).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn corrupted_magic_is_rejected() {
        let mut bytes = encode(&sample_metadata());
        bytes[0] = b'Z';
        let err = decode(&bytes).expect_err("must reject");
        assert!(matches!(err, AbiError::Corrupted));
    }

    #[test]
    fn truncated_payload_is_rejected() {
        let bytes = encode(&sample_metadata());
        let err = decode(&bytes[..PAYLOAD_SIZE - 1]).expect_err("must reject");
        assert!(matches!(err, AbiError::Corrupted));
    }

    #[test]
    fn future_format_version_is_rejected() {
        let mut bytes = encode(&sample_metadata());
        bytes[4] = 2;
        let err = decode(&bytes).expect_err("must reject");
        assert!(matches!(err, AbiError::FutureFormat { got: 2 }));
    }

    #[test]
    fn abi_mismatch_detected_by_check_versions() {
        // Decoded metadata with an `abi_version` ahead of the SDK
        // must surface as `AbiMismatch` when validated against the
        // running SDK. Phase 6 pinned the current version to 2; we
        // bump the encoded value to `CURRENT_ABI_VERSION + 1` so the
        // test stays useful across future bumps.
        let mut bytes = encode(&sample_metadata());
        let drift = CURRENT_ABI_VERSION + 1;
        // abi_version is u16 LE at offset 5..7.
        bytes[5..7].copy_from_slice(&drift.to_le_bytes());
        let meta = decode(&bytes).expect("decode succeeds, version check is separate");
        assert_eq!(meta.abi_version, drift);
        let err = check_versions(&meta).expect_err("must reject");
        assert!(matches!(
            err,
            AbiError::AbiMismatch { wanted, got } if wanted == CURRENT_ABI_VERSION && got == drift
        ));
    }

    #[test]
    fn older_module_rejected_by_current_host_sdk() {
        // Every prior `abi_version` (1 = pre-Phase-6, 2 = pre-Phase-11)
        // describes a binary handshake the current host SDK no longer
        // speaks. Phase 6 added the `required_capabilities` slot;
        // Phase 11 moved the capability bitmap from an imported global
        // into a fifth `run_main` argument. Modules emitted by either
        // older codegen must refuse-to-load with `AbiMismatch` rather
        // than silently roundtripping through a half-compatible host.
        for older in 1u16..CURRENT_ABI_VERSION {
            let mut bytes = encode(&sample_metadata());
            bytes[5..7].copy_from_slice(&older.to_le_bytes());
            let meta = decode(&bytes).expect("decode still succeeds");
            let err = check_versions(&meta)
                .expect_err("current host must reject older abi_version modules");
            match err {
                AbiError::AbiMismatch { wanted, got } => {
                    assert_eq!(wanted, CURRENT_ABI_VERSION);
                    assert_eq!(got, older);
                }
                other => panic!("expected AbiMismatch, got {other:?}"),
            }
        }
    }

    #[test]
    fn required_capabilities_roundtrips() {
        let mut meta = sample_metadata();
        meta.required_capabilities = u64::MAX;
        let bytes = encode(&meta);
        // Encoded payload size now includes the 8-byte
        // required_capabilities slot.
        assert_eq!(bytes.len(), PAYLOAD_SIZE);
        let decoded = decode(&bytes).expect("decode");
        assert_eq!(decoded.required_capabilities, u64::MAX);
        assert_eq!(decoded, meta);
    }

    #[test]
    fn hash_bytes_survive_roundtrip() {
        // Belt-and-braces: every byte of both 32-byte hashes must
        // come back unchanged.
        let mut meta = sample_metadata();
        for (i, byte) in meta.main_schema_hash.iter_mut().enumerate() {
            *byte = i as u8;
        }
        for (i, byte) in meta.return_schema_hash.iter_mut().enumerate() {
            *byte = (255 - i) as u8;
        }
        let bytes = encode(&meta);
        let decoded = decode(&bytes).expect("decode");
        assert_eq!(decoded.main_schema_hash, meta.main_schema_hash);
        assert_eq!(decoded.return_schema_hash, meta.return_schema_hash);
    }

    #[test]
    fn placeholder_emits_current_versions_and_zero_hashes() {
        let meta = AbiMetadata::placeholder();
        assert_eq!(meta.abi_version, CURRENT_ABI_VERSION);
        assert_eq!(meta.codegen_version, CURRENT_CODEGEN_VERSION);
        assert_eq!(meta.main_schema_hash, [0u8; 32]);
        assert_eq!(meta.return_schema_hash, [0u8; 32]);
        assert_eq!(meta.flags, 0);
        assert_eq!(meta.required_capabilities, 0);
        // Roundtrip the placeholder so the constants encode correctly.
        let bytes = encode(&meta);
        let decoded = decode(&bytes).expect("decode placeholder");
        assert_eq!(decoded, meta);
    }
}
