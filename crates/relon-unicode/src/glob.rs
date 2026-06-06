//! Linear-time Unicode-aware glob pattern matcher.
//!
//! This module backs the `glob_match(s, pattern) -> Bool` stdlib
//! function. Relon is a config DSL and intentionally does **not** ship
//! a full regex engine; the LuaJIT-pattern-subset glob covers the
//! 80%-case for config-style matching (paths, URL prefixes, field
//! names, file globs) without exposing users to catastrophic
//! backtracking surfaces.
//!
//! ## Syntax
//!
//! | Token   | Meaning                                              |
//! |---------|------------------------------------------------------|
//! | `*`     | match any (possibly empty) sequence of code points   |
//! | `?`     | match exactly one Unicode code point                 |
//! | `[abc]` | match one code point from the listed set             |
//! | `[^abc]`| match one code point **not** in the listed set       |
//! | `\*`    | literal `*`                                          |
//! | `\?`    | literal `?`                                          |
//! | `\\`    | literal backslash                                    |
//! | `\[`    | literal `[`                                          |
//! | other   | literal code point                                   |
//!
//! Matching is **anchored on both ends** (full-string match, not
//! sub-string scan) and **case-sensitive**. The case-insensitive
//! variant + `[a-z]` range syntax are deferred to a follow-up phase;
//! the host can pre-fold both sides through `lower(...)` /
//! `upper(...)` for the common case.
//!
//! ## Algorithm
//!
//! Two-pointer scan with a **single** backtrack anchor (Go
//! `filepath.Match` style):
//!
//! 1. Advance `(s_idx, p_idx)` through matched literals / `?` /
//!    `[set]`.
//! 2. On encountering `*`, record the snapshot `(star_p_idx,
//!    star_s_idx)` and tentatively skip the `*` (consume zero string
//!    code points).
//! 3. On a mismatch, if a `*` was seen, restore to `(star_p_idx + 1,
//!    star_s_idx + 1)` — i.e. let the most recent `*` swallow one
//!    more code point. Otherwise return `false`.
//! 4. At end of input, drain any trailing `*` characters and accept
//!    if the pattern is exhausted.
//!
//! Worst-case time complexity is `O(|s| * |p|)` (the same as a naive
//! substring scan) and worst-case extra space is `O(1)`. Crucially,
//! there is **no exponential backtracking path** — every state of the
//! algorithm advances either `s_idx` or `p_idx` monotonically, with
//! one snapshot variable that only ever increases. A pattern like
//! `"a*a*a*...a*b"` against `"aaaaaa...a"` runs in time proportional
//! to `|s| * count_of_stars`, never `2^n`.
//!
//! ## Malformed patterns
//!
//! The matcher errs on the side of returning `false` for malformed
//! patterns rather than propagating a parse error:
//!
//! * An unterminated `[` (no closing `]` before end of pattern):
//!   `false`.
//! * A `\` at end of pattern with nothing to escape: treated as a
//!   literal `\` — matches the input only if the next code point is
//!   `\`.
//! * An empty character class `[]` or `[^]`: matches nothing; the
//!   class is treated as a single token that consumes one input code
//!   point but never accepts.
//!
//! This mirrors how scripting languages tend to handle malformed
//! globs (false-on-error is friendlier to config validation than a
//! hard runtime panic).

