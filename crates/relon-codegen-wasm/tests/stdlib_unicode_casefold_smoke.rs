//! v3+ a-4 Unicode-aware case folding roundtrip tests.
//!
//! Covers the upgraded `upper(String) -> String` / `lower(String) -> String`
//! stdlib bodies — the Phase 4.c-2 ASCII fast path was replaced with a
//! UTF-8 decode + simple case-folding-table binary search + UTF-8
//! re-encode pipeline. The smoke tests here probe:
//!
//! * ASCII pass-through stays bit-identical (regression guard for the
//!   former fast path).
//! * Single-codepoint replacements across Latin, Greek, and Cyrillic
//!   scripts.
//! * Codepoints with no case mapping (Han, emoji) pass through
//!   verbatim.
//! * Mixed-script payloads (ASCII + non-ASCII codepoints) keep their
//!   relative ordering.
//! * Multi-codepoint mappings (German eszett -> SS) stay un-folded
//!   under the simple-folding contract — they survive as the original
//!   codepoint instead of being mangled.
//!
//! All Unicode input/output strings use `\u{...}` escapes so the test
//! source stays ASCII-only — keeping the file's bytes restricted to
//! ASCII makes the CJK-detection pre-commit hook happy and means the
//! test still reads cleanly when piped through tools that don't know
//! Unicode normalisation.

use relon_codegen_wasm::compile_lowered_entry;
use relon_eval_api::buffer::{BufferBuilder, BufferReader};
use relon_eval_api::layout::SchemaLayout;
use relon_eval_api::schema_canonical::Schema;
use relon_ir::lower_workspace_single;
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

