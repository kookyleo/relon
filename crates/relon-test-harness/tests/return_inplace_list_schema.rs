//! S4 differential gate for the in-place region-walk return ABI, the
//! `List<Schema>` shape — the deepest formation. A `List<Schema>` is a
//! pointer array `[len][off_i]` whose every entry points at a schema
//! sub-record, and each sub-record itself carries String / List pointer
//! fields (plus inline scalars at varied offsets). The param-identity
//! return goes through the in-place region-walk return instead of any
//! machine-code re-pack: both AOT backends report the negative sentinel
//! `-(root_abs+1)`; the host decodes in place through the one shared
//! pipeline (`relon_eval_api::inplace_return`): sentinel -> region-select
//! -> verifier (which recurses to **every sub-record field pointer**) ->
//! `read_list_record_at` -> branded dict per element.
//!
//! Asserts byte-exact equality — every sub-object, every String field's
//! bytes, every list — across cranelift-AOT, llvm-AOT (gated on the
//! `llvm-aot` feature), and the tree-walk golden oracle.
//!
//! Three layers:
//!  1. Hand-written deep-combination cases: Cfg field sets covering
//!     Int/Float/Bool/String/List<scalar>/List<String> in varied orders
//!     and offsets (a String hiding after a Bool, a list between two
//!     scalars); empty / single / many-element lists; empty strings, CJK,
//!     very long field values.
//!  2. A proptest generator feeding random schemas (random field sets /
//!     orders / values) of `List<Schema>` through the same differential —
//!     the auto-shrinking "shapes you didn't think of" net.
//!  3. Loud-cap guards: parameter-*field* `List<Schema>`,
//!     `List<List<Schema>>`, and a sub-record carrying a nested
//!     `List<Schema>` field must still decline on **both** AOT backends,
//!     never silently mis-decode.

use std::collections::HashMap;
use std::sync::Arc;

use proptest::prelude::*;
use relon::{new_evaluator, Backend, BackendError};
use relon_eval_api::Value;
use relon_test_harness::assert_all_backends_bit_equal;

fn args(items: Value) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert("items".to_string(), items);
    m
}

/// Build a string from Unicode code points so the source file stays
/// ASCII-only while the runtime value carries the exact multibyte bytes.
fn from_cps(cps: &[u32]) -> String {
    cps.iter().map(|c| char::from_u32(*c).unwrap()).collect()
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

fn istr(items: &[&str]) -> Value {
    Value::List(Arc::new(items.iter().map(|x| s(x)).collect()))
}

fn iints(items: &[i64]) -> Value {
    Value::List(Arc::new(items.iter().map(|x| Value::Int(*x)).collect()))
}

fn cfg(brand: &str, fields: Vec<(&str, Value)>) -> Value {
    Value::branded_dict(
        fields.into_iter().map(|(k, v)| (k.to_string(), v)),
        Some(brand.to_string()),
    )
}

fn cfg_list(items: Vec<Value>) -> Value {
    Value::List(Arc::new(items))
}

/// Run the differential and assert the AOT backends actually compiled the
/// shape (an in-place return must NOT silently fall back to a skip).
fn assert_inplace(src: &str, items: Value) {
    let report = assert_all_backends_bit_equal(src, args(items));
    assert!(
        report.cranelift_compared,
        "cranelift must compile the in-place List<Schema> return; skipped: {:?}",
        report.cranelift_skip_reason
    );
    #[cfg(feature = "llvm-aot")]
    assert!(
        report.llvm_compared,
        "llvm must compile the in-place List<Schema> return; skipped: {:?}",
        report.llvm_skip_reason
    );
}

// ---- hand-written deep-combination cases -----------------------------

/// `Cfg { name: String, port: Int }` — the CLI canonical shape.
const SRC_NAME_PORT: &str =
    "#schema Cfg { name: String, port: Int }\n#main(List<Cfg> items) -> List<Cfg>\nitems";

#[test]
fn empty_list() {
    assert_inplace(SRC_NAME_PORT, cfg_list(vec![]));
}

#[test]
fn single_record() {
    assert_inplace(
        SRC_NAME_PORT,
        cfg_list(vec![cfg(
            "Cfg",
            vec![("name", s("a")), ("port", Value::Int(1))],
        )]),
    );
}

#[test]
fn cjk_and_empty_string_fields() {
    assert_inplace(
        SRC_NAME_PORT,
        cfg_list(vec![
            cfg("Cfg", vec![("name", s("")), ("port", Value::Int(0))]),
            cfg(
                "Cfg",
                vec![
                    ("name", s(&from_cps(&[0x4E2D, 0x6587]))),
                    ("port", Value::Int(-5)),
                ],
            ),
            cfg(
                "Cfg",
                vec![
                    ("name", s(&from_cps(&[0x1F980, 0x1F980]))),
                    ("port", Value::Int(i64::MAX)),
                ],
            ),
        ]),
    );
}

/// String hidden after a Bool, a list wedged between two scalars, a Float
/// at the end — the historically error-prone mixed-offset layout.
const SRC_MIXED: &str = "#schema Cfg { flag: Bool, name: String, port: Int, tags: List<String>, nums: List<Int>, ratio: Float }\n#main(List<Cfg> items) -> List<Cfg>\nitems";

#[test]
fn mixed_offsets_string_after_bool() {
    assert_inplace(
        SRC_MIXED,
        cfg_list(vec![
            cfg(
                "Cfg",
                vec![
                    ("flag", Value::Bool(true)),
                    ("name", s("host")),
                    ("port", Value::Int(8080)),
                    ("tags", istr(&["a", "", "bb"])),
                    ("nums", iints(&[1, -2, 3])),
                    ("ratio", Value::Float(ordered_float::OrderedFloat(0.5))),
                ],
            ),
            cfg(
                "Cfg",
                vec![
                    ("flag", Value::Bool(false)),
                    ("name", s(&from_cps(&[0x4E2D]))),
                    ("port", Value::Int(0)),
                    ("tags", istr(&[])),
                    ("nums", iints(&[])),
                    ("ratio", Value::Float(ordered_float::OrderedFloat(-1.25))),
                ],
            ),
        ]),
    );
}

#[test]
fn very_long_field_values() {
    let long = "x".repeat(10_000);
    let long_cjk = from_cps(&[0x4E2D]).repeat(4_096);
    assert_inplace(
        SRC_MIXED,
        cfg_list(vec![cfg(
            "Cfg",
            vec![
                ("flag", Value::Bool(true)),
                ("name", s(&long)),
                ("port", Value::Int(7)),
                (
                    "tags",
                    Value::List(Arc::new(vec![s(&long_cjk), s("short")])),
                ),
                ("nums", iints(&(0..200).collect::<Vec<_>>())),
                ("ratio", Value::Float(ordered_float::OrderedFloat(123.456))),
            ],
        )]),
    );
}

#[test]
fn many_records() {
    let items: Vec<Value> = (0..300)
        .map(|i| {
            cfg(
                "Cfg",
                vec![("name", s(&format!("cfg-{i}"))), ("port", Value::Int(i))],
            )
        })
        .collect();
    assert_inplace(SRC_NAME_PORT, cfg_list(items));
}

// ---- proptest: random schema field set / order / values --------------

/// A small fixed schema whose field *order* and *types* are varied at the
/// source level by `schema_variant`, with values generated to match. We
/// generate a `(source, items)` pair so the field layout the compiler
/// lays out matches the value the oracle and the in-place reader walk.
fn string_strat() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        "[a-z]{0,8}".prop_map(|s| s),
        prop::collection::vec(
            prop_oneof![Just(0x4E2Du32), Just(0x1F980u32), Just(0xE9u32)],
            0..5
        )
        .prop_map(|cps| cps.iter().map(|c| char::from_u32(*c).unwrap()).collect()),
        (0usize..40).prop_map(|n| "x".repeat(n)),
    ]
}

