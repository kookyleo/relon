//! Reserved root-level names — identifiers whose meaning is fixed by
//! the language and must not be shadowed by user dict fields, closure
//! parameters, comprehension binders, or `where`-clause names.
//!
//! These names are checked by the analyzer (which emits a structural
//! diagnostic) and by the evaluator (which would otherwise silently let
//! a local binding mask the predefined value). Keeping them as named
//! constants — rather than scattered string literals — means that
//! adding a new reserved name (`@time`, `@env`, …) is a single-site
//! change with grep-friendly call sites.

/// The push-style external-input root. See `Context::with_input` and
/// `host-integration.md §推荐范式：Push-by-default`.
pub const INPUT: &str = "input";

/// All reserved root-level names. Keep alphabetized.
pub const ALL: &[&str] = &[INPUT];

/// True when `name` is reserved and therefore must not appear as a
/// user-defined dict field, variable, or parameter.
pub fn is_reserved(name: &str) -> bool {
    ALL.contains(&name)
}
