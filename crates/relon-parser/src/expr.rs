use crate::fn_call::parse_fn_call;
use crate::prim::{parse_bool, parse_null, parse_number, parse_string};
use crate::reference_var::parse_ref_var;
use crate::structure::dict::parse_dict;
use crate::structure::list::parse_list;
use crate::var::parse_var;
use crate::{combine_ranges, create_range, soc0, Expr, Node, Operator, Span};
use winnow::combinator::{alt, delimited, opt, preceded, repeat, separated};
use winnow::prelude::*;
use winnow::stream::{Location, Stream};
use winnow::token::literal;

pub fn parse_expr<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    parse_where(input)
}

// Level 10: Where (expr where bindings)
fn parse_where<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();
    let main_expr = parse_match.parse_next(input)?;

    let checkpoint = input.checkpoint();
    if (soc0, "where", soc0).parse_next(input).is_ok() {
        if let Ok(bindings) = crate::structure::dict::parse_dict.parse_next(input) {
            let end_offset = input.location();
            return Ok(Node::new(
                Expr::Where {
                    expr: main_expr,
                    bindings,
                },
                create_range(input, start_offset, end_offset),
            ));
        }
    }
    input.reset(&checkpoint);
    Ok(main_expr)
}

// Level 9.5: Match (expr match { arms })
fn parse_match<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();
    let main_expr = parse_ternary.parse_next(input)?;

    let checkpoint = input.checkpoint();
    if (soc0, "match", soc0, "{").parse_next(input).is_ok() {
        let arms = separated(0.., parse_match_arm, (soc0, ",", soc0)).parse_next(input)?;
        let _ = (soc0, opt(","), soc0, "}").parse_next(input)?;
        let end_offset = input.location();
        Ok(Node::new(
            Expr::Match {
                expr: main_expr,
                arms,
            },
            create_range(input, start_offset, end_offset),
        ))
    } else {
        input.reset(&checkpoint);
        Ok(main_expr)
    }
}

fn parse_match_arm<'a>(input: &mut Span<'a>) -> ModalResult<(Node, Node)> {
    let pattern = preceded(
        soc0,
        alt((
            parse_type_node.map(|t| {
                let range = t.range;
                Node::new(Expr::Type(t), range)
            }),
            parse_wildcard,
        )),
    )
    .parse_next(input)?;
    let _ = (soc0, ":", soc0).parse_next(input)?;
    let result = parse_expr.parse_next(input)?;
    Ok((pattern, result))
}

// Level 9: Ternary (cond ? then : else)
fn parse_ternary<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();
    let cond = parse_pipe.parse_next(input)?;

    let checkpoint = input.checkpoint();
    if (soc0, '?', soc0).parse_next(input).is_ok() {
        let then = parse_expr.parse_next(input)?;
        let _ = (soc0, ':', soc0).parse_next(input)?;
        let els = parse_expr.parse_next(input)?;
        let end_offset = input.location();
        Ok(Node::new(
            Expr::Ternary { cond, then, els },
            create_range(input, start_offset, end_offset),
        ))
    } else {
        input.reset(&checkpoint);
        Ok(cond)
    }
}

// Level 8: Pipe (|)
fn parse_pipe<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let left = parse_logic_or.parse_next(input)?;
    let rest: Vec<(Operator, Node)> = repeat(
        0..,
        ((soc0, "|", soc0).value(Operator::Pipe), parse_logic_or),
    )
    .parse_next(input)?;
    Ok(fold_binary(left, rest))
}

// Level 7: Logic OR (||)
fn parse_logic_or<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let left = parse_logic_and.parse_next(input)?;
    let rest: Vec<(Operator, Node)> = repeat(
        0..,
        ((soc0, "||", soc0).value(Operator::Or), parse_logic_and),
    )
    .parse_next(input)?;
    Ok(fold_binary(left, rest))
}

// Level 6: Logic AND (&&)
fn parse_logic_and<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let left = parse_comparison.parse_next(input)?;
    let rest: Vec<(Operator, Node)> = repeat(
        0..,
        ((soc0, "&&", soc0).value(Operator::And), parse_comparison),
    )
    .parse_next(input)?;
    Ok(fold_binary(left, rest))
}

