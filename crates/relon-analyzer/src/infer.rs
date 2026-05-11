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
use crate::workspace_build::WorkspaceImportIndex;

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
    /// v1.7: tuple type. Element types in declaration order. Empty
    /// vec is the unit tuple `()`. List literals (`[1, "x", true]`)
    /// infer to `Tuple` so each element's type is preserved; the
    /// subsumption logic folds a `Tuple` back to `List<T>` (or
    /// rejects mismatch) when the slot's expected type is a List.
    Tuple(Vec<InferredType>),
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
            InferredType::Tuple(elems) => {
                if elems.is_empty() {
                    "()".into()
                } else if elems.len() == 1 {
                    format!("({},)", elems[0].name())
                } else {
                    let parts: Vec<String> = elems.iter().map(|t| t.name()).collect();
                    format!("({})", parts.join(", "))
                }
            }
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
        self.subsumes_with_imports(expected, bases, None)
    }

    /// v1.8 cross-module: same as [`subsumes_with`] but also consults
    /// a [`WorkspaceImportIndex`] when the expected slot is a
    /// two-segment path (`pkg.Name`). A `pkg.User` slot is folded to a
    /// `Schema("User")` slot when `pkg` is a known alias and `User`
    /// is one of its exports. Pre-v1.8 all multi-segment expected
    /// types accepted unconditionally.
    pub(crate) fn subsumes_with_imports(
        &self,
        expected: &TypeNode,
        bases: Option<&SchemaBaseIndex>,
        imports: Option<&WorkspaceImportIndex>,
    ) -> bool {
        // Shorthand: `Any` accepts anything; `T?` accepts `Null` or
        // recursively-stripped `T`.
        if let InferredType::Any = self {
            return true;
        }
        if expected.is_optional && matches!(self, InferredType::Null) {
            return true;
        }
        // v1.8 / v1.8e: `pkg.User` (two segments, alias resolved)
        // collapses to a single-segment `Schema("alias.User")` slot —
        // the *qualified* key so two aliases of `User` from different
        // libs don't collide on the bare name.
        if expected.path.len() == 2 {
            if let Some(qualified) =
                cross_module_schema(&expected.path[0], &expected.path[1], imports)
            {
                let mut folded = expected.clone();
                folded.path = vec![qualified];
                return self.subsumes_with_imports(&folded, bases, imports);
            }
        }
        // Multi-segment custom types (`module.Name`) we couldn't
        // resolve via the import index: conservative pass. The
        // downstream `re_check_unknown_types` pass catches the
        // `pkg.Wrong` case at the param/return level.
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
                // v1.7: a tuple subsumes `List<T>` iff every element
                // type satisfies T. This is the "fold a tuple into a
                // homogeneous list" path — what makes `[1, 2, 3]`
                // (inferred as `Tuple(Int, Int, Int)`) acceptable in
                // a `List<Int>` slot. Heterogeneous tuples like
                // `Tuple(Int, String)` correctly fail against
                // `List<Int>` because `String.subsumes(Int)` is
                // false.
                InferredType::Tuple(elems) => match expected.generics.first() {
                    Some(slot) => elems.iter().all(|e| e.subsumes_with(slot, bases)),
                    // Bare `List` — v1.7 forbids this, but we keep
                    // permissive accept here so the diagnostic for
                    // bare-generic fires once (at the type-position
                    // walker) instead of cascading into mismatches.
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
            // v1.8: `Enum<alt1, alt2, ...>` slot. The value must
            // satisfy at least one alternative. Alternatives come in
            // three flavours: (i) built-in type (`Int`, `String`, …)
            // or custom schema — we recurse into `subsumes_with`;
            // (ii) bareword identifier — at the analyzer level we
            // can't tell whether it's a string-literal alternative
            // (parser strips quotes from `"up"` so `up` and a real
            // bareword `Active` look identical) or a tagged-variant
            // / schema reference. We treat any non-builtin bareword
            // alternative as compatible with `String` values (the
            // runtime's cheap path does the same), and recurse into
            // `subsumes_with` for everything else.
            "Enum" => {
                if expected.generics.is_empty() {
                    return true; // ban-bare catches this upstream
                }
                expected
                    .generics
                    .iter()
                    .any(|alt| enum_alt_matches(self, alt, bases))
            }
            // v1.7: tuple-typed slot. Two cases:
            //   - self is a Tuple: arity must match, then check
            //     element-by-element against the slot's positional
            //     generic args.
            //   - self is a List<T>: every element of T must satisfy
            //     each slot type (rare but well-defined: `List<Int>`
            //     fits into `(Int, Int, Int)` only when arity is 3
            //     and T is Int — but at the analyzer level we don't
            //     know the runtime length of a List, so we fall
            //     back to true and let runtime own the verdict).
            "Tuple" => match self {
                InferredType::Tuple(elems) => {
                    let slot_count = expected.generics.len();
                    if elems.len() != slot_count {
                        return false;
                    }
                    elems
                        .iter()
                        .zip(expected.generics.iter())
                        .all(|(e, slot)| e.subsumes_with(slot, bases))
                }
                // A `List<T>` against a fixed-arity tuple slot: the
                // arity isn't statically known, so we accept and
                // defer to runtime. Heterogeneous tuples never end
                // up here because list literals infer as Tuple
                // directly.
                InferredType::List(_) => true,
                _ => false,
            },
            // Custom schema name: we'd need the schema_index to know
            // whether `self` (a Schema or Variant) actually fits. The
            // caller (`check_typed_binding`) handles structural
            // sub-walks of dict literals against schemas; we accept
            // ambiguous shapes here and let runtime own deeper
            // validation. v1.8 tightens this for *clearly-non-schema*
            // values: a primitive / list / fn / tuple landing in a
            // schema slot is a hard mismatch the analyzer can flag
            // statically (the runtime would too).
            _ => match self {
                InferredType::Schema(name) => {
                    name == head
                        || bases
                            .map(|idx| schema_extends(idx, name, head))
                            .unwrap_or(false)
                }
                InferredType::Variant(enum_name, _) => enum_name == head,
                // v1.8: structural-shape clearly distinct from a
                // schema (which is always Dict-shaped at runtime).
                // Primitives, lists, fns, tuples can never fit a
                // schema slot, so reject statically.
                InferredType::Int
                | InferredType::Float
                | InferredType::Number
                | InferredType::Bool
                | InferredType::String
                | InferredType::Null
                | InferredType::List(_)
                | InferredType::Tuple(_)
                | InferredType::Fn(_, _) => false,
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
            // v1.7: tuple ∪ tuple. Same arity → element-wise join;
            // different arity → fall through to Any (the values
            // can't share a tuple shape).
            (InferredType::Tuple(a), InferredType::Tuple(b)) if a.len() == b.len() => {
                InferredType::Tuple(
                    a.iter()
                        .zip(b.iter())
                        .map(|(x, y)| Self::join(x, y))
                        .collect(),
                )
            }
            // Tuple ∪ List: collapse the tuple to its element-join
            // and join with the list's element type. Used by match-arm
            // joins where one arm builds a tuple-shaped literal and
            // another returns a List-typed value.
            (InferredType::Tuple(elems), InferredType::List(l))
            | (InferredType::List(l), InferredType::Tuple(elems)) => {
                let mut acc = (**l).clone();
                for e in elems {
                    acc = Self::join(&acc, e);
                }
                InferredType::List(Box::new(acc))
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
/// generics beyond `List`/`Dict`, multi-segment custom paths whose
/// alias prefix isn't in the workspace import index).
///
/// Use [`infer_from_type_node_with_imports`] when a
/// [`WorkspaceImportIndex`] is in scope so `pkg.User` lifts to
/// `Schema("User")` instead of falling back to `Any`.
pub(crate) fn infer_from_type_node(t: &TypeNode) -> InferredType {
    infer_from_type_node_with_imports(t, None)
}

/// v1.8 (cross-module schema): same as
/// [`infer_from_type_node`] but consults the workspace import
/// index. A two-segment `path[0].path[1]` whose head is a known
/// alias and whose tail is one of that alias's exported schema
/// names lifts to `InferredType::Schema(path[1])`.
pub(crate) fn infer_from_type_node_with_imports(
    t: &TypeNode,
    imports: Option<&WorkspaceImportIndex>,
) -> InferredType {
    let recurse = |inner: &TypeNode| infer_from_type_node_with_imports(inner, imports);
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
                    .map(&recurse)
                    .unwrap_or(InferredType::Any);
                InferredType::List(Box::new(inner))
            }
            "Dict" => {
                let val = t.generics.get(1).map(&recurse).unwrap_or(InferredType::Any);
                InferredType::Dict(Box::new(val))
            }
            // v1.7: `Closure<T1, ..., Tn, Ret>` lifts to a structured
            // `Fn` shape (last generic is the return type, the rest
            // are param types). This is what stdlib `_list_map` /
            // `_list_filter` write. Bare `Closure` / `Fn` (no
            // generics) is rejected at source by the `BareGeneric`
            // walker, so we don't need a degenerate fallback here —
            // an unannotated zero-generic shape now means the user
            // wrote a multi-segment custom path that happens to end
            // in `Closure`, which we treat as an opaque Schema below.
            "Closure" | "Fn" if !t.generics.is_empty() => {
                let (params, ret) = t.generics.split_at(t.generics.len() - 1);
                let param_tys: Vec<InferredType> = params.iter().map(&recurse).collect();
                let ret_ty = recurse(&ret[0]);
                InferredType::Fn(param_tys, Box::new(ret_ty))
            }
            // v1.7: lift a `Tuple<T1, T2, ...>` TypeNode (parser
            // encoding for `(T1, T2, ...)`) into a structured
            // `InferredType::Tuple` so element-by-element subsumption
            // works.
            "Tuple" => {
                let elems: Vec<InferredType> = t.generics.iter().map(&recurse).collect();
                InferredType::Tuple(elems)
            }
            // v1.8: `Enum<alt1, alt2, ...>` lifts into the join of
            // all alternatives so the value-side type is as precise
            // as the alternatives allow:
            //   - `Enum<"up", "down">` → all alts are `String`
            //     literals, join is `String`.
            //   - `Enum<Int, Float>` → join is `Number`.
            //   - heterogeneous (`Enum<Int, String>`) → join is `Any`.
            "Enum" if !t.generics.is_empty() => t
                .generics
                .iter()
                .map(enum_alt_value_type)
                .reduce(|acc, ty| InferredType::join(&acc, &ty))
                .unwrap_or(InferredType::Any),
            // v1.7: bare `Closure` / `Fn` / `Enum` reach this arm
            // only if the source-level ban-bare walker hasn't fired
            // yet (analyzer pre-passes). Collapse to the internal
            // `Any` placeholder rather than fabricating a phony
            // schema — the diagnostic surfaces independently.
            "Closure" | "Fn" | "Enum" => InferredType::Any,
            other => InferredType::Schema(other.to_string()),
        },
        // v1.8 / v1.8e cross-module: `pkg.User` lifts to the
        // *qualified* `Schema("pkg.User")` (not the bare `User`) so
        // two aliases of `User` from different libs are
        // distinguishable. The schema-index merge key is the same
        // qualified string, so lookups round-trip cleanly.
        [head, tail] => {
            if let Some(qualified) = cross_module_schema(head, tail, imports) {
                InferredType::Schema(qualified)
            } else {
                InferredType::Any
            }
        }
        _ => InferredType::Any,
    };
    if t.is_optional {
        InferredType::Optional(Box::new(base))
    } else {
        base
    }
}

/// v1.8: classify a single `Enum<...>` alternative as the
/// `InferredType` of any *value* that could match it. Used by
/// `infer_from_type_node`'s `"Enum"` arm to compute a value-side
/// upper bound for the whole enum slot.
///
/// The parser strips quotes from string-literal alternatives (so
/// `Enum<"up", "down">` and `Enum<Active, Inactive>` both parse as
/// `[TypeNode { path: ["up"|"Active"], ... }, ...]`). Without the
/// schema index here we treat every single-segment bareword
/// alternative that isn't a built-in primitive as a `String`
/// candidate — matching the runtime's cheap-path policy in
/// `enum_alt_matches_cheaply`.
fn enum_alt_value_type(t: &TypeNode) -> InferredType {
    if t.path.len() == 1 && t.generics.is_empty() {
        let head = t.path[0].as_str();
        if !is_known_builtin_alt(head) {
            // Either a quoted string literal (parser-stripped) or a
            // schema/variant bareword. Either way, runtime accepts
            // String values via the cheap path.
            return InferredType::String;
        }
    }
    infer_from_type_node(t)
}

/// v1.8 cross-module: returns the qualified schema key
/// `Some("alias.tail")` when `head.tail` resolves through the import
/// index (i.e. `head` is a known alias and `tail` is one of its
/// exported schema names). The qualified name is what
/// `imported_schemas` is keyed by, so `walk_path` / `subsumes_with`
/// look up the right schema even when two imports both export `User`
/// with different fields.
fn cross_module_schema(
    head: &str,
    tail: &str,
    imports: Option<&WorkspaceImportIndex>,
) -> Option<String> {
    let idx = imports?;
    if idx
        .aliased
        .get(head)
        .map(|set| set.contains(tail))
        .unwrap_or(false)
    {
        Some(format!("{head}.{tail}"))
    } else {
        None
    }
}

fn is_known_builtin_alt(s: &str) -> bool {
    matches!(
        s,
        "Any"
            | "Null"
            | "Bool"
            | "Int"
            | "Float"
            | "Number"
            | "String"
            | "List"
            | "Dict"
            | "Closure"
            | "Fn"
    )
}

/// v1.8: per-alternative subsumption for an `Enum<...>` slot.
/// Mirrors the runtime's cheap-then-structural cascade. Returns
/// `true` if the inferred value type `actual` is statically
/// compatible with the alternative `alt`.
fn enum_alt_matches(
    actual: &InferredType,
    alt: &TypeNode,
    bases: Option<&SchemaBaseIndex>,
) -> bool {
    // Single-segment bareword without generics: either a built-in
    // primitive or a string-literal / schema-name candidate.
    if alt.path.len() == 1 && alt.generics.is_empty() {
        let head = alt.path[0].as_str();
        if !is_known_builtin_alt(head) {
            // String-literal cheap path: only `String` values match.
            return matches!(actual, InferredType::String);
        }
    }
    actual.subsumes_with(alt, bases)
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
                    return Some(infer_from_type_node_with_imports(
                        t,
                        self.tree.and_then(|t| t.workspace_import_index.as_ref()),
                    ));
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
                        return Some(infer_from_type_node_with_imports(
                            hint,
                            self.tree.and_then(|t| t.workspace_import_index.as_ref()),
                        ));
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
            // v1.7: list literals are inferred as a tuple
            // (`Tuple(t1, t2, ..., tn)`), preserving each element's
            // type. Subsumption against a `List<T>` slot folds the
            // tuple element-by-element; subsumption against a tuple
            // slot checks position-by-position. Empty list literals
            // become the unit tuple `Tuple()` — they still subsume
            // every `List<T>` slot trivially because the
            // "all elements satisfy T" predicate vacuously holds.
            //
            // Pre-v1.7 list literals collapsed to `List<join(...)>`.
            // The collapse happened too early and lost information
            // (e.g. `[1, "x"]` → `List<Any>`); the tuple-first
            // strategy keeps the per-position type until subsumption
            // forces a decision.
            let elems: Vec<InferredType> = items
                .iter()
                .map(|item| infer_type(item, scope).unwrap_or(InferredType::Any))
                .collect();
            Some(InferredType::Tuple(elems))
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
        Expr::Variable(path) => infer_path_inferred(path, scope),
        Expr::Reference { path, .. } => infer_path_inferred(path, scope),
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
                Some(rt) => infer_from_type_node_with_imports(
                    rt,
                    scope.tree.and_then(|t| t.workspace_import_index.as_ref()),
                ),
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
            // Convert the path into a Vec<String> so we can route both
            // single-segment and multi-segment names through the same
            // path-aware lookup. Stops at the first non-String segment
            // (dynamic / spread keys can't form a callable name).
            let mut name_path: Vec<String> = Vec::with_capacity(path.len());
            for seg in path {
                match seg {
                    TokenKey::String(s, _, _) => name_path.push(s.clone()),
                    _ => return None,
                }
            }
            if name_path.is_empty() {
                return None;
            }
            let tree = scope.tree?;
            // v1.5: try the path-aware lookup first so `alias.method`
            // (cross-module) and other multi-segment forms reach
            // `lookup_signature_path`. Single-segment paths still pick
            // up sibling closure / host fn / stdlib signatures via the
            // legacy entry point.
            let sig = if name_path.len() == 1 {
                lookup_signature(&name_path[0], tree, &tree.host_fn_signatures)?
            } else {
                crate::sig::lookup_signature_path(&name_path, tree, &tree.host_fn_signatures)?
            };
            let imports = scope.tree.and_then(|t| t.workspace_import_index.as_ref());
            if sig.generics.is_empty() {
                return Some(infer_from_type_node_with_imports(&sig.return_type, imports));
            }
            let bindings = crate::generics::collect_bindings(&sig, args, scope);
            let instantiated = instantiate(&sig, &bindings);
            Some(infer_from_type_node_with_imports(
                &instantiated.return_type,
                imports,
            ))
        }
        // v1.5: Spread used as a standalone expression evaluates to the
        // inner value (the surrounding dict / list arm only uses the
        // spread shape via TokenKey::Spread; the Expr::Spread case is
        // when a spread node sits in an expression-position slot like
        // an arg). Mirrors the runtime's `Expr::Spread(inner) =>
        // self.eval(inner, ...)`.
        Expr::Spread(inner) => infer_type(inner, scope),
        // v1.5: list-comprehension `[elem for x in iterable if cond]`
        // produces `List<element_type>`. We infer the iterable, peel
        // off `List<T>` (or `Dict<V>` → values are `V`), seed the
        // inferred element type into a child scope keyed by the
        // comprehension's binding name, and infer the element body
        // there. Anything we can't classify falls back to `Any`
        // element so callers still see a well-formed `List<...>`.
        Expr::Comprehension {
            element,
            id,
            iterable,
            condition: _,
        } => {
            let iter_ty = infer_type(iterable, scope).unwrap_or(InferredType::Any);
            let item_ty = match iter_ty {
                InferredType::List(t) => *t,
                InferredType::Dict(v) => *v,
                // v1.8+ fix: list literals now infer as `Tuple(...)`
                // (v1.7 change), but a comprehension iterating over a
                // list literal still wants the per-element type. Fold
                // the tuple element types via `join` so
                // `[x*x for x in [1,2,3]]` derives `x: Int` instead of
                // `x: Any` (which used to leak past strict checks and
                // return-type inference). Empty tuple → `Any` (caller
                // will error on iterating an empty literal anyway).
                InferredType::Tuple(elems) => {
                    if elems.is_empty() {
                        InferredType::Any
                    } else {
                        elems
                            .into_iter()
                            .reduce(|acc, t| InferredType::join(&acc, &t))
                            .unwrap_or(InferredType::Any)
                    }
                }
                // The runtime's comprehension demands a `List` iter
                // (and the static walker will surface a mismatch
                // separately). For inference, we keep the binding
                // typed `Any` so the body still infers something
                // reasonable.
                _ => InferredType::Any,
            };
            let mut child_locals = scope.locals.clone();
            child_locals.insert(id.clone(), item_ty);
            let child = TypeScope {
                locals: child_locals,
                schemas: scope.schemas,
                frames: scope.frames.clone(),
                tree: scope.tree,
            };
            let elem_ty = infer_type(element, &child).unwrap_or(InferredType::Any);
            Some(InferredType::List(Box::new(elem_ty)))
        }
        // v1.5: `expr where { k1: v1, k2: v2 }` — bindings is always a
        // dict literal (parser-enforced). Infer each binding's value,
        // seed them into a child scope, and infer `expr` there. The
        // result type is the body's type.
        Expr::Where { expr, bindings } => {
            let mut child_locals = scope.locals.clone();
            if let Expr::Dict(pairs) = &*bindings.expr {
                for (key, value) in pairs {
                    if let TokenKey::String(name, _, _) = key {
                        // Each binding's value type seeds a local.
                        // Falling back to `Any` keeps the body's
                        // inference well-formed; strict-mode callers
                        // see the failure via the body inference path.
                        let val_ty = if let Some(t) = &value.type_hint {
                            infer_from_type_node_with_imports(
                                t,
                                scope.tree.and_then(|t| t.workspace_import_index.as_ref()),
                            )
                        } else {
                            infer_type(value, scope).unwrap_or(InferredType::Any)
                        };
                        child_locals.insert(name.clone(), val_ty);
                    }
                }
            }
            let child = TypeScope {
                locals: child_locals,
                schemas: scope.schemas,
                frames: scope.frames.clone(),
                tree: scope.tree,
            };
            infer_type(expr, &child)
        }
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

/// v1.4: collect the dotted-segment names of a `Variable` / `Reference`
/// path. Stops at the first non-`String` segment (Index / Dynamic /
/// Spread) — those aren't representable as a "field name chain", so we
/// surface what we have so far and the caller decides how to handle the
/// remainder.
pub(crate) fn path_segments(path: &[TokenKey]) -> Vec<String> {
    let mut out = Vec::with_capacity(path.len());
    for seg in path {
        if let TokenKey::String(s, _, _) = seg {
            out.push(s.clone());
        } else {
            break;
        }
    }
    out
}

/// v1.8 tuple-position access: walker-friendly view of a path that
/// preserves Index segments alongside named ones. Stops at the
/// first segment we can't statically classify (`Dynamic` /
/// `Spread`).
#[derive(Debug, Clone)]
enum WalkSeg {
    Name(String),
    Index(usize),
}

fn walk_segments(path: &[TokenKey]) -> Vec<WalkSeg> {
    let mut out = Vec::with_capacity(path.len());
    for seg in path {
        match seg {
            TokenKey::String(s, _, _) => out.push(WalkSeg::Name(s.clone())),
            TokenKey::Index(i, _) => out.push(WalkSeg::Index(*i)),
            _ => break,
        }
    }
    out
}

/// Schema-rooted §J follow-up: rewrite a single-segment user-schema
/// `TypeNode` so its head carries the importer's alias prefix —
/// `User` (recorded inside `lib_with_value.relon`) becomes
/// `lib.User` when read through `#import lib`. Builtin / prelude
/// names are left untouched so primitive types and generic
/// containers don't sprout phantom alias prefixes.
///
/// This is what makes `aliased_values[alias][field]` lifts land on
/// the same qualified schema key (`alias.Name`) that
/// `build_schema_index` already merged from `imported_schemas`.
/// Without it, the bare `User` would lift to `Schema("User")`,
/// `walk_path`'s mid-step schema lookup would miss the importer's
/// `lib.User` entry, and the rest of the chain would surface as
/// `UnknownStep`.
fn qualify_type_node_for_alias(hint: &TypeNode, alias: &str) -> TypeNode {
    if hint.path.len() == 1
        && hint.generics.is_empty()
        && !is_known_builtin_alt(hint.path[0].as_str())
    {
        let mut qualified = hint.clone();
        qualified.path = vec![alias.to_string(), hint.path[0].clone()];
        qualified
    } else {
        hint.clone()
    }
}

/// Outcome of walking the tail of a `Variable` / `Reference` path under
/// the inference engine. Lets callers distinguish "fully resolved" from
/// "head was found but a middle segment is opaque" — the latter being
/// the precise shape strict mode wants to flag.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PathTailOutcome {
    /// Walk completed; the final type is `ty`. `ty` may itself be
    /// `Any` when the user explicitly typed something as `Any`, but
    /// every intermediate hop succeeded.
    Resolved(InferredType),
    /// Head resolved but a later segment couldn't be classified (e.g.
    /// the schema isn't visible, or the running type doesn't admit
    /// nested fields). `at_segment` is the 0-based index into `path`
    /// of the failing segment; `running_name` is the human-readable
    /// type-name we had at the point of failure.
    UnknownStep {
        at_segment: usize,
        running_name: String,
    },
    /// Path head itself isn't visible in the active scope. Strict mode
    /// turns this into `UnknownReferenceType`; non-strict callers fall
    /// back to `Any`.
    UnknownHead,
}

