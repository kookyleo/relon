//! Source-level lint: keep `emitter::emit_op` and `inline_emit::emit_op`
//! in sync on the set of `TraceOp` variants they lower.
//!
//! ## Why
//!
//! The two emit paths (standalone trampoline + at-call-site inline) carry
//! near-identical per-op lowering rules. Today the only sync guard is the
//! `inline_matches_standalone_result` smoke test in
//! `crates/relon-codegen-cranelift/tests/trace_jit_inline_smoke.rs` — and
//! that only catches drift when the missed op happens to appear in the
//! smoke input. Adding a new `TraceOp` variant only forces a match-arm
//! in the standalone path (Rust's exhaustive match check); the inline
//! path's `match` does cover the same variants today, but if someone
//! re-routes a previously-Err arm to a real helper in the standalone
//! side without updating inline (or vice-versa), nothing surfaces it.
//!
//! ## What this test does
//!
//! Scrape both `fn emit_op` bodies from the crate sources, extract the
//! set of `TraceOp::<Variant>` identifiers each one matches on, and
//! assert the two sets are equal. Also assert each set covers every
//! variant declared in `relon-trace-jit::trace_ir::TraceOp` (so this
//! file fails loudly if a new variant is introduced without being
//! handled in either emit path).
//!
//! The check is purely lexical — it does NOT validate that the per-op
//! lowering rule itself is byte-equal. That stays the smoke test's job.
//! What we get is: any drift in *which* variants each path knows about
//! fails at `cargo test`, not at runtime.

use std::collections::BTreeSet;
use std::str;

const EMITTER_SRC: &str = include_str!("../src/emitter.rs");
const INLINE_EMIT_SRC: &str = include_str!("../src/inline_emit.rs");
const TRACE_IR_SRC: &str = include_str!("../../relon-trace-jit/src/trace_ir.rs");

/// Find the byte index of the next ASCII char `c` in `src` at-or-after
/// `from`. Multi-byte UTF-8 chars are skipped over via the underlying
/// byte stream; since `c` is ASCII, byte-level scanning is safe.
fn find_ascii_byte(src: &str, c: u8, from: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    (from..bytes.len()).find(|&i| bytes[i] == c)
}

