//! Typed wrappers over the rowan CST — the `relon-fmt` support layer.
//!
//! Positioning (final architecture): the parser has exactly two
//! output layers. The lossless rowan CST ([`crate::cst`] /
//! [`crate::syntax`]) serves the formatter, LSP features, and
//! span-accurate diagnostics; the [`crate::Node`] / [`crate::Expr`]
//! tree is the official AST that the analyzer, evaluator, and IR
//! lowering consume. This module is *not* a third semantic layer: it
//! is a small typed-accessor surface over the CST, consumed by
//! `relon-fmt` (which formats off the lossless tree) and by the
//! CST → `Node` lowering in `crate::lower`. It grows accessors only
//! when those two callers need them. New semantic consumers should
//! parse via [`crate::parse_document`] and walk [`crate::Node`].
//!
//! Each wrapper is a transparent tuple struct around a `SyntaxNode`
//! (or `SyntaxToken`) — there is no extra allocation, and a wrapper
//! can be obtained from a CST node in O(1) via `cast(node)` (which
//! returns `None` when the kind doesn't match).
//!
//! ## Variant kinds
//!
//! `Expr` is the central typed enum — one variant per [`crate::Expr`]
//! variant plus a new [`Expr::Error`] for spans the CST couldn't fit
//! into any production. Each variant carries a structured wrapper
//! that exposes the relevant children:
//!
//! * `Expr::Dict(Dict)` — has `.fields()` iterator.
//! * `Expr::List(List)` / `Expr::Comprehension(Comprehension)`.
//! * `Expr::Binary(BinaryExpr)` — `.op_kind()` + `.lhs()` + `.rhs()`.
//! * `Expr::Call(CallExpr)` — `.callee()` + `.args()`.
//! * `Expr::Closure(Closure)` — `.params()` + `.body()`.
//! * ...etc, see the impls below.
//!
//! When the underlying `SyntaxKind` is `ERROR` (or any unrecognised
//! node), `Expr::cast` returns `Expr::Error(ErrorNode)`. Downstream
//! callers must add a no-op arm for this in match statements.

use crate::syntax::{SyntaxKind, SyntaxNode};

// =====================================================================
// Macro: define a typed wrapper around a SyntaxNode of one specific
// kind. Generates the standard `cast` / `syntax` / `text` boilerplate.
// =====================================================================