// Level 5: Comparison (==, !=, <, >, <=, >=)
fn parse_comparison<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let left = parse_additive.parse_next(input)?;
    let rest: Vec<(Operator, Node)> = repeat(
        0..,
        (
            (
                soc0,
                alt((
                    literal("==").value(Operator::Eq),
                    literal("!=").value(Operator::Ne),
                    literal("<=").value(Operator::Le),
                    literal(">=").value(Operator::Ge),
                    literal("<").value(Operator::Lt),
                    literal(">").value(Operator::Gt),
                )),
                soc0,
            )
                .map(|(_, op, _)| op),
            parse_additive,
        ),
    )
    .parse_next(input)?;
    Ok(fold_binary(left, rest))
}

// Level 4: Additive (+, -)
fn parse_additive<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let left = parse_multiplicative.parse_next(input)?;
    let rest: Vec<(Operator, Node)> = repeat(
        0..,
        (
            (
                soc0,
                alt((
                    literal("+").value(Operator::Add),
                    literal("-").value(Operator::Sub),
                )),
                soc0,
            )
                .map(|(_, op, _)| op),
            parse_multiplicative,
        ),
    )
    .parse_next(input)?;
    Ok(fold_binary(left, rest))
}

// Level 3: Multiplicative (*, /, %)
fn parse_multiplicative<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let left = parse_unary.parse_next(input)?;
    let rest: Vec<(Operator, Node)> = repeat(
        0..,
        (
            (
                soc0,
                alt((
                    literal("*").value(Operator::Mul),
                    literal("/").value(Operator::Div),
                    literal("%").value(Operator::Mod),
                )),
                soc0,
            )
                .map(|(_, op, _)| op),
            parse_unary,
        ),
    )
    .parse_next(input)?;
    Ok(fold_binary(left, rest))
}

// Level 2: Unary (!, -)
fn parse_unary<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();
    let checkpoint = input.checkpoint();

    fn parse_unary_op<'a>(i: &mut Span<'a>) -> ModalResult<Operator> {
        alt((
            literal("!").value(Operator::Not),
            literal("-").value(Operator::Sub),
        ))
        .parse_next(i)
    }

    if let Ok(op) = parse_unary_op.parse_next(input) {
        let node = parse_unary.parse_next(input)?;
        let end_offset = input.location();
        Ok(Node::new(
            Expr::Unary(op, node),
            create_range(input, start_offset, end_offset),
        ))
    } else {
        input.reset(&checkpoint);
        parse_atomic.parse_next(input)
    }
}

// Level 0: Atomic
fn parse_atomic<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    preceded(
        soc0,
        alt((
            parse_null,
            parse_bool,
            parse_number,
            parse_string,
            |i: &mut Span<'a>| crate::fmt_string::parse_fmt_string(i),
            parse_closure,
            parse_type_expr,
            parse_wildcard,
            parse_ref_var,
            |i: &mut Span<'a>| parse_fn_call(i, parse_expr),
            parse_variant_ctor,
            parse_var,
            parse_list,
            parse_dict,
            delimited("(", parse_expr, ")"),
        )),
    )
    .parse_next(input)
}