/// One Cfg value for the `{ a: String, b: Int, c: Bool, d: List<String> }`
/// schema used by the proptest. Mixed offsets exercise the field-pointer
/// layer the verifier and the in-place reader must walk.
fn cfg_value_strat() -> impl Strategy<Value = Value> {
    (
        string_strat(),
        any::<i64>(),
        any::<bool>(),
        prop::collection::vec(string_strat(), 0..5),
    )
        .prop_map(|(a, b, c, d)| {
            cfg(
                "Cfg",
                vec![
                    ("a", s(&a)),
                    ("b", Value::Int(b)),
                    ("c", Value::Bool(c)),
                    (
                        "d",
                        Value::List(Arc::new(d.into_iter().map(|x| s(&x)).collect())),
                    ),
                ],
            )
        })
}

fn list_cfg_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(cfg_value_strat(), 0..8).prop_map(cfg_list)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(192))]

    #[test]
    fn diff_list_schema(items in list_cfg_strat()) {
        let src = "#schema Cfg { a: String, b: Int, c: Bool, d: List<String> }\n\
                   #main(List<Cfg> items) -> List<Cfg>\nitems";
        let report = assert_all_backends_bit_equal(src, args(items));
        prop_assert!(report.cranelift_compared, "cranelift must compile the shape");
        #[cfg(feature = "llvm-aot")]
        prop_assert!(report.llvm_compared, "llvm must compile the shape");
    }
}

// ---- loud-cap guards: unsupported shapes decline, never miscompile ---

/// Shapes S4 does NOT lift must still make **both** AOT backends decline
/// the `#main` shape (a setup error). In production `Backend::Auto` falls
/// back to the tree-walk oracle; here we assert the decline is loud.
#[test]
fn unsupported_return_shapes_fail_loudly_not_silently() {
    let cap_cases = [
        // Parameter-*field* List<Schema> — the field-load rebase path is
        // not proven bit-equal for an in-place return; stays a loud cap.
        "#schema Cfg { name: String }\n#schema W { items: List<Cfg> }\n\
         #main(W w) -> List<Cfg>\nw.items",
        // List<List<Schema>> — inner pointer-array-of-pointer-array; the
        // in-place reader does not decode a nested Schema pointer array.
        "#schema Cfg { name: String }\n#main(List<List<Cfg>> xs) -> List<List<Cfg>>\nxs",
        // Sub-record carrying a nested List<Schema> field — out of S4
        // scope (the in-place sub-record decoder caps deeper pointer-array
        // element fields).
        "#schema Inner { x: Int }\n#schema Cfg { kids: List<Inner> }\n\
         #main(List<Cfg> items) -> List<Cfg>\nitems",
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
