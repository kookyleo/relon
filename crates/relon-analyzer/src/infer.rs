//! Lightweight expression-level type inference.
//!
//! Drives [`crate::Diagnostic::StaticTypeMismatch`] reporting from the
//! analyzer side so errors that are derivable from source + module
//! graph + schemas alone never reach the evaluator's runtime
//! `TypeMismatch` path.
//!
//! This module is intentionally conservative: when in doubt (function
//! call without a static signature, dynamic spread, deref through a
//! schema we don't know about, …) it returns [`InferredType::Any`] or
//! `None` so the runtime check stays the source of truth. Stage 1 only
//! widens analyzer-side coverage; Stage 3 will tackle FnCall typing.
//!
//! The engine is built around three pieces:
//!
//! * [`InferredType`] — a typed view of expression results, replacing
//!   the earlier ad-hoc `String`-name representation.
//! * [`TypeScope`] — a stack-friendly view of locals introduced by
//!   closure params (and, eventually, `where` / comprehension bindings).
//! * [`infer_type`] — a pure, side-effect-free function that walks an
//!   expression and reports back its inferred type.

use relon_parser::{Expr, Node, Operator, TokenKey, TypeNode};
use std::collections::HashMap;

use crate::resolve::ScopeFrame;
use crate::sig::{instantiate, lookup_signature};
use crate::tree::AnalyzedTree;

/// Map from schema-name → field-name → declared type. Lets the inference
/// pass look up `User alice: { ... }` and validate each inner field
/// against `User`'s schema. Re-exported from [`crate::typecheck`] so the
/// two passes share one shape.
pub(crate) type SchemaIndex = HashMap<String, HashMap<String, TypeNode>>;

/// Map from schema-name → list of direct base schema names. Drives the
/// Stage 2.3 brand / inheritance check inside [`InferredType::subsumes_with`].
/// `schema A {}; schema B A + { ... };` would store `bases["B"] = ["A"]`.
pub(crate) type SchemaBaseIndex = HashMap<String, Vec<String>>;

/// Walk the base-schema chain looking for `target` reachable from
/// `child`. Treats unknown intermediate names as a soft stop (returns
/// false) — the caller already handles the `name == head` exact-match
/// case before delegating here.
fn schema_extends(idx: &SchemaBaseIndex, child: &str, target: &str) -> bool {
    use std::collections::HashSet;
    let mut visited: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = vec![child.to_string()];
    while let Some(name) = stack.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        if name == target {
            return true;
        }
        if let Some(parents) = idx.get(&name) {
            for parent in parents {
                if !visited.contains(parent) {
                    stack.push(parent.clone());
                }
            }
        }
    }
    false
}

/// A type derived from a source expression by static inference. Compared
/// to the older string-based representation, this carries enough
/// information to validate generics, joins, and schema references
/// without re-walking the source.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InferredType {
    /// Unknown / unconstrained. Always passes any subsumption check —
    /// used when an expression can't be classified or the slot's
    /// declared type is `Any`.
    Any,
    Null,
    Bool,
    Int,
    Float,
    /// Either `Int` or `Float`. Produced by mixed numeric arithmetic
    /// (`Int + Float` → `Number`) and by `Number`-typed slots.
    Number,
    String,
    /// Homogeneous list. `List(Box::new(Any))` for empty / heterogeneous
    /// list literals.
    List(Box<InferredType>),
    /// Dict with String keys and the given value type. We only model the
    /// String-keyed case because the language doesn't admit non-String
    /// keys today (any non-string literal is already a parse error).
    Dict(Box<InferredType>),
    /// Reference to a user / prelude schema by name.
    Schema(String),
    /// Tagged variant: `(enum_name, variant_name)`.
    Variant(String, String),
    /// `T?`. The inner is the underlying type; a value of `Optional<T>`
    /// matches either `T` or `Null`.
    Optional(Box<InferredType>),
    /// Closure / function with the given parameter types and return.
    /// Param types are `Any` when the user didn't annotate them.
    Fn(Vec<InferredType>, Box<InferredType>),
}