macro_rules! ast_node {
    ($(#[$meta:meta])* $name:ident, $kind:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(SyntaxNode);

        impl $name {
            /// Wrap `node` if its `SyntaxKind` matches; otherwise
            /// return `None`. O(1) — just a kind check.
            pub fn cast(node: SyntaxNode) -> Option<Self> {
                if node.kind() == SyntaxKind::$kind {
                    Some(Self(node))
                } else {
                    None
                }
            }

            /// Borrow the underlying [`SyntaxNode`]. Useful for
            /// downstream traversals that want CST-level access.
            pub fn syntax(&self) -> &SyntaxNode {
                &self.0
            }

            /// Verbatim source text spanned by this node, including
            /// trivia. Cheap on rowan (it walks the green tree).
            pub fn text(&self) -> String {
                self.0.text().to_string()
            }
        }
    };
}

ast_node!(
    /// `DOCUMENT` root — every parse produces exactly one. Carries
    /// the leading directives + the root expression body.
    Document, DOCUMENT
);

ast_node!(
    /// `#name <body>` form. Body shape depends on the directive's
    /// name; the typed-AST layer reads it from [`Directive::name`].
    Directive, DIRECTIVE
);

ast_node!(
    /// `@name(args?)` decorator.
    Decorator, DECORATOR
);

ast_node!(Dict, DICT);
ast_node!(DictField, DICT_FIELD);
ast_node!(List, LIST);
ast_node!(
    /// `(e1, e2, ...)` tuple value literal. `()` is the unit tuple
    /// (no children); `(e,)` the trailing-comma 1-tuple. A bare
    /// grouping `(e)` is NOT a tuple and never produces this node.
    Tuple,
    TUPLE
);
ast_node!(Comprehension, COMPREHENSION);
ast_node!(Closure, CLOSURE);
ast_node!(ClosureParam, CLOSURE_PARAM);
ast_node!(CallExpr, CALL_EXPR);
ast_node!(CallArg, CALL_ARG);
ast_node!(BinaryExpr, BINARY_EXPR);
ast_node!(UnaryExpr, UNARY_EXPR);
ast_node!(TernaryExpr, TERNARY_EXPR);
ast_node!(ReferenceExpr, REFERENCE_EXPR);
ast_node!(VariableExpr, VARIABLE_EXPR);
ast_node!(WhereExpr, WHERE_EXPR);
ast_node!(MatchExpr, MATCH_EXPR);
ast_node!(MatchArm, MATCH_ARM);
ast_node!(VariantCtor, VARIANT_CTOR);
ast_node!(FString, F_STRING);
ast_node!(FStringInterpolation, F_STRING_INTERPOLATION);
ast_node!(SpreadExpr, SPREAD_EXPR);
ast_node!(TypeNode, TYPE_NODE);
ast_node!(
    /// `(T1, T2, ...)` tuple type. Sits inside a TypeNode-shaped
    /// position (typed dict field, generic argument list, closure
    /// parameter, schema-method param).
    TupleType,
    TUPLE_TYPE
);
ast_node!(
    /// `with { ... }` body of a `#schema` / `#extend` directive.
    /// Children: pragma directives + zero or more [`SchemaMethod`].
    SchemaWith,
    SCHEMA_WITH
);
ast_node!(
    /// One method declaration inside a [`SchemaWith`] block.
    /// Children: leading pragma directives, the method name token,
    /// optional generic params, a closure-param list, return type,
    /// and an optional body expression.
    SchemaMethod,
    SCHEMA_METHOD
);
ast_node!(Wildcard, WILDCARD);
ast_node!(Literal, LITERAL);
ast_node!(ErrorNode, ERROR);

// =====================================================================
// `Expr` — the top-level typed enum. Mirrors `crate::Expr` plus an
// `Error` variant for partial parses.
// =====================================================================

/// Typed view over any expression-shaped CST node. Returned by
/// [`Expr::cast`].
///
/// Note the variant naming follows the CST kinds, not the
/// [`crate::Expr`] AST enum — `Literal` covers `true` / `false` /
/// numeric / string atoms uniformly, plus the removed `null` spelling
/// for diagnostics, where the AST enum splits them into `Bool` /
/// `Int` / `Float` / `String` plus internal `Missing`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Expr {
    Literal(Literal),
    Variable(VariableExpr),
    Reference(ReferenceExpr),
    Dict(Dict),
    List(List),
    Tuple(Tuple),
    Spread(SpreadExpr),
    Comprehension(Comprehension),
    Binary(BinaryExpr),
    Unary(UnaryExpr),
    Ternary(TernaryExpr),
    Call(CallExpr),
    FString(FString),
    Type(TypeNode),
    Wildcard(Wildcard),
    Where(WhereExpr),
    Match(MatchExpr),
    Closure(Closure),
    VariantCtor(VariantCtor),
    /// New variant introduced by the CST rewrite — spans bytes the
    /// parser couldn't fit into any production. Downstream callers
    /// must handle it (typically by skipping or surfacing a
    /// diagnostic).
    Error(ErrorNode),
}

