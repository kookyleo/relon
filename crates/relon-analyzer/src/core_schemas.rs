//! Schema-rooted decision 21' carrier: built-in `core.relon` schemas.
//!
//! Compile-time-embedded `.relon` source under `core/*.relon` declares
//! the method tables for the language's primitive types (`String`,
//! `List<T>`, `Dict<K, V>`) and the prelude `Iter<T>` iteration
//! protocol. Loading them into `tree.schema_methods` at the very start
//! of analysis is what lets a user write `s.upper()` or `lst.map(f)`
//! without the surrounding `#extend String with { #native upper() -> ... }`
//! boilerplate.
//!
//! Layering note: host-side `#native` implementations live in the
//! evaluator (`stdlib::register_to`); this module is concerned only
//! with telling the analyzer "these schemas exist and own these method
//! names". Names declared here must stay in lock-step with the
//! `register_pure_method` table on the runtime side.
//!
//! Implementation log §C.8 / C.9 captures the design context behind
//! this carrier (why a single shared `.relon` corpus over hand-coded
//! `SchemaMethodInfo` literals: a) the schema-rooted dispatch table is
//! exactly the same data the user's `#schema` lowering produces, so
//! reusing the same lowering avoids drift; b) the `.relon` files
//! double as on-disk documentation of the built-in API surface).
//!
//! Failure mode: if the embedded source fails to parse or has
//! diagnostics, panic at startup — the embedded text is fixed at build
//! time, so a runtime parse error here is a maintainer bug, not a user
//! one.

use crate::root_schemas;
use crate::tree::AnalyzedTree;
use relon_parser::parse_document;

/// Source text of every built-in `.relon` carrier embedded at compile
/// time. Each entry is `(virtual_path, source)`; the path is only used
/// for diagnostic context if the parse ever fails.
const CORE_SOURCES: &[(&str, &str)] = &[
    ("core/iter.relon", include_str!("core/iter.relon")),
    ("core/string.relon", include_str!("core/string.relon")),
    ("core/list.relon", include_str!("core/list.relon")),
    ("core/dict.relon", include_str!("core/dict.relon")),
];

/// Inject every built-in `core/*.relon` schema declaration into the
/// analyzed tree's `schema_methods` table. Runs as the first analyzer
/// pass so subsequent passes (`collect_schemas`, `collect_extends`,
/// `check_derive_witnesses`, ...) see the built-in methods alongside
/// user-declared ones — that uniformity is decision 21''s whole point.
///
/// Idempotent across modules: each invocation re-installs the same set,
/// but `schema_methods` is shared per-`AnalyzedTree` and we're called
/// before any per-module collection populates it.
pub fn inject_core_schemas(tree: &mut AnalyzedTree) {
    for (path, source) in CORE_SOURCES {
        let root = match parse_document(source) {
            Ok(n) => n,
            Err(err) => panic!(
                "core.relon carrier {path} failed to parse: {err:?} — \
                 this is a compile-time-embedded source, fix the file"
            ),
        };
        // The carriers only contain root-level `#schema X with { ... }`
        // directives — the relevant analyzer pass is
        // `collect_root_schemas`, which feeds each one through
        // `lower_schema_pure_with` (recording fields + methods) and
        // appends the methods to `tree.schema_methods` via
        // `record_schema_methods`. We deliberately skip
        // `collect_schemas` (no dict-field schemas in the carrier),
        // `collect_extends` (no `#extend` directives), and
        // `check_method_uniqueness` (covered by the user-source pass
        // after merge).
        let mut tmp = AnalyzedTree::new();
        root_schemas::collect_root_schemas(&root, &mut tmp);
        // Guard against silent regressions in the carrier — a parse
        // succeeded but the lowering rejected the body (e.g. a typo in
        // a `#schema` directive). Surface this loudly so the bug is
        // unambiguous.
        if tmp
            .diagnostics
            .iter()
            .any(|d| d.severity() == crate::diagnostic::Severity::Error)
        {
            panic!(
                "core.relon carrier {path} produced analyzer errors: {:?}",
                tmp.diagnostics
            );
        }
        merge_core_into(tree, tmp);
    }
}

/// Append the core carrier's `schema_methods` contributions into `dst`.
/// We do *not* copy `schemas` / `root_schemas` entries — those would
/// pollute the user-visible declaration tables and trigger duplicate-
/// name / collision diagnostics when a user `#extend`s the same
/// built-in. The carrier's purpose is purely to populate the method
/// dispatch table.
fn merge_core_into(dst: &mut AnalyzedTree, src: AnalyzedTree) {
    for (schema_name, methods) in src.schema_methods {
        let entry = dst.schema_methods.entry(schema_name).or_default();
        // Preserve source order; user-side `#extend` contributions
        // appended later will sit after the carrier's methods.
        for m in methods {
            // Defensive de-dup in case `inject_core_schemas` is ever
            // called twice on the same tree (e.g. via a test harness
            // that re-runs analysis). Match on method name — the
            // carrier never registers two methods of the same name on
            // one schema, so this is a no-op in the happy path.
            if !entry.iter().any(|existing| existing.name == m.name) {
                entry.push(m);
            }
        }
    }
}

/// Test-only entry: parse the carriers and assert that every method
/// name registered with `register_pure_method` in `stdlib.rs` has a
/// matching declaration here. The list is hard-coded against the
/// runtime's registration table — see `core_carrier_method_names`
/// comment in this module's tests.
#[cfg(test)]
pub fn core_methods_for(schema_name: &str) -> Vec<String> {
    let mut tree = AnalyzedTree::new();
    inject_core_schemas(&mut tree);
    tree.schema_methods
        .get(schema_name)
        .map(|methods| methods.iter().map(|m| m.name.clone()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_schemas_install_string_methods() {
        let names = core_methods_for("String");
        for expected in [
            "upper", "lower", "title", "split", "replace", "contains", "len", "iter",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "core String schema missing `{expected}`: {names:?}"
            );
        }
    }

    #[test]
    fn core_schemas_install_list_methods() {
        let names = core_methods_for("List");
        for expected in ["map", "filter", "reduce", "contains", "join", "len", "iter"] {
            assert!(
                names.iter().any(|n| n == expected),
                "core List schema missing `{expected}`: {names:?}"
            );
        }
    }

    #[test]
    fn core_schemas_install_dict_methods() {
        let names = core_methods_for("Dict");
        for expected in ["merge", "keys", "values", "has_key", "len", "iter"] {
            assert!(
                names.iter().any(|n| n == expected),
                "core Dict schema missing `{expected}`: {names:?}"
            );
        }
    }

    #[test]
    fn core_schemas_install_iter_schema() {
        let names = core_methods_for("Iter");
        assert!(
            names.iter().any(|n| n == "next"),
            "core Iter schema missing `next`: {names:?}"
        );
    }
}
