//! Lookup machinery for the bundled stdlib registry.
//!
//! These functions translate user-visible surfaces (function name,
//! `(receiver_type, method_name)`, etc.) into wasm-level callee
//! indices against [`super::registry::builtin_stdlib`]. The IDX
//! constants for cycle-broken internal helpers live in
//! [`super::signatures`] — body builders consult them at construction
//! time without re-entering the registry.
//!
//! See the module-level doc comment in [`super`] for why the
//! declaration order of `builtin_stdlib` is part of the wire format.

use crate::ir::IrType;

use super::registry::builtin_stdlib;

///
/// The index is determined by [`builtin_stdlib`]'s declaration order
/// — see the module-level comment for why that order is part of the
/// wire format.
pub fn stdlib_function_index(name: &str) -> Option<u32> {
    // F-D2-G: lookup only consults the cached name slice — never
    // touches a body, so the eager lookup stays O(N) over the
    // metadata vector without forcing the lazy bodies to build.
    builtin_stdlib()
        .iter()
        .position(|f| f.name == name)
        .map(|i| i as u32)
}

/// Number of bundled stdlib functions. Codegen uses this to compute
/// the wasm-level function index offset for user functions
/// (user-fn index = `stdlib_function_count() + ir_user_func_index`).
pub fn stdlib_function_count() -> u32 {
    builtin_stdlib().len() as u32
}

/// Phase 4.b method-dispatch table: resolve `(receiver_ir_type,
/// method_name)` to the registry index of the stdlib function that
/// implements that method on the given receiver type.
///
/// Distinct from [`stdlib_function_index`] because the same surface
/// method name (e.g. `length`) is implemented by different bundled
/// bodies depending on the receiver type — `String::length` goes
/// through the `length` body (index `0`), while `List<Int>::length`
/// goes through `list_int_length` (index `1`). Free-call form
/// (`length(x)`) still resolves through [`stdlib_function_index`];
/// the receiver-typed dispatch only fires when lowering sees an
/// explicit receiver path.
///
/// Returns `None` for unknown `(ty, name)` pairs; lowering surfaces
/// its own diagnostic.
pub fn stdlib_method_index(receiver_ty: IrType, name: &str) -> Option<u32> {
    match (receiver_ty, name) {
        (IrType::String, "length") => stdlib_function_index("length"),
        (IrType::ListInt, "length") => stdlib_function_index("list_int_length"),
        (IrType::String, "is_empty") => stdlib_function_index("is_empty"),
        // Phase 4.c-2: String / List<Int> method-form dispatch.
        // Free-call form (`concat(a, b)` / `list_int_sum(xs)`) still
        // routes through `stdlib_function_index` directly; method
        // form (`a.concat(b)` / `xs.sum()`) goes through this table
        // so the same surface name resolves against the receiver's
        // IR type.
        (IrType::String, "concat") => stdlib_function_index("concat"),
        (IrType::String, "upper") => stdlib_function_index("upper"),
        (IrType::String, "lower") => stdlib_function_index("lower"),
        // v3++ b-4: word-boundary aware case fold. `s.title()` and the
        // free-call `title(s)` both route here.
        (IrType::String, "title") => stdlib_function_index("title"),
        // v3++ b-5: Unicode normalization (UAX #15). `s.nfc()` /
        // `s.nfd()` / `s.nfkc()` / `s.nfkd()` and the matching
        // free-call forms all dispatch to the shared body builders.
        (IrType::String, "nfc") => stdlib_function_index("nfc"),
        (IrType::String, "nfd") => stdlib_function_index("nfd"),
        (IrType::String, "nfkc") => stdlib_function_index("nfkc"),
        (IrType::String, "nfkd") => stdlib_function_index("nfkd"),
        // v3++ b-6: locale-aware case folding. `s.upper_locale("tr")`
        // and the free-call form `upper_locale(s, "tr")` both route
        // through the same stdlib body.
        (IrType::String, "upper_locale") => stdlib_function_index("upper_locale"),
        (IrType::String, "lower_locale") => stdlib_function_index("lower_locale"),
        (IrType::String, "title_locale") => stdlib_function_index("title_locale"),
        (IrType::String, "substring") => stdlib_function_index("substring"),
        (IrType::String, "starts_with") => stdlib_function_index("starts_with"),
        // F-D7-D: `s.contains(needle)` and the free-call form
        // `contains(s, needle)` both resolve to the same body. The
        // trace recorder short-circuits the call onto
        // `TraceOp::StrContains` via `STDLIB_IDX_CONTAINS = 36`; the
        // tree-walk path stays in `Value`-space (see
        // `relon_evaluator::stdlib::call_method`).
        (IrType::String, "contains") => stdlib_function_index("contains"),
        // 2026-05-21: `s.glob_match(pattern)` and the free-call form
        // `glob_match(s, pattern)` resolve to the same body. The
        // matcher itself lives in `crate::glob::glob_match`; the
        // bundled stdlib slot at index 37 is a routing anchor — see
        // `super::defs::glob_match_string` for the per-backend
        // dispatch matrix.
        (IrType::String, "glob_match") => stdlib_function_index("glob_match"),
        (IrType::ListInt, "sum") => stdlib_function_index("list_int_sum"),
        (IrType::ListInt, "max") => stdlib_function_index("list_int_max"),
        // Phase 10-a higher-order List<Int> methods. Dispatch covers
        // the `xs.map(|x| ...)` / `xs.filter(|x| ...)` /
        // `xs.fold(init, |acc, x| ...)` surfaces.
        (IrType::ListInt, "map") => stdlib_function_index("list_int_map"),
        (IrType::ListInt, "filter") => stdlib_function_index("list_int_filter"),
        (IrType::ListInt, "fold") => stdlib_function_index("list_int_fold"),
        // Phase 10-c length dispatch for the new list types. Each
        // length body just reads the leading `[len: u32 LE]` of the
        // record (all list shapes share the same header), but routes
        // through a distinct stdlib slot so the IR-level param type
        // check stays honest.
        (IrType::ListFloat, "length") => stdlib_function_index("list_float_length"),
        (IrType::ListBool, "length") => stdlib_function_index("list_bool_length"),
        (IrType::ListString, "length") => stdlib_function_index("list_string_length"),
        (IrType::ListSchema, "length") => stdlib_function_index("list_schema_length"),
        (IrType::ListList, "length") => stdlib_function_index("list_list_length"),
        _ => None,
    }
}

/// Phase 10-a: side-table describing the expected closure signature
/// for each `Op::Call` arg slot of a stdlib function. Returns `Some`
/// only for entries where slot `arg_idx` is a `Closure` parameter
/// (so the caller can run free-variable analysis + closure
/// conversion against the matching shape).
///
/// Keyed off the stdlib function's surface name; this stays in
/// `stdlib.rs` so the lowering pass has a single source of truth for
/// closure surfaces.
pub fn stdlib_closure_arg_signature(name: &str, arg_idx: u32) -> Option<(Vec<IrType>, IrType)> {
    match (name, arg_idx) {
        // `xs.map(|x| ...)` — closure param at arg index 1.
        ("list_int_map", 1) => Some((vec![IrType::I64], IrType::I64)),
        // `xs.filter(|x| ...)` — closure param at arg index 1.
        ("list_int_filter", 1) => Some((vec![IrType::I64], IrType::Bool)),
        // `xs.fold(init, |acc, x| ...)` — closure param at arg index 2.
        ("list_int_fold", 2) => Some((vec![IrType::I64, IrType::I64], IrType::I64)),
        _ => None,
    }
}
