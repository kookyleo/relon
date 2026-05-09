use crate::{
    create_range, expr::parse_expr, parse_attributes, parse_leading_comments,
    prim::string::parse_string, soc0, ws0, Decorator, Directive, Expr, Node, Span, TokenKey,
};
use winnow::combinator::{alt, delimited, opt, separated};
use winnow::prelude::*;
use winnow::stream::{Location, Stream};

/// One entry inside a dict literal. Either a regular key/value pair or a
/// stack of standalone `#directive` lines (e.g. `#schema A : Body` used
/// to introduce schemas without producing a dict field).
///
/// `Pair` is significantly larger than `Directives`, but the enum is
/// produced and consumed in the parser pass and never persisted, so the
/// size disparity is acceptable.
#[allow(clippy::large_enum_variant)]
pub enum DictEntry {
    Pair(TokenKey, Node),
    Directives(Vec<Directive>),
}

pub fn parse_dict<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();

    let entries: Vec<DictEntry> = delimited(
        ("{", ws0),
        separated(0.., parse_dict_entry, (ws0, ",", ws0)),
        (soc0, opt(","), soc0, "}"),
    )
    .parse_next(input)?;

    let mut pairs: Vec<(TokenKey, Node)> = Vec::new();
    let mut standalone_directives: Vec<Directive> = Vec::new();
    for entry in entries {
        match entry {
            DictEntry::Pair(key, value) => pairs.push((key, value)),
            DictEntry::Directives(dirs) => standalone_directives.extend(dirs),
        }
    }

    let end_offset = input.location();
    let mut node = Node::new(
        Expr::Dict(pairs),
        create_range(input, start_offset, end_offset),
    );
    node.directives = standalone_directives;
    Ok(node)
}

pub fn parse_dict_entry<'a>(input: &mut Span<'a>) -> ModalResult<DictEntry> {
    parse_pair(input)
}

/// Parse a `[expr]:` or v1.3 typed `[<T> expr]:` dynamic-key. Returns
/// the parsed key Node (with `type_hint` populated when the form is
/// typed) plus a placeholder optional flag (always `false` for now —
/// the parser doesn't accept `[expr]?: value` yet).
fn parse_dynamic_key<'a>(input: &mut Span<'a>) -> ModalResult<(Node, bool)> {
    let _ = winnow::token::literal::<_, _, winnow::error::ContextError>("[").parse_next(input)?;
    let _ = soc0.parse_next(input);
    // v1.3: optional `<T>` typehint between `[` and the expression.
    let mut typehint: Option<crate::TypeNode> = None;
    if input.as_ref().starts_with('<') {
        let pre_t = input.checkpoint();
        let _: ModalResult<_> =
            winnow::token::literal::<_, _, winnow::error::ContextError>("<").parse_next(input);
        let _ = soc0.parse_next(input);
        match crate::expr::parse_type_node.parse_next(input) {
            Ok(t) => {
                let _ = soc0.parse_next(input);
                if winnow::token::literal::<_, _, winnow::error::ContextError>(">")
                    .parse_next(input)
                    .is_ok()
                {
                    typehint = Some(t);
                } else {
                    input.reset(&pre_t);
                }
            }
            Err(_) => {
                input.reset(&pre_t);
            }
        }
        let _ = soc0.parse_next(input);
    }
    let mut expr = parse_expr.parse_next(input)?;
    let _ = soc0.parse_next(input);
    let _ = winnow::token::literal::<_, _, winnow::error::ContextError>("]").parse_next(input)?;
    if let Some(t) = typehint {
        expr.type_hint = Some(t);
    }
    Ok((expr, false))
}

