//! Precise static signatures for the public `#relaxed` stdlib wrappers
//! authored in `crates/relon-evaluator/src/std_relon/*.relon`
//! (`list.relon`, `dict.relon`, ...).
//!
//! Why this table exists (issue #1, step 1). The stdlib wrappers are
//! ordinary `.relon` closures — e.g. `sum(l): l | _list_reduce(0, ...)`
//! — authored under `#relaxed` with **no return annotation**. The
//! closure-signature extractor
//! ([`crate::typecheck::helpers::extract_closure_signature`]) defaults
//! an un-annotated return to `Any`, so every call through the public
//! surface (`#import list from "std/list"; Int t: list.sum(xs)`)
//! collapses to `Any` and silently short-circuits the caller's typed
//! slot. That is the mechanism behind `Int total: list.sum(xs)`
//! accepting a `String` binding, too.
//!
//! Rather than annotate the `#relaxed` sources (which would change the
//! runtime-owned dynamic contract those closures rely on) or widen the
//! native-fn table in [`crate::stdlib_signatures`] (whose drift test
//! demands its key set equal the evaluator's `register_fn` names, and
//! whose entries the two-segment `alias.method` lookup never reads), we
//! overlay these hand-written signatures onto the *exported closure*
//! table during import-index construction — but **only for `std/`
//! paths**, so a user module that happens to export a `sum` closure is
//! never hijacked.
//!
//! The signatures reuse the same generic machinery as
//! [`crate::stdlib_signatures`]: `<T>`/`<U>` placeholders in `List<T>`,
//! `Closure<T, U>`, etc. are bound at the call site by
//! [`crate::generics::collect_bindings`] and substituted into the
//! return slot by [`crate::sig::instantiate`]. This mirrors the native
//! intrinsics each wrapper forwards to (`_list_map`, `_list_reduce`, …)
//! so a wrapper call types identically to the intrinsic it wraps.

use crate::sig::{type_node_generic, type_node_simple, FnParam, FnSignature};
use relon_parser::TypeNode;
use std::collections::HashMap;
use std::sync::OnceLock;

/// `tn!(Int)` → `type_node_simple("Int")`. Concrete (non-placeholder).
macro_rules! tn {
    ($name:ident) => {
        type_node_simple(stringify!($name))
    };
}

/// Single-segment generic placeholder (`T`, `U`, `V`). Encoded like any
/// other single-segment `TypeNode`; disambiguated from concrete names
/// by the surrounding signature's `generics` list.
fn tvar(name: &str) -> TypeNode {
    type_node_simple(name)
}

/// `List<inner>`.
fn list_of(inner: TypeNode) -> TypeNode {
    type_node_generic("List", vec![inner])
}

/// `Dict<String, value>`.
fn dict_of(value: TypeNode) -> TypeNode {
    type_node_generic("Dict", vec![tn!(String), value])
}

fn param(name: &str, ty: TypeNode) -> FnParam {
    FnParam {
        name: name.to_string(),
        ty,
        optional: false,
    }
}

fn sig(
    name: &str,
    generics: &[&str],
    params: Vec<FnParam>,
    return_type: TypeNode,
) -> (String, FnSignature) {
    (
        name.to_string(),
        FnSignature {
            name: name.to_string(),
            generics: generics.iter().map(|g| g.to_string()).collect(),
            params,
            return_type,
            variadic_tail: None,
        },
    )
}

/// Return the precise wrapper-signature overlay for one `std/*` module
/// path (e.g. `"std/list"`), or `None` for a non-std / unknown path.
/// Keyed by the verbatim import path string (`ModuleImport::path`).
///
/// Only closure names that the module actually exports are overlaid at
/// the call site — [`crate::workspace_build::build_import_index`] guards
/// each override with a `contains_key` check against the real exported
/// closures, so a stale entry here can never inject a phantom method.
pub(crate) fn std_wrapper_signatures(
    module_path: &str,
) -> Option<&'static HashMap<String, FnSignature>> {
    static TABLE: OnceLock<HashMap<&'static str, HashMap<String, FnSignature>>> = OnceLock::new();
    TABLE.get_or_init(build).get(module_path)
}

fn build() -> HashMap<&'static str, HashMap<String, FnSignature>> {
    let mut table = HashMap::new();
    table.insert("std/list", build_list());
    table.insert("std/dict", build_dict());
    table.insert("std/string", build_string());
    table.insert("std/math", build_math());
    table.insert("std/value", build_value());
    table.insert("std/is", build_is());
    table
}

