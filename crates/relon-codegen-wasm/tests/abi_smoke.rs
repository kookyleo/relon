//! Phase 2.a `relon.abi` integration tests.
//!
//! Two angles of coverage:
//!
//! 1. **End-to-end emit + load:** compile a Phase 1.beta source through
//!    `compile_module`, then hand the bytes to `WasmModule::from_bytes`
//!    and assert the ABI metadata roundtrips with the placeholder
//!    schema hashes Phase 2.a is supposed to emit.
//! 2. **Loader failure surface:** corrupt the abi section's
//!    abi_version + strip the abi section entirely, and confirm the
//!    loader returns the matching `LoadError::Abi(...)` variant
//!    instead of silently parsing the module.
//!
//! These are deliberately Phase 2.a-shaped: schema hash validation
//! is *not* exercised here because the codegen pipeline doesn't yet
//! accept a schema input (both hashes encode as 32 zero bytes). That
//! shape comes online in Phase 2.b.

use relon_codegen_wasm::{abi, compile_lowered_entry, AbiError, LoadError, WasmModule};
use relon_eval_api::schema_canonical::schema_hash;
use relon_ir::lower_workspace_single;
use wasmparser::{Parser, Payload};

/// Compile a Relon source string end-to-end (parse, analyze, lower,
/// codegen). Mirrors the helper used by `srcmap_smoke.rs` so the two
/// test suites pin the same Phase 1.beta source shape.
fn compile_source(src: &str) -> Vec<u8> {
    let ast = relon_parser::parse_document(src).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    assert!(
        !analyzed.has_errors(),
        "analyzer reported errors: {:?}",
        analyzed.diagnostics
    );
    let ir = lower_workspace_single(&analyzed, &ast).expect("lower");
    compile_lowered_entry(&ir).expect("compile")
}

/// Same compile path but also expose the canonical schemas so tests
/// can hash them independently and confirm the embedded values match.
fn compile_source_with_schemas(
    src: &str,
) -> (
    Vec<u8>,
    relon_eval_api::schema_canonical::Schema,
    relon_eval_api::schema_canonical::Schema,
) {
    let ast = relon_parser::parse_document(src).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    assert!(
        !analyzed.has_errors(),
        "analyzer reported errors: {:?}",
        analyzed.diagnostics
    );
    let ir = lower_workspace_single(&analyzed, &ast).expect("lower");
    let bytes = compile_lowered_entry(&ir).expect("compile");
    (bytes, ir.main_schema, ir.return_schema)
}

/// Closure type used to mutate the abi section's payload in-place.
type AbiMutator<'a> = &'a dyn Fn(&mut [u8]);

