//! Bidirectional offset/position translation for LSP.
//!
//! All four helpers approximate UTF-16 (the LSP wire format) by walking
//! `chars()` of the source string. Multi-byte characters (CJK, emoji)
//! still produce valid positions; offsets pointing inside a multi-byte
//! char are clamped to the char's start.

use lsp_types::{Position, Range};
use relon_parser::{TokenPosition, TokenRange};

/// Convert an LSP position (line + UTF-16 character) into a UTF-8 byte
/// offset against `source`. Tolerant of out-of-range positions: clamps
/// to the source length so we never panic on bad client input.
pub fn position_to_offset(source: &str, position: Position) -> usize {
    let target_line = position.line;
    let target_char = position.character;
    let mut line = 0u32;
    let mut character = 0u32;
    let mut byte = 0;
    for ch in source.chars() {
        if line == target_line && character >= target_char {
            return byte;
        }
        let len = ch.len_utf8();
        if ch == '\n' {
            // If the cursor is past the end of `target_line`, snap to
            // the line break.
            if line == target_line {
                return byte;
            }
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
        byte += len;
    }
    source.len()
}

/// Map a UTF-8 byte offset to an LSP `Position`.
pub fn offset_to_position(source: &str, offset: usize) -> Position {
    let offset = offset.min(source.len());
    let mut line = 0u32;
    let mut character = 0u32;
    let mut byte = 0;
    for ch in source.chars() {
        if byte >= offset {
            break;
        }
        let len = ch.len_utf8();
        if byte + len > offset {
            // Offset lands inside a multi-byte char — clamp to char start.
            break;
        }
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
        byte += len;
    }
    Position { line, character }
}

/// Convert a parser `TokenPosition` to an LSP `Position`.
///
/// Parser uses 1-based lines/columns; LSP uses 0-based. `column` is
/// byte-aligned in the parser; we approximate by treating it as a
/// character index, which matches our offset-based math elsewhere.
pub fn token_position(pos: TokenPosition) -> Position {
    Position {
        line: pos.line.saturating_sub(1),
        character: (pos.column.saturating_sub(1)) as u32,
    }
}

/// Convert a parser `TokenRange` to an LSP `Range`.
pub fn token_range(range: TokenRange) -> Range {
    Range {
        start: token_position(range.start),
        end: token_position(range.end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_to_position_handles_multiline_source() {
        let source = "ab\ncd\nef";
        // offset 6 = 'e' on line 2, col 0
        let pos = offset_to_position(source, 6);
        assert_eq!(pos.line, 2);
        assert_eq!(pos.character, 0);
    }

    #[test]
    fn position_to_offset_round_trips_simple_source() {
        let source = "hello\nworld";
        let pos = Position {
            line: 1,
            character: 2,
        };
        let off = position_to_offset(source, pos);
        assert_eq!(off, 8); // 'r' in world
    }
}
