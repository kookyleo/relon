//! Property tests for the P5 parse-never-panics contract.
//!
//! These cheap proptest cases generate random byte strings + structured
//! noise (balanced/imbalanced brackets, half-finished f-strings,
//! half-typed identifiers) and assert three invariants for every input:
//!
//!   1. **No panic** — `parse_cst` always returns a `Parse`.
//!   2. **Round-trip** — `parse_cst(s).syntax().text() == s` byte-for-
//!      byte. This is the load-bearing lossless invariant; any failure
//!      means the lexer dropped or duplicated source bytes.
//!   3. **Total `parse_document`** — the legacy combinator path is
//!      allowed to return either `Ok(_)` or `Err(_)`, but it MUST
//!      never panic. The Err arm is reserved for syntactically invalid
//!      inputs (see `broken_fixtures.rs` for hand-curated examples).
//!
//! Kept lightweight (16-64 cases per test) so the suite stays well
//! under the 10-second budget called out in the P5 plan.

use proptest::prelude::*;

use relon_parser::{cst::parse_cst, parse_document};

fn check_invariants(source: &str) {
    let parsed = parse_cst(source);
    let reconstructed = parsed.syntax().text().to_string();
    assert_eq!(
        reconstructed, source,
        "round-trip mismatch on {source:?} — got {reconstructed:?}"
    );
    // parse_document is allowed to return Err on malformed input.
    // We just need it not to panic.
    let _ = parse_document(source);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        max_shrink_iters: 64,
        ..ProptestConfig::default()
    })]

    /// Bytes selected from a Relon-flavoured ASCII alphabet — operators,
    /// punctuation, identifiers, digits, escape characters. The set is
    /// rich enough to exercise lexer alternatives without spending most
    /// cases on uninteresting whitespace.
    #[test]
    fn random_ascii_bytes_round_trip(s in r##"[a-zA-Z0-9 \t\r\n,:;.\[\](){}<>+\-*/%=!&|"'#@_^?$~]{0,40}"##) {
        check_invariants(&s);
    }

    /// Arbitrary UTF-8 strings up to 32 bytes. Catches lexer issues
    /// around char-boundary handling and unknown-byte recovery.
    #[test]
    fn random_utf8_round_trip(s in proptest::string::string_regex(".{0,32}").unwrap()) {
        check_invariants(&s);
    }

    /// Inputs shaped like `{ ... }` with random inner noise. Forces the
    /// dict production into its error-recovery paths.
    #[test]
    fn dict_shaped_noise(inner in r##"[a-z0-9 ,:\[\](){}"#@]{0,30}"##) {
        let s = format!("{{ {inner} }}");
        check_invariants(&s);
    }

    /// Inputs shaped like `[ ... ]` with random inner noise.
    #[test]
    fn list_shaped_noise(inner in r##"[a-z0-9 ,:\[\](){}"#@]{0,30}"##) {
        let s = format!("[ {inner} ]");
        check_invariants(&s);
    }

    /// f-string-like inputs with possibly-unbalanced interpolation
    /// braces. The recursive interpolation parser needs to recover
    /// cleanly from missing `}` or stray `"`.
    #[test]
    fn fstring_noise(literal in r##"[a-z0-9 ]{0,10}"##, expr in r##"[a-z0-9 +\-*]{0,10}"##) {
        for shape in [
            format!(r#"f"{literal}""#),
            format!(r#"f"{literal}${{{expr}}}""#),
            format!(r#"f"{literal}${{{expr}""#),
            format!(r#"f"{literal}${{""#),
            format!(r#"f"${{{expr}"#),
        ] {
            check_invariants(&shape);
        }
    }

    /// Imbalanced bracket prefixes. The closing bracket sync sets need
    /// to keep recovery from runaway-consuming the whole input.
    #[test]
    fn imbalanced_brackets(
        opens in proptest::collection::vec(prop_oneof![Just("{"), Just("["), Just("(")], 0..6),
        closes in proptest::collection::vec(prop_oneof![Just("}"), Just("]"), Just(")")], 0..6),
        atoms in proptest::collection::vec(
            prop_oneof![Just("a"), Just("1"), Just(" "), Just(":"), Just(",")],
            0..10,
        ),
    ) {
        let mut s = String::new();
        for o in &opens { s.push_str(o); }
        for a in &atoms { s.push_str(a); }
        for c in &closes { s.push_str(c); }
        check_invariants(&s);
    }

    /// Half-typed identifiers / numerics / directives — the kind of
    /// thing an IDE sees on every keystroke.
    #[test]
    fn typing_progression(prefix in r##"[a-zA-Z_]"##, body in r##"[a-zA-Z0-9_]{0,8}"##) {
        for (lead, sep) in [
            ("{ ", ": 1 }"),
            ("{ a: ", " }"),
            ("[", ", 2]"),
            ("#", " { x: 1 }"),
            ("@", "\n{ x: 1 }"),
            ("&", "\n{ x: 1 }"),
        ] {
            let s = format!("{lead}{prefix}{body}{sep}");
            check_invariants(&s);
        }
    }
}

/// Hand-picked corner cases that have historically triggered panics in
/// related projects. Cheap to run alongside the proptest cases.
#[test]
fn known_panic_avoidance_cases() {
    for s in [
        "",
        "\u{0000}",
        "\u{0000}\u{0001}\u{0002}",
        "}",
        "{",
        "[}",
        "{]",
        "f\"",
        "f\"${",
        "f\"${}",
        "f\"${}\"",
        "#\n#\n#\n",
        "@@@@",
        "&&&&",
        "\"\\",
        "\"\\\\\"",
        "0x",
        "0b",
        "0o",
        "1e",
        "1.",
        ".1",
        "....",
        "/*",
        "//",
        "/**/",
        "/* /* */",
    ] {
        check_invariants(s);
    }
}
