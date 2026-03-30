//! Code actions for Makefiles.

use std::collections::HashSet;

use makefile_lossless::Makefile;
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{
    CodeAction, CodeActionKind, Position, Range, TextEdit, Uri, WorkspaceEdit,
};

use crate::position::{offset_to_position, try_position_to_offset};

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
        // No .PHONY exists — add at the top
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
        // Position on 'U' in UNDEFINED — col 11
        let actions = parse_and_actions(text, Position::new(0, 11));
        assert!(actions.iter().any(|a| a.title.contains("Define variable")));
    }

    #[test]
    fn test_no_define_action_for_defined_variable() {
        let text = "CC = gcc\nCFLAGS = $(CC)\n";
        // Position on 'C' in $(CC) — col 11
        let actions = parse_and_actions(text, Position::new(1, 11));
        assert!(!actions.iter().any(|a| a.title.contains("Define variable")));
    }
}