/// Parse a tagged-enum variant constructor expression of the shape
/// `Identifier (.Identifier)+ { ... }`. Only matches when at least two
/// dotted segments precede a literal `{` — that's enough to disambiguate
/// from member access followed by a dict literal in an unrelated position
/// (which the grammar doesn't otherwise allow as an atom).
fn parse_variant_ctor<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let checkpoint = input.checkpoint();
    let start_offset = input.location();

    let head = match crate::id::id.parse_next(input) {
        Ok(t) => t.0,
        Err(e) => {
            input.reset(&checkpoint);
            return Err(e);
        }
    };
    // Peek for the mandatory `.Identifier` continuation before allocating
    // the `path` Vec — this fast-fails the common "bare identifier" case
    // without an extra allocation.
    let after_head = input.checkpoint();
    if winnow::token::literal::<_, _, winnow::error::ContextError>(".")
        .parse_next(input)
        .is_err()
    {
        input.reset(&checkpoint);
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::default(),
        ));
    }
    input.reset(&after_head);
    let mut path = vec![head];
    loop {
        let seg_checkpoint = input.checkpoint();
        if winnow::token::literal::<_, _, winnow::error::ContextError>(".")
            .parse_next(input)
            .is_err()
        {
            input.reset(&seg_checkpoint);
            break;
        }
        match crate::id::id.parse_next(input) {
            Ok(t) => path.push(t.0),
            Err(_) => {
                input.reset(&checkpoint);
                return Err(winnow::error::ErrMode::Backtrack(
                    winnow::error::ContextError::default(),
                ));
            }
        }
    }
    if path.len() < 2 {
        input.reset(&checkpoint);
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::default(),
        ));
    }
    // Require an opening brace right here (whitespace permitted) — that's
    // what tells us "constructor", not "field access".
    let _ = soc0.parse_next(input)?;
    if winnow::token::literal::<_, _, winnow::error::ContextError>("{")
        .parse_next(input)
        .is_err()
    {
        input.reset(&checkpoint);
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::default(),
        ));
    }
    // We already consumed `{`; reconstruct the dict by parsing the inner
    // entries ourselves, mirroring what `parse_dict` does after its own `{`.
    let body_start = input.location() - 1;
    let entries: Vec<crate::structure::dict::DictEntry> = winnow::combinator::separated(
        0..,
        crate::structure::dict::parse_dict_entry,
        (soc0, ",", soc0),
    )
    .parse_next(input)?;
    let _ = (soc0, opt(","), soc0, "}").parse_next(input)?;
    let body_end = input.location();
    let mut pairs: Vec<(crate::TokenKey, Node)> = Vec::new();
    let mut body_dirs: Vec<crate::Directive> = Vec::new();
    for entry in entries {
        match entry {
            crate::structure::dict::DictEntry::Pair(k, v) => pairs.push((k, v)),
            crate::structure::dict::DictEntry::Directives(d) => body_dirs.extend(d),
        }
    }
    let mut body = Node::new(Expr::Dict(pairs), create_range(input, body_start, body_end));
    body.directives = body_dirs;

    let variant = path.pop().unwrap();
    let end_offset = input.location();
    Ok(Node::new(
        Expr::VariantCtor {
            enum_path: path,
            variant,
            body,
        },
        create_range(input, start_offset, end_offset),
    ))
}

fn parse_wildcard<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();
    let _ = "*".parse_next(input)?;
    let end_offset = input.location();
    Ok(Node::new(
        Expr::Wildcard,
        create_range(input, start_offset, end_offset),
    ))
}

fn parse_type_expr<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();
    let checkpoint = input.checkpoint();

    let t = parse_type_node.parse_next(input)?;

    // If it's followed by '(', it's likely a function call (e.g., lib.shout()), not a type.
    if winnow::token::literal::<_, _, winnow::error::ContextError>("(")
        .parse_next(input)
        .is_ok()
    {
        input.reset(&checkpoint);
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::default(),
        ));
    }

    if !t.generics.is_empty()
        || t.is_optional
        || (t.path.len() == 1
            && matches!(
                t.path[0].as_str(),
                "Int" | "String" | "Bool" | "Any" | "Null" | "List" | "Dict" | "Enum"
            ))
    {
        let end_offset = input.location();
        Ok(Node::new(
            Expr::Type(t),
            create_range(input, start_offset, end_offset),
        ))
    } else {
        input.reset(&checkpoint);
        Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::default(),
        ))
    }
}

