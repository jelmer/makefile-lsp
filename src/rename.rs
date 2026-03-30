//! Rename support for Makefiles.

use std::collections::HashMap;

use makefile_lossless::{is_in_prerequisites, variable_at_offset, word_at_offset, Makefile};
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{
    Position, PrepareRenameResponse, Range, TextEdit, Uri, WorkspaceEdit,
};

use crate::position::{offset_to_position, text_range_to_lsp_range, try_position_to_offset};

/// Check if renaming is possible at the given position and return the current name and range.
pub fn prepare_rename(
    makefile: &Makefile,
    source_text: &str,
    position: Position,
) -> Option<PrepareRenameResponse> {
    let offset = try_position_to_offset(source_text, position)?;
    let byte_offset: usize = offset.into();

    // Variable reference
    if let Some(var_name) = variable_at_offset(source_text, byte_offset) {
        // Only allow renaming user-defined variables
        if makefile
            .variable_definitions()
            .any(|v| v.name().as_deref() == Some(var_name))
        {
            let start = find_var_name_start_in_ref(source_text, byte_offset)?;
            let start_pos =
                offset_to_position(source_text, text_size::TextSize::from(start as u32));
            let end_pos =
                Position::new(start_pos.line, start_pos.character + var_name.len() as u32);
            return Some(PrepareRenameResponse::RangeWithPlaceholder {
                range: Range::new(start_pos, end_pos),
                placeholder: var_name.to_string(),
            });
        }
        return None;
    }

    // Target in prerequisites
    if is_in_prerequisites(source_text, byte_offset) {
        if let Some(word) = word_at_offset(source_text, byte_offset) {
            let word_start = find_word_start(source_text, byte_offset);
            let start_pos =
                offset_to_position(source_text, text_size::TextSize::from(word_start as u32));
            let end_pos = Position::new(start_pos.line, start_pos.character + word.len() as u32);
            return Some(PrepareRenameResponse::RangeWithPlaceholder {
                range: Range::new(start_pos, end_pos),
                placeholder: word.to_string(),
            });
        }
        return None;
    }

    // Target definition or variable definition at start of line
    if let Some(word) = word_at_offset(source_text, byte_offset) {
        let is_target = makefile.rules().any(|r| {
            r.targets().any(|t| {
                if t != word {
                    return false;
                }
                let rule_start: usize = r.syntax().text_range().start().into();
                byte_offset >= rule_start && byte_offset < rule_start + t.len()
            })
        });
        let is_var = makefile.variable_definitions().any(|v| {
            if v.name().as_deref() != Some(word) {
                return false;
            }
            let var_start: usize = v.syntax().text_range().start().into();
            byte_offset >= var_start && byte_offset < var_start + word.len()
        });

        if is_target || is_var {
            let word_start = find_word_start(source_text, byte_offset);
            let start_pos =
                offset_to_position(source_text, text_size::TextSize::from(word_start as u32));
            let end_pos = Position::new(start_pos.line, start_pos.character + word.len() as u32);
            return Some(PrepareRenameResponse::RangeWithPlaceholder {
                range: Range::new(start_pos, end_pos),
                placeholder: word.to_string(),
            });
        }
    }

    None
}

/// Perform a rename of the symbol at the given position.
pub fn rename(
    makefile: &Makefile,
    source_text: &str,
    position: Position,
    new_name: &str,
    uri: &Uri,
) -> Option<WorkspaceEdit> {
    let offset = try_position_to_offset(source_text, position)?;
    let byte_offset: usize = offset.into();

    // Variable reference
    if let Some(var_name) = variable_at_offset(source_text, byte_offset) {
        if makefile
            .variable_definitions()
            .any(|v| v.name().as_deref() == Some(var_name))
        {
            return Some(rename_variable(
                makefile,
                source_text,
                var_name,
                new_name,
                uri,
            ));
        }
        return None;
    }

    // Target in prerequisites
    if is_in_prerequisites(source_text, byte_offset) {
        if let Some(word) = word_at_offset(source_text, byte_offset) {
            return Some(rename_target(makefile, source_text, word, new_name, uri));
        }
        return None;
    }

    // Target or variable definition
    if let Some(word) = word_at_offset(source_text, byte_offset) {
        let is_target = makefile.rules().any(|r| {
            r.targets().any(|t| {
                if t != word {
                    return false;
                }
                let rule_start: usize = r.syntax().text_range().start().into();
                byte_offset >= rule_start && byte_offset < rule_start + t.len()
            })
        });
        if is_target {
            return Some(rename_target(makefile, source_text, word, new_name, uri));
        }

        let is_var = makefile.variable_definitions().any(|v| {
            if v.name().as_deref() != Some(word) {
                return false;
            }
            let var_start: usize = v.syntax().text_range().start().into();
            byte_offset >= var_start && byte_offset < var_start + word.len()
        });
        if is_var {
            return Some(rename_variable(makefile, source_text, word, new_name, uri));
        }
    }

    None
}

