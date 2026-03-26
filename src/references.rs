//! Find references for Makefiles.

use makefile_lossless::{is_in_prerequisites, variable_at_offset, word_at_offset, Makefile};
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{Location, Position, Range, Uri};

use crate::position::{text_range_to_lsp_range, try_position_to_offset};

/// Find all references to the symbol at the given position.
pub fn find_references(
    makefile: &Makefile,
    source_text: &str,
    position: Position,
    uri: &Uri,
    include_declaration: bool,
) -> Vec<Location> {
    let Some(offset) = try_position_to_offset(source_text, position) else {
        return vec![];
    };
    let byte_offset: usize = offset.into();

    // Variable reference
    if let Some(var_name) = variable_at_offset(source_text, byte_offset) {
        return find_variable_references(makefile, source_text, var_name, uri, include_declaration);
    }

    // Target name in prerequisites
    if is_in_prerequisites(source_text, byte_offset) {
        if let Some(word) = word_at_offset(source_text, byte_offset) {
            return find_target_references(makefile, source_text, word, uri, include_declaration);
        }
    }

    // Target name at line start (the definition itself)
    if let Some(word) = word_at_offset(source_text, byte_offset) {
        // Check if this word is a target in a rule definition
        let is_target = makefile.rules().any(|r| {
            r.targets().any(|t| {
                if t != word {
                    return false;
                }
                let rule_range = r.syntax().text_range();
                let rule_start: usize = rule_range.start().into();
                // The target should be near the start of the rule
                byte_offset >= rule_start && byte_offset < rule_start + t.len()
            })
        });
        if is_target {
            return find_target_references(makefile, source_text, word, uri, include_declaration);
        }

        // Check if this word is a variable definition name
        let is_var_def = makefile.variable_definitions().any(|v| {
            if v.name().as_deref() != Some(word) {
                return false;
            }
            let var_range = v.syntax().text_range();
            let var_start: usize = var_range.start().into();
            byte_offset >= var_start && byte_offset < var_start + word.len()
        });
        if is_var_def {
            return find_variable_references(
                makefile,
                source_text,
                word,
                uri,
                include_declaration,
            );
        }
    }

    vec![]
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
            for target in rule.targets() {
                if target == target_name {
                    let range =
                        text_range_to_lsp_range(source_text, rule.syntax().text_range());
                    // Narrow to just the target name at the start
                    let start = range.start;
                    let end = Position::new(start.line, start.character + target.len() as u32);
                    locations.push(Location {
                        uri: uri.clone(),
                        range: Range::new(start, end),
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
                    find_word_in_prerequisites(source_text, &rule, target_name, uri, &mut locations);
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
            if var_def.name().as_deref() == Some(var_name) {
                let range =
                    text_range_to_lsp_range(source_text, var_def.syntax().text_range());
                let start = range.start;
                let end = Position::new(start.line, start.character + var_name.len() as u32);
                locations.push(Location {
                    uri: uri.clone(),
                    range: Range::new(start, end),
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
                    let end =
                        Position::new(start.line, start.character + var_name.len() as u32);
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

    fn test_uri() -> Uri {
        "file:///test/Makefile".parse().unwrap()
    }

    #[test]
    fn test_find_target_references_from_prereq() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Cursor on "build" in prerequisites (col 5)
        let refs = find_references(&makefile, text, Position::new(0, 5), &test_uri(), true);
        assert_eq!(refs.len(), 2); // declaration + prerequisite reference
    }

    #[test]
    fn test_find_target_references_from_definition() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Cursor on "build" in its definition (line 2, col 0)
        let refs = find_references(&makefile, text, Position::new(2, 0), &test_uri(), true);
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn test_find_target_references_no_declaration() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let refs = find_references(&makefile, text, Position::new(0, 5), &test_uri(), false);
        assert_eq!(refs.len(), 1); // only the prerequisite reference
    }

    #[test]
    fn test_find_variable_references() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Cursor on CC in $(CC) (line 2, col 3)
        let refs = find_references(&makefile, text, Position::new(2, 3), &test_uri(), true);
        assert_eq!(refs.len(), 2); // definition + usage
    }

    #[test]
    fn test_find_variable_references_from_definition() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Cursor on CC in definition (line 0, col 0)
        let refs = find_references(&makefile, text, Position::new(0, 0), &test_uri(), true);
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn test_find_variable_multiple_usages() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\nclean:\n\t$(CC) --version\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let refs = find_references(&makefile, text, Position::new(0, 0), &test_uri(), true);
        assert_eq!(refs.len(), 3); // definition + 2 usages
    }

    #[test]
    fn test_find_references_nothing() {
        let text = "all:\n\techo hello\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let refs = find_references(&makefile, text, Position::new(1, 2), &test_uri(), true);
        assert!(refs.is_empty());
    }
}
