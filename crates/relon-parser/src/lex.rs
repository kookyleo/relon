//! Round-trip lexer for the v2 (rowan-backed) parser.
//!
//! Produces a flat `Vec<(SyntaxKind, &str)>` covering EVERY byte of
//! input — including whitespace, comments, and stray characters the
//! lexer can't classify. Round-trip invariant:
//!
//! ```ignore
//! lex(source).iter().map(|(_, t)| *t).collect::<String>() == source
//! ```
//!
//! Errors are NEVER returned. An unterminated string / block comment
//! / unknown byte still gets a token covering the bytes; downstream
//! parser layers (P2) decide whether to emit a diagnostic. The
//! "lexer never fails" contract is the bedrock of error-recovering
//! parsing — without it, the CST couldn't represent partial input.

use crate::syntax::SyntaxKind;

/// Tokenize `source` into a sequence of `(kind, lexeme)` slices.
/// `lexeme` borrows directly from `source` — no allocations.
///
/// Order matters: every consecutive pair of returned lexemes is
/// adjacent in source (concatenation reconstructs the original).
pub fn lex(source: &str) -> Vec<(SyntaxKind, &str)> {
    let mut out: Vec<(SyntaxKind, &str)> = Vec::new();
    let bytes = source.as_bytes();
    let mut idx = 0;
    while idx < source.len() {
        let (kind, end) = scan_one(source, bytes, idx);
        out.push((kind, &source[idx..end]));
        debug_assert!(end > idx, "lexer did not advance at byte {idx}");
        idx = end;
    }
    out
}

/// Scan ONE token starting at `idx`. Returns `(kind, end_byte)`
/// where `end_byte > idx`. Always succeeds — falls back to `UNKNOWN`
/// for any byte that doesn't match a known pattern, advancing one
/// codepoint at a time.
fn scan_one(source: &str, bytes: &[u8], idx: usize) -> (SyntaxKind, usize) {
    let b = bytes[idx];

    // 1. Whitespace run — coalesced into a single WHITESPACE token.
    if is_whitespace_byte(b) {
        let mut end = idx + 1;
        while end < bytes.len() && is_whitespace_byte(bytes[end]) {
            end += 1;
        }
        return (SyntaxKind::WHITESPACE, end);
    }

    // 2. Comments. Check before `/` operator.
    if b == b'/' && bytes.get(idx + 1) == Some(&b'/') {
        let end = scan_line_comment(bytes, idx);
        return (SyntaxKind::LINE_COMMENT, end);
    }
    if b == b'/' && bytes.get(idx + 1) == Some(&b'*') {
        let end = scan_block_comment(bytes, idx);
        return (SyntaxKind::BLOCK_COMMENT, end);
    }

    // 3. Strings — must come before identifier scan because `r` and
    //    `f` may prefix strings.
    if let Some(end) = scan_string_like(source, bytes, idx) {
        return (SyntaxKind::STRING, end);
    }

    // 4. Multi-char punctuation / operators. Checked longest first.
    if starts_with(bytes, idx, b"...") {
        return (SyntaxKind::ELLIPSIS, idx + 3);
    }
    for (lexeme, kind) in MULTI_CHAR_OPS {
        if starts_with(bytes, idx, lexeme.as_bytes()) {
            return (*kind, idx + lexeme.len());
        }
    }

    // 5. Identifiers / numbers / single-char punct.
    if is_ident_start(b) {
        return (SyntaxKind::IDENT, scan_ident(bytes, idx));
    }
    if b.is_ascii_digit() {
        return (SyntaxKind::NUMBER, scan_number(bytes, idx));
    }
    if let Some(kind) = single_char_kind(b) {
        return (kind, idx + 1);
    }

    // 6. Unknown — emit one UTF-8 codepoint as UNKNOWN.
    let len = utf8_codepoint_len(b);
    (SyntaxKind::UNKNOWN, idx + len)
}

