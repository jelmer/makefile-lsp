//! Go-to-definition for Makefiles.

use makefile_lossless::{is_in_prerequisites, variable_at_offset, word_at_offset, Makefile};
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{GotoDefinitionResponse, Location, Position, Uri};

use crate::position::{text_range_to_lsp_range, try_position_to_offset};

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
        assert_goto_line("CC = gcc\nall:\n\t$(CC) main.c\n", Position::new(2, 3), 0);
    }

    #[test]
    fn test_goto_variable_in_prerequisites() {
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
}
