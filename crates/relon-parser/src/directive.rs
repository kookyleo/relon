//! `#name ...` directive parsing.
//!
//! Directives are the **structural / declarative** half of the language's
//! sigil split (`@` vs `#`):
//!
//! * `@name(...)` — value-transform decorators (host-registerable, also
//!   user-definable as `@f` where `f` is a closure or native fn).
//! * `#name ...` — directives that *describe attributes* of a node or the
//!   file itself (imports, schemas, defaults, error messages, brand,
//!   privacy, the `#main(...)` entry signature). Host-registered only;
//!   no user-definable `#`.
//!
//! Each directive has one of five fixed shapes (see [`DirectiveShape`]),
//! parsed by name dispatch. The 9 directive names recognized in v1 are
//! listed in [`DIRECTIVE_SHAPES`].
//!
//! The parser collects directives in the same positions decorators were
//! collected: stacked above a root `{...}` / `[...]` for root-level
//! directives, or stacked above a dict-field key for field-level ones.

use crate::expr::parse_expr;
use crate::id::id;
use crate::prim::string::parse_string;
use crate::var::parse_path;
use crate::{
    create_range, soc0, Directive, DirectiveBinding, DirectiveImportSpec, DirectiveMainParam,
    DirectiveShape, Expr, Span, TokenKey,
};
use winnow::combinator::{opt, preceded, repeat, separated};
use winnow::prelude::*;
use winnow::stream::{Location, Stream};

/// Directive name → expected shape. Dispatch happens by name; unknown
/// `#name` produces a parse error.
pub const DIRECTIVE_SHAPES: &[(&str, DirectiveShape)] = &[
    ("private", DirectiveShape::Bare),
    ("default", DirectiveShape::Value),
    ("expect", DirectiveShape::Value),
    ("msg", DirectiveShape::Value),
    ("error", DirectiveShape::Value),
    ("brand", DirectiveShape::Value),
    ("schema", DirectiveShape::Binding),
    ("import", DirectiveShape::Import),
    ("main", DirectiveShape::Main),
];

/// Look up a directive's expected shape by name. Returns `None` for
/// unknown directives.
pub fn directive_shape(name: &str) -> Option<DirectiveShape> {
    DIRECTIVE_SHAPES
        .iter()
        .find_map(|(n, s)| (*n == name).then_some(*s))
}

/// Parse zero or more leading directives, each starting with `#`.
pub fn parse_directives<'a>(input: &mut Span<'a>) -> ModalResult<Vec<Directive>> {
    repeat(0.., preceded(soc0, directive)).parse_next(input)
}

fn directive<'a>(input: &mut Span<'a>) -> ModalResult<Directive> {
    let start_offset = input.location();
    let _ = '#'.parse_next(input)?;
    let name_token = id.parse_next(input)?;
    let name = name_token.0;
    let Some(shape) = directive_shape(&name) else {
        return Err(winnow::error::ErrMode::Cut(
            winnow::error::ContextError::default(),
        ));
    };

    let body = match shape {
        DirectiveShape::Bare => parse_bare_body(input)?,
        DirectiveShape::Value => parse_value_body(input)?,
        DirectiveShape::Binding => parse_binding_body(input)?,
        DirectiveShape::Import => parse_import_body(input)?,
        DirectiveShape::Main => parse_main_body(input)?,
    };

    let end_offset = input.location();
    Ok(Directive {
        name,
        body,
        range: create_range(input, start_offset, end_offset),
    })
}

fn parse_bare_body<'a>(_input: &mut Span<'a>) -> ModalResult<crate::DirectiveBody> {
    Ok(crate::DirectiveBody::Bare)
}

fn parse_value_body<'a>(input: &mut Span<'a>) -> ModalResult<crate::DirectiveBody> {
    soc0.parse_next(input)?;
    let value = parse_expr.parse_next(input)?;
    Ok(crate::DirectiveBody::Value(Box::new(value)))
}

fn parse_binding_body<'a>(input: &mut Span<'a>) -> ModalResult<crate::DirectiveBody> {
    soc0.parse_next(input)?;
    let bindings: Vec<DirectiveBinding> =
        separated(1.., parse_binding, (soc0, ',', soc0)).parse_next(input)?;
    Ok(crate::DirectiveBody::Bindings(bindings))
}

fn parse_binding<'a>(input: &mut Span<'a>) -> ModalResult<DirectiveBinding> {
    soc0.parse_next(input)?;
    let start_offset = input.location();
    let name_token = id.parse_next(input)?;
    let name_range = create_range(input, start_offset, input.location());
    let _ = (soc0, ':', soc0).parse_next(input)?;
    let value = parse_expr.parse_next(input)?;
    Ok(DirectiveBinding {
        name: name_token.0,
        name_range,
        value: Box::new(value),
    })
}

fn parse_import_body<'a>(input: &mut Span<'a>) -> ModalResult<crate::DirectiveBody> {
    soc0.parse_next(input)?;
    let spec = parse_import_spec(input)?;
    let _ = (soc0, "from", soc0).parse_next(input)?;
    let path_node = parse_string.parse_next(input)?;
    let path = match path_node.expr.as_ref() {
        Expr::String(s) => s.clone(),
        _ => unreachable!("parse_string yields Expr::String"),
    };
    Ok(crate::DirectiveBody::Import {
        spec,
        path,
        path_range: path_node.range,
    })
}

