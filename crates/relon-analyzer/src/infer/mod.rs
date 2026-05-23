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

mod walk;

#[cfg(test)]
mod tests;

use walk::infer_path_inferred;
pub(crate) use walk::{path_segments, walk_path, PathTailOutcome};

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

pub(super) fn is_known_builtin_alt(s: &str) -> bool {
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
    /// Outer scope frames, walked outward when `locals` misses. Built
    /// as a stack of borrowed locals maps so entering a child scope
    /// only clones the (small, pointer-sized) chain instead of the
    /// parent's full locals HashMap. Innermost-parent last.
    pub(crate) parent_locals: Vec<&'a HashMap<String, InferredType>>,
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
            parent_locals: Vec::new(),
            schemas: Some(schemas),
            frames: Vec::new(),
            tree: Some(tree),
        }
    }

    /// Build a child scope inheriting the parent's visibility but
    /// seeded with only `new_locals`. The parent's `locals` is
    /// stitched in by reference, so the per-recursion allocation is
    /// bounded by the new bindings instead of cloning the whole
    /// parent map (hot path for nested closures / comprehensions /
    /// where-blocks).
    pub(crate) fn child_with_locals(
        &'a self,
        new_locals: HashMap<String, InferredType>,
    ) -> TypeScope<'a> {
        let mut parent_locals = Vec::with_capacity(self.parent_locals.len() + 1);
        parent_locals.extend(self.parent_locals.iter().copied());
        parent_locals.push(&self.locals);
        TypeScope {
            locals: new_locals,
            parent_locals,
            schemas: self.schemas,
            frames: self.frames.clone(),
            tree: self.tree,
        }
    }

    /// Walk `locals` then the parent chain (innermost-first) looking
    /// for `name`. Mirrors the prior single-map lookup but lets the
    /// chain stand in for a cloned parent HashMap.
    fn lookup_local(&self, name: &str) -> Option<InferredType> {
        if let Some(t) = self.locals.get(name) {
            return Some(t.clone());
        }
        for map in self.parent_locals.iter().rev() {
            if let Some(t) = map.get(name) {
                return Some(t.clone());
            }
        }
        None
    }

    /// Look up `name` against (in order) closure params, dict frames,
    /// and schema names. Returns `None` when nothing matches.
    pub(crate) fn lookup(&self, name: &str) -> Option<InferredType> {
        if let Some(t) = self.lookup_local(name) {
            return Some(t);
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
                    // Reset locals + parent chain — we're inferring a
                    // sibling field's value type, not continuing the
                    // current closure body.
                    let scope = TypeScope {
                        locals: HashMap::new(),
                        parent_locals: Vec::new(),
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
            // Only the new bindings live in the child's owned map; the
            // parent's locals are stitched in by reference via
            // `child_with_locals` so we skip cloning the outer HashMap
            // (hot path for deeply nested closures).
            let mut new_locals = HashMap::with_capacity(params.len());
            for (param, ty) in params.iter().zip(param_types.iter()) {
                new_locals.insert(param.name.clone(), ty.clone());
            }
            let child = scope.child_with_locals(new_locals);
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
            // Seed the comprehension binding into a fresh child frame
            // and chain back to the parent's locals by reference (no
            // outer-map clone).
            let mut new_locals = HashMap::with_capacity(1);
            new_locals.insert(id.clone(), item_ty);
            let child = scope.child_with_locals(new_locals);
            let elem_ty = infer_type(element, &child).unwrap_or(InferredType::Any);
            Some(InferredType::List(Box::new(elem_ty)))
        }
        // v1.5: `expr where { k1: v1, k2: v2 }` — bindings is always a
        // dict literal (parser-enforced). Infer each binding's value,
        // seed them into a child scope, and infer `expr` there. The
        // result type is the body's type.
        Expr::Where { expr, bindings } => {
            // Only the new where-bindings live in the child's owned
            // map; the parent's locals are reused by reference.
            let mut new_locals: HashMap<String, InferredType> = HashMap::new();
            if let Expr::Dict(pairs) = &*bindings.expr {
                new_locals.reserve(pairs.len());
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
                        new_locals.insert(name.clone(), val_ty);
                    }
                }
            }
            let child = scope.child_with_locals(new_locals);
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
