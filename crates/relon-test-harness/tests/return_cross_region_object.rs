//! F1b differential gate: cross-region object returns.
//!
//! `#main(List<Cfg> servers) -> Dict { servers: servers, n: Int }` builds
//! the object head in `out_buf`, but the `servers` field is a `#main`
//! parameter identity whose `List<Cfg>` data lives in `in_buf` — a genuine
//! cross-region field pointer. Under the F1 arena-absolute slot convention
//! the codegen stores the parameter list root's arena-absolute offset
//! directly into the object's field slot (no tail copy). The host runs the
//! multi-region object verifier (`verify_object_return_multi`) over the
//! whole arena anchored at `out_ptr`, which classifies the slot pointer
//! into the input region and bounds-checks the entire reachable graph; only
//! on a clean verify does `BufferReader::new_at_base` + `read_value_from_reader`
//! follow it cross-region.
//!
//! Scope (F1b): cranelift only. The IR lowering is shared, so the llvm +
//! wasm backends reach the same op; they cap loudly at codegen (the
//! `StoreFieldAtRecord { ListSchema | ListList }` gate) and land in F2. We
//! assert here that cranelift bit-equals the tree-walk oracle and that llvm
//! declines (loud cap, never a silent miscompile).
//!
//! Layers:
//!  1. Hand-written cases: `List<Schema>` field + scalar field; `List<List<Int>>`
//!     field; multiple cross-region fields; mixed scalar + cross-region;
//!     empty / single / many `servers`; CJK / empty / long String fields.
//!  2. proptest: random server lists through the same differential.
//!  3. Adversarial: the multi-region verifier must reject an object whose
//!     field pointer is forced out of every region (covered by the
//!     verifier crate's own multi-region adversarial tests; here we pin the
//!     end-to-end happy path + the llvm/wasm cap).

use std::collections::HashMap;
use std::sync::Arc;

use proptest::prelude::*;
use relon::{new_evaluator, Backend, BackendError};
use relon_eval_api::Value;
use relon_test_harness::assert_all_backends_bit_equal;

fn args1(name: &str, v: Value) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert(name.to_string(), v);
    m
}

fn from_cps(cps: &[u32]) -> String {
    cps.iter().map(|c| char::from_u32(*c).unwrap()).collect()
}

