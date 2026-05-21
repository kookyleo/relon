//! v3+ a-4 Unicode-aware case folding tables embedded into wasm-AOT
//! `upper` / `lower` stdlib bodies.
//!
//! The tables hold **simple** case-folding mappings only — each entry
//! is a single-codepoint input paired with a single-codepoint
//! replacement. Multi-codepoint cases (e.g. German `ß` -> `SS`,
//! Latin small ligatures, Armenian `\u{0587}` -> `ԵՒ`) are excluded
//! from the simple-folding pass and pass through unchanged in the
//! wasm body. A full case folding pass that handles those is a v3++
//! item.
//!
//! Both tables are sorted ascending by input codepoint so the wasm
//! body's binary search keeps a stable contract. The build.rs sibling
//! generates them at compile time from `char::to_uppercase` /
//! `char::to_lowercase` — Rust's stdlib pulls the data from the
//! bundled Unicode tables, which means our table tracks whichever
//! Unicode version the host toolchain was built against.

// The build.rs generates this file with `pub(crate)` visibility so
// the IR crate's internal modules can access it. We re-export the
// tables via the helper module functions below so the codegen crate
// pulls the data through a stable surface.
include!(concat!(env!("OUT_DIR"), "/case_folding_table.rs"));

// Re-export through `pub` so the codegen crate can splice the table
// bytes into the wasm data section.
//
// Visibility note: the generated file declares these as `pub(crate)`,
// which is too narrow for the codegen crate. We work around it by
// wrapping with `pub` wrappers; callers go through these instead of
// touching the `pub(crate)` consts directly.
/// Public view of the simple upper case-folding table. Sorted by the
/// input codepoint ascending.
pub fn simple_upper_folding() -> &'static [(u32, u32)] {
    SIMPLE_UPPER_FOLDING
}

/// Public view of the simple lower case-folding table. Sorted by the
/// input codepoint ascending.
pub fn simple_lower_folding() -> &'static [(u32, u32)] {
    SIMPLE_LOWER_FOLDING
}

/// Encode the case-folding table into the raw byte layout the wasm
/// data section expects.
///
/// Layout: a 4-byte little-endian entry count followed by `count` *
/// 8 bytes, each entry being `(input_cp: u32 LE, output_cp: u32 LE)`.
/// The header lets the wasm body emit a fixed-shape binary search
/// without taking the table length as a separate constant.
pub fn encode_table_bytes(table: &[(u32, u32)]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + table.len() * 8);
    bytes.extend_from_slice(&(table.len() as u32).to_le_bytes());
    for (k, v) in table {
        bytes.extend_from_slice(&k.to_le_bytes());
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes
}

/// Byte size of the encoded table — header (4 bytes) plus 8 bytes
/// per entry. Codegen uses this to pre-size the data section before
/// it lays out per-table offsets.
pub fn encoded_table_size(table: &[(u32, u32)]) -> usize {
    4 + table.len() * 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upper_table_sorted_and_non_empty() {
        let table = SIMPLE_UPPER_FOLDING;
        assert!(!table.is_empty(), "upper table must not be empty");
        for win in table.windows(2) {
            assert!(win[0].0 < win[1].0, "upper table must be sorted asc");
        }
    }

    #[test]
    fn lower_table_sorted_and_non_empty() {
        let table = SIMPLE_LOWER_FOLDING;
        assert!(!table.is_empty(), "lower table must not be empty");
        for win in table.windows(2) {
            assert!(win[0].0 < win[1].0, "lower table must be sorted asc");
        }
    }

    #[test]
    fn ascii_letters_present() {
        // ASCII a -> A and A -> a must be in the simple-folding tables.
        let upper = SIMPLE_UPPER_FOLDING
            .iter()
            .find(|(k, _)| *k == 'a' as u32)
            .expect("a -> A mapping");
        assert_eq!(upper.1, 'A' as u32);
        let lower = SIMPLE_LOWER_FOLDING
            .iter()
            .find(|(k, _)| *k == 'A' as u32)
            .expect("A -> a mapping");
        assert_eq!(lower.1, 'a' as u32);
    }

    #[test]
    fn cyrillic_letters_present() {
        // U+0420 CYRILLIC CAPITAL LETTER ER -> U+0440 small er.
        let lower = SIMPLE_LOWER_FOLDING
            .iter()
            .find(|(k, _)| *k == 0x0420)
            .expect("Р -> р mapping");
        assert_eq!(lower.1, 0x0440);
        let upper = SIMPLE_UPPER_FOLDING
            .iter()
            .find(|(k, _)| *k == 0x0440)
            .expect("р -> Р mapping");
        assert_eq!(upper.1, 0x0420);
    }

    #[test]
    fn encode_table_bytes_layout() {
        let toy: &[(u32, u32)] = &[(0x61, 0x41), (0x62, 0x42)];
        let bytes = encode_table_bytes(toy);
        assert_eq!(bytes.len(), 4 + 16);
        // Header is a little-endian u32 count.
        assert_eq!(&bytes[0..4], &2u32.to_le_bytes());
        // First entry payload: (0x61, 0x41).
        assert_eq!(&bytes[4..8], &0x61u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &0x41u32.to_le_bytes());
    }
}
