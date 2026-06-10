//! Inference engine tests. Exercise [`super::infer_type`] +
//! [`super::walk::walk_path`] + the `InferredType` / `TypeScope` /
//! `subsumes` / `join` machinery through the integrated public
//! surface so the same suite covers mod.rs and the walk sub-module
//! together.

use super::*;
use crate::analyze;
use relon_parser::parse_document;

fn analyze_str(src: &str) -> AnalyzedTree {
    let node = parse_document(src).unwrap();
    analyze(&node)
}

#[test]
fn join_int_float_is_number() {
    assert_eq!(
        InferredType::join(&InferredType::Int, &InferredType::Float),
        InferredType::Number
    );
}

#[test]
fn join_unrelated_is_any() {
    assert_eq!(
        InferredType::join(&InferredType::Int, &InferredType::String),
        InferredType::Any
    );
}

#[test]
fn option_none_subsumes_optional_slot_only() {
    let mut int_slot = relon_parser::TypeNode {
        path: vec!["Int".to_string()],
        generics: vec![],
        is_optional: false,
        range: relon_parser::TokenRange::default(),
        variant_fields: None,
        doc_comment: None,
    };
    let none = InferredType::Variant("Option".to_string(), "None".to_string());

    assert!(!none.subsumes(&int_slot));
    int_slot.is_optional = true;
    assert!(none.subsumes(&int_slot));
}

#[test]
fn binary_string_plus_int_is_concat_not_invalid() {
    // `Int + String` / `String + Int` is a coercion concat, not a
    // static mismatch: the runtime renders the non-String operand via
    // `Display` and `format!`-concats it (`arithmetic.rs`). Both
    // operand orders are valid for `Add`.
    assert!(!binary_known_invalid(
        Operator::Add,
        &InferredType::Int,
        &InferredType::String
    ));
    assert!(!binary_known_invalid(
        Operator::Add,
        &InferredType::String,
        &InferredType::Int
    ));
}

#[test]
fn binary_any_is_never_invalid() {
    assert!(!binary_known_invalid(
        Operator::Add,
        &InferredType::Any,
        &InferredType::String
    ));
}

