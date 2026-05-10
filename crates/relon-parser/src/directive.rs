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
    create_range, soc0, Directive, DirectiveImportSpec, DirectiveMainParam, DirectiveShape, Expr,
    Span, TokenKey,
};
use winnow::combinator::{opt, preceded, repeat, separated};
use winnow::prelude::*;
use winnow::stream::{Location, Stream};

/// Canonical directive names. Centralizing the strings here lets
/// downstream crates (`relon-analyzer`, `relon-evaluator`) refer to the
/// same identifiers without maintaining their own private mirrors.
pub const PRIVATE: &str = "private";
pub const DEFAULT: &str = "default";
pub const EXPECT: &str = "expect";
pub const MSG: &str = "msg";
pub const ERROR: &str = "error";
pub const BRAND: &str = "brand";
pub const SCHEMA: &str = "schema";
pub const IMPORT: &str = "import";
pub const MAIN: &str = "main";
/// v1.3: `#strict` — bare directive at the file level enabling strict
/// inference mode. Once present, every value must have a statically
/// inferable type; sites that the analyzer would otherwise silently
/// fall back on (uninferrable spread sources, dynamic keys without a
/// type hint, references with no type, native fn returns, …) become
/// errors. The flag is *contagious*: an entry module marked `#strict`
/// applies the rule transitively to every reachable `#import` target.
pub const STRICT: &str = "strict";
/// Phase A of the trait-bound / schema-method system: a method-level
/// pragma `#derive <Constraint>` declares the following method is the
/// witness for the named built-in constraint (e.g. `Equatable`,
/// `Comparable`). Body shape is a single bare identifier (the
/// constraint name). Registered globally so the parser accepts it; the
/// analyzer enforces that it only appears immediately above a method
/// inside a `with { ... }` block.
pub const DERIVE: &str = "derive";
/// Schema-level (or, in rare cases, method-level) pragma
/// `#no_auto_derive <Constraint>` opts the schema out of structural
/// auto-derivation for the named constraint (e.g. opt out of the
/// default `JsonProjectable` derivation for an internal-only schema).
pub const NO_AUTO_DERIVE: &str = "no_auto_derive";
/// Method-level pragma `#native` declares the method's body lives in
/// host Rust (registered through the schema-method host API). The
/// parser leaves the method's body empty when this pragma is present;
/// the analyzer cross-checks against the host registry.
pub const NATIVE: &str = "native";
/// Schema-rooted Phase A.1: `#extend X with { ... }` adds methods to
/// an already-declared schema X (built-in or user). Same parser shape
/// as `#schema` (NameBody), distinguished from `#schema` by the
/// directive name. Visibility is tied to the file's `#import` chain
/// (decision 9). Cannot re-declare X — only extend its method table.
///
/// Note: method-level `#private` is the existing [`PRIVATE`] directive
/// reused — `#private` was already a field-level visibility marker for
/// schema bodies. In a `with { ... }` block, the same `#private`
/// directive marks a method as schema-internal (only callable from
/// other method bodies on the same schema).
pub const EXTEND: &str = "extend";

