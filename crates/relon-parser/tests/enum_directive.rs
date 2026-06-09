use relon_parser::{parse_document, DirectiveBody, Expr, INTERNAL_ENUM_TYPE_NAME};

#[test]
fn enum_directive_lowers_to_tagged_enum_schema_body() {
    let src = "#enum Notification { Email { address: String, subject: String }, Push }
{}";
    let node = parse_document(src).expect("parse #enum");
    let dir = node
        .directives
        .iter()
        .find(|dir| dir.name == "enum")
        .expect("enum directive");
    let DirectiveBody::NameBody { name, body, .. } = &dir.body else {
        panic!("expected NameBody, got {:?}", dir.body);
    };
    assert_eq!(name, "Notification");
    let Expr::Type(enum_ty) = body.expr.as_ref() else {
        panic!("expected enum type body, got {:?}", body.expr);
    };
    assert_eq!(enum_ty.path, vec![INTERNAL_ENUM_TYPE_NAME]);
    assert_eq!(enum_ty.generics.len(), 2);
    assert_eq!(enum_ty.generics[0].path, vec!["Email"]);
    assert_eq!(
        enum_ty.generics[0]
            .variant_fields
            .as_ref()
            .expect("Email fields")
            .iter()
            .map(|(name, ty)| (name.as_str(), ty.path.as_slice()))
            .collect::<Vec<_>>(),
        vec![
            ("address", &["String".to_string()][..]),
            ("subject", &["String".to_string()][..])
        ],
    );
    assert_eq!(enum_ty.generics[1].path, vec!["Push"]);
    assert!(enum_ty.generics[1]
        .variant_fields
        .as_ref()
        .expect("Push unit marker")
        .is_empty());
}

#[test]
fn enum_directive_rejects_string_literal_variant() {
    let err = parse_document(
        r#"#enum Stat { "up" }
{}"#,
    )
    .expect_err("string literal variants are not valid #enum variants");
    let message = err.to_string();
    assert!(
        message.contains("expected enum variant name"),
        "unexpected parse error: {message}"
    );
}

#[test]
fn enum_directive_lowers_generics() {
    let src = "#enum Box<T> { Some(T), None }\n{}";
    let node = parse_document(src).expect("parse generic #enum");
    let dir = node
        .directives
        .iter()
        .find(|dir| dir.name == "enum")
        .expect("enum directive");
    let DirectiveBody::NameBody { name, generics, .. } = &dir.body else {
        panic!("expected NameBody, got {:?}", dir.body);
    };
    assert_eq!(name, "Box");
    assert_eq!(generics, &vec!["T".to_string()]);
}

#[test]
fn enum_tuple_match_payload_pattern_lowers() {
    let node = parse_document(
        r#"#enum Packet { Pair(Int, String), Empty }
#main(Packet p) -> Int
p match { Pair(n, *): n + 1, Empty: 0 }
"#,
    )
    .expect("parse tuple payload pattern");
    let Expr::Match { arms, .. } = node.expr.as_ref() else {
        panic!("expected match expr, got {:?}", node.expr);
    };
    let Expr::VariantPattern {
        enum_path,
        variant,
        bindings,
    } = arms[0].0.expr.as_ref()
    else {
        panic!("expected variant pattern, got {:?}", arms[0].0.expr);
    };
    assert!(enum_path.is_empty());
    assert_eq!(variant, "Pair");
    assert_eq!(bindings.len(), 2);
    assert_eq!(bindings[0].field.as_deref(), None);
    assert_eq!(bindings[0].binding.as_deref(), Some("n"));
    assert_eq!(bindings[1].field.as_deref(), None);
    assert_eq!(bindings[1].binding.as_deref(), None);
}

#[test]
fn enum_struct_match_payload_pattern_lowers() {
    let node = parse_document(
        r#"#enum Msg { Email { address: String, subject: String }, Push }
#main(Msg m) -> String
m match { Msg.Email { address, subject: s }: address + s, Push: "" }
"#,
    )
    .expect("parse struct payload pattern");
    let Expr::Match { arms, .. } = node.expr.as_ref() else {
        panic!("expected match expr, got {:?}", node.expr);
    };
    let Expr::VariantPattern {
        enum_path,
        variant,
        bindings,
    } = arms[0].0.expr.as_ref()
    else {
        panic!("expected variant pattern, got {:?}", arms[0].0.expr);
    };
    assert_eq!(enum_path, &vec!["Msg".to_string()]);
    assert_eq!(variant, "Email");
    assert_eq!(bindings.len(), 2);
    assert_eq!(bindings[0].field.as_deref(), Some("address"));
    assert_eq!(bindings[0].binding.as_deref(), Some("address"));
    assert_eq!(bindings[1].field.as_deref(), Some("subject"));
    assert_eq!(bindings[1].binding.as_deref(), Some("s"));
}
