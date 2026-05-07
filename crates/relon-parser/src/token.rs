use ordered_float::OrderedFloat;
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU32, Ordering};

/// Stable identifier assigned to every `Node` at parse time.
///
/// Used as the key in side-tables maintained by `relon-analyzer` (resolved
/// references, desugar caches, diagnostics) so analyzer passes can attach
/// information without mutating the AST itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct NodeId(pub u32);

impl NodeId {
    /// Sentinel id for synthetic nodes built outside the parser (e.g. by
    /// the evaluator when fabricating a `Type` node mid-flight). Analyzer
    /// side-tables must not key on this value.
    pub const SYNTHETIC: NodeId = NodeId(0);

    /// Allocate a fresh, process-wide-unique id.
    ///
    /// Public so AST rewriters outside the parser (analyzer, evaluator
    /// fabricated nodes, host transforms) can mint ids that won't collide
    /// with parser-emitted ones.
    pub fn alloc() -> NodeId {
        // Start at 1 so `SYNTHETIC` (0) stays distinct from any real node.
        static COUNTER: AtomicU32 = AtomicU32::new(1);
        NodeId(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

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
    /// `Some(_)` only when this node is an alternative inside a tagged
    /// `Enum<...>` (sum type). `Some(vec![])` is a unit variant; `Some(non-empty)`
    /// carries the variant's struct-shape payload as `(field_name, field_type)`
    /// pairs. Stays `None` for every non-variant type expression so the rest
    /// of the type system continues to ignore it.
    pub variant_fields: Option<Vec<(String, TypeNode)>>,
    /// Documentation extracted from leading comments.
    pub doc_comment: Option<String>,
}

#[derive(Debug, PartialEq, Clone)]
pub struct ClosureParam {
    pub name: String,
    pub type_hint: Option<TypeNode>,
    pub range: TokenRange,
}

#[derive(Debug, Clone)]
pub struct Node {
    /// Stable identity assigned at construction. Analyzer side-tables key
    /// off this; not part of structural equality.
    pub id: NodeId,
    pub expr: Box<Expr>,
    pub decorators: Vec<Decorator>,
    pub type_hint: Option<TypeNode>,
    pub range: TokenRange,
    /// Documentation extracted from leading comments immediately preceding
    /// the node.
    pub doc_comment: Option<String>,
}

/// Structural equality only — `id` is intentionally excluded so two
/// independently-parsed-but-identical AST fragments still compare equal.
/// This matters for `Value::Closure` PartialEq (compares `body: Node`) and
/// for parser tests that round-trip syntactic shape.
impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        self.expr == other.expr
            && self.decorators == other.decorators
            && self.type_hint == other.type_hint
            && self.range == other.range
            && self.doc_comment == other.doc_comment
    }
}

impl Node {
    pub fn new(expr: Expr, range: TokenRange) -> Self {
        Self {
            id: NodeId::alloc(),
            expr: Box::new(expr),
            decorators: Vec::new(),
            type_hint: None,
            range,
            doc_comment: None,
        }
    }

