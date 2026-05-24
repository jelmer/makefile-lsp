//! Code actions for Makefiles.

use std::collections::HashSet;

use makefile_lossless::{Makefile, SyntaxKind};
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{
    CodeAction, CodeActionKind, Position, Range, TextEdit, Uri, WorkspaceEdit,
};

use crate::position::{offset_to_position, text_range_to_lsp_range, try_position_to_offset};

/// Generate code actions for the given range.
pub fn get_code_actions(
    makefile: &Makefile,
    source_text: &str,
    range: Range,
    uri: &Uri,
) -> Vec<CodeAction> {
    let mut actions = Vec::new();

    let Some(offset) = try_position_to_offset(source_text, range.start) else {
        return actions;
    };
    let byte_offset: usize = offset.into();

    actions.extend(add_phony_action(makefile, source_text, byte_offset, uri));
    actions.extend(define_variable_action(
        makefile,
        source_text,
        byte_offset,
        uri,
    ));
    actions.extend(replace_spaces_with_tab_action(
        makefile,
        source_text,
        byte_offset,
        uri,
    ));

    actions
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_and_actions(text: &str, pos: Position) -> Vec<CodeAction> {
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let uri: Uri = "file:///test/Makefile".parse().unwrap();
        let range = Range::new(pos, pos);
        get_code_actions(&makefile, text, range, &uri)
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
}