/// Rebuild the wasm binary with the `relon.abi` section either
/// mutated in-place (via `mutate`) or removed entirely (when `mutate`
/// is `None`). Returns the new module bytes.
///
/// Locates the section's payload bytes via `wasmparser`'s
/// `data_offset()` (which points at the actual `RLNA...` payload, not
/// the outer custom-section header) so the mutation acts on the same
/// byte slice `abi::decode` would consume. Stripping the section
/// walks the parser a second time to identify every section's outer
/// extent and skip the matching range on copy.
fn rewrite_abi_section(bytes: &[u8], mutate: Option<AbiMutator<'_>>) -> Vec<u8> {
    // Locate the abi section: data_offset() is the byte index where
    // the payload starts; the payload's length is reader.data().len().
    let mut payload_range: Option<std::ops::Range<usize>> = None;
    // Track every section's `range()` so the strip path can splice
    // out the abi section without depending on the outer-header
    // length encoding.
    let mut sections_with_kind: Vec<(std::ops::Range<usize>, Option<String>)> = Vec::new();

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.expect("payload parse");
        match &payload {
            Payload::CustomSection(reader) => {
                let data_start = reader.data_offset();
                let data_end = data_start + reader.data().len();
                if reader.name() == abi::SECTION_NAME {
                    payload_range = Some(data_start..data_end);
                }
                sections_with_kind.push((reader.range(), Some(reader.name().to_string())));
            }
            Payload::TypeSection(r) => {
                sections_with_kind.push((r.range(), None));
            }
            Payload::FunctionSection(r) => {
                sections_with_kind.push((r.range(), None));
            }
            Payload::ExportSection(r) => {
                sections_with_kind.push((r.range(), None));
            }
            Payload::CodeSectionStart { range, .. } => {
                sections_with_kind.push((range.clone(), None));
            }
            _ => {}
        }
    }

    if let Some(f) = mutate {
        let payload_range = payload_range.expect("abi section payload not found");
        let mut new_bytes = bytes.to_vec();
        f(&mut new_bytes[payload_range]);
        return new_bytes;
    }

    // Strip path: splice the abi section out. `range()` covers the
    // section body (name + payload) but not the outer
    // `id_byte + size_varint` header. The wasm spec puts those just
    // before the body; we conservatively scan back at most 6 bytes
    // (1 id byte + up to 5 LEB128 size bytes) and confirm the id
    // byte is 0x00 (custom section marker) before extending the
    // strip range. The id byte is unambiguous because every section
    // body must start right after one.
    let (abi_section_range, _) = sections_with_kind
        .iter()
        .find(|(_, name)| name.as_deref() == Some(abi::SECTION_NAME))
        .expect("abi section in tracked list")
        .clone();

    // Walk back from `abi_section_range.start - 1` to find the id
    // byte 0x00 that introduces the custom section header. Between
    // the id byte and `range().start` lies the LEB128 size varint
    // (1..=5 bytes).
    let mut header_start = abi_section_range.start;
    for back in 1..=6 {
        if back > abi_section_range.start {
            break;
        }
        let candidate = abi_section_range.start - back;
        if bytes[candidate] == 0x00 {
            header_start = candidate;
            break;
        }
    }
    assert!(
        header_start < abi_section_range.start,
        "could not locate abi section header"
    );

    let mut out = bytes[..header_start].to_vec();
    out.extend_from_slice(&bytes[abi_section_range.end..]);
    out
}

#[test]
fn from_bytes_carries_real_schema_hashes() {
    // Phase 2.b: the abi section now embeds the sha256 of the
    // canonical `#main` schema (params + return) rather than zero
    // placeholders. The loader must round-trip those hashes byte-for-
    // byte, and they must match what the host computes from the same
    // canonical schemas the lowering pass produced.
    let (wasm, main_schema, return_schema) =
        compile_source_with_schemas("#main(Int x) -> Int\nx * 2");
    let module = WasmModule::from_bytes(wasm).expect("load");

    assert_eq!(module.abi().abi_version, abi::CURRENT_ABI_VERSION);
    assert_eq!(module.abi().codegen_version, abi::CURRENT_CODEGEN_VERSION);
    assert_ne!(
        module.abi().main_schema_hash,
        [0u8; 32],
        "Phase 2.b emits a real main_schema hash"
    );
    assert_eq!(module.abi().main_schema_hash, schema_hash(&main_schema));
    assert_eq!(module.abi().return_schema_hash, schema_hash(&return_schema));
    assert_eq!(module.abi().flags, 0);

    // The srcmap section must also be readable — Phase 1.gamma
    // guarantees at least one entry per emitted instruction plus the
    // function prologue.
    assert!(
        !module.srcmap().entries.is_empty(),
        "loader must parse the srcmap section alongside abi"
    );
}

#[test]
fn from_bytes_with_schema_accepts_matching_pair() {
    // Host knows the expected `#main` shape — `from_bytes_with_schema`
    // recomputes the canonical hash and compares against the
    // embedded value, returning Ok when they match.
    let (wasm, main_schema, return_schema) =
        compile_source_with_schemas("#main(Int x) -> Int\nx * 2");
    let module = WasmModule::from_bytes_with_schema(wasm, &main_schema, &return_schema)
        .expect("matching schemas must load");
    assert_eq!(module.abi().main_schema_hash, schema_hash(&main_schema));
}