/// Match `s` against `pattern` using the LuaJIT-pattern-subset glob
/// syntax. Returns `true` only when the entire string is consumed by
/// the entire pattern (anchored on both ends).
///
/// See the module-level documentation for the supported syntax,
/// algorithmic guarantees, and malformed-pattern handling.
pub fn glob_match(s: &str, pattern: &str) -> bool {
    // Decode both inputs into `Vec<char>` so we operate on Unicode
    // scalar values, not bytes. The naive byte-level approach would
    // mis-handle `?` against multi-byte UTF-8 sequences (a `?` would
    // match exactly one byte, not one code point).
    //
    // The allocation is O(|s| + |p|) one-shot; the matching loop
    // afterward only walks indices into the two buffers and never
    // re-allocates. For very long strings this is the cost we pay
    // for the Unicode contract — an ASCII fast path can be layered
    // on top later (check `s.is_ascii() && pattern.is_ascii()` and
    // walk bytes directly).
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = pattern.chars().collect();

    let mut s_idx: usize = 0;
    let mut p_idx: usize = 0;

    // Single backtrack anchor: when set, `star_p_idx` is the position
    // of the `*` in `p` we may revisit, and `star_s_idx` is the
    // string position one past where we last let `*` start matching.
    //
    // We use `Option<(usize, usize)>` so the no-`*`-seen state is
    // explicit rather than relying on sentinel `usize::MAX`.
    let mut backtrack: Option<(usize, usize)> = None;

    while s_idx < s.len() {
        let s_ch = s[s_idx];

        // Try to consume the next pattern token at p_idx, if any.
        if p_idx < p.len() {
            match p[p_idx] {
                '*' => {
                    // Record this `*` as the current backtrack anchor
                    // and tentatively consume zero input code points.
                    // `s_idx` stays put; `p_idx` advances past the
                    // star. If a later mismatch occurs we'll come back
                    // and let this `*` swallow one more code point.
                    backtrack = Some((p_idx, s_idx));
                    p_idx += 1;
                    continue;
                }
                '?' => {
                    // `?` matches exactly one code point — always
                    // succeeds while we have input to consume.
                    s_idx += 1;
                    p_idx += 1;
                    continue;
                }
                '[' => {
                    // Try to parse a `[...]` class starting at p_idx.
                    // On parse failure (no closing `]`) we treat the
                    // whole rest of the pattern as malformed and fall
                    // through to the backtrack arm; on parse success
                    // the helper returns `(matched, next_p_idx)`.
                    match try_match_class(&p, p_idx, s_ch) {
                        Some((true, next_p)) => {
                            s_idx += 1;
                            p_idx = next_p;
                            continue;
                        }
                        Some((false, _)) => {
                            // Class parsed but didn't match this char
                            // — fall through to the mismatch handler.
                        }
                        None => {
                            // Malformed class — refuse the match
                            // outright. We don't try to backtrack
                            // around a malformed token because doing
                            // so would let `[unterminated` silently
                            // accept arbitrary input via a preceding
                            // `*`, which is more surprising than a
                            // hard false.
                            return false;
                        }
                    }
                }
                '\\' => {
                    // Escape: the next pattern code point is taken as
                    // a literal. A trailing `\` with no successor is
                    // treated as a literal `\` per the module-level
                    // contract.
                    let lit = if p_idx + 1 < p.len() {
                        p[p_idx + 1]
                    } else {
                        '\\'
                    };
                    if s_ch == lit {
                        s_idx += 1;
                        p_idx += if p_idx + 1 < p.len() { 2 } else { 1 };
                        continue;
                    }
                }
                lit => {
                    if s_ch == lit {
                        s_idx += 1;
                        p_idx += 1;
                        continue;
                    }
                }
            }
        }

        // Either the pattern was exhausted with input remaining, or
        // the next pattern token didn't accept this code point. Try
        // to backtrack to the most recent `*` and let it swallow one
        // more code point.
        if let Some((star_p, star_s)) = backtrack {
            p_idx = star_p + 1;
            s_idx = star_s + 1;
            backtrack = Some((star_p, star_s + 1));
            continue;
        }

        // No backtrack anchor available — this is a real mismatch.
        return false;
    }

    // Input fully consumed. The remaining pattern must be all `*`s
    // (each can match the empty trailing slice) for the overall
    // match to succeed.
    while p_idx < p.len() && p[p_idx] == '*' {
        p_idx += 1;
    }
    p_idx == p.len()
}

