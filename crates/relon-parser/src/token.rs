use ordered_float::OrderedFloat;
use std::fmt::{Display, Formatter};

#[derive(Debug, PartialEq, Clone, Eq, Copy, Default, Hash)]
pub struct TokenPosition {
    pub line: u32,
    pub column: usize,
    pub offset: usize,
}

#[derive(Debug, PartialEq, Clone, Eq, Copy, Default, Hash)]
pub struct TokenRange {
    pub start: TokenPosition,
    pub end: TokenPosition,
}

impl From<TokenRange> for miette::SourceSpan {
    fn from(range: TokenRange) -> Self {
        let len = range.end.offset.saturating_sub(range.start.offset);
        (range.start.offset, len).into()
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum TokenKey {
    Dummy,
    Index(usize, bool),               // index, is_optional
    String(String, TokenRange, bool), // name, range, is_optional
    Dynamic(Node, bool),              // expr, is_optional
    Spread(TokenRange),
}

impl TokenKey {
    pub fn name(&self) -> String {
        match self {
            TokenKey::Dummy => "_".to_string(),
            TokenKey::Index(i, _) => i.to_string(),
            TokenKey::String(s, _, _) => s.clone(),
            TokenKey::Dynamic(_, _) => "<dynamic>".to_string(),
            TokenKey::Spread(_) => "...".to_string(),
        }
    }

    pub fn to_string_key(&self) -> String {
        self.name()
    }

    pub fn is_optional(&self) -> bool {
        match self {
            TokenKey::Index(_, opt) => *opt,
            TokenKey::String(_, _, opt) => *opt,
            TokenKey::Dynamic(_, opt) => *opt,
            _ => false,
        }
    }
}

#[derive(Debug, PartialEq, Clone, Hash, Eq)]
pub struct TokenId(pub String, pub TokenRange);

impl TokenId {
    pub fn name(&self) -> &str {
        &self.0
    }
}

/// Represents a single argument in a function call or decorator.
/// Can be positional or named (keyword).
#[derive(Debug, PartialEq, Clone)]
pub struct CallArg {
    pub name: Option<String>,
    pub value: Node,
}

#[derive(Debug, PartialEq, Clone)]
pub struct Decorator {
    pub path: Vec<TokenKey>,
    pub args: Vec<CallArg>,
    pub range: TokenRange,
}

#[derive(Debug, PartialEq, Clone)]
pub struct TypeNode {
    pub path: Vec<String>,
    pub generics: Vec<TypeNode>,
    pub is_optional: bool,
    pub range: TokenRange,
}

#[derive(Debug, PartialEq, Clone)]
pub struct ClosureParam {
    pub name: String,
    pub type_hint: Option<TypeNode>,
    pub range: TokenRange,
}

#[derive(Debug, PartialEq, Clone)]
pub struct Node {
    pub expr: Box<Expr>,
    pub decorators: Vec<Decorator>,
    pub type_hint: Option<TypeNode>,
    pub range: TokenRange,
}

impl Node {
    pub fn new(expr: Expr, range: TokenRange) -> Self {
        Self {
            expr: Box::new(expr),
            decorators: Vec::new(),
            type_hint: None,
            range,
        }
    }

    pub fn with_decorators(mut self, decorators: Vec<Decorator>) -> Self {
        self.decorators = decorators;
        self
    }

    pub fn with_type_hint(mut self, type_hint: Option<TypeNode>) -> Self {
        self.type_hint = type_hint;
        self
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum Expr {
    Null,
    Bool(bool),
    Int(i64),
    Float(OrderedFloat<f64>),
    String(String),

    List(Vec<Node>),
    Dict(Vec<(TokenKey, Node)>),

    Spread(Node),

    Comprehension {
        element: Node,
        id: String,
        iterable: Node,
        condition: Option<Node>,
    },

    Variable(Vec<TokenKey>),
    Reference {
        base: RefBase,
        path: Vec<TokenKey>,
    },

    Binary(Operator, Node, Node),
    Unary(Operator, Node),
    Ternary {
        cond: Node,
        then: Node,
        els: Node,
    },

    FnCall {
        path: Vec<TokenKey>,
        args: Vec<CallArg>,
    },

    FString(Vec<FStringPart>),

    Type(TypeNode),

    Wildcard,

    Where {
        expr: Node,
        bindings: Node,
    },

    Match {
        expr: Node,
        arms: Vec<(Node, Node)>,
    },

    Closure {
        params: Vec<ClosureParam>,
        return_type: Option<TypeNode>,
        body: Node,
    },
}
#[derive(Debug, PartialEq, Clone, Copy, Eq, Hash)]
pub enum RefBase {
    Root,
    Sibling,
    Uncle,
    Prev,
    Next,
    Index,
    This,
}

#[derive(Debug, PartialEq, Clone)]
pub enum FStringPart {
    Literal(String),
    Interpolation(Node),
}

#[derive(Debug, PartialEq, Clone, Copy, Eq, Hash)]
pub enum Operator {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    Not,
    Pipe,
    Concat,
}

impl Display for Expr {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Expr::Null => write!(f, "null"),
            Expr::Bool(v) => write!(f, "{}", v),
            Expr::Int(v) => write!(f, "{}", v),
            Expr::Float(v) => write!(f, "{}", v),
            Expr::String(v) => write!(f, "\"{}\"", v),
            _ => write!(f, "<expr>"),
        }
    }
}