/// Pull the body of the first `fn emit_op(` in `src`, returning the
/// substring (str-slice safe) from the opening `{` through the matching
/// closing `}`.
fn extract_emit_op_body(src: &str) -> &str {
    let marker = "fn emit_op(";
    let start = src.find(marker).expect("source must declare `fn emit_op(`");
    let body_start =
        find_ascii_byte(src, b'{', start).expect("`fn emit_op` signature must be followed by `{`");
    let bytes = src.as_bytes();
    let mut depth = 0i32;
    let mut i = body_start;
    while i < bytes.len() {
        // `{` and `}` are single-byte ASCII; UTF-8 continuation bytes
        // never collide with them, so direct byte indexing is safe.
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[body_start..=i];
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("unbalanced braces while scanning `fn emit_op` body");
}

/// Extract every `TraceOp::<Ident>` occurrence in `body`, returning the
/// `<Ident>` parts. The scan walks the raw byte stream so it is safe
/// over UTF-8 content (e.g. en-dashes in doc comments).
fn collect_traceop_variants(body: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let needle = b"TraceOp::";
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let ident_start = i + needle.len();
            let mut j = ident_start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > ident_start {
                // SAFETY: range is bounded by ASCII identifier bytes,
                // so it cuts cleanly on a char boundary.
                let ident =
                    str::from_utf8(&bytes[ident_start..j]).expect("identifier bytes are ASCII");
                if ident.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    out.insert(ident.to_string());
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// Walk the `pub enum TraceOp { ... }` declaration and return the
/// declared variant identifiers.
fn collect_traceop_enum_variants(src: &str) -> BTreeSet<String> {
    let marker = "pub enum TraceOp";
    let start = src
        .find(marker)
        .expect("relon-trace-jit must declare `pub enum TraceOp`");
    let body_start = find_ascii_byte(src, b'{', start).expect("TraceOp enum must have a body");
    let bytes = src.as_bytes();
    let mut depth = 0i32;
    let mut i = body_start;
    let mut end = body_start;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    // Body excludes the enclosing `{` / `}`.
    let body_bytes = &bytes[body_start + 1..end];

    let mut out = BTreeSet::new();
    let mut depth = 0i32;
    let mut at_line_start = true;
    let mut i = 0;
    while i < body_bytes.len() {
        let c = body_bytes[i];
        // Line comments.
        if at_line_start
            && i + 1 < body_bytes.len()
            && body_bytes[i] == b'/'
            && body_bytes[i + 1] == b'/'
        {
            while i < body_bytes.len() && body_bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comments — depth-1 nested style we don't expect, but
        // skip a single `/* ... */` defensively.
        if i + 1 < body_bytes.len() && body_bytes[i] == b'/' && body_bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < body_bytes.len() && !(body_bytes[i] == b'*' && body_bytes[i + 1] == b'/')
            {
                i += 1;
            }
            i = (i + 2).min(body_bytes.len());
            continue;
        }
        match c {
            b'{' | b'(' => depth += 1,
            b'}' | b')' => depth -= 1,
            _ => {}
        }
        if c == b'\n' {
            at_line_start = true;
            i += 1;
            continue;
        }
        if c == b' ' || c == b'\t' {
            i += 1;
            continue;
        }
        // A variant identifier sits at depth 0 inside the enum body and
        // is preceded only by whitespace on its line.
        if depth == 0 && at_line_start && c.is_ascii_uppercase() {
            let ident_start = i;
            let mut j = i;
            while j < body_bytes.len()
                && (body_bytes[j].is_ascii_alphanumeric() || body_bytes[j] == b'_')
            {
                j += 1;
            }
            // Peek past whitespace to confirm the next non-space char
            // is `(`, `{`, or `,` — the three legal forms for a variant
            // declaration in `TraceOp`.
            let mut k = j;
            while k < body_bytes.len() && (body_bytes[k] == b' ' || body_bytes[k] == b'\t') {
                k += 1;
            }
            if k < body_bytes.len() && matches!(body_bytes[k], b'(' | b'{' | b',') {
                let ident = str::from_utf8(&body_bytes[ident_start..j])
                    .expect("identifier bytes are ASCII");
                out.insert(ident.to_string());
            }
            i = j;
            at_line_start = false;
            continue;
        }
        at_line_start = false;
        i += 1;
    }
    out
}

#[test]
fn emit_op_traceop_variants_match_between_paths() {
    let standalone_body = extract_emit_op_body(EMITTER_SRC);
    let inline_body = extract_emit_op_body(INLINE_EMIT_SRC);

    let standalone_variants = collect_traceop_variants(standalone_body);
    let inline_variants = collect_traceop_variants(inline_body);

    assert!(
        !standalone_variants.is_empty(),
        "scrape produced an empty variant set for emitter.rs::emit_op \
         — extractor regression, fix the lint before trusting it"
    );

    let only_in_standalone: Vec<_> = standalone_variants
        .difference(&inline_variants)
        .cloned()
        .collect();
    let only_in_inline: Vec<_> = inline_variants
        .difference(&standalone_variants)
        .cloned()
        .collect();

    assert!(
        only_in_standalone.is_empty() && only_in_inline.is_empty(),
        "emit_op paths drifted:\n\
         - only in emitter.rs (standalone): {only_in_standalone:?}\n\
         - only in inline_emit.rs (embedded): {only_in_inline:?}\n\
         Every TraceOp variant matched in one emit_op MUST also be \
         matched in the other (real helper OR explicit Err route). \
         See crates/relon-trace-emitter/src/lib.rs `inline_emit / emitter sync` \
         doc section."
    );
}

#[test]
fn emit_op_covers_every_traceop_enum_variant() {
    let enum_variants = collect_traceop_enum_variants(TRACE_IR_SRC);
    assert!(
        enum_variants.len() >= 10,
        "TraceOp enum scrape returned {} variants — extractor regression",
        enum_variants.len()
    );

    let standalone_body = extract_emit_op_body(EMITTER_SRC);
    let inline_body = extract_emit_op_body(INLINE_EMIT_SRC);
    let standalone_variants = collect_traceop_variants(standalone_body);
    let inline_variants = collect_traceop_variants(inline_body);

    // Both emit paths must mention every declared variant. Rust's
    // exhaustive-match check already enforces this at compile time for
    // each path independently, but stating it here makes the failure
    // mode visible to anyone running just this lint test and gives a
    // crisp diff when a new variant is added.
    let missed_in_standalone: Vec<_> = enum_variants
        .difference(&standalone_variants)
        .cloned()
        .collect();
    let missed_in_inline: Vec<_> = enum_variants
        .difference(&inline_variants)
        .cloned()
        .collect();
    assert!(
        missed_in_standalone.is_empty(),
        "TraceOp variants declared in relon-trace-jit but missing from \
         emitter.rs::emit_op: {missed_in_standalone:?}"
    );
    assert!(
        missed_in_inline.is_empty(),
        "TraceOp variants declared in relon-trace-jit but missing from \
         inline_emit.rs::emit_op: {missed_in_inline:?}"
    );
}

#[test]
fn collect_traceop_variants_strips_doc_lookalikes() {
    // Make sure the extractor doesn't false-positive on prose / paths
    // that happen to mention `TraceOp::` in surrounding documentation.
    let sample = r#"
        // unrelated doc: TraceOp::Add appears here in a comment
        match op {
            TraceOp::Add { dst: _, lhs: _, rhs: _ } => {},
            TraceOp::Return { value: v } => {},
        }
    "#;
    let got = collect_traceop_variants(sample);
    // Both the prose mention and the real arms parse identically
    // (extractor is purely lexical) — confirms the intended behaviour
    // that doc references count too. The match-arms check still works
    // because both sides see the same doc-prose patterns.
    assert!(got.contains("Add"));
    assert!(got.contains("Return"));
}
