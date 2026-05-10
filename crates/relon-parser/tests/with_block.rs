//! Fixture-driven integration tests for the schema-method `with { ... }`
//! syntax (Phase A of the trait-bound system).
//!
//! Each `.relon` file under `tests/fixtures/with_block/` must parse
//! cleanly and produce a `#schema` directive whose body's `methods`
//! and/or `schema_no_auto_derives` are populated. Each `.relon` file
//! under `tests/fixtures/with_block_invalid/` must fail to parse.

use relon_parser::{parse_document, DirectiveBody, Node};
use std::fs;
use std::path::{Path, PathBuf};

fn fixtures_dir(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(sub)
}

fn read_fixture(sub: &str, name: &str) -> String {
    let path = fixtures_dir(sub).join(format!("{name}.relon"));
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Find the first `#schema` directive on `root` and return a reference
/// to its `NameBody` body fields. Panics if no schema directive is
/// found — fixtures are expected to declare exactly one.
fn first_schema_namebody(root: &Node) -> &DirectiveBody {
    root.directives
        .iter()
        .find(|d| d.name == "schema")
        .map(|d| &d.body)
        .expect("expected a #schema directive in the fixture")
}

fn parse_or_panic(name: &str) -> Node {
    let source = read_fixture("with_block", name);
    parse_document(&source).unwrap_or_else(|e| panic!("fixture `{name}` failed to parse: {e:?}"))
}

#[test]
fn simple_method_parses() {
    let root = parse_or_panic("simple_method");
    let body = first_schema_namebody(&root);
    let DirectiveBody::NameBody { methods, .. } = body else {
        panic!("expected NameBody");
    };
    assert_eq!(methods.len(), 1);
    let m = &methods[0];
    assert_eq!(m.name, "cents_value");
    assert!(m.params.is_empty());
    assert_eq!(m.return_type.path, vec!["Int".to_string()]);
    assert!(m.body.is_some());
    assert!(m.derives.is_empty());
    assert!(!m.is_native);
}

#[test]
fn derive_equatable_parses() {
    let root = parse_or_panic("derive_equatable");
    let body = first_schema_namebody(&root);
    let DirectiveBody::NameBody { methods, .. } = body else {
        panic!("expected NameBody");
    };
    assert_eq!(methods.len(), 1);
    let m = &methods[0];
    assert_eq!(m.name, "eq");
    assert_eq!(m.derives, vec!["Equatable".to_string()]);
    assert_eq!(m.params.len(), 1);
    assert_eq!(m.params[0].name, "other");
    assert_eq!(m.params[0].type_node.path, vec!["Self".to_string()]);
}

#[test]
fn derive_comparable_parses_two_methods() {
    let root = parse_or_panic("derive_comparable");
    let body = first_schema_namebody(&root);
    let DirectiveBody::NameBody { methods, .. } = body else {
        panic!("expected NameBody");
    };
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[0].name, "eq");
    assert_eq!(methods[0].derives, vec!["Equatable".to_string()]);
    assert_eq!(methods[1].name, "lt");
    assert_eq!(methods[1].derives, vec!["Comparable".to_string()]);
}

#[test]
fn multiple_methods_parses() {
    let root = parse_or_panic("multiple_methods");
    let body = first_schema_namebody(&root);
    let DirectiveBody::NameBody { methods, .. } = body else {
        panic!("expected NameBody");
    };
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[0].name, "full_name");
    assert_eq!(methods[1].name, "is_admin");
    assert!(methods.iter().all(|m| m.derives.is_empty()));
}

#[test]
fn native_method_parses_with_no_body() {
    let root = parse_or_panic("native_method");
    let body = first_schema_namebody(&root);
    let DirectiveBody::NameBody { methods, .. } = body else {
        panic!("expected NameBody");
    };
    assert_eq!(methods.len(), 1);
    let m = &methods[0];
    assert_eq!(m.name, "render");
    assert!(m.is_native);
    assert!(m.body.is_none());
    assert_eq!(m.return_type.path, vec!["String".to_string()]);
}

#[test]
fn no_auto_derive_schema_level_parses() {
    let root = parse_or_panic("no_auto_derive_schema_level");
    let body = first_schema_namebody(&root);
    let DirectiveBody::NameBody {
        methods,
        schema_no_auto_derives,
        ..
    } = body
    else {
        panic!("expected NameBody");
    };
    assert!(methods.is_empty());
    assert_eq!(schema_no_auto_derives, &vec!["JsonProjectable".to_string()]);
}

#[test]
fn no_auto_derive_with_method_parses() {
    let root = parse_or_panic("no_auto_derive_with_method");
    let body = first_schema_namebody(&root);
    let DirectiveBody::NameBody {
        methods,
        schema_no_auto_derives,
        ..
    } = body
    else {
        panic!("expected NameBody");
    };
    assert_eq!(schema_no_auto_derives, &vec!["Equatable".to_string()]);
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].name, "eq");
    assert_eq!(methods[0].derives, vec!["Equatable".to_string()]);
}

#[test]
fn generic_schema_with_self_return_parses() {
    let root = parse_or_panic("generic_schema_methods");
    let body = first_schema_namebody(&root);
    let DirectiveBody::NameBody {
        generics, methods, ..
    } = body
    else {
        panic!("expected NameBody");
    };
    assert_eq!(generics, &vec!["T".to_string()]);
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].name, "same");
    assert_eq!(methods[0].return_type.path, vec!["Self".to_string()]);
}

