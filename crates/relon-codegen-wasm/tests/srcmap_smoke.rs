//! Phase 1.gamma srcmap integration tests.
//!
//! Three angles of coverage:
//!
//! 1. **End-to-end emit:** compile a small Relon source through
//!    `compile_module` and confirm the emitted wasm carries a
//!    `relon.srcmap` custom section whose entry pcs all fall inside
//!    the module's code section.
//! 2. **Round-trip:** hand-build a `SrcMap`, encode it, decode it,
//!    and confirm field-for-field equality. This guards against
//!    silent encoder drift independent of `compile_module`.
//! 3. **Decoder failure surface:** corrupt the magic prefix and a
//!    future format version, and confirm the decoder reports the
//!    matching `SrcMapError` variant rather than papering over it.
//!
//! The end-to-end test also bounds entry pcs against the module's
//! code section so future codegen changes that accidentally write
//! the section before the code section (and thus end up with stale
//! offsets) fail loudly here rather than silently in production.

use relon_codegen_wasm::{compile_lowered_entry, srcmap, SrcMap, SrcMapEntry, SrcMapError};
use relon_ir::lower_workspace_single;
use wasmparser::{Parser, Payload};

/// Compile a Relon source string end-to-end (parse + analyze + lower
/// + codegen).
///
/// Mirrors the helper from `lowering_smoke.rs` so the srcmap tests
/// pin the same shape the engine smoke test exercises.
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

/// Pull the raw `relon.srcmap` custom section bytes out of a wasm
/// module. Panics if the section is missing — these tests treat
/// "section emitted" as a precondition.
fn extract_srcmap_section(bytes: &[u8]) -> &[u8] {
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.expect("payload");
        if let Payload::CustomSection(reader) = payload {
            if reader.name() == srcmap::SECTION_NAME {
                return reader.data();
            }
        }
    }
    panic!("module has no `relon.srcmap` custom section");
}

/// Pull `(code_section_start, code_section_end)` byte offsets out of
/// the module so the integration assert can bound where srcmap pcs
/// are allowed to point.
fn code_section_bounds(bytes: &[u8]) -> (u32, u32) {
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.expect("payload");
        if let Payload::CodeSectionStart {
            count: _,
            range,
            size: _,
        } = payload
        {
            return (range.start as u32, range.end as u32);
        }
    }
    panic!("module has no code section");
}

#[test]
fn srcmap_section_present_and_decodes_with_entries_inside_code_section() {
    // Phase 2.b `x * 2 + 1` lowers to:
    //   in_len guard       (6 ops)
    //   out_cap guard      (6 ops)
    //   load_field x       (2 ops: local.get + i64.load)
    //   i64.const 2
    //   i64.mul
    //   i64.const 1
    //   i64.add
    //   store_field ret    (4 ops: local.set tmp; local.get out_ptr;
    //                       local.get tmp; i64.store)
    //   i32.const ret_size (bytes_written)
    //   end
    // Plus one prologue srcmap entry pinning the function header.
    // That comes out to roughly 26 entries; assert `>= 20` to allow
    // future codegen tweaks without making the test brittle.
    let source = "#main(Int x) -> Int\nx * 2 + 1";
    let wasm = compile_source(source);

    let payload = extract_srcmap_section(&wasm);
    let srcmap = srcmap::decode_from_bytes(payload).expect("decode srcmap");

    assert!(
        !srcmap.entries.is_empty(),
        "srcmap should contain at least one entry"
    );
    assert!(
        srcmap.entries.len() >= 20,
        "expected >= 20 entries (handshake guards + body + epilogue + prologue), got {}",
        srcmap.entries.len()
    );

    // Single placeholder file slot until Phase 2 threads paths through.
    assert_eq!(srcmap.files.len(), 1);

    // Every pc must land inside the module's code section so wasmtime's
    // `module_offset()` can hit them at trap time. Phase 7 wires the
    // host-side translate_trap; here we only validate the producer
    // contract.
    let (code_start, code_end) = code_section_bounds(&wasm);
    for (i, e) in srcmap.entries.iter().enumerate() {
        assert!(
            e.pc >= code_start,
            "entry {i} pc={} is before code section start {code_start}",
            e.pc
        );
        assert!(
            e.pc < code_end,
            "entry {i} pc={} is past code section end {code_end}",
            e.pc
        );
        // file_idx must be valid against the file table.
        assert!((e.file_idx as usize) < srcmap.files.len());
        // line / col are 1-based per spec.
        assert!(e.line >= 1, "entry {i} line is 0");
        assert!(e.col >= 1, "entry {i} col is 0");
    }

    // Entries must be sorted ascending so a host-side binary search
    // is correct.
    for win in srcmap.entries.windows(2) {
        assert!(
            win[0].pc <= win[1].pc,
            "entries out of order: {} then {}",
            win[0].pc,
            win[1].pc
        );
    }
}

