//! S3 differential gate for the in-place region-walk return ABI, the
//! `List<String>` shape — the formation that originally segfaulted under
//! the old rigid-copy return path (the per-entry pointer array confused
//! the contiguous-block relocation). It now goes through the in-place
//! region-walk return instead of any copy.
//!
//! Asserts that a `#main(List<String> ss) -> List<String> = ss` identity
//! return produces **bit-identical** output — including every string's
//! bytes — on the cranelift-AOT backend, the llvm-AOT backend (gated
//! behind the `llvm-aot` feature), and the tree-walk golden oracle. Both
//! AOT backends report the negative sentinel `-(root_abs+1)` and the host
//! decodes the value in place at its source region through the one shared
//! pipeline (`relon_eval_api::inplace_return`): sentinel -> region-select
//! -> verifier -> `read_list_string_at`.
//!
//! Three layers:
//!  1. Hand-written string edge cases (empty string, empty list, single /
//!     many elements, CJK multibyte, very long, special control bytes
//!     inside valid UTF-8, alignment-boundary record lengths). CJK and
//!     emoji literals are built from code points (the codebase keeps
//!     source ASCII-only) but exercise the exact same multibyte payloads.
//!  2. A proptest generator feeding random `List<String>` values
//!     (strings drawn from empty / multibyte / long / boundary lengths)
//!     through the same differential — the auto-shrinking "shapes you
//!     didn't think of" net (the param-source form was the one originally
//!     missed).
//!  3. Loud-cap guards: parameter-*field* `List<String>` and
//!     `List<List<String>>` returns must still decline on **both** AOT
//!     backends, never silently mis-decode.

use std::collections::HashMap;
use std::sync::Arc;

use proptest::prelude::*;
use relon::{new_evaluator, Backend, BackendError};
use relon_eval_api::Value;
use relon_test_harness::assert_all_backends_bit_equal;

const SRC: &str = "#main(List<String> ss) -> List<String>\nss";

fn args(v: Value) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert("ss".to_string(), v);
    m
}

fn strs(items: &[&str]) -> Value {
    Value::List(Arc::new(
        items.iter().map(|s| Value::String((*s).into())).collect(),
    ))
}

fn list(items: Vec<String>) -> Value {
    Value::List(Arc::new(
        items.into_iter().map(|s| Value::String(s.into())).collect(),
    ))
}

/// Build a string from Unicode code points so the source file stays
/// ASCII-only while the runtime value carries the exact multibyte bytes.
fn from_cps(cps: &[u32]) -> String {
    cps.iter().map(|c| char::from_u32(*c).unwrap()).collect()
}

/// Run the differential and assert the AOT backends actually compiled the
/// shape (an in-place return must NOT silently fall back to a skip — the
/// whole point of S3 is that the backends express it). cranelift is always
/// asserted; llvm only under the `llvm-aot` feature.
fn assert_inplace(v: Value) {
    let report = assert_all_backends_bit_equal(SRC, args(v));
    assert!(
        report.cranelift_compared,
        "cranelift must compile the in-place List<String> return; skipped: {:?}",
        report.cranelift_skip_reason
    );
    #[cfg(feature = "llvm-aot")]
    assert!(
        report.llvm_compared,
        "llvm must compile the in-place List<String> return; skipped: {:?}",
        report.llvm_skip_reason
    );
}

// ---- hand-written edge cases ----------------------------------------

#[test]
fn empty_list() {
    assert_inplace(strs(&[]));
}

#[test]
fn single_empty_string() {
    assert_inplace(strs(&[""]));
}

#[test]
fn single_ascii() {
    assert_inplace(strs(&["x"]));
}

#[test]
fn empty_and_nonempty_mixed() {
    assert_inplace(strs(&["", "x", "", "abc", ""]));
}

#[test]
fn cjk_multibyte() {
    // U+4E2D U+6587 (3-byte each), U+65E5 U+672C U+8A9E, U+D55C U+AD6D U+C5B4.
    let zh = from_cps(&[0x4E2D, 0x6587]);
    let ja = from_cps(&[0x65E5, 0x672C, 0x8A9E]);
    let ko = from_cps(&[0xD55C, 0xAD6D, 0xC5B4]);
    let one = from_cps(&[0x4E2D]);
    assert_inplace(list(vec![zh, ja, ko, one, String::new()]));
}

#[test]
fn mixed_scripts_and_emoji() {
    // ascii+CJK, two crabs (U+1F980, 4-byte), accented latin, Greek.
    let mixed = format!("a{}b", from_cps(&[0x4E2D]));
    let crabs = from_cps(&[0x1F980, 0x1F980]);
    let cafe = from_cps(&[0x63, 0x61, 0x66, 0xE9]); // cafe with acute e
    let naive = from_cps(&[0x6E, 0x61, 0xEF, 0x76, 0x65]); // naive with diaeresis
    let greek = from_cps(&[0x3A9, 0x3BC, 0x3AD, 0x3B3, 0x3B1]);
    assert_inplace(list(vec![mixed, crabs, cafe, naive, greek]));
}

#[test]
fn very_long_string() {
    let long = "x".repeat(10_000);
    let long_cjk = from_cps(&[0x4E2D]).repeat(4_096);
    assert_inplace(list(vec![long, "short".to_string(), long_cjk]));
}

#[test]
fn special_control_bytes_inside_valid_utf8() {
    // Valid UTF-8 carrying control / whitespace / quote bytes (NOT
    // invalid UTF-8 — `Value::String` is always valid UTF-8, matching the
    // tree-walk oracle). Exercises the byte-exact payload copy.
    assert_inplace(strs(&[
        "a\tb",
        "line1\nline2",
        "q\"q",
        "\0nul",
        "\\back",
        "\r\n",
    ]));
}