impl Expr {
    /// Wrap `node` if it names an expression-shaped CST kind.
    /// Returns `None` for non-expression nodes (DICT_FIELD,
    /// CLOSURE_PARAM, etc.) — those have their own typed wrappers.
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        Some(match node.kind() {
            SyntaxKind::LITERAL => Self::Literal(Literal(node)),
            SyntaxKind::VARIABLE_EXPR => Self::Variable(VariableExpr(node)),
            SyntaxKind::REFERENCE_EXPR => Self::Reference(ReferenceExpr(node)),
            SyntaxKind::DICT => Self::Dict(Dict(node)),
            SyntaxKind::LIST => Self::List(List(node)),
            SyntaxKind::TUPLE => Self::Tuple(Tuple(node)),
            SyntaxKind::SPREAD_EXPR => Self::Spread(SpreadExpr(node)),
            SyntaxKind::COMPREHENSION => Self::Comprehension(Comprehension(node)),
            SyntaxKind::BINARY_EXPR => Self::Binary(BinaryExpr(node)),
            SyntaxKind::UNARY_EXPR => Self::Unary(UnaryExpr(node)),
            SyntaxKind::TERNARY_EXPR => Self::Ternary(TernaryExpr(node)),
            SyntaxKind::CALL_EXPR => Self::Call(CallExpr(node)),
            SyntaxKind::F_STRING => Self::FString(FString(node)),
            SyntaxKind::TYPE_NODE => Self::Type(TypeNode(node)),
            SyntaxKind::WILDCARD => Self::Wildcard(Wildcard(node)),
            SyntaxKind::WHERE_EXPR => Self::Where(WhereExpr(node)),
            SyntaxKind::MATCH_EXPR => Self::Match(MatchExpr(node)),
            SyntaxKind::CLOSURE => Self::Closure(Closure(node)),
            SyntaxKind::VARIANT_CTOR => Self::VariantCtor(VariantCtor(node)),
            SyntaxKind::ERROR => Self::Error(ErrorNode(node)),
            _ => return None,
        })
    }

    /// Borrow the underlying [`SyntaxNode`] regardless of variant.
    pub fn syntax(&self) -> &SyntaxNode {
        match self {
            Self::Literal(n) => n.syntax(),
            Self::Variable(n) => n.syntax(),
            Self::Reference(n) => n.syntax(),
            Self::Dict(n) => n.syntax(),
            Self::List(n) => n.syntax(),
            Self::Tuple(n) => n.syntax(),
            Self::Spread(n) => n.syntax(),
            Self::Comprehension(n) => n.syntax(),
            Self::Binary(n) => n.syntax(),
            Self::Unary(n) => n.syntax(),
            Self::Ternary(n) => n.syntax(),
            Self::Call(n) => n.syntax(),
            Self::FString(n) => n.syntax(),
            Self::Type(n) => n.syntax(),
            Self::Wildcard(n) => n.syntax(),
            Self::Where(n) => n.syntax(),
            Self::Match(n) => n.syntax(),
            Self::Closure(n) => n.syntax(),
            Self::VariantCtor(n) => n.syntax(),
            Self::Error(n) => n.syntax(),
        }
    }

    /// Verbatim source text. Convenience over `self.syntax().text()`.
    pub fn text(&self) -> String {
        self.syntax().text().to_string()
    }
}

// =====================================================================
// Per-node accessors. Each wrapper exposes the structural data its
// callers (relon-fmt and the CST → `Node` lowering) actually consume.
// =====================================================================

impl Document {
    /// All directives stacked above the root value, in source order.
    pub fn directives(&self) -> impl Iterator<Item = Directive> + '_ {
        self.0.children().filter_map(Directive::cast)
    }

    /// All decorators stacked above the root value, in source order.
    pub fn decorators(&self) -> impl Iterator<Item = Decorator> + '_ {
        self.0.children().filter_map(Decorator::cast)
    }

    /// The root expression, if the file has one. Files containing
    /// only directives (e.g. a `#schema` library) have `None`.
    pub fn root_expr(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
}

impl Directive {
    /// Directive name (everything after the `#`). `None` when the
    /// parser emitted an ERROR before the name was captured.
    pub fn name(&self) -> Option<String> {
        // The first IDENT token under the DIRECTIVE node is the name
        // (the `#` itself is the leading leaf).
        self.0
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string())
    }

    /// Direct-child expression(s) of the directive body. For
    /// `#schema X { ... }` this yields the body dict; for
    /// `#default 0` it yields the value expression. The number of
    /// items is shape-dependent and the typed-AST layer above this
    /// crate decides interpretation.
    pub fn body_exprs(&self) -> impl Iterator<Item = Expr> + '_ {
        self.0.children().filter_map(Expr::cast)
    }
}

