//! Broken-input fixture corpus for the P5 error-recovery contract.
//!
//! Each `.relon` file under `tests/fixtures/broken/` is a hand-curated
//! malformed input exercising a common typo class (missing colon,
//! unclosed brace, half-finished f-string, dangling cursor, etc.).
//! For every fixture we assert four properties:
//!
//!   1. `parse_cst` does not panic.
//!   2. The lossless round-trip invariant holds —
//!      `parse_cst(s).syntax().text() == s`.
//!   3. The parser reports at least one error (the fixture is
//!      intentionally invalid).
//!   4. `parse_document` rejects the input (the public contract
//!      surfaces a `ParseDocumentError`). This shields downstream
//!      consumers from silently lowering a malformed source.
//!
//! Together these properties prove the parser is panic-free and
//! recovery-safe on the kinds of input an IDE sees mid-typing.

use std::fs;
use std::path::PathBuf;

use relon_parser::{cst::parse_cst, parse_document};

fn broken_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/broken")
}

fn collect_fixtures() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in fs::read_dir(broken_dir()).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().and_then(|e| e.to_str()) != Some("relon") {
            continue;
        }
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let source = fs::read_to_string(&p).unwrap();
        out.push((name, source));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn normalize_snapshot_text(text: &str) -> String {
    text.replace("\r\n", "\n")
}

#[test]
fn broken_fixtures_round_trip_through_cst() {
    let fixtures = collect_fixtures();
    assert!(!fixtures.is_empty(), "expected at least one broken fixture");
    for (name, source) in fixtures {
        let parsed = parse_cst(&source);
        assert_eq!(
            parsed.syntax().text().to_string(),
            source,
            "round-trip mismatch on {name}"
        );
    }
}

#[test]
fn broken_fixtures_report_at_least_one_error() {
    for (name, source) in collect_fixtures() {
        // Some inputs (e.g. `{ a: 1, , b: 2 }`) are accepted by the
        // CST tokenizer/grammar shape but rejected by the lowering
        // layer. We still want a corpus-level signal that the parser
        // recognises the input as broken — checked either via CST
        // errors OR via the `parse_document` Err arm.
        let parsed = parse_cst(&source);
        let cst_has_errors = !parsed.errors.is_empty();
        let lower_err = parse_document(&source).is_err();
        assert!(
            cst_has_errors || lower_err,
            "fixture {name} reports no error — either the CST or lower_document should reject it"
        );
    }
}

#[test]
fn broken_fixtures_parse_document_returns_err() {
    for (name, source) in collect_fixtures() {
        let result = parse_document(&source);
        assert!(
            result.is_err(),
            "broken fixture {name} unexpectedly parsed: {result:?}"
        );
    }
}

#[test]
fn broken_fixtures_match_golden_summary() {
    // Golden-file snapshot. Update by running:
    //   `BLESS=1 cargo test -p relon-parser \
    //        broken_fixtures_match_golden_summary`
    // and inspecting the resulting diff.
    let mut buf = String::new();
    for (name, source) in collect_fixtures() {
        let source = normalize_snapshot_text(&source);
        buf.push_str(&format!("=== {name} ===\n"));
        buf.push_str(&format!("source: {source:?}\n"));
        let parsed = parse_cst(&source);
        buf.push_str(&format!(
            "round_trip: {}\n",
            parsed.syntax().text().to_string() == source
        ));
        buf.push_str(&format!("cst_errors: {}\n", parsed.errors.len()));
        for e in &parsed.errors {
            buf.push_str(&format!("  - {} @ {}\n", e.message, e.offset));
        }
        let result = parse_document(&source);
        let result_label = match &result {
            Ok(_) => "Ok".to_string(),
            Err(e) => format!("Err({e})"),
        };
        buf.push_str(&format!("parse_document: {result_label}\n\n"));
    }

    let golden_path = broken_dir().join("snapshot.txt");
    let buf = normalize_snapshot_text(&buf);
    if std::env::var_os("BLESS").is_some() {
        fs::write(&golden_path, &buf).expect("write golden");
        return;
    }
    let existing = normalize_snapshot_text(
        &fs::read_to_string(&golden_path).unwrap_or_else(|_| String::new()),
    );
    if existing != buf {
        let actual_path = golden_path.with_extension("actual");
        fs::write(&actual_path, &buf).expect("write actual");
        panic!(
            "golden mismatch — actual written to {}. Re-run with BLESS=1 to update.",
            actual_path.display()
        );
    }
}
