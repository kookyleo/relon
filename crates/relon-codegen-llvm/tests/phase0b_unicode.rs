//! Phase 0b — integration coverage for the LLVM-AOT Unicode `*TableAddr`
//! long tail (`CaseFoldTableAddr`, `FullCaseFoldTableAddr`,
//! `TurkishCaseFoldTableAddr`, `CombiningMarkRangesAddr`,
//! `WhitespaceRangesAddr`, `CasedRangesAddr`, `CaseIgnorableRangesAddr`,
//! `DecompTableAddr`, `CccTableAddr`, `CompositionTableAddr`).
//!
//! # Why this file does not run a three-way value differential
//!
//! These ops never appear in user IR directly — they are emitted only
//! inside the bundled Unicode stdlib bodies (`upper` / `lower` /
//! `title` / `nfd` / `nfc` / `nfkd` / `nfkc` / `*_locale`). Those bodies
//! also reference ops that NEITHER Phase-0b backend lowers yet:
//!
//!   * cranelift rejects `Op::LoadStringPtr`
//!     (`unsupported op in v5-beta-2 stage 3: LoadStringPtr`);
//!   * the LLVM backend rejects the `Op::Trap { InvalidUtf8 }` / string
//!     decode ops these bodies open with (Phase-0b `call.rs` seam).
//!
//! So `from_source` of a `s.upper()` workload fails to *build* on both
//! sides — there is no compiled entry to invoke, hence no observable
//! value to compare. A runtime three-way differential is therefore
//! deferred until the surrounding string ops land; the byte-for-byte
//! alignment with cranelift's gold-standard table data is pinned now by
//! the in-crate unit tests in `src/codegen/unicode.rs`
//! (`encode_bytes_match_shared_encoders` checks the emitted table bytes
//! against the exact `relon_ir` encoders cranelift's `ConstPool`
//! consumes; `table_addr_emits_global_of_encoder_length` checks the
//! lowering wires those bytes into the module global unchanged).
//!
//! This integration test pins the one end-to-end property reachable
//! through the public API today: lowering a Unicode workload no longer
//! trips the Unicode `*TableAddr` seam — the `*TableAddr` ops compile,
//! and any remaining build failure is attributable to a *different*,
//! not-yet-implemented op family. If a future change re-stubs the
//! Unicode seam, this fails loudly.

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
