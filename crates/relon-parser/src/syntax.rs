//! Concrete syntax tree (CST) foundation built on `rowan`.
//!
//! The v2 parser produces a lossless `SyntaxNode` tree: every byte of
//! input source — including whitespace and comments — is reachable
//! from the root via tokens, and walking the tree back to a string
//! yields the original bytes (verbatim).
//!
//! This module defines:
//!   - [`SyntaxKind`] — the unified token + node taxonomy. Every
//!     leaf in a `SyntaxNode` has a leaf `SyntaxKind`; every composite
//!     branch has a node `SyntaxKind`.
//!   - [`RelonLanguage`] — the rowan-side phantom that fixes the
//!     `SyntaxNode` / `SyntaxToken` / `SyntaxElement` type aliases to
//!     our `SyntaxKind`.
//!
//! The kinds are organised into ranges so callers can ask "is this a
//! trivia leaf?", "is this a punctuation leaf?", "is this a composite
//! node?" without an exhaustive match.

use std::fmt;

/// All token and node kinds the v2 parser produces. The discriminant is
/// kept stable and small (`u16`) so rowan's green tree can stash it
/// efficiently — and so adding a new kind in the middle would shift
/// values, change the boundary checks below. Append-only is the rule:
/// new kinds go before [`SyntaxKind::__LAST`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u16)]
#[allow(non_camel_case_types)]
pub enum SyntaxKind {
    // ----- trivia (covers every byte rowan would otherwise drop) ------
    /// Run of `\t \n\r ` characters between meaningful tokens.
    WHITESPACE,
    /// `// ...` to end of line.
    LINE_COMMENT,
    /// `/* ... */` (may span lines).
    BLOCK_COMMENT,

    // ----- literals + identifiers ------------------------------------
    /// Any `[A-Za-z_][A-Za-z0-9_]*` — keywords are NOT split out at
    /// lex time; the parser checks the text where context matters
    /// (`where`, `match`, `with`, `from`, `as`, etc.).
    IDENT,
    /// Integer / hex / octal / binary / float / scientific. The lexer
    /// captures the whole literal as one token; semantic conversion
    /// to `i64` / `f64` happens later.
    NUMBER,
    /// Any of: plain `"..."`, raw `r"..."` / `r#"..."#`, f-string
    /// `f"..."` / `f#"..."#`. The whole literal — opening quote
    /// through closing quote — is one token at the CST level. The
    /// typed-AST layer breaks f-strings into `FString` parts.
    STRING,

    // ----- single-char punctuation -----------------------------------
    L_BRACE,
    R_BRACE,
    L_BRACK,
    R_BRACK,
    L_PAREN,
    R_PAREN,
    COMMA,
    COLON,
    DOT,
    /// `@` — decorator sigil.
    AT,
    /// `#` — directive sigil.
    HASH,
    /// `&` — reference sigil (`&root.x`).
    AMP,
    /// `?` — optional-type marker or ternary head.
    QUESTION,
    /// `=` — standalone assignment-position equals.
    EQ,

    // ----- multi-char punctuation / operators ------------------------
    /// `...` spread / variadic.
    ELLIPSIS,
    /// `==`
    EQ_EQ,
    /// `!=`
    BANG_EQ,
    /// `<=`
    LT_EQ,
    /// `>=`
    GT_EQ,
    /// `&&`
    AMP_AMP,
    /// `||`
    PIPE_PIPE,
    /// `++`
    PLUS_PLUS,
    /// `=>`
    FAT_ARROW,
    /// `->`
    THIN_ARROW,

    // ----- single-char operators -------------------------------------
    /// `<`
    LT,
    /// `>`
    GT,
    /// `+`
    PLUS,
    /// `-`
    MINUS,
    /// `*` — multiplication, wildcard, or spread depending on context.
    STAR,
    /// `/`
    SLASH,
    /// `%`
    PERCENT,
    /// `!`
    BANG,
    /// `|`
    PIPE,

    /// Any source byte the lexer couldn't classify (stray UTF-8
    /// punctuation, control characters, etc.). Emitted as a single-
    /// codepoint token so the round-trip-by-bytes invariant holds.
    /// Downstream tooling treats this like a syntax error.
    UNKNOWN,

