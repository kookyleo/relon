//! v3++ b-6 full Unicode case folding smoke tests.
//!
//! Probes the wasm-AOT default and locale-aware case folding stdlib
//! bodies (`upper` / `lower` / `title` / `upper_locale` /
//! `lower_locale` / `title_locale`).
//!
//! Scope split between wasm-AOT and tree-walk:
//!
//!   * The wasm-AOT body keeps the v3+ a-4 single-codepoint simple-
//!     folding contract for the default `upper` / `lower` / `title`
//!     entry points. Multi-codepoint mappings (`ß` -> `SS`, the
//!     Latin / Armenian ligatures) and the Greek final-sigma context
//!     are handled by the tree-walk evaluator's `fold_string` helper
//!     in `crates/relon-evaluator/src/stdlib.rs`; the matching tests
//!     for those live there.
//!
//!   * The locale-aware bodies (`upper_locale` / `lower_locale` /
//!     `title_locale`) accept a second `String` parameter and
//!     dispatch to the Turkish / Azerbaijani override table when the
//!     leading two-letter prefix matches `tr` / `az` (case-
//!     insensitive). Misses fall back to the default folding chain.
//!
//! All Unicode inputs use `\u{...}` escapes so the source stays
//! ASCII-only.

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
const OUT_PTR: i32 = 8192;
const OUT_CAP: i32 = 8192;

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