fn s(v: &str) -> Value {
    Value::String(v.into())
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

fn list(items: Vec<Value>) -> Value {
    Value::List(Arc::new(items))
}

/// Run the differential and assert cranelift compiled the cross-region
/// object shape (it must NOT silently fall back to a skip), while llvm is
/// allowed to decline (F1b is cranelift-only; llvm caps loudly until F2).
fn assert_cross_region(src: &str, args: HashMap<String, Value>) {
    let report = assert_all_backends_bit_equal(src, args);
    assert!(
        report.cranelift_compared,
        "cranelift must compile the cross-region object return; skipped: {:?}",
        report.cranelift_skip_reason
    );
    // F1b: llvm must NOT compile this shape yet — it caps loudly at codegen.
    // (When the feature is off the leg is skipped for an unrelated reason, so
    // only assert the cap when the feature is compiled in.)
    #[cfg(feature = "llvm-aot")]
    assert!(
        !report.llvm_compared,
        "F1b is cranelift-only: llvm must decline the cross-region object shape, but it compiled"
    );
}

// ---- hand-written cases ---------------------------------------------

/// The CLI canonical shape: `List<Server>` field + an Int scalar field.
const SRC_SERVERS_N: &str = "#schema Server { name: String, port: Int }\n\
     #main(List<Server> servers) -> Dict\n{ servers: servers, n: 1 }";

#[test]
fn servers_empty() {
    assert_cross_region(SRC_SERVERS_N, args1("servers", list(vec![])));
}

#[test]
fn servers_single() {
    assert_cross_region(
        SRC_SERVERS_N,
        args1(
            "servers",
            list(vec![cfg(
                "Server",
                vec![("name", s("alpha")), ("port", Value::Int(8080))],
            )]),
        ),
    );
}

#[test]
fn servers_many_with_cjk_empty_long() {
    let long = "x".repeat(5_000);
    let long_cjk = from_cps(&[0x4E2D]).repeat(2_048);
    assert_cross_region(
        SRC_SERVERS_N,
        args1(
            "servers",
            list(vec![
                cfg("Server", vec![("name", s("")), ("port", Value::Int(0))]),
                cfg(
                    "Server",
                    vec![
                        ("name", s(&from_cps(&[0x4E2D, 0x6587]))),
                        ("port", Value::Int(-1)),
                    ],
                ),
                cfg(
                    "Server",
                    vec![
                        ("name", s(&from_cps(&[0x1F980, 0x1F980]))),
                        ("port", Value::Int(i64::MAX)),
                    ],
                ),
                cfg("Server", vec![("name", s(&long)), ("port", Value::Int(7))]),
                cfg(
                    "Server",
                    vec![("name", s(&long_cjk)), ("port", Value::Int(-9))],
                ),
            ]),
        ),
    );
}

/// Server sub-record with mixed pointer + scalar fields (a list field
/// inside the element, a String hidden after a Bool).
const SRC_SERVERS_MIXED: &str =
    "#schema Server { flag: Bool, name: String, port: Int, tags: List<String> }\n\
     #main(List<Server> servers) -> Dict\n{ servers: servers, n: 42 }";

#[test]
fn servers_mixed_element_fields() {
    assert_cross_region(
        SRC_SERVERS_MIXED,
        args1(
            "servers",
            list(vec![
                cfg(
                    "Server",
                    vec![
                        ("flag", Value::Bool(true)),
                        ("name", s("host")),
                        ("port", Value::Int(1)),
                        ("tags", list(vec![s("a"), s(""), s("bb")])),
                    ],
                ),
                cfg(
                    "Server",
                    vec![
                        ("flag", Value::Bool(false)),
                        ("name", s(&from_cps(&[0x4E2D]))),
                        ("port", Value::Int(-2)),
                        ("tags", list(vec![])),
                    ],
                ),
            ]),
        ),
    );
}

/// `List<List<Int>>` cross-region field.
const SRC_GRID: &str = "#main(List<List<Int>> grid) -> Dict\n{ g: grid, n: 1 }";

#[test]
fn grid_list_list_int() {
    assert_cross_region(
        SRC_GRID,
        args1(
            "grid",
            list(vec![
                iints(&[1, 2, 3]),
                iints(&[]),
                iints(&[-7, i64::MIN, i64::MAX]),
            ]),
        ),
    );
}

/// Two cross-region fields in one object (both parameter-sourced lists).
const SRC_TWO_LISTS: &str = "#schema Server { name: String }\n\
     #main(List<Server> servers, List<List<Int>> grid) -> Dict\n\
     { servers: servers, grid: grid, n: 3 }";

#[test]
fn two_cross_region_fields() {
    let mut m = HashMap::new();
    m.insert(
        "servers".to_string(),
        list(vec![
            cfg("Server", vec![("name", s("a"))]),
            cfg("Server", vec![("name", s(&from_cps(&[0x6587])))]),
        ]),
    );
    m.insert("grid".to_string(), list(vec![iints(&[1, 2]), iints(&[3])]));
    assert_cross_region(SRC_TWO_LISTS, m);
}

/// Mixed: a scalar field, a String scalar field, and a cross-region list
/// field interleaved.
const SRC_MIXED_SCALAR_CROSS: &str = "#schema Server { name: String, port: Int }\n\
     #main(List<Server> servers) -> Dict\n\
     { title: \"cfg\", servers: servers, count: 2, ratio: 1.5 }";

#[test]
fn mixed_scalar_and_cross_region() {
    assert_cross_region(
        SRC_MIXED_SCALAR_CROSS,
        args1(
            "servers",
            list(vec![
                cfg("Server", vec![("name", s("x")), ("port", Value::Int(1))]),
                cfg(
                    "Server",
                    vec![
                        ("name", s(&from_cps(&[0x4E2D, 0x6587]))),
                        ("port", Value::Int(2)),
                    ],
                ),
            ]),
        ),
    );
}

// ---- proptest -------------------------------------------------------

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

fn server_strat() -> impl Strategy<Value = Value> {
    (string_strat(), any::<i64>()).prop_map(|(name, port)| {
        cfg(
            "Server",
            vec![("name", s(&name)), ("port", Value::Int(port))],
        )
    })
}

fn servers_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(server_strat(), 0..8).prop_map(list)
}