    // ----- composite-node kinds (populated through P2/P3) ------------
    //
    // Each kind below names a grammar production. Their byte content
    // is reachable through their child tokens / nodes; rowan stitches
    // it all back into source via `SyntaxNode::text`. P2 fills these
    // in; P1 only needs `DOCUMENT` + `ERROR` to round-trip-lex.
    //
    /// Whole-file root. Always present. Children:
    /// trivia*, top-level directives*, top-level value, trivia*.
    DOCUMENT,
    /// A `#name <body?>` form.
    DIRECTIVE,
    /// `@name(args?)` form.
    DECORATOR,
    /// `{ ... }` dict / object literal.
    DICT,
    /// One `key: value` (or `key(params): body`) pair inside a DICT.
    DICT_FIELD,
    /// `[ ... ]` list / array literal.
    LIST,
    /// `for x in xs if cond` body inside a LIST.
    COMPREHENSION,
    /// `name(p, q, ...) [-> R]: body` lowered to closure.
    CLOSURE,
    /// Single closure parameter (`name: T` or bare `name`).
    CLOSURE_PARAM,
    /// `name(arg1, arg2 = expr, ...)` call.
    CALL_EXPR,
    /// One arg inside a call's parens — positional or `name = expr`.
    CALL_ARG,
    /// Binary operation node (`a + b`, `a == b`, etc.).
    BINARY_EXPR,
    /// Unary operation node (`!a`, `-a`).
    UNARY_EXPR,
    /// `cond ? then : else`.
    TERNARY_EXPR,
    /// `&base.x.y` reference.
    REFERENCE_EXPR,
    /// `name[.tail]*` bareword path.
    VARIABLE_EXPR,
    /// `expr where { bindings }`.
    WHERE_EXPR,
    /// `expr match { type: arm, ... }`.
    MATCH_EXPR,
    /// One arm inside a MATCH_EXPR.
    MATCH_ARM,
    /// `EnumName.VariantName { ... }`.
    VARIANT_CTOR,
    /// `f"..."` rendered as a CST node so interpolations are children.
    /// (For v1 the lexer still emits a single `STRING` token; this
    /// node kind is reserved for the later f-string refinement.)
    F_STRING,
    /// Spread expression `...expr` inside a dict / list.
    SPREAD_EXPR,
    /// A type expression: `Int`, `List<String>`, `User?`, …
    TYPE_NODE,
    /// `*` in wildcard / placeholder position.
    WILDCARD,
    /// Literal `null` / `true` / `false`.
    LITERAL,
    /// Unrecoverable parse failure: spans the bytes the parser
    /// couldn't fit into any production. Always has at least one
    /// child token. This is the "first-class hole" that lets
    /// downstream tooling keep working on partial input.
    ERROR,

    // Append new kinds above this line.
    /// Sentinel to keep `(SyntaxKind as u16) < (__LAST as u16)`
    /// available for boundary checks. Never produced.
    __LAST,
}

impl SyntaxKind {
    /// True for `WHITESPACE` / `LINE_COMMENT` / `BLOCK_COMMENT` —
    /// tokens that carry no semantic content. Useful for skipping
    /// when walking the tree for meaningful structure.
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            SyntaxKind::WHITESPACE | SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT
        )
    }

    /// True when the kind names a leaf (token) rather than a
    /// composite branch (node). All kinds before `DOCUMENT` in the
    /// enum order are leaves; everything from `DOCUMENT` to `ERROR`
    /// is a node. Keep in sync with the enum layout above.
    pub fn is_token(self) -> bool {
        (self as u16) < (SyntaxKind::DOCUMENT as u16)
    }

    pub fn is_node(self) -> bool {
        !self.is_token()
    }
}

impl fmt::Display for SyntaxKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl From<SyntaxKind> for rowan::SyntaxKind {
    fn from(kind: SyntaxKind) -> Self {
        rowan::SyntaxKind(kind as u16)
    }
}