#[test]
fn handcrafted_srcmap_roundtrips_field_for_field() {
    // Hand-built to mimic the shape `compile_module` produces:
    //   - a single file_table entry
    //   - a leading prologue entry covering the function header
    //   - per-op entries with monotonically increasing pcs
    //   - a trailing "end" entry pointing back at the function range
    let original = SrcMap {
        files: vec!["main.relon".to_string()],
        entries: vec![
            SrcMapEntry {
                pc: 0x21,
                file_idx: 0,
                line: 1,
                col: 1,
                range_len: 18,
            },
            SrcMapEntry {
                pc: 0x23,
                file_idx: 0,
                line: 2,
                col: 1,
                range_len: 1,
            },
            SrcMapEntry {
                pc: 0x25,
                file_idx: 0,
                line: 2,
                col: 5,
                range_len: 1,
            },
            SrcMapEntry {
                pc: 0x27,
                file_idx: 0,
                line: 2,
                col: 3,
                range_len: 5,
            },
            SrcMapEntry {
                pc: 0x28,
                file_idx: 0,
                line: 1,
                col: 1,
                range_len: 18,
            },
        ],
    };

    let bytes = srcmap::encode_to_bytes(&original);
    let decoded = srcmap::decode_from_bytes(&bytes).expect("decode");
    assert_eq!(decoded, original);

    // The decoded form is usable for lookup. Sample queries land on
    // the entries we expect (binary search picks the largest pc <=
    // query).
    assert_eq!(decoded.lookup(0x21).map(|e| e.line), Some(1));
    assert_eq!(decoded.lookup(0x24).map(|e| e.line), Some(2)); // between 0x23 and 0x25
    assert_eq!(decoded.lookup(0x99).map(|e| e.pc), Some(0x28));
    assert!(decoded.lookup(0x00).is_none());
}

#[test]
fn corrupted_magic_is_rejected() {
    // Encode a real payload, then flip the first magic byte. The
    // decoder must report BadMagic — never silently parse a section
    // that happens to be valid wasm tooling junk.
    let mut bytes = srcmap::encode_to_bytes(&SrcMap::default());
    bytes[0] = b'Z';
    match srcmap::decode_from_bytes(&bytes) {
        Err(SrcMapError::BadMagic { got }) => {
            assert_eq!(got, [b'Z', b'L', b'N', b'S']);
        }
        other => panic!("expected BadMagic, got {other:?}"),
    }
}

#[test]
fn fake_future_format_version_is_rejected() {
    // Future-version sections must hard-fail rather than partially
    // parse. Bump format_version to a value newer than the codec
    // knows about.
    let mut bytes = srcmap::encode_to_bytes(&SrcMap::default());
    bytes[4] = 42;
    match srcmap::decode_from_bytes(&bytes) {
        Err(SrcMapError::FutureFormat { got, supported }) => {
            assert_eq!(got, 42);
            assert_eq!(supported, 1);
        }
        other => panic!("expected FutureFormat, got {other:?}"),
    }
}

#[test]
fn truncated_header_is_rejected() {
    let err = srcmap::decode_from_bytes(b"RL").expect_err("decoder must reject");
    assert!(matches!(err, SrcMapError::Truncated { .. }));
}