// =====================================================================
// Lexical sub-scanners.
//
// Each `scan_*` always returns an `end >= idx + 1`. Unterminated
// constructs swallow to end-of-input instead of erroring — see the
// module-doc "never fails" contract.
// =====================================================================

fn scan_line_comment(bytes: &[u8], start: usize) -> usize {
    let mut end = start + 2;
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    end
}

fn scan_block_comment(bytes: &[u8], start: usize) -> usize {
    let mut end = start + 2;
    while end + 1 < bytes.len() {
        if bytes[end] == b'*' && bytes[end + 1] == b'/' {
            return end + 2;
        }
        end += 1;
    }
    bytes.len()
}

/// Detect and consume a string literal at `idx`. Returns `Some(end)`
/// on match; `None` when there's no string here. Handles plain
/// `"..."`, raw `r"..."` / `r#"..."#`, f-string `f"..."` /
/// `f#"..."#`.
fn scan_string_like(source: &str, bytes: &[u8], idx: usize) -> Option<usize> {
    match bytes[idx] {
        b'"' => Some(scan_normal_string(bytes, idx)),
        b'r' if next_is_hash_quote(bytes, idx + 1) => Some(scan_raw_string(source, idx, 1)),
        b'f' if next_is_hash_quote(bytes, idx + 1) => Some(scan_f_string(source, idx)),
        _ => None,
    }
}

/// True if `bytes[off..]` starts with zero or more `#` followed by
/// `"`. Distinguishes a string-prefix `r` / `f` (which IS followed
/// by `"` or `#"...`) from a regular identifier starting with `r` /
/// `f`.
fn next_is_hash_quote(bytes: &[u8], off: usize) -> bool {
    let mut i = off;
    while bytes.get(i) == Some(&b'#') {
        i += 1;
    }
    bytes.get(i) == Some(&b'"')
}

fn scan_normal_string(bytes: &[u8], start: usize) -> usize {
    let mut idx = start + 1;
    while idx < bytes.len() {
        let b = bytes[idx];
        if b == b'\\' {
            idx += 1;
            if idx < bytes.len() {
                idx += utf8_codepoint_len(bytes[idx]);
            }
            continue;
        }
        if b == b'"' {
            return idx + 1;
        }
        idx += utf8_codepoint_len(b);
    }
    bytes.len()
}

/// `pub(crate)` re-export of [`scan_normal_string`] for the CST
/// builder's f-string interior scan. The CST walks inside an
/// already-captured f-string byte by byte; when it crosses a `"`
/// (mid-interpolation), it needs the same "skip a balanced string"
/// logic the main lexer uses to avoid mistaking a closing-`"`
/// inside the interpolation for the f-string's outer close.
pub(crate) fn scan_normal_string_for_cst(bytes: &[u8], start: usize) -> usize {
    scan_normal_string(bytes, start)
}

/// `pub(crate)` re-export of [`utf8_codepoint_len`] for the CST
/// builder's f-string interior scan. Same forward-progress
/// guarantees apply.
pub(crate) fn utf8_codepoint_len_for_cst(b: u8) -> usize {
    utf8_codepoint_len(b)
}

fn scan_raw_string(source: &str, start: usize, prefix_len: usize) -> usize {
    let bytes = source.as_bytes();
    let mut quote = start + prefix_len;
    while bytes.get(quote) == Some(&b'#') {
        quote += 1;
    }
    if bytes.get(quote) != Some(&b'"') {
        // Fallback — the caller (`scan_string_like`) shouldn't have
        // dispatched here without confirming the trailing `"`. If
        // somehow this fires, treat as one byte so we don't loop.
        return start + 1;
    }
    let hashes = quote - start - prefix_len;
    let body_start = quote + 1;
    let mut closing = String::from("\"");
    for _ in 0..hashes {
        closing.push('#');
    }
    match source[body_start..].find(&closing) {
        Some(rel) => body_start + rel + closing.len(),
        None => source.len(),
    }
}

