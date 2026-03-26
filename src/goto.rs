//! Go-to-definition for Makefiles.

use makefile_lossless::Makefile;
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{GotoDefinitionResponse, Location, Position, Uri};

use crate::position::{text_range_to_lsp_range, try_position_to_offset};

/// Extract a variable name from `$(VAR)` or `${VAR}` surrounding the byte offset
/// within `text`. Returns `None` if the offset is not inside a variable reference.
fn variable_at_offset(text: &str, offset: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut start = None;
    let mut i = offset;
    while i >= 2 {
        i -= 1;
        if i > 0 && (bytes[i] == b'(' || bytes[i] == b'{') && bytes[i - 1] == b'$' {
            start = Some(i + 1);
            break;
        }
        if bytes[i] == b')' || bytes[i] == b'}' || bytes[i] == b'\n' {
            return None;
        }
    }
    let start = start?;
    let rest = &text[start..];
    let end = rest.find([')', '}'])?;
    let var_name = &rest[..end];
    if offset >= start && offset <= start + end {
        Some(var_name)
    } else {
        None
    }
}

/// Extract the word (identifier) at the given byte offset.
fn word_at_offset(text: &str, offset: usize) -> Option<&str> {
    if offset > text.len() {
        return None;
    }
    let bytes = text.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-';
    if offset < text.len() && !is_ident(bytes[offset]) {
        return None;
    }
    let start = (0..offset)
        .rev()
        .take_while(|&i| is_ident(bytes[i]))
        .last()
        .unwrap_or(offset);
    let end = (offset..text.len())
        .take_while(|&i| is_ident(bytes[i]))
        .last()
        .map(|i| i + 1)
        .unwrap_or(offset);
    if start == end {
        return None;
    }
    Some(&text[start..end])
}

/// Determine if the given byte offset is in the prerequisites area of a rule line
/// (i.e. after the first `:` on a non-recipe line).
fn is_in_prerequisites(text: &str, offset: usize) -> bool {
    let line_start = text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &text[line_start..];
    // Recipe lines start with a tab
    if line.starts_with('\t') {
        return false;
    }
    let col = offset - line_start;
    // Check if there's a `:` before our position on this line
    line[..col].contains(':')
}

/// Find the definition of the symbol at the given position.
pub fn goto_definition(
    makefile: &Makefile,
    source_text: &str,
    position: Position,
    uri: &Uri,
) -> Option<GotoDefinitionResponse> {
    let offset = try_position_to_offset(source_text, position)?;
    let byte_offset: usize = offset.into();

    // Check if cursor is inside a variable reference $(VAR) or ${VAR}
    if let Some(var_name) = variable_at_offset(source_text, byte_offset) {
        return find_variable_definition(makefile, source_text, var_name, uri);
    }

    // Check if cursor is on a word in the prerequisites area
    if is_in_prerequisites(source_text, byte_offset) {
        if let Some(word) = word_at_offset(source_text, byte_offset) {
            return find_target_definition(makefile, source_text, word, uri);
        }
    }

    None
}

/// Find the definition of a target by name.
fn find_target_definition(
    makefile: &Makefile,
    source_text: &str,
    target_name: &str,
    uri: &Uri,
) -> Option<GotoDefinitionResponse> {
    let rule = makefile
        .rules()
        .find(|r| r.targets().any(|t| t == target_name))?;

    let range = text_range_to_lsp_range(source_text, rule.syntax().text_range());

    Some(GotoDefinitionResponse::Scalar(Location {
        uri: uri.clone(),
        range,
    }))
}

/// Find the definition of a variable by name.
fn find_variable_definition(
    makefile: &Makefile,
    source_text: &str,
    var_name: &str,
    uri: &Uri,
) -> Option<GotoDefinitionResponse> {
    let var_def = makefile
        .variable_definitions()
        .find(|v| v.name().as_deref() == Some(var_name))?;

    let range = text_range_to_lsp_range(source_text, var_def.syntax().text_range());

    Some(GotoDefinitionResponse::Scalar(Location {
        uri: uri.clone(),
        range,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_uri() -> Uri {
        "file:///test/Makefile".parse().unwrap()
    }

    fn assert_goto_line(text: &str, pos: Position, expected_line: u32) {
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let result = goto_definition(&makefile, text, pos, &test_uri());
        match result {
            Some(GotoDefinitionResponse::Scalar(loc)) => {
                assert_eq!(loc.range.start.line, expected_line);
            }
            Some(_) => panic!("Expected scalar response"),
            None => panic!("Expected a definition, got None"),
        }
    }

    fn assert_goto_none(text: &str, pos: Position) {
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let result = goto_definition(&makefile, text, pos, &test_uri());
        assert!(result.is_none(), "Expected None, got {:?}", result);
    }

    #[test]
    fn test_goto_prerequisite_found() {
        assert_goto_line("all: build\n\nbuild:\n\techo ok\n", Position::new(0, 5), 2);
    }

    #[test]
    fn test_goto_prerequisite_not_found() {
        assert_goto_none("all: build\n\nbuilder:\n\techo ok\n", Position::new(0, 5));
    }

    #[test]
    fn test_goto_variable_in_recipe() {
        // Line 2: "\t$(CC) main.c", CC at col 3
        assert_goto_line("CC = gcc\nall:\n\t$(CC) main.c\n", Position::new(2, 3), 0);
    }

    #[test]
    fn test_goto_variable_in_prerequisites() {
        // Line 1: "all: $(OBJS)", OBJS at col 7
        assert_goto_line("OBJS = main.o\nall: $(OBJS)\n", Position::new(1, 7), 0);
    }

    #[test]
    fn test_goto_no_definition() {
        assert_goto_none("all:\n\techo hello\n", Position::new(1, 2));
    }

    #[test]
    fn test_goto_undefined_variable() {
        assert_goto_none("all:\n\t$(UNDEFINED) foo\n", Position::new(1, 3));
    }

    #[test]
    fn test_variable_at_offset_parens() {
        assert_eq!(variable_at_offset("$(FOO)", 2), Some("FOO"));
        assert_eq!(variable_at_offset("$(FOO)", 4), Some("FOO"));
    }

    #[test]
    fn test_variable_at_offset_braces() {
        assert_eq!(variable_at_offset("${BAR}", 2), Some("BAR"));
    }

    #[test]
    fn test_variable_at_offset_none() {
        assert_eq!(variable_at_offset("plain text", 3), None);
    }

    #[test]
    fn test_word_at_offset() {
        assert_eq!(word_at_offset("hello world", 0), Some("hello"));
        assert_eq!(word_at_offset("hello world", 3), Some("hello"));
        assert_eq!(word_at_offset("hello world", 5), None); // space
        assert_eq!(word_at_offset("hello world", 6), Some("world"));
    }

    #[test]
    fn test_is_in_prerequisites() {
        let text = "all: build test\n\techo ok\n";
        assert!(!is_in_prerequisites(text, 0)); // 'a' in target
        assert!(is_in_prerequisites(text, 5));  // 'b' in prerequisites
        assert!(!is_in_prerequisites(text, 17)); // 'e' in recipe
    }
}