fn grid_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(
        prop::collection::vec(any::<i64>(), 0..6)
            .prop_map(|r| Value::List(Arc::new(r.into_iter().map(Value::Int).collect()))),
        0..6,
    )
    .prop_map(list)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(160))]

    #[test]
    fn diff_servers_object(servers in servers_strat()) {
        let src = "#schema Server { name: String, port: Int }\n\
                   #main(List<Server> servers) -> Dict\n{ servers: servers, n: 1 }";
        let report = assert_all_backends_bit_equal(src, args1("servers", servers));
        prop_assert!(report.cranelift_compared, "cranelift must compile the cross-region shape");
        #[cfg(feature = "llvm-aot")]
        prop_assert!(!report.llvm_compared, "F1b: llvm must decline the cross-region shape");
    }

    #[test]
    fn diff_grid_object(grid in grid_strat()) {
        let src = "#main(List<List<Int>> grid) -> Dict\n{ g: grid, n: 1 }";
        let report = assert_all_backends_bit_equal(src, args1("grid", grid));
        prop_assert!(report.cranelift_compared, "cranelift must compile the cross-region grid shape");
        #[cfg(feature = "llvm-aot")]
        prop_assert!(!report.llvm_compared, "F1b: llvm must decline the cross-region grid shape");
    }
}

// ---- loud-cap guards ------------------------------------------------

/// F1b ships cranelift only; llvm (and wasm, which shares the llvm codegen
/// path) must decline the cross-region object shape loudly — never a silent
/// miscompile. Also pins that shapes still beyond F1b stay capped on both
/// backends.
#[test]
fn llvm_declines_cross_region_object() {
    let cross_region = [
        "#schema Server { name: String, port: Int }\n\
         #main(List<Server> servers) -> Dict\n{ servers: servers, n: 1 }",
        "#main(List<List<Int>> grid) -> Dict\n{ g: grid, n: 1 }",
    ];
    for src in cross_region {
        // cranelift accepts (F1b).
        assert!(
            new_evaluator(src, Backend::CraneliftAot).is_ok(),
            "cranelift must compile the cross-region object shape: `{src}`"
        );
        // llvm declines loudly (F2 territory).
        #[cfg(feature = "llvm-aot")]
        match new_evaluator(src, Backend::LlvmAot) {
            Err(BackendError::LlvmAot(_)) => { /* loud decline — correct */ }
            Err(other) => panic!("expected an LlvmAot decline for `{src}`, got {other}"),
            Ok(_) => panic!(
                "llvm unexpectedly accepted the cross-region object shape `{src}` — F1b is \
                 cranelift-only; a silent llvm cross-region path may have opened"
            ),
        }
    }
}

/// Shapes still beyond F1b must cap on cranelift too (loud decline).
#[test]
fn beyond_f1b_still_capped_on_cranelift() {
    let cap_cases = [
        // Branded-struct field surface still routes through the branded
        // dict-into-record path (not the anon-Dict cross-region path),
        // which has no cross-region field store yet — stays capped.
        "#schema Server { name: String }\n#schema Cfg { servers: List<Server> }\n\
         #main(List<Server> servers) -> Cfg\n{ servers: servers }",
        // Parameter-*field* List<Schema> inside an object — the field-load
        // rebase path is not an identity walk; stays capped.
        "#schema Server { name: String }\n#schema W { items: List<Server> }\n\
         #main(W w) -> Dict\n{ servers: w.items, n: 1 }",
        // List<List<Schema>> field — inner pointer-array-of-pointer-array,
        // out of F1b scope.
        "#schema Server { name: String }\n\
         #main(List<List<Server>> xs) -> Dict\n{ xs: xs, n: 1 }",
    ];
    for src in cap_cases {
        match new_evaluator(src, Backend::CraneliftAot) {
            Err(BackendError::CraneliftAot(_)) => { /* loud decline — correct */ }
            Err(other) => panic!("expected a CraneliftAot decline for `{src}`, got {other}"),
            Ok(_) => panic!(
                "cranelift unexpectedly accepted a beyond-F1b shape: `{src}` — a silent \
                 miscompile path may have opened"
            ),
        }
    }
}