pub(crate) fn parse_pair<'a>(input: &mut Span<'a>) -> ModalResult<DictEntry> {
    // Check for spread operator first: { ...base }, optionally typed
    // (`...<T> base`) for v1.3 strict-mode hinting. The `<T>` form binds
    // a static type to the spread source so the analyzer can validate
    // / merge keys without seeing the source's value at compile time.
    let checkpoint = input.checkpoint();
    soc0.parse_next(input)?;
    let spread_start_offset = input.location();
    if winnow::token::literal::<_, _, winnow::error::ContextError>("...")
        .parse_next(input)
        .is_ok()
    {
        let spread_end_offset = input.location();
        // v1.3: optional `<T>` typehint between `...` and the spread
        // expression. We attempt to parse a type-node only when the
        // very next char is `<`; failure rewinds so non-typed spreads
        // (`...base`) still work.
        let mut typehint: Option<crate::TypeNode> = None;
        let _ = soc0.parse_next(input);
        if input.as_ref().starts_with('<') {
            let pre_t = input.checkpoint();
            // Skip the leading `<`, parse a TypeNode, expect `>`.
            let _: ModalResult<_> =
                winnow::token::literal::<_, _, winnow::error::ContextError>("<").parse_next(input);
            let _ = soc0.parse_next(input);
            match crate::expr::parse_type_node.parse_next(input) {
                Ok(t) => {
                    let _ = soc0.parse_next(input);
                    if winnow::token::literal::<_, _, winnow::error::ContextError>(">")
                        .parse_next(input)
                        .is_ok()
                    {
                        typehint = Some(t);
                    } else {
                        input.reset(&pre_t);
                    }
                }
                Err(_) => {
                    input.reset(&pre_t);
                }
            }
            let _ = soc0.parse_next(input);
        }
        let mut base = parse_expr.parse_next(input)?;
        if let Some(t) = typehint {
            base.type_hint = Some(t);
        }
        return Ok(DictEntry::Pair(
            TokenKey::Spread(create_range(input, spread_start_offset, spread_end_offset)),
            base,
        ));
    }
    input.reset(&checkpoint);

    let doc_comment = parse_leading_comments(input)?;
    let (decs_before_key, dirs_before_key) = parse_attributes(input)?;
    soc0.parse_next(input)?;

    // Treat the directive stack as standalone (i.e. hoisted onto the
    // dict node's own `directives` list rather than attached to the
    // next pair) in either of two cases:
    //
    //   1. We're already at `,`/`}`/EOF — the stack has no value to
    //      attach to.
    //   2. The stack contains at least one self-contained directive
    //      (`#schema X Body`, `#import ... from ...`, `#main(...)`).
    //      Those forms carry their own body and are conceptually
    //      statements, not modifiers; binding them to the next dict
    //      pair would silently change their meaning.
    //
    // In both cases standalone decorators (`@foo` with no value) are
    // refused — decorators always need a value to wrap.
    let has_self_contained_directive = dirs_before_key.iter().any(|d| {
        matches!(
            d.body,
            crate::DirectiveBody::NameBody { .. }
                | crate::DirectiveBody::Import { .. }
                | crate::DirectiveBody::Main { .. }
        )
    });
    if !dirs_before_key.is_empty() {
        let peek = input.as_ref().chars().next();
        let trailing = matches!(peek, Some(',') | Some('}') | None);
        if trailing || has_self_contained_directive {
            // Decorators in this position aren't supported standalone —
            // they always need a value to wrap. Refuse the source.
            if !decs_before_key.is_empty() {
                return Err(winnow::error::ErrMode::Cut(
                    winnow::error::ContextError::default(),
                ));
            }
            return Ok(DictEntry::Directives(dirs_before_key));
        }
    }

    parse_keyed_value(input, decs_before_key, dirs_before_key, doc_comment)
}