fn compile(src: &str) -> (Vec<u8>, Schema, Schema) {
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

const IN_PTR: i32 = 0;
const OUT_PTR: i32 = 1024;
const OUT_CAP: i32 = 1024;

struct WasmSession {
    store: Store<()>,
    memory: Memory,
    run_main: TypedFunc<(i32, i32, i32, i32, i64), i32>,
}

impl WasmSession {
    fn new(bytes: &[u8]) -> Self {
        let engine = Engine::default();
        let module = Module::new(&engine, bytes).expect("module load");
        let mut store: Store<()> = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
        let memory = instance
            .get_memory(&mut store, "memory")
            .expect("memory export");
        let run_main = instance
            .get_typed_func::<(i32, i32, i32, i32, i64), i32>(&mut store, "run_main")
            .expect("run_main typed view");
        Self {
            store,
            memory,
            run_main,
        }
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) {
        self.memory
            .write(&mut self.store, offset, bytes)
            .expect("memory write");
    }

    fn read(&mut self, offset: usize, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        self.memory
            .read(&mut self.store, offset, &mut out)
            .expect("memory read");
        out
    }

    fn call(&mut self, in_ptr: i32, in_len: i32, out_ptr: i32, out_cap: i32) -> i32 {
        self.run_main
            .call(
                &mut self.store,
                (in_ptr, in_len, out_ptr, out_cap, i64::MAX),
            )
            .expect("run_main call must not trap")
    }
}

fn build_str_input(main_schema: &Schema, name: &str, value: &str) -> Vec<u8> {
    let main_layout = SchemaLayout::offsets_for(main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder
        .write_string(name, value)
        .expect("write string field");
    builder.finish()
}

fn read_string_return(return_schema: &Schema, out_bytes: &[u8]) -> String {
    let return_layout = SchemaLayout::offsets_for(return_schema).expect("return layout");
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, out_bytes).expect("reader");
    reader
        .read_string("value")
        .expect("read string value")
        .to_string()
}

/// Roundtrip helper for the most common shape: compile `upper(s)`
/// or `lower(s)`, push `input`, return the folded String.
fn fold_once(op: &str, input: &str) -> String {
    let src = format!("#main(String s) -> String\n{op}(s)");
    let (wasm, main_schema, return_schema) = compile(&src);
    let in_bytes = build_str_input(&main_schema, "s", input);
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    read_string_return(&return_schema, &out)
}

// ---------------------------------------------------------------------------
// ASCII pass-through (regression guards for the former ASCII fast path).
// ---------------------------------------------------------------------------

#[test]
fn upper_ascii_only() {
    assert_eq!(fold_once("upper", "hello"), "HELLO");
}

#[test]
fn lower_ascii_only() {
    assert_eq!(fold_once("lower", "HELLO"), "hello");
}

// ---------------------------------------------------------------------------
// Latin extended (single-codepoint mappings; same byte width).
//
// a-umlaut U+00E4 / A-umlaut U+00C4, e-acute U+00E9 / E-acute U+00C9,
// o-umlaut U+00F6 / O-umlaut U+00D6. Each is 2-byte UTF-8.
// ---------------------------------------------------------------------------

#[test]
fn upper_latin_with_diacritics() {
    assert_eq!(
        fold_once("upper", "\u{00E4}\u{00E9}\u{00F6}"),
        "\u{00C4}\u{00C9}\u{00D6}"
    );
}

#[test]
fn lower_latin_with_diacritics() {
    assert_eq!(
        fold_once("lower", "\u{00C4}\u{00C9}\u{00D6}"),
        "\u{00E4}\u{00E9}\u{00F6}"
    );
}

// ---------------------------------------------------------------------------
// Greek.
//
// Lowercase omega U+03C9 -> capital U+03A9; mu U+03BC -> U+039C;
// epsilon-with-tonos U+03AD -> U+0388; gamma U+03B3 -> U+0393;
// alpha U+03B1 -> U+0391. All 2-byte UTF-8.
// ---------------------------------------------------------------------------

#[test]
fn upper_greek_omega_lowercase() {
    assert_eq!(
        fold_once("upper", "\u{03C9}\u{03BC}\u{03AD}\u{03B3}\u{03B1}"),
        "\u{03A9}\u{039C}\u{0388}\u{0393}\u{0391}"
    );
}

#[test]
fn lower_greek_uppercase() {
    // Reverse direction. NOTE: Greek capital sigma (U+03A3) has a
    // context-sensitive folding (-> U+03C3 in word-medial position,
    // -> U+03C2 word-final) requiring a full case folding pass. The
    // simple folding pass maps U+03A3 -> U+03C3 uniformly. The Greek
    // payload below avoids sigma so the simple/full distinction
    // doesn't fire — the round-trip is exact under simple folding.
    assert_eq!(
        fold_once("lower", "\u{03A9}\u{039C}\u{0388}\u{0393}\u{0391}"),
        "\u{03C9}\u{03BC}\u{03AD}\u{03B3}\u{03B1}"
    );
}

// ---------------------------------------------------------------------------
// Cyrillic.
//
// Lowercase er U+0440 -> U+0420, o U+043E -> U+041E, es U+0441 ->
// U+0421, i U+0438 -> U+0418, ya U+044F -> U+042F. All 2-byte UTF-8.
// ---------------------------------------------------------------------------

#[test]
fn upper_cyrillic() {
    assert_eq!(
        fold_once("upper", "\u{0440}\u{043E}\u{0441}\u{0441}\u{0438}\u{044F}"),
        "\u{0420}\u{041E}\u{0421}\u{0421}\u{0418}\u{042F}"
    );
}

#[test]
fn lower_cyrillic() {
    assert_eq!(
        fold_once("lower", "\u{0420}\u{041E}\u{0421}\u{0421}\u{0418}\u{042F}"),
        "\u{0440}\u{043E}\u{0441}\u{0441}\u{0438}\u{044F}"
    );
}

// ---------------------------------------------------------------------------
// Mixed-script payloads — ASCII + Cyrillic mir U+043C U+0438 U+0440 /
// U+041C U+0418 U+0420.
// ---------------------------------------------------------------------------

#[test]
fn upper_mixed_ascii_unicode() {
    assert_eq!(
        fold_once("upper", "hello \u{043C}\u{0438}\u{0440}"),
        "HELLO \u{041C}\u{0418}\u{0420}"
    );
}

#[test]
fn lower_mixed_ascii_unicode() {
    assert_eq!(
        fold_once("lower", "HELLO \u{041C}\u{0418}\u{0420}"),
        "hello \u{043C}\u{0438}\u{0440}"
    );
}

// ---------------------------------------------------------------------------
// No-mapping codepoints (Han, emoji) — pass through verbatim.
//
// U+1F30D EARTH GLOBE EUROPE-AFRICA is a 4-byte UTF-8 codepoint with
// no case mapping. Han ideographs U+4F60 / U+597D (3-byte UTF-8 each)
// likewise have no Unicode case mapping.
// ---------------------------------------------------------------------------

#[test]
fn upper_emoji_passthrough() {
    assert_eq!(fold_once("upper", "hello \u{1F30D}"), "HELLO \u{1F30D}");
}

#[test]
fn lower_emoji_passthrough() {
    assert_eq!(fold_once("lower", "HELLO \u{1F30D}"), "hello \u{1F30D}");
}

#[test]
fn upper_han_passthrough() {
    assert_eq!(fold_once("upper", "\u{4F60}\u{597D}"), "\u{4F60}\u{597D}");
}

#[test]
fn lower_han_passthrough() {
    assert_eq!(fold_once("lower", "\u{4F60}\u{597D}"), "\u{4F60}\u{597D}");
}

// ---------------------------------------------------------------------------
// Multi-codepoint mappings — German eszett (U+00DF) -> SS is NOT in
// the simple folding table (it'd need a "full" pass), so the body
// leaves the codepoint alone. Documented behaviour for the v3+ a-4
// surface.
// ---------------------------------------------------------------------------

#[test]
fn upper_german_szlig_simple_folding_passthrough() {
    // "gross" with eszett (gro + U+00DF) -> "GRO" + U+00DF.
    // The eszett stays because simple folding has no single-codepoint
    // replacement for it. A v3++ full-folding pass would produce
    // "GROSS".
    assert_eq!(fold_once("upper", "gro\u{00DF}"), "GRO\u{00DF}");
}

#[test]
fn lower_strasse_already_lowercase_keeps_ss() {
    // STRASSE -> strasse. The two SS bytes lower to "ss" the same way
    // any other ASCII pair would; there is no simple-folding rule
    // that recombines them back into eszett.
    assert_eq!(fold_once("lower", "STRASSE"), "strasse");
}

// ---------------------------------------------------------------------------
// Single-codepoint corner cases.
// ---------------------------------------------------------------------------

#[test]
fn upper_single_codepoint() {
    assert_eq!(fold_once("upper", "a"), "A");
}

#[test]
fn lower_single_codepoint() {
    assert_eq!(fold_once("lower", "Z"), "z");
}

#[test]
fn upper_micro_sign_to_capital_mu() {
    // U+00B5 (micro sign, 2-byte UTF-8) maps under simple folding to
    // U+039C (Greek capital mu, also 2-byte UTF-8). A good example
    // of cross-script single-codepoint simple folding.
    assert_eq!(fold_once("upper", "\u{00B5}"), "\u{039C}");
}

// ---------------------------------------------------------------------------
// Method-form dispatch — `s.upper()` instead of `upper(s)`.
//
// `caf{e-acute}` (U+0063 U+0061 U+0066 U+00E9) round-trips against
// `CAF{E-acute}` (U+0043 U+0041 U+0046 U+00C9).
// ---------------------------------------------------------------------------

#[test]
fn upper_method_form_unicode() {
    let (wasm, main_schema, return_schema) = compile("#main(String s) -> String\ns.upper()");
    let in_bytes = build_str_input(&main_schema, "s", "caf\u{00E9}");
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_string_return(&return_schema, &out), "CAF\u{00C9}");
}

#[test]
fn lower_method_form_unicode() {
    let (wasm, main_schema, return_schema) = compile("#main(String s) -> String\ns.lower()");
    let in_bytes = build_str_input(&main_schema, "s", "CAF\u{00C9}");
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_string_return(&return_schema, &out), "caf\u{00E9}");
}