impl InferredType {
    /// Human-readable name for diagnostics. Mirrors the runtime's
    /// `Value::type_name` formatting where possible.
    pub(crate) fn name(&self) -> String {
        match self {
            InferredType::Any => "Any".into(),
            InferredType::Null => "Null".into(),
            InferredType::Bool => "Bool".into(),
            InferredType::Int => "Int".into(),
            InferredType::Float => "Float".into(),
            InferredType::Number => "Number".into(),
            InferredType::String => "String".into(),
            InferredType::List(inner) => format!("List<{}>", inner.name()),
            InferredType::Dict(v) => format!("Dict<String, {}>", v.name()),
            InferredType::Schema(s) => s.clone(),
            InferredType::Variant(enum_name, v) => format!("{enum_name}.{v}"),
            InferredType::Optional(inner) => format!("{}?", inner.name()),
            InferredType::Fn(_, _) => "Closure".into(),
        }
    }

    /// True if a value of `self` is assignable to a slot declared as
    /// `expected`. Consults `bases` (when provided) so that a value
    /// typed as a derived schema satisfies a slot expecting one of its
    /// bases (Stage 2.3). Conservative: anything we can't classify
    /// returns `true` (the runtime check still owns the authoritative
    /// call).
    #[cfg(test)]
    pub(crate) fn subsumes(&self, expected: &TypeNode) -> bool {
        self.subsumes_with(expected, None)
    }

    /// Same as [`subsumes`] but consults a [`SchemaBaseIndex`] when
    /// comparing custom-schema heads, so a value typed as a derived
    /// schema satisfies a slot expecting one of its bases (Stage 2.3).
    pub(crate) fn subsumes_with(
        &self,
        expected: &TypeNode,
        bases: Option<&SchemaBaseIndex>,
    ) -> bool {
        // Shorthand: `Any` accepts anything; `T?` accepts `Null` or
        // recursively-stripped `T`.
        if let InferredType::Any = self {
            return true;
        }
        if expected.is_optional && matches!(self, InferredType::Null) {
            return true;
        }
        // Multi-segment custom types (`module.Name`): conservative path
        // — without a workspace import index visible here we can't tell
        // whether `path[0]` is a module alias or a nested-dict scope
        // path. The downstream `re_check_unknown_types` pass already
        // catches the `pkg.Wrong` case at the param/return level. For
        // typed-binding subsumption we stay conservative and let
        // runtime own the verdict.
        if expected.path.len() != 1 {
            return true;
        }
        let head = expected.path[0].as_str();
        match head {
            "Any" => true,
            "Number" => matches!(
                self,
                InferredType::Int | InferredType::Float | InferredType::Number
            ),
            "Int" => matches!(self, InferredType::Int),
            "Float" => matches!(self, InferredType::Float),
            "Bool" => matches!(self, InferredType::Bool),
            "String" => matches!(self, InferredType::String),
            "Null" => matches!(self, InferredType::Null),
            // v1.1: when the slot declares an element type
            // (`List<T>`, `Dict<String, T>`), recurse so a `List<Int>`
            // doesn't slip into a `List<String>` slot. A bare
            // unparameterized `List` keeps the v1 permissive
            // behavior.
            "List" => match self {
                InferredType::List(elem) => match expected.generics.first() {
                    Some(slot) => elem.subsumes_with(slot, bases),
                    None => true,
                },
                _ => false,
            },
            "Dict" => match self {
                InferredType::Dict(val) => match expected.generics.get(1) {
                    Some(slot) => val.subsumes_with(slot, bases),
                    None => true,
                },
                _ => false,
            },
            "Closure" | "Fn" => matches!(self, InferredType::Fn(_, _)),
            "Enum" => true, // tagged-enum metadata isn't tracked here
            // Custom schema name: we'd need the schema_index to know
            // whether `self` (a Schema or Variant) actually fits. The
            // caller (`check_typed_binding`) handles structural
            // sub-walks of dict literals against schemas; we accept here
            // and let runtime own deeper validation.
            _ => match self {
                InferredType::Schema(name) => {
                    name == head
                        || bases
                            .map(|idx| schema_extends(idx, name, head))
                            .unwrap_or(false)
                }
                InferredType::Variant(enum_name, _) => enum_name == head,
                _ => true,
            },
        }
    }