/// `std/list` — mirrors the `_list_*` intrinsics so a wrapper call types
/// identically to the intrinsic it forwards to.
fn build_list() -> HashMap<String, FnSignature> {
    let mut m = HashMap::new();
    let mut ins = |kv: (String, FnSignature)| {
        m.insert(kv.0, kv.1);
    };
    // map(l, f): `<T, U>(List<T>, Closure<(T) -> U>) -> List<U>`.
    ins(sig(
        "map",
        &["T", "U"],
        vec![
            param("l", list_of(tvar("T"))),
            param(
                "f",
                type_node_generic("Closure", vec![tvar("T"), tvar("U")]),
            ),
        ],
        list_of(tvar("U")),
    ));
    // filter(l, f): `<T>(List<T>, Closure<(T) -> Bool>) -> List<T>`.
    ins(sig(
        "filter",
        &["T"],
        vec![
            param("l", list_of(tvar("T"))),
            param(
                "f",
                type_node_generic("Closure", vec![tvar("T"), tn!(Bool)]),
            ),
        ],
        list_of(tvar("T")),
    ));
    // reduce(l, i, f): `<T, U>(List<T>, U, Closure<(U, T) -> U>) -> U`.
    // `U` binds from the `init` arg — enough to instantiate the return.
    ins(sig(
        "reduce",
        &["T", "U"],
        vec![
            param("l", list_of(tvar("T"))),
            param("i", tvar("U")),
            param(
                "f",
                type_node_generic("Closure", vec![tvar("U"), tvar("T"), tvar("U")]),
            ),
        ],
        tvar("U"),
    ));
    // contains(l, i): `<T>(List<T>, T) -> Bool`.
    ins(sig(
        "contains",
        &["T"],
        vec![param("l", list_of(tvar("T"))), param("i", tvar("T"))],
        tn!(Bool),
    ));
    // sum(l): `<T>(List<T>) -> T`. Element type flows to the result —
    // `sum([1,2,3])` is `Int`, `sum([1.0])` is `Float`. This is the
    // core step-1 fix: previously `Any`, so `Int t: list.sum(xs)`
    // silently accepted a `String` slot too.
    ins(sig(
        "sum",
        &["T"],
        vec![param("l", list_of(tvar("T")))],
        tvar("T"),
    ));
    // avg(l): `<T>(List<T>) -> T`. `avg = sum(l) / _len(l)`; relon keeps
    // `Int / Int` as `Int` (infer::binary), and `Float / Int` promotes
    // to `Float`, so the element type round-trips exactly like `sum`.
    ins(sig(
        "avg",
        &["T"],
        vec![param("l", list_of(tvar("T")))],
        tvar("T"),
    ));
    // len(l): `<T>(T) -> Int`. Mirrors the permissive `_len` intrinsic
    // (String / List / Dict all accepted); the placeholder keeps `Any`
    // off the surface without over-constraining the arg to `List`.
    ins(sig("len", &["T"], vec![param("l", tvar("T"))], tn!(Int)));
    // first(l): `<T>(List<T>) -> T` — `l[0]`.
    ins(sig(
        "first",
        &["T"],
        vec![param("l", list_of(tvar("T")))],
        tvar("T"),
    ));
    // last(l): `<T>(List<T>) -> T` — `l[len(l) - 1]`.
    ins(sig(
        "last",
        &["T"],
        vec![param("l", list_of(tvar("T")))],
        tvar("T"),
    ));
    // compact(l): `<T>(List<T>) -> List<T>` — drops `None`, keeps shape.
    ins(sig(
        "compact",
        &["T"],
        vec![param("l", list_of(tvar("T")))],
        list_of(tvar("T")),
    ));
    // flatten(l): `<T>(List<List<T>>) -> List<T>` — one level of nesting
    // removed.
    ins(sig(
        "flatten",
        &["T"],
        vec![param("l", list_of(list_of(tvar("T"))))],
        list_of(tvar("T")),
    ));
    m
}

/// `std/dict` — mirrors the `_dict_*` intrinsics.
fn build_dict() -> HashMap<String, FnSignature> {
    let mut m = HashMap::new();
    let mut ins = |kv: (String, FnSignature)| {
        m.insert(kv.0, kv.1);
    };
    // merge(a, b): `<V>(Dict<String, V>, Dict<String, V>) -> Dict<String, V>`.
    // The wrapper takes exactly two dicts (the intrinsic is variadic).
    ins(sig(
        "merge",
        &["V"],
        vec![
            param("a", dict_of(tvar("V"))),
            param("b", dict_of(tvar("V"))),
        ],
        dict_of(tvar("V")),
    ));
    // keys(d): `<V>(Dict<String, V>) -> List<String>`.
    ins(sig(
        "keys",
        &["V"],
        vec![param("d", dict_of(tvar("V")))],
        list_of(tn!(String)),
    ));
    // values(d): `<V>(Dict<String, V>) -> List<V>` — value type flows.
    ins(sig(
        "values",
        &["V"],
        vec![param("d", dict_of(tvar("V")))],
        list_of(tvar("V")),
    ));
    // has_key(d, k): `<V>(Dict<String, V>, String) -> Bool`.
    ins(sig(
        "has_key",
        &["V"],
        vec![param("d", dict_of(tvar("V"))), param("k", tn!(String))],
        tn!(Bool),
    ));
    m
}