fn scan_f_string(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut quote = start + 1;
    while bytes.get(quote) == Some(&b'#') {
        quote += 1;
    }
    if bytes.get(quote) != Some(&b'"') {
        return start + 1;
    }
    let hashes = quote - start - 1;
    let mut closing = String::from("\"");
    for _ in 0..hashes {
        closing.push('#');
    }
    let mut idx = quote + 1;
    let mut interp_depth: usize = 0;
    while idx < bytes.len() {
        if interp_depth == 0 {
            if starts_with(bytes, idx, closing.as_bytes()) {
                return idx + closing.len();
            }
            if starts_with(bytes, idx, b"${") {
                interp_depth = 1;
                idx += 2;
                continue;
            }
            if hashes == 0 && bytes[idx] == b'\\' {
                idx += 1;
                if idx < bytes.len() {
                    idx += utf8_codepoint_len(bytes[idx]);
                }
                continue;
            }
            idx += utf8_codepoint_len(bytes[idx]);
            continue;
        }
        // Inside `${...}` — track balanced braces so a literal `}`
        // doesn't close the interpolation prematurely.
        match bytes[idx] {
            b'{' => {
                interp_depth += 1;
                idx += 1;
            }
            b'}' => {
                interp_depth -= 1;
                idx += 1;
            }
            b'"' => {
                idx = scan_normal_string(bytes, idx);
            }
            _ => {
                idx += utf8_codepoint_len(bytes[idx]);
            }
        }
    }
    bytes.len()
}

fn scan_ident(bytes: &[u8], start: usize) -> usize {
    let mut end = start + 1;
    while end < bytes.len() && is_ident_continue(bytes[end]) {
        end += 1;
    }
    end
}

fn scan_number(bytes: &[u8], start: usize) -> usize {
    let mut idx = start;
    if bytes.get(idx) == Some(&b'0') {
        if matches!(bytes.get(idx + 1), Some(b'x' | b'X')) {
            idx += 2;
            while bytes.get(idx).is_some_and(|b| b.is_ascii_hexdigit()) {
                idx += 1;
            }
            return idx;
        }
        if matches!(bytes.get(idx + 1), Some(b'o' | b'O')) {
            idx += 2;
            while bytes.get(idx).is_some_and(|b| matches!(b, b'0'..=b'7')) {
                idx += 1;
            }
            return idx;
        }
        if matches!(bytes.get(idx + 1), Some(b'b' | b'B')) {
            idx += 2;
            while bytes.get(idx).is_some_and(|b| matches!(b, b'0' | b'1')) {
                idx += 1;
            }
            return idx;
        }
    }
    while bytes.get(idx).is_some_and(|b| b.is_ascii_digit()) {
        idx += 1;
    }
    if bytes.get(idx) == Some(&b'.') && bytes.get(idx + 1).is_some_and(|b| b.is_ascii_digit()) {
        idx += 1;
        while bytes.get(idx).is_some_and(|b| b.is_ascii_digit()) {
            idx += 1;
        }
    }
    if matches!(bytes.get(idx), Some(b'e' | b'E')) {
        let checkpoint = idx;
        idx += 1;
        if matches!(bytes.get(idx), Some(b'+' | b'-')) {
            idx += 1;
        }
        let digits_start = idx;
        while bytes.get(idx).is_some_and(|b| b.is_ascii_digit()) {
            idx += 1;
        }
        if idx == digits_start {
            idx = checkpoint;
        }
    }
    idx
}

