//! Hardcoded static signatures for every stdlib fn registered by
//! `crates/relon-evaluator/src/stdlib.rs::register_to`.
//!
//! Stage 3.2 — keep this list lockstep with the evaluator's
//! `register_fn(...)` calls. A drift-defense test (see bottom of
//! `typecheck.rs::tests`) compares this table against the evaluator's
//! registered names and fails if any name lacks a signature.
//!
//! v1 deliberately models polymorphic fns conservatively:
//! - `len` / `type` accept `Any`; their precise `String∣List∣Dict` union
//!   is not modeled because we have no union shape in `InferredType`
//!   yet. Returns `Int` / `String` respectively.
//! - `_list_*` / `_math_*` polymorphics return `Any` (no generic
//!   instantiation in v1).
//! - `_dict_merge` is variadic over `Dict`.
//! - Validators (`ensure.*`) accept the value as `Any`, return `Any`,
//!   and treat the trailing `message` arg as `optional: true`.

use crate::sig::{type_node_generic, FnParam, FnSignature};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Macro to keep the call sites compact. `tn!(Int)` → `type_node_simple("Int")`.
macro_rules! tn {
    ($name:ident) => {
        $crate::sig::type_node_simple(stringify!($name))
    };
}

/// v1.1 helper: build a single-segment `TypeNode` whose name is
/// listed in the surrounding signature's `generics`. Visually
/// distinct from [`tn!`] at the call site so reviewers can see at a
/// glance which slots are placeholders. Identical encoding —
/// disambiguation happens via [`crate::sig::FnSignature::generics`].
fn tn_var(name: &str) -> relon_parser::TypeNode {
    crate::sig::type_node_simple(name)
}

/// Required-positional parameter helper. `param!("x", tn!(Int))`.
fn param(name: &str, ty: relon_parser::TypeNode) -> FnParam {
    FnParam {
        name: name.to_string(),
        ty,
        optional: false,
    }
}

/// Optional trailing parameter (validator messages).
fn param_opt(name: &str, ty: relon_parser::TypeNode) -> FnParam {
    FnParam {
        name: name.to_string(),
        ty,
        optional: true,
    }
}

fn sig(
    name: &str,
    params: Vec<FnParam>,
    return_type: relon_parser::TypeNode,
    variadic_tail: Option<relon_parser::TypeNode>,
) -> (String, FnSignature) {
    sig_generic(name, Vec::new(), params, return_type, variadic_tail)
}

/// v1.1: variant of [`sig`] for polymorphic stdlib fns. `generics` is
/// the ordered list of placeholder names that may appear inside any
/// `TypeNode` slot (param ty, variadic_tail, return_type). The call
/// site instantiates them via [`crate::sig::instantiate`] after
/// running unification against the actual arg types.
fn sig_generic(
    name: &str,
    generics: Vec<String>,
    params: Vec<FnParam>,
    return_type: relon_parser::TypeNode,
    variadic_tail: Option<relon_parser::TypeNode>,
) -> (String, FnSignature) {
    (
        name.to_string(),
        FnSignature {
            name: name.to_string(),
            generics,
            params,
            return_type,
            variadic_tail,
        },
    )
}

/// Lazy table of every stdlib fn the evaluator registers. Returned by
/// reference so callers don't pay the `HashMap` build cost per lookup.
pub(crate) fn stdlib_signatures() -> &'static HashMap<String, FnSignature> {
    static SIGS: OnceLock<HashMap<String, FnSignature>> = OnceLock::new();
    SIGS.get_or_init(build)
}