fn build_one_str(main_schema: &Schema, name: &str, value: &str) -> Vec<u8> {
    let main_layout = SchemaLayout::offsets_for(main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder
        .write_string(name, value)
        .expect("write string field");
    builder.finish()
}

fn build_two_str(
    main_schema: &Schema,
    name_a: &str,
    value_a: &str,
    name_b: &str,
    value_b: &str,
) -> Vec<u8> {
    let main_layout = SchemaLayout::offsets_for(main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder
        .write_string(name_a, value_a)
        .expect("write string field a");
    builder
        .write_string(name_b, value_b)
        .expect("write string field b");
    builder.finish()
}

fn read_str(return_schema: &Schema, out_bytes: &[u8]) -> String {
    let return_layout = SchemaLayout::offsets_for(return_schema).expect("return layout");
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, out_bytes).expect("reader");
    reader
        .read_string("value")
        .expect("read string value")
        .to_string()
}

fn run_unary(op: &str, input: &str) -> String {
    let src = format!("#main(String s) -> String\n{op}(s)");
    let (wasm, main_schema, return_schema) = compile(&src);
    let in_bytes = build_one_str(&main_schema, "s", input);
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    read_str(&return_schema, &out)
}

fn run_locale(op: &str, input: &str, locale: &str) -> String {
    let src = format!("#main(String s, String locale) -> String\n{op}(s, locale)");
    let (wasm, main_schema, return_schema) = compile(&src);
    let in_bytes = build_two_str(&main_schema, "s", input, "locale", locale);
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    read_str(&return_schema, &out)
}

// ---------------------------------------------------------------------------
// Locale dispatch — default (no-locale) behaviour
// ---------------------------------------------------------------------------

#[test]
fn default_lower_capital_i_with_dot_passes_through_identity() {
    // U+0130 (İ) has no 1:1 simple lowercase mapping (Rust's tables
    // expand it to `i\u{0307}` — a multi-codepoint case). The
    // wasm-AOT body keeps the simple-fold contract and emits the
    // input cp verbatim. The full multi-cp folding is on the
    // tree-walk side; see `crates/relon-evaluator/src/stdlib.rs`.
    let out = run_unary("lower", "\u{0130}stanbul");
    assert!(out.starts_with('\u{0130}'));
    assert!(out.ends_with("stanbul"));
}

#[test]
fn default_upper_passes_through_ss() {
    // `ß` (U+00DF) has no simple upper mapping — the wasm body keeps
    // it identity. (The full `ß -> SS` mapping is a tree-walk-side
    // feature; verified separately.)
    let out = run_unary("upper", "stra\u{00DF}e");
    assert_eq!(out, "STRA\u{00DF}E");
}

#[test]
fn default_upper_micro_sign() {
    // Micro sign (U+00B5) -> Greek capital mu (U+039C) is a simple
    // 1:1 fold so the wasm body handles it.
    assert_eq!(run_unary("upper", "\u{00B5}"), "\u{039C}");
}

// ---------------------------------------------------------------------------
// upper_locale — Turkish branch flips i -> İ
// ---------------------------------------------------------------------------

#[test]
fn upper_locale_tr_lowercase_i_to_dotted_i() {
    // The Turkish override maps `i` (U+0069) to `İ` (U+0130).
    assert_eq!(
        run_locale("upper_locale", "istanbul", "tr"),
        "\u{0130}STANBUL"
    );
}

#[test]
fn upper_locale_tr_capital_az_unchanged() {
    // ASCII uppercase already — no transformation.
    assert_eq!(run_locale("upper_locale", "AZ", "tr"), "AZ");
}

#[test]
fn upper_locale_en_default_path() {
    // English locale falls through to default — `i` -> `I`.
    assert_eq!(run_locale("upper_locale", "istanbul", "en"), "ISTANBUL");
}

#[test]
fn upper_locale_az_subtag_match() {
    // Subtag `az_AZ` selects the Turkish branch.
    assert_eq!(
        run_locale("upper_locale", "izmir", "az_AZ"),
        "\u{0130}ZM\u{0130}R"
    );
}

#[test]
fn upper_locale_tr_dotless_i_to_capital_i() {
    // Dotless lowercase `ı` (U+0131) -> capital `I` under Turkish.
    assert_eq!(run_locale("upper_locale", "\u{0131}", "tr"), "I");
}

// ---------------------------------------------------------------------------
// lower_locale — Turkish branch flips I -> ı, İ -> i
// ---------------------------------------------------------------------------

#[test]
fn lower_locale_tr_capital_i_to_dotless() {
    // Turkish `I` (U+0049) -> `ı` (U+0131).
    assert_eq!(run_locale("lower_locale", "I", "tr"), "\u{0131}");
}

#[test]
fn lower_locale_tr_capital_dotted_i_to_i() {
    // Turkish `İ` (U+0130) -> `i` (U+0069). Note the default
    // (non-locale) lowercase mapping is also `i` so the test stays
    // single-codepoint.
    assert_eq!(run_locale("lower_locale", "\u{0130}", "tr"), "i");
}

#[test]
fn lower_locale_tr_full_word() {
    // `ISTANBUL` -> `ıstanbul` in Turkish.
    assert_eq!(
        run_locale("lower_locale", "ISTANBUL", "tr"),
        "\u{0131}stanbul"
    );
}

#[test]
fn lower_locale_en_capital_i_to_lowercase_i() {
    // English: `I` -> `i`.
    assert_eq!(run_locale("lower_locale", "I", "en"), "i");
}

#[test]
fn lower_locale_uppercase_locale_tag_matches() {
    // Uppercase locale tag still hits the Turkish branch — locale
    // matching is case-insensitive on the two-letter prefix.
    assert_eq!(run_locale("lower_locale", "I", "TR"), "\u{0131}");
}

#[test]
fn lower_locale_az_with_region_subtag() {
    // `az-AZ` subtag selects Turkish branch.
    assert_eq!(run_locale("lower_locale", "I", "az-AZ"), "\u{0131}");
}

// ---------------------------------------------------------------------------
// title_locale — first cased cp uppercased, rest lowercased
// ---------------------------------------------------------------------------

#[test]
fn title_locale_tr_two_words() {
    // `istanbul izmir` under Turkish title-case: first cp of each
    // word uppercases via Turkish override (`i` -> `İ`), the rest
    // lower via simple table.
    assert_eq!(
        run_locale("title_locale", "istanbul izmir", "tr"),
        "\u{0130}stanbul \u{0130}zmir"
    );
}

#[test]
fn title_locale_en_simple() {
    // English title — `hello world` -> `Hello World`.
    assert_eq!(
        run_locale("title_locale", "hello world", "en"),
        "Hello World"
    );
}

// ---------------------------------------------------------------------------
// Locale boundary checks — `tr`-prefixed but not actually Turkish
// ---------------------------------------------------------------------------

#[test]
fn upper_locale_tron_is_not_turkish() {
    // `tron` is NOT a Turkish locale prefix (no `-` / `_` boundary
    // after the leading `tr`). Body falls through to default upper.
    assert_eq!(run_locale("upper_locale", "istanbul", "tron"), "ISTANBUL");
}

#[test]
fn upper_locale_empty_locale_is_default() {
    // Empty locale -> default branch.
    assert_eq!(run_locale("upper_locale", "i", ""), "I");
}

#[test]
fn upper_locale_method_form_works() {
    // Method-form receives the locale as its single explicit arg.
    let (wasm, main_schema, return_schema) =
        compile("#main(String s, String l) -> String\ns.upper_locale(l)");
    let in_bytes = build_two_str(&main_schema, "s", "istanbul", "l", "tr");
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_str(&return_schema, &out), "\u{0130}STANBUL");
}