fn parse_keyed_value<'a>(
    input: &mut Span<'a>,
    decs_before_key: Vec<Decorator>,
    dirs_before_key: Vec<Directive>,
    doc_comment: Option<String>,
) -> ModalResult<DictEntry> {
    // Try parsing type hint (optional)
    let type_checkpoint = input.checkpoint();
    let type_hint = crate::expr::parse_type_node.parse_next(input).ok();

    // Now try to parse the key. If we successfully parsed a type hint but the key parsing fails,
    // or if the key parsing succeeds but there's no colon or parenthesis, we might have parsed
    // the key itself as the type hint!

    // Helper to parse key
    fn parse_key<'a>(i: &mut Span<'a>) -> ModalResult<TokenKey> {
        soc0.parse_next(i)?;
        alt((
            parse_string.map(|node| {
                if let Expr::String(s) = *node.expr {
                    TokenKey::String(s, node.range, false)
                } else {
                    unreachable!()
                }
            }),
            // v1.7: route a tuple-typed `parse_type_node` failure
            // back to the caller so the surrounding method-shorthand
            // parse (`name(params): body`) keeps its `(`. We fail
            // early here when the parsed type is a tuple shape
            // (`path == ["Tuple"]`) — those land at this position
            // only when `parse_type_node` was matched against
            // method-shorthand parens like `name(...)`, never as a
            // legitimate dict key.
            (|i: &mut Span<'a>| -> ModalResult<TokenKey> {
                let cp = i.checkpoint();
                let t = crate::expr::parse_type_node.parse_next(i)?;
                if t.path.len() == 1 && t.path[0] == "Tuple" {
                    i.reset(&cp);
                    return Err(winnow::error::ErrMode::Backtrack(
                        winnow::error::ContextError::default(),
                    ));
                }
                // If the type node is just a simple identifier
                // without generics or optional marker, treat it as a
                // standard string key.
                if t.generics.is_empty() && t.path.len() == 1 && !t.is_optional {
                    Ok(TokenKey::String(t.path[0].clone(), t.range, false))
                } else {
                    let range = t.range;
                    Ok(TokenKey::Dynamic(Node::new(Expr::Type(t), range), false))
                }
            }),
            // Dynamic key: `[<T> expr]: value` (with optional v1.3
            // type hint) or the legacy `[expr]: value` form. The
            // optional `<T>` between `[` and the expression is
            // recognized lazily — when present, it's stamped onto the
            // inner Node's `type_hint` so the strict-mode analyzer
            // can validate `expr` against `T`.
            parse_dynamic_key.map(|(node, _opt)| TokenKey::Dynamic(node, false)),
        ))
        .parse_next(i)
    }

    let mut parsed_type_hint = None;
    let mut parsed_key = None;

    if let Some(t) = type_hint {
        if let Ok(k) = parse_key.parse_next(input) {
            // We have both Type and Key. Now we expect either '(' or ':'
            soc0.parse_next(input)?;
            let peek = input.as_ref().chars().next();
            if peek == Some(':') || peek == Some('(') {
                parsed_type_hint = Some(t);
                parsed_key = Some(k);
            }
        }
    }

    if parsed_key.is_none() {
        // Fallback: The type_hint was actually the key, or there was no type hint.
        input.reset(&type_checkpoint);
        parsed_key = Some(parse_key.parse_next(input)?);
    }

    let key = parsed_key.unwrap();
    soc0.parse_next(input)?;

    // Check for method shorthand: `Key (params) : Expr`
    let is_method = winnow::token::literal::<_, _, winnow::error::ContextError>("(")
        .parse_next(input)
        .is_ok();

    let mut params = Vec::new();
    if is_method {
        params = separated(0.., crate::expr::parse_closure_param, (soc0, ",", soc0))
            .parse_next(input)?;
        let _ = (soc0, ")").parse_next(input)?;
    }

    (soc0, ":", soc0).parse_next(input)?;

    let (decs_after_colon, dirs_after_colon) = parse_attributes(input)?;
    soc0.parse_next(input)?;

    let value_start = input.location();
    let mut value = parse_expr.parse_next(input)?;
    let value_end = input.location();

    if is_method {
        // Desugar into a closure
        let closure_expr = Expr::Closure {
            params,
            return_type: parsed_type_hint.clone(),
            body: value,
        };
        value = Node::new(closure_expr, create_range(input, value_start, value_end));
    } else {
        // It's a regular field, apply type hint
        if parsed_type_hint.is_some() {
            value = value.with_type_hint(parsed_type_hint);
        }
    }

    let mut all_decs = decs_before_key;
    all_decs.extend(decs_after_colon);
    let mut all_dirs = dirs_before_key;
    all_dirs.extend(dirs_after_colon);
    value = value
        .with_decorators(all_decs)
        .with_directives(all_dirs)
        .with_doc_comment(doc_comment);

    Ok(DictEntry::Pair(key, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dict_spread() {
        let mut s = Span::new("{ a: 1, ...base }");
        let node = parse_dict(&mut s).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            assert_eq!(pairs.len(), 2);
            if let TokenKey::Spread(_) = pairs[1].0 {
            } else {
                panic!("Expected spread key")
            }
        } else {
            panic!()
        }
    }

    #[test]
    fn test_dict_with_standalone_directive() {
        let mut s = Span::new("{ #schema X { a: 1 }, alice: 2 }");
        let node = parse_dict(&mut s).unwrap();
        assert_eq!(node.directives.len(), 1);
        if let Expr::Dict(pairs) = *node.expr {
            assert_eq!(pairs.len(), 1);
            assert_eq!(pairs[0].0.name(), "alice");
        } else {
            panic!()
        }
    }

    /// v1.3 forward: typed spread `...<T> base` stamps `T` onto the
    /// spread inner Node's `type_hint`.
    #[test]
    fn test_typed_spread_records_hint() {
        let mut s = Span::new("{ ...<Extra> base }");
        let node = parse_dict(&mut s).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            let (k, v) = &pairs[0];
            assert!(matches!(k, TokenKey::Spread(_)));
            let hint = v.type_hint.as_ref().expect("typed spread carries hint");
            assert_eq!(hint.path, vec!["Extra".to_string()]);
        } else {
            panic!()
        }
    }

    /// v1.3 reverse: untyped spread `...base` does not stamp a hint.
    #[test]
    fn test_untyped_spread_has_no_hint() {
        let mut s = Span::new("{ ...base }");
        let node = parse_dict(&mut s).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            assert!(pairs[0].1.type_hint.is_none());
        } else {
            panic!()
        }
    }

    /// v1.3 forward: typed dynamic key `[<String> k]: v` stamps `String`
    /// onto the inner key Node's `type_hint`.
    #[test]
    fn test_typed_dynamic_key_records_hint() {
        let mut s = Span::new(r#"{ [<String> "k"]: 1 }"#);
        let node = parse_dict(&mut s).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            let (k, _) = &pairs[0];
            if let TokenKey::Dynamic(inner, _) = k {
                let hint = inner.type_hint.as_ref().expect("typed key carries hint");
                assert_eq!(hint.path, vec!["String".to_string()]);
            } else {
                panic!("expected Dynamic key, got {:?}", k);
            }
        } else {
            panic!()
        }
    }

    /// v1.3 reverse: untyped dynamic key `[k]: v` has no hint on its
    /// key Node.
    #[test]
    fn test_untyped_dynamic_key_has_no_hint() {
        let mut s = Span::new(r#"{ [k]: 1 }"#);
        let node = parse_dict(&mut s).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            if let TokenKey::Dynamic(inner, _) = &pairs[0].0 {
                assert!(inner.type_hint.is_none());
            } else {
                panic!("expected Dynamic key");
            }
        } else {
            panic!()
        }
    }

    /// v1.3 forward: typed spread with a parameterized type
    /// (`...<Dict<String, Int>> kv`).
    #[test]
    fn test_typed_spread_with_parameterized_type() {
        let mut s = Span::new("{ ...<Dict<String, Int>> kv }");
        let node = parse_dict(&mut s).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            let hint = pairs[0]
                .1
                .type_hint
                .as_ref()
                .expect("typed spread carries hint");
            assert_eq!(hint.path, vec!["Dict".to_string()]);
            assert_eq!(hint.generics.len(), 2);
        } else {
            panic!()
        }
    }

    /// v1.3 boundary: a typed spread followed by a regular pair must
    /// keep the regular pair intact.
    #[test]
    fn test_typed_spread_followed_by_named_pair() {
        let mut s = Span::new("{ ...<Extra> e, x: 1 }");
        let node = parse_dict(&mut s).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            assert_eq!(pairs.len(), 2);
            assert!(matches!(pairs[0].0, TokenKey::Spread(_)));
            assert!(pairs[0].1.type_hint.is_some());
            assert_eq!(pairs[1].0.name(), "x");
        } else {
            panic!()
        }
    }

    /// v1.3 boundary: typed dynamic key followed by a regular pair.
    #[test]
    fn test_typed_dynkey_followed_by_named_pair() {
        let mut s = Span::new(r#"{ [<String> "k"]: 1, x: 2 }"#);
        let node = parse_dict(&mut s).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            assert_eq!(pairs.len(), 2);
            if let TokenKey::Dynamic(inner, _) = &pairs[0].0 {
                assert!(inner.type_hint.is_some());
            } else {
                panic!()
            }
            assert_eq!(pairs[1].0.name(), "x");
        } else {
            panic!()
        }
    }

    /// v1.3 boundary: nested dict literals carry their own typed
    /// spread; outer + inner both record hints.
    #[test]
    fn test_nested_typed_spreads() {
        let mut s = Span::new("{ ...<Outer> a, x: { ...<Inner> b } }");
        let node = parse_dict(&mut s).unwrap();
        if let Expr::Dict(outer) = *node.expr {
            assert_eq!(outer[0].1.type_hint.as_ref().unwrap().path[0], "Outer");
            if let Expr::Dict(inner) = &*outer[1].1.expr {
                assert_eq!(inner[0].1.type_hint.as_ref().unwrap().path[0], "Inner");
            } else {
                panic!()
            }
        } else {
            panic!()
        }
    }
}