impl Decorator {
    pub fn name(&self) -> Option<String> {
        self.0
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string())
    }

    pub fn args(&self) -> impl Iterator<Item = Expr> + '_ {
        // CALL_ARG is the child node holding the parens; the args
        // inside it are the actual expressions.
        self.0
            .children()
            .find(|c| c.kind() == SyntaxKind::CALL_ARG)
            .into_iter()
            .flat_map(|n| n.children().filter_map(Expr::cast).collect::<Vec<_>>())
    }
}

impl Dict {
    pub fn fields(&self) -> impl Iterator<Item = DictField> + '_ {
        self.0.children().filter_map(DictField::cast)
    }
}

impl DictField {
    /// Key text — the bare identifier or string-literal key.
    /// Returns `None` for spread / dynamic-keyed fields (callers
    /// should inspect the children directly for those shapes).
    pub fn key_text(&self) -> Option<String> {
        self.0
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|t| t.kind() == SyntaxKind::IDENT || t.kind() == SyntaxKind::STRING)
            .map(|t| t.text().to_string())
    }

    /// The value expression. For method-shorthand closure fields
    /// (`key(params): body`) this is the closure; otherwise it's
    /// whatever follows the `:`.
    pub fn value(&self) -> Option<Expr> {
        self.0.children().filter_map(Expr::cast).next()
    }
}

impl List {
    pub fn items(&self) -> impl Iterator<Item = Expr> + '_ {
        self.0.children().filter_map(Expr::cast)
    }
}

impl Tuple {
    /// Element expressions in source order. Empty for the unit tuple `()`.
    pub fn items(&self) -> impl Iterator<Item = Expr> + '_ {
        self.0.children().filter_map(Expr::cast)
    }
}

impl Comprehension {
    /// `[ element for id in iterable (if cond)? ]`. Returns the
    /// inner expressions in their structural roles. Falls back on
    /// CST-order when a malformed comprehension drops one of them.
    pub fn parts(&self) -> Vec<Expr> {
        self.0.children().filter_map(Expr::cast).collect()
    }

    /// The bound identifier between `for` and `in`. `None` on a
    /// malformed comprehension.
    pub fn binding(&self) -> Option<String> {
        let mut after_for = false;
        for el in self.0.children_with_tokens() {
            if let Some(t) = el.as_token() {
                if t.kind() == SyntaxKind::IDENT {
                    let s = t.text();
                    if after_for {
                        return Some(s.to_string());
                    }
                    if s == "for" {
                        after_for = true;
                    }
                }
            }
        }
        None
    }
}

impl Closure {
    pub fn params(&self) -> impl Iterator<Item = ClosureParam> + '_ {
        self.0.children().filter_map(ClosureParam::cast)
    }

    /// Optional return type — `-> Type`. The TYPE_NODE child that
    /// follows the `->` arrow.
    pub fn return_type(&self) -> Option<TypeNode> {
        let mut saw_arrow = false;
        for el in self.0.children_with_tokens() {
            if let Some(t) = el.as_token() {
                if t.kind() == SyntaxKind::THIN_ARROW {
                    saw_arrow = true;
                }
            } else if let Some(n) = el.as_node() {
                if saw_arrow && n.kind() == SyntaxKind::TYPE_NODE {
                    return TypeNode::cast(n.clone());
                }
            }
        }
        None
    }

    /// The body expression — everything after `=>` (or after `:`
    /// for the dict-field method shorthand).
    pub fn body(&self) -> Option<Expr> {
        // The body is the LAST expression child — both the typed
        // params (which contain their own TYPE_NODEs) and the
        // return type sit before it in source order. Filter out
        // the return TYPE_NODE if it exists.
        let mut last: Option<Expr> = None;
        for child in self.0.children() {
            if child.kind() == SyntaxKind::CLOSURE_PARAM || child.kind() == SyntaxKind::TYPE_NODE {
                continue;
            }
            if let Some(e) = Expr::cast(child) {
                last = Some(e);
            }
        }
        last
    }
}