fn single_char_kind(b: u8) -> Option<SyntaxKind> {
    Some(match b {
        b'{' => SyntaxKind::L_BRACE,
        b'}' => SyntaxKind::R_BRACE,
        b'[' => SyntaxKind::L_BRACK,
        b']' => SyntaxKind::R_BRACK,
        b'(' => SyntaxKind::L_PAREN,
        b')' => SyntaxKind::R_PAREN,
        b',' => SyntaxKind::COMMA,
        b':' => SyntaxKind::COLON,
        b'.' => SyntaxKind::DOT,
        b'@' => SyntaxKind::AT,
        b'#' => SyntaxKind::HASH,
        b'&' => SyntaxKind::AMP,
        b'?' => SyntaxKind::QUESTION,
        b'=' => SyntaxKind::EQ,
        b'<' => SyntaxKind::LT,
        b'>' => SyntaxKind::GT,
        b'+' => SyntaxKind::PLUS,
        b'-' => SyntaxKind::MINUS,
        b'*' => SyntaxKind::STAR,
        b'/' => SyntaxKind::SLASH,
        b'%' => SyntaxKind::PERCENT,
        b'!' => SyntaxKind::BANG,
        b'|' => SyntaxKind::PIPE,
        _ => return None,
    })
}

/// Multi-char operators, longest first. Each entry is `(lexeme,
/// kind)`. Ellipsis `...` is handled inline above (longest-match
/// before this list runs).
const MULTI_CHAR_OPS: &[(&str, SyntaxKind)] = &[
    ("==", SyntaxKind::EQ_EQ),
    ("!=", SyntaxKind::BANG_EQ),
    ("<=", SyntaxKind::LT_EQ),
    (">=", SyntaxKind::GT_EQ),
    ("&&", SyntaxKind::AMP_AMP),
    ("||", SyntaxKind::PIPE_PIPE),
    ("++", SyntaxKind::PLUS_PLUS),
    ("=>", SyntaxKind::FAT_ARROW),
    ("->", SyntaxKind::THIN_ARROW),
];

fn starts_with(bytes: &[u8], idx: usize, needle: &[u8]) -> bool {
    bytes
        .get(idx..idx + needle.len())
        .is_some_and(|slice| slice == needle)
}

fn is_whitespace_byte(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn is_ident_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic()
}