/// Walk an arbitrary dotted path under `scope`, starting from the type
/// returned by `scope.lookup(path[0])` and descending one segment at a
/// time:
///
/// * `Schema(name)` → segment must name a declared field of that
///   schema (looked up in `scope.schemas`); the walk continues with
///   the field's declared type.
/// * `Dict(value_ty)` → every key has the same value type, so the
///   segment is structurally fine and the walk continues with
///   `value_ty`.
/// * `Optional(inner)` → strip the `?` wrapper and try again; matches
///   the runtime's `T? . x` semantics.
/// * `Schema` referring to a name not in the schema index → soft stop
///   (`UnknownStep`), so strict mode reports the user-visible reason.
/// * Anything else (`Int`, `String`, `List<…>`, …) → `UnknownStep`,
///   because a non-schema, non-dict head can't have nested fields.
///
/// Recursion depth is naturally bounded by `path.len()` (each iteration
/// strips one segment), so the walk terminates regardless of schema
/// shape.
pub(crate) fn walk_path(path: &[TokenKey], scope: &TypeScope) -> PathTailOutcome {
    let segs = walk_segments(path);
    let Some(WalkSeg::Name(head)) = segs.first() else {
        // Path empty, or starts with an index — not a valid path
        // shape (the parser only produces Index segments after a
        // String head).
        return PathTailOutcome::UnknownHead;
    };
    // Schema-rooted §J follow-up: 2-segment alias-prefixed value
    // lookup. When `head` names a known import alias and the next
    // segment matches a value field exported by that alias's module,
    // synthesize the walking-current type from `aliased_values[alias]
    // [field]` and skip both segments before descending. Without this,
    // `pkg.alice.region` would stop at `scope.lookup("pkg")` (an alias
    // is not a regular binding) and `walk_path` would return
    // `UnknownHead`, silently leaking the rest of the chain to `Any`
    // through `infer_path_inferred`.
    //
    // The value's type-hint TypeNode (`User`) is recorded under the
    // exporter's namespace — bare schema names that the importer's
    // own `tree.schemas` doesn't carry. Before lifting we splice the
    // alias prefix onto single-segment schema paths so the result
    // lands on the same qualified key (`lib.User`) that
    // `build_schema_index` merged from `imported_schemas`. Builtin
    // primitives stay un-qualified.
    let mut start_offset = 0usize;
    let mut current: InferredType;
    let alias_value_resolved = if segs.len() >= 2 {
        if let WalkSeg::Name(field) = &segs[1] {
            scope
                .tree
                .and_then(|t| t.workspace_import_index.as_ref())
                .and_then(|idx| idx.aliased_values.get(head))
                .and_then(|values| values.get(field))
                .map(|hint| {
                    let qualified = qualify_type_node_for_alias(hint, head);
                    infer_from_type_node_with_imports(
                        &qualified,
                        scope.tree.and_then(|t| t.workspace_import_index.as_ref()),
                    )
                })
        } else {
            None
        }
    } else {
        None
    };
    if let Some(ty) = alias_value_resolved {
        current = ty;
        start_offset = 1;
    } else {
        let Some(looked_up) = scope.lookup(head) else {
            return PathTailOutcome::UnknownHead;
        };
        current = looked_up;
    }
    for (offset, seg) in segs[1 + start_offset..].iter().enumerate() {
        // Re-base offset so `at_segment` indices reported in
        // `UnknownStep` line up with the *original* `path` —
        // callers (strict-mode diagnostics) read `at_segment` to
        // pluck the failing source name, which is unaffected by the
        // alias-prefix shortcut. After the shortcut, the loop body
        // is checking `segs[1 + start_offset + offset]`, so the
        // original-index is `1 + start_offset + offset`.
        let at_segment = 1 + start_offset + offset;
        // Strip Optional wrappers before stepping, so `Maybe<T> . x`
        // is checked against `T`'s field set.
        if let InferredType::Optional(inner) = current {
            current = *inner;
        }
        match (current.clone(), seg) {
            (InferredType::Any, _) => {
                // After v1.6 ban-`Any` and v1.7 ban-bare-generic, the
                // only path-head that can still land here is a closure
                // parameter without a `type_hint` under non-strict
                // mode (strict raises `StrictForbidsUntypedClosureParam`
                // and never reaches the walker). Propagate `Any` so
                // non-strict callers continue to defer to runtime.
                return PathTailOutcome::Resolved(InferredType::Any);
            }
            (InferredType::Schema(schema_name), WalkSeg::Name(name)) => {
                let Some(schemas) = scope.schemas else {
                    return PathTailOutcome::UnknownStep {
                        at_segment,
                        running_name: schema_name,
                    };
                };
                let Some(fields) = schemas.get(&schema_name) else {
                    return PathTailOutcome::UnknownStep {
                        at_segment,
                        running_name: schema_name,
                    };
                };
                let Some(field_ty) = fields.get(name) else {
                    return PathTailOutcome::UnknownStep {
                        at_segment,
                        running_name: schema_name,
                    };
                };
                current = infer_from_type_node_with_imports(
                    field_ty,
                    scope.tree.and_then(|t| t.workspace_import_index.as_ref()),
                );
            }
            (InferredType::Dict(value_ty), WalkSeg::Name(_)) => {
                current = *value_ty;
            }
            // v1.8: positional access on a Tuple. `pair.0` /
            // `pair.1` produce the i-th element's type; out-of-
            // range indices surface as `UnknownStep` so strict mode
            // reports the user-visible reason.
            (InferredType::Tuple(elems), WalkSeg::Index(i)) => {
                let arity = elems.len();
                if let Some(elem) = elems.into_iter().nth(*i) {
                    current = elem;
                } else {
                    return PathTailOutcome::UnknownStep {
                        at_segment,
                        running_name: format!("Tuple of arity {arity}"),
                    };
                }
            }
            (InferredType::Tuple(elems), WalkSeg::Name(_)) => {
                // Tuples are positional — stepping by name is a hard
                // failure. (Use `pair.0` instead of `pair.first`.)
                return PathTailOutcome::UnknownStep {
                    at_segment,
                    running_name: InferredType::Tuple(elems).name(),
                };
            }
            // v1.8: positional access on a List yields its element
            // type. Out-of-range indices can't be statically rejected
            // (the literal length isn't tracked here), so we accept
            // and let runtime own the bounds check.
            (InferredType::List(elem), WalkSeg::Index(_)) => {
                current = *elem;
            }
            (other, _) => {
                // Int/String/Bool/Closure/Variant/etc. don't have
                // user-visible nested fields, and a tuple wasn't
                // matched by the more specific arms above.
                return PathTailOutcome::UnknownStep {
                    at_segment,
                    running_name: other.name(),
                };
            }
        }
    }
    PathTailOutcome::Resolved(current)
}