/// Parser for one alternative inside `Enum<...>`. Tries the variant form
/// (named single-segment identifier optionally followed by a `{ field: Type, ... }`
/// body) before falling back to `parse_type_node`. The variant form sets
/// `variant_fields = Some(...)` so the analyzer can detect tagged-enum shapes
/// downstream.
pub fn parse_enum_alternative<'a>(input: &mut Span<'a>) -> ModalResult<crate::TypeNode> {
    let pre_doc_checkpoint = input.checkpoint();
    let doc_comment = crate::parse_leading_comments(input)?;
    let _checkpoint = input.checkpoint();
    let start_offset = input.location();

    // Variant form requires a single identifier (no dots, no `<...>` generics,
    // no `?`) followed by either `{ ... }` or a separator (`,` / `>`).
    let id_start = input.location();
    let Ok(name) = crate::id::id.parse_next(input).map(|t| t.0) else {
        input.reset(&pre_doc_checkpoint);
        return parse_type_node.parse_next(input);
    };
    let id_end = input.location();

    // Look ahead: does a `{` come next (after whitespace)?
    let _ = soc0.parse_next(input)?;
    let peek = input.as_ref().chars().next();
    if peek == Some('{') {
        let _ = "{".parse_next(input)?;
        let fields_result: ModalResult<Vec<(String, crate::TypeNode)>> =
            winnow::combinator::separated(0.., parse_variant_field, (soc0, ",", soc0))
                .parse_next(input);
        match fields_result {
            Ok(fields) => {
                let _ = (soc0, opt(","), soc0, "}").parse_next(input)?;
                let end_offset = input.location();
                return Ok(crate::TypeNode {
                    path: vec![name],
                    generics: Vec::new(),
                    is_optional: false,
                    range: create_range(input, start_offset, end_offset),
                    variant_fields: Some(fields),
                    doc_comment,
                });
            }
            Err(_) => {
                input.reset(&pre_doc_checkpoint);
                return parse_type_node.parse_next(input);
            }
        }
    }

    // Bare identifier — only a unit variant if the next non-space char is
    // `,` (more arms follow) or `>` (end of Enum<>). Otherwise fall back so
    // path-shaped types like `Foo.Bar` and `Some<T>` still parse.
    if peek == Some(',') || peek == Some('>') {
        return Ok(crate::TypeNode {
            path: vec![name],
            generics: Vec::new(),
            is_optional: false,
            range: create_range(input, id_start, id_end),
            variant_fields: Some(Vec::new()),
            doc_comment,
        });
    }

    input.reset(&pre_doc_checkpoint);
    parse_type_node.parse_next(input)
}

fn parse_variant_field<'a>(input: &mut Span<'a>) -> ModalResult<(String, crate::TypeNode)> {
    let _ = soc0.parse_next(input)?;
    let name = crate::id::id.parse_next(input)?.0;
    let _ = (soc0, ":", soc0).parse_next(input)?;
    let ty = parse_type_node.parse_next(input)?;
    Ok((name, ty))
}