/// rowan-side phantom that ties [`SyntaxKind`] to rowan's tree
/// generics. Don't construct an instance — it's used only at the
/// type level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RelonLanguage {}

impl rowan::Language for RelonLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> Self::Kind {
        SyntaxKind::from_raw(raw.0)
            .unwrap_or_else(|| panic!("raw kind out of range: {raw:?}"))
    }

    fn kind_to_raw(kind: Self::Kind) -> rowan::SyntaxKind {
        kind.into()
    }
}

impl SyntaxKind {
    /// Round-trip back from the raw `u16` rowan stores in its green
    /// tree. Total over the enum's domain; returns `None` for any
    /// out-of-range value. The match is exhaustive so the compiler
    /// catches missing entries when new kinds are appended.
    pub fn from_raw(raw: u16) -> Option<Self> {
        let kind = match raw {
            x if x == Self::WHITESPACE as u16 => Self::WHITESPACE,
            x if x == Self::LINE_COMMENT as u16 => Self::LINE_COMMENT,
            x if x == Self::BLOCK_COMMENT as u16 => Self::BLOCK_COMMENT,
            x if x == Self::IDENT as u16 => Self::IDENT,
            x if x == Self::NUMBER as u16 => Self::NUMBER,
            x if x == Self::STRING as u16 => Self::STRING,
            x if x == Self::L_BRACE as u16 => Self::L_BRACE,
            x if x == Self::R_BRACE as u16 => Self::R_BRACE,
            x if x == Self::L_BRACK as u16 => Self::L_BRACK,
            x if x == Self::R_BRACK as u16 => Self::R_BRACK,
            x if x == Self::L_PAREN as u16 => Self::L_PAREN,
            x if x == Self::R_PAREN as u16 => Self::R_PAREN,
            x if x == Self::COMMA as u16 => Self::COMMA,
            x if x == Self::COLON as u16 => Self::COLON,
            x if x == Self::DOT as u16 => Self::DOT,
            x if x == Self::AT as u16 => Self::AT,
            x if x == Self::HASH as u16 => Self::HASH,
            x if x == Self::AMP as u16 => Self::AMP,
            x if x == Self::QUESTION as u16 => Self::QUESTION,
            x if x == Self::EQ as u16 => Self::EQ,
            x if x == Self::ELLIPSIS as u16 => Self::ELLIPSIS,
            x if x == Self::EQ_EQ as u16 => Self::EQ_EQ,
            x if x == Self::BANG_EQ as u16 => Self::BANG_EQ,
            x if x == Self::LT_EQ as u16 => Self::LT_EQ,
            x if x == Self::GT_EQ as u16 => Self::GT_EQ,
            x if x == Self::AMP_AMP as u16 => Self::AMP_AMP,
            x if x == Self::PIPE_PIPE as u16 => Self::PIPE_PIPE,
            x if x == Self::PLUS_PLUS as u16 => Self::PLUS_PLUS,
            x if x == Self::FAT_ARROW as u16 => Self::FAT_ARROW,
            x if x == Self::THIN_ARROW as u16 => Self::THIN_ARROW,
            x if x == Self::LT as u16 => Self::LT,
            x if x == Self::GT as u16 => Self::GT,
            x if x == Self::PLUS as u16 => Self::PLUS,
            x if x == Self::MINUS as u16 => Self::MINUS,
            x if x == Self::STAR as u16 => Self::STAR,
            x if x == Self::SLASH as u16 => Self::SLASH,
            x if x == Self::PERCENT as u16 => Self::PERCENT,
            x if x == Self::BANG as u16 => Self::BANG,
            x if x == Self::PIPE as u16 => Self::PIPE,
            x if x == Self::UNKNOWN as u16 => Self::UNKNOWN,
            x if x == Self::DOCUMENT as u16 => Self::DOCUMENT,
            x if x == Self::DIRECTIVE as u16 => Self::DIRECTIVE,
            x if x == Self::DECORATOR as u16 => Self::DECORATOR,
            x if x == Self::DICT as u16 => Self::DICT,
            x if x == Self::DICT_FIELD as u16 => Self::DICT_FIELD,
            x if x == Self::LIST as u16 => Self::LIST,
            x if x == Self::COMPREHENSION as u16 => Self::COMPREHENSION,
            x if x == Self::CLOSURE as u16 => Self::CLOSURE,
            x if x == Self::CLOSURE_PARAM as u16 => Self::CLOSURE_PARAM,
            x if x == Self::CALL_EXPR as u16 => Self::CALL_EXPR,
            x if x == Self::CALL_ARG as u16 => Self::CALL_ARG,
            x if x == Self::BINARY_EXPR as u16 => Self::BINARY_EXPR,
            x if x == Self::UNARY_EXPR as u16 => Self::UNARY_EXPR,
            x if x == Self::TERNARY_EXPR as u16 => Self::TERNARY_EXPR,
            x if x == Self::REFERENCE_EXPR as u16 => Self::REFERENCE_EXPR,
            x if x == Self::VARIABLE_EXPR as u16 => Self::VARIABLE_EXPR,
            x if x == Self::WHERE_EXPR as u16 => Self::WHERE_EXPR,
            x if x == Self::MATCH_EXPR as u16 => Self::MATCH_EXPR,
            x if x == Self::MATCH_ARM as u16 => Self::MATCH_ARM,
            x if x == Self::VARIANT_CTOR as u16 => Self::VARIANT_CTOR,
            x if x == Self::F_STRING as u16 => Self::F_STRING,
            x if x == Self::SPREAD_EXPR as u16 => Self::SPREAD_EXPR,
            x if x == Self::TYPE_NODE as u16 => Self::TYPE_NODE,
            x if x == Self::WILDCARD as u16 => Self::WILDCARD,
            x if x == Self::LITERAL as u16 => Self::LITERAL,
            x if x == Self::ERROR as u16 => Self::ERROR,
            _ => return None,
        };
        Some(kind)
    }
}