    /// Best common upper bound between two inferred types. Used by
    /// match arm body unification and (eventually) heterogeneous list
    /// joins. Returns [`InferredType::Any`] when no useful bound exists.
    pub(crate) fn join(a: &InferredType, b: &InferredType) -> InferredType {
        if a == b {
            return a.clone();
        }
        match (a, b) {
            (InferredType::Any, other) | (other, InferredType::Any) => other.clone(),
            (InferredType::Int, InferredType::Float)
            | (InferredType::Float, InferredType::Int)
            | (InferredType::Int, InferredType::Number)
            | (InferredType::Number, InferredType::Int)
            | (InferredType::Float, InferredType::Number)
            | (InferredType::Number, InferredType::Float) => InferredType::Number,
            (InferredType::Null, InferredType::Optional(inner))
            | (InferredType::Optional(inner), InferredType::Null) => {
                InferredType::Optional(inner.clone())
            }
            (InferredType::Null, other) | (other, InferredType::Null) => {
                InferredType::Optional(Box::new(other.clone()))
            }
            (InferredType::List(la), InferredType::List(lb)) => {
                InferredType::List(Box::new(Self::join(la, lb)))
            }
            (InferredType::Dict(va), InferredType::Dict(vb)) => {
                InferredType::Dict(Box::new(Self::join(va, vb)))
            }
            (InferredType::Variant(ea, _), InferredType::Variant(eb, _)) if ea == eb => {
                InferredType::Schema(ea.clone())
            }
            _ => InferredType::Any,
        }
    }
}

/// Convert a declared `TypeNode` into the equivalent `InferredType` so
/// formal-parameter / schema-field annotations can seed the inference
/// scope. Falls back to `Any` for shapes we don't model yet (multi-arg
/// generics beyond `List`/`Dict`, multi-segment custom paths).
pub(crate) fn infer_from_type_node(t: &TypeNode) -> InferredType {
    let base = match t.path.as_slice() {
        [single] => match single.as_str() {
            "Any" => InferredType::Any,
            "Null" => InferredType::Null,
            "Bool" => InferredType::Bool,
            "Int" => InferredType::Int,
            "Float" => InferredType::Float,
            "Number" => InferredType::Number,
            "String" => InferredType::String,
            "List" => {
                let inner = t
                    .generics
                    .first()
                    .map(infer_from_type_node)
                    .unwrap_or(InferredType::Any);
                InferredType::List(Box::new(inner))
            }
            "Dict" => {
                let val = t
                    .generics
                    .get(1)
                    .map(infer_from_type_node)
                    .unwrap_or(InferredType::Any);
                InferredType::Dict(Box::new(val))
            }
            "Closure" | "Fn" => InferredType::Fn(Vec::new(), Box::new(InferredType::Any)),
            "Enum" => InferredType::Any,
            other => InferredType::Schema(other.to_string()),
        },
        _ => InferredType::Any,
    };
    if t.is_optional {
        InferredType::Optional(Box::new(base))
    } else {
        base
    }
}