impl ClosureParam {
    pub fn name(&self) -> Option<String> {
        // The non-type IDENT is the parameter name. Skip any
        // TYPE_NODE child and pick the trailing IDENT token.
        self.0
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|t| t.kind() == SyntaxKind::IDENT)
            .last()
            .map(|t| t.text().to_string())
    }

    pub fn type_hint(&self) -> Option<TypeNode> {
        self.0.children().find_map(TypeNode::cast)
    }
}

impl CallExpr {
    /// The callee expression (the thing being called). It's the
    /// first expression child — typically a VARIABLE_EXPR but in
    /// principle any postfix-able expression.
    pub fn callee(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }

    /// Arguments inside the parens.
    pub fn args(&self) -> impl Iterator<Item = Expr> + '_ {
        self.0
            .children()
            .find(|c| c.kind() == SyntaxKind::CALL_ARG)
            .into_iter()
            .flat_map(|n| n.children().filter_map(Expr::cast).collect::<Vec<_>>())
    }
}

impl BinaryExpr {
    /// Return the operator token's `SyntaxKind` (e.g.
    /// `SyntaxKind::PLUS`, `SyntaxKind::EQ_EQ`). `None` only on a
    /// malformed parse where the operator token is missing.
    pub fn op_kind(&self) -> Option<SyntaxKind> {
        self.0
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .map(|t| t.kind())
            .find(|k| {
                matches!(
                    k,
                    SyntaxKind::PLUS
                        | SyntaxKind::MINUS
                        | SyntaxKind::STAR
                        | SyntaxKind::SLASH
                        | SyntaxKind::PERCENT
                        | SyntaxKind::PLUS_PLUS
                        | SyntaxKind::EQ_EQ
                        | SyntaxKind::BANG_EQ
                        | SyntaxKind::LT
                        | SyntaxKind::GT
                        | SyntaxKind::LT_EQ
                        | SyntaxKind::GT_EQ
                        | SyntaxKind::AMP_AMP
                        | SyntaxKind::PIPE_PIPE
                        | SyntaxKind::PIPE
                )
            })
    }

    pub fn lhs(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }

    pub fn rhs(&self) -> Option<Expr> {
        self.0.children().filter_map(Expr::cast).nth(1)
    }
}

impl UnaryExpr {
    /// Operator token kind (`MINUS` / `BANG` / `PLUS`).
    pub fn op_kind(&self) -> Option<SyntaxKind> {
        self.0
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .map(|t| t.kind())
            .find(|k| matches!(k, SyntaxKind::MINUS | SyntaxKind::BANG | SyntaxKind::PLUS))
    }

    pub fn operand(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
}

impl TernaryExpr {
    pub fn cond(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }

    pub fn then(&self) -> Option<Expr> {
        self.0.children().filter_map(Expr::cast).nth(1)
    }

    pub fn els(&self) -> Option<Expr> {
        self.0.children().filter_map(Expr::cast).nth(2)
    }
}

impl ReferenceExpr {
    /// Reference base identifier (`root`, `sibling`, `uncle`,
    /// `this`, `prev`, `next`, `index`). The CST keeps the bare
    /// IDENT token directly under the node.
    pub fn base_name(&self) -> Option<String> {
        self.0
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string())
    }

    /// Whole `&base.x.y` text. Cheap fallback when callers don't
    /// need to inspect each path segment individually.
    pub fn path_text(&self) -> String {
        self.text()
    }
}

impl VariableExpr {
    /// Every IDENT-shaped path segment in source order.
    pub fn segments(&self) -> Vec<String> {
        self.0
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string())
            .collect()
    }
}

impl Literal {
    /// Kind of the underlying literal token. Useful for the
    /// `true`/`false`/NUMBER/STRING dispatch downstream
    /// callers need to type-check.
    pub fn kind(&self) -> Option<SyntaxKind> {
        self.0
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .map(|t| t.kind())
            .find(|k| {
                matches!(
                    k,
                    SyntaxKind::NUMBER | SyntaxKind::STRING | SyntaxKind::IDENT
                )
            })
    }

    /// Verbatim text of the literal token (e.g. `"42"`, `r#""hi""#`,
    /// `"true"`).
    pub fn value_text(&self) -> String {
        self.text()
    }
}