fn build() -> HashMap<String, FnSignature> {
    let mut m = HashMap::new();

    // -- builtins always in scope ----------------------------------------
    // `len(value)` — String/List/Dict → Int. Modeled as `Any` since we
    // don't carry union types; runtime reports the shape mismatch.
    let (k, v) = sig("len", vec![param("value", tn!(Any))], tn!(Int), None);
    m.insert(k, v);
    // `_len` is the alias the evaluator also registers.
    let (k, v) = sig("_len", vec![param("value", tn!(Any))], tn!(Int), None);
    m.insert(k, v);

    // `range(stop)` or `range(start, stop)` → List<Int>. v1 expresses this
    // as a 1-arg fixed param + an Int variadic_tail so we don't false-flag
    // the 2-arg form. Strict arity checking would refuse the legal 2-arg
    // call.
    let (k, v) = sig(
        "range",
        vec![param("a", tn!(Int))],
        type_node_generic("List", vec![tn!(Int)]),
        Some(tn!(Int)),
    );
    m.insert(k, v);

    // `type(v)` → String. Accepts anything.
    let (k, v) = sig("type", vec![param("value", tn!(Any))], tn!(String), None);
    m.insert(k, v);

    // -- list intrinsics (polymorphic; v1.1 generics) -----------------
    // `_list_map<T, U>(List<T>, Closure<(T) -> U>) -> List<U>`.
    //
    // The closure slot is encoded as `Closure<T, U>` (positional —
    // last generic is the return slot). The unifier descends into
    // closure-literal args, runs the closure body under a scope
    // where `T` is already bound, and pulls the body's type back out
    // as the binding for `U`. Args that aren't closure literals fall
    // back to the per-arg subsumption check, which accepts any
    // closure shape (Closure / Fn / Any).
    let (k, v) = sig_generic(
        "_list_map",
        vec!["T".into(), "U".into()],
        vec![
            param("list", type_node_generic("List", vec![tn_var("T")])),
            param(
                "f",
                type_node_generic("Closure", vec![tn_var("T"), tn_var("U")]),
            ),
        ],
        type_node_generic("List", vec![tn_var("U")]),
        None,
    );
    m.insert(k, v);
    // `_list_filter<T>(List<T>, Closure<(T) -> Bool>) -> List<T>`.
    let (k, v) = sig_generic(
        "_list_filter",
        vec!["T".into()],
        vec![
            param("list", type_node_generic("List", vec![tn_var("T")])),
            param(
                "f",
                type_node_generic("Closure", vec![tn_var("T"), tn!(Bool)]),
            ),
        ],
        type_node_generic("List", vec![tn_var("T")]),
        None,
    );
    m.insert(k, v);
    // `_list_reduce<T, U>(List<T>, U, Closure<(U, T) -> U>) -> U`.
    // We bind `U` from the `init` arg (arg 1) — that's enough to
    // instantiate the return type when the caller declares
    // `Int s: _list_reduce([1,2,3], 0, ...)`.
    let (k, v) = sig_generic(
        "_list_reduce",
        vec!["T".into(), "U".into()],
        vec![
            param("list", type_node_generic("List", vec![tn_var("T")])),
            param("init", tn_var("U")),
            param(
                "f",
                type_node_generic("Closure", vec![tn_var("U"), tn_var("T"), tn_var("U")]),
            ),
        ],
        tn_var("U"),
        None,
    );
    m.insert(k, v);
    // `_list_contains<T>(List<T>, T) -> Bool`. `T` is bound from
    // arg 0; the per-arg subsumption check then catches a
    // `_list_contains([1,2], "x")` mismatch as `String` vs `Int`.
    let (k, v) = sig_generic(
        "_list_contains",
        vec!["T".into()],
        vec![
            param("list", type_node_generic("List", vec![tn_var("T")])),
            param("needle", tn_var("T")),
        ],
        tn!(Bool),
        None,
    );
    m.insert(k, v);

    // -- string intrinsics ----------------------------------------------
    let (k, v) = sig(
        "_string_split",
        vec![param("s", tn!(String)), param("sep", tn!(String))],
        type_node_generic("List", vec![tn!(String)]),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "_string_join",
        vec![
            param("list", type_node_generic("List", vec![tn!(Any)])),
            param("sep", tn!(String)),
        ],
        tn!(String),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "_string_replace",
        vec![
            param("s", tn!(String)),
            param("from", tn!(String)),
            param("to", tn!(String)),
        ],
        tn!(String),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "_string_upper",
        vec![param("s", tn!(String))],
        tn!(String),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "_string_lower",
        vec![param("s", tn!(String))],
        tn!(String),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "_string_contains",
        vec![param("s", tn!(String)), param("needle", tn!(String))],
        tn!(Bool),
        None,
    );
    m.insert(k, v);

    // -- dict intrinsics ------------------------------------------------
    // `_dict_merge` — at least one Dict; merges any number of trailing
    // Dict arguments into the first. v1 models this as 1 fixed Dict param
    // + Dict variadic_tail.
    let (k, v) = sig(
        "_dict_merge",
        vec![param(
            "base",
            type_node_generic("Dict", vec![tn!(String), tn!(Any)]),
        )],
        type_node_generic("Dict", vec![tn!(String), tn!(Any)]),
        Some(type_node_generic("Dict", vec![tn!(String), tn!(Any)])),
    );
    m.insert(k, v);
    let (k, v) = sig(
        "_dict_keys",
        vec![param(
            "d",
            type_node_generic("Dict", vec![tn!(String), tn!(Any)]),
        )],
        type_node_generic("List", vec![tn!(String)]),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "_dict_values",
        vec![param(
            "d",
            type_node_generic("Dict", vec![tn!(String), tn!(Any)]),
        )],
        type_node_generic("List", vec![tn!(Any)]),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "_dict_has_key",
        vec![
            param("d", type_node_generic("Dict", vec![tn!(String), tn!(Any)])),
            param("key", tn!(String)),
        ],
        tn!(Bool),
        None,
    );
    m.insert(k, v);

    // -- math intrinsics (polymorphic Number; return Any in v1) ----------
    // `_math_abs(n)` — Number → Number. v1 accepts Any to avoid false
    // flagging on `Int` (where `Number` subsumption already holds, but
    // the param check uses `subsumes_with` which wouldn't recognize a
    // declared `Number` slot accepting an `Int` literal as wrong).
    let (k, v) = sig(
        "_math_abs",
        vec![param("n", tn!(Number))],
        tn!(Number),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "_math_max",
        vec![param("a", tn!(Number)), param("b", tn!(Number))],
        tn!(Number),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "_math_min",
        vec![param("a", tn!(Number)), param("b", tn!(Number))],
        tn!(Number),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "_math_clamp",
        vec![
            param("v", tn!(Number)),
            param("lo", tn!(Number)),
            param("hi", tn!(Number)),
        ],
        tn!(Number),
        None,
    );
    m.insert(k, v);

    // -- ensure.* validators --------------------------------------------
    // All `ensure.*` validators take the value first, then their
    // type-specific args, then an optional trailing `message: String`.
    // Return the value unchanged (v1 models as Any for simplicity).
    let value_param = || param("value", tn!(Any));
    let msg_param = || param_opt("message", tn!(String));

    for tname in [
        "ensure.int",
        "ensure.string",
        "ensure.bool",
        "ensure.float",
        "ensure.list",
        "ensure.dict",
    ] {
        let (k, v) = sig(tname, vec![value_param(), msg_param()], tn!(Any), None);
        m.insert(k, v);
    }

    // `ensure.at_least(value, min, message?)`.
    let (k, v) = sig(
        "ensure.at_least",
        vec![value_param(), param("min", tn!(Number)), msg_param()],
        tn!(Any),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "ensure.at_most",
        vec![value_param(), param("max", tn!(Number)), msg_param()],
        tn!(Any),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "ensure.one_of",
        vec![
            value_param(),
            param("allowed", type_node_generic("List", vec![tn!(Any)])),
            msg_param(),
        ],
        tn!(Any),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "ensure.required_fields",
        vec![
            param(
                "dict",
                type_node_generic("Dict", vec![tn!(String), tn!(Any)]),
            ),
            param("fields", type_node_generic("List", vec![tn!(String)])),
            msg_param(),
        ],
        type_node_generic("Dict", vec![tn!(String), tn!(Any)]),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "ensure.requires",
        vec![
            param(
                "dict",
                type_node_generic("Dict", vec![tn!(String), tn!(Any)]),
            ),
            param("field", tn!(String)),
            param("required", tn!(String)),
            msg_param(),
        ],
        type_node_generic("Dict", vec![tn!(String), tn!(Any)]),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig(
        "ensure.fields_equal",
        vec![
            param(
                "dict",
                type_node_generic("Dict", vec![tn!(String), tn!(Any)]),
            ),
            param("left", tn!(String)),
            param("right", tn!(String)),
            msg_param(),
        ],
        type_node_generic("Dict", vec![tn!(String), tn!(Any)]),
        None,
    );
    m.insert(k, v);

    m
}
