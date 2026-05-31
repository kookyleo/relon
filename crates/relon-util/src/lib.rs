//! Leaf utility helpers shared across Relon crates.
//!
//! This crate is deliberately a **leaf**: it depends on no other
//! `relon-*` crate. It collects tiny, byte-identical helpers that were
//! previously copy-pasted across the codegen / ABI / build crates so a
//! single definition is shared (and unit-tested) in one place.

/// Round `value` up to the next multiple of `align`. `align` is
/// expected to be a power of two.
pub fn align_up(value: u32, align: u32) -> u32 {
    let rem = value % align;
    if rem == 0 {
        value
    } else {
        value + (align - rem)
    }
}

/// Lightweight check that a string is a valid Rust identifier
/// (`[A-Za-z_][A-Za-z0-9_]*`). This does not reject Rust keywords —
/// callers that splice the string into generated `mod foo { ... }` /
/// `fn foo` shapes will surface a keyword conflict at compile time,
/// which is loud enough for the trivial demo paths that use it.
pub fn is_valid_rust_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_already_aligned_is_identity() {
        assert_eq!(align_up(0, 8), 0);
        assert_eq!(align_up(8, 8), 8);
        assert_eq!(align_up(16, 8), 16);
        assert_eq!(align_up(64, 8), 64);
    }

    #[test]
    fn align_up_rounds_up_to_next_multiple() {
        assert_eq!(align_up(1, 8), 8);
        assert_eq!(align_up(7, 8), 8);
        assert_eq!(align_up(9, 8), 16);
        assert_eq!(align_up(15, 8), 16);
        assert_eq!(align_up(17, 8), 24);
    }

    #[test]
    fn align_up_align_one_is_identity() {
        assert_eq!(align_up(0, 1), 0);
        assert_eq!(align_up(1, 1), 1);
        assert_eq!(align_up(42, 1), 42);
        assert_eq!(align_up(u32::MAX, 1), u32::MAX);
    }

    #[test]
    fn align_up_non_power_of_two_align() {
        // The function does not require a power-of-two `align`; it
        // simply rounds to the next multiple.
        assert_eq!(align_up(10, 4), 12);
        assert_eq!(align_up(12, 4), 12);
        assert_eq!(align_up(5, 3), 6);
    }

    #[test]
    fn is_valid_rust_ident_accepts_normal_idents() {
        assert!(is_valid_rust_ident("foo"));
        assert!(is_valid_rust_ident("Foo"));
        assert!(is_valid_rust_ident("_foo"));
        assert!(is_valid_rust_ident("_"));
        assert!(is_valid_rust_ident("foo_bar_42"));
        assert!(is_valid_rust_ident("a1"));
        assert!(is_valid_rust_ident("X"));
        // Keyword-ish strings still pass the lexical check — keyword
        // rejection is intentionally left to the Rust compiler.
        assert!(is_valid_rust_ident("fn"));
        assert!(is_valid_rust_ident("match"));
    }

    #[test]
    fn is_valid_rust_ident_rejects_invalid() {
        assert!(!is_valid_rust_ident(""));
        assert!(!is_valid_rust_ident("1foo"));
        assert!(!is_valid_rust_ident("9"));
        assert!(!is_valid_rust_ident("foo-bar"));
        assert!(!is_valid_rust_ident("foo bar"));
        assert!(!is_valid_rust_ident("foo.bar"));
        assert!(!is_valid_rust_ident("foo!"));
        // Non-ASCII is rejected (ASCII-only by design). Built from
        // `char` escapes so the source file stays pure ASCII.
        let accented: String = ['c', 'a', 'f', '\u{e9}'].iter().collect(); // "cafe" + e-acute
        assert!(!is_valid_rust_ident(&accented));
        let cyrillic: String = ['\u{43f}', '\u{440}'].iter().collect(); // Cyrillic "pr"
        assert!(!is_valid_rust_ident(&cyrillic));
        let cjk: String = ['\u{540d}', '\u{524d}'].iter().collect(); // CJK ideographs
        assert!(!is_valid_rust_ident(&cjk));
        let n_tilde: String = ['\u{f1}'].iter().collect(); // n-tilde
        assert!(!is_valid_rust_ident(&n_tilde));
    }
}