/// Convenience wrapper for callers that don't care *why* the walk
/// stopped — only "what type did we end up with, if any". `UnknownStep`
/// and `UnknownHead` both collapse to `None` so the caller falls back
/// to whatever its own "uninferrable" branch does (typically `Any`).
fn infer_path_inferred(path: &[TokenKey], scope: &TypeScope) -> Option<InferredType> {
    match walk_path(path, scope) {
        PathTailOutcome::Resolved(t) => Some(t),
        PathTailOutcome::UnknownStep { .. } => Some(InferredType::Any),
        PathTailOutcome::UnknownHead => None,
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

    // ============= v1.4 path-tail walker =============

    /// `path_segments` returns every leading String segment.
    #[test]
    fn v1_4_path_segments_strings_only() {
        let path = vec![
            TokenKey::String("o".to_string(), Default::default(), false),
            TokenKey::String("id".to_string(), Default::default(), false),
        ];
        assert_eq!(path_segments(&path), vec!["o", "id"]);
    }

    /// `path_segments` stops at the first non-String segment.
    #[test]
    fn v1_4_path_segments_stops_at_dynamic() {
        use relon_parser::Node;
        let path = vec![
            TokenKey::String("o".to_string(), Default::default(), false),
            TokenKey::Dynamic(
                Node::new(Expr::Int(0), relon_parser::TokenRange::default()),
                false,
            ),
        ];
        assert_eq!(path_segments(&path), vec!["o"]);
    }

    /// `walk_path` returns `UnknownHead` for an unbound name.
    #[test]
    fn v1_4_walk_path_unknown_head() {
        let tree = analyze_str(r#"{ x: 1 }"#);
        let schemas = SchemaIndex::new();
        let scope = TypeScope::new(&tree, &schemas);
        let path = vec![TokenKey::String(
            "missing".to_string(),
            Default::default(),
            false,
        )];
        assert_eq!(walk_path(&path, &scope), PathTailOutcome::UnknownHead);
    }

    /// `walk_path` resolves a single-segment binding to its declared
    /// type via the scope's frames.
    #[test]
    fn v1_4_walk_path_single_seg_via_frame() {
        // Using a `#main(Int n)` so the resolver builds a synthetic
        // root frame populating `n` as `Int`.
        let tree = analyze_str(
            r#"
            #main(Int n) -> Int
            n
            "#,
        );
        let schemas = SchemaIndex::new();
        let mut scope = TypeScope::new(&tree, &schemas);
        scope.locals.insert("n".to_string(), InferredType::Int);
        let path = vec![TokenKey::String("n".to_string(), Default::default(), false)];
        assert_eq!(
            walk_path(&path, &scope),
            PathTailOutcome::Resolved(InferredType::Int)
        );
    }

    /// `walk_path` reports `UnknownStep` when a Schema head is missing
    /// the requested field.
    #[test]
    fn v1_4_walk_path_schema_missing_field() {
        let tree = analyze_str(
            r#"
            #schema Order { Int id: * }
            #main(Order o) -> Int
            o.id
            "#,
        );
        let schemas = crate::typecheck::build_schema_index(&tree);
        let mut scope = TypeScope::new(&tree, &schemas);
        scope
            .locals
            .insert("o".to_string(), InferredType::Schema("Order".to_string()));
        let path = vec![
            TokenKey::String("o".to_string(), Default::default(), false),
            TokenKey::String("nope".to_string(), Default::default(), false),
        ];
        match walk_path(&path, &scope) {
            PathTailOutcome::UnknownStep { at_segment, .. } => assert_eq!(at_segment, 1),
            other => panic!("expected UnknownStep, got {other:?}"),
        }
    }

    /// `walk_path` flows through a `Dict<String, T>` head, returning
    /// the value type for any key step.
    #[test]
    fn v1_4_walk_path_dict_value() {
        let tree = analyze_str(r#"{ x: 1 }"#);
        let schemas = SchemaIndex::new();
        let mut scope = TypeScope::new(&tree, &schemas);
        scope.locals.insert(
            "kv".to_string(),
            InferredType::Dict(Box::new(InferredType::Int)),
        );
        let path = vec![
            TokenKey::String("kv".to_string(), Default::default(), false),
            TokenKey::String("foo".to_string(), Default::default(), false),
        ];
        assert_eq!(
            walk_path(&path, &scope),
            PathTailOutcome::Resolved(InferredType::Int)
        );
    }

    /// `walk_path` strips an Optional wrapper before stepping into the
    /// inner schema.
    #[test]
    fn v1_4_walk_path_optional_strip() {
        let tree = analyze_str(
            r#"
            #schema Customer { String name: * }
            { x: 1 }
            "#,
        );
        let schemas = crate::typecheck::build_schema_index(&tree);
        let mut scope = TypeScope::new(&tree, &schemas);
        scope.locals.insert(
            "c".to_string(),
            InferredType::Optional(Box::new(InferredType::Schema("Customer".to_string()))),
        );
        let path = vec![
            TokenKey::String("c".to_string(), Default::default(), false),
            TokenKey::String("name".to_string(), Default::default(), false),
        ];
        assert_eq!(
            walk_path(&path, &scope),
            PathTailOutcome::Resolved(InferredType::String)
        );
    }

    /// `walk_path` returns `UnknownStep` when descending into a leaf
    /// type (Int has no nested fields).
    #[test]
    fn v1_4_walk_path_descend_into_leaf() {
        let tree = analyze_str(r#"{ x: 1 }"#);
        let schemas = SchemaIndex::new();
        let mut scope = TypeScope::new(&tree, &schemas);
        scope.locals.insert("n".to_string(), InferredType::Int);
        let path = vec![
            TokenKey::String("n".to_string(), Default::default(), false),
            TokenKey::String("something".to_string(), Default::default(), false),
        ];
        match walk_path(&path, &scope) {
            PathTailOutcome::UnknownStep { running_name, .. } => assert_eq!(running_name, "Int"),
            other => panic!("expected UnknownStep, got {other:?}"),
        }
    }

    /// `walk_path` propagates `Any` once encountered — strict-mode
    /// callers see `Resolved(Any)` and decide whether to flag.
    #[test]
    fn v1_4_walk_path_any_short_circuits() {
        let tree = analyze_str(r#"{ x: 1 }"#);
        let schemas = SchemaIndex::new();
        let mut scope = TypeScope::new(&tree, &schemas);
        scope.locals.insert("x".to_string(), InferredType::Any);
        let path = vec![
            TokenKey::String("x".to_string(), Default::default(), false),
            TokenKey::String("y".to_string(), Default::default(), false),
        ];
        assert_eq!(
            walk_path(&path, &scope),
            PathTailOutcome::Resolved(InferredType::Any)
        );
    }

    // ============= v1.5 inference upgrades =============

    /// v1.5: `Expr::Spread(inner)` infers as the inner's type.
    #[test]
    fn v1_5_spread_inferes_inner_type() {
        let tree = analyze_str(r#"{ x: 1 }"#);
        let schemas = SchemaIndex::new();
        let scope = TypeScope::new(&tree, &schemas);
        let inner = relon_parser::Node::new(Expr::Int(7), relon_parser::TokenRange::default());
        let spread =
            relon_parser::Node::new(Expr::Spread(inner), relon_parser::TokenRange::default());
        assert_eq!(infer_type(&spread, &scope), Some(InferredType::Int));
    }

    /// v1.5: `Expr::Comprehension` infers `List<elem>`. Element body
    /// `id` (binding name) refers to the iterable's element type.
    #[test]
    fn v1_5_comprehension_list_int() {
        let tree = analyze_str(
            r#"
            #main(Int n) -> List<Int>
            [x for x in range(n)]
            "#,
        );
        // The pre-flight check should not flag a return mismatch —
        // i.e. the body's type infers cleanly as `List<Int>`.
        let mm = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, crate::Diagnostic::MainReturnTypeMismatch { .. }))
            .count();
        assert_eq!(mm, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5: `Expr::Where` infers from the body in a scope extended
    /// with the bindings — `(n + 1) where { n: x }` infers as the
    /// body type.
    #[test]
    fn v1_5_where_uses_binding_scope() {
        let tree = analyze_str(
            r#"
            #main(Int x) -> Int
            (n + 1) where { n: x }
            "#,
        );
        let mm = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, crate::Diagnostic::MainReturnTypeMismatch { .. }))
            .count();
        assert_eq!(mm, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5: FnCall multi-segment alias.method routes through
    /// `lookup_signature_path`. The single-segment fast-path stays
    /// behaviorally identical.
    #[test]
    fn v1_5_fncall_single_seg_unchanged() {
        // `range` is a stdlib name — single-segment path goes through
        // `lookup_signature` exactly as in v1.4.
        let tree = analyze_str(
            r#"
            #main(Int n) -> List<Int>
            range(n)
            "#,
        );
        let mm = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, crate::Diagnostic::MainReturnTypeMismatch { .. }))
            .count();
        assert_eq!(mm, 0, "{:?}", tree.diagnostics);
    }
}