fn is_ident_continue(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Length of the UTF-8 codepoint starting at `b`. Returns at least
/// 1 so the lexer makes forward progress even on invalid UTF-8.
fn utf8_codepoint_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1, // continuation byte or invalid leader — advance by 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concatenating every lexeme must yield the original source —
    /// the central invariant that justifies the lossless tree.
    fn assert_round_trip(source: &str) {
        let tokens = lex(source);
        let reconstructed: String = tokens.iter().map(|(_, t)| *t).collect();
        assert_eq!(reconstructed, source, "round-trip mismatch");
        // And every token covers at least one byte.
        for (kind, text) in &tokens {
            assert!(!text.is_empty(), "empty token {kind:?}");
        }
    }

    #[test]
    fn empty_source() {
        let tokens = lex("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn whitespace_only() {
        assert_round_trip("   \n\t\r\n  ");
    }

    #[test]
    fn line_and_block_comments() {
        assert_round_trip("// hi\n/* multi\n   line */ x");
    }

    #[test]
    fn punctuation_and_operators() {
        assert_round_trip(
            "{ } [ ] ( ) , : . @ # & ? = == != <= >= && || ++ => -> + - * / % < > ! | ...",
        );
    }

    #[test]
    fn identifiers_keywords_numbers() {
        assert_round_trip("foo bar_baz where match 0x1F 0b101 0o77 1_2 1.5 1e10 1.5e-2");
    }

    #[test]
    fn strings_normal_raw_f() {
        assert_round_trip(r###""hi" "esc\"d" r"raw" r#"r#hash"# f"fs ${x+1}" f"plain""###);
    }

    #[test]
    fn unterminated_constructs_dont_panic() {
        // None of these errored under the old strict tokenizer; the
        // round-trip lexer must still cover every byte.
        for src in [
            "\"unterminated",
            "/* never closes",
            "r#\"raw without end",
            "f\"prefix ${nested",
        ] {
            assert_round_trip(src);
        }
    }

    #[test]
    fn unknown_byte_classifies_as_unknown() {
        // `\0` and the cent sign aren't valid Relon syntax. Each
        // becomes one UNKNOWN token — the round-trip still holds.
        let src = "x \0 y";
        let tokens = lex(src);
        let has_unknown = tokens.iter().any(|(k, _)| *k == SyntaxKind::UNKNOWN);
        assert!(has_unknown, "expected an UNKNOWN token: {tokens:?}");
        assert_round_trip(src);
    }

    #[test]
    fn dict_with_comment_and_string() {
        let src = "// header\n{\n    foo: \"hi\",\n    bar: 1\n}\n";
        let tokens = lex(src);
        // Must contain a LINE_COMMENT, a couple STRINGs / NUMBERs,
        // and the brace pair.
        let kinds: Vec<SyntaxKind> = tokens.iter().map(|(k, _)| *k).collect();
        assert!(kinds.contains(&SyntaxKind::LINE_COMMENT));
        assert!(kinds.contains(&SyntaxKind::STRING));
        assert!(kinds.contains(&SyntaxKind::NUMBER));
        assert!(kinds.contains(&SyntaxKind::L_BRACE));
        assert!(kinds.contains(&SyntaxKind::R_BRACE));
        assert_round_trip(src);
    }

    #[test]
    fn multi_char_op_takes_priority_over_singles() {
        let tokens = lex("a == b");
        let kinds: Vec<SyntaxKind> = tokens.iter().map(|(k, _)| *k).collect();
        assert!(
            kinds.contains(&SyntaxKind::EQ_EQ),
            "expected EQ_EQ, got {kinds:?}"
        );
        assert!(!kinds.contains(&SyntaxKind::EQ));
    }

    /// Walk every `.relon` file shipped with the project and verify
    /// the lossless round-trip invariant. This is the strongest
    /// real-world coverage — 200+ files cover every construct the
    /// language supports today.
    #[test]
    fn every_relon_fixture_round_trips() {
        use std::fs;
        use std::path::PathBuf;

        // Walk from the workspace root. `CARGO_MANIFEST_DIR` points
        // at the `relon-parser` crate; the workspace root is two up.
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = crate_dir
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root above crates/relon-parser")
            .to_path_buf();
        let mut files = Vec::new();
        collect_relon_files(&workspace_root, &mut files);
        // Exclude any target-dir paths that may slip in if a future
        // contributor checks one in (defensive).
        files.retain(|p| !p.to_string_lossy().contains("/target/"));
        assert!(
            !files.is_empty(),
            "no .relon fixtures found under {workspace_root:?}"
        );
        for path in files {
            let source = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            let tokens = lex(&source);
            let reconstructed: String = tokens.iter().map(|(_, t)| *t).collect();
            assert_eq!(reconstructed, source, "lex/round-trip mismatch on {path:?}");
        }
    }

    fn collect_relon_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let p = entry.path();
            if p.is_dir() {
                // Skip vendored dirs that aren't fixtures of our own.
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if matches!(name, "target" | "node_modules" | ".git") {
                    continue;
                }
                collect_relon_files(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("relon") {
                out.push(p);
            }
        }
    }

    #[test]
    fn raw_string_prefix_distinguished_from_r_ident() {
        // `r` alone is an identifier. `r"..."` is a string.
        let tokens = lex(r#"r r"x" foo"#);
        let kinds: Vec<SyntaxKind> = tokens.iter().map(|(k, _)| *k).collect();
        // We expect: IDENT("r"), WS, STRING("r\"x\""), WS, IDENT("foo")
        assert_eq!(kinds[0], SyntaxKind::IDENT);
        assert_eq!(kinds[2], SyntaxKind::STRING);
        assert_eq!(kinds[4], SyntaxKind::IDENT);
    }
}
