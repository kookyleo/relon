//! v3++ b-5 Unicode normalization (UAX #15) smoke tests.
//!
//! Probes the four normalization stdlib bodies (`nfc` / `nfd` /
//! `nfkc` / `nfkd`) against UCD 14.0.0 data embedded by codegen-wasm.
//! Each test runs the compiled module under wasmtime and compares the
//! output against the same outputs the tree-walk evaluator produces
//! through `relon_ir::normalization` — both executors share the same
//! data tables so a divergence here means the wasm body lost a step.
//!
//! All inputs use `\u{...}` escapes so the file stays ASCII-only.

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
const OUT_PTR: i32 = 4096;
const OUT_CAP: i32 = 4096;

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

fn normalize_once(op: &str, input: &str) -> String {
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
// ASCII roundtrip — all four forms must leave ASCII strings unchanged.
// ---------------------------------------------------------------------------

#[test]
fn nfc_ascii_roundtrip() {
    assert_eq!(normalize_once("nfc", "hello world"), "hello world");
}

#[test]
fn nfd_ascii_roundtrip() {
    assert_eq!(normalize_once("nfd", "hello world"), "hello world");
}

#[test]
fn nfkc_ascii_roundtrip() {
    assert_eq!(normalize_once("nfkc", "hello world"), "hello world");
}

#[test]
fn nfkd_ascii_roundtrip() {
    assert_eq!(normalize_once("nfkd", "hello world"), "hello world");
}

// ---------------------------------------------------------------------------
// cafe with combining acute — NFC composes, NFD decomposes.
// ---------------------------------------------------------------------------

#[test]
fn nfc_composes_combining_acute() {
    let decomposed = "cafe\u{0301}";
    let composed = "caf\u{00E9}";
    assert_eq!(normalize_once("nfc", decomposed), composed);
}

#[test]
fn nfd_decomposes_precomposed_acute() {
    let composed = "caf\u{00E9}";
    let decomposed = "cafe\u{0301}";
    assert_eq!(normalize_once("nfd", composed), decomposed);
}

// ---------------------------------------------------------------------------
// Hangul algorithmic decomposition / composition.
// ---------------------------------------------------------------------------

#[test]
fn hangul_nfd_uses_algorithmic_decomposition() {
    let composed = "\u{D55C}";
    let decomposed = "\u{1112}\u{1161}\u{11AB}";
    assert_eq!(normalize_once("nfd", composed), decomposed);
}

#[test]
fn hangul_nfc_recomposes_jamos() {
    let composed = "\u{D55C}";
    let decomposed = "\u{1112}\u{1161}\u{11AB}";
    assert_eq!(normalize_once("nfc", decomposed), composed);
}

// ---------------------------------------------------------------------------
// Compatibility decomposition (NFKD / NFKC).
// ---------------------------------------------------------------------------

#[test]
fn nfkd_expands_half_fraction() {
    // U+00BD (one-half) -> "1" + U+2044 + "2"
    assert_eq!(normalize_once("nfkd", "\u{00BD}"), "1\u{2044}2");
}

#[test]
fn nfkc_expands_half_fraction_without_recompose() {
    assert_eq!(normalize_once("nfkc", "\u{00BD}"), "1\u{2044}2");
}

#[test]
fn nfd_leaves_compatibility_form_alone() {
    // NFD keeps U+00BD untouched — only NFKD touches the compatibility map.
    assert_eq!(normalize_once("nfd", "\u{00BD}"), "\u{00BD}");
}

// ---------------------------------------------------------------------------
// Canonical reorder — two combining marks with different CCC must sort.
// ---------------------------------------------------------------------------

#[test]
fn nfd_reorders_combining_marks_by_ccc() {
    // U+0307 has CCC 230; U+0323 has CCC 220. NFD swaps them so 220
    // precedes 230.
    let input = "a\u{0307}\u{0323}";
    let expected = "a\u{0323}\u{0307}";
    assert_eq!(normalize_once("nfd", input), expected);
}

// ---------------------------------------------------------------------------
// Idempotence checks across all four forms.
// ---------------------------------------------------------------------------

#[test]
fn nfc_idempotent_on_composed_input() {
    let s = "caf\u{00E9}";
    assert_eq!(normalize_once("nfc", s), s);
}

#[test]
fn nfd_idempotent_on_decomposed_input() {
    let s = "cafe\u{0301}";
    assert_eq!(normalize_once("nfd", s), s);
}

#[test]
fn nfkc_idempotent_on_expanded_input() {
    let s = "1\u{2044}2";
    assert_eq!(normalize_once("nfkc", s), s);
}

#[test]
fn nfkd_idempotent_on_expanded_input() {
    let s = "1\u{2044}2";
    assert_eq!(normalize_once("nfkd", s), s);
}

// ---------------------------------------------------------------------------
// Full_Composition_Exclusion: U+212A (Kelvin sign) must not recompose.
// ---------------------------------------------------------------------------

#[test]
fn nfc_skips_full_composition_exclusion_kelvin() {
    // U+212A decomposes canonically to 'K'. NFC must NOT recompose
    // 'K' back to U+212A (Full_Composition_Exclusion = True). The
    // composition pair table generator filters U+212A out so the
    // runtime never sees it.
    assert_eq!(normalize_once("nfc", "\u{212A}"), "K");
    assert_eq!(normalize_once("nfc", "K"), "K");
}

// ---------------------------------------------------------------------------
// Method-form dispatch sanity (s.nfc() / s.nfd() / s.nfkc() / s.nfkd()).
// ---------------------------------------------------------------------------

#[test]
fn nfc_method_form_composes() {
    let (wasm, main_schema, return_schema) = compile("#main(String s) -> String\ns.nfc()");
    let in_bytes = build_str_input(&main_schema, "s", "cafe\u{0301}");
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_string_return(&return_schema, &out), "caf\u{00E9}");
}
