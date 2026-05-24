//! Code actions for Makefiles.

use std::collections::HashSet;

use makefile_lossless::{Makefile, Parse, SyntaxKind, VariableReference};
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{
    CodeAction, CodeActionKind, Position, Range, TextEdit, Uri, WorkspaceEdit,
};

use crate::position::{offset_to_position, text_range_to_lsp_range, try_position_to_offset};

/// Generate code actions for the given range.
pub fn get_code_actions(
    parsed: &Parse<Makefile>,
    source_text: &str,
    range: Range,
    uri: &Uri,
) -> Vec<CodeAction> {
    let mut actions = Vec::new();

    let Some(offset) = try_position_to_offset(source_text, range.start) else {
        return actions;
    };
    let byte_offset: usize = offset.into();

    let makefile = parsed.tree();
    actions.extend(add_phony_action(&makefile, source_text, byte_offset, uri));
    actions.extend(define_variable_action(
        &makefile,
        source_text,
        byte_offset,
        uri,
    ));
    actions.extend(replace_spaces_with_tab_action(
        &makefile,
        source_text,
        byte_offset,
        uri,
    ));
    actions.extend(remove_trailing_whitespace_action(
        parsed,
        source_text,
        byte_offset,
        uri,
    ));
    actions.extend(convert_to_simply_expanded_action(
        parsed,
        source_text,
        byte_offset,
        uri,
    ));

    actions
}

/// Build a TextEdit that replaces `original_range` (offsets in `source_text`)
/// with the current text of the given mutated node.
///
/// Used by code actions that mutate the AST: capture the node's text_range
/// before mutation, then call this with the (now-modified) node to compute
/// the edit.
fn edit_for_node_change(
    source_text: &str,
    original_range: text_size::TextRange,
    mutated_node: &rowan::SyntaxNode<makefile_lossless::Lang>,
) -> TextEdit {
    TextEdit {
        range: text_range_to_lsp_range(source_text, original_range),
        new_text: mutated_node.text().to_string(),
    }
}

