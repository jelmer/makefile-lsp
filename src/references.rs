//! Find references for Makefiles.

use makefile_lossless::{Makefile, ReferenceLocation, TextRange, TextSize, VariableReference};
use tower_lsp_server::ls_types::{Location, Position, Uri};

use crate::position::{text_range_to_lsp_range, try_position_to_offset};
use crate::targets::{
    prerequisite_at_offset, prerequisites_with_ranges, target_at_offset, targets_with_ranges,
};
use crate::workspace::FileSet;

/// A target or variable name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Symbol {
    Variable(String),
    Target(String),
}

/// Identify the symbol at `byte_offset`: a variable reference, a prerequisite,
/// or the name in a target or variable definition.
pub fn symbol_at(makefile: &Makefile, byte_offset: usize) -> Option<Symbol> {
    let offset = TextSize::from(byte_offset as u32);
    // The innermost reference, for nested ones such as `$(FOO.$(BAR))`.
    let reference = variable_references(makefile)
        .into_iter()
        .filter(|(_, range)| range.start() <= offset && offset <= range.end())
        .min_by_key(|(_, range)| range.len());
    if let Some((name, _)) = reference {
        return Some(Symbol::Variable(name));
    }

    if let Some((target, _)) = makefile.rules().find_map(|r| {
        target_at_offset(&r, byte_offset).or_else(|| prerequisite_at_offset(&r, byte_offset))
    }) {
        return Some(Symbol::Target(target));
    }

    makefile.variable_definitions().find_map(|v| {
        let range = v.name_range()?;
        if byte_offset < usize::from(range.start()) || byte_offset >= usize::from(range.end()) {
            return None;
        }
        v.name().map(Symbol::Variable)
    })
}

/// Find all occurrences of `symbol` in one document.
pub fn symbol_locations(
    makefile: &Makefile,
    source_text: &str,
    uri: &Uri,
    symbol: &Symbol,
    include_declaration: bool,
) -> Vec<Location> {
    match symbol {
        Symbol::Variable(name) => {
            find_variable_references(makefile, source_text, name, uri, include_declaration)
        }
        Symbol::Target(name) => {
            find_target_references(makefile, source_text, name, uri, include_declaration)
        }
    }
}

/// Find all references to the symbol at the given position, in all
/// documents of the file set.
pub fn find_references(
    files: &FileSet,
    position: Position,
    include_declaration: bool,
) -> Vec<Location> {
    let current = files.current();
    let Some(offset) = try_position_to_offset(current.text(), position) else {
        return vec![];
    };
    let Some(symbol) = symbol_at(&current.makefile(), offset.into()) else {
        return vec![];
    };
    files
        .docs()
        .flat_map(|doc| {
            symbol_locations(
                &doc.makefile(),
                doc.text(),
                doc.uri(),
                &symbol,
                include_declaration,
            )
        })
        .collect()
}

/// Find all references to the symbol at the given position within one
/// document.
pub fn find_document_references(
    makefile: &Makefile,
    source_text: &str,
    position: Position,
    uri: &Uri,
    include_declaration: bool,
) -> Vec<Location> {
    let Some(offset) = try_position_to_offset(source_text, position) else {
        return vec![];
    };
    let Some(symbol) = symbol_at(makefile, offset.into()) else {
        return vec![];
    };
    symbol_locations(makefile, source_text, uri, &symbol, include_declaration)
}

/// Find all references to a target name.
fn find_target_references(
    makefile: &Makefile,
    source_text: &str,
    target_name: &str,
    uri: &Uri,
    include_declaration: bool,
) -> Vec<Location> {
    let mut locations = Vec::new();

    for rule in makefile.rules() {
        let declarations = if include_declaration {
            targets_with_ranges(&rule)
        } else {
            vec![]
        };
        for (name, range) in declarations
            .into_iter()
            .chain(prerequisites_with_ranges(&rule))
        {
            if name == target_name {
                locations.push(Location {
                    uri: uri.clone(),
                    range: text_range_to_lsp_range(source_text, range),
                });
            }
        }
    }

    locations.sort_by_key(|l| (l.range.start.line, l.range.start.character));
    locations
}

