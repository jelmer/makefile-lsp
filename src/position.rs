//! UTF-16 position conversion utilities for LSP protocol compatibility.

use text_size::{TextRange, TextSize};
use tower_lsp_server::ls_types::{Position, Range};

/// Return the UTF-16 code unit length of a string.
pub fn utf16_len(s: &str) -> u32 {
    s.chars().map(|c| c.len_utf16() as u32).sum()
}

/// Convert TextSize (byte offset) to LSP Position (line, UTF-16 code unit offset)
pub fn offset_to_position(text: &str, offset: TextSize) -> Position {
    let mut line = 0u32;
    let mut utf16_col = 0u32;

    for (i, ch) in text.char_indices() {
        let current_offset = TextSize::try_from(i).unwrap();

        if current_offset >= offset {
            break;
        }

        if ch == '\n' {
            line += 1;
            utf16_col = 0;
        } else {
            utf16_col += ch.len_utf16() as u32;
        }
    }

    Position {
        line,
        character: utf16_col,
    }
}

/// Convert TextRange to LSP Range
pub fn text_range_to_lsp_range(text: &str, range: TextRange) -> Range {
    Range {
        start: offset_to_position(text, range.start()),
        end: offset_to_position(text, range.end()),
    }
}

/// Convert LSP Position (line, UTF-16 code unit offset) to TextSize (byte offset)
pub fn try_position_to_offset(text: &str, position: Position) -> Option<TextSize> {
    let mut line = 0u32;
    let mut line_start = 0usize;

    for (i, ch) in text.char_indices() {
        if line == position.line {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = i + 1;
        }
    }

    if line < position.line {
        return None;
    }

    let mut utf16_col = 0u32;
    for (i, ch) in text[line_start..].char_indices() {
        if utf16_col >= position.character {
            return TextSize::try_from(line_start + i).ok();
        }
        if ch == '\n' {
            break;
        }
        utf16_col += ch.len_utf16() as u32;
    }

    if utf16_col >= position.character {
        let line_end = text[line_start..]
            .find('\n')
            .map(|rel| line_start + rel)
            .unwrap_or(text.len());
        return TextSize::try_from(line_end).ok();
    }

    None
}

/// Convert LSP Range to TextRange
pub fn try_lsp_range_to_text_range(text: &str, range: &Range) -> Option<TextRange> {
    let start = try_position_to_offset(text, range.start)?;
    let end = try_position_to_offset(text, range.end)?;
    Some(TextRange::new(start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_offset_to_position_simple() {
        let text = "hello\nworld\n";
        assert_eq!(
            offset_to_position(text, TextSize::from(0u32)),
            Position::new(0, 0)
        );
        assert_eq!(
            offset_to_position(text, TextSize::from(6u32)),
            Position::new(1, 0)
        );
        assert_eq!(
            offset_to_position(text, TextSize::from(8u32)),
            Position::new(1, 2)
        );
    }

    #[test]
    fn test_try_position_to_offset_simple() {
        let text = "hello\nworld\n";
        assert_eq!(
            try_position_to_offset(text, Position::new(0, 0)),
            Some(TextSize::from(0u32))
        );
        assert_eq!(
            try_position_to_offset(text, Position::new(1, 0)),
            Some(TextSize::from(6u32))
        );
    }

    #[test]
    fn test_roundtrip() {
        let text = "all: build\n\t$(CC) -o $@ $^\n";
        let range = Range::new(Position::new(0, 0), Position::new(0, 3));
        let text_range = try_lsp_range_to_text_range(text, &range).unwrap();
        assert_eq!(&text[..usize::from(text_range.end())], "all");
    }

    #[test]
    fn test_utf16_len() {
        assert_eq!(utf16_len("hello"), 5);
        assert_eq!(utf16_len(""), 0);
    }
}