impl WhereExpr {
    /// The leading expression (everything before `where`).
    pub fn expr(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }

    /// The binding dict that follows `where`.
    pub fn bindings(&self) -> Option<Dict> {
        self.0.children().filter_map(Dict::cast).next()
    }
}

impl MatchExpr {
    /// The scrutinee (everything before `match`).
    pub fn scrutinee(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }

    pub fn arms(&self) -> impl Iterator<Item = MatchArm> + '_ {
        self.0.children().filter_map(MatchArm::cast)
    }
}

impl MatchArm {
    /// Pattern — typically a TYPE_NODE; `*` wildcards parse as
    /// a [`Wildcard`] child.
    pub fn pattern(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }

    /// Arm body (everything after `:`).
    pub fn body(&self) -> Option<Expr> {
        self.0.children().filter_map(Expr::cast).nth(1)
    }
}

impl SpreadExpr {
    /// The inner expression being spread.
    pub fn inner(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
}

impl VariantCtor {
    /// Body dict literal `Enum.Variant { ... }`.
    pub fn body(&self) -> Option<Dict> {
        self.0.children().find_map(Dict::cast)
    }
}

impl FString {
    /// Iterator over the f-string's literal text chunks and
    /// interpolation sub-nodes, in source order.
    pub fn parts(&self) -> Vec<FStringPart> {
        let mut out = Vec::new();
        for el in self.0.children_with_tokens() {
            if let Some(t) = el.as_token() {
                if t.kind() == SyntaxKind::F_STRING_LITERAL {
                    out.push(FStringPart::Literal(t.text().to_string()));
                }
            } else if let Some(n) = el.as_node() {
                if let Some(interp) = FStringInterpolation::cast(n.clone()) {
                    out.push(FStringPart::Interpolation(interp));
                }
            }
        }
        out
    }
}

impl FStringInterpolation {
    /// The inner expression — what gets evaluated and formatted in.
    pub fn expr(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
}

/// View of one piece of an [`FString`]. Mirrors `crate::FStringPart`
/// at the rowan side.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FStringPart {
    Literal(String),
    Interpolation(FStringInterpolation),
}

impl TypeNode {
    /// Path segments — `Foo` / `Foo.Bar` / `"foo".Bar`. Returns the
    /// raw text of each IDENT / STRING token preceding the first
    /// generic / `?`.
    pub fn path_text(&self) -> Vec<String> {
        let mut out = Vec::new();
        for el in self.0.children_with_tokens() {
            if let Some(t) = el.as_token() {
                match t.kind() {
                    SyntaxKind::LT => break,
                    SyntaxKind::QUESTION => break,
                    SyntaxKind::DOT => continue,
                    SyntaxKind::IDENT | SyntaxKind::STRING => out.push(t.text().to_string()),
                    _ => {}
                }
            } else {
                break;
            }
        }
        out
    }

    /// Direct-child TYPE_NODEs nested inside this one's generic
    /// argument list.
    pub fn generics(&self) -> impl Iterator<Item = TypeNode> + '_ {
        self.0.children().filter_map(TypeNode::cast)
    }

    /// `Foo?` — true when the trailing `?` is present.
    pub fn is_optional(&self) -> bool {
        self.0
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .any(|t| t.kind() == SyntaxKind::QUESTION)
    }
}

// =====================================================================
// Convenience entry points.
// =====================================================================

/// Cast the root of a [`crate::cst::Parse`] result to a typed
/// [`Document`]. The root kind is always `DOCUMENT` so the call
/// never returns `None`; the `Option` is for API uniformity with
/// the other `cast` entries.
pub fn document_of(syntax: SyntaxNode) -> Option<Document> {
    Document::cast(syntax)
}