#[test]
fn alignment_boundary_lengths() {
    // String records are `[len:4][utf8]`, 4-aligned. Sweep payload
    // lengths around the 4-byte boundary so the per-entry record padding
    // and the next entry's start are exercised at every residue.
    let lens: Vec<String> = (0..20).map(|n| "a".repeat(n)).collect();
    assert_inplace(list(lens));
}

#[test]
fn many_elements() {
    let v: Vec<String> = (0..500).map(|i| format!("item-{i}")).collect();
    assert_inplace(list(v));
}

// ---- proptest: the "shapes you didn't think of" net ------------------

/// A single string drawn from buckets that stress the ABI: empty,
/// short ascii, control/special bytes, multibyte CJK / emoji, and longer
/// / near alignment-boundary lengths. All are valid UTF-8 (the
/// `Value::String` invariant the oracle also upholds).
fn string_strat() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        "[a-z]{0,8}".prop_map(|s| s),
        "[\\t\\n\\r \"\\\\]{0,6}".prop_map(|s| s),
        prop::collection::vec(
            prop_oneof![
                Just(0x4E2Du32),  // CJK
                Just(0x1F980u32), // crab (4-byte)
                Just(0xE9u32),    // accented latin (2-byte)
                Just(0x3A9u32),   // Greek (2-byte)
            ],
            0..6
        )
        .prop_map(|cps| cps.iter().map(|c| char::from_u32(*c).unwrap()).collect()),
        (0usize..40).prop_map(|n| "x".repeat(n)),
    ]
}

fn list_string_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(string_strat(), 0..8).prop_map(list)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn diff_list_string(val in list_string_strat()) {
        let report = assert_all_backends_bit_equal(SRC, args(val));
        prop_assert!(report.cranelift_compared, "cranelift must compile the shape");
        #[cfg(feature = "llvm-aot")]
        prop_assert!(report.llvm_compared, "llvm must compile the shape");
    }
}

// ---- F4: parameter-FIELD List<String> return (`w.tags`) --------------
//
// `#main(W w) -> List<String>\nw.tags` — the returned list is `w`'s field,
// reached through a two-segment field walk. Post-F1 the field-load pushes
// the field list root's arena-absolute offset, identical to the param
// identity in-place return; bit-equal across tree-walk / cranelift / llvm
// (four-way incl. wasm in the llvm crate).

const SRC_W_TAGS: &str = "#schema W { tags: List<String>, n: Int }\n\
     #main(W w) -> List<String>\nw.tags";

fn w_tags(tags: &[&str], n: i64) -> HashMap<String, Value> {
    let map = std::collections::BTreeMap::from([
        (relon_eval_api::smol_str::SmolStr::from("tags"), strs(tags)),
        (relon_eval_api::smol_str::SmolStr::from("n"), Value::Int(n)),
    ]);
    let w = Value::branded_dict(map, Some("W".into()));
    let mut m = HashMap::new();
    m.insert("w".to_string(), w);
    m
}

#[test]
fn param_field_tags_cjk_empty_long() {
    let long = "x".repeat(4096);
    let report = assert_all_backends_bit_equal(
        SRC_W_TAGS,
        w_tags(&["", &from_cps(&[0x4E2D, 0x6587]), &long, "z"], 3),
    );
    assert!(report.cranelift_compared, "cranelift must compile w.tags");
    #[cfg(feature = "llvm-aot")]
    assert!(report.llvm_compared, "llvm must compile w.tags");
}

#[test]
fn param_field_tags_empty() {
    let report = assert_all_backends_bit_equal(SRC_W_TAGS, w_tags(&[], 0));
    assert!(report.cranelift_compared, "cranelift must compile w.tags");
    #[cfg(feature = "llvm-aot")]
    assert!(report.llvm_compared, "llvm must compile w.tags");
}

// ---- loud-cap guards: unsupported shapes decline, never miscompile ---

/// Shapes S3 does NOT lift must still make **both** AOT backends decline
/// the `#main` shape (a setup error). In production `Backend::Auto` falls
/// back to the tree-walk oracle; here we assert the decline is loud (an
/// `Err`), so a silent miscompile can never sneak in on either backend.
#[test]
fn unsupported_return_shapes_fail_loudly_not_silently() {
    let cap_cases = [
        // List<List<String>> — inner pointer-array-of-pointer-array; the
        // in-place reader does not decode a nested String pointer array (F5).
        "#main(List<List<String>> xss) -> List<List<String>>\nxss",
    ];
    for src in cap_cases {
        match new_evaluator(src, Backend::CraneliftAot) {
            Err(BackendError::CraneliftAot(_)) => { /* loud decline — correct */ }
            Err(other) => panic!("expected a CraneliftAot decline for `{src}`, got {other}"),
            Ok(_) => panic!(
                "cranelift unexpectedly accepted an unsupported in-place return shape: `{src}` — \
                 a silent-miscompile path may have opened"
            ),
        }
        #[cfg(feature = "llvm-aot")]
        match new_evaluator(src, Backend::LlvmAot) {
            Err(BackendError::LlvmAot(_)) => { /* loud decline — correct */ }
            Err(other) => panic!("expected an LlvmAot decline for `{src}`, got {other}"),
            Ok(_) => panic!(
                "llvm unexpectedly accepted an unsupported in-place return shape: `{src}` — \
                 a silent-miscompile path may have opened"
            ),
        }
    }
}
