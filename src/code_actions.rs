//! Code actions for Makefiles.

use std::collections::HashSet;

use std::path::Path;

use makefile_lossless::{
    Conditional, Include, Makefile, Parse, ParseErrorKind, Recipe, Rule, SyntaxKind,
    VariableDefinition, VariableReference,
};
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{
    CodeAction, CodeActionKind, Diagnostic, NumberOrString, Position, Range, TextEdit, Uri,
    WorkspaceEdit,
};

use crate::position::{offset_to_position, text_range_to_lsp_range, try_position_to_offset};
use crate::targets::target_at_offset;
use crate::workspace::FileSet;

/// Generate code actions for the given range.
///
/// `diagnostics` are the diagnostics the client sent along with the request;
/// quick fixes that resolve one of them are linked to it. Actions are only
/// offered for the current document of `files`; the other makefiles are
/// consulted for what they define.
pub fn get_code_actions(
    files: &FileSet,
    range: Range,
    diagnostics: &[Diagnostic],
) -> Vec<CodeAction> {
    let mut actions = Vec::new();
    let current = files.current();
    let parsed = current.parsed();
    let source_text = current.text();
    let uri = current.uri();

    let Some(offset) = try_position_to_offset(source_text, range.start) else {
        return actions;
    };
    let byte_offset: usize = offset.into();

    let makefile = parsed.tree();
    actions.extend(add_phony_action(
        &makefile,
        source_text,
        byte_offset,
        uri,
        diagnostics,
    ));
    actions.extend(define_variable_action(
        &makefile,
        source_text,
        byte_offset,
        uri,
    ));
    actions.extend(replace_spaces_with_tab_action(
        parsed,
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
    actions.extend(add_missing_endef_action(
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
    actions.extend(attach_to_default_goal_action(
        parsed,
        source_text,
        byte_offset,
        uri,
    ));
    actions.extend(inline_prerequisite_action(
        parsed,
        source_text,
        byte_offset,
        uri,
    ));
    actions.extend(make_include_optional_action(
        &makefile,
        source_text,
        byte_offset,
        uri,
    ));
    // Whether a file exists can only be checked for documents on disk.
    if let Some(dir) = current.dir() {
        actions.extend(create_target_action(
            files,
            &makefile,
            source_text,
            byte_offset,
            uri,
            dir,
        ));
    }

    actions
}

/// Offer "Create target for X" on a prerequisite that no rule in the file
/// set builds and that is not an existing file, appending an empty `X:`
/// rule at the end of the file.
fn create_target_action(
    files: &FileSet,
    makefile: &Makefile,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
    base_dir: &Path,
) -> Option<CodeAction> {
    let offset = text_size::TextSize::from(byte_offset as u32);
    let prerequisite = makefile
        .syntax()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::PREREQUISITE)
        .find(|n| n.text_range().contains(offset))?;
    let rule = prerequisite.ancestors().find_map(Rule::cast)?;
    // Prerequisites of special targets such as .SUFFIXES are not files to
    // build, except for .PHONY, whose entries should have a rule.
    if rule.targets().any(|t| t.starts_with('.') && t != ".PHONY") {
        return None;
    }

    // Look the name up as the parser reads it, e.g. with `\#` unescaped.
    let index = prerequisite
        .parent()?
        .children()
        .filter(|n| n.kind() == SyntaxKind::PREREQUISITE)
        .position(|n| n == prerequisite)?;
    let name = rule
        .prerequisites()
        .chain(rule.order_only_prerequisites())
        .nth(index)?;
    if name.contains(['$', '%'])
        || files
            .docs()
            .any(|doc| is_built_by_rule(&doc.makefile(), &name))
    {
        return None;
    }
    if base_dir.join(&name).exists() {
        return None;
    }

    let eol = if source_text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let separator = if source_text.is_empty() || source_text.ends_with(&format!("{eol}{eol}")) {
        String::new()
    } else if source_text.ends_with('\n') {
        eol.to_string()
    } else {
        format!("{eol}{eol}")
    };
    let end = offset_to_position(source_text, text_size::TextSize::of(source_text));
    let edit = TextEdit {
        range: Range::new(end, end),
        new_text: format!(
            "{separator}{}:{eol}",
            prerequisite.text().to_string().trim()
        ),
    };

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: format!("Create target for '{}'", name),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Whether some explicit or pattern rule builds `name`.
fn is_built_by_rule(makefile: &Makefile, name: &str) -> bool {
    makefile
        .rules()
        .filter(|rule| rule.operator().is_some())
        .flat_map(|rule| rule.targets().collect::<Vec<_>>())
        .any(|target| match target.split_once('%') {
            Some((prefix, suffix)) => {
                name.len() > prefix.len() + suffix.len()
                    && name.starts_with(prefix)
                    && name.ends_with(suffix)
            }
            None => target == name,
        })
}

/// Offer "Change include to -include" on an `include` directive, so that
/// make ignores included files that do not exist (see the
/// missing-include-file diagnostic).
fn make_include_optional_action(
    makefile: &Makefile,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    let offset = text_size::TextSize::from(byte_offset as u32);
    let include = makefile
        .syntax()
        .descendants()
        .filter(|n| n.text_range().contains(offset))
        .find_map(Include::cast)?;
    if include.is_optional() {
        return None;
    }
    // BSD (`.include`) and nmake (`!include`) keywords have no `-include`
    // spelling.
    let keyword = include
        .syntax()
        .first_token()
        .filter(|t| t.text() == "include")?;

    let edit = TextEdit {
        range: text_range_to_lsp_range(source_text, keyword.text_range()),
        new_text: "-include".to_string(),
    };
    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: "Change include to -include".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
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
///
/// Linked to any `missing-phony` diagnostic for that target.
fn add_phony_action(
    makefile: &Makefile,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
    diagnostics: &[Diagnostic],
) -> Option<CodeAction> {
    let offset = text_size::TextSize::from(byte_offset as u32);
    let (target, target_range) = makefile.rules().find_map(|rule| {
        crate::diagnostics::target_name_ranges(&rule)
            .into_iter()
            .find(|(_, range)| range.contains_inclusive(offset))
    })?;

    // Skip if already phony
    if makefile.is_phony(&target) {
        return None;
    }

    // Skip special targets and pattern rules
    if target.starts_with('.') || target.contains('%') {
        return None;
    }

    let target_lsp_range = text_range_to_lsp_range(source_text, target_range);
    let fixes: Vec<Diagnostic> = diagnostics
        .iter()
        .filter(|d| {
            d.code == Some(NumberOrString::String("missing-phony".to_string()))
                && d.range == target_lsp_range
        })
        .cloned()
        .collect();

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
        is_preferred: (!fixes.is_empty()).then_some(true),
        diagnostics: (!fixes.is_empty()).then_some(fixes),
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
    parsed: &Parse<Makefile>,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    let indent = space_indented_recipes(parsed, source_text).find(|range| {
        let start = usize::from(range.start());
        start <= byte_offset && !source_text[start..byte_offset].contains('\n')
    })?;
    tab_edit_action("Replace spaces with tab", vec![indent], source_text, uri)
}

/// Ranges of the leading spaces of recipe lines indented with spaces.
fn space_indented_recipes<'a>(
    parsed: &'a Parse<Makefile>,
    source_text: &'a str,
) -> impl Iterator<Item = text_size::TextRange> + 'a {
    parsed
        .positioned_errors()
        .iter()
        .filter_map(|error| crate::diagnostics::space_indent_range(source_text, error))
}

/// Build a quick fix replacing each of `ranges` with a tab.
fn tab_edit_action(
    title: &str,
    ranges: Vec<text_size::TextRange>,
    source_text: &str,
    uri: &Uri,
) -> Option<CodeAction> {
    let edits = ranges
        .into_iter()
        .map(|range| TextEdit {
            range: text_range_to_lsp_range(source_text, range),
            new_text: "\t".to_string(),
        })
        .collect();

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), edits);

    Some(CodeAction {
        title: title.to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
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

/// Offer "Insert missing endef" when the cursor is inside a `define` block
/// that runs to the end of the file without a matching `endef`.
///
/// makefile-lossless has no API to add an `endef`, so this appends it as text.
fn add_missing_endef_action(
    parsed: &Parse<Makefile>,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    let offset = text_size::TextSize::from(byte_offset as u32);

    let error = parsed
        .positioned_errors()
        .iter()
        .find(|e| e.kind() == ParseErrorKind::MissingEndef)?;
    // The error covers the `define` keyword of the unterminated block.
    let define = parsed
        .tree()
        .syntax()
        .covering_element(error.range)
        .ancestors()
        .find(|n| n.kind() == SyntaxKind::VARIABLE)?;
    if !define.text_range().contains_inclusive(offset) {
        return None;
    }

    // Nested defines are part of the body text, so count how many are open.
    let body = define.text().to_string();
    let depth = body
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .fold(0usize, |depth, word| match word {
            "define" => depth + 1,
            "endef" => depth.saturating_sub(1),
            _ => depth,
        });
    if depth == 0 {
        return None;
    }

    let mut new_text = String::new();
    if !body.ends_with('\n') {
        new_text.push('\n');
    }
    new_text.push_str(&"endef\n".repeat(depth));
    let end = offset_to_position(source_text, define.text_range().end());
    let edit = TextEdit {
        range: Range::new(end, end),
        new_text,
    };

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: "Insert missing endef".to_string(),
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
    let ranges: Vec<_> = space_indented_recipes(parsed, source_text).collect();
    if ranges.len() < 2 {
        return None;
    }
    tab_edit_action(
        "Convert all space-indented recipes to tabs",
        ranges,
        source_text,
        uri,
    )
}

/// Offer "Inline variable" when the cursor is on a variable definition with a
/// simple literal value (no `$` characters in the value).
///
/// Replaces every `$(NAME)` / `${NAME}` reference, including those in
/// recipes and define bodies, with the literal value, then deletes the
/// variable definition's line. Not offered if a recipe or define body uses
/// the variable with modifiers, as in `$(NAME:.c=.o)`.
///
/// Only offered for plain assignments (`=`, `:=`, `::=`, `:::=`). `+=`,
/// `?=`, and `!=` have semantics we don't want to inline silently.
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

    // Recipes and define bodies are raw text, so their references are not
    // in the syntax tree. Recipe lines outside rules are included.
    for node in makefile.syntax().descendants() {
        let raw_refs = if let Some(recipe) = Recipe::cast(node.clone()) {
            recipe.variable_references()
        } else if let Some(definition) = VariableDefinition::cast(node) {
            definition.define_variable_references()
        } else {
            continue;
        };
        for raw_ref in raw_refs.iter().filter(|r| r.name() == name) {
            let range = plain_reference_range(source_text, raw_ref.text_range())?;
            edits.push(TextEdit {
                range: text_range_to_lsp_range(source_text, range),
                new_text: value.clone(),
            });
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

/// The range of the whole `$(NAME)` or `${NAME}` reference whose name is at
/// `name_range`, or None if the name is followed by modifiers.
// TODO: use the reference's own range if makefile-lossless provides one for
// references in recipes and define bodies.
fn plain_reference_range(
    source_text: &str,
    name_range: text_size::TextRange,
) -> Option<text_size::TextRange> {
    let start = usize::from(name_range.start()).checked_sub(2)?;
    let end = usize::from(name_range.end());
    let close = match source_text.get(start..start + 2)? {
        "$(" => ")",
        "${" => "}",
        _ => return None,
    };
    source_text[end..].starts_with(close).then(|| {
        text_size::TextRange::new(
            text_size::TextSize::from(start as u32),
            name_range.end() + text_size::TextSize::of(close),
        )
    })
}

/// Offer "Add '<target>' as prerequisite of '<goal>'" when the cursor is on
/// the target name of an orphan rule (one with no incoming dependency edges).
///
/// The goal is picked as: a rule with target `all`, else `default`, else the
/// first non-pattern non-special rule in the file. The action is suppressed
/// when the target *is* the chosen goal, when it's already a prerequisite, or
/// when there's no usable goal rule.
fn attach_to_default_goal_action(
    parsed: &Parse<Makefile>,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    let makefile = parsed.tree();

    // Cursor must be on a target name at the head of some rule.
    let (target, _) = makefile
        .rules()
        .find_map(|rule| target_at_offset(&rule, byte_offset))?;

    if !crate::dep_graph::is_graph_target(&target) {
        return None;
    }

    // Conventional entry points (`all`, `install`, `clean`, …) ARE the
    // top-level targets — there's nothing to attach them to.
    if crate::dep_graph::is_conventional_entry_point(&target) {
        return None;
    }

    // Orphan check: nothing else lists this target as a prerequisite.
    let graph = crate::dep_graph::DependencyGraph::from_makefile(&makefile);
    if graph.referrers(&target).any(|r| r != target) {
        return None;
    }

    let goal_name = pick_default_goal(&makefile, &target)?;
    if goal_name == target {
        return None;
    }

    // First rule with this target wins — that's what make's default-goal
    // semantics do, and it's where prerequisites should accumulate.
    let mut goal_rule = makefile.rules_by_target(&goal_name).next()?;
    if goal_rule.prerequisites().any(|p| p == target) {
        return None;
    }

    let original_range = goal_rule.syntax().text_range();
    goal_rule.add_prerequisite(&target).ok()?;
    let edit = edit_for_node_change(source_text, original_range, goal_rule.syntax());

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: format!("Add '{}' as prerequisite of '{}'", target, goal_name),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Pick a default goal to attach orphans to: `all`, then `default`, then the
/// first non-pattern non-special graph target. Returns `None` if none exists
/// or if the only candidate *is* `target`.
fn pick_default_goal(makefile: &Makefile, target: &str) -> Option<String> {
    for preferred in ["all", "default"] {
        if makefile.rules_by_target(preferred).next().is_some() && preferred != target {
            return Some(preferred.to_string());
        }
    }
    makefile
        .rules()
        .flat_map(|r| r.targets().collect::<Vec<_>>())
        .find(|t| t != target && crate::dep_graph::is_graph_target(t))
}

/// Offer "Inline prerequisite '<p>'" — remove a prereq from the rule's list
/// when it's already reachable transitively via another prereq.
///
/// Pairs with the `redundant-prerequisite` diagnostic. Triggered when the
/// cursor is on a specific prerequisite identifier so the action targets
/// that one even if the rule has multiple redundancies.
fn inline_prerequisite_action(
    parsed: &Parse<Makefile>,
    source_text: &str,
    byte_offset: usize,
    uri: &Uri,
) -> Option<CodeAction> {
    let offset = text_size::TextSize::from(byte_offset as u32);
    let makefile = parsed.tree();

    let mut rule = makefile
        .rules()
        .find(|r| r.syntax().text_range().contains(offset))?;

    // Locate the IDENTIFIER token under the cursor that lives inside a
    // PREREQUISITES node — that's the prereq we'd remove.
    let prereqs_node = rule
        .syntax()
        .children()
        .find(|c| c.kind() == SyntaxKind::PREREQUISITES)?;
    let token = prereqs_node
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == SyntaxKind::IDENTIFIER && t.text_range().contains(offset))?;
    let cursor_prereq = token.text().to_string();

    let prereqs: Vec<String> = rule.prerequisites().collect();
    if prereqs.len() < 2 || !prereqs.contains(&cursor_prereq) {
        return None;
    }
    let targets: HashSet<String> = rule.targets().collect();
    if targets.contains(&cursor_prereq) {
        return None;
    }

    let graph = crate::dep_graph::DependencyGraph::from_makefile(&makefile);
    let branches = crate::conditionals::conditional_branches(rule.syntax());
    let via = prereqs.iter().find(|other| {
        other.as_str() != cursor_prereq.as_str()
            && graph
                .reachable_from(other, &branches)
                .contains(&cursor_prereq)
    })?;
    let via = via.clone();

    // Build the new list: drop only the first occurrence at the cursor.
    let mut new_prereqs: Vec<String> = Vec::with_capacity(prereqs.len() - 1);
    let mut dropped = false;
    for p in &prereqs {
        if !dropped && p == &cursor_prereq {
            dropped = true;
            continue;
        }
        new_prereqs.push(p.clone());
    }

    let original_range = rule.syntax().text_range();
    let refs: Vec<&str> = new_prereqs.iter().map(String::as_str).collect();
    rule.set_prerequisites(refs).ok()?;
    let edit = edit_for_node_change(source_text, original_range, rule.syntax());

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: format!(
            "Inline prerequisite '{}' (already via '{}')",
            cursor_prereq, via
        ),
        kind: Some(CodeActionKind::REFACTOR_INLINE),
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

    fn actions_at(
        uri: &str,
        text: &str,
        range: Range,
        diagnostics: &[Diagnostic],
    ) -> Vec<CodeAction> {
        let doc = crate::workspace::Document::new(uri.parse().unwrap(), text.to_string());
        get_code_actions(&FileSet::single(doc), range, diagnostics)
    }

    fn parse_and_actions(text: &str, pos: Position) -> Vec<CodeAction> {
        actions_at("file:///test/Makefile", text, Range::new(pos, pos), &[])
    }

    #[test]
    fn test_add_phony_action() {
        let text = "all: build\n\techo done\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(actions.iter().any(|a| a.title.contains(".PHONY")));
    }

    #[test]
    fn test_add_phony_action_second_target() {
        let text = "a b:\n\techo done\n";
        let titles: Vec<String> = parse_and_actions(text, Position::new(0, 2))
            .into_iter()
            .map(|a| a.title)
            .filter(|t| t.contains(".PHONY"))
            .collect();
        assert_eq!(titles, vec!["Add 'b' to .PHONY"]);
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
    fn test_add_phony_action_on_second_target() {
        let text = "clean distclean:\n\trm -f x\n";
        let actions = parse_and_actions(text, Position::new(0, 8));
        let titles: Vec<_> = actions
            .iter()
            .filter(|a| a.title.contains(".PHONY"))
            .map(|a| a.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Add 'distclean' to .PHONY"]);
    }

    #[test]
    fn test_add_phony_action_linked_to_missing_phony() {
        let text = "all: foo\n\ttouch foo\n";
        let parsed = Makefile::parse(text);
        let dir = tempfile::tempdir().unwrap();
        let diagnostics: Vec<_> =
            crate::diagnostics::get_diagnostics(text, &parsed, Some(dir.path()))
                .into_iter()
                .filter(|d| d.code == Some(NumberOrString::String("missing-phony".to_string())))
                .collect();
        assert_eq!(diagnostics.len(), 1);
        let actions = actions_at(
            "file:///test/Makefile",
            text,
            diagnostics[0].range,
            &diagnostics,
        );
        let action = actions
            .iter()
            .find(|a| a.title == "Add 'all' to .PHONY")
            .unwrap();
        assert_eq!(action.diagnostics, Some(diagnostics.clone()));
        assert_eq!(action.is_preferred, Some(true));
        assert_eq!(
            apply_edit(text, only_edit(action)),
            ".PHONY: all\nall: foo\n\ttouch foo\n"
        );
    }

    #[test]
    fn test_add_phony_action_unlinked_without_diagnostic() {
        let text = "all: foo\n\ttouch foo\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Add 'all' to .PHONY")
            .unwrap();
        assert_eq!(action.diagnostics, None);
        assert_eq!(action.is_preferred, None);
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
    fn test_insert_missing_endef_action() {
        let text = "define greeting\necho hello\n";
        let actions = parse_and_actions(text, Position::new(1, 2));
        let action = actions
            .iter()
            .find(|a| a.title == "Insert missing endef")
            .expect("expected quickfix");
        assert_eq!(
            apply_edit(text, only_edit(action)),
            "define greeting\necho hello\nendef\n"
        );
    }

    #[test]
    fn test_insert_missing_endef_without_trailing_newline() {
        let text = "all:\n\techo\ndefine greeting\necho hello";
        let actions = parse_and_actions(text, Position::new(2, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Insert missing endef")
            .expect("expected quickfix");
        assert_eq!(
            apply_edit(text, only_edit(action)),
            "all:\n\techo\ndefine greeting\necho hello\nendef\n"
        );
    }

    #[test]
    fn test_insert_missing_endef_nested() {
        let text = "define outer\ndefine inner\nbody\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Insert missing endef")
            .expect("expected quickfix");
        assert_eq!(
            apply_edit(text, only_edit(action)),
            "define outer\ndefine inner\nbody\nendef\nendef\n"
        );
    }

    #[test]
    fn test_no_insert_endef_for_override_define_in_body() {
        // Like make, only a bare `define` nests.
        let text = "define outer\noverride define inner\nbody\nendef\n";
        let actions = parse_and_actions(text, Position::new(2, 0));
        assert!(!actions.iter().any(|a| a.title == "Insert missing endef"));
    }

    #[test]
    fn test_insert_missing_endef_in_conditional() {
        let text = "ifdef X\ndefine g\nbody\n";
        let actions = parse_and_actions(text, Position::new(2, 0));
        let action = actions
            .iter()
            .find(|a| a.title == "Insert missing endef")
            .expect("expected quickfix");
        assert_eq!(
            apply_edit(text, only_edit(action)),
            "ifdef X\ndefine g\nbody\nendef\n"
        );
    }

    #[test]
    fn test_no_insert_endef_when_terminated() {
        let text = "define greeting\necho hello\nendef\n";
        let actions = parse_and_actions(text, Position::new(1, 0));
        assert!(!actions.iter().any(|a| a.title == "Insert missing endef"));
    }

    #[test]
    fn test_no_insert_endef_outside_define() {
        let text = "all:\n\techo\ndefine greeting\necho hello\n";
        let actions = parse_and_actions(text, Position::new(0, 1));
        assert!(!actions.iter().any(|a| a.title == "Insert missing endef"));
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

    fn inline_result(text: &str, name: &str) -> Option<String> {
        parse_and_actions(text, Position::new(0, 0))
            .iter()
            .find(|a| a.title == format!("Inline variable '{name}'"))
            .map(|a| {
                let edits = a.edit.as_ref().unwrap().changes.as_ref().unwrap();
                apply_edits(text, edits.values().next().unwrap())
            })
    }

    #[test]
    fn test_inline_variable_in_define_body() {
        assert_eq!(
            inline_result("OUT = dist\ndefine F\ncp $(OUT)/a ${OUT}\nendef\n", "OUT"),
            Some("define F\ncp dist/a dist\nendef\n".to_string())
        );
    }

    #[test]
    fn test_inline_variable_in_orphan_recipe() {
        assert_eq!(
            inline_result("OUT = dist\nifdef X\n\tcp $(OUT) .\nendif\n", "OUT"),
            Some("ifdef X\n\tcp dist .\nendif\n".to_string())
        );
    }

    #[test]
    fn test_no_inline_for_substitution_reference_in_recipe() {
        assert_eq!(
            inline_result("OUT = a.c\nall:\n\techo $(OUT) $(OUT:.c=.o)\n", "OUT"),
            None
        );
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

    // Attach-to-default-goal tests

    fn find_attach_action(actions: &[CodeAction]) -> Option<&CodeAction> {
        actions
            .iter()
            .find(|a| a.title.starts_with("Add '") && a.title.contains("as prerequisite of"))
    }

    #[test]
    fn test_attach_to_default_goal_uses_all() {
        // 'helper' is orphan; 'all' exists -> offer attaching to 'all'.
        let text = "all: build\n\t@:\nbuild:\n\t@:\nhelper:\n\techo hi\n";
        let actions = parse_and_actions(text, Position::new(4, 0));
        let action = find_attach_action(&actions).expect("expected attach action");
        assert_eq!(action.title, "Add 'helper' as prerequisite of 'all'");
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
        assert_eq!(
            result,
            "all: build helper\n\t@:\nbuild:\n\t@:\nhelper:\n\techo hi\n"
        );
    }

    #[test]
    fn test_attach_second_target_of_rule() {
        let text = "all: build\n\t@:\nbuild:\n\t@:\nx helper:\n\techo hi\n";
        let actions = parse_and_actions(text, Position::new(4, 7));
        let action = find_attach_action(&actions).expect("expected attach action");
        assert_eq!(action.title, "Add 'helper' as prerequisite of 'all'");
    }

    #[test]
    fn test_attach_to_default_goal_falls_back_to_first_rule() {
        // No 'all' or 'default'; fall back to the first non-pattern rule.
        let text = "release: dist\n\t@:\ndist:\n\t@:\nhelper:\n\t@:\n";
        let actions = parse_and_actions(text, Position::new(4, 0));
        let action = find_attach_action(&actions).expect("expected attach action");
        assert_eq!(action.title, "Add 'helper' as prerequisite of 'release'");
    }

    #[test]
    fn test_attach_silenced_when_referenced() {
        // 'helper' is already wired into 'all'; no action.
        let text = "all: helper\n\t@:\nhelper:\n\techo hi\n";
        let actions = parse_and_actions(text, Position::new(2, 0));
        assert!(find_attach_action(&actions).is_none());
    }

    #[test]
    fn test_attach_silenced_for_default_goal_itself() {
        // Cursor is on 'all' — don't suggest attaching a target to itself.
        let text = "all: build\n\t@:\nbuild:\n\t@:\n";
        let actions = parse_and_actions(text, Position::new(0, 0));
        assert!(find_attach_action(&actions).is_none());
    }

    #[test]
    fn test_attach_silenced_for_pattern_rule() {
        let text = "all: foo\n\t@:\nfoo:\n\t@:\n%.o: %.c\n\t@:\n";
        let actions = parse_and_actions(text, Position::new(4, 0));
        assert!(find_attach_action(&actions).is_none());
    }

    #[test]
    fn test_attach_silenced_for_special_target() {
        // '.PHONY' is a special target — don't offer attaching it.
        let text = "all: build\n\t@:\nbuild:\n\t@:\n.PHONY: helper\n";
        let actions = parse_and_actions(text, Position::new(4, 0));
        assert!(find_attach_action(&actions).is_none());
    }

    // Inline prerequisite tests

    fn find_inline_prereq_action(actions: &[CodeAction]) -> Option<&CodeAction> {
        actions
            .iter()
            .find(|a| a.title.starts_with("Inline prerequisite"))
    }

    #[test]
    fn test_inline_prereq_removes_redundant() {
        // `all: lib main` plus `main: lib` -> cursor on `lib` offers removal.
        let text = "all: lib main\n\t@:\nmain: lib\n\t@:\nlib:\n\t@:\n";
        // Position on 'l' of the first `lib` in `all: lib main` -> col 5.
        let actions = parse_and_actions(text, Position::new(0, 5));
        let action = find_inline_prereq_action(&actions).expect("expected inline action");
        assert!(action.title.contains("'lib'"));
        assert!(action.title.contains("via 'main'"));
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
        assert_eq!(result, "all: main\n\t@:\nmain: lib\n\t@:\nlib:\n\t@:\n");
    }

    #[test]
    fn test_inline_prereq_silenced_for_non_redundant() {
        let text = "all: a b\n\t@:\na:\n\t@:\nb:\n\t@:\n";
        let actions = parse_and_actions(text, Position::new(0, 5));
        assert!(find_inline_prereq_action(&actions).is_none());
    }

    #[test]
    fn test_inline_prereq_only_when_cursor_on_redundant_one() {
        // Cursor on `main` (col 9) — `main` is NOT redundant. No action.
        let text = "all: lib main\n\t@:\nmain: lib\n\t@:\nlib:\n\t@:\n";
        let actions = parse_and_actions(text, Position::new(0, 9));
        assert!(find_inline_prereq_action(&actions).is_none());
    }

    #[test]
    fn test_inline_prereq_silenced_across_exclusive_branches() {
        let text = "ifdef X\nb: c\nelse\nall: b c\nendif\nc:\n";
        let actions = parse_and_actions(text, Position::new(3, 7));
        assert_eq!(
            find_inline_prereq_action(&actions).map(|a| a.title.as_str()),
            None
        );
    }

    #[test]
    fn test_inline_prereq_within_one_branch() {
        let text = "ifdef X\nb: c\nall: b c\nendif\nc:\n";
        let actions = parse_and_actions(text, Position::new(2, 7));
        assert_eq!(
            find_inline_prereq_action(&actions).map(|a| a.title.as_str()),
            Some("Inline prerequisite 'c' (already via 'b')")
        );
    }

    #[test]
    fn test_inline_prereq_silenced_for_single_prereq() {
        let text = "all: only\n\t@:\nonly:\n\t@:\n";
        let actions = parse_and_actions(text, Position::new(0, 5));
        assert!(find_inline_prereq_action(&actions).is_none());
    }

    fn include_optional_action(text: &str, pos: Position) -> Option<String> {
        parse_and_actions(text, pos)
            .iter()
            .find(|a| a.title == "Change include to -include")
            .map(|a| apply_edit(text, only_edit(a)))
    }

    #[test]
    fn test_make_include_optional_action() {
        assert_eq!(
            include_optional_action("include $(DIR)/x.mk\nall:\n", Position::new(0, 12)),
            Some("-include $(DIR)/x.mk\nall:\n".to_string())
        );
        assert_eq!(
            include_optional_action("ifdef X\n  include a.mk b.mk\nendif\n", Position::new(1, 3)),
            Some("ifdef X\n  -include a.mk b.mk\nendif\n".to_string())
        );
    }

    #[test]
    fn test_no_make_include_optional_action() {
        assert_eq!(
            include_optional_action("-include a.mk\n", Position::new(0, 2)),
            None
        );
        assert_eq!(
            include_optional_action("sinclude a.mk\n", Position::new(0, 2)),
            None
        );
        assert_eq!(
            include_optional_action("include a.mk\nall:\n", Position::new(1, 0)),
            None
        );
    }

    fn create_target_action(text: &str, pos: Position, dir: &Path) -> Option<(String, String)> {
        let uri = Uri::from_file_path(dir.join("Makefile")).unwrap();
        actions_at(uri.as_str(), text, Range::new(pos, pos), &[])
            .iter()
            .find(|a| a.title.starts_with("Create target"))
            .map(|a| (a.title.clone(), apply_edit(text, only_edit(a))))
    }

    #[test]
    fn test_create_target_action() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            create_target_action("all: foo\n\techo\n", Position::new(0, 6), dir.path()),
            Some((
                "Create target for 'foo'".to_string(),
                "all: foo\n\techo\n\nfoo:\n".to_string()
            ))
        );
    }

    #[test]
    fn test_create_target_action_without_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            create_target_action("CC = gcc\nall: foo | bar", Position::new(1, 12), dir.path()),
            Some((
                "Create target for 'bar'".to_string(),
                "CC = gcc\nall: foo | bar\n\nbar:\n".to_string()
            ))
        );
    }

    #[test]
    fn test_create_target_action_escaped() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            create_target_action("all: a\\#b\n\n", Position::new(0, 6), dir.path()),
            Some((
                "Create target for 'a#b'".to_string(),
                "all: a\\#b\n\na\\#b:\n".to_string()
            ))
        );
    }

    #[test]
    fn test_no_create_target_action() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.c"), "").unwrap();
        for (text, pos) in [
            // Already a target.
            ("all: foo\nfoo:\n", Position::new(0, 6)),
            // Built by a pattern rule.
            ("all: foo.o\n%.o: %.c\n", Position::new(0, 6)),
            // An existing file.
            ("app: main.c\n", Position::new(0, 7)),
            // Not a file name.
            ("all: $(OBJS)\n", Position::new(0, 7)),
            (".SUFFIXES: .c\n", Position::new(0, 12)),
            // Not on a prerequisite.
            ("all: foo\n", Position::new(0, 1)),
        ] {
            assert_eq!(
                create_target_action(text, pos, dir.path()),
                None,
                "{text:?}"
            );
        }
    }

    #[test]
    fn test_no_create_target_action_without_base_dir() {
        let pos = Position::new(0, 6);
        let actions = actions_at("untitled:Makefile", "all: foo\n", Range::new(pos, pos), &[]);
        assert!(!actions.iter().any(|a| a.title.starts_with("Create target")));
    }

    fn file_set_create_target_titles(
        files: &[(&str, &str)],
        name: &str,
        pos: Position,
    ) -> Vec<String> {
        let fx = crate::workspace::tests::Fixture::new(files);
        let (mut ws, makefile) = fx.open("Makefile");
        ws.file_set(&makefile).unwrap();
        let uri = if name == "Makefile" {
            makefile
        } else {
            fx.open_in(&mut ws, name)
        };
        get_code_actions(&ws.file_set(&uri).unwrap(), Range::new(pos, pos), &[])
            .into_iter()
            .map(|a| a.title)
            .filter(|t| t.starts_with("Create target"))
            .collect()
    }

    #[test]
    fn test_no_create_target_action_for_target_in_other_file() {
        let files = [
            ("Makefile", "include rules.mk\nall: foo bar\n"),
            ("rules.mk", "foo:\n\techo\nlint: all\n"),
        ];
        let empty: Vec<String> = vec![];
        assert_eq!(
            file_set_create_target_titles(&files, "Makefile", Position::new(1, 5)),
            empty
        );
        assert_eq!(
            file_set_create_target_titles(&files, "Makefile", Position::new(1, 9)),
            vec!["Create target for 'bar'".to_string()]
        );
        // `all` is defined by the including makefile.
        assert_eq!(
            file_set_create_target_titles(&files, "rules.mk", Position::new(2, 7)),
            empty
        );
    }
}
