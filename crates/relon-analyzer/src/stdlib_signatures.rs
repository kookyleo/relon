//! Hardcoded static signatures for every stdlib fn registered by
//! `crates/relon-evaluator/src/stdlib.rs::register_to`.
//!
//! Stage 3.2 — keep this list lockstep with the evaluator's
//! `register_fn(...)` calls. A drift-defense test (see bottom of
//! `typecheck.rs::tests`) compares this table against the evaluator's
//! registered names and fails if any name lacks a signature.
//!
//! v1.6 ban-Any policy: every stdlib position that previously declared
//! `Any` as its parameter or return type is now expressed via an
//! *unbound generic placeholder*. Single-arg accept-anything fns (e.g.
//! `len(value)`) declare a single `<T>` placeholder; pass-through
//! validators (e.g. `ensure.int(value, message?)`) declare `<T>` and
//! return `T` so the caller's typed binding picks up the original
//! arg's type. The placeholder is bound at the call site by
//! `crates/relon-analyzer/src/generics.rs::collect_bindings` and
//! substituted into the return slot via
//! `crates/relon-analyzer/src/sig.rs::instantiate`.
//!
//! Concretely:
//! - `len` / `_len` / `type` accept `<T>(T) -> Int|String` — the
//!   placeholder doesn't constrain the arg (Relon has no trait bounds
//!   yet) but keeps `Any` out of the language surface.
//! - `_string_join` accepts `<T>(List<T>, String) -> String`.
//! - `_dict_*` operate on `<V>(Dict<String, V>, ...) -> ...`, returning
//!   `List<V>` / preserved `Dict<String, V>` shape where applicable.
//! - `ensure.*` validators bind `<T>` from the value arg and return
//!   `T` — strictly more informative than the previous `Any` return.
//! - `_dict_merge` is variadic over `Dict<String, V>` (one V across
//!   every Dict).

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

/// Public surface for tooling (autocomplete, completion suggesters):
/// the names of every stdlib fn the evaluator registers, sorted for
/// deterministic output. Signature details stay internal.
pub fn stdlib_fn_names() -> impl Iterator<Item = &'static str> {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    let names = NAMES.get_or_init(|| {
        let mut names: Vec<String> = stdlib_signatures().keys().cloned().collect();
        names.sort();
        names
    });
    names.iter().map(|s| s.as_str())
}