/// Lightweight scope used by inference. Mirrors `ScopeFrame` but only
/// carries what we need for type lookup (closure-param types and
/// stacked dict frames so a `Variable` / `Reference` head can walk
/// outward). Borrowed from the type-check walker so the two passes
/// share a single notion of "what's visible right now".
#[derive(Debug, Default)]
pub(crate) struct TypeScope<'a> {
    /// Closure-param types in the innermost frame. Populated when we
    /// enter a `Closure { params, ... }` node.
    pub(crate) locals: HashMap<String, InferredType>,
    /// Pointer back to the schema index so we can lift a `Variable(X)`
    /// that happens to name a schema into [`InferredType::Schema`].
    pub(crate) schemas: Option<&'a SchemaIndex>,
    /// Stack of dict scope frames (innermost last) so a bare `Variable`
    /// or `&sibling.X` can find the static type-hint of the field it
    /// resolves to.
    pub(crate) frames: Vec<&'a ScopeFrame>,
    /// Snapshot index from the analyzer tree, for translating frame
    /// field NodeIds back into their values' static types.
    pub(crate) tree: Option<&'a AnalyzedTree>,
}

impl<'a> TypeScope<'a> {
    pub(crate) fn new(tree: &'a AnalyzedTree, schemas: &'a SchemaIndex) -> Self {
        Self {
            locals: HashMap::new(),
            schemas: Some(schemas),
            frames: Vec::new(),
            tree: Some(tree),
        }
    }

    /// Look up `name` against (in order) closure params, dict frames,
    /// and schema names. Returns `None` when nothing matches.
    pub(crate) fn lookup(&self, name: &str) -> Option<InferredType> {
        if let Some(t) = self.locals.get(name) {
            return Some(t.clone());
        }
        if !self.frames.is_empty() {
            for frame in self.frames.iter().rev() {
                // Closure params shadow dict siblings (mirrors how
                // `resolve_variable` walks the chain).
                if let Some(t) = frame.closure_param_types.get(name) {
                    return Some(infer_from_type_node(t));
                }
                if frame.closure_params.contains_key(name) {
                    // Closure param without a declared type — `Any`,
                    // not `None`, so callers don't false-positive on
                    // a generic `f(x): x + 1`.
                    return Some(InferredType::Any);
                }
                if let (Some(tree), Some(node_id)) = (self.tree, frame.fields.get(name).copied()) {
                    let target = tree.node_index.get(&node_id)?;
                    // If the field carries an explicit type-hint, prefer
                    // that — it's the user's declared intent.
                    if let Some(hint) = &target.type_hint {
                        return Some(infer_from_type_node(hint));
                    }
                    // Otherwise infer from the value expression itself.
                    let scope = TypeScope {
                        locals: HashMap::new(),
                        schemas: self.schemas,
                        frames: Vec::new(),
                        tree: self.tree,
                    };
                    return infer_type(target, &scope);
                }
            }
        }
        if let Some(idx) = self.schemas {
            if idx.contains_key(name) {
                return Some(InferredType::Schema(name.to_string()));
            }
        }
        None
    }
}