/// Offer "Add to .PHONY" for a target name.
fn add_phony_action(
    makefile: &Makefile,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    // Find if cursor is on a target name at the start of a rule
    let target = makefile.rules().find_map(|rule| {
        let rule_range = rule.syntax().text_range();
        let rule_start: usize = rule_range.start().into();
        rule.targets()
            .find(|target| byte_offset >= rule_start && byte_offset < rule_start + target.len())
    })?;

    // Skip if already phony
    if makefile.is_phony(&target) {
        return None;
    }

    // Skip special targets and pattern rules
    if target.starts_with('.') || target.contains('%') {
        return None;
    }

    // Find the insert position: after the last .PHONY line, or at the top of the file
    let edit = if let Some(last_phony) = makefile.rules_by_target(".PHONY").last() {
        // Append to the last .PHONY rule's prerequisites
        let phony_range = last_phony.syntax().text_range();
        let end = offset_to_position(source_text, phony_range.end());
        // Insert before the newline at end of the .PHONY line
        let insert_pos = Position::new(end.line, 0);
        let text = format!(".PHONY: {}\n", target);
        TextEdit {
            range: Range::new(insert_pos, insert_pos),
            new_text: text,
        }
    } else {
        // No .PHONY exists; add at the top
        let insert_pos = Position::new(0, 0);
        let text = format!(".PHONY: {}\n", target);
        TextEdit {
            range: Range::new(insert_pos, insert_pos),
            new_text: text,
        }
    };

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: format!("Add '{}' to .PHONY", target),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Offer "Define variable" for an undefined variable reference.
fn define_variable_action(
    makefile: &Makefile,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    let var_name = makefile_lossless::variable_at_offset(source_text, byte_offset)?;

    // Check if the variable is already defined
    let defined_vars: HashSet<String> = makefile
        .variable_definitions()
        .filter_map(|v| v.name())
        .collect();
    if defined_vars.contains(var_name) {
        return None;
    }

    // Insert at the top of the file
    let insert_pos = Position::new(0, 0);
    let text = format!("{} =\n", var_name);
    let edit = TextEdit {
        range: Range::new(insert_pos, insert_pos),
        new_text: text,
    };

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: format!("Define variable '{}'", var_name),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Offer "Replace spaces with tab" for a recipe line indented with spaces.
fn replace_spaces_with_tab_action(
    makefile: &Makefile,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    let offset = text_size::TextSize::from(byte_offset as u32);

    for rule in makefile.rules() {
        for recipe in rule.recipe_nodes() {
            if !recipe.syntax().text_range().contains(offset) {
                continue;
            }
            // Find the INDENT token
            let indent_token = recipe.syntax().children_with_tokens().find_map(|it| {
                if let Some(token) = it.as_token() {
                    if token.kind() == SyntaxKind::INDENT {
                        return Some(token.clone());
                    }
                }
                None
            })?;

            // Only offer if the indent is spaces, not a tab
            if indent_token.text().starts_with('\t') {
                return None;
            }

            let range = text_range_to_lsp_range(source_text, indent_token.text_range());
            let edit = TextEdit {
                range,
                new_text: "\t".to_string(),
            };

            let mut changes = std::collections::HashMap::new();
            changes.insert(uri.clone(), vec![edit]);

            return Some(CodeAction {
                title: "Replace spaces with tab".to_string(),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
    }
    None
}

/// Offer "Remove trailing whitespace" when the cursor is on a variable
/// definition whose value ends in whitespace.
///
/// Drives the change through `VariableDefinition::trim_trailing_value_whitespace`
/// on a fresh mutable tree, then emits a TextEdit replacing the VARIABLE
/// node's original range with the mutated text.
fn remove_trailing_whitespace_action(
    parsed: &Parse<Makefile>,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    let offset = text_size::TextSize::from(byte_offset as u32);

    let makefile = parsed.tree();
    let mut var_def = makefile
        .variable_definitions()
        .find(|v| v.syntax().text_range().contains(offset))?;

    let original_range = var_def.syntax().text_range();
    if !var_def.trim_trailing_value_whitespace() {
        return None;
    }
    let edit = edit_for_node_change(source_text, original_range, var_def.syntax());

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: "Remove trailing whitespace".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Offer "Use := for shell expansion" on a recursive (`=`) assignment whose
/// value contains a `$(shell ...)` call — under `=`, the shell command would
/// run on every expansion.
///
/// Drives the change through `VariableDefinition::set_assignment_operator`.
fn convert_to_simply_expanded_action(
    parsed: &Parse<Makefile>,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    let offset = text_size::TextSize::from(byte_offset as u32);

    let makefile = parsed.tree();
    let mut var_def = makefile
        .variable_definitions()
        .find(|v| v.syntax().text_range().contains(offset))?;

    if var_def.assignment_operator().as_deref() != Some("=") {
        return None;
    }

    let has_shell = var_def.syntax().descendants().any(|d| {
        VariableReference::cast(d)
            .filter(|v| v.is_function_call() && v.name().as_deref() == Some("shell"))
            .is_some()
    });
    if !has_shell {
        return None;
    }

    let original_range = var_def.syntax().text_range();
    var_def.set_assignment_operator(":=");
    let edit = edit_for_node_change(source_text, original_range, var_def.syntax());

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: "Use := for shell expansion".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_and_actions(text: &str, pos: Position) -> Vec<CodeAction> {
        let parsed = Makefile::parse(text);
        let uri: Uri = "file:///test/Makefile".parse().unwrap();
        let range = Range::new(pos, pos);
        get_code_actions(&parsed, text, range, &uri)
    }

    #[test]
    fn test_add_phony_action() {
        let text = "all: build\n\techo done\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(actions.iter().any(|a| a.title.contains(".PHONY")));
    }

    #[test]
    fn test_no_phony_action_if_already_phony() {
        let text = ".PHONY: all\nall: build\n\techo done\n";
        let actions = parse_and_actions(text, Position::new(1, 0));
        assert!(!actions.iter().any(|a| a.title.contains(".PHONY")));
    }

    #[test]
    fn test_no_phony_action_for_pattern_rule() {
        let text = "%.o: %.c\n\t$(CC) -c $<\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions.iter().any(|a| a.title.contains(".PHONY")));
    }

    #[test]
    fn test_define_variable_action() {
        let text = "CFLAGS = $(UNDEFINED)\n";
        // Position on 'U' in UNDEFINED, col 11
        let actions = parse_and_actions(text, Position::new(0, 11));
        assert!(actions.iter().any(|a| a.title.contains("Define variable")));
    }

    #[test]
    fn test_no_define_action_for_defined_variable() {
        let text = "CC = gcc\nCFLAGS = $(CC)\n";
        // Position on 'C' in $(CC), col 11
        let actions = parse_and_actions(text, Position::new(1, 11));
        assert!(!actions.iter().any(|a| a.title.contains("Define variable")));
    }

    #[test]
    fn test_replace_spaces_with_tab_action() {
        let text = "all:\n    echo done\n";
        // Position on the space-indented recipe line
        let actions = parse_and_actions(text, Position::new(1, 2));
        let tab_actions: Vec<_> = actions
            .iter()
            .filter(|a| a.title.contains("Replace spaces with tab"))
            .collect();
        assert_eq!(tab_actions.len(), 1);

        // Verify the edit replaces spaces with a tab
        let edit = tab_actions[0].edit.as_ref().unwrap();
        let changes = edit.changes.as_ref().unwrap();
        let edits = changes.values().next().unwrap();
        assert_eq!(edits[0].new_text, "\t");
    }

    #[test]
    fn test_no_replace_spaces_for_tab_indented_recipe() {
        let text = "all:\n\techo done\n";
        let actions = parse_and_actions(text, Position::new(1, 2));
        assert!(!actions
            .iter()
            .any(|a| a.title.contains("Replace spaces with tab")));
    }

    fn only_edit(action: &CodeAction) -> &TextEdit {
        let edits = action
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(edits.len(), 1);
        &edits[0]
    }

    /// Apply a single TextEdit to a source string.
    fn apply_edit(source: &str, edit: &TextEdit) -> String {
        let to_byte = |p: Position| {
            let mut byte = 0usize;
            let mut line = 0u32;
            for ch in source.chars() {
                if line == p.line {
                    break;
                }
                if ch == '\n' {
                    line += 1;
                }
                byte += ch.len_utf8();
            }
            // byte now points at the start of the requested line (UTF-8 in this
            // codepath since we only test ASCII).
            byte + p.character as usize
        };
        let start = to_byte(edit.range.start);
        let end = to_byte(edit.range.end);
        let mut result = String::new();
        result.push_str(&source[..start]);
        result.push_str(&edit.new_text);
        result.push_str(&source[end..]);
        result
    }

    #[test]
    fn test_remove_trailing_whitespace_action() {
        let text = "FOO = bar \n";
        let actions = parse_and_actions(text, Position::new(0, 8));
        let action = actions
            .iter()
            .find(|a| a.title == "Remove trailing whitespace")
            .expect("expected quickfix");
        let edit = only_edit(action);
        // The edit replaces the entire VARIABLE node's range with its trimmed text.
        assert_eq!(apply_edit(text, edit), "FOO = bar\n");
    }

    #[test]
    fn test_remove_trailing_whitespace_multiple_spaces() {
        let text = "FOO = bar   \n";
        let actions = parse_and_actions(text, Position::new(0, 8));
        let action = actions
            .iter()
            .find(|a| a.title == "Remove trailing whitespace")
            .unwrap();
        let edit = only_edit(action);
        assert_eq!(apply_edit(text, edit), "FOO = bar\n");
    }

    #[test]
    fn test_no_remove_trailing_whitespace_when_clean() {
        let text = "FOO = bar\n";
        let actions = parse_and_actions(text, Position::new(0, 8));
        assert!(!actions
            .iter()
            .any(|a| a.title == "Remove trailing whitespace"));
    }

    #[test]
    fn test_no_remove_trailing_whitespace_for_empty_value() {
        let text = "FOO = \n";
        let actions = parse_and_actions(text, Position::new(0, 4));
        assert!(!actions
            .iter()
            .any(|a| a.title == "Remove trailing whitespace"));
    }

    #[test]
    fn test_remove_trailing_whitespace_preserves_var_ref() {
        let text = "FOO = $(BAR)  \n";
        let actions = parse_and_actions(text, Position::new(0, 6));
        let action = actions
            .iter()
            .find(|a| a.title == "Remove trailing whitespace")
            .unwrap();
        let edit = only_edit(action);
        assert_eq!(apply_edit(text, edit), "FOO = $(BAR)\n");
    }

    #[test]
    fn test_convert_to_simply_expanded_action() {
        let text = "FILES = $(shell ls)\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Use := for shell expansion")
            .expect("expected quickfix");
        let edit = only_edit(action);
        assert_eq!(apply_edit(text, edit), "FILES := $(shell ls)\n");
    }

    #[test]
    fn test_no_convert_when_already_simply_expanded() {
        let text = "FILES := $(shell ls)\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions
            .iter()
            .any(|a| a.title == "Use := for shell expansion"));
    }

    #[test]
    fn test_no_convert_when_no_shell() {
        let text = "FILES = file1 file2\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions
            .iter()
            .any(|a| a.title == "Use := for shell expansion"));
    }

    #[test]
    fn test_convert_with_shell_nested() {
        let text = "FILES = $(strip $(shell ls))\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Use := for shell expansion")
            .unwrap();
        let edit = only_edit(action);
        assert_eq!(apply_edit(text, edit), "FILES := $(strip $(shell ls))\n");
    }
}