#[test]
fn from_bytes_with_schema_rejects_main_drift() {
    // A reordered field set produces a different canonical hash, so
    // the loader must surface SchemaDrift { which: "main" }.
    let (wasm, _orig_main, return_schema) =
        compile_source_with_schemas("#main(Int a, Float b) -> Int\na");

    // Construct an "expected" schema with the params swapped.
    let drifted_main = relon_eval_api::schema_canonical::Schema {
        name: "MainParams".to_string(),
        generics: vec![],
        fields: vec![
            relon_eval_api::schema_canonical::Field {
                name: "b".to_string(),
                ty: relon_eval_api::schema_canonical::TypeRepr::Float,
                default: None,
            },
            relon_eval_api::schema_canonical::Field {
                name: "a".to_string(),
                ty: relon_eval_api::schema_canonical::TypeRepr::Int,
                default: None,
            },
        ],
    };

    match WasmModule::from_bytes_with_schema(wasm, &drifted_main, &return_schema) {
        Err(LoadError::Abi(AbiError::SchemaDrift { which: "main" })) => {}
        other => panic!("expected SchemaDrift on main, got {other:?}"),
    }
}

#[test]
fn from_bytes_with_schema_rejects_return_drift() {
    // Same module, but the caller claims the return field is Float.
    let (wasm, main_schema, _orig_return) =
        compile_source_with_schemas("#main(Int x) -> Int\nx * 2");

    let drifted_return = relon_eval_api::schema_canonical::Schema {
        name: "Ret".to_string(),
        generics: vec![],
        fields: vec![relon_eval_api::schema_canonical::Field {
            name: "value".to_string(),
            ty: relon_eval_api::schema_canonical::TypeRepr::Float,
            default: None,
        }],
    };

    match WasmModule::from_bytes_with_schema(wasm, &main_schema, &drifted_return) {
        Err(LoadError::Abi(AbiError::SchemaDrift { which: "return" })) => {}
        other => panic!("expected SchemaDrift on return, got {other:?}"),
    }
}

#[test]
fn from_bytes_rejects_abi_version_mismatch() {
    let wasm = compile_source("#main(Int x) -> Int\nx * 2");

    // Flip the abi_version bytes (offset 5..7 of the payload — see
    // `abi::encode` layout) from 1 to 2.
    let bad = rewrite_abi_section(
        &wasm,
        Some(&|payload: &mut [u8]| {
            payload[5] = 2;
            payload[6] = 0;
        }),
    );

    match WasmModule::from_bytes(bad) {
        Err(LoadError::Abi(AbiError::AbiMismatch { wanted, got })) => {
            assert_eq!(wanted, 1);
            assert_eq!(got, 2);
        }
        other => panic!("expected LoadError::Abi(AbiMismatch), got {other:?}"),
    }
}

#[test]
fn from_bytes_rejects_missing_abi_section() {
    let wasm = compile_source("#main(Int x) -> Int\nx * 2");
    let stripped = rewrite_abi_section(&wasm, None);

    match WasmModule::from_bytes(stripped) {
        Err(LoadError::Abi(AbiError::AbiSectionMissing)) => {}
        other => panic!("expected LoadError::Abi(AbiSectionMissing), got {other:?}"),
    }
}

#[test]
fn from_bytes_rejects_corrupted_abi_magic() {
    let wasm = compile_source("#main(Int x) -> Int\nx * 2");

    // Flip the first byte of the abi magic. Decoder must reject as
    // Corrupted rather than silently coercing.
    let bad = rewrite_abi_section(
        &wasm,
        Some(&|payload: &mut [u8]| {
            payload[0] = b'Z';
        }),
    );

    match WasmModule::from_bytes(bad) {
        Err(LoadError::Abi(AbiError::Corrupted)) => {}
        other => panic!("expected LoadError::Abi(Corrupted), got {other:?}"),
    }
}