/// Rename all occurrences of a variable.
fn rename_variable(
    makefile: &Makefile,
    source_text: &str,
    old_name: &str,
    new_name: &str,
    uri: &Uri,
) -> WorkspaceEdit {
    let mut edits = Vec::new();

    // Rename in definitions
    for var_def in makefile.variable_definitions() {
        if var_def.name().as_deref() == Some(old_name) {
            let range = text_range_to_lsp_range(source_text, var_def.syntax().text_range());
            let start = range.start;
            let end = Position::new(start.line, start.character + old_name.len() as u32);
            edits.push(TextEdit {
                range: Range::new(start, end),
                new_text: new_name.to_string(),
            });
        }
    }

    // Rename in $(VAR) and ${VAR} references
    let paren_pattern = format!("$({}", old_name);
    let brace_pattern = format!("${{{}", old_name);

    for pattern in [&paren_pattern, &brace_pattern] {
        let close = if pattern.starts_with("$(") { ')' } else { '}' };
        for (idx, _) in source_text.match_indices(pattern.as_str()) {
            let after = idx + pattern.len();
            if after < source_text.len() {
                let next = source_text.as_bytes()[after];
                if next == close as u8 || next == b' ' || next == b')' || next == b'}' {
                    let name_start = idx + 2;
                    let start = offset_to_position(
                        source_text,
                        text_size::TextSize::from(name_start as u32),
                    );
                    let end = Position::new(start.line, start.character + old_name.len() as u32);
                    edits.push(TextEdit {
                        range: Range::new(start, end),
                        new_text: new_name.to_string(),
                    });
                }
            }
        }
    }

    edits.sort_by_key(|e| (e.range.start.line, e.range.start.character));
    edits.dedup_by(|a, b| a.range == b.range);

    let mut changes = HashMap::new();
    changes.insert(uri.clone(), edits);
    WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    }
}

/// Rename all occurrences of a target.
fn rename_target(
    makefile: &Makefile,
    source_text: &str,
    old_name: &str,
    new_name: &str,
    uri: &Uri,
) -> WorkspaceEdit {
    let mut edits = Vec::new();

    for rule in makefile.rules() {
        // Rename in target definitions
        for target in rule.targets() {
            if target == old_name {
                let range = text_range_to_lsp_range(source_text, rule.syntax().text_range());
                let start = range.start;
                let end = Position::new(start.line, start.character + old_name.len() as u32);
                edits.push(TextEdit {
                    range: Range::new(start, end),
                    new_text: new_name.to_string(),
                });
            }
        }

        // Rename in prerequisites
        for prereq in rule.prerequisites() {
            if prereq == old_name {
                collect_word_edits_in_prerequisites(
                    source_text,
                    &rule,
                    old_name,
                    new_name,
                    &mut edits,
                );
            }
        }
    }

    edits.sort_by_key(|e| (e.range.start.line, e.range.start.character));
    edits.dedup_by(|a, b| a.range == b.range);

    let mut changes = HashMap::new();
    changes.insert(uri.clone(), edits);
    WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    }
}