    /// Construct a `Node` with a caller-supplied `NodeId`. Used by tests
    /// and (rarely) by AST rewriters that want to preserve the original
    /// node's identity after a structural transform.
    pub fn with_id(id: NodeId, expr: Expr, range: TokenRange) -> Self {
        Self {
            id,
            expr: Box::new(expr),
            decorators: Vec::new(),
            type_hint: None,
            range,
            doc_comment: None,
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

    pub fn with_doc_comment(mut self, doc_comment: Option<String>) -> Self {
        self.doc_comment = doc_comment;
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

    /// Tagged-enum variant constructor: `EnumName.VariantName { field: value, ... }`.
    /// Unit variants share the bare-identifier-path form parsed as `Variable`
    /// — the evaluator promotes them to a variant when the head resolves to
    /// a sum-type schema.
    VariantCtor {
        enum_path: Vec<String>,
        variant: String,
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
    Interpolation(Box<Node>),
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

impl Expr {
    /// Stable, allocation-free name for the variant — used by diagnostics
    /// (`SchemaBodyNotDict { found }`) and any walker that wants a cheap
    /// dispatch tag without matching the full enum.
    pub fn kind(&self) -> &'static str {
        match self {
            Expr::Null => "Null",
            Expr::Bool(_) => "Bool",
            Expr::Int(_) => "Int",
            Expr::Float(_) => "Float",
            Expr::String(_) => "String",
            Expr::List(_) => "List",
            Expr::Dict(_) => "Dict",
            Expr::Spread(_) => "Spread",
            Expr::Comprehension { .. } => "Comprehension",
            Expr::Variable(_) => "Variable",
            Expr::Reference { .. } => "Reference",
            Expr::Binary(_, _, _) => "Binary",
            Expr::Unary(_, _) => "Unary",
            Expr::Ternary { .. } => "Ternary",
            Expr::FnCall { .. } => "FnCall",
            Expr::FString(_) => "FString",
            Expr::Type(_) => "Type",
            Expr::Wildcard => "Wildcard",
            Expr::Where { .. } => "Where",
            Expr::Match { .. } => "Match",
            Expr::Closure { .. } => "Closure",
            Expr::VariantCtor { .. } => "VariantCtor",
        }
    }
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

/// Cheap predicate over the canonical builtin-type identifier set used by
/// the type system (`Int`, `String`, ..., `Enum`). Centralized here so the
/// analyzer, evaluator, and runtime checker all agree on one list.
pub fn is_builtin_type_name(name: &str) -> bool {
    matches!(
        name,
        "Int"
            | "Float"
            | "Number"
            | "String"
            | "Bool"
            | "Any"
            | "Null"
            | "List"
            | "Dict"
            | "Closure"
            | "Fn"
            | "Enum"
    )
}

/// Lift a decorator-argument [`Expr`] back into a [`TypeNode`].
///
/// Used by every site that consumes a `@brand(Type)` argument — the
/// evaluator's `BrandDecorator::wrap_with_ast` runs this on the live
/// argument, and the analyzer's schema-field lowering runs it to lift
/// `@brand(X)` placed on a typeless schema field into an implicit type
/// prefix.
///
/// Accepted shapes:
///
/// * Full type expression (`Map<String, Int>`, `Foo<T>`, `Weather?`,
///   `Int`, `Enum<...>`) — produced by [`crate::expr::parse_type_expr`]
///   and surfaced as `Expr::Type`. The contained `TypeNode` is returned
///   verbatim so generics and `is_optional` survive.
/// * Bareword / dotted path (`Weather`, `geo.Location`) — surfaced as
///   `Expr::Variable` because the parser only commits to `Expr::Type`
///   when it sees generics, `?`, or a known builtin head. Each path
///   segment must be a simple identifier (no `?.`, `[i]`, or spread).
/// * String literal (`"Weather"`, `"geo.Location"`) — split on `.` for
///   parity with the bareword form.
///
/// Returns `None` when `expr` is none of the above; callers turn that
/// into a user-facing "argument must be a type" error.
pub fn type_node_from_brand_arg(expr: &Expr, range: TokenRange) -> Option<TypeNode> {
    match expr {
        Expr::Type(t) => Some(t.clone()),
        Expr::Variable(path) => {
            let mut segs = Vec::with_capacity(path.len());
            for tk in path {
                match tk {
                    TokenKey::String(s, _, false) => segs.push(s.clone()),
                    _ => return None,
                }
            }
            if segs.is_empty() {
                return None;
            }
            Some(TypeNode {
                path: segs,
                generics: Vec::new(),
                is_optional: false,
                range,
                variant_fields: None,
                doc_comment: None,
            })
        }
        Expr::String(s) => {
            if s.is_empty() {
                return None;
            }
            let segs: Vec<String> = s.split('.').map(|p| p.to_string()).collect();
            if segs.iter().any(|p| p.is_empty()) {
                return None;
            }
            Some(TypeNode {
                path: segs,
                generics: Vec::new(),
                is_optional: false,
                range,
                variant_fields: None,
                doc_comment: None,
            })
        }
        _ => None,
    }
}