/// Infer the static type of `node` under `scope`. Returns `None` for
/// expressions whose result depends on runtime computation we can't
/// statically classify (FnCall without a known signature, fully
/// dynamic references, …). Callers should treat `None` as "defer to
/// runtime", not as an error.
pub(crate) fn infer_type(node: &Node, scope: &TypeScope) -> Option<InferredType> {
    match &*node.expr {
        Expr::Null => Some(InferredType::Null),
        Expr::Bool(_) => Some(InferredType::Bool),
        Expr::Int(_) => Some(InferredType::Int),
        Expr::Float(_) => Some(InferredType::Float),
        Expr::String(_) => Some(InferredType::String),
        Expr::FString(_) => Some(InferredType::String),
        Expr::List(items) => {
            // Empty list → List<Any>; otherwise join element types to
            // get the tightest homogeneous bound.
            if items.is_empty() {
                return Some(InferredType::List(Box::new(InferredType::Any)));
            }
            let mut acc: Option<InferredType> = None;
            for item in items {
                let t = infer_type(item, scope).unwrap_or(InferredType::Any);
                acc = Some(match acc {
                    None => t,
                    Some(prev) => InferredType::join(&prev, &t),
                });
            }
            Some(InferredType::List(Box::new(
                acc.unwrap_or(InferredType::Any),
            )))
        }
        Expr::Dict(pairs) => {
            // Same idea, joining values into the dict's value type.
            if pairs.is_empty() {
                return Some(InferredType::Dict(Box::new(InferredType::Any)));
            }
            let mut acc: Option<InferredType> = None;
            for (_, v) in pairs {
                let t = infer_type(v, scope).unwrap_or(InferredType::Any);
                acc = Some(match acc {
                    None => t,
                    Some(prev) => InferredType::join(&prev, &t),
                });
            }
            Some(InferredType::Dict(Box::new(
                acc.unwrap_or(InferredType::Any),
            )))
        }
        Expr::Binary(op, left, right) => {
            let lt = infer_type(left, scope)?;
            let rt = infer_type(right, scope)?;
            infer_binary(*op, &lt, &rt)
        }
        Expr::Unary(op, inner) => {
            let t = infer_type(inner, scope)?;
            match op {
                Operator::Not if matches!(t, InferredType::Bool) => Some(InferredType::Bool),
                Operator::Sub
                    if matches!(
                        t,
                        InferredType::Int | InferredType::Float | InferredType::Number
                    ) =>
                {
                    Some(t)
                }
                _ => None,
            }
        }
        Expr::Ternary { then, els, .. } => {
            let tt = infer_type(then, scope)?;
            let et = infer_type(els, scope)?;
            // Ternary mirrors the runtime: at evaluation time exactly one
            // branch produces a value, so the result is one of `tt` /
            // `et`. Special-case Int/Float to promote to Float (matching
            // the runtime's numeric coercion) so existing `Float b:
            // cond ? 1 : 2.2` slots stay legal.
            let joined = match (&tt, &et) {
                (InferredType::Int, InferredType::Float)
                | (InferredType::Float, InferredType::Int) => InferredType::Float,
                _ => InferredType::join(&tt, &et),
            };
            // When neither branch was already `Any` but their join
            // collapses to `Any`, the branches are statically heterogeneous —
            // signal "uninferrable" so the caller can fall back to a
            // per-branch slot check (`Int c: true ? 1 : "2"` flags `"2"`
            // against `Int`).
            if matches!(joined, InferredType::Any)
                && !matches!(tt, InferredType::Any)
                && !matches!(et, InferredType::Any)
            {
                return None;
            }
            Some(joined)
        }
        Expr::Variable(path) => path_head(path).and_then(|name| scope.lookup(&name)),
        Expr::Reference { path, .. } => path_head(path).and_then(|name| scope.lookup(&name)),
        Expr::Closure {
            params,
            return_type,
            body,
        } => {
            // Param types come from explicit annotations; fall back to
            // `Any` when the user didn't declare one.
            let param_types: Vec<InferredType> = params
                .iter()
                .map(|p| {
                    p.type_hint
                        .as_ref()
                        .map(infer_from_type_node)
                        .unwrap_or(InferredType::Any)
                })
                .collect();
            // Body type: build a child scope with the params installed.
            let mut child_locals = scope.locals.clone();
            for (param, ty) in params.iter().zip(param_types.iter()) {
                child_locals.insert(param.name.clone(), ty.clone());
            }
            let child = TypeScope {
                locals: child_locals,
                schemas: scope.schemas,
                frames: scope.frames.clone(),
                tree: scope.tree,
            };
            let body_type = infer_type(body, &child).unwrap_or(InferredType::Any);
            let return_ty = match return_type {
                Some(rt) => infer_from_type_node(rt),
                None => body_type,
            };
            Some(InferredType::Fn(param_types, Box::new(return_ty)))
        }
        Expr::VariantCtor {
            enum_path, variant, ..
        } => {
            let head = enum_path.first()?;
            Some(InferredType::Variant(head.clone(), variant.clone()))
        }
        Expr::Match { arms, .. } => {
            // Match returns the join of arm bodies. If we can't infer
            // any arm, defer to runtime by returning None.
            let mut acc: Option<InferredType> = None;
            for (_, body) in arms {
                if let Some(t) = infer_type(body, scope) {
                    acc = Some(match acc {
                        None => t,
                        Some(prev) => InferredType::join(&prev, &t),
                    });
                }
            }
            acc
        }
        // Stage 3.5: when the call's head resolves to a known
        // signature (closure-index → host fns → stdlib), surface its
        // return type. Multi-segment / unknown calls still return
        // `None` so runtime keeps the verdict.
        //
        // v1.1: if the signature is generic, run unification against
        // each arg's inferred type so the returned `InferredType`
        // reflects placeholders the call site can pin down — e.g.
        // `_list_map([1,2,3], (n) => n + 1)` returns `List<Int>`
        // instead of `Any`. Args that infer to `None` / `Any`
        // contribute no binding; remaining placeholders fall back to
        // `Any` via `infer_from_type_node`.
        Expr::FnCall { path, args } => {
            if path.len() != 1 {
                return None;
            }
            let TokenKey::String(name, _, _) = path.first()? else {
                return None;
            };
            let tree = scope.tree?;
            let sig = lookup_signature(name, tree, &tree.host_fn_signatures)?;
            if sig.generics.is_empty() {
                return Some(infer_from_type_node(&sig.return_type));
            }
            let bindings = crate::generics::collect_bindings(&sig, args, scope);
            let instantiated = instantiate(&sig, &bindings);
            Some(infer_from_type_node(&instantiated.return_type))
        }
        // Comprehension / Where / Spread fall through to None — Stage 1
        // explicitly leaves them for later phases.
        _ => None,
    }
}