/// Find word occurrences in the prerequisites area and create text edits.
fn collect_word_edits_in_prerequisites(
    source_text: &str,
    rule: &makefile_lossless::Rule,
    word: &str,
    new_name: &str,
    edits: &mut Vec<TextEdit>,
) {
    let rule_range = rule.syntax().text_range();
    let rule_text = &source_text[usize::from(rule_range.start())..usize::from(rule_range.end())];
    let rule_offset: usize = rule_range.start().into();

    if let Some(colon_pos) = rule_text.find(':') {
        let after_colon = &rule_text[colon_pos + 1..];
        let end = after_colon.find('\n').unwrap_or(after_colon.len());
        let prereq_text = &after_colon[..end];
        let prereq_start = rule_offset + colon_pos + 1;

        for (idx, _) in prereq_text.match_indices(word) {
            let abs_offset = prereq_start + idx;
            let before_ok = idx == 0
                || !prereq_text.as_bytes()[idx - 1].is_ascii_alphanumeric()
                    && prereq_text.as_bytes()[idx - 1] != b'_';
            let after_idx = idx + word.len();
            let after_ok = after_idx >= prereq_text.len()
                || !prereq_text.as_bytes()[after_idx].is_ascii_alphanumeric()
                    && prereq_text.as_bytes()[after_idx] != b'_';
            if before_ok && after_ok {
                let start =
                    offset_to_position(source_text, text_size::TextSize::from(abs_offset as u32));
                let end_pos = Position::new(start.line, start.character + word.len() as u32);
                edits.push(TextEdit {
                    range: Range::new(start, end_pos),
                    new_text: new_name.to_string(),
                });
            }
        }
    }
}

/// Find the byte offset of the start of the word at the given offset.
fn find_word_start(text: &str, offset: usize) -> usize {
    let bytes = text.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-';
    (0..offset)
        .rev()
        .take_while(|&i| is_ident(bytes[i]))
        .last()
        .unwrap_or(offset)
}

/// Find the start offset of the variable name within a $() or ${} reference.
fn find_var_name_start_in_ref(text: &str, offset: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = offset;
    while i >= 2 {
        i -= 1;
        if i > 0 && (bytes[i] == b'(' || bytes[i] == b'{') && bytes[i - 1] == b'$' {
            return Some(i + 1);
        }
        if bytes[i] == b')' || bytes[i] == b'}' || bytes[i] == b'\n' {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_uri() -> Uri {
        "file:///test/Makefile".parse().unwrap()
    }

    fn get_edits(text: &str, pos: Position, new_name: &str) -> Vec<TextEdit> {
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let uri = test_uri();
        let result = rename(&makefile, text, pos, new_name, &uri);
        result
            .and_then(|ws| ws.changes.and_then(|c| c.get(&uri).cloned()))
            .unwrap_or_default()
    }

    #[test]
    fn test_rename_variable() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let edits = get_edits(text, Position::new(0, 0), "GCC");
        assert_eq!(edits.len(), 2);
        // First edit: definition
        assert_eq!(edits[0].new_text, "GCC");
        assert_eq!(edits[0].range.start.line, 0);
        // Second edit: usage
        assert_eq!(edits[1].new_text, "GCC");
        assert_eq!(edits[1].range.start.line, 2);
    }

    #[test]
    fn test_rename_variable_from_reference() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let edits = get_edits(text, Position::new(2, 3), "GCC");
        assert_eq!(edits.len(), 2);
    }

    #[test]
    fn test_rename_target() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let edits = get_edits(text, Position::new(2, 0), "compile");
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(|e| e.new_text == "compile"));
    }

    #[test]
    fn test_rename_target_from_prereq() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        let edits = get_edits(text, Position::new(0, 5), "compile");
        assert_eq!(edits.len(), 2);
    }

    #[test]
    fn test_prepare_rename_variable() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let result = prepare_rename(&makefile, text, Position::new(0, 0));
        assert!(result.is_some());
        match result.unwrap() {
            PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. } => {
                assert_eq!(placeholder, "CC");
            }
            _ => panic!("Expected RangeWithPlaceholder"),
        }
    }

    #[test]
    fn test_prepare_rename_nothing() {
        let text = "all:\n\techo hello\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let result = prepare_rename(&makefile, text, Position::new(1, 2));
        assert!(result.is_none());
    }

    #[test]
    fn test_rename_variable_multiple_usages() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\nclean:\n\t$(CC) --version\n";
        let edits = get_edits(text, Position::new(0, 0), "COMPILER");
        assert_eq!(edits.len(), 3);
    }
}