pub fn parse_type_node<'a>(input: &mut Span<'a>) -> ModalResult<crate::TypeNode> {
    let doc_comment = crate::parse_leading_comments(input)?;
    let start_offset = input.location();

    let first_part = alt((
        crate::id::id.map(|i| i.0),
        crate::prim::string::parse_string.map(|node| {
            if let Expr::String(s) = *node.expr {
                s
            } else {
                unreachable!()
            }
        }),
    ))
    .parse_next(input)?;

    let mut path = vec![first_part];

    let rest: Vec<String> = repeat(
        0..,
        preceded(
            ".",
            alt((
                crate::id::id.map(|i| i.0),
                crate::prim::string::parse_string.map(|node| {
                    if let Expr::String(s) = *node.expr {
                        s
                    } else {
                        unreachable!()
                    }
                }),
            )),
        ),
    )
    .parse_next(input)?;
    path.extend(rest);

    let generics_checkpoint = input.checkpoint();
    // Enum<...> alternatives may carry variant-struct bodies (`Email { ... }`)
    // — switch parsers when the head identifier is `Enum`. Everything else
    // sticks with `parse_type_node` so generic params elsewhere stay strict.
    let is_enum_head = path.len() == 1 && path[0] == "Enum";
    let mut generics = if opt(preceded(soc0, "<")).parse_next(input)?.is_some() {
        let params_result: ModalResult<Vec<crate::TypeNode>> = if is_enum_head {
            winnow::combinator::separated(1.., parse_enum_alternative, (soc0, ",", soc0))
                .parse_next(input)
        } else {
            winnow::combinator::separated(1.., parse_type_node, (soc0, ",", soc0)).parse_next(input)
        };
        match params_result {
            Ok(params) => {
                // Allow trailing comma inside `Enum<..., Variant,>`.
                if is_enum_head {
                    let _ = (soc0, opt(","), soc0).parse_next(input)?;
                } else {
                    let _ = soc0.parse_next(input)?;
                }
                if winnow::token::literal::<_, _, winnow::error::ContextError>(">")
                    .parse_next(input)
                    .is_ok()
                {
                    params
                } else {
                    input.reset(&generics_checkpoint);
                    Vec::new()
                }
            }
            Err(_) => {
                input.reset(&generics_checkpoint);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    // Disambiguate `Enum<Int, String>` (untagged) from `Enum<Push>`: a
    // sum-type Enum requires at least one alternative with a `{ ... }`
    // body. If no alternative carries struct-shape fields, clear the
    // tentative unit-variant markers so the rest of the pipeline treats
    // this as the classic untagged form.
    if is_enum_head {
        let any_struct_form = generics
            .iter()
            .any(|g| g.variant_fields.as_ref().is_some_and(|f| !f.is_empty()));
        if !any_struct_form {
            for g in &mut generics {
                g.variant_fields = None;
            }
        }
    }

    let is_optional = opt("?").parse_next(input)?.is_some();

    let end_offset = input.location();
    Ok(crate::TypeNode {
        path,
        generics,
        is_optional,
        range: create_range(input, start_offset, end_offset),
        variant_fields: None,
        doc_comment,
    })
}

pub fn parse_closure_param<'a>(input: &mut Span<'a>) -> ModalResult<crate::ClosureParam> {
    let start_offset = input.location();
    let checkpoint = input.checkpoint();

    let (type_hint, name) = if let Ok(t) = parse_type_node.parse_next(input) {
        if soc0.parse_next(input).is_ok() {
            if let Ok(id) = crate::id::id.parse_next(input) {
                (Some(t), id.0)
            } else {
                input.reset(&checkpoint);
                let id = crate::id::id.parse_next(input)?;
                (None, id.0)
            }
        } else {
            input.reset(&checkpoint);
            let id = crate::id::id.parse_next(input)?;
            (None, id.0)
        }
    } else {
        let id = crate::id::id.parse_next(input)?;
        (None, id.0)
    };

    let end_offset = input.location();
    Ok(crate::ClosureParam {
        name,
        type_hint,
        range: create_range(input, start_offset, end_offset),
    })
}

pub fn parse_closure<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();
    let checkpoint = input.checkpoint();

    // ( [ClosureParam, ...] ) [-> TypeNode] => Expr
    if winnow::token::literal::<_, _, winnow::error::ContextError>("(")
        .parse_next(input)
        .is_err()
    {
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::default(),
        ));
    }

    let params_result: ModalResult<Vec<crate::ClosureParam>> =
        winnow::combinator::separated(0.., parse_closure_param, (soc0, ",", soc0))
            .parse_next(input);
    let params = match params_result {
        Ok(p) => p,
        Err(_) => {
            input.reset(&checkpoint);
            return Err(winnow::error::ErrMode::Backtrack(
                winnow::error::ContextError::default(),
            ));
        }
    };

    if (soc0, ")").parse_next(input).is_err() {
        input.reset(&checkpoint);
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::default(),
        ));
    }

    let rt_checkpoint = input.checkpoint();
    let return_type = if (soc0, "->", soc0).parse_next(input).is_ok() {
        if let Ok(t) = parse_type_node.parse_next(input) {
            Some(t)
        } else {
            input.reset(&rt_checkpoint);
            None
        }
    } else {
        input.reset(&rt_checkpoint);
        None
    };

    if (soc0, "=>", soc0).parse_next(input).is_err() {
        input.reset(&checkpoint);
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::default(),
        ));
    }

    if let Ok(body) = parse_expr.parse_next(input) {
        let end_offset = input.location();
        Ok(Node::new(
            Expr::Closure {
                params,
                return_type,
                body,
            },
            create_range(input, start_offset, end_offset),
        ))
    } else {
        input.reset(&checkpoint);
        Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::default(),
        ))
    }
}