fn parse_import_spec<'a>(input: &mut Span<'a>) -> ModalResult<DirectiveImportSpec> {
    // `*`, `{ a, b as c }`, or a single dotted path.
    let checkpoint = input.checkpoint();
    if winnow::token::literal::<_, _, winnow::error::ContextError>("*")
        .parse_next(input)
        .is_ok()
    {
        return Ok(DirectiveImportSpec::Spread);
    }
    input.reset(&checkpoint);

    if winnow::token::literal::<_, _, winnow::error::ContextError>("{")
        .parse_next(input)
        .is_ok()
    {
        let entries: Vec<(String, Option<String>)> =
            separated(1.., parse_destruct_entry, (soc0, ',', soc0)).parse_next(input)?;
        let _ = (soc0, opt(','), soc0, '}').parse_next(input)?;
        return Ok(DirectiveImportSpec::Destructure(entries));
    }
    input.reset(&checkpoint);

    // Single alias — must be a bare identifier (parser limits this to
    // single path segments; longer dotted paths are reserved for future).
    let path = parse_path.parse_next(input)?;
    let alias = match path.first() {
        Some(TokenKey::String(s, _, _)) if path.len() == 1 => s.clone(),
        _ => {
            return Err(winnow::error::ErrMode::Cut(
                winnow::error::ContextError::default(),
            ))
        }
    };
    Ok(DirectiveImportSpec::Alias(alias))
}

fn parse_destruct_entry<'a>(input: &mut Span<'a>) -> ModalResult<(String, Option<String>)> {
    soc0.parse_next(input)?;
    let name = id.parse_next(input)?.0;
    let alias_checkpoint = input.checkpoint();
    let alias = if (soc0, "as", soc0)
        .parse_next(input)
        .is_ok()
    {
        match id.parse_next(input) {
            Ok(t) => Some(t.0),
            Err(e) => {
                input.reset(&alias_checkpoint);
                return Err(e);
            }
        }
    } else {
        input.reset(&alias_checkpoint);
        None
    };
    Ok((name, alias))
}

fn parse_main_body<'a>(input: &mut Span<'a>) -> ModalResult<crate::DirectiveBody> {
    let _ = (soc0, '(', soc0).parse_next(input)?;
    let params: Vec<DirectiveMainParam> =
        separated(0.., parse_main_param, (soc0, ',', soc0)).parse_next(input)?;
    let _ = (soc0, opt(','), soc0, ')').parse_next(input)?;
    Ok(crate::DirectiveBody::Main { params })
}

fn parse_main_param<'a>(input: &mut Span<'a>) -> ModalResult<DirectiveMainParam> {
    soc0.parse_next(input)?;
    let start_offset = input.location();
    let name_token = id.parse_next(input)?;
    let name_range = create_range(input, start_offset, input.location());
    let _ = (soc0, ':', soc0).parse_next(input)?;
    let type_node = crate::expr::parse_type_node.parse_next(input)?;
    Ok(DirectiveMainParam {
        name: name_token.0,
        name_range,
        type_node,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_directive() {
        let mut s = Span::new("#private");
        let dirs = parse_directives(&mut s).unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].name, "private");
        assert!(matches!(dirs[0].body, crate::DirectiveBody::Bare));
    }

    #[test]
    fn parses_value_directive() {
        let mut s = Span::new("#default 0");
        let dirs = parse_directives(&mut s).unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].name, "default");
        assert!(matches!(dirs[0].body, crate::DirectiveBody::Value(_)));
    }

    #[test]
    fn parses_binding_directive_single() {
        let mut s = Span::new("#schema User : { String name: * }");
        let dirs = parse_directives(&mut s).unwrap();
        assert_eq!(dirs.len(), 1);
        match &dirs[0].body {
            crate::DirectiveBody::Bindings(b) => {
                assert_eq!(b.len(), 1);
                assert_eq!(b[0].name, "User");
            }
            _ => panic!("expected Bindings"),
        }
    }

    #[test]
    fn parses_import_alias() {
        let mut s = Span::new(r#"#import string from "std/string""#);
        let dirs = parse_directives(&mut s).unwrap();
        assert_eq!(dirs.len(), 1);
        match &dirs[0].body {
            crate::DirectiveBody::Import { spec, path, .. } => {
                assert!(matches!(spec, DirectiveImportSpec::Alias(s) if s == "string"));
                assert_eq!(path, "std/string");
            }
            _ => panic!("expected Import"),
        }
    }

    #[test]
    fn parses_import_spread() {
        let mut s = Span::new(r#"#import * from "std/list""#);
        let dirs = parse_directives(&mut s).unwrap();
        assert!(matches!(
            dirs[0].body,
            crate::DirectiveBody::Import {
                spec: DirectiveImportSpec::Spread,
                ..
            }
        ));
    }

    #[test]
    fn parses_import_destructure() {
        let mut s = Span::new(r#"#import { upper, lower as lo } from "std/string""#);
        let dirs = parse_directives(&mut s).unwrap();
        match &dirs[0].body {
            crate::DirectiveBody::Import { spec: DirectiveImportSpec::Destructure(entries), .. } => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0], ("upper".to_string(), None));
                assert_eq!(entries[1], ("lower".to_string(), Some("lo".to_string())));
            }
            _ => panic!("expected Destructure"),
        }
    }

    #[test]
    fn parses_main_directive() {
        let mut s = Span::new("#main(u: User, cart: Cart)");
        let dirs = parse_directives(&mut s).unwrap();
        match &dirs[0].body {
            crate::DirectiveBody::Main { params } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].name, "u");
                assert_eq!(params[1].name, "cart");
            }
            _ => panic!("expected Main"),
        }
    }

    #[test]
    fn rejects_unknown_directive_name() {
        let mut s = Span::new("#bogus 0");
        assert!(parse_directives(&mut s).is_err());
    }
}