/// True if `lt op rt` is a *known-bad* combination — used by the
/// type-check walker to push a [`Diagnostic::StaticTypeMismatch`] for
/// e.g. `1 + "hello"`. Returning `false` doesn't necessarily mean the
/// combination is *good*; only that we can't prove it bad statically
/// (e.g. one operand is `Any`).
pub(crate) fn binary_known_invalid(op: Operator, lt: &InferredType, rt: &InferredType) -> bool {
    // If either side is Any we can't accuse the user — defer to runtime.
    if matches!(lt, InferredType::Any) || matches!(rt, InferredType::Any) {
        return false;
    }
    // Equality / comparison / logical ops accept any same-kind pair —
    // analyzer side stays conservative and only flags the arithmetic
    // and Add cases where the runtime would reject.
    match op {
        Operator::Add | Operator::Sub | Operator::Mul | Operator::Div | Operator::Mod => {
            // Numeric arithmetic on Int/Float is always fine. Add also
            // permits same-typed String / List / Dict concat. Anything
            // else (Int + String, Bool + Int, …) is statically bad.
            let is_numeric = |t: &InferredType| {
                matches!(
                    t,
                    InferredType::Int | InferredType::Float | InferredType::Number
                )
            };
            if is_numeric(lt) && is_numeric(rt) {
                return false;
            }
            if op == Operator::Add {
                if matches!(lt, InferredType::String) && matches!(rt, InferredType::String) {
                    return false;
                }
                if matches!(lt, InferredType::List(_)) && matches!(rt, InferredType::List(_)) {
                    return false;
                }
                if matches!(lt, InferredType::Dict(_)) && matches!(rt, InferredType::Dict(_)) {
                    return false;
                }
            }
            true
        }
        Operator::And | Operator::Or => {
            !(matches!(lt, InferredType::Bool) && matches!(rt, InferredType::Bool))
        }
        // `==`, `<`, `>`, … accept mismatched-but-comparable shapes
        // today (the runtime decides). Don't flag.
        Operator::Eq
        | Operator::Ne
        | Operator::Lt
        | Operator::Gt
        | Operator::Le
        | Operator::Ge
        | Operator::Pipe
        | Operator::Concat
        | Operator::Not => false,
    }
}

