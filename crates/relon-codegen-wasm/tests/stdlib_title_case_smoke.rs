//! v3++ b-4 `title(String) -> String` stdlib smoke tests + grapheme
//! awareness regression guards for `upper` / `lower`.
//!
//! Probes:
//! * `title` ASCII single-word and multi-word inputs.
//! * Non-ASCII whitespace separator (U+3000 ideographic space).
//! * Combining-mark grapheme contract — the `at_word_start` flag
//!   must not reset across a combining mark, so the cp following a
//!   base+mark sequence is treated as inside the same word.
//! * Emoji ZWJ sequence — ZWJ (U+200D) is *not* a Mark; the cps it
//!   joins are non-cased and pass through unchanged.
//! * Method-form dispatch (`s.title()`).
//! * `upper` / `lower` combining-mark identity passthrough — direct
//!   contrast with the v3+ a-4 behaviour where the table lookup did
//!   the same thing implicitly.
//!
//! All inputs use `\u{...}` escapes so the file stays ASCII-only and
//! the CJK-detection pre-commit hook stays happy.

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
const OUT_PTR: i32 = 2048;
const OUT_CAP: i32 = 2048;

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

/// Compile `op(s)` (free-call form) and run it against `input`.
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
// title() — ASCII (free-call form).
// ---------------------------------------------------------------------------

#[test]
fn title_ascii_single_word() {
    assert_eq!(fold_once("title", "hello"), "Hello");
}

#[test]
fn title_ascii_multi_word() {
    assert_eq!(fold_once("title", "hello world foo"), "Hello World Foo");
}

#[test]
fn title_ascii_already_titled_idempotent() {
    assert_eq!(fold_once("title", "Hello World"), "Hello World");
}

#[test]
fn title_ascii_all_upper_lowers_tail() {
    assert_eq!(fold_once("title", "THE QUICK BROWN"), "The Quick Brown");
}

#[test]
fn title_ascii_leading_trailing_spaces() {
    assert_eq!(fold_once("title", "  hello  world  "), "  Hello  World  ");
}

#[test]
fn title_ascii_multiple_separators() {
    // Tabs + newlines + space — all ASCII whitespace.
    assert_eq!(
        fold_once("title", "alpha\tbeta\ngamma"),
        "Alpha\tBeta\nGamma"
    );
}

#[test]
fn title_ascii_empty_string() {
    assert_eq!(fold_once("title", ""), "");
}

// ---------------------------------------------------------------------------
// title() — Latin with combining marks (grapheme contract).
// ---------------------------------------------------------------------------

#[test]
fn title_latin_with_precomposed_diacritic() {
    // cafe + U+00E9 (precomposed). The "é" (U+00E9) is the 4th cp
    // and folds to U+00C9 only when at_word_start; here it's in the
    // middle of the word so the lower-table identity applies.
    assert_eq!(fold_once("title", "caf\u{00E9}"), "Caf\u{00E9}");
}

#[test]
fn title_latin_with_combining_acute_grapheme_aware() {
    // "cafe" + U+0301 (combining acute). Naive cp-by-cp walks would
    // treat the combining mark as the 5th codepoint past "Cafe" and
    // do nothing (mark has no upper/lower mapping). The grapheme-
    // aware path here additionally leaves at_word_start untouched
    // across the mark, so the *next* word still title-cases.
    assert_eq!(
        fold_once("title", "cafe\u{0301} bar"),
        "Cafe\u{0301} Bar"
    );
}

#[test]
fn title_combining_mark_after_uppercase_letter_passes_through() {
    // CAFE + combining acute. Title-case lowers everything after the
    // first letter of the word, except marks which pass through.
    // Output: Cafe + U+0301.
    assert_eq!(fold_once("title", "CAFE\u{0301}"), "Cafe\u{0301}");
}

// ---------------------------------------------------------------------------
// title() — CJK + space-separated.
// ---------------------------------------------------------------------------

