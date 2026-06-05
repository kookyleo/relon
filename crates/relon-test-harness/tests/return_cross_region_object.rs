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
//! Scope (F2): cranelift AND llvm (and wasm, which shares the llvm codegen
//! path — its four-way leg lives in
//! `relon-codegen-llvm/tests/cross_region_object_four_way.rs`). The IR
//! lowering is shared and the `StoreFieldAtRecord { ListSchema | ListList }`
//! store writes the parameter list root's arena-absolute offset verbatim
//! into the object slot on every backend. We assert here that both native
//! compiled backends (cranelift + llvm) bit-equal the tree-walk oracle.
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
/// object shape (it must NOT silently fall back to a skip). F2: llvm must
/// also compile + bit-equal it (the differential panics on any divergence).
fn assert_cross_region(src: &str, args: HashMap<String, Value>) {
    let report = assert_all_backends_bit_equal(src, args);
    assert!(
        report.cranelift_compared,
        "cranelift must compile the cross-region object return; skipped: {:?}",
        report.cranelift_skip_reason
    );
    // F2: llvm now ships this shape too — it must participate (and the
    // differential already asserted bit-equality). Only enforce when the
    // feature is compiled in.
    #[cfg(feature = "llvm-aot")]
    assert!(
        report.llvm_compared,
        "F2: llvm must compile the cross-region object shape; skipped: {:?}",
        report.llvm_skip_reason
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

// ---- F3: scalar / String list object fields (anon-Dict path) --------

/// `List<String>` parameter-identity object field (cross-region, not the
/// const-pool literal copy path).
const SRC_TAGS: &str = "#main(List<String> tags) -> Dict\n{ tags: tags, n: 1 }";

#[test]
fn anon_dict_list_string_field() {
    assert_cross_region(
        SRC_TAGS,
        args1(
            "tags",
            list(vec![
                s("a"),
                s(""),
                s(&from_cps(&[0x4E2D, 0x6587])),
                s(&"x".repeat(3000)),
            ]),
        ),
    );
}

#[test]
fn anon_dict_list_string_field_empty() {
    assert_cross_region(SRC_TAGS, args1("tags", list(vec![])));
}

/// `List<Int>` parameter-identity object field (inline-fixed scalar list,
/// still routed cross-region for a uniform arena-absolute slot).
const SRC_XS: &str = "#main(List<Int> xs) -> Dict\n{ xs: xs, n: 1 }";

#[test]
fn anon_dict_list_int_field() {
    assert_cross_region(SRC_XS, args1("xs", iints(&[0, -1, i64::MIN, i64::MAX, 42])));
}

/// Two cross-region scalar/String lists + a scalar field in one object.
const SRC_MULTI_LIST: &str = "#main(List<String> tags, List<Int> xs) -> Dict\n\
     { tags: tags, xs: xs, n: 7 }";

#[test]
fn anon_dict_multi_cross_region_lists() {
    let mut m = HashMap::new();
    m.insert(
        "tags".to_string(),
        list(vec![s(&from_cps(&[0x1F980])), s("bb"), s("")]),
    );
    m.insert("xs".to_string(), iints(&[10, -20, 30]));
    assert_cross_region(SRC_MULTI_LIST, m);
}

// ---- F3: branded-struct cross-region fields (branded path) ----------

const SRC_WRAP_SERVERS: &str = "#schema Server { name: String, port: Int }\n\
     #schema Wrapper { servers: List<Server>, n: Int }\n\
     #main(List<Server> servers) -> Wrapper { servers: servers, n: 7 }";

#[test]
fn branded_struct_servers_field() {
    assert_cross_region(
        SRC_WRAP_SERVERS,
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
                        ("name", s(&"y".repeat(4096))),
                        ("port", Value::Int(i64::MAX)),
                    ],
                ),
            ]),
        ),
    );
}

#[test]
fn branded_struct_servers_field_empty() {
    assert_cross_region(SRC_WRAP_SERVERS, args1("servers", list(vec![])));
}

const SRC_WRAP_TAGS: &str = "#schema Wrapper { tags: List<String>, n: Int }\n\
     #main(List<String> tags) -> Wrapper { tags: tags, n: 1 }";