/// Find all references to a variable name (in $(VAR) or ${VAR} patterns).
fn find_variable_references(
    makefile: &Makefile,
    source_text: &str,
    var_name: &str,
    uri: &Uri,
    include_declaration: bool,
) -> Vec<Location> {
    let mut locations = Vec::new();

    // Find the declaration
    if include_declaration {
        for var_def in makefile.variable_definitions() {
            if var_def.name().as_deref() != Some(var_name) {
                continue;
            }
            if let Some(range) = var_def.name_range() {
                locations.push(Location {
                    uri: uri.clone(),
                    range: text_range_to_lsp_range(source_text, range),
                });
            }
        }
    }

    for (name, range) in variable_references(makefile) {
        if name == var_name {
            locations.push(Location {
                uri: uri.clone(),
                range: text_range_to_lsp_range(source_text, range),
            });
        }
    }

    locations.sort_by_key(|l| (l.range.start.line, l.range.start.character));
    locations
}

/// All references in the document with the range of the variable name.
/// Function calls such as `$(shell ...)` and call parameters such as `$(1)`
/// are left out, but references in function arguments are included.
pub(crate) fn variable_references(makefile: &Makefile) -> Vec<(String, TextRange)> {
    makefile
        .variable_references()
        .filter(|reference| !reference.is_function_call())
        .filter_map(|reference| Some((reference.name()?, reference.name_range()?)))
        .filter(|(name, _)| !crate::builtins::is_call_parameter(name))
        .collect()
}

/// Whether `reference` is in a recipe line, possibly nested in other
/// references.
pub(crate) fn in_recipe(reference: &VariableReference) -> bool {
    let mut outer = reference.clone();
    while let Some(parent) = outer.parent_reference() {
        outer = parent;
    }
    matches!(outer.location(), ReferenceLocation::Recipe(_))
}

/// The name ranges of single-character references such as `$X`.
pub(crate) fn single_char_reference_ranges(makefile: &Makefile) -> Vec<TextRange> {
    makefile
        .variable_references()
        .filter_map(|reference| {
            let range = reference.name_range()?;
            // The name follows the `$` directly, without a parenthesis.
            (range.start() == reference.text_range().start() + TextSize::from(1)).then_some(range)
        })
        .collect()
}

