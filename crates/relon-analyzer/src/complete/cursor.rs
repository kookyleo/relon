//! Complete sub-module: cursor context classification.
//!
//! [`classify_cursor`] inspects the bytes immediately before the
//! cursor offset to decide which family of candidates makes sense:
//!
//! * leading `#` → [`super::CursorContext::Directive`]
//! * leading `@` → [`super::CursorContext::Decorator`]
//! * leading `&` → [`super::CursorContext::Reference`]
//! * leading `<` after a type head / `,` inside generics / `*` at a
//!   dict field start / whitespace after `->` → `Type`
//! * leading `.` → [`super::CursorContext::Member`] with the head
//!   identifier captured
//! * otherwise → [`super::CursorContext::Bare`]
//!
//! All inspection is byte-level so it survives partially-typed input
//! the parser can't recover.

use super::{is_ident_byte, CursorContext};

pub(super) fn classify_cursor(source: &str, offset: usize) -> CursorContext {
    let bytes = source.as_bytes();
    // Anchor: walk back through identifier chars to find the start of
    // the word the user is currently typing.
    let mut word_start = offset.min(bytes.len());
    while word_start > 0 && is_ident_byte(bytes[word_start - 1]) {
        word_start -= 1;
    }
    let suffix = source[word_start..offset.min(source.len())].to_string();

    // Look at the byte immediately before the word.
    let prev = word_start.checked_sub(1).map(|i| bytes[i]);

    match prev {
        Some(b'#') => CursorContext::Directive { prefix: suffix },
        Some(b'@') => CursorContext::Decorator { prefix: suffix },
        Some(b'&') => CursorContext::Reference { prefix: suffix },
        Some(b'<') if preceded_by_type_head(bytes, word_start - 1) => {
            CursorContext::Type { prefix: suffix }
        }
        Some(b',') if inside_generic_args(bytes, word_start - 1) => {
            CursorContext::Type { prefix: suffix }
        }
        Some(b'*') if at_field_start(bytes, word_start - 1) => {
            CursorContext::Type { prefix: suffix }
        }
        Some(b'.') => {
            // Walk back past the dot to grab the head identifier.
            let dot_pos = word_start - 1;
            let mut head_end = dot_pos;
            // Skip whitespace between head and dot (rare but possible).
            while head_end > 0 && bytes[head_end - 1].is_ascii_whitespace() {
                head_end -= 1;
            }
            let mut head_start = head_end;
            while head_start > 0 && is_ident_byte(bytes[head_start - 1]) {
                head_start -= 1;
            }
            // A bare-dot context (no head, e.g. mid-string) falls back
            // to plain bare completion.
            if head_start == head_end {
                CursorContext::Bare { prefix: suffix }
            } else {
                CursorContext::Member {
                    head: source[head_start..head_end].to_string(),
                    suffix,
                }
            }
        }
        _ if after_arrow(bytes, word_start) => CursorContext::Type { prefix: suffix },
        _ => CursorContext::Bare { prefix: suffix },
    }
}

/// `<` is a generic-args opener when the byte just before it is an
/// identifier byte. Differentiates `Foo<│>` from `<` as a less-than
/// operator in arithmetic context — the latter has a number / closing
/// paren / space + identifier just before, not the bare identifier
/// tail required for a type head.
fn preceded_by_type_head(bytes: &[u8], lt_pos: usize) -> bool {
    if lt_pos == 0 {
        return false;
    }
    is_ident_byte(bytes[lt_pos - 1])
}

/// Track whether the cursor sits inside an unbalanced `<...>` opened
/// by a type head. Walks backward balancing `<` / `>` and giving up
/// when an unrelated delimiter (newline, `{`, `}`, `;`) appears
/// before finding the opener — those mark a non-generic context.
fn inside_generic_args(bytes: &[u8], comma_pos: usize) -> bool {
    let mut depth: i32 = 0;
    let mut i = comma_pos;
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b'>' => depth += 1,
            b'<' => {
                if depth == 0 {
                    return preceded_by_type_head(bytes, i);
                }
                depth -= 1;
            }
            b'\n' | b'{' | b'}' | b';' => return false,
            _ => {}
        }
    }
    false
}

/// `*` is a typed-spread marker when it sits at the head of a dict
/// or list field — i.e. the preceding non-whitespace byte is `,`,
/// `{`, `[`, or the start of the file. Inside an expression `*`
/// would be a binary operator and gets routed to Bare context.
fn at_field_start(bytes: &[u8], star_pos: usize) -> bool {
    let mut i = star_pos;
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            b',' | b'{' | b'[' | b'(' => return true,
            _ => return false,
        }
    }
    true
}

/// Detect the `->` arrow position (closure return type). The cursor
/// has just passed any whitespace following the `->`; we walk back
/// over that whitespace and look for the two-byte arrow.
fn after_arrow(bytes: &[u8], word_start: usize) -> bool {
    let mut i = word_start;
    while i > 0 && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
        i -= 1;
    }
    i >= 2 && &bytes[i - 2..i] == b"->"
}