/// Re-export of [`crate::syntax::SyntaxToken`] for callers who need it but don't
/// otherwise depend on the `syntax` module.
pub use crate::syntax::SyntaxToken as _Token;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cst::parse_cst;

    #[test]
    fn document_round_trip() {
        let p = parse_cst("{ a: 1, b: 2 }");
        let doc = Document::cast(p.syntax()).expect("DOCUMENT kind");
        assert!(doc.root_expr().is_some());
    }

    #[test]
    fn dict_fields() {
        let p = parse_cst("{ alice: 1, bob: 2 }");
        let doc = Document::cast(p.syntax()).unwrap();
        let dict = match doc.root_expr().unwrap() {
            Expr::Dict(d) => d,
            _ => panic!(),
        };
        let keys: Vec<_> = dict.fields().filter_map(|f| f.key_text()).collect();
        assert_eq!(keys, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn binary_op_kind() {
        let p = parse_cst("{ x: 1 + 2 }");
        let doc = Document::cast(p.syntax()).unwrap();
        let dict = match doc.root_expr().unwrap() {
            Expr::Dict(d) => d,
            _ => panic!(),
        };
        let value = dict.fields().next().and_then(|f| f.value()).unwrap();
        let bin = match value {
            Expr::Binary(b) => b,
            other => panic!("not binary: {other:?}"),
        };
        assert_eq!(bin.op_kind(), Some(SyntaxKind::PLUS));
        assert!(bin.lhs().is_some());
        assert!(bin.rhs().is_some());
    }

    #[test]
    fn closure_typed_params() {
        let p = parse_cst("{ add(Int a, Int b): a + b }");
        let doc = Document::cast(p.syntax()).unwrap();
        let dict = match doc.root_expr().unwrap() {
            Expr::Dict(d) => d,
            _ => panic!(),
        };
        let cls = match dict.fields().next().and_then(|f| f.value()).unwrap() {
            Expr::Closure(c) => c,
            other => panic!("not closure: {other:?}"),
        };
        let params: Vec<_> = cls.params().collect();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name().as_deref(), Some("a"));
        assert!(params[0].type_hint().is_some());
    }

    #[test]
    fn f_string_parts() {
        let p = parse_cst(r#"{ msg: f"hi ${name}!" }"#);
        let doc = Document::cast(p.syntax()).unwrap();
        let dict = match doc.root_expr().unwrap() {
            Expr::Dict(d) => d,
            _ => panic!(),
        };
        let fs = match dict.fields().next().and_then(|f| f.value()).unwrap() {
            Expr::FString(f) => f,
            _ => panic!(),
        };
        let parts = fs.parts();
        let mut has_lit = false;
        let mut has_interp = false;
        for p in &parts {
            match p {
                FStringPart::Literal(_) => has_lit = true,
                FStringPart::Interpolation(_) => has_interp = true,
            }
        }
        assert!(has_lit && has_interp);
    }

    #[test]
    fn directive_name() {
        let p = parse_cst("#schema X { Int a: * }\n{ x: 1 }");
        let doc = Document::cast(p.syntax()).unwrap();
        let dirs: Vec<_> = doc.directives().collect();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].name().as_deref(), Some("schema"));
    }

    #[test]
    fn match_arms() {
        let p = parse_cst("{ f(x): x match { Int: 1, _ : 0 } }");
        let doc = Document::cast(p.syntax()).unwrap();
        let dict = match doc.root_expr().unwrap() {
            Expr::Dict(d) => d,
            _ => panic!(),
        };
        let cls = match dict.fields().next().and_then(|f| f.value()).unwrap() {
            Expr::Closure(c) => c,
            _ => panic!(),
        };
        let body = cls.body().unwrap();
        let m = match body {
            Expr::Match(m) => m,
            _ => panic!(),
        };
        assert_eq!(m.arms().count(), 2);
    }

    #[test]
    fn error_variant_for_partial_parse() {
        // Force an ERROR child by feeding malformed bytes.
        let p = parse_cst("{ broken @ # }");
        let doc = Document::cast(p.syntax()).unwrap();
        // Walk: anything kind-Error should round-trip via Expr::cast.
        let any_error = doc
            .syntax()
            .descendants()
            .filter_map(Expr::cast)
            .any(|e| matches!(e, Expr::Error(_)));
        assert!(any_error, "expected at least one Expr::Error variant");
    }
}
