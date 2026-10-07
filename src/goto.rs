//! Go-to-definition for Makefiles.

use makefile_lossless::Makefile;
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{GotoDefinitionResponse, Location, Position, Range, Uri};

use crate::position::{text_range_to_lsp_range, try_position_to_offset};
use crate::targets::prerequisite_at_offset;
use crate::workspace::{FileSet, Resolution};

/// Find the definition of the symbol at the given position.
///
/// Definitions in the current document win; otherwise the first definition
/// in the other documents, in the order make reads them, is used. On an
/// include file name this jumps to the included file.
pub fn goto_definition(files: &FileSet, position: Position) -> Option<GotoDefinitionResponse> {
    let source_text = files.current().text();
    let offset = try_position_to_offset(source_text, position)?;
    let byte_offset: usize = offset.into();

    let makefile = files.current().makefile();
    if let Some(reference) = makefile.variable_reference_at(offset) {
        // A function name such as `wildcard` is not a variable.
        if reference.is_function_call() {
            return None;
        }
        let var_name = reference.name()?;
        return files.docs().find_map(|doc| {
            find_variable_definition(&doc.makefile(), doc.text(), &var_name, doc.uri())
        });
    }

    if let Some(inc) = files.include_at(offset) {
        let (Resolution::Found(path) | Resolution::Unreadable(path, _)) = &inc.resolution else {
            return None;
        };
        let Some(uri) = Uri::from_file_path(path) else {
            tracing::warn!("unable to convert {} to a URI", path.display());
            return None;
        };
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri,
            range: Range::default(),
        }));
    }

    let (prerequisite, _) = makefile
        .rules()
        .find_map(|rule| prerequisite_at_offset(&rule, byte_offset))?;
    files.docs().find_map(|doc| {
        find_target_definition(&doc.makefile(), doc.text(), &prerequisite, doc.uri())
    })
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
    let var_def = makefile.variable_definitions_by_name(var_name).next()?;

    let range = text_range_to_lsp_range(source_text, var_def.syntax().text_range());

    Some(GotoDefinitionResponse::Scalar(Location {
        uri: uri.clone(),
        range,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::tests::Fixture;
    use crate::workspace::Document;

    fn test_uri() -> Uri {
        "file:///test/Makefile".parse().unwrap()
    }

    fn single(text: &str) -> FileSet {
        FileSet::single(Document::new(test_uri(), text.to_string()))
    }

    fn assert_goto_line(text: &str, pos: Position, expected_line: u32) {
        let result = goto_definition(&single(text), pos);
        match result {
            Some(GotoDefinitionResponse::Scalar(loc)) => {
                assert_eq!(loc.range.start.line, expected_line);
            }
            Some(_) => panic!("Expected scalar response"),
            None => panic!("Expected a definition, got None"),
        }
    }

    fn assert_goto_none(text: &str, pos: Position) {
        let result = goto_definition(&single(text), pos);
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
    fn test_goto_prerequisite_with_directory() {
        assert_goto_line("all: src/foo.o\nsrc/foo.o:\n", Position::new(0, 6), 1);
    }

    #[test]
    fn test_goto_prerequisite_on_continuation_line() {
        assert_goto_line("all: a \\\n  b\nb:\n", Position::new(1, 2), 2);
    }

    #[test]
    fn test_goto_target_name_in_variable_value() {
        assert_goto_none("FOO := x\nx:\n", Position::new(0, 7));
    }

    #[test]
    fn test_goto_function_name() {
        assert_goto_none(
            "wildcard = x\nFILES = $(wildcard *.c)\n",
            Position::new(1, 11),
        );
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

    fn goto(fx: &Fixture, name: &str, pos: Position) -> Option<Location> {
        match goto_definition(&fx.file_set(name), pos) {
            Some(GotoDefinitionResponse::Scalar(loc)) => Some(loc),
            Some(other) => panic!("Expected scalar response, got {:?}", other),
            None => None,
        }
    }

    #[test]
    fn test_goto_variable_in_included_file() {
        let fx = Fixture::new(&[
            ("Makefile", "include rules.mk\nall:\n\t$(CC) x\n"),
            ("rules.mk", "\nCC = gcc\n"),
        ]);
        assert_eq!(
            goto(&fx, "Makefile", Position::new(2, 3)),
            Some(Location {
                uri: fx.uri("rules.mk"),
                range: Range::new(Position::new(1, 0), Position::new(2, 0)),
            })
        );
    }

    #[test]
    fn test_goto_prefers_current_file() {
        let fx = Fixture::new(&[
            (
                "Makefile",
                "include rules.mk\nCC = clang\nall:\n\t$(CC) x\n",
            ),
            ("rules.mk", "CC = gcc\n"),
        ]);
        let loc = goto(&fx, "Makefile", Position::new(3, 3)).unwrap();
        assert_eq!((loc.uri, loc.range.start.line), (fx.uri("Makefile"), 1));
    }

    #[test]
    fn test_goto_target_in_included_file() {
        let fx = Fixture::new(&[
            ("Makefile", "include sub/rules.mk\nall: build\n"),
            ("sub/rules.mk", "build:\n\techo\n"),
        ]);
        let loc = goto(&fx, "Makefile", Position::new(1, 6)).unwrap();
        assert_eq!((loc.uri, loc.range.start.line), (fx.uri("sub/rules.mk"), 0));
    }

    #[test]
    fn test_goto_include_path() {
        let fx = Fixture::new(&[("Makefile", "include a.mk b.mk\n"), ("b.mk", "")]);
        assert_eq!(
            goto(&fx, "Makefile", Position::new(0, 14)),
            Some(Location {
                uri: fx.uri("b.mk"),
                range: Range::default(),
            })
        );
        // a.mk doesn't exist.
        assert_eq!(goto(&fx, "Makefile", Position::new(0, 9)), None);
    }

    #[test]
    fn test_goto_from_included_file_to_includer() {
        let fx = Fixture::new(&[
            ("Makefile", "CC = gcc\ninclude rules.mk\n"),
            ("rules.mk", "all:\n\t$(CC) x\n"),
        ]);
        let (mut ws, makefile) = fx.open("Makefile");
        ws.file_set(&makefile).unwrap();
        let rules = fx.open_in(&mut ws, "rules.mk");
        let set = ws.file_set(&rules).unwrap();
        match goto_definition(&set, Position::new(1, 3)) {
            Some(GotoDefinitionResponse::Scalar(loc)) => {
                assert_eq!((loc.uri, loc.range.start.line), (makefile, 0));
            }
            other => panic!("Expected scalar response, got {:?}", other),
        }
    }
}