#[test]
fn self_param_self_return_parses() {
    let root = parse_or_panic("self_param_self_return");
    let body = first_schema_namebody(&root);
    let DirectiveBody::NameBody { methods, .. } = body else {
        panic!("expected NameBody");
    };
    assert_eq!(methods.len(), 1);
    let m = &methods[0];
    assert_eq!(m.name, "plus");
    assert_eq!(m.return_type.path, vec!["Self".to_string()]);
    assert_eq!(m.params.len(), 1);
    assert_eq!(m.params[0].type_node.path, vec!["Self".to_string()]);
}

#[test]
fn multi_param_method_parses() {
    let root = parse_or_panic("multi_param_method");
    let body = first_schema_namebody(&root);
    let DirectiveBody::NameBody { methods, .. } = body else {
        panic!("expected NameBody");
    };
    assert_eq!(methods.len(), 1);
    let m = &methods[0];
    assert_eq!(m.params.len(), 3);
    assert_eq!(m.params[0].name, "prefix");
    assert_eq!(m.params[1].name, "suffix");
    assert_eq!(m.params[2].name, "count");
}

// -------------------------------------------------------------------
// Phase A.1 fixtures — body-less #schema, #extend, #private.

#[test]
fn bodyless_primitive_schema_parses() {
    let root = parse_or_panic("bodyless_primitive");
    let body = first_schema_namebody(&root);
    let DirectiveBody::NameBody {
        name,
        methods,
        body,
        ..
    } = body
    else {
        panic!("expected NameBody");
    };
    assert_eq!(name, "MyString");
    assert_eq!(methods.len(), 2);
    assert!(methods.iter().all(|m| m.is_native));
    // body 是合成的空 dict 占位
    let relon_parser::Expr::Dict(entries) = body.expr.as_ref() else {
        panic!("expected synthesized empty dict body, got {:?}", body.expr);
    };
    assert!(
        entries.is_empty(),
        "body-less synthesized dict must be empty"
    );
}

#[test]
fn extend_user_schema_parses() {
    let source = read_fixture("with_block", "extend_user_schema");
    let root = parse_document(&source).expect("parse");
    let extend_dir = root
        .directives
        .iter()
        .find(|d| d.name == "extend")
        .expect("expected #extend directive");
    let DirectiveBody::NameBody {
        name,
        methods,
        body,
        ..
    } = &extend_dir.body
    else {
        panic!("extend body must be NameBody");
    };
    assert_eq!(name, "User");
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].name, "is_admin");
    let relon_parser::Expr::Dict(entries) = body.expr.as_ref() else {
        panic!("extend body must be synthesized empty dict");
    };
    assert!(entries.is_empty());
}

#[test]
fn extend_with_derive_parses() {
    let source = read_fixture("with_block", "extend_with_derive");
    let root = parse_document(&source).expect("parse");
    let extend_dir = root
        .directives
        .iter()
        .find(|d| d.name == "extend")
        .expect("expected #extend directive");
    let DirectiveBody::NameBody { name, methods, .. } = &extend_dir.body else {
        panic!("extend body must be NameBody");
    };
    assert_eq!(name, "MyData");
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].name, "eq");
    assert_eq!(methods[0].derives, vec!["Equatable".to_string()]);
}

#[test]
fn private_method_parses() {
    let root = parse_or_panic("private_method");
    let body = first_schema_namebody(&root);
    let DirectiveBody::NameBody { methods, .. } = body else {
        panic!("expected NameBody");
    };
    assert_eq!(methods.len(), 2);
    let format = &methods[0];
    let helper = &methods[1];
    assert_eq!(format.name, "format");
    assert!(!format.is_private);
    assert_eq!(helper.name, "amount_string");
    assert!(helper.is_private);
}

#[test]
fn extend_builtin_parses() {
    let source = read_fixture("with_block", "extend_builtin");
    let root = parse_document(&source).expect("parse");
    let extend_dir = root
        .directives
        .iter()
        .find(|d| d.name == "extend")
        .expect("expected #extend directive");
    let DirectiveBody::NameBody { name, methods, .. } = &extend_dir.body else {
        panic!("extend body must be NameBody");
    };
    assert_eq!(name, "String");
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].name, "is_email");
}

// -------------------------------------------------------------------
// Negative cases — every file under `with_block_invalid/` must fail
// to parse. The exact diagnostic isn't asserted (Phase A), only the
// `parse_document` returning an error.

fn invalid_must_fail(name: &str) {
    let source = read_fixture("with_block_invalid", name);
    let result = parse_document(&source);
    assert!(
        result.is_err(),
        "fixture `{name}` was expected to fail but parsed: {result:?}"
    );
}

#[test]
fn invalid_with_before_body_rejected() {
    invalid_must_fail("with_before_body");
}

#[test]
fn invalid_native_with_body_rejected() {
    invalid_must_fail("native_with_body");
}

#[test]
fn invalid_unknown_pragma_rejected() {
    invalid_must_fail("unknown_pragma");
}

#[test]
fn invalid_bare_constraint_name_rejected() {
    invalid_must_fail("bare_constraint_name");
}