#[test]
fn branded_struct_tags_field() {
    assert_cross_region(
        SRC_WRAP_TAGS,
        args1(
            "tags",
            list(vec![
                s(""),
                s("a"),
                s(&from_cps(&[0x6587])),
                s(&"z".repeat(2000)),
            ]),
        ),
    );
}

const SRC_WRAP_XS: &str = "#schema Wrapper { xs: List<Int>, n: Int }\n\
     #main(List<Int> xs) -> Wrapper { xs: xs, n: 1 }";

#[test]
fn branded_struct_xs_field() {
    assert_cross_region(
        SRC_WRAP_XS,
        args1("xs", iints(&[0, -7, i64::MIN, i64::MAX])),
    );
}

const SRC_WRAP_GRID: &str = "#schema Wrapper { g: List<List<Int>>, n: Int }\n\
     #main(List<List<Int>> grid) -> Wrapper { g: grid, n: 1 }";

#[test]
fn branded_struct_grid_field() {
    assert_cross_region(
        SRC_WRAP_GRID,
        args1(
            "grid",
            list(vec![
                iints(&[1, 2, 3]),
                iints(&[]),
                iints(&[i64::MIN, i64::MAX]),
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
        prop_assert!(report.llvm_compared, "F2: llvm must compile the cross-region shape");
    }

    #[test]
    fn diff_grid_object(grid in grid_strat()) {
        let src = "#main(List<List<Int>> grid) -> Dict\n{ g: grid, n: 1 }";
        let report = assert_all_backends_bit_equal(src, args1("grid", grid));
        prop_assert!(report.cranelift_compared, "cranelift must compile the cross-region grid shape");
        #[cfg(feature = "llvm-aot")]
        prop_assert!(report.llvm_compared, "F2: llvm must compile the cross-region grid shape");
    }

    // F3: cross-region String-list object field (anon-Dict path).
    #[test]
    fn diff_tags_object(
        tags in prop::collection::vec(string_strat(), 0..8)
            .prop_map(|v| list(v.into_iter().map(|x| s(&x)).collect()))
    ) {
        let src = "#main(List<String> tags) -> Dict\n{ tags: tags, n: 1 }";
        let report = assert_all_backends_bit_equal(src, args1("tags", tags));
        prop_assert!(report.cranelift_compared, "cranelift must compile the cross-region tags shape");
        #[cfg(feature = "llvm-aot")]
        prop_assert!(report.llvm_compared, "F3: llvm must compile the cross-region tags shape");
    }

    // F3: cross-region Int-list object field (anon-Dict path).
    #[test]
    fn diff_xs_int_object(
        xs in prop::collection::vec(any::<i64>(), 0..10)
            .prop_map(|v| list(v.into_iter().map(Value::Int).collect()))
    ) {
        let src = "#main(List<Int> xs) -> Dict\n{ xs: xs, n: 1 }";
        let report = assert_all_backends_bit_equal(src, args1("xs", xs));
        prop_assert!(report.cranelift_compared, "cranelift must compile the cross-region xs shape");
        #[cfg(feature = "llvm-aot")]
        prop_assert!(report.llvm_compared, "F3: llvm must compile the cross-region xs shape");
    }

    // F3: cross-region branded-struct List<Schema> field (branded path).
    #[test]
    fn diff_branded_servers_object(servers in servers_strat()) {
        let src = "#schema Server { name: String, port: Int }\n\
                   #schema Wrapper { servers: List<Server>, n: Int }\n\
                   #main(List<Server> servers) -> Wrapper { servers: servers, n: 7 }";
        let report = assert_all_backends_bit_equal(src, args1("servers", servers));
        prop_assert!(report.cranelift_compared, "cranelift must compile the F3 branded-struct shape");
        #[cfg(feature = "llvm-aot")]
        prop_assert!(report.llvm_compared, "F3: llvm must compile the branded-struct shape");
    }
}

// ---- loud-cap guards ------------------------------------------------

/// F2 ships the cross-region object shape on cranelift AND llvm (wasm
/// shares the llvm codegen path; its four-way leg lives in
/// `relon-codegen-llvm/tests/cross_region_object_four_way.rs`). Both native
/// backends must accept it — a decline here would mean a regression back to
/// the F1b cranelift-only state.
#[test]
fn both_backends_compile_cross_region_object() {
    let cross_region = [
        "#schema Server { name: String, port: Int }\n\
         #main(List<Server> servers) -> Dict\n{ servers: servers, n: 1 }",
        "#main(List<List<Int>> grid) -> Dict\n{ g: grid, n: 1 }",
    ];
    for src in cross_region {
        assert!(
            new_evaluator(src, Backend::CraneliftAot).is_ok(),
            "cranelift must compile the cross-region object shape: `{src}`"
        );
        #[cfg(feature = "llvm-aot")]
        assert!(
            new_evaluator(src, Backend::LlvmAot).is_ok(),
            "F2: llvm must compile the cross-region object shape: `{src}`"
        );
    }
}

/// Shapes still beyond F3 must cap on both native backends (loud decline).
/// F3 released branded-struct fields + scalar/String list object fields; what
/// remains capped is the parameter-*field* list source (`w.items` / `o.tags`,
/// F4) and the doubly-nested pointer-array (`List<List<Schema>>`, F5).
#[test]
fn beyond_f3_still_capped() {
    let cap_cases = [
        // Parameter-*field* List<Schema> inside an anon-Dict — the field-load
        // rebase path is not an identity walk; stays capped (F4).
        "#schema Server { name: String }\n#schema W { items: List<Server> }\n\
         #main(W w) -> Dict\n{ servers: w.items, n: 1 }",
        // Parameter-*field* List<Schema> inside a branded struct — same
        // field-load source, stays capped (F4).
        "#schema Server { name: String }\n#schema W { items: List<Server> }\n\
         #schema Cfg { servers: List<Server> }\n\
         #main(W w) -> Cfg\n{ servers: w.items }",
        // List<List<Schema>> field — inner pointer-array-of-pointer-array,
        // out of scope (F5).
        "#schema Server { name: String }\n\
         #main(List<List<Server>> xs) -> Dict\n{ xs: xs, n: 1 }",
    ];
    for src in cap_cases {
        match new_evaluator(src, Backend::CraneliftAot) {
            Err(BackendError::CraneliftAot(_)) => { /* loud decline — correct */ }
            Err(other) => panic!("expected a CraneliftAot decline for `{src}`, got {other}"),
            Ok(_) => panic!(
                "cranelift unexpectedly accepted a beyond-F3 shape: `{src}` — a silent \
                 miscompile path may have opened"
            ),
        }
        #[cfg(feature = "llvm-aot")]
        match new_evaluator(src, Backend::LlvmAot) {
            Err(BackendError::LlvmAot(_)) => { /* loud decline — correct */ }
            Err(other) => panic!("expected an LlvmAot decline for `{src}`, got {other}"),
            Ok(_) => panic!("llvm unexpectedly accepted a beyond-F3 shape: `{src}`"),
        }
    }
}

/// F3: the branded-struct cross-region field surface now compiles on both
/// native backends (was a cap through F2). It rides the branded
/// dict-into-record lowering path, distinct from the anon-Dict path. The
/// four-way bit-equal proof lives in
/// `relon-codegen-llvm/tests/cross_region_object_four_way.rs`.
#[test]
fn branded_struct_cross_region_field_compiles() {
    let shapes = [
        "#schema Server { name: String, port: Int }\n\
         #schema Wrapper { servers: List<Server>, n: Int }\n\
         #main(List<Server> servers) -> Wrapper { servers: servers, n: 7 }",
        "#schema Wrapper { g: List<List<Int>>, n: Int }\n\
         #main(List<List<Int>> grid) -> Wrapper { g: grid, n: 1 }",
        "#schema Wrapper { tags: List<String>, n: Int }\n\
         #main(List<String> tags) -> Wrapper { tags: tags, n: 1 }",
        "#schema Wrapper { xs: List<Int>, n: Int }\n\
         #main(List<Int> xs) -> Wrapper { xs: xs, n: 1 }",
    ];
    for src in shapes {
        assert!(
            new_evaluator(src, Backend::CraneliftAot).is_ok(),
            "cranelift must compile the F3 branded-struct cross-region shape: `{src}`"
        );
        #[cfg(feature = "llvm-aot")]
        assert!(
            new_evaluator(src, Backend::LlvmAot).is_ok(),
            "llvm must compile the F3 branded-struct cross-region shape: `{src}`"
        );
    }
}