/// Compute the result type of `lt op rt`. None when the operation is
/// statically known to be invalid; callers that *only* care about the
/// validity decision should use [`binary_known_invalid`].
fn infer_binary(op: Operator, lt: &InferredType, rt: &InferredType) -> Option<InferredType> {
    if binary_known_invalid(op, lt, rt) {
        return None;
    }
    match op {
        Operator::Add | Operator::Sub | Operator::Mul | Operator::Div | Operator::Mod => {
            match (lt, rt) {
                // Int + Int stays Int; any Float involvement promotes
                // to Float (matches the runtime's coercion rule). Pure
                // `Number` operands stay `Number` because we don't know
                // the runtime branch the value will take.
                (InferredType::Int, InferredType::Int) => Some(InferredType::Int),
                (InferredType::Float, _) | (_, InferredType::Float)
                    if matches!(lt, InferredType::Int | InferredType::Float)
                        && matches!(rt, InferredType::Int | InferredType::Float) =>
                {
                    Some(InferredType::Float)
                }
                (
                    InferredType::Int | InferredType::Float | InferredType::Number,
                    InferredType::Int | InferredType::Float | InferredType::Number,
                ) => Some(InferredType::Number),
                (InferredType::String, InferredType::String) if op == Operator::Add => {
                    Some(InferredType::String)
                }
                (InferredType::List(a), InferredType::List(b)) if op == Operator::Add => {
                    Some(InferredType::List(Box::new(InferredType::join(a, b))))
                }
                (InferredType::Dict(a), InferredType::Dict(b)) if op == Operator::Add => {
                    Some(InferredType::Dict(Box::new(InferredType::join(a, b))))
                }
                _ => Some(InferredType::Any),
            }
        }
        Operator::And | Operator::Or => Some(InferredType::Bool),
        Operator::Eq | Operator::Ne | Operator::Lt | Operator::Gt | Operator::Le | Operator::Ge => {
            Some(InferredType::Bool)
        }
        _ => None,
    }
}

fn path_head(path: &[TokenKey]) -> Option<String> {
    match path.first()? {
        TokenKey::String(s, _, _) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze;
    use relon_parser::parse_document;

    fn analyze_str(src: &str) -> AnalyzedTree {
        let node = parse_document(src).unwrap();
        analyze(&node)
    }

    #[test]
    fn join_int_float_is_number() {
        assert_eq!(
            InferredType::join(&InferredType::Int, &InferredType::Float),
            InferredType::Number
        );
    }

    #[test]
    fn join_unrelated_is_any() {
        assert_eq!(
            InferredType::join(&InferredType::Int, &InferredType::String),
            InferredType::Any
        );
    }

    #[test]
    fn subsumes_optional_null() {
        let t = TypeNode {
            path: vec!["Int".to_string()],
            generics: Vec::new(),
            is_optional: true,
            range: relon_parser::TokenRange::default(),
            variant_fields: None,
            doc_comment: None,
        };
        assert!(InferredType::Null.subsumes(&t));
    }

    #[test]
    fn binary_string_plus_int_is_invalid() {
        assert!(binary_known_invalid(
            Operator::Add,
            &InferredType::Int,
            &InferredType::String
        ));
    }

    #[test]
    fn binary_any_is_never_invalid() {
        assert!(!binary_known_invalid(
            Operator::Add,
            &InferredType::Any,
            &InferredType::String
        ));
    }

    #[test]
    fn infer_string_literal() {
        let tree = analyze_str(r#"{ x: "hello" }"#);
        let scope = TypeScope::default();
        // Drill into x's value via the analyzer tree.
        let entry = tree.node_index.values().find_map(|n| match &*n.expr {
            Expr::String(s) if s == "hello" => Some(n.clone()),
            _ => None,
        });
        let n = entry.expect("string node indexed");
        assert_eq!(infer_type(&n, &scope), Some(InferredType::String));
    }
}