#[test]
fn infer_string_literal() {
    let tree = analyze_str(r#"{ x: "hello" }"#);
    let scope = TypeScope::default();
    // Drill into x's value via the analyzer tree.
    let entry = tree.node_index.values().find_map(|n| match &*n.expr {
        Expr::String(s) if s == "hello" => Some(n.clone()),
        _ => None,
    });
    let n = entry.expect("string node indexed");
    assert_eq!(infer_type(&n, &scope), Some(InferredType::String));
}

// ============= v1.4 path-tail walker =============

/// `path_segments` returns every leading String segment.
#[test]
fn v1_4_path_segments_strings_only() {
    let path = vec![
        TokenKey::String("o".to_string(), Default::default(), false),
        TokenKey::String("id".to_string(), Default::default(), false),
    ];
    assert_eq!(path_segments(&path), vec!["o", "id"]);
}

/// `path_segments` stops at the first non-String segment.
#[test]
fn v1_4_path_segments_stops_at_dynamic() {
    use relon_parser::Node;
    let path = vec![
        TokenKey::String("o".to_string(), Default::default(), false),
        TokenKey::Dynamic(
            Node::new(Expr::Int(0), relon_parser::TokenRange::default()),
            false,
        ),
    ];
    assert_eq!(path_segments(&path), vec!["o"]);
}

/// `walk_path` returns `UnknownHead` for an unbound name.
#[test]
fn v1_4_walk_path_unknown_head() {
    let tree = analyze_str(r#"{ x: 1 }"#);
    let schemas = SchemaIndex::new();
    let scope = TypeScope::new(&tree, &schemas);
    let path = vec![TokenKey::String(
        "missing".to_string(),
        Default::default(),
        false,
    )];
    assert_eq!(walk_path(&path, &scope), PathTailOutcome::UnknownHead);
}

/// `walk_path` resolves a single-segment binding to its declared
/// type via the scope's frames.
#[test]
fn v1_4_walk_path_single_seg_via_frame() {
    // Using a `#main(Int n)` so the resolver builds a synthetic
    // root frame populating `n` as `Int`.
    let tree = analyze_str(
        r#"
        #main(Int n) -> Int
        n
        "#,
    );
    let schemas = SchemaIndex::new();
    let mut scope = TypeScope::new(&tree, &schemas);
    scope.locals.insert("n".to_string(), InferredType::Int);
    let path = vec![TokenKey::String("n".to_string(), Default::default(), false)];
    assert_eq!(
        walk_path(&path, &scope),
        PathTailOutcome::Resolved(InferredType::Int)
    );
}

/// `walk_path` reports `UnknownStep` when a Schema head is missing
/// the requested field.
#[test]
fn v1_4_walk_path_schema_missing_field() {
    let tree = analyze_str(
        r#"
        #schema Order { Int id: * }
        #main(Order o) -> Int
        o.id
        "#,
    );
    let schemas = crate::typecheck::build_schema_index(&tree);
    let mut scope = TypeScope::new(&tree, &schemas);
    scope
        .locals
        .insert("o".to_string(), InferredType::Schema("Order".to_string()));
    let path = vec![
        TokenKey::String("o".to_string(), Default::default(), false),
        TokenKey::String("nope".to_string(), Default::default(), false),
    ];
    match walk_path(&path, &scope) {
        PathTailOutcome::UnknownStep { at_segment, .. } => assert_eq!(at_segment, 1),
        other => panic!("expected UnknownStep, got {other:?}"),
    }
}

/// `walk_path` flows through a `Dict<String, T>` head, returning
/// the value type for any key step.
#[test]
fn v1_4_walk_path_dict_value() {
    let tree = analyze_str(r#"{ x: 1 }"#);
    let schemas = SchemaIndex::new();
    let mut scope = TypeScope::new(&tree, &schemas);
    scope.locals.insert(
        "kv".to_string(),
        InferredType::Dict(Box::new(InferredType::Int)),
    );
    let path = vec![
        TokenKey::String("kv".to_string(), Default::default(), false),
        TokenKey::String("foo".to_string(), Default::default(), false),
    ];
    assert_eq!(
        walk_path(&path, &scope),
        PathTailOutcome::Resolved(InferredType::Int)
    );
}

/// `walk_path` strips an Optional wrapper before stepping into the
/// inner schema.
#[test]
fn v1_4_walk_path_optional_strip() {
    let tree = analyze_str(
        r#"
        #schema Customer { String name: * }
        { x: 1 }
        "#,
    );
    let schemas = crate::typecheck::build_schema_index(&tree);
    let mut scope = TypeScope::new(&tree, &schemas);
    scope.locals.insert(
        "c".to_string(),
        InferredType::Optional(Box::new(InferredType::Schema("Customer".to_string()))),
    );
    let path = vec![
        TokenKey::String("c".to_string(), Default::default(), false),
        TokenKey::String("name".to_string(), Default::default(), false),
    ];
    assert_eq!(
        walk_path(&path, &scope),
        PathTailOutcome::Resolved(InferredType::String)
    );
}

/// `walk_path` returns `UnknownStep` when descending into a leaf
/// type (Int has no nested fields).
#[test]
fn v1_4_walk_path_descend_into_leaf() {
    let tree = analyze_str(r#"{ x: 1 }"#);
    let schemas = SchemaIndex::new();
    let mut scope = TypeScope::new(&tree, &schemas);
    scope.locals.insert("n".to_string(), InferredType::Int);
    let path = vec![
        TokenKey::String("n".to_string(), Default::default(), false),
        TokenKey::String("something".to_string(), Default::default(), false),
    ];
    match walk_path(&path, &scope) {
        PathTailOutcome::UnknownStep { running_name, .. } => assert_eq!(running_name, "Int"),
        other => panic!("expected UnknownStep, got {other:?}"),
    }
}

/// `walk_path` propagates `Any` once encountered — strict-mode
/// callers see `Resolved(Any)` and decide whether to flag.
#[test]
fn v1_4_walk_path_any_short_circuits() {
    let tree = analyze_str(r#"{ x: 1 }"#);
    let schemas = SchemaIndex::new();
    let mut scope = TypeScope::new(&tree, &schemas);
    scope.locals.insert("x".to_string(), InferredType::Any);
    let path = vec![
        TokenKey::String("x".to_string(), Default::default(), false),
        TokenKey::String("y".to_string(), Default::default(), false),
    ];
    assert_eq!(
        walk_path(&path, &scope),
        PathTailOutcome::Resolved(InferredType::Any)
    );
}

// ============= v1.5 inference upgrades =============

/// v1.5: `Expr::Spread(inner)` infers as the inner's type.
#[test]
fn v1_5_spread_inferes_inner_type() {
    let tree = analyze_str(r#"{ x: 1 }"#);
    let schemas = SchemaIndex::new();
    let scope = TypeScope::new(&tree, &schemas);
    let inner = relon_parser::Node::new(Expr::Int(7), relon_parser::TokenRange::default());
    let spread = relon_parser::Node::new(Expr::Spread(inner), relon_parser::TokenRange::default());
    assert_eq!(infer_type(&spread, &scope), Some(InferredType::Int));
}

/// v1.5: `Expr::Comprehension` infers `List<elem>`. Element body
/// `id` (binding name) refers to the iterable's element type.
#[test]
fn v1_5_comprehension_list_int() {
    let tree = analyze_str(
        r#"
        #main(Int n) -> List<Int>
        [x for x in range(n)]
        "#,
    );
    // The pre-flight check should not flag a return mismatch —
    // i.e. the body's type infers cleanly as `List<Int>`.
    let mm = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, crate::Diagnostic::MainReturnTypeMismatch { .. }))
        .count();
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

/// v1.5: `Expr::Where` infers from the body in a scope extended
/// with the bindings — `(n + 1) where { n: x }` infers as the
/// body type.
#[test]
fn v1_5_where_uses_binding_scope() {
    let tree = analyze_str(
        r#"
        #main(Int x) -> Int
        (n + 1) where { n: x }
        "#,
    );
    let mm = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, crate::Diagnostic::MainReturnTypeMismatch { .. }))
        .count();
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

/// v1.5: FnCall multi-segment alias.method routes through
/// `lookup_signature_path`. The single-segment fast-path stays
/// behaviorally identical.
#[test]
fn v1_5_fncall_single_seg_unchanged() {
    // `range` is a stdlib name — single-segment path goes through
    // `lookup_signature` exactly as in v1.4.
    let tree = analyze_str(
        r#"
        #main(Int n) -> List<Int>
        range(n)
        "#,
    );
    let mm = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, crate::Diagnostic::MainReturnTypeMismatch { .. }))
        .count();
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}