/// `std/string` — mirrors the `_string_*` intrinsics.
fn build_string() -> HashMap<String, FnSignature> {
    let mut m = HashMap::new();
    let mut ins = |kv: (String, FnSignature)| {
        m.insert(kv.0, kv.1);
    };
    // split(s, sep): `(String, String) -> List<String>`.
    ins(sig(
        "split",
        &[],
        vec![param("s", tn!(String)), param("sep", tn!(String))],
        list_of(tn!(String)),
    ));
    // join(l, sep): `<T>(List<T>, String) -> String` — the runtime
    // renders each element, so the element placeholder is unbound.
    ins(sig(
        "join",
        &["T"],
        vec![param("l", list_of(tvar("T"))), param("sep", tn!(String))],
        tn!(String),
    ));
    // replace(s, old, new): `(String, String, String) -> String`.
    ins(sig(
        "replace",
        &[],
        vec![
            param("s", tn!(String)),
            param("old", tn!(String)),
            param("new", tn!(String)),
        ],
        tn!(String),
    ));
    // upper(s) / lower(s): `(String) -> String`.
    ins(sig(
        "upper",
        &[],
        vec![param("s", tn!(String))],
        tn!(String),
    ));
    ins(sig(
        "lower",
        &[],
        vec![param("s", tn!(String))],
        tn!(String),
    ));
    // contains(s, sub): `(String, String) -> Bool`.
    ins(sig(
        "contains",
        &[],
        vec![param("s", tn!(String)), param("sub", tn!(String))],
        tn!(Bool),
    ));
    // glob_match(s, pattern): `(String, String) -> Bool`.
    ins(sig(
        "glob_match",
        &[],
        vec![param("s", tn!(String)), param("pattern", tn!(String))],
        tn!(Bool),
    ));
    m
}

/// `std/math` — `<T>(T, ...) -> T` preserves Int-vs-Float precision the
/// way the branch-based source does (`abs(-5)` is `Int`, `abs(-5.0)` is
/// `Float`); Relon has no numeric trait bound yet, so the placeholder is
/// otherwise unconstrained.
fn build_math() -> HashMap<String, FnSignature> {
    let mut m = HashMap::new();
    let mut ins = |kv: (String, FnSignature)| {
        m.insert(kv.0, kv.1);
    };
    // abs(x): `<T>(T) -> T`.
    ins(sig("abs", &["T"], vec![param("x", tvar("T"))], tvar("T")));
    // max(a, b) / min(a, b): `<T>(T, T) -> T` — returns one of the args.
    ins(sig(
        "max",
        &["T"],
        vec![param("a", tvar("T")), param("b", tvar("T"))],
        tvar("T"),
    ));
    ins(sig(
        "min",
        &["T"],
        vec![param("a", tvar("T")), param("b", tvar("T"))],
        tvar("T"),
    ));
    // clamp(v, min, max): `<T>(T, T, T) -> T`.
    ins(sig(
        "clamp",
        &["T"],
        vec![
            param("v", tvar("T")),
            param("min", tvar("T")),
            param("max", tvar("T")),
        ],
        tvar("T"),
    ));
    m
}

/// `std/value` — `default(v, fallback): v == None ? fallback : v`, so the
/// result is one of the two args. `<T>(T, T) -> T` binds `T` from both
/// (joined); a mixed call degrades to `Any` rather than false-rejecting.
fn build_value() -> HashMap<String, FnSignature> {
    let mut m = HashMap::new();
    let (k, v) = sig(
        "default",
        &["T"],
        vec![param("v", tvar("T")), param("fallback", tvar("T"))],
        tvar("T"),
    );
    m.insert(k, v);
    m
}

/// `std/is` — every predicate returns `Bool`; the inspected value takes
/// any type via the placeholder.
fn build_is() -> HashMap<String, FnSignature> {
    let mut m = HashMap::new();
    let mut ins = |kv: (String, FnSignature)| {
        m.insert(kv.0, kv.1);
    };
    for name in [
        "int", "string", "bool", "float", "list", "dict", "number", "empty",
    ] {
        ins(sig(name, &["T"], vec![param("v", tvar("T"))], tn!(Bool)));
    }
    m
}