/// Whether `reference` is in the body of a `define`, possibly nested in
/// other references.
pub(crate) fn in_define_body(reference: &VariableReference) -> bool {
    let mut outer = reference.clone();
    while let Some(parent) = outer.parent_reference() {
        outer = parent;
    }
    matches!(outer.location(), ReferenceLocation::VariableValue(def) if def.is_define())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::tests::Fixture;
    use tower_lsp_server::ls_types::Range;

    fn test_uri() -> Uri {
        "file:///test/Makefile".parse().unwrap()
    }

    #[test]
    fn test_find_target_references_from_prereq() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Cursor on "build" in prerequisites (col 5)
        let refs =
            find_document_references(&makefile, text, Position::new(0, 5), &test_uri(), true);
        assert_eq!(refs.len(), 2); // declaration + prerequisite reference
    }

    #[test]
    fn test_find_target_references_from_definition() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Cursor on "build" in its definition (line 2, col 0)
        let refs =
            find_document_references(&makefile, text, Position::new(2, 0), &test_uri(), true);
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn test_find_target_references_second_target_of_rule() {
        let text = "all: b\na b: c\n";
        let makefile = Makefile::parse(text).tree();
        let ranges: Vec<Range> =
            find_document_references(&makefile, text, Position::new(0, 5), &test_uri(), true)
                .into_iter()
                .map(|l| l.range)
                .collect();
        assert_eq!(
            ranges,
            vec![
                Range::new(Position::new(0, 5), Position::new(0, 6)),
                Range::new(Position::new(1, 2), Position::new(1, 3)),
            ]
        );
    }

    #[test]
    fn test_find_target_references_no_declaration() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let refs =
            find_document_references(&makefile, text, Position::new(0, 5), &test_uri(), false);
        assert_eq!(refs.len(), 1); // only the prerequisite reference
    }

    #[test]
    fn test_find_variable_references() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Cursor on CC in $(CC) (line 2, col 3)
        let refs =
            find_document_references(&makefile, text, Position::new(2, 3), &test_uri(), true);
        assert_eq!(refs.len(), 2); // definition + usage
    }

    #[test]
    fn test_find_variable_references_from_definition() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Cursor on CC in definition (line 0, col 0)
        let refs =
            find_document_references(&makefile, text, Position::new(0, 0), &test_uri(), true);
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn test_find_variable_multiple_usages() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\nclean:\n\t$(CC) --version\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let refs =
            find_document_references(&makefile, text, Position::new(0, 0), &test_uri(), true);
        assert_eq!(refs.len(), 3); // definition + 2 usages
    }

    #[test]
    fn test_find_references_nothing() {
        let text = "all:\n\techo hello\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let refs =
            find_document_references(&makefile, text, Position::new(1, 2), &test_uri(), true);
        assert!(refs.is_empty());
    }

    fn range(line: u32, start: u32, end: u32) -> Range {
        Range::new(Position::new(line, start), Position::new(line, end))
    }

    /// References to `FOO`, found from its definition on the first line.
    fn foo_refs(text: &str) -> Vec<Range> {
        assert!(text.starts_with("FOO = 1\n"));
        let makefile = Makefile::parse(text).tree();
        find_document_references(&makefile, text, Position::new(0, 0), &test_uri(), true)
            .into_iter()
            .map(|l| l.range)
            .collect()
    }

    #[test]
    fn test_symbol_at_substitution_reference() {
        let text = "FOO = a.c\nX = $(FOO:.c=.o) ${FOO:%.c=%.o}\n";
        let makefile = Makefile::parse(text).tree();
        let foo = Some(Symbol::Variable("FOO".to_string()));
        assert_eq!(symbol_at(&makefile, 16), foo);
        assert_eq!(symbol_at(&makefile, 19), foo);
        assert_eq!(symbol_at(&makefile, 29), foo);
    }

    #[test]
    fn test_symbol_at_nested_reference() {
        let text = "X = $(subst a,b,$(FOO)) $(BAR.$(Y))\n";
        let makefile = Makefile::parse(text).tree();
        assert_eq!(
            symbol_at(&makefile, 19),
            Some(Symbol::Variable("FOO".to_string()))
        );
        assert_eq!(
            symbol_at(&makefile, 32),
            Some(Symbol::Variable("Y".to_string()))
        );
    }

    #[test]
    fn test_find_variable_references_in_values() {
        let text = "FOO = 1\nX = $(FOO:.c=.o) ${FOO} $(subst a,b,$(FOO)) $(FOO)$(FOO)\n";
        assert_eq!(
            foo_refs(text),
            vec![
                range(0, 0, 3),
                range(1, 6, 9),
                range(1, 19, 22),
                range(1, 38, 41),
                range(1, 46, 49),
                range(1, 52, 55),
            ]
        );
    }

    #[test]
    fn test_find_variable_references_skips_other_names() {
        let text = "FOO = 1\nX = $(FOOBAR) $(FOO.$(Y)) $$(FOO) $(FOO x)\n";
        assert_eq!(foo_refs(text), vec![range(0, 0, 3)]);
    }

    #[test]
    fn test_find_variable_references_in_conditional_headers() {
        let text = "FOO = 1\nifeq ($(FOO:a=b),x)\nendif\nifneq \"$(FOO)\" \"\"\nendif\n";
        assert_eq!(
            foo_refs(text),
            vec![range(0, 0, 3), range(1, 8, 11), range(3, 9, 12)]
        );
    }

    #[test]
    fn test_find_variable_references_in_define_bodies() {
        let text = "FOO = 1\ndefine A\n$(FOO) $(FOO:a=b)\nendef\n\
                    define B\n\techo $(FOO) $$(FOO)\nall: ${FOO}\nendef\n";
        assert_eq!(
            foo_refs(text),
            vec![
                range(0, 0, 3),
                range(2, 2, 5),
                range(2, 9, 12),
                range(5, 8, 11),
                range(6, 7, 10),
            ]
        );
    }

    #[test]
    fn test_find_variable_references_in_recipes() {
        let text = "FOO = 1\nall: ; echo $(FOO)\n\
                    \techo $(FOO:.c=.o) $$(FOO) $(shell ${FOO})\n\
                    ifdef X\n\techo $(FOO)\nendif\n";
        assert_eq!(
            foo_refs(text),
            vec![
                range(0, 0, 3),
                range(1, 14, 17),
                range(2, 8, 11),
                range(2, 37, 40),
                range(4, 8, 11),
            ]
        );
    }

    #[test]
    fn test_find_variable_references_in_orphan_recipes() {
        // Recipe lines outside any rule are a make error, but references in
        // them are still found.
        let text = "FOO = 1\n\techo $(FOO)\nifdef X\n\techo $(FOO)\nendif\n";
        assert_eq!(
            foo_refs(text),
            vec![range(0, 0, 3), range(1, 8, 11), range(3, 8, 11)]
        );
    }

    #[test]
    fn test_find_single_char_reference_in_recipe() {
        let text = "X = 1\nall:\n\techo $X\n";
        assert_eq!(
            ref_ranges(text, Position::new(0, 0)),
            vec![range(0, 0, 1), range(2, 7, 8)]
        );
    }

    #[test]
    fn test_find_single_char_reference_range() {
        // `$FOO` references `F`, followed by the text `OO`.
        let text = "F = 1\nX = $FOO\n";
        assert_eq!(
            ref_ranges(text, Position::new(0, 0)),
            vec![range(0, 0, 1), range(1, 5, 6)]
        );
    }

    #[test]
    fn test_find_variable_references_in_vpath_and_define_names() {
        let text = "FOO = 1\nvpath %.c $(FOO)\ndefine $(FOO)_F\nendef\n";
        assert_eq!(
            foo_refs(text),
            vec![range(0, 0, 3), range(1, 12, 15), range(2, 9, 12)]
        );
    }

    #[test]
    fn test_symbol_at_in_define_body() {
        let text = "define F\n$(1) $(A.${B})\nendef\n";
        let makefile = Makefile::parse(text).tree();
        // `$(1)` is a call parameter, not a variable.
        assert_eq!(symbol_at(&makefile, 11), None);
        assert_eq!(
            symbol_at(&makefile, 16),
            Some(Symbol::Variable("A.${B}".to_string()))
        );
        assert_eq!(
            symbol_at(&makefile, 20),
            Some(Symbol::Variable("B".to_string()))
        );
    }

    #[test]
    fn test_find_variable_references_nested_in_define_body() {
        let text = "B = 1\ndefine F\n$(A.${B}) $(B)\nendef\nX = $(A.${B})\n";
        assert_eq!(
            ref_ranges(text, Position::new(0, 0)),
            vec![
                range(0, 0, 1),
                range(2, 6, 7),
                range(2, 12, 13),
                range(4, 10, 11)
            ]
        );
        assert_eq!(
            ref_ranges(text, Position::new(2, 2)),
            vec![range(2, 2, 8), range(4, 6, 12)]
        );
    }

    #[test]
    fn test_find_variable_references_in_rule_heads() {
        let text = "FOO = 1\n$(FOO): $(FOO:.c=.o)\nall: Z = $(FOO)\n";
        assert_eq!(
            foo_refs(text),
            vec![
                range(0, 0, 3),
                range(1, 2, 5),
                range(1, 10, 13),
                range(2, 11, 14),
            ]
        );
    }

    fn ref_ranges(text: &str, pos: Position) -> Vec<Range> {
        let makefile = Makefile::parse(text).tree();
        find_document_references(&makefile, text, pos, &test_uri(), true)
            .into_iter()
            .map(|l| l.range)
            .collect()
    }

    #[test]
    fn test_symbol_at_escaped_prerequisite() {
        let text = "all: a\\#b\na\\#b:\n";
        let makefile = Makefile::parse(text).tree();
        assert_eq!(
            symbol_at(&makefile, 5),
            Some(Symbol::Target("a#b".to_string()))
        );
        assert_eq!(
            symbol_at(&makefile, 8),
            Some(Symbol::Target("a#b".to_string()))
        );
    }

    #[test]
    fn test_find_references_escaped_prerequisite() {
        let text = "all: a\\#b\na\\#b:\n";
        let expected = vec![range(0, 5, 9), range(1, 0, 4)];
        assert_eq!(ref_ranges(text, Position::new(0, 5)), expected);
        assert_eq!(ref_ranges(text, Position::new(1, 0)), expected);
    }

    #[test]
    fn test_find_references_prerequisite_not_substring() {
        // `a` must not match inside `a.o` or `b-a`.
        let text = "all: a.o b-a a\na:\n";
        assert_eq!(
            ref_ranges(text, Position::new(1, 0)),
            vec![range(0, 13, 14), range(1, 0, 1)]
        );
    }

    #[test]
    fn test_find_references_prerequisite_with_slash() {
        let text = "all: dir/foo\ndir/foo:\n";
        assert_eq!(
            ref_ranges(text, Position::new(0, 10)),
            vec![range(0, 5, 12), range(1, 0, 7)]
        );
    }

    #[test]
    fn test_find_references_prerequisites_on_continuation_line() {
        let text = "all: a \\\n  b\nb:\n";
        assert_eq!(
            ref_ranges(text, Position::new(2, 0)),
            vec![range(1, 2, 3), range(2, 0, 1)]
        );
    }

    #[test]
    fn test_find_references_phony_prerequisite() {
        let text = ".PHONY: build\nbuild:\n";
        assert_eq!(
            ref_ranges(text, Position::new(1, 0)),
            vec![range(0, 8, 13), range(1, 0, 5)]
        );
    }

    fn locations(locs: &[Location]) -> Vec<(Uri, u32, u32)> {
        locs.iter()
            .map(|l| (l.uri.clone(), l.range.start.line, l.range.start.character))
            .collect()
    }

    #[test]
    fn test_find_variable_references_across_files() {
        let fx = Fixture::new(&[
            ("Makefile", "include rules.mk\nall:\n\t$(CC) x\n"),
            ("rules.mk", "CC = gcc\nX = $(CC)\n"),
        ]);
        let refs = find_references(&fx.file_set("Makefile"), Position::new(2, 3), true);
        assert_eq!(
            locations(&refs),
            vec![
                (fx.uri("Makefile"), 2, 3),
                (fx.uri("rules.mk"), 0, 0),
                (fx.uri("rules.mk"), 1, 6),
            ]
        );
    }

    #[test]
    fn test_find_target_references_from_included_file() {
        let fx = Fixture::new(&[
            ("Makefile", "include rules.mk\nall: build\n"),
            ("rules.mk", "build:\n\techo\n"),
        ]);
        let (mut ws, makefile) = fx.open("Makefile");
        ws.file_set(&makefile).unwrap();
        let rules = fx.open_in(&mut ws, "rules.mk");
        let refs = find_references(&ws.file_set(&rules).unwrap(), Position::new(0, 1), false);
        assert_eq!(locations(&refs), vec![(makefile, 1, 5)]);
    }
}
