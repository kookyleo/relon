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
use crate::schema::SchemaMethodInfo;
use crate::tree::AnalyzedTree;
use relon_parser::parse_document;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Source text of every built-in `.relon` carrier embedded at compile
/// time. Each entry is `(virtual_path, source)`; the path is only used
/// for diagnostic context if the parse ever fails.
const CORE_SOURCES: &[(&str, &str)] = &[
    ("core/iter.relon", include_str!("core/iter.relon")),
    ("core/string.relon", include_str!("core/string.relon")),
    ("core/list.relon", include_str!("core/list.relon")),
    ("core/dict.relon", include_str!("core/dict.relon")),
];

/// Process-local cache of the parsed + lowered `schema_methods` table
/// produced by the four built-in `core/*.relon` carriers. Parsing those
/// 80-odd lines and running them through `collect_root_schemas` was a
/// hidden ~0.8 ms per `analyze_with_options` call before the cache —
/// the carriers are static, the lowering deterministic, so we pay it
/// once and clone the resulting `Vec<SchemaMethodInfo>` per analyzer
/// invocation.
///
/// `OnceLock` is process-local: each `relon-cli` invocation pays the
/// build cost on its first analyzer call, and amortises across every
/// subsequent module / re-analysis in that process. For a single-shot
/// CLI run this saves us nothing extra; the win is "every call after
/// the first" — and v6-fix-D2's W11 cold-start path measures the
/// every-call case (host runs analyze on the entry, then again for any
/// `#import`).
fn cached_core_schema_methods() -> &'static HashMap<String, Vec<SchemaMethodInfo>> {
    static CACHE: OnceLock<HashMap<String, Vec<SchemaMethodInfo>>> = OnceLock::new();
    CACHE.get_or_init(build_core_schema_methods)
}

/// Slow-path builder driving `parse_document` + `collect_root_schemas`
/// over every embedded carrier. Identical semantics to the pre-cache
/// implementation; the only difference is the merge target is a free-
/// standing map rather than the live `AnalyzedTree`.
fn build_core_schema_methods() -> HashMap<String, Vec<SchemaMethodInfo>> {
    let mut out: HashMap<String, Vec<SchemaMethodInfo>> = HashMap::new();
    for (path, source) in CORE_SOURCES {
        let root = match parse_document(source) {
            Ok(n) => n,
            Err(err) => panic!(
                "core.relon carrier {path} failed to parse: {err:?} — \
                 this is a compile-time-embedded source, fix the file"
            ),
        };
        let mut tmp = AnalyzedTree::new();
        root_schemas::collect_root_schemas(&root, &mut tmp);
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
        for (schema_name, methods) in tmp.schema_methods {
            let entry = out.entry(schema_name).or_default();
            for m in methods {
                if !entry.iter().any(|existing| existing.name == m.name) {
                    entry.push(m);
                }
            }
        }
    }
    out
}

/// Inject every built-in `core/*.relon` schema declaration into the
/// analyzed tree's `schema_methods` table. Runs as the first analyzer
/// pass so subsequent passes (`collect_schemas`, `collect_extends`,
/// `check_derive_witnesses`, ...) see the built-in methods alongside
/// user-declared ones — that uniformity is decision 21''s whole point.
///
/// Idempotent across modules: each invocation re-installs the same set,
/// but `schema_methods` is shared per-`AnalyzedTree` and we're called
/// before any per-module collection populates it.
///
/// v6-fix-D2: parsing + lowering the carrier source is cached in a
/// process-local `OnceLock` (see [`cached_core_schema_methods`]). Every
/// call after the first clones from the cache instead of re-parsing.
pub fn inject_core_schemas(tree: &mut AnalyzedTree) {
    let cached = cached_core_schema_methods();
    for (schema_name, methods) in cached {
        let entry = tree.schema_methods.entry(schema_name.clone()).or_default();
        for m in methods {
            if !entry.iter().any(|existing| existing.name == m.name) {
                entry.push(m.clone());
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
            "upper", "lower", "title", "nfc", "nfd", "nfkc", "nfkd", "split", "replace",
            "contains", "len", "iter",
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