fn build() -> HashMap<String, FnSignature> {
    let mut m = HashMap::new();

    // -- builtins always in scope ----------------------------------------
    // v1.6: `len(value)` — String/List/Dict → Int. Modeled as
    // `<T>(T) -> Int` so `Any` doesn't leak into the language surface.
    // The unbound `T` is unconstrained (Relon has no trait bounds
    // today), but this still beats `Any` because the placeholder is
    // call-site bound and the runtime keeps owning the
    // String/List/Dict shape check.
    let (k, v) = sig_generic(
        "len",
        vec!["T".into()],
        vec![param("value", tn_var("T"))],
        tn!(Int),
        None,
    );
    m.insert(k, v);
    // `_len` is the alias the evaluator also registers.
    let (k, v) = sig_generic(
        "_len",
        vec!["T".into()],
        vec![param("value", tn_var("T"))],
        tn!(Int),
        None,
    );
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

    // v1.6: `type(v)` → String. Same treatment as `len` — accepts any
    // `<T>` without committing to `Any`.
    let (k, v) = sig_generic(
        "type",
        vec!["T".into()],
        vec![param("value", tn_var("T"))],
        tn!(String),
        None,
    );
    m.insert(k, v);

    // Built-in WASI-backed capability primitives — always in scope, no
    // `#import` required. `clock()` reads the wall clock (ns since the
    // Unix epoch); `random()` reads 8 random bytes. Both are nullary,
    // return `Int`, and are capability-gated (`reads_clock` / `uses_rng`)
    // by the implicit gates injected in `capability_check`. The lowering
    // recognises them as `Op::ReadClock` / `Op::ReadRandom`; native
    // lowers to a host runtime helper, wasm to a standard WASI import.
    let (k, v) = sig("clock", vec![], tn!(Int), None);
    m.insert(k, v);
    let (k, v) = sig("random", vec![], tn!(Int), None);
    m.insert(k, v);

    // P-fs Stage 1: `read_file(path: String) -> String` — read a file's
    // UTF-8 contents. Always in scope, no `#import` required. Gated by
    // `reads_fs` (implicit gate injected in `capability_check`). The
    // lowering recognises it as `Op::ReadFile`; native lowers to a host
    // runtime helper (sandboxed to the configured root), wasm to the
    // standard preview1 `path_open`/`fd_read`/`fd_close` fd protocol.
    let (k, v) = sig(
        "read_file",
        vec![param("path", tn!(String))],
        tn!(String),
        None,
    );
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
    // v1.6: `_string_join<T>(List<T>, String) -> String` — the runtime
    // calls `Display`-equivalent on each element; the analyzer doesn't
    // model that constraint, so the placeholder is unbound. Still
    // strictly better than `List<Any>`: `_string_join([1,2], ",")`
    // binds `T → Int` in the return-type pipeline (no behavioral
    // change since the return type is `String`, but uniformly clean).
    let (k, v) = sig_generic(
        "_string_join",
        vec!["T".into()],
        vec![
            param("list", type_node_generic("List", vec![tn_var("T")])),
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
    // v1.6: `_dict_merge<V>(Dict<String, V>, ...Dict<String, V>) ->
    // Dict<String, V>`. One placeholder across every Dict — the
    // unifier collects bindings from each arg's value type, joining
    // them when the arg infers concretely. Mixed-V calls
    // (`_dict_merge({a: 1}, {b: "x"})`) end up with `V → Any` after
    // join, equivalent to the v1.5 behavior; uniform-V calls
    // (`_dict_merge({a: 1}, {b: 2})`) bind `V → Int` and let the
    // caller's typed slot pick that up.
    let (k, v) = sig_generic(
        "_dict_merge",
        vec!["V".into()],
        vec![param(
            "base",
            type_node_generic("Dict", vec![tn!(String), tn_var("V")]),
        )],
        type_node_generic("Dict", vec![tn!(String), tn_var("V")]),
        Some(type_node_generic("Dict", vec![tn!(String), tn_var("V")])),
    );
    m.insert(k, v);
    let (k, v) = sig_generic(
        "_dict_keys",
        vec!["V".into()],
        vec![param(
            "d",
            type_node_generic("Dict", vec![tn!(String), tn_var("V")]),
        )],
        type_node_generic("List", vec![tn!(String)]),
        None,
    );
    m.insert(k, v);
    // v1.6: `_dict_values<V>(Dict<String, V>) -> List<V>` — the value
    // type now flows from input to output. Previously `List<Any>`.
    let (k, v) = sig_generic(
        "_dict_values",
        vec!["V".into()],
        vec![param(
            "d",
            type_node_generic("Dict", vec![tn!(String), tn_var("V")]),
        )],
        type_node_generic("List", vec![tn_var("V")]),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig_generic(
        "_dict_has_key",
        vec!["V".into()],
        vec![
            param(
                "d",
                type_node_generic("Dict", vec![tn!(String), tn_var("V")]),
            ),
            param("key", tn!(String)),
        ],
        tn!(Bool),
        None,
    );
    m.insert(k, v);

    // -- math intrinsics (Number-typed; no Any leak) --------------------
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
    // v1.6: every validator binds `<T>` from the value arg and
    // returns `T`. Pre-v1.6 the return type was `Any`, which
    // collapsed information at the typed binding site
    // (`Int n: ensure.int(x)` lost the `Int` brand because `Any`
    // subsumed it). Now `T` round-trips: `ensure.int(7)` returns
    // `Int`, and the caller's slot stays sharp.
    let value_param_t = || param("value", tn_var("T"));
    let value_param_d = || {
        param(
            "dict",
            type_node_generic("Dict", vec![tn!(String), tn_var("V")]),
        )
    };
    let msg_param = || param_opt("message", tn!(String));

    for tname in [
        "ensure.int",
        "ensure.string",
        "ensure.bool",
        "ensure.float",
        "ensure.list",
        "ensure.dict",
    ] {
        let (k, v) = sig_generic(
            tname,
            vec!["T".into()],
            vec![value_param_t(), msg_param()],
            tn_var("T"),
            None,
        );
        m.insert(k, v);
    }

    // `ensure.at_least(value, min, message?)` — bound: `<T>`.
    let (k, v) = sig_generic(
        "ensure.at_least",
        vec!["T".into()],
        vec![value_param_t(), param("min", tn!(Number)), msg_param()],
        tn_var("T"),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig_generic(
        "ensure.at_most",
        vec!["T".into()],
        vec![value_param_t(), param("max", tn!(Number)), msg_param()],
        tn_var("T"),
        None,
    );
    m.insert(k, v);
    // `ensure.one_of<T>(value, List<T>, message?)` — `T` flows from
    // either the value arg (single placeholder) or the list arg's
    // element type. The unifier joins the two; mismatched calls
    // (`ensure.one_of(1, ["a", "b"])`) collapse to `T → Any` (after
    // join) and behave like the v1.5 pre-strict path.
    let (k, v) = sig_generic(
        "ensure.one_of",
        vec!["T".into()],
        vec![
            value_param_t(),
            param("allowed", type_node_generic("List", vec![tn_var("T")])),
            msg_param(),
        ],
        tn_var("T"),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig_generic(
        "ensure.required_fields",
        vec!["V".into()],
        vec![
            value_param_d(),
            param("fields", type_node_generic("List", vec![tn!(String)])),
            msg_param(),
        ],
        type_node_generic("Dict", vec![tn!(String), tn_var("V")]),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig_generic(
        "ensure.requires",
        vec!["V".into()],
        vec![
            value_param_d(),
            param("field", tn!(String)),
            param("required", tn!(String)),
            msg_param(),
        ],
        type_node_generic("Dict", vec![tn!(String), tn_var("V")]),
        None,
    );
    m.insert(k, v);
    let (k, v) = sig_generic(
        "ensure.fields_equal",
        vec!["V".into()],
        vec![
            value_param_d(),
            param("left", tn!(String)),
            param("right", tn!(String)),
            msg_param(),
        ],
        type_node_generic("Dict", vec![tn!(String), tn_var("V")]),
        None,
    );
    m.insert(k, v);

    m
}