/// Directive name → expected shape. Dispatch happens by name; unknown
/// `#name` produces a parse error.
pub const DIRECTIVE_SHAPES: &[(&str, DirectiveShape)] = &[
    (PRIVATE, DirectiveShape::Bare),
    (DEFAULT, DirectiveShape::Value),
    (EXPECT, DirectiveShape::Value),
    (MSG, DirectiveShape::Value),
    (ERROR, DirectiveShape::Value),
    (BRAND, DirectiveShape::Value),
    (SCHEMA, DirectiveShape::NameBody),
    (IMPORT, DirectiveShape::Import),
    (MAIN, DirectiveShape::Main),
    (STRICT, DirectiveShape::Bare),
    // Trait-bound / schema-method pragmas (Phase A): parsed globally,
    // semantic placement enforced by the analyzer.
    (DERIVE, DirectiveShape::Value),
    (NO_AUTO_DERIVE, DirectiveShape::Value),
    (NATIVE, DirectiveShape::Bare),
    (EXTEND, DirectiveShape::NameBody),
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

/// Parse a single `#name ...` directive. Used by callers that want to
/// interleave `@`-decorators and `#`-directives explicitly.
pub fn parse_directive<'a>(input: &mut Span<'a>) -> ModalResult<Directive> {
    directive(input)
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
        DirectiveShape::NameBody => parse_name_body(input)?,
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

/// Parse a name-body directive body: `<ident> <body-expr>` (no colon).
///
/// Special-case: when the would-be ident is followed by `:` (after
/// optional whitespace) the directive is interpreted as **bare** —
/// the `<ident> :` is left for the surrounding dict-field grammar.
/// This is what enables `#schema User: { ... }` inside a dict to
/// decorate the `User: { ... }` field, while a standalone
/// `#schema User { ... }` (no colon) parses as a name-body declaration.
fn parse_name_body<'a>(input: &mut Span<'a>) -> ModalResult<crate::DirectiveBody> {
    let pre_body_checkpoint = input.checkpoint();
    let _ = soc0.parse_next(input)?;
    let after_ws = input.checkpoint();
    let name_start = input.location();
    let Ok(name_token) = id.parse_next(input) else {
        // No identifier — treat as bare directive and rewind.
        input.reset(&pre_body_checkpoint);
        return Ok(crate::DirectiveBody::Bare);
    };
    let name_end = input.location();
    let name_range = create_range(input, name_start, name_end);

    // Optional `<T, U, ...>` type-parameter list. Form params are bare
    // identifiers — nested types are not allowed in *declarations*. We
    // try-parse and rewind on failure so anything that doesn't look like
    // a generic clause is left for the surrounding grammar.
    let pre_generics = input.checkpoint();
    let generics: Vec<String> = if input.as_ref().starts_with('<') {
        match parse_generic_param_list(input) {
            Ok(list) => list,
            Err(_) => {
                input.reset(&pre_generics);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let _ = soc0.parse_next(input)?;
    let peek = input.as_ref().chars().next();
    if peek == Some(':') {
        // Surrounding context is `<ident>: <value>` — leave it for
        // the dict-field parser. Bare-directive form.
        input.reset(&after_ws);
        return Ok(crate::DirectiveBody::Bare);
    }

    // Schema-rooted Phase A.1: `#schema X with { ... }` and
    // `#extend X with { ... }` are body-less — they carry only a
    // method-table contribution. When `with` sits at a word boundary
    // immediately after the name (no field-shape body in between),
    // synthesize an empty dict body so downstream code that pattern-
    // matches `NameBody { body, .. }` keeps working unchanged. The
    // analyzer differentiates `schema` vs `extend` by the directive's
    // `name` field, not by the body shape.
    let (body, with_already_consumed) = if at_keyword(input, "with") {
        let body_range = create_range(input, input.location(), input.location());
        let synth = crate::Node {
            id: crate::NodeId::alloc(),
            expr: Box::new(crate::Expr::Dict(Vec::new())),
            decorators: Vec::new(),
            directives: Vec::new(),
            type_hint: None,
            range: body_range,
            doc_comment: None,
        };
        (synth, false)
    } else {
        match parse_expr.parse_next(input) {
            Ok(b) => (b, false),
            Err(_) => {
                // No body — bare directive, rewind so the surrounding
                // grammar sees the ident again (e.g. as a key).
                input.reset(&after_ws);
                return Ok(crate::DirectiveBody::Bare);
            }
        }
    };
    let _ = with_already_consumed; // reserved for future grammar tweaks

    // Phase A: optional trailing `with { ... }` block carrying schema
    // methods. Detection is conservative — if `with` doesn't sit at a
    // word boundary or `{` doesn't follow, leave the input alone for
    // the surrounding grammar to consume.
    let (methods, schema_no_auto_derives) = opt_parse_with_block(input).unwrap_or_default();

    Ok(crate::DirectiveBody::NameBody {
        name: name_token.0,
        name_range,
        generics,
        body: Box::new(body),
        methods,
        schema_no_auto_derives,
    })
}

/// Try to consume a trailing `with { ... }` block after a schema body.
/// Returns the parsed methods + schema-level `#no_auto_derive` constraint
/// names. When the input doesn't start with `with` (after optional
/// whitespace), the input is rewound and `None` is returned. Inside the
/// `with { ... }` block, parse errors are propagated as cuts — once
/// committed (we saw `with {`), malformed contents must surface as
/// errors rather than silent rewind.
fn opt_parse_with_block<'a>(
    input: &mut Span<'a>,
) -> Option<(Vec<crate::SchemaMethod>, Vec<String>)> {
    let pre = input.checkpoint();
    if soc0.parse_next(input).is_err() {
        input.reset(&pre);
        return None;
    }
    if !at_keyword(input, "with") {
        input.reset(&pre);
        return None;
    }
    // Consume the keyword and the opening brace.
    let _ = winnow::token::literal::<_, _, winnow::error::ContextError>("with")
        .parse_next(input)
        .ok()?;
    if (soc0, '{').parse_next(input).is_err() {
        input.reset(&pre);
        return None;
    }

    let mut methods: Vec<crate::SchemaMethod> = Vec::new();
    let mut schema_no_auto_derives: Vec<String> = Vec::new();

    loop {
        if soc0.parse_next(input).is_err() {
            return Some((methods, schema_no_auto_derives));
        }
        if winnow::token::literal::<_, _, winnow::error::ContextError>("}")
            .parse_next(input)
            .is_ok()
        {
            return Some((methods, schema_no_auto_derives));
        }

        // Collect leading directive pragmas. Each one is attributed to
        // the right scope by name:
        //   * `#derive C` / `#native` are method-level — they decorate
        //     the next method declaration in this `with { ... }` block.
        //   * `#no_auto_derive C` is *always* schema-level — it opts
        //     the enclosing schema out of structural derivation, so it
        //     lands directly on `schema_no_auto_derives` regardless of
        //     whether a method follows. Mixing the two scopes in one
        //     stack is allowed; source order between them is preserved
        //     within each scope's vec.
        let mut method_derives: Vec<String> = Vec::new();
        let mut is_native = false;
        let mut is_private = false;
        loop {
            let pre_dir = input.checkpoint();
            let _ = soc0.parse_next(input);
            if !input.as_ref().starts_with('#') {
                input.reset(&pre_dir);
                break;
            }
            let dir = match parse_directive(input) {
                Ok(d) => d,
                Err(_) => {
                    input.reset(&pre_dir);
                    break;
                }
            };
            match dir.name.as_str() {
                DERIVE => match constraint_name_from_value(&dir.body) {
                    Some(name) => method_derives.push(name),
                    None => return None,
                },
                NO_AUTO_DERIVE => match constraint_name_from_value(&dir.body) {
                    Some(name) => schema_no_auto_derives.push(name),
                    None => return None,
                },
                NATIVE => is_native = true,
                PRIVATE => is_private = true,
                _ => {
                    // Not a method/schema pragma — bail out of the with
                    // block; the directive is likely intended for the
                    // surrounding grammar. Rewind any progress made.
                    return None;
                }
            }
        }

        let _ = soc0.parse_next(input);

        // After the pragma stack, either a method header follows or we
        // hit `}` — in which case any collected method-level pragma
        // (`#derive` / `#native`) without a method is a parse error
        // (`#no_auto_derive` already landed on schema_no_auto_derives).
        if winnow::token::literal::<_, _, winnow::error::ContextError>("}")
            .parse_next(input)
            .is_ok()
        {
            if !method_derives.is_empty() || is_native {
                // Stray method pragmas without a following method.
                return None;
            }
            return Some((methods, schema_no_auto_derives));
        }

        // Method header: `name(p: T, ...) -> R`.
        let method_start = input.location();
        let name_start = input.location();
        let name_token = match id.parse_next(input) {
            Ok(t) => t,
            Err(_) => return None,
        };
        let name_range = create_range(input, name_start, input.location());

        if (soc0, '(', soc0).parse_next(input).is_err() {
            return None;
        }
        let params: Vec<crate::SchemaMethodParam> =
            match separated::<_, _, Vec<crate::SchemaMethodParam>, _, _, _, _>(
                0..,
                parse_schema_method_param,
                (soc0, ',', soc0),
            )
            .parse_next(input)
            {
                Ok(v) => v,
                Err(_) => return None,
            };
        if (soc0, opt(','), soc0, ')').parse_next(input).is_err() {
            return None;
        }
        if (soc0, "->", soc0).parse_next(input).is_err() {
            return None;
        }
        let return_type = match crate::expr::parse_type_node.parse_next(input) {
            Ok(t) => t,
            Err(_) => return None,
        };

        // Body: `: <expr>` for non-native methods; absent for `#native`.
        let body = if is_native {
            None
        } else {
            if (soc0, ':', soc0).parse_next(input).is_err() {
                return None;
            }
            match parse_expr.parse_next(input) {
                Ok(b) => Some(Box::new(b)),
                Err(_) => return None,
            }
        };

        let method_end = input.location();
        methods.push(crate::SchemaMethod {
            name: name_token.0,
            name_range,
            params,
            return_type,
            body,
            derives: method_derives,
            is_native,
            is_private,
            range: create_range(input, method_start, method_end),
            doc_comment: None,
        });
    }
}

/// Parse one `<ident>: <TypeNode>` schema-method parameter.
fn parse_schema_method_param<'a>(input: &mut Span<'a>) -> ModalResult<crate::SchemaMethodParam> {
    soc0.parse_next(input)?;
    let name_start = input.location();
    let name_token = id.parse_next(input)?;
    let name_range = create_range(input, name_start, input.location());
    let _ = (soc0, ':', soc0).parse_next(input)?;
    let type_node = crate::expr::parse_type_node.parse_next(input)?;
    Ok(crate::SchemaMethodParam {
        name: name_token.0,
        name_range,
        type_node,
    })
}

/// Extract a bare-identifier constraint name from a `#derive` /
/// `#no_auto_derive` directive body. Returns `None` when the body isn't
/// a single Path of one segment (which would be a parser-level
/// programming error — the analyzer surfaces a friendlier diagnostic).
fn constraint_name_from_value(body: &crate::DirectiveBody) -> Option<String> {
    let crate::DirectiveBody::Value(node) = body else {
        return None;
    };
    let Expr::Variable(path) = node.expr.as_ref() else {
        return None;
    };
    if path.len() != 1 {
        return None;
    }
    match path.first()? {
        TokenKey::String(s, _, _) => Some(s.clone()),
        _ => None,
    }
}

/// Word-boundary check: does the input start with `keyword` followed by
/// a non-identifier character (whitespace, `{`, `(`, EOF, etc.)?
/// Without this `with` would also match the prefix of `withhold` etc.
fn at_keyword(input: &Span<'_>, keyword: &str) -> bool {
    let s = input.as_ref();
    if !s.starts_with(keyword) {
        return false;
    }
    match s.as_bytes().get(keyword.len()) {
        None => true,
        Some(&b) => !b.is_ascii_alphanumeric() && b != b'_',
    }
}

/// Parse `<T, U, ...>` — a comma-separated list of bare identifiers.
/// Used by `parse_name_body` to capture schema type parameters such as
/// `#schema Result<T, E> ...`.
fn parse_generic_param_list<'a>(input: &mut Span<'a>) -> ModalResult<Vec<String>> {
    let _ = '<'.parse_next(input)?;
    let _ = soc0.parse_next(input)?;
    let names: Vec<String> =
        separated(1.., id.map(|t: crate::TokenId| t.0), (soc0, ',', soc0)).parse_next(input)?;
    let _ = (soc0, opt(','), soc0, '>').parse_next(input)?;
    Ok(names)
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
    let alias = if (soc0, "as", soc0).parse_next(input).is_ok() {
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

    // Optional `-> ReturnType` after the parameter list. When absent
    // the evaluator leaves the entry's return value unchecked.
    let rt_checkpoint = input.checkpoint();
    let return_type = if (soc0, "->", soc0).parse_next(input).is_ok() {
        match crate::expr::parse_type_node.parse_next(input) {
            Ok(t) => Some(t),
            Err(_) => {
                input.reset(&rt_checkpoint);
                None
            }
        }
    } else {
        input.reset(&rt_checkpoint);
        None
    };

    Ok(crate::DirectiveBody::Main {
        params,
        return_type,
    })
}

fn parse_main_param<'a>(input: &mut Span<'a>) -> ModalResult<DirectiveMainParam> {
    soc0.parse_next(input)?;
    let type_node = crate::expr::parse_type_node.parse_next(input)?;
    let _ = soc0.parse_next(input)?;
    let start_offset = input.location();
    let name_token = id.parse_next(input)?;
    let name_range = create_range(input, start_offset, input.location());
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
    fn parses_name_body_directive() {
        let mut s = Span::new("#schema User { String name: * }");
        let dirs = parse_directives(&mut s).unwrap();
        assert_eq!(dirs.len(), 1);
        match &dirs[0].body {
            crate::DirectiveBody::NameBody { name, generics, .. } => {
                assert_eq!(name, "User");
                assert!(generics.is_empty());
            }
            _ => panic!("expected NameBody"),
        }
    }

    #[test]
    fn parses_name_body_with_generics() {
        let mut s = Span::new("#schema Result<T, E> Enum<Ok, Err>");
        let dirs = parse_directives(&mut s).unwrap();
        match &dirs[0].body {
            crate::DirectiveBody::NameBody { name, generics, .. } => {
                assert_eq!(name, "Result");
                assert_eq!(generics, &vec!["T".to_string(), "E".to_string()]);
            }
            _ => panic!("expected NameBody"),
        }
    }

    #[test]
    fn parses_name_body_single_generic() {
        let mut s = Span::new("#schema Box<T> { T value: * }");
        let dirs = parse_directives(&mut s).unwrap();
        match &dirs[0].body {
            crate::DirectiveBody::NameBody { name, generics, .. } => {
                assert_eq!(name, "Box");
                assert_eq!(generics, &vec!["T".to_string()]);
            }
            _ => panic!("expected NameBody"),
        }
    }

    #[test]
    fn schema_bare_form_for_dict_field() {
        // Inside a dict literal, `#schema User: { ... }` decorates the
        // `User: { ... }` field — the directive is bare and the `User:`
        // is a dict-field key. Our parser rewinds the ident so the
        // dict-pair parser can see it.
        let mut s = Span::new("#schema User: { x: 1 }");
        let dirs = parse_directives(&mut s).unwrap();
        assert_eq!(dirs.len(), 1);
        assert!(matches!(dirs[0].body, crate::DirectiveBody::Bare));
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
            crate::DirectiveBody::Import {
                spec: DirectiveImportSpec::Destructure(entries),
                ..
            } => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0], ("upper".to_string(), None));
                assert_eq!(entries[1], ("lower".to_string(), Some("lo".to_string())));
            }
            _ => panic!("expected Destructure"),
        }
    }

    #[test]
    fn parses_main_directive() {
        let mut s = Span::new("#main(User u, Cart cart)");
        let dirs = parse_directives(&mut s).unwrap();
        match &dirs[0].body {
            crate::DirectiveBody::Main {
                params,
                return_type,
            } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].name, "u");
                assert_eq!(params[0].type_node.path, vec!["User".to_string()]);
                assert_eq!(params[1].name, "cart");
                assert_eq!(params[1].type_node.path, vec!["Cart".to_string()]);
                assert!(return_type.is_none());
            }
            _ => panic!("expected Main"),
        }
    }

    #[test]
    fn parses_main_with_return_type() {
        let mut s = Span::new("#main(User u) -> Result<Order>");
        let dirs = parse_directives(&mut s).unwrap();
        match &dirs[0].body {
            crate::DirectiveBody::Main {
                params,
                return_type,
            } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "u");
                let rt = return_type.as_ref().expect("return_type present");
                assert_eq!(rt.path, vec!["Result".to_string()]);
                assert_eq!(rt.generics.len(), 1);
                assert_eq!(rt.generics[0].path, vec!["Order".to_string()]);
            }
            _ => panic!("expected Main"),
        }
    }

    #[test]
    fn rejects_unknown_directive_name() {
        let mut s = Span::new("#bogus 0");
        assert!(parse_directives(&mut s).is_err());
    }

    /// v1.3 forward: bare `#strict` directive parses as a Bare body.
    #[test]
    fn parses_strict_directive() {
        let mut s = Span::new("#strict");
        let dirs = parse_directives(&mut s).unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].name, "strict");
        assert!(matches!(dirs[0].body, crate::DirectiveBody::Bare));
    }

    /// v1.3: `#strict` interleaves with other directives.
    #[test]
    fn parses_strict_alongside_other_directives() {
        let mut s = Span::new("#strict\n#main(Int n) -> Int");
        let dirs = parse_directives(&mut s).unwrap();
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].name, "strict");
        assert_eq!(dirs[1].name, "main");
    }
}