#[test]
fn title_cjk_with_ascii_space() {
    // Han ideographs U+4F60 U+597D are non-cased; pass through.
    // The trailing "hello" titles. ASCII space splits.
    assert_eq!(
        fold_once("title", "\u{4F60}\u{597D} hello"),
        "\u{4F60}\u{597D} Hello"
    );
}

#[test]
fn title_cjk_with_ideographic_space() {
    // U+3000 (ideographic space) is non-ASCII whitespace; the title
    // body's table-search path must catch it and reset
    // at_word_start.
    assert_eq!(
        fold_once("title", "hello\u{3000}world"),
        "Hello\u{3000}World"
    );
}

#[test]
fn title_cjk_passthrough_only() {
    // No cased codepoints anywhere — the body is essentially a copy
    // pass plus the table-driven word-boundary updates.
    assert_eq!(
        fold_once("title", "\u{4F60}\u{597D}\u{3001}\u{4E16}\u{754C}"),
        "\u{4F60}\u{597D}\u{3001}\u{4E16}\u{754C}"
    );
}

// ---------------------------------------------------------------------------
// title() — emoji ZWJ sequence.
// ---------------------------------------------------------------------------

#[test]
fn title_emoji_zwj_sequence_preserved() {
    // Family emoji: man + ZWJ + woman + ZWJ + girl + ZWJ + boy.
    // ZWJ (U+200D) is a Format char, NOT a Mark. The body emits each
    // cp verbatim (emoji are non-cased), so the entire sequence
    // round-trips. The flanking "hi" titles to "Hi".
    let zwj_family =
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
    let input = format!("hi {zwj_family}");
    let expected = format!("Hi {zwj_family}");
    assert_eq!(fold_once("title", &input), expected);
}

#[test]
fn title_emoji_with_variation_selector() {
    // Emoji + U+FE0F (Variation Selector-16). VS-16 *is* a Mark
    // (Mn) — the body emits it verbatim AND keeps at_word_start
    // untouched, so the next character (still in the same word)
    // doesn't get re-titled.
    assert_eq!(
        fold_once("title", "hello \u{2764}\u{FE0F} world"),
        "Hello \u{2764}\u{FE0F} World"
    );
}

// ---------------------------------------------------------------------------
// title() — method-form dispatch.
// ---------------------------------------------------------------------------

#[test]
fn title_method_form() {
    let (wasm, main_schema, return_schema) = compile("#main(String s) -> String\ns.title()");
    let in_bytes = build_str_input(&main_schema, "s", "the quick brown");
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(
        read_string_return(&return_schema, &out),
        "The Quick Brown"
    );
}

// ---------------------------------------------------------------------------
// upper() / lower() — combining-mark identity passthrough (v3++ b-4
// explicit check). The combining mark was identity-folded already in
// v3+ a-4 via the table miss path, but b-4 makes the skip explicit
// in the body. Regression guard against the b-6 full-folding pass
// accidentally walking marks through context-sensitive rules.
// ---------------------------------------------------------------------------

#[test]
fn upper_skips_combining_mark_explicitly() {
    // "cafe" + U+0301. Upper: "CAFE" + U+0301. The mark stays where
    // it was (after the now-uppercase "E" / U+0045) and is not
    // folded — verifies the new is_mark branch fires.
    assert_eq!(fold_once("upper", "cafe\u{0301}"), "CAFE\u{0301}");
}

#[test]
fn lower_skips_combining_mark_explicitly() {
    // "CAFE" + U+0301. Lower: "cafe" + U+0301.
    assert_eq!(fold_once("lower", "CAFE\u{0301}"), "cafe\u{0301}");
}

#[test]
fn upper_multiple_combining_marks() {
    // Letter "a" stacked with three combining marks.
    assert_eq!(
        fold_once("upper", "a\u{0301}\u{0302}\u{0303}"),
        "A\u{0301}\u{0302}\u{0303}"
    );
}
