use super::*;
use relon_parser::parse_document;

fn analyze(src: &str) -> AnalyzedTree {
    let node = parse_document(src).expect("parse");
    crate::analyze(&node)
}

#[test]
fn binds_sibling_at_root_level() {
    // `&sibling.a` should resolve to the value-node id of field `a`.
    let tree = analyze(r#"{ a: 1, b: &sibling.a }"#);
    assert_eq!(tree.references.len(), 1);
    let resolved = tree.references.values().next().unwrap();
    // The recorded target must round-trip back to a node we
    // tracked in the index.
    assert!(tree.node(resolved.target).is_some());
    assert!(matches!(resolved.via, RefBase::Sibling));
}

#[test]
fn binds_root_reference_from_nested_dict() {
    let tree = analyze(
        r#"{
                a: 10,
                inner: { ptr: &root.a }
            }"#,
    );
    // `ptr` resolves to the top-level `a`.
    let resolved = tree
        .references
        .values()
        .find(|r| matches!(r.via, RefBase::Root))
        .expect("root ref");
    let target_node = tree.node(resolved.target).expect("indexed");
    assert!(matches!(&*target_node.expr, Expr::Int(10)));
}

#[test]
fn does_not_bind_list_context_refs() {
    // `&prev` / `&index` / `&next` need iteration state — skip them.
    let tree = analyze(
        r#"[
                { v: 1, p: &prev },
                { v: 2, p: &prev.v }
            ]"#,
    );
    assert!(tree.references.is_empty(), "{:?}", tree.references);
}

#[test]
fn variables_resolve_like_siblings() {
    // Bare identifiers that name a sibling should bind too.
    let tree = analyze(r#"{ helper(x): x + 1, twice: helper }"#);
    // The `helper` reference inside `twice: helper` is a Variable
    // expression. Confirm it's bound.
    assert!(tree.references.values().any(|r| {
        let node = tree.node(r.target).unwrap();
        matches!(&*node.expr, Expr::Closure { .. })
    }));
}

#[test]
fn closure_params_shadow_outer_siblings() {
    // `x` inside the closure body should bind to the closure
    // param, not to the outer `x: 100` field.
    let tree = analyze(
        r#"{
                x: 100,
                fn(x): x + 1
            }"#,
    );
    // Find the `Variable(x)` reference inside the closure body
    // (the `x + 1` expression).
    let bound = tree
        .references
        .values()
        .find(|r| {
            let target = tree.node(r.target).unwrap();
            // Closure-param sentinel is the body's NodeId, which
            // is the `Binary(Add, x, 1)` expression.
            matches!(&*target.expr, Expr::Binary(_, _, _))
        })
        .expect("closure param resolved");
    assert!(matches!(bound.via, RefBase::This));
}

#[test]
fn dict_with_spread_marks_frame_dynamic() {
    // The spread expands `base`'s keys at runtime. The frame
    // containing the spread should report `has_dynamic_spread`
    // so a downstream typecheck pass won't false-positive on
    // names that may come from `base`. Inline check: ask the
    // builder directly.
    use relon_parser::{parse_document, Expr, TokenKey};
    let node = parse_document(
        r#"{
                base: { x: 1, y: 2 },
                merged: { ...&sibling.base, z: 3 }
            }"#,
    )
    .unwrap();
    // Drill down to the inner dict (the value of "merged") and
    // build a frame for it.
    let Expr::Dict(root_pairs) = &*node.expr else {
        panic!()
    };
    let merged_value = &root_pairs
        .iter()
        .find(|(k, _)| matches!(k, TokenKey::String(s, _, _) if s == "merged"))
        .unwrap()
        .1;
    let Expr::Dict(merged_pairs) = &*merged_value.expr else {
        panic!()
    };
    let frame = build_frame(merged_pairs);
    assert!(frame.has_dynamic_spread);
    assert!(!frame.fields.contains_key("x"));
    assert!(frame.fields.contains_key("z"));
}