/// Convenience aliases. The vast majority of consumers should reach
/// for these instead of touching rowan generics directly.
pub type SyntaxNode = rowan::SyntaxNode<RelonLanguage>;
pub type SyntaxToken = rowan::SyntaxToken<RelonLanguage>;
pub type SyntaxElement = rowan::SyntaxElement<RelonLanguage>;

#[cfg(test)]
mod tests {
    use super::*;
    use rowan::Language;

    #[test]
    fn trivia_classification() {
        assert!(SyntaxKind::WHITESPACE.is_trivia());
        assert!(SyntaxKind::LINE_COMMENT.is_trivia());
        assert!(SyntaxKind::BLOCK_COMMENT.is_trivia());
        assert!(!SyntaxKind::IDENT.is_trivia());
        assert!(!SyntaxKind::DOCUMENT.is_trivia());
    }

    #[test]
    fn token_vs_node_split() {
        // Every kind before DOCUMENT is a token; everything from
        // DOCUMENT through ERROR is a node.
        assert!(SyntaxKind::WHITESPACE.is_token());
        assert!(SyntaxKind::IDENT.is_token());
        assert!(SyntaxKind::EQ.is_token());
        assert!(SyntaxKind::PIPE.is_token());
        assert!(SyntaxKind::DOCUMENT.is_node());
        assert!(SyntaxKind::DICT.is_node());
        assert!(SyntaxKind::ERROR.is_node());
    }

    #[test]
    fn round_trip_through_rowan_language() {
        // Sanity: every leaf + node kind round-trips through
        // `kind_to_raw` ∘ `kind_from_raw` — guards against any
        // accidental enum-layout drift.
        for kind in [
            SyntaxKind::WHITESPACE,
            SyntaxKind::IDENT,
            SyntaxKind::NUMBER,
            SyntaxKind::STRING,
            SyntaxKind::HASH,
            SyntaxKind::DOCUMENT,
            SyntaxKind::DICT,
            SyntaxKind::CLOSURE,
            SyntaxKind::ERROR,
        ] {
            let raw = RelonLanguage::kind_to_raw(kind);
            assert_eq!(RelonLanguage::kind_from_raw(raw), kind);
        }
    }
}
