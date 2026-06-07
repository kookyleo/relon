//! Phase 0b — integration coverage for the LLVM-AOT Unicode `*TableAddr`
//! long tail (`CaseFoldTableAddr`, `FullCaseFoldTableAddr`,
//! `TurkishCaseFoldTableAddr`, `CombiningMarkRangesAddr`,
//! `WhitespaceRangesAddr`, `CasedRangesAddr`, `CaseIgnorableRangesAddr`,
//! `DecompTableAddr`, `CccTableAddr`, `CompositionTableAddr`).
//!
//! # Scope of this file
//!
//! These ops never appear in user IR directly — they are emitted only
//! inside the bundled Unicode stdlib bodies (`upper` / `lower` /
//! `title` / `nfd` / `nfc` / `nfkd` / `nfkc` / `*_locale`). This file
//! pins the narrow build-time property that the Phase-0b `*TableAddr`
//! seam is filled: lowering a Unicode workload no longer returns the
//! Phase-0b stub error.
//!
//! The byte-for-byte alignment with cranelift's gold-standard table data
//! is pinned by the in-crate unit tests in `src/codegen/unicode.rs`
//! (`encode_bytes_match_shared_encoders` checks the emitted table bytes
//! against the exact `relon_ir` encoders cranelift's `ConstPool`
//! consumes; `const_pool_lays_tables_at_aligned_offsets` /
//! `lower_pushes_const_pool_offset` check the lowering lays those bytes
//! into the const-data prefix and resolves the op to that offset).
//!
//! Wave R14 took this seam all the way to a four-way value differential:
//! `upper` / `lower` / `title` / `nfd` now run byte-identically across
//! tree-walk == cranelift == llvm-native == llvm-wasm (see
//! `tests/unicode_four_way.rs`). If a future change re-stubs the Unicode
//! seam, this file still fails loudly on the build-time property.

use relon_codegen_llvm::LlvmAotEvaluator;

/// Every Unicode-method source whose inlined stdlib body emits at least
/// one `*TableAddr` op. Surface forms taken from
/// `relon_ir::stdlib::index` (the `(String, name)` method-dispatch
/// table).
const UNICODE_SRCS: &[(&str, &str)] = &[
    ("upper", "#main(String s) -> String\ns.upper()"),
    ("lower", "#main(String s) -> String\ns.lower()"),
    ("title", "#main(String s) -> String\ns.title()"),
    ("nfd", "#main(String s) -> String\ns.nfd()"),
    ("nfc", "#main(String s) -> String\ns.nfc()"),
    ("nfkd", "#main(String s) -> String\ns.nfkd()"),
    ("nfkc", "#main(String s) -> String\ns.nfkc()"),
    (
        "upper_locale",
        "#main(String s) -> String\ns.upper_locale(\"tr\")",
    ),
    (
        "lower_locale",
        "#main(String s) -> String\ns.lower_locale(\"tr\")",
    ),
    (
        "title_locale",
        "#main(String s) -> String\ns.title_locale(\"tr\")",
    ),
];

/// The marker the Phase-0b Unicode stub used to return. Removing the
/// stub means this string must never surface from any Unicode workload
/// build again.
const UNICODE_SEAM_MARKER: &str = "Phase 0b unicode seam";

#[test]
fn unicode_table_addr_ops_no_longer_hit_the_phase0b_seam() {
    for (name, src) in UNICODE_SRCS {
        match LlvmAotEvaluator::from_source(src) {
            Ok(_) => {
                // The whole body lowered — even better; the `*TableAddr`
                // ops clearly compiled.
            }
            Err(e) => {
                let msg = format!("{e:?}");
                assert!(
                    !msg.contains(UNICODE_SEAM_MARKER),
                    "`{name}` build hit the Unicode `*TableAddr` Phase-0b \
                     seam, which Phase 0b was supposed to fill. The \
                     `*TableAddr` lowering must handle the op rather than \
                     return the stub error.\n  error: {msg}"
                );
            }
        }
    }
}
