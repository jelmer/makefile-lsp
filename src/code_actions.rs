//! Code actions for Makefiles.

use std::collections::HashSet;

use makefile_lossless::{Conditional, Makefile, Parse, SyntaxKind, VariableReference};
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
    actions.extend(add_missing_endif_action(
        parsed,
        source_text,
        byte_offset,
        uri,
    ));
    actions.extend(remove_from_phony_action(
        parsed,
        source_text,
        byte_offset,
        uri,
    ));
    actions.extend(sort_phony_prerequisites_action(
        parsed,
        source_text,
        byte_offset,
        uri,
    ));
    actions.extend(replace_all_spaces_with_tabs_action(
        parsed,
        source_text,
        uri,
    ));
    actions.extend(inline_variable_action(
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

/// Offer "Add missing endif" when the cursor is inside a conditional block
/// that has no matching `endif`.
///
/// Drives the change through `Conditional::add_endif`.
fn add_missing_endif_action(
    parsed: &Parse<Makefile>,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    let offset = text_size::TextSize::from(byte_offset as u32);

    let makefile = parsed.tree();
    // Find the innermost Conditional containing the cursor that is missing an
    // endif and has a recognized opener.
    let mut cond = makefile
        .syntax()
        .descendants()
        .filter_map(Conditional::cast)
        .filter(|c| c.syntax().text_range().contains_inclusive(offset))
        .filter(|c| c.conditional_type().is_some())
        .filter(|c| {
            !c.syntax()
                .children_with_tokens()
                .any(|child| child.kind() == SyntaxKind::CONDITIONAL_ENDIF)
        })
        .max_by_key(|c| c.syntax().text_range().start())?;

    let original_range = cond.syntax().text_range();
    if !cond.add_endif().ok()? {
        return None;
    }
    let edit = edit_for_node_change(source_text, original_range, cond.syntax());

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: "Add missing endif".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Offer "Remove from .PHONY" when the cursor is on a name listed as a
/// prerequisite of a `.PHONY` rule and that name has no actual target
/// definition in the makefile.
///
/// Drives the change through `Makefile::remove_phony_target`, which also
/// removes the `.PHONY` rule entirely if the removed name was its only
/// prerequisite. The edit is emitted as a whole-document replacement since
/// the affected node may disappear from the tree.
fn remove_from_phony_action(
    parsed: &Parse<Makefile>,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    let offset = text_size::TextSize::from(byte_offset as u32);

    let makefile = parsed.tree();

    // Find a .PHONY rule whose PREREQUISITE node contains the cursor.
    let target_name = makefile.rules_by_target(".PHONY").find_map(|rule| {
        let prereqs = rule
            .syntax()
            .children()
            .find(|c| c.kind() == SyntaxKind::PREREQUISITES)?;
        let prereq = prereqs
            .children()
            .filter(|c| c.kind() == SyntaxKind::PREREQUISITE)
            .find(|c| c.text_range().contains(offset))?;
        Some(prereq.text().to_string().trim().to_string())
    })?;

    // Skip if the name actually has a target definition somewhere.
    let defined_targets: HashSet<String> = makefile
        .rules()
        .flat_map(|r| r.targets().collect::<Vec<_>>())
        .collect();
    if defined_targets.contains(&target_name) {
        return None;
    }

    // Mutate on a fresh tree and emit a whole-document edit, since the affected
    // .PHONY rule may be removed entirely (and its node would disappear).
    let mut mutated = parsed.tree();
    let removed = mutated.remove_phony_target(&target_name).ok()?;
    if !removed {
        return None;
    }
    let new_text = mutated.code();

    let doc_range = Range::new(
        offset_to_position(source_text, text_size::TextSize::from(0)),
        offset_to_position(
            source_text,
            text_size::TextSize::from(source_text.len() as u32),
        ),
    );
    let edit = TextEdit {
        range: doc_range,
        new_text,
    };

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: format!("Remove '{}' from .PHONY", target_name),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Offer "Sort .PHONY prerequisites" when the cursor is on a `.PHONY` rule
/// whose prerequisites aren't already in lexicographic order.
///
/// Drives the change through `Rule::set_prerequisites`.
fn sort_phony_prerequisites_action(
    parsed: &Parse<Makefile>,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    let offset = text_size::TextSize::from(byte_offset as u32);

    let makefile = parsed.tree();
    let mut rule = makefile
        .rules_by_target(".PHONY")
        .find(|r| r.syntax().text_range().contains(offset))?;

    let current: Vec<String> = rule.prerequisites().collect();
    if current.len() < 2 {
        return None;
    }
    let mut sorted = current.clone();
    sorted.sort();
    if sorted == current {
        return None;
    }

    let original_range = rule.syntax().text_range();
    let sorted_refs: Vec<&str> = sorted.iter().map(|s| s.as_str()).collect();
    rule.set_prerequisites(sorted_refs).ok()?;
    let edit = edit_for_node_change(source_text, original_range, rule.syntax());

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: "Sort .PHONY prerequisites".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Offer "Convert all space-indented recipes to tabs" when there are at
/// least two space-indented recipe lines in the file. Bulk variant of
/// `replace_spaces_with_tab_action`.
fn replace_all_spaces_with_tabs_action(
    parsed: &Parse<Makefile>,
    source_text: &str,
    uri: &Uri,
) -> Option<CodeAction> {
    let makefile = parsed.tree();

    let mut edits: Vec<TextEdit> = Vec::new();
    for rule in makefile.rules() {
        for recipe in rule.recipe_nodes() {
            let Some(indent_token) = recipe.syntax().children_with_tokens().find_map(|it| {
                it.as_token()
                    .filter(|t| t.kind() == SyntaxKind::INDENT)
                    .cloned()
            }) else {
                continue;
            };
            if indent_token.text().starts_with('\t') {
                continue;
            }
            let range = text_range_to_lsp_range(source_text, indent_token.text_range());
            edits.push(TextEdit {
                range,
                new_text: "\t".to_string(),
            });
        }
    }

    if edits.len() < 2 {
        return None;
    }

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), edits);

    Some(CodeAction {
        title: "Convert all space-indented recipes to tabs".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Offer "Inline variable" when the cursor is on a variable definition with a
/// simple literal value (no `$` characters in the value).
///
/// Replaces every `$(NAME)` / `${NAME}` reference visible to the parser AND
/// every such reference in recipe TEXT (byte-scanned) with the literal
/// value, then deletes the variable definition's line.
///
/// Only offered for plain assignments (`=`, `:=`, `::=`, `:::=`). `+=`,
/// `?=`, and `!=` have semantics we don't want to inline silently.
///
/// TODO: drop the recipe byte-scan once makefile-lossless tokenizes recipes
/// structurally.
fn inline_variable_action(
    parsed: &Parse<Makefile>,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    let offset = text_size::TextSize::from(byte_offset as u32);

    let makefile = parsed.tree();
    let var_def = makefile
        .variable_definitions()
        .find(|v| v.syntax().text_range().contains(offset))?;
    let name = var_def.name()?;
    let op = var_def.assignment_operator()?;
    if !matches!(op.as_str(), "=" | ":=" | "::=" | ":::=") {
        return None;
    }
    if var_def.is_export() || var_def.is_override() {
        return None;
    }
    let value = var_def.raw_value()?;
    // Only inline values that are plain literals: no variable references,
    // function calls, or `$$` escapes. We'd otherwise be reasoning about
    // expansion order.
    if value.contains('$') {
        return None;
    }

    // Collect edits for AST-visible references.
    let mut edits: Vec<TextEdit> = Vec::new();
    for var_ref in makefile.variable_references() {
        if var_ref.name().as_deref() != Some(name.as_str()) {
            continue;
        }
        // Skip references inside the variable's own value EXPR (shouldn't
        // happen since we required no `$` in value, but defensive).
        if var_def
            .syntax()
            .text_range()
            .contains_range(var_ref.text_range())
        {
            continue;
        }
        let range = text_range_to_lsp_range(source_text, var_ref.text_range());
        edits.push(TextEdit {
            range,
            new_text: value.clone(),
        });
    }

    // Collect edits for references inside recipe TEXT (byte-scanned).
    for rule in makefile.rules() {
        for recipe in rule.recipe_nodes() {
            for token in recipe
                .syntax()
                .descendants_with_tokens()
                .filter_map(|c| c.into_token())
            {
                if token.kind() != SyntaxKind::TEXT {
                    continue;
                }
                let base: u32 = token.text_range().start().into();
                for (start, end) in scan_named_var_ref_offsets(token.text(), &name) {
                    let range = text_size::TextRange::new(
                        text_size::TextSize::from(base + start as u32),
                        text_size::TextSize::from(base + end as u32),
                    );
                    edits.push(TextEdit {
                        range: text_range_to_lsp_range(source_text, range),
                        new_text: value.clone(),
                    });
                }
            }
        }
    }

    if edits.is_empty() {
        return None;
    }

    // Remove the variable definition's line — its full text range plus any
    // trailing newline already covered by the definition node.
    let def_range = var_def.syntax().text_range();
    edits.push(TextEdit {
        range: text_range_to_lsp_range(source_text, def_range),
        new_text: String::new(),
    });

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), edits);

    Some(CodeAction {
        title: format!("Inline variable '{}'", name),
        kind: Some(CodeActionKind::REFACTOR_INLINE),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Scan recipe text for `$(NAME)` and `${NAME}` references to a specific
/// variable, returning byte offsets covering the whole reference. `$$` is
/// treated as an escape and skipped.
fn scan_named_var_ref_offsets(text: &str, name: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        if i + 1 >= bytes.len() {
            break;
        }
        let next = bytes[i + 1];
        if next == b'$' {
            i += 2;
            continue;
        }
        if next == b'(' || next == b'{' {
            let close = if next == b'(' { b')' } else { b'}' };
            let inner_start = i + 2;
            let mut j = inner_start;
            while j < bytes.len() {
                let b = bytes[j];
                if b == close || b == b' ' || b == b'\t' || b == b'\n' {
                    break;
                }
                j += 1;
            }
            if j > inner_start
                && j < bytes.len()
                && bytes[j] == close
                && &bytes[inner_start..j] == name.as_bytes()
            {
                out.push((i, j + 1));
            }
            i += 1;
            continue;
        }
        i += 2; // single-char auto var or other special
    }
    out
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

    #[test]
    fn test_add_missing_endif_action() {
        let text = "ifdef DEBUG\nVAR = 1\n";
        let actions = parse_and_actions(text, Position::new(1, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Add missing endif")
            .expect("expected quickfix");
        let edit = only_edit(action);
        assert_eq!(apply_edit(text, edit), "ifdef DEBUG\nVAR = 1\nendif\n");
    }

    #[test]
    fn test_no_add_endif_when_already_terminated() {
        let text = "ifdef DEBUG\nVAR = 1\nendif\n";
        let actions = parse_and_actions(text, Position::new(1, 0));
        assert!(!actions.iter().any(|a| a.title == "Add missing endif"));
    }

    #[test]
    fn test_no_add_endif_outside_conditional() {
        let text = "VAR = 1\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions.iter().any(|a| a.title == "Add missing endif"));
    }

    #[test]
    fn test_add_endif_picks_innermost() {
        // Nested: outer is unterminated, inner is terminated. Cursor inside
        // inner should still offer the action for the outer (since the inner
        // is fine).
        let text = "ifdef OUTER\nifdef INNER\nVAR = 1\nendif\n";
        let actions = parse_and_actions(text, Position::new(2, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Add missing endif")
            .unwrap();
        let edit = only_edit(action);
        assert_eq!(
            apply_edit(text, edit),
            "ifdef OUTER\nifdef INNER\nVAR = 1\nendif\nendif\n"
        );
    }

    #[test]
    fn test_no_add_endif_for_bare_else() {
        let text = "else\nVAR = 1\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions.iter().any(|a| a.title == "Add missing endif"));
    }

    /// Apply a TextEdit whose range may span the entire document.
    fn apply_doc_edit(source: &str, edit: &TextEdit) -> String {
        // For the .PHONY tests the edit covers the whole document.
        if edit.range.start.line == 0 && edit.range.start.character == 0 {
            edit.new_text.clone()
        } else {
            apply_edit(source, edit)
        }
    }

    #[test]
    fn test_remove_from_phony_action() {
        // 'clean' is in .PHONY but has no actual target → action should fire.
        let text = ".PHONY: clean\n";
        let actions = parse_and_actions(text, Position::new(0, 9));
        let action = actions
            .iter()
            .find(|a| a.title == "Remove 'clean' from .PHONY")
            .expect("expected quickfix");
        let edit = only_edit(action);
        assert_eq!(apply_doc_edit(text, edit), "");
    }

    #[test]
    fn test_remove_from_phony_one_of_many() {
        // 'clean' is undefined; 'build' has a real target. Action fires on
        // 'clean' but leaves 'build' alone.
        let text = ".PHONY: clean build\nbuild:\n\techo build\n";
        let actions = parse_and_actions(text, Position::new(0, 9));
        let action = actions
            .iter()
            .find(|a| a.title == "Remove 'clean' from .PHONY")
            .unwrap();
        let edit = only_edit(action);
        let result = apply_doc_edit(text, edit);
        // We don't pin the exact whitespace; we just verify 'clean' is gone
        // and 'build' is still there.
        assert!(!result.contains("clean"));
        assert!(result.contains("build"));
    }

    #[test]
    fn test_no_remove_from_phony_when_target_defined() {
        let text = ".PHONY: clean\nclean:\n\trm -f *.o\n";
        let actions = parse_and_actions(text, Position::new(0, 9));
        assert!(!actions
            .iter()
            .any(|a| a.title.starts_with("Remove '") && a.title.contains("from .PHONY")));
    }

    #[test]
    fn test_no_remove_from_phony_when_cursor_elsewhere() {
        let text = ".PHONY: clean\nbuild:\n\techo done\n";
        // Cursor on 'build:' line, not on the .PHONY prereq.
        let actions = parse_and_actions(text, Position::new(1, 0));
        assert!(!actions
            .iter()
            .any(|a| a.title.starts_with("Remove '") && a.title.contains("from .PHONY")));
    }

    #[test]
    fn test_sort_phony_prerequisites_action() {
        let text = ".PHONY: test build clean\ntest:\nbuild:\nclean:\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Sort .PHONY prerequisites")
            .expect("expected sort action");
        let edit = only_edit(action);
        let result = apply_edit(text, edit);
        assert!(result.starts_with(".PHONY: build clean test\n"));
    }

    #[test]
    fn test_no_sort_when_already_sorted() {
        let text = ".PHONY: build clean test\nbuild:\nclean:\ntest:\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions
            .iter()
            .any(|a| a.title == "Sort .PHONY prerequisites"));
    }

    #[test]
    fn test_no_sort_with_single_prerequisite() {
        let text = ".PHONY: clean\nclean:\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions
            .iter()
            .any(|a| a.title == "Sort .PHONY prerequisites"));
    }

    #[test]
    fn test_no_sort_when_cursor_not_on_phony() {
        let text = "build: foo bar baz\n\techo done\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions
            .iter()
            .any(|a| a.title == "Sort .PHONY prerequisites"));
    }

    /// Apply multiple TextEdits in reverse-offset order to avoid invalidating later ranges.
    fn apply_edits(source: &str, edits: &[TextEdit]) -> String {
        let mut sorted: Vec<&TextEdit> = edits.iter().collect();
        sorted.sort_by(|a, b| {
            b.range
                .start
                .line
                .cmp(&a.range.start.line)
                .then(b.range.start.character.cmp(&a.range.start.character))
        });
        let mut result = source.to_string();
        for edit in sorted {
            result = apply_edit(&result, edit);
        }
        result
    }

    #[test]
    fn test_replace_all_spaces_with_tabs_action() {
        let text = "all:\n    echo one\nfoo:\n  echo two\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Convert all space-indented recipes to tabs")
            .expect("expected bulk quickfix");
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
        assert_eq!(edits.len(), 2);
        let result = apply_edits(text, edits);
        assert_eq!(result, "all:\n\techo one\nfoo:\n\techo two\n");
    }

    #[test]
    fn test_no_bulk_action_when_only_one_space_recipe() {
        // The per-line action is offered; the bulk one isn't (we want at least two).
        let text = "all:\n    echo one\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions
            .iter()
            .any(|a| a.title == "Convert all space-indented recipes to tabs"));
    }

    #[test]
    fn test_no_bulk_action_when_all_tab_indented() {
        let text = "all:\n\techo one\nfoo:\n\techo two\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions
            .iter()
            .any(|a| a.title == "Convert all space-indented recipes to tabs"));
    }

    #[test]
    fn test_bulk_action_offered_anywhere_in_file() {
        // Cursor on a completely unrelated line (a variable definition) still
        // sees the bulk action — it's not cursor-position-dependent.
        let text = "VAR = 1\nall:\n    echo one\nfoo:\n    echo two\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(actions
            .iter()
            .any(|a| a.title == "Convert all space-indented recipes to tabs"));
    }

    #[test]
    fn test_inline_variable_in_other_value() {
        let text = "CC = gcc\nCFLAGS = $(CC) -Wall\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Inline variable 'CC'")
            .expect("expected inline action");
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
        let result = apply_edits(text, edits);
        assert_eq!(result, "CFLAGS = gcc -Wall\n");
    }

    #[test]
    fn test_inline_variable_in_recipe() {
        let text = "OUT = build/out\nall:\n\tmkdir -p $(OUT)\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Inline variable 'OUT'")
            .unwrap();
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
        let result = apply_edits(text, edits);
        assert_eq!(result, "all:\n\tmkdir -p build/out\n");
    }

    #[test]
    fn test_inline_variable_braced_form() {
        let text = "OUT = dist\nall:\n\tcp ${OUT}/foo .\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Inline variable 'OUT'")
            .unwrap();
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
        let result = apply_edits(text, edits);
        assert_eq!(result, "all:\n\tcp dist/foo .\n");
    }

    #[test]
    fn test_no_inline_when_unused() {
        let text = "FOO = bar\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions
            .iter()
            .any(|a| a.title.starts_with("Inline variable")));
    }

    #[test]
    fn test_no_inline_when_value_has_dollar() {
        let text = "FOO = $(BAR)\nBAZ = $(FOO)\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions.iter().any(|a| a.title == "Inline variable 'FOO'"));
    }

    #[test]
    fn test_no_inline_for_append_assignment() {
        let text = "FOO += bar\nall:\n\techo $(FOO)\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions.iter().any(|a| a.title == "Inline variable 'FOO'"));
    }

    #[test]
    fn test_no_inline_for_conditional_assignment() {
        let text = "FOO ?= bar\nall:\n\techo $(FOO)\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions.iter().any(|a| a.title == "Inline variable 'FOO'"));
    }

    #[test]
    fn test_no_inline_for_exported_variable() {
        let text = "export FOO = bar\nall:\n\techo $(FOO)\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions.iter().any(|a| a.title == "Inline variable 'FOO'"));
    }

    #[test]
    fn test_no_inline_for_override() {
        let text = "override FOO = bar\nall:\n\techo $(FOO)\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(!actions.iter().any(|a| a.title == "Inline variable 'FOO'"));
    }

    #[test]
    fn test_dollar_dollar_in_recipe_not_inlined() {
        // `$$FOO` is shell expansion; `$(FOO)` is the make ref.
        let text = "FOO = bar\nall:\n\techo $$FOO $(FOO)\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Inline variable 'FOO'")
            .unwrap();
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
        let result = apply_edits(text, edits);
        assert_eq!(result, "all:\n\techo $$FOO bar\n");
    }
}