fn fold_binary(mut left: Node, rest: Vec<(Operator, Node)>) -> Node {
    for (op, right) in rest {
        let range = combine_ranges(left.range, right.range);
        left = Node::new(Expr::Binary(op, left, right), range);
    }
    left
}

pub fn parse_expr_zone<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    delimited(("${", soc0), parse_expr, (soc0, "}")).parse_next(input)
}

/// Yield the expression-shaped child nodes of `node` for AST walkers
/// (analyzer passes, LSP enclosing-scope lookups, ...). Decorators,
/// directives, and type hints are intentionally *not* included — those
/// have their own dedicated walkers that need different semantics.
pub fn child_nodes(node: &Node) -> Vec<&Node> {
    let mut out = Vec::new();
    match &*node.expr {
        Expr::Dict(pairs) => {
            for (_, value) in pairs {
                out.push(value);
            }
        }
        Expr::List(items) => out.extend(items.iter()),
        Expr::Spread(inner) => out.push(inner),
        Expr::Comprehension {
            element,
            iterable,
            condition,
            ..
        } => {
            out.push(element);
            out.push(iterable);
            if let Some(cond) = condition {
                out.push(cond);
            }
        }
        Expr::Binary(_, l, r) => {
            out.push(l);
            out.push(r);
        }
        Expr::Unary(_, inner) => out.push(inner),
        Expr::Ternary { cond, then, els } => {
            out.push(cond);
            out.push(then);
            out.push(els);
        }
        Expr::FnCall { args, .. } => {
            for arg in args {
                out.push(&arg.value);
            }
        }
        Expr::FString(parts) => {
            for part in parts {
                if let crate::FStringPart::Interpolation(n) = part {
                    out.push(n);
                }
            }
        }
        Expr::Where { expr, bindings } => {
            out.push(expr);
            out.push(bindings);
        }
        Expr::Match { expr, arms } => {
            out.push(expr);
            for (pat, body) in arms {
                out.push(pat);
                out.push(body);
            }
        }
        Expr::Closure { body, .. } => out.push(body),
        Expr::VariantCtor { body, .. } => out.push(body),
        Expr::Reference { .. }
        | Expr::Variable(_)
        | Expr::Type(_)
        | Expr::Wildcard
        | Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::String(_) => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_group() {
        let mut s = Span::new("(1 + 2)");
        let node = parse_expr(&mut s).unwrap();
        match *node.expr {
            Expr::Binary(Operator::Add, _, _) => {}
            _ => panic!("Expected binary add"),
        }
    }

    #[test]
    fn test_atomic() {
        let mut s = Span::new("null");
        assert!(matches!(*parse_atomic(&mut s).unwrap().expr, Expr::Null));

        let mut s = Span::new("true");
        assert!(matches!(
            *parse_atomic(&mut s).unwrap().expr,
            Expr::Bool(true)
        ));

        let mut s = Span::new("123");
        assert!(matches!(
            *parse_atomic(&mut s).unwrap().expr,
            Expr::Int(123)
        ));

        let mut s = Span::new("\"hello\"");
        assert!(matches!(
            *parse_atomic(&mut s).unwrap().expr,
            Expr::String(_)
        ));
    }

    #[test]
    fn test_precedence() {
        let mut s = Span::new("1 + 2 * 3");
        let node = parse_expr(&mut s).unwrap();
        // Should be 1 + (2 * 3)
        if let Expr::Binary(Operator::Add, left, right) = *node.expr {
            assert!(matches!(*left.expr, Expr::Int(1)));
            assert!(matches!(*right.expr, Expr::Binary(Operator::Mul, _, _)));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_parse_expr_zone() {
        let mut s = Span::new("${ 1 + 2 }");
        let node = parse_expr_zone(&mut s).unwrap();
        assert!(matches!(*node.expr, Expr::Binary(Operator::Add, _, _)));
    }

    #[test]
    fn test_expr_ternary() {
        let mut s = Span::new("true ? 1 : 2");
        let node = parse_expr(&mut s).unwrap();
        if let Expr::Ternary { ref cond, .. } = *node.expr {
            assert!(matches!(*cond.expr, Expr::Bool(true)));
        } else {
            panic!()
        }

        let mut s = Span::new("true? 1:2");
        assert!(parse_expr(&mut s).is_ok());
    }

    #[test]
    fn test_unary() {
        let mut s = Span::new("!true");
        let node = parse_expr(&mut s).unwrap();
        assert!(matches!(*node.expr, Expr::Unary(Operator::Not, _)));

        let mut s = Span::new("-1");
        let node = parse_expr(&mut s).unwrap();
        assert!(matches!(*node.expr, Expr::Unary(Operator::Sub, _)));
    }

    #[test]
    fn test_complex_expr() {
        let mut s = Span::new("1 + f(2, 3) * var3");
        assert!(parse_expr(&mut s).is_ok());
    }

    #[test]
    fn test_expr_zone_with_comments() {
        let mut s = Span::new("${ /* comment */ 1 // line comment\n }");
        let node = parse_expr_zone(&mut s).unwrap();
        assert!(matches!(*node.expr, Expr::Int(1)));
    }

    #[test]
    fn test_parse_enum_variant_struct_form() {
        // `Enum<Email { address: String }>` — single struct-shape variant.
        let mut s = Span::new("Enum<Email { address: String, subject: String }>");
        let t = parse_type_node(&mut s).unwrap();
        assert_eq!(t.path, vec!["Enum".to_string()]);
        assert_eq!(t.generics.len(), 1);
        let alt = &t.generics[0];
        assert_eq!(alt.path, vec!["Email".to_string()]);
        let fields = alt.variant_fields.as_ref().expect("variant_fields set");
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].0, "address");
        assert_eq!(fields[1].0, "subject");
    }

    #[test]
    fn test_parse_enum_variant_unit_form() {
        // Mix struct + unit variants.
        let mut s = Span::new("Enum<Email { address: String }, Push>");
        let t = parse_type_node(&mut s).unwrap();
        assert_eq!(t.generics.len(), 2);
        assert_eq!(t.generics[1].path, vec!["Push".to_string()]);
        // Unit variant carries an empty variant_fields, NOT None.
        assert_eq!(
            t.generics[1].variant_fields.as_ref().map(|v| v.len()),
            Some(0)
        );
    }

    #[test]
    fn test_parse_enum_untagged_form_unchanged() {
        // The classic `Enum<"a", "b">` and `Enum<Int, String>` must keep
        // working with `variant_fields = None` on every alternative.
        let mut s = Span::new(r#"Enum<"a", "b">"#);
        let t = parse_type_node(&mut s).unwrap();
        assert_eq!(t.generics.len(), 2);
        assert!(t.generics.iter().all(|g| g.variant_fields.is_none()));

        let mut s = Span::new("Enum<Int, String>");
        let t = parse_type_node(&mut s).unwrap();
        assert_eq!(t.generics.len(), 2);
        assert!(t.generics.iter().all(|g| g.variant_fields.is_none()));
    }

    #[test]
    fn test_parse_variant_ctor_expr() {
        // `Notification.Email { address: "x" }` parses as a VariantCtor.
        let mut s = Span::new(r#"Notification.Email { address: "x" }"#);
        let node = parse_expr(&mut s).unwrap();
        match &*node.expr {
            Expr::VariantCtor {
                enum_path, variant, ..
            } => {
                assert_eq!(enum_path, &vec!["Notification".to_string()]);
                assert_eq!(variant, "Email");
            }
            other => panic!("expected VariantCtor, got {other:?}"),
        }
    }
}
