//! Find references for Makefiles.

use makefile_lossless::{is_in_prerequisites, variable_at_offset, word_at_offset, Makefile};
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{Location, Position, Range, Uri};

use crate::position::{text_range_to_lsp_range, try_position_to_offset};
use crate::targets::{target_at_offset, targets_with_ranges};
use crate::workspace::FileSet;

/// A target or variable name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Symbol {
    Variable(String),
    Target(String),
}

/// Identify the symbol at `byte_offset`: a variable reference, a prerequisite,
/// or the name in a target or variable definition.
pub fn symbol_at(makefile: &Makefile, source_text: &str, byte_offset: usize) -> Option<Symbol> {
    if let Some(var_name) = variable_at_offset(source_text, byte_offset) {
        return Some(Symbol::Variable(var_name.to_string()));
    }

    let word = word_at_offset(source_text, byte_offset)?;
    if is_in_prerequisites(source_text, byte_offset) {
        return Some(Symbol::Target(word.to_string()));
    }

    if let Some((target, _)) = makefile
        .rules()
        .find_map(|r| target_at_offset(&r, byte_offset))
    {
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
    let Some(symbol) = symbol_at(&current.makefile(), current.text(), offset.into()) else {
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
    let Some(symbol) = symbol_at(makefile, source_text, offset.into()) else {
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
        // Target definitions
        if include_declaration {
            for (target, range) in targets_with_ranges(&rule) {
                if target == target_name {
                    locations.push(Location {
                        uri: uri.clone(),
                        range: text_range_to_lsp_range(source_text, range),
                    });
                }
            }
        }

        // Prerequisite references
        for prereq in rule.prerequisites() {
            if prereq == target_name {
                // Find the prerequisite in the source text by scanning
                find_word_in_prerequisites(source_text, &rule, target_name, uri, &mut locations);
            }
        }
    }

    // Also find references in .PHONY and similar
    for rule in makefile.rules() {
        let targets: Vec<String> = rule.targets().collect();
        if targets.iter().any(|t| t.starts_with('.')) && targets.iter().all(|t| t != target_name) {
            for prereq in rule.prerequisites() {
                if prereq == target_name {
                    find_word_in_prerequisites(
                        source_text,
                        &rule,
                        target_name,
                        uri,
                        &mut locations,
                    );
                }
            }
        }
    }

    locations.sort_by_key(|l| (l.range.start.line, l.range.start.character));
    locations.dedup_by(|a, b| a.range == b.range);
    locations
}

/// Find occurrences of a word in the prerequisites area of a rule.
fn find_word_in_prerequisites(
    source_text: &str,
    rule: &makefile_lossless::Rule,
    word: &str,
    uri: &Uri,
    locations: &mut Vec<Location>,
) {
    let rule_range = rule.syntax().text_range();
    let rule_text = &source_text[usize::from(rule_range.start())..usize::from(rule_range.end())];
    let rule_offset: usize = rule_range.start().into();

    // Find the colon in the rule line
    if let Some(colon_pos) = rule_text.find(':') {
        let after_colon = &rule_text[colon_pos + 1..];
        // Find newline (end of prerequisites line)
        let end = after_colon.find('\n').unwrap_or(after_colon.len());
        let prereq_text = &after_colon[..end];
        let prereq_start = rule_offset + colon_pos + 1;

        for (idx, _) in prereq_text.match_indices(word) {
            let abs_offset = prereq_start + idx;
            // Verify it's a whole word match
            let before_ok = idx == 0
                || !prereq_text.as_bytes()[idx - 1].is_ascii_alphanumeric()
                    && prereq_text.as_bytes()[idx - 1] != b'_';
            let after_idx = idx + word.len();
            let after_ok = after_idx >= prereq_text.len()
                || !prereq_text.as_bytes()[after_idx].is_ascii_alphanumeric()
                    && prereq_text.as_bytes()[after_idx] != b'_';
            if before_ok && after_ok {
                let start = crate::position::offset_to_position(
                    source_text,
                    text_size::TextSize::from(abs_offset as u32),
                );
                let end = Position::new(start.line, start.character + word.len() as u32);
                locations.push(Location {
                    uri: uri.clone(),
                    range: Range::new(start, end),
                });
            }
        }
    }
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

    // Find all $(VAR) and ${VAR} references in source text
    let paren_pattern = format!("$({}", var_name);
    let brace_pattern = format!("${{{}", var_name);

    for pattern in [&paren_pattern, &brace_pattern] {
        let close = if pattern.starts_with("$(") { ')' } else { '}' };
        for (idx, _) in source_text.match_indices(pattern.as_str()) {
            let after = idx + pattern.len();
            // Check that the next char is the closing delimiter or whitespace/comma (for functions)
            if after < source_text.len() {
                let next = source_text.as_bytes()[after];
                if next == close as u8 || next == b' ' || next == b')' || next == b'}' {
                    // The variable name starts after "$(" or "${"
                    let name_start = idx + 2;
                    let start = crate::position::offset_to_position(
                        source_text,
                        text_size::TextSize::from(name_start as u32),
                    );
                    let end = Position::new(start.line, start.character + var_name.len() as u32);
                    locations.push(Location {
                        uri: uri.clone(),
                        range: Range::new(start, end),
                    });
                }
            }
        }
    }

    locations.sort_by_key(|l| (l.range.start.line, l.range.start.character));
    locations.dedup_by(|a, b| a.range == b.range);
    locations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::tests::Fixture;

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