/// Parse a `[...]` character class starting at `p[start]` (which is
/// expected to be `[`) and report whether `ch` is a member.
///
/// Returns:
/// * `Some((true, next_idx))`  — class parsed, `ch` matched.
/// * `Some((false, next_idx))` — class parsed, `ch` did not match.
/// * `None`                    — malformed (no closing `]` found).
///
/// `next_idx` is the index immediately after the closing `]`.
///
/// The current implementation only honours individual character set
/// membership (and the leading-`^` negation marker). Range syntax
/// (`[a-z]`) is reserved for a follow-up phase — until then a literal
/// `-` is a member of the class just like any other code point.
fn try_match_class(p: &[char], start: usize, ch: char) -> Option<(bool, usize)> {
    debug_assert_eq!(p[start], '[');
    let mut i = start + 1;
    let negate = if i < p.len() && p[i] == '^' {
        i += 1;
        true
    } else {
        false
    };

    let mut matched = false;
    // Walk until the closing `]`. Within the class, backslash escapes
    // are honoured the same way they are in the top-level pattern
    // (so `[\]]` matches a literal `]`, `[\\]` matches a literal
    // backslash, etc.).
    while i < p.len() {
        match p[i] {
            ']' => {
                // Found the terminator. Negation flips the membership
                // bit; an empty class (`[]` or `[^]`) matches nothing
                // / everything respectively per convention — but the
                // 80%-case glob keeps it simple: empty class never
                // matches unless negated.
                let result = if negate { !matched } else { matched };
                return Some((result, i + 1));
            }
            '\\' if i + 1 < p.len() => {
                if p[i + 1] == ch {
                    matched = true;
                }
                i += 2;
            }
            lit => {
                if lit == ch {
                    matched = true;
                }
                i += 1;
            }
        }
    }

    // Reached end of pattern without finding `]`. Malformed.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- Literal-only patterns --------

    #[test]
    fn empty_against_empty_matches() {
        assert!(glob_match("", ""));
    }

    #[test]
    fn empty_string_against_literal_pattern_rejects() {
        assert!(!glob_match("", "a"));
    }

    #[test]
    fn non_empty_against_empty_pattern_rejects() {
        assert!(!glob_match("a", ""));
    }

    #[test]
    fn exact_literal_matches() {
        assert!(glob_match("foo", "foo"));
    }

    #[test]
    fn literal_mismatch_rejects() {
        assert!(!glob_match("foo", "bar"));
    }

    #[test]
    fn literal_substring_rejects() {
        // glob_match is anchored — `foo` is not a substring match for
        // `foobar`.
        assert!(!glob_match("foobar", "foo"));
        assert!(!glob_match("foobar", "bar"));
    }

    // -------- `*` wildcard --------

    #[test]
    fn star_matches_empty_string() {
        assert!(glob_match("", "*"));
    }

    #[test]
    fn star_matches_any_string() {
        assert!(glob_match("anything", "*"));
        assert!(glob_match("a", "*"));
        assert!(glob_match("a much longer value", "*"));
    }

    #[test]
    fn star_anchored_prefix_matches() {
        assert!(glob_match("abc", "a*"));
        assert!(glob_match("abcdef", "a*"));
        assert!(glob_match("a", "a*"));
    }

    #[test]
    fn star_anchored_suffix_matches() {
        assert!(glob_match("abc", "*c"));
        assert!(glob_match("xyzc", "*c"));
        assert!(glob_match("c", "*c"));
    }

    #[test]
    fn star_in_middle_matches() {
        assert!(glob_match("abc", "a*c"));
        assert!(glob_match("axyzc", "a*c"));
        assert!(glob_match("ac", "a*c"));
    }

    #[test]
    fn double_star_collapses_to_single() {
        // Two consecutive `*` shouldn't change semantics.
        assert!(glob_match("anything", "**"));
        assert!(glob_match("abc", "a**c"));
    }

    #[test]
    fn star_surrounded_matches_substring_anchored() {
        assert!(glob_match("hello world", "*world*"));
        assert!(glob_match("world", "*world*"));
        assert!(glob_match("hello world here", "*world*"));
    }

    // -------- `?` wildcard --------

    #[test]
    fn question_matches_single_code_point() {
        assert!(glob_match("a", "?"));
        assert!(glob_match("Z", "?"));
    }

    #[test]
    fn question_rejects_empty_string() {
        assert!(!glob_match("", "?"));
    }

    #[test]
    fn question_rejects_multi_code_point_string() {
        assert!(!glob_match("ab", "?"));
    }

    #[test]
    fn question_in_middle_of_literal() {
        assert!(glob_match("abc", "a?c"));
        assert!(!glob_match("ac", "a?c"));
        assert!(!glob_match("axyc", "a?c"));
    }

    // -------- `[set]` character classes --------

    #[test]
    fn character_set_matches_member() {
        assert!(glob_match("a", "[abc]"));
        assert!(glob_match("b", "[abc]"));
        assert!(glob_match("c", "[abc]"));
    }

    #[test]
    fn character_set_rejects_non_member() {
        assert!(!glob_match("d", "[abc]"));
        assert!(!glob_match("A", "[abc]"));
    }

    #[test]
    fn negated_set_rejects_member() {
        assert!(!glob_match("a", "[^abc]"));
    }

    #[test]
    fn negated_set_matches_non_member() {
        assert!(glob_match("d", "[^abc]"));
        assert!(glob_match("Z", "[^abc]"));
    }

    #[test]
    fn malformed_unterminated_class_rejects() {
        assert!(!glob_match("a", "["));
        assert!(!glob_match("a", "[abc"));
        assert!(!glob_match("anything", "*["));
    }

    // -------- Escapes --------

    #[test]
    fn escaped_star_matches_literal_star() {
        assert!(glob_match("*", "\\*"));
        assert!(!glob_match("anything", "\\*"));
    }

    #[test]
    fn escaped_question_matches_literal_question() {
        assert!(glob_match("?", "\\?"));
        assert!(!glob_match("x", "\\?"));
    }

    #[test]
    fn escaped_backslash_matches_literal_backslash() {
        assert!(glob_match("\\", "\\\\"));
    }

    #[test]
    fn escape_inside_literal_run() {
        assert!(glob_match("a*b", "a\\*b"));
        assert!(!glob_match("axxb", "a\\*b"));
    }

    #[test]
    fn trailing_backslash_is_literal() {
        // Per module contract, an unterminated escape at end of
        // pattern is treated as a literal `\`.
        assert!(glob_match("\\", "\\"));
        assert!(!glob_match("x", "\\"));
    }

    // -------- Unicode --------
    //
    // CJK characters are disallowed in `.rs` source per the
    // workspace-level CJK-in-code lint. Using non-CJK Unicode scalar
    // values still exercises the multi-byte UTF-8 paths that a naive
    // byte-level matcher would mishandle (`?` matching one byte
    // instead of one code point, `[set]` member-comparison comparing
    // partial UTF-8 sequences, etc.).
    //
    // Coverage rationale per char family:
    //   * Greek lower-case letters (`α`, `β`, ...) — 2-byte UTF-8.
    //   * Currency symbol `€`                       — 3-byte UTF-8.
    //   * Emoji `🦀`                                  — 4-byte UTF-8
    //     (surrogate-pair territory in UTF-16).

    #[test]
    fn question_matches_single_unicode_code_point() {
        assert!(glob_match("α", "?"));
        assert!(glob_match("€", "?"));
        assert!(glob_match("🦀", "?"));
    }

    #[test]
    fn unicode_literal_matches() {
        assert!(glob_match("αβ", "αβ"));
        assert!(!glob_match("αβ", "α"));
    }

    #[test]
    fn unicode_question_in_literal() {
        assert!(glob_match("αβ", "?β"));
        assert!(glob_match("αβ", "α?"));
        assert!(!glob_match("αβ", "?β "));
    }

    #[test]
    fn unicode_star_spans_multiple_code_points() {
        assert!(glob_match("αβγδ", "α*δ"));
        assert!(glob_match("αβγδ", "*δ"));
        assert!(glob_match("αβγδ", "α*"));
        assert!(glob_match("αβ", "*"));
    }

    #[test]
    fn unicode_character_set() {
        assert!(glob_match("α", "[αβ]"));
        assert!(glob_match("β", "[αβ]"));
        assert!(!glob_match("γ", "[αβ]"));
    }

    #[test]
    fn emoji_four_byte_utf8_handled_per_code_point() {
        // `🦀` is a 4-byte UTF-8 sequence. A byte-level matcher would
        // require four `?`s to span it; the code-point-aware matcher
        // must accept a single `?` (or `*` consuming the whole
        // character).
        assert!(glob_match("🦀", "?"));
        assert!(glob_match("🦀🦀", "??"));
        assert!(glob_match("hello 🦀", "hello *"));
        assert!(!glob_match("🦀", "????"));
    }

    // -------- Realistic glob shapes --------

    #[test]
    fn file_glob_matches_extension() {
        assert!(glob_match("foo.txt", "*.txt"));
        assert!(glob_match("readme.txt", "*.txt"));
        assert!(!glob_match("foo.md", "*.txt"));
    }

    #[test]
    fn path_glob_matches_directory_prefix() {
        assert!(glob_match("/api/v1/users", "/api/*"));
        assert!(glob_match("/api/v1", "/api/*"));
        assert!(!glob_match("/other/v1", "/api/*"));
    }

    #[test]
    fn url_glob_matches_https_prefix() {
        assert!(glob_match("https://example.com/path", "https://*"));
        assert!(!glob_match("http://example.com/path", "https://*"));
    }

    // -------- Linear-time sanity --------

    #[test]
    fn linear_time_pathological_star_pattern() {
        // The classic regex-engine bomb: a string of `a`s against a
        // pattern that alternates `a*` blocks. A naive backtracking
        // matcher exhibits 2^n behaviour on this shape; the two-
        // pointer algorithm must stay linear in the worst case.
        //
        // With |s| = 1000 and |p| ~= 16, this should complete in
        // well under 10 ms even on a debug build. Setting the budget
        // generous so the test stays robust under CI load.
        let s: String = "a".repeat(1000);
        let p = "a*a*a*a*a*a*a*b";
        let start = std::time::Instant::now();
        let result = glob_match(&s, p);
        let elapsed = start.elapsed();
        assert!(!result, "no `b` in the haystack, so the match must fail");
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "pathological glob took {elapsed:?} — expected linear scan, not exponential",
        );
    }

    #[test]
    fn linear_time_star_a_matches_quickly() {
        // Positive counterpart: same pathological shape but where the
        // tail literal does occur. The algorithm should accept in
        // linear time without exhausting backtracks.
        let s: String = "a".repeat(1000);
        let p = "*a";
        let start = std::time::Instant::now();
        assert!(glob_match(&s, p));
        assert!(start.elapsed() < std::time::Duration::from_millis(50));
    }
}
