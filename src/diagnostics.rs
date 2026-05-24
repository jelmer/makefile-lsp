//! Diagnostics for Makefile files.

use std::collections::{HashMap, HashSet};

use makefile_lossless::{
    Conditional, Makefile, MakefileItem, Parse, SyntaxKind, VariableReference,
};
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

use crate::builtins;
use crate::position::text_range_to_lsp_range;

fn make_diagnostic(
    range: Range,
    severity: DiagnosticSeverity,
    code: &str,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(severity),
        code: Some(NumberOrString::String(code.to_string())),
        source: Some("makefile-lsp".to_string()),
        message,
        ..Default::default()
    }
}

/// Collect diagnostics from parse errors and semantic analysis.
///
/// `base_dir`, when provided, is used to resolve relative include paths for
/// the missing-include-file check. Pass `None` to skip filesystem-touching
/// checks (e.g. in tests where the makefile doesn't live on disk).
pub fn get_diagnostics(
    source_text: &str,
    parsed: &Parse<makefile_lossless::Makefile>,
    base_dir: Option<&std::path::Path>,
) -> Vec<Diagnostic> {
    let mut diagnostics: Vec<Diagnostic> = parsed
        .positioned_errors()
        .iter()
        .map(|error| {
            let range = text_range_to_lsp_range(source_text, error.range);
            make_diagnostic(
                range,
                DiagnosticSeverity::ERROR,
                error.code.as_deref().unwrap_or("parse-error"),
                error.message.clone(),
            )
        })
        .collect();

    let makefile = parsed.tree();
    diagnostics.extend(check_undefined_variables(source_text, &makefile));
    diagnostics.extend(check_recursive_variable_self_reference(
        source_text,
        &makefile,
    ));
    diagnostics.extend(check_empty_variable_references(source_text, &makefile));
    diagnostics.extend(check_self_dependency(source_text, &makefile));
    diagnostics.extend(check_circular_dependencies(source_text, &makefile));
    diagnostics.extend(check_duplicate_targets(source_text, &makefile));
    diagnostics.extend(check_missing_phony_targets(source_text, &makefile));
    diagnostics.extend(check_unused_phony_targets(source_text, &makefile));
    diagnostics.extend(check_include_missing_path(source_text, &makefile));
    diagnostics.extend(check_spaces_in_recipes(source_text, &makefile));
    diagnostics.extend(check_trailing_whitespace_in_value(source_text, &makefile));
    diagnostics.extend(check_duplicate_prerequisites(source_text, &makefile));
    diagnostics.extend(check_shell_in_recursive_assignment(source_text, &makefile));
    diagnostics.extend(check_empty_automatic_variables(source_text, &makefile));
    diagnostics.extend(check_unterminated_conditionals(source_text, &makefile));
    diagnostics.extend(check_unused_variables(source_text, &makefile));
    diagnostics.extend(check_mixed_assignment_operators(source_text, &makefile));
    diagnostics.extend(check_orphan_recipe_line(source_text, &makefile));
    diagnostics.extend(check_empty_rule_probably_phony(source_text, &makefile));
    if let Some(dir) = base_dir {
        diagnostics.extend(check_missing_include_file(source_text, &makefile, dir));
    }

    diagnostics
}

/// Check for references to undefined variables.
fn check_undefined_variables(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let defined_vars: HashSet<String> = makefile
        .variable_definitions()
        .filter_map(|v| v.name())
        .collect();

    for var_ref in makefile.variable_references() {
        let Some(name) = var_ref.name() else {
            continue;
        };
        if builtins::is_known_variable(&name) || defined_vars.contains(&name) {
            continue;
        }
        let range = text_range_to_lsp_range(source_text, var_ref.text_range());
        diagnostics.push(make_diagnostic(
            range,
            DiagnosticSeverity::WARNING,
            "undefined-variable",
            format!("variable '{}' is not defined", name),
        ));
    }

    diagnostics
}

/// Check for recursive variables that reference themselves, which causes infinite expansion.
///
/// Only flags `=` (recursively-expanded) assignments, since `:=`/`::=`/`:::=` expand
/// immediately and self-references are valid there (they refer to the previous value).
fn check_recursive_variable_self_reference(
    source_text: &str,
    makefile: &Makefile,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for var_def in makefile.variable_definitions() {
        let Some(name) = var_def.name() else {
            continue;
        };
        let op = var_def.assignment_operator().unwrap_or_default();
        if op != "=" {
            continue;
        }

        // Walk the EXPR descendants of this variable definition for self-references
        for child in var_def.syntax().descendants() {
            if let Some(var_ref) = VariableReference::cast(child) {
                if var_ref.name().as_deref() == Some(&name) {
                    let range = text_range_to_lsp_range(source_text, var_ref.text_range());
                    diagnostics.push(make_diagnostic(
                        range,
                        DiagnosticSeverity::WARNING,
                        "recursive-variable-reference",
                        format!(
                            "variable '{}' references itself in a recursively-expanded definition",
                            name
                        ),
                    ));
                }
            }
        }
    }

    diagnostics
}

/// Special targets where duplicate definitions are expected (they accumulate prerequisites).
const ACCUMULATING_TARGETS: &[&str] = &[
    ".PHONY",
    ".SUFFIXES",
    ".PRECIOUS",
    ".INTERMEDIATE",
    ".SECONDARY",
    ".IGNORE",
    ".SILENT",
    ".NOTPARALLEL",
];

/// Check for duplicate target definitions.
///
/// In GNU Make, when the same target appears in multiple single-colon rules,
/// only the last one's recipe is used, which is almost always a mistake.
/// Double-colon rules (`::`) are intentionally excluded since they allow
/// multiple recipe blocks.
fn check_duplicate_targets(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut seen: HashMap<String, Range> = HashMap::new();

    for rule in makefile.rules() {
        for target in rule.targets() {
            // Skip pattern rules (contain %)
            if target.contains('%') {
                continue;
            }
            // Skip special targets that accumulate prerequisites
            if ACCUMULATING_TARGETS.contains(&target.as_str()) {
                continue;
            }
            // Skip double-colon rules (they intentionally allow multiple definitions)
            if rule.is_double_colon() {
                continue;
            }

            let rule_range = text_range_to_lsp_range(source_text, rule.syntax().text_range());
            // Narrow the range to just the target name
            let target_range = Range {
                start: rule_range.start,
                end: Position::new(
                    rule_range.start.line,
                    rule_range.start.character + target.len() as u32,
                ),
            };

            if let Some(first_range) = seen.get(&target) {
                diagnostics.push(make_diagnostic(
                    target_range,
                    DiagnosticSeverity::WARNING,
                    "duplicate-target",
                    format!(
                        "target '{}' already defined on line {}",
                        target,
                        first_range.start.line + 1
                    ),
                ));
            } else {
                seen.insert(target, target_range);
            }
        }
    }

    diagnostics
}

/// Check for empty variable references like `$()` or `${}`.
fn check_empty_variable_references(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for var_ref in makefile.variable_references() {
        if var_ref.name().is_none() {
            let range = text_range_to_lsp_range(source_text, var_ref.text_range());
            diagnostics.push(make_diagnostic(
                range,
                DiagnosticSeverity::WARNING,
                "empty-variable-reference",
                "empty variable reference".to_string(),
            ));
        }
    }

    diagnostics
}

/// Check for targets that list themselves as prerequisites.
fn check_self_dependency(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for rule in makefile.rules() {
        let targets: Vec<String> = rule.targets().collect();
        for prereq in rule.prerequisites() {
            if targets.contains(&prereq) {
                // Find the prerequisite position within the PREREQUISITES node
                let rule_range = text_range_to_lsp_range(source_text, rule.syntax().text_range());
                diagnostics.push(make_diagnostic(
                    rule_range,
                    DiagnosticSeverity::WARNING,
                    "self-dependency",
                    format!("target '{}' lists itself as a prerequisite", prereq),
                ));
            }
        }
    }

    diagnostics
}

/// Check for circular dependencies in the target graph.
///
/// Each cycle is reported once, anchored on the rule of the first target in
/// its canonical rotation. Self-loops are left to `check_self_dependency`.
fn check_circular_dependencies(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let graph = crate::dep_graph::DependencyGraph::from_makefile(makefile);
    let cycles = graph.find_cycles();
    if cycles.is_empty() {
        return Vec::new();
    }

    // Map each target in a reported cycle to a rule range to anchor on.
    let mut target_range: HashMap<String, Range> = HashMap::new();
    for rule in makefile.rules() {
        let rule_range = text_range_to_lsp_range(source_text, rule.syntax().text_range());
        for target in rule.targets() {
            target_range.entry(target).or_insert(rule_range);
        }
    }

    cycles
        .into_iter()
        .filter_map(|cycle| {
            let range = *target_range.get(&cycle[0])?;
            let mut display = cycle.clone();
            display.push(cycle[0].clone());
            Some(make_diagnostic(
                range,
                DiagnosticSeverity::WARNING,
                "circular-dependency",
                format!("circular dependency: {}", display.join(" -> ")),
            ))
        })
        .collect()
}

/// Check for `.PHONY` prerequisites that are never defined as targets.
fn check_missing_phony_targets(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let defined_targets: HashSet<String> = makefile
        .rules()
        .flat_map(|r| r.targets().collect::<Vec<_>>())
        .collect();

    for rule in makefile.rules_by_target(".PHONY") {
        let rule_range = text_range_to_lsp_range(source_text, rule.syntax().text_range());
        for prereq in rule.prerequisites() {
            if !defined_targets.contains(&prereq) {
                diagnostics.push(make_diagnostic(
                    rule_range,
                    DiagnosticSeverity::WARNING,
                    "undefined-phony-target",
                    format!(
                        "target '{}' is declared .PHONY but is never defined",
                        prereq
                    ),
                ));
            }
        }
    }

    diagnostics
}

/// Check for `.PHONY` targets that nothing references.
///
/// Two related diagnostics:
///
/// * `unused-phony-target` (warning): the target is declared `.PHONY`, has a
///   rule with no recipe and no prerequisites, AND nothing depends on it.
///   High-confidence typo or stale declaration — there's nothing the rule
///   could do and no caller wants it.
/// * `unreferenced-phony-target` (hint): the target is declared `.PHONY` and
///   has a real rule, but nothing depends on it AND the name isn't a
///   conventional entry point (`all`, `install`, …). The author may have
///   forgotten to wire it into a top-level target like `all`.
///
/// Self-references don't count as incoming edges. Targets that don't appear
/// as the head of any rule are handled by `check_missing_phony_targets`.
fn check_unused_phony_targets(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let graph = crate::dep_graph::DependencyGraph::from_makefile(makefile);

    // Collect every name declared in any `.PHONY:` rule, keyed to that rule's
    // range so the diagnostic anchors on the declaration site.
    let mut phony_decls: HashMap<String, Range> = HashMap::new();
    for rule in makefile.rules_by_target(".PHONY") {
        let range = text_range_to_lsp_range(source_text, rule.syntax().text_range());
        for name in rule.prerequisites() {
            phony_decls.entry(name).or_insert(range);
        }
    }

    // Index target rules so we can ask "does this name have a rule, and is
    // that rule empty?" without rescanning. A name with multiple rules is
    // empty only if *every* rule is empty (any non-empty rule means there's
    // real work attached).
    let mut has_rule: HashSet<String> = HashSet::new();
    let mut nonempty_rule: HashSet<String> = HashSet::new();
    for rule in makefile.rules() {
        let prereqs: Vec<String> = rule.prerequisites().collect();
        let has_recipe = rule.recipe_nodes().next().is_some();
        for target in rule.targets() {
            has_rule.insert(target.clone());
            // Self-prerequisites don't count as real work — they can't build
            // anything and are flagged separately by check_self_dependency.
            let has_real_prereqs = prereqs.iter().any(|p| p != &target);
            if has_recipe || has_real_prereqs {
                nonempty_rule.insert(target);
            }
        }
    }

    let mut names: Vec<&String> = phony_decls.keys().collect();
    names.sort();

    for name in names {
        if !has_rule.contains(name) {
            // No rule at all -> handled by check_missing_phony_targets.
            continue;
        }
        let referenced = graph.referrers(name).any(|r| r != name);
        if referenced {
            continue;
        }

        let range = phony_decls[name];
        if !nonempty_rule.contains(name) {
            diagnostics.push(make_diagnostic(
                range,
                DiagnosticSeverity::WARNING,
                "unused-phony-target",
                format!(
                    "'.PHONY' target '{}' has no recipe and nothing depends on it",
                    name
                ),
            ));
        } else if !crate::dep_graph::is_conventional_entry_point(name) {
            diagnostics.push(make_diagnostic(
                range,
                DiagnosticSeverity::HINT,
                "unreferenced-phony-target",
                format!("nothing depends on '.PHONY' target '{}'", name),
            ));
        }
    }

    diagnostics
}

/// Check for `include` directives with missing paths.
fn check_include_missing_path(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for item in makefile.items() {
        if let MakefileItem::Include(inc) = item {
            let path = inc.path().unwrap_or_default();
            if path.is_empty() {
                let range = text_range_to_lsp_range(source_text, inc.syntax().text_range());
                diagnostics.push(make_diagnostic(
                    range,
                    DiagnosticSeverity::ERROR,
                    "include-missing-path",
                    "include directive has no file path".to_string(),
                ));
            }
        }
    }

    diagnostics
}

/// Check for recipe lines that use spaces instead of a tab for indentation.
///
/// GNU Make requires recipe lines to start with a tab character. When spaces
/// are used instead, make rejects the file with a confusing error message.
fn check_spaces_in_recipes(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for rule in makefile.rules() {
        for recipe in rule.recipe_nodes() {
            let indent = recipe.indent();
            if let Some(ref indent_text) = indent {
                if !indent_text.starts_with('\t') {
                    // Find the INDENT token's text range for precise positioning
                    let indent_range = recipe
                        .syntax()
                        .children_with_tokens()
                        .find_map(|it| {
                            if let Some(token) = it.as_token() {
                                if token.kind() == SyntaxKind::INDENT {
                                    return Some(token.text_range());
                                }
                            }
                            None
                        })
                        .unwrap_or_else(|| recipe.syntax().text_range());

                    let range = text_range_to_lsp_range(source_text, indent_range);
                    diagnostics.push(make_diagnostic(
                        range,
                        DiagnosticSeverity::ERROR,
                        "spaces-instead-of-tab",
                        "recipe lines must start with a tab, not spaces".to_string(),
                    ));
                }
            }
        }
    }

    diagnostics
}

/// Check for automatic variables that expand to empty in their context.
///
/// `$<`, `$^`, `$+`, `$?` all expand to (part of) the prerequisite list, so
/// they're empty in a rule with no prerequisites. `$*` expands to the stem of
/// a pattern rule, so it's empty in a non-pattern rule.
///
/// TODO: `makefile-lossless` currently tokenizes recipe content as a single
/// flat TEXT token, so we byte-scan it here for `$X` / `$(X)` / `${X}`. Once
/// the parser surfaces structured VariableReference nodes inside recipes,
/// replace this scanner with an AST walk over those nodes.
fn check_empty_automatic_variables(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    use text_size::{TextRange, TextSize};

    let mut diagnostics = Vec::new();

    for rule in makefile.rules() {
        let has_prereqs = rule.prerequisites().next().is_some();
        let is_pattern = rule.targets().any(|t| t.contains('%'));

        if has_prereqs && is_pattern {
            continue;
        }

        for recipe in rule.recipe_nodes() {
            for token in recipe
                .syntax()
                .descendants_with_tokens()
                .filter_map(|c| c.into_token())
            {
                if token.kind() != SyntaxKind::TEXT {
                    continue;
                }
                let text = token.text();
                let base: u32 = token.text_range().start().into();
                for (var, start_off, end_off) in scan_automatic_vars(text) {
                    let flagged = match var {
                        '<' | '^' | '+' | '?' => !has_prereqs,
                        '*' => !is_pattern,
                        _ => false,
                    };
                    if !flagged {
                        continue;
                    }
                    let range = TextRange::new(
                        TextSize::from(base + start_off as u32),
                        TextSize::from(base + end_off as u32),
                    );
                    let lsp_range = text_range_to_lsp_range(source_text, range);
                    let reason = match var {
                        '*' => "non-pattern rule",
                        _ => "rule has no prerequisites",
                    };
                    diagnostics.push(make_diagnostic(
                        lsp_range,
                        DiagnosticSeverity::WARNING,
                        "empty-automatic-variable",
                        format!("${} expands to empty: {}", var, reason),
                    ));
                }
            }
        }
    }

    diagnostics
}

/// Scan a recipe text snippet for automatic variable references.
///
/// Returns `(var_char, start, end)` tuples where `start..end` covers the
/// whole `$X` (or `$(X)` / `${X}`) sequence within `text`. Only the single-
/// character automatic variables we care about are reported: `<`, `^`, `+`,
/// `?`, `*`. `$$` is treated as an escape and skipped.
///
/// TODO: see `check_empty_automatic_variables` — drop this when recipe
/// content is structurally tokenized in makefile-lossless.
fn scan_automatic_vars(text: &str) -> Vec<(char, usize, usize)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        // Past the `$`. What's next?
        if i + 1 >= bytes.len() {
            break;
        }
        let next = bytes[i + 1];
        if next == b'$' {
            // Escaped `$$` — skip both.
            i += 2;
            continue;
        }
        if next == b'(' || next == b'{' {
            // `$(X)` or `${X}` — only report if the contents are exactly a
            // single automatic-variable character.
            let close = if next == b'(' { b')' } else { b'}' };
            if i + 3 < bytes.len() && bytes[i + 3] == close {
                let c = bytes[i + 2];
                if matches!(c, b'<' | b'^' | b'+' | b'?' | b'*') {
                    out.push((c as char, i, i + 4));
                }
            }
            i += 1;
            continue;
        }
        if matches!(next, b'<' | b'^' | b'+' | b'?' | b'*') {
            out.push((next as char, i, i + 2));
            i += 2;
            continue;
        }
        i += 1;
    }
    out
}

/// Check for variables that are defined but never referenced.
///
/// Emits a hint (not a warning) to keep the noise low: the check has known
/// false-positive sources (recipes are byte-scanned because makefile-lossless
/// tokenizes them flatly; `$(eval)` and `$(call)` can reference variables in
/// ways we can't see statically) and many makefiles intentionally export
/// variables for sub-makes or external tooling.
///
/// Skipped:
/// - Variables exported with `export` (consumed outside the makefile).
/// - Variables overriding a builtin (would change make's behaviour).
/// - Variables defined inside conditionals (often configuration toggles).
fn check_unused_variables(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Collect every name that's referenced anywhere we can see.
    let mut referenced: HashSet<String> = makefile
        .variable_references()
        .filter_map(|v| v.name())
        .collect();

    // `ifdef NAME` / `ifndef NAME` reference NAME but don't show up in
    // `variable_references()` because the argument is a bare identifier.
    for cond in makefile
        .syntax()
        .descendants()
        .filter_map(Conditional::cast)
    {
        match cond.conditional_type().as_deref() {
            Some("ifdef") | Some("ifndef") => {
                if let Some(name) = cond.condition() {
                    referenced.insert(name);
                }
            }
            _ => {}
        }
    }

    // Recipe TEXT is a single flat token; byte-scan for $(NAME) / ${NAME}.
    //
    // TODO: replace with an AST walk once makefile-lossless tokenizes recipes
    // structurally.
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
                for name in scan_variable_ref_names(token.text()) {
                    referenced.insert(name);
                }
            }
        }
    }

    for var_def in makefile.variable_definitions() {
        let Some(name) = var_def.name() else {
            continue;
        };
        if referenced.contains(&name) {
            continue;
        }
        if var_def.is_export() {
            continue;
        }
        if builtins::is_known_variable(&name) {
            continue;
        }
        // Inside a conditional? Skip — likely a configuration toggle.
        if var_def
            .syntax()
            .ancestors()
            .any(|a| a.kind() == SyntaxKind::CONDITIONAL)
        {
            continue;
        }

        // Point at the IDENTIFIER token (the variable name) for precision.
        let name_range = var_def
            .syntax()
            .children_with_tokens()
            .filter_map(|c| c.into_token())
            .find(|t| t.kind() == SyntaxKind::IDENTIFIER && t.text() != "export")
            .map(|t| t.text_range())
            .unwrap_or_else(|| var_def.syntax().text_range());

        let range = text_range_to_lsp_range(source_text, name_range);
        diagnostics.push(make_diagnostic(
            range,
            DiagnosticSeverity::HINT,
            "unused-variable",
            format!("variable '{}' is defined but never used", name),
        ));
    }

    diagnostics
}

/// Scan a snippet of recipe text for `$(NAME)` and `${NAME}` references and
/// return the names. `$$` is treated as an escape and skipped.
///
/// TODO: drop in favour of structured tokenization in makefile-lossless.
fn scan_variable_ref_names(text: &str) -> Vec<String> {
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
            // Find a matching close on the same line. The name is the
            // unbroken identifier-character run between `(` and either the
            // close or a space (since `$(foo bar)` is a function call, not a
            // variable ref to `foo bar`).
            let name_start = i + 2;
            let mut j = name_start;
            while j < bytes.len() {
                let b = bytes[j];
                if b == close || b == b' ' || b == b'\t' || b == b'\n' {
                    break;
                }
                j += 1;
            }
            if j > name_start && j < bytes.len() && bytes[j] == close {
                // `$(NAME)` form — pure variable reference.
                if let Ok(s) = std::str::from_utf8(&bytes[name_start..j]) {
                    if is_valid_var_name(s) {
                        out.push(s.to_string());
                    }
                }
            } else if j > name_start && j < bytes.len() && bytes[j] == b' ' {
                // `$(func args...)` — function call. The first argument may
                // contain variable refs; rely on recursion into the rest of
                // the line below (we'll re-enter on the inner `$`).
            }
            i += 1;
            continue;
        }
        // Single-char automatic var like `$<` — not a user variable name.
        i += 2;
    }
    out
}

fn is_valid_var_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// Check for `include` directives whose path doesn't exist on disk.
///
/// Resolves the path relative to `base_dir`. Optional includes (`-include` /
/// `sinclude`) are skipped — they explicitly tolerate a missing file. Paths
/// containing variable references (`include $(CONFIG)`) are also skipped
/// since we can't evaluate them statically.
fn check_missing_include_file(
    source_text: &str,
    makefile: &Makefile,
    base_dir: &std::path::Path,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for item in makefile.items() {
        let MakefileItem::Include(inc) = item else {
            continue;
        };
        if inc.is_optional() {
            continue;
        }
        let Some(path) = inc.path() else { continue };
        if path.is_empty() {
            // Covered by check_include_missing_path; skip here.
            continue;
        }
        // Skip if the path contains an unresolved variable reference.
        if path.contains('$') {
            continue;
        }

        let resolved = base_dir.join(&path);
        if resolved.exists() {
            continue;
        }

        let range = inc
            .path_range()
            .map(|r| text_range_to_lsp_range(source_text, r))
            .unwrap_or_else(|| text_range_to_lsp_range(source_text, inc.syntax().text_range()));
        diagnostics.push(make_diagnostic(
            range,
            DiagnosticSeverity::WARNING,
            "missing-include-file",
            format!("included file '{}' does not exist", path),
        ));
    }

    diagnostics
}

/// Check for rules with no prerequisites and no recipe lines — these are
/// often phony declarations the author forgot to mark with `.PHONY`.
///
/// Hint-level: a regular target that already exists as a file is fine to
/// declare empty (it relies on the file's existence). The hint nudges the
/// common case where the author meant to make it phony.
fn check_empty_rule_probably_phony(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for rule in makefile.rules() {
        // Skip rules with prerequisites — they're meta-targets, not phony candidates.
        if rule.prerequisites().next().is_some() {
            continue;
        }
        // Skip rules with any recipe lines.
        if rule.recipe_nodes().next().is_some() {
            continue;
        }
        let targets: Vec<String> = rule.targets().collect();
        if targets.is_empty() {
            continue;
        }
        for target in &targets {
            // Special targets, pattern rules, and already-phony targets are fine empty.
            if target.starts_with('.') || target.contains('%') {
                continue;
            }
            if makefile.is_phony(target) {
                continue;
            }

            let rule_range = text_range_to_lsp_range(source_text, rule.syntax().text_range());
            diagnostics.push(make_diagnostic(
                rule_range,
                DiagnosticSeverity::HINT,
                "empty-rule-probably-phony",
                format!(
                    "target '{}' has no prerequisites and no recipe; \
                     declare it with .PHONY if it's not a file",
                    target
                ),
            ));
        }
    }

    diagnostics
}

/// Check for tab-indented recipe lines that aren't attached to any rule.
///
/// A line like `\techo something` outside any rule is parsed as a stray
/// INDENT + TEXT at the root level. This almost always means the author
/// forgot the target above it, or a copy-paste leftover.
fn check_orphan_recipe_line(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let root = makefile.syntax();

    let mut iter = root.children_with_tokens().peekable();
    while let Some(elem) = iter.next() {
        let Some(token) = elem.as_token() else {
            continue;
        };
        if token.kind() != SyntaxKind::INDENT {
            continue;
        }
        // Empty indent (no following text) is not a recipe line.
        let mut end = token.text_range().end();
        while let Some(next) = iter.peek() {
            let kind = next.kind();
            end = next.text_range().end();
            iter.next();
            if kind == SyntaxKind::NEWLINE {
                break;
            }
        }
        let range = text_size::TextRange::new(token.text_range().start(), end);
        let lsp_range = text_range_to_lsp_range(source_text, range);
        diagnostics.push(make_diagnostic(
            lsp_range,
            DiagnosticSeverity::ERROR,
            "orphan-recipe-line",
            "recipe line is not attached to any target".to_string(),
        ));
    }

    diagnostics
}

/// Check for the same variable being assigned with both `=` (recursive) and
/// `:=`/`::=`/`:::=` (immediate) flavours.
///
/// These flavours have different evaluation semantics; mixing them means the
/// later assignment silently wins and changes how earlier-referencing code
/// behaves. `+=` (append) and `?=` (conditional) are not flagged — they're
/// normal companions to either flavour.
fn check_mixed_assignment_operators(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // For each name, collect (flavour, range, line) for each non-`+=`/`?=`
    // assignment. Compare flavours within each name.
    enum Flavour {
        Recursive, // `=`
        Immediate, // `:=`, `::=`, `:::=`
    }
    let classify = |op: &str| match op {
        "=" => Some(Flavour::Recursive),
        ":=" | "::=" | ":::=" => Some(Flavour::Immediate),
        _ => None,
    };

    let mut by_name: HashMap<String, Vec<(Flavour, Range)>> = HashMap::new();
    for var_def in makefile.variable_definitions() {
        let Some(name) = var_def.name() else { continue };
        let Some(op) = var_def.assignment_operator() else {
            continue;
        };
        let Some(flavour) = classify(&op) else {
            continue;
        };
        let range = text_range_to_lsp_range(source_text, var_def.syntax().text_range());
        by_name.entry(name).or_default().push((flavour, range));
    }

    for (name, assignments) in by_name {
        if assignments.len() < 2 {
            continue;
        }
        let has_recursive = assignments
            .iter()
            .any(|(f, _)| matches!(f, Flavour::Recursive));
        let has_immediate = assignments
            .iter()
            .any(|(f, _)| matches!(f, Flavour::Immediate));
        if !(has_recursive && has_immediate) {
            continue;
        }
        // Flag every assignment that participated in the mix, so the user
        // sees each problem assignment.
        for (_, range) in &assignments {
            diagnostics.push(make_diagnostic(
                *range,
                DiagnosticSeverity::WARNING,
                "mixed-assignment-operators",
                format!(
                    "variable '{}' is assigned with both `=` and `:=`; the later assignment silently wins",
                    name
                ),
            ));
        }
    }

    diagnostics
}

/// Check for conditional blocks that are missing their `endif`.
///
/// Bare `else` or `endif` outside a conditional are already reported by the
/// parser as "unknown conditional directive". The case the parser silently
/// accepts is an `ifdef`/`ifeq` that runs to end-of-file without a matching
/// `endif`.
fn check_unterminated_conditionals(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for cond in makefile
        .syntax()
        .descendants()
        .filter_map(Conditional::cast)
    {
        // Skip orphans that don't even have a recognized opener — the parser
        // already complains about those (e.g. bare `else`/`endif`).
        if cond.conditional_type().is_none() {
            continue;
        }
        let has_endif = cond
            .syntax()
            .children()
            .any(|c| c.kind() == SyntaxKind::CONDITIONAL_ENDIF);
        if has_endif {
            continue;
        }

        // Point at the opening directive (CONDITIONAL_IF) for clarity.
        let opener_range = cond
            .syntax()
            .children()
            .find(|c| c.kind() == SyntaxKind::CONDITIONAL_IF)
            .map(|c| c.text_range())
            .unwrap_or_else(|| cond.syntax().text_range());

        let range = text_range_to_lsp_range(source_text, opener_range);
        let kind = cond.conditional_type().unwrap_or_default();
        diagnostics.push(make_diagnostic(
            range,
            DiagnosticSeverity::ERROR,
            "unterminated-conditional",
            format!("'{}' is missing a matching 'endif'", kind),
        ));
    }

    diagnostics
}

/// Check for `$(shell ...)` inside a recursively-expanded (`=`) assignment.
///
/// With `=`, the shell command is re-executed every time the variable is
/// expanded — once per recipe line that mentions it, often many times. Using
/// `:=` (or `::=`) runs it once at parse time. This is both a performance trap
/// and a correctness trap when the shell command has side effects or its
/// output changes between invocations.
fn check_shell_in_recursive_assignment(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for var_def in makefile.variable_definitions() {
        let op = var_def.assignment_operator().unwrap_or_default();
        if op != "=" {
            continue;
        }

        for child in var_def.syntax().descendants() {
            let Some(var_ref) = VariableReference::cast(child) else {
                continue;
            };
            if !var_ref.is_function_call() {
                continue;
            }
            if var_ref.name().as_deref() != Some("shell") {
                continue;
            }
            let range = text_range_to_lsp_range(source_text, var_ref.text_range());
            diagnostics.push(make_diagnostic(
                range,
                DiagnosticSeverity::WARNING,
                "shell-in-recursive-assignment",
                "$(shell ...) in a recursively-expanded (=) variable is re-run \
                 on every expansion; use := to run it once"
                    .to_string(),
            ));
        }
    }

    diagnostics
}

/// Check for duplicate prerequisites within a single rule.
///
/// `foo: a b a` is harmless but always a mistake — the duplicate adds no
/// information and usually means the author intended a different name.
fn check_duplicate_prerequisites(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for rule in makefile.rules() {
        let prereqs: Vec<String> = rule.prerequisites().collect();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut reported: HashSet<&str> = HashSet::new();
        for prereq in &prereqs {
            if !seen.insert(prereq.as_str()) && reported.insert(prereq.as_str()) {
                let rule_range = text_range_to_lsp_range(source_text, rule.syntax().text_range());
                diagnostics.push(make_diagnostic(
                    rule_range,
                    DiagnosticSeverity::WARNING,
                    "duplicate-prerequisite",
                    format!("prerequisite '{}' is listed more than once", prereq),
                ));
            }
        }
    }

    diagnostics
}

/// Check for trailing whitespace in variable assignment values.
///
/// In GNU Make, trailing whitespace is part of the variable's value (up to the
/// `#` comment or end of line). This is almost always unintentional and a
/// classic source of bugs (e.g. comparing `$(FOO)` to `"bar"` silently fails).
fn check_trailing_whitespace_in_value(source_text: &str, makefile: &Makefile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for var_def in makefile.variable_definitions() {
        let Some(expr) = var_def
            .syntax()
            .children()
            .find(|c| c.kind() == SyntaxKind::EXPR)
        else {
            continue;
        };

        // The EXPR's last child must be a WHITESPACE token (ignoring any trailing
        // COMMENT tokens). Nested nodes (e.g. variable references) don't count —
        // we only flag whitespace that's actually at the tail of the value.
        let last_non_comment = expr
            .children_with_tokens()
            .filter(|c| c.kind() != SyntaxKind::COMMENT)
            .last();
        let Some(ws) = last_non_comment
            .and_then(|c| c.into_token())
            .filter(|t| t.kind() == SyntaxKind::WHITESPACE)
        else {
            continue;
        };

        let range = text_range_to_lsp_range(source_text, ws.text_range());
        diagnostics.push(make_diagnostic(
            range,
            DiagnosticSeverity::WARNING,
            "trailing-whitespace-in-value",
            "trailing whitespace is included in the variable value".to_string(),
        ));
    }

    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    use makefile_lossless::Makefile;

    fn get_diags(text: &str) -> Vec<Diagnostic> {
        let parsed = Makefile::parse(text);
        get_diagnostics(text, &parsed, None)
    }

    fn diag_codes(text: &str) -> Vec<String> {
        get_diags(text)
            .into_iter()
            .filter_map(|d| d.code)
            .map(|c| match c {
                NumberOrString::String(s) => s,
                NumberOrString::Number(n) => n.to_string(),
            })
            .collect()
    }

    #[test]
    fn test_valid_makefile_no_diagnostics() {
        let text = "all: build\n\techo done\n";
        let diagnostics = get_diags(text);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_defined_variable_no_warning() {
        let text = "CC = gcc\nCFLAGS = $(CC) -Wall\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"undefined-variable".to_string()));
    }

    #[test]
    fn test_builtin_variable_no_warning() {
        // `$(MAKE)` is a builtin, so no undefined-variable warning. `CMD`
        // itself is unused (HINT), which is unrelated to this test.
        let text = "CMD = $(MAKE) -C subdir\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"undefined-variable".to_string()));
    }

    #[test]
    fn test_builtin_function_no_warning() {
        let text = "FILES = $(wildcard *.c)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"undefined-variable".to_string()));
    }

    #[test]
    fn test_undefined_variable_in_value() {
        let text = "CFLAGS = $(UNDEFINED_VAR) -Wall\n";
        let codes = diag_codes(text);
        assert_eq!(codes, vec!["undefined-variable"]);
    }

    #[test]
    fn test_undefined_variable_in_prerequisites() {
        let text = "all: $(MISSING_TARGETS)\n";
        let codes = diag_codes(text);
        assert_eq!(codes, vec!["undefined-variable"]);
    }

    #[test]
    fn test_undefined_variable_message() {
        let text = "CFLAGS = $(MISSING) -Wall\n";
        let diags = get_diags(text);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "variable 'MISSING' is not defined");
    }

    #[test]
    fn test_multiple_undefined_variables() {
        let text = "CFLAGS = $(FOO) $(BAR)\n";
        let codes = diag_codes(text);
        assert_eq!(codes, vec!["undefined-variable", "undefined-variable"]);
    }

    // Recursive self-reference tests

    #[test]
    fn test_recursive_self_reference() {
        let text = "FOO = $(FOO) bar\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"recursive-variable-reference".to_string()));
    }

    #[test]
    fn test_simple_expand_self_reference_ok() {
        // := expands immediately, so self-reference is valid (refers to previous value)
        let text = "FOO := $(FOO) bar\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"recursive-variable-reference".to_string()));
    }

    #[test]
    fn test_append_self_reference_ok() {
        let text = "FOO += bar\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"recursive-variable-reference".to_string()));
    }

    #[test]
    fn test_recursive_self_reference_message() {
        let text = "FOO = $(FOO)\n";
        let diags = get_diags(text);
        let self_ref: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "recursive-variable-reference".to_string(),
                    ))
            })
            .collect();
        assert_eq!(self_ref.len(), 1);
        assert!(self_ref[0].message.contains("FOO"));
    }

    // Duplicate target tests

    #[test]
    fn test_duplicate_target() {
        let text = "all: build\n\techo first\n\nall: test\n\techo second\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"duplicate-target".to_string()));
    }

    #[test]
    fn test_no_duplicate_different_targets() {
        let text = "all: build\n\techo all\n\nbuild:\n\techo build\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"duplicate-target".to_string()));
    }

    #[test]
    fn test_duplicate_target_phony_ok() {
        // .PHONY can appear multiple times
        let text = ".PHONY: all\n.PHONY: build\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"duplicate-target".to_string()));
    }

    #[test]
    fn test_duplicate_target_double_colon_ok() {
        // Double-colon rules intentionally allow duplicates
        let text = "all:: dep1\n\techo first\n\nall:: dep2\n\techo second\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"duplicate-target".to_string()));
    }

    #[test]
    fn test_duplicate_target_message() {
        let text = "all: build\n\techo first\n\nall: test\n\techo second\n";
        let diags = get_diags(text);
        let dups: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("duplicate-target".to_string())))
            .collect();
        assert_eq!(dups.len(), 1);
        assert!(dups[0].message.contains("all"));
        assert!(dups[0].message.contains("line 1"));
    }

    #[test]
    fn test_pattern_rule_not_duplicate() {
        let text = "%.o: %.c\n\t$(CC) -c $<\n\n%.o: %.cpp\n\t$(CXX) -c $<\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"duplicate-target".to_string()));
    }

    // Empty variable reference tests

    #[test]
    fn test_empty_variable_reference() {
        let text = "FOO = $()\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-variable-reference".to_string()));
    }

    #[test]
    fn test_non_empty_variable_reference_ok() {
        let text = "FOO = $(BAR)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-variable-reference".to_string()));
    }

    // Self-dependency tests

    #[test]
    fn test_self_dependency() {
        let text = "foo: foo bar\n\techo $@\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"self-dependency".to_string()));
    }

    #[test]
    fn test_no_self_dependency() {
        let text = "foo: bar baz\n\techo $@\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"self-dependency".to_string()));
    }

    #[test]
    fn test_self_dependency_message() {
        let text = "foo: foo\n\techo $@\n";
        let diags = get_diags(text);
        let self_deps: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("self-dependency".to_string())))
            .collect();
        assert_eq!(self_deps.len(), 1);
        assert!(self_deps[0].message.contains("foo"));
    }

    // Circular dependency tests

    fn circular_diags(text: &str) -> Vec<Diagnostic> {
        get_diags(text)
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("circular-dependency".to_string())))
            .collect()
    }

    #[test]
    fn test_circular_dependency_two_targets() {
        let text = "a: b\n\techo a\nb: a\n\techo b\n";
        let diags = circular_diags(text);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "circular dependency: a -> b -> a");
    }

    #[test]
    fn test_circular_dependency_three_targets() {
        let text = "a: b\n\t@:\nb: c\n\t@:\nc: a\n\t@:\n";
        let diags = circular_diags(text);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "circular dependency: a -> b -> c -> a");
    }

    #[test]
    fn test_no_circular_dependency_linear() {
        let text = "a: b\n\t@:\nb: c\n\t@:\nc:\n\t@:\n";
        let diags = circular_diags(text);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_no_circular_dependency_diamond() {
        let text = "a: b c\n\t@:\nb: d\n\t@:\nc: d\n\t@:\nd:\n\t@:\n";
        let diags = circular_diags(text);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_circular_dependency_self_loop_not_double_reported() {
        // Self-loops are reported by `check_self_dependency`, not here.
        let text = "a: a\n\t@:\n";
        let diags = circular_diags(text);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_circular_dependency_via_accumulated_prereqs() {
        // Make merges prerequisites across multiple rules for the same target,
        // so this still forms a cycle a -> b -> a.
        let text = "a: b\n\t@:\nb:\n\t@:\nb: a\n";
        let diags = circular_diags(text);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "circular dependency: a -> b -> a");
    }

    #[test]
    fn test_circular_dependency_ignores_undefined_prereq() {
        // `missing` is a file on disk (or just an error), not a target —
        // no cycle possible through it.
        let text = "a: missing\n\t@:\n";
        let diags = circular_diags(text);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_circular_dependency_dedupes_rotations() {
        // The graph has one cycle (a, b, c) which the DFS could discover from
        // any starting node — make sure we only report it once.
        let text = "a: b\n\t@:\nb: c\n\t@:\nc: a\n\t@:\nentry: a b c\n\t@:\n";
        let diags = circular_diags(text);
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn test_circular_dependency_two_disjoint_cycles() {
        let text = concat!("a: b\n\t@:\nb: a\n\t@:\n", "x: y\n\t@:\ny: x\n\t@:\n",);
        let diags = circular_diags(text);
        assert_eq!(diags.len(), 2);
    }

    // Undefined .PHONY target tests

    #[test]
    fn test_undefined_phony_target() {
        let text = ".PHONY: clean\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"undefined-phony-target".to_string()));
    }

    #[test]
    fn test_defined_phony_target_ok() {
        let text = ".PHONY: clean\nclean:\n\trm -f *.o\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"undefined-phony-target".to_string()));
    }

    #[test]
    fn test_phony_partially_defined() {
        let text = ".PHONY: all clean\nall: build\n\techo done\n";
        let diags = get_diags(text);
        let phony_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code == Some(NumberOrString::String("undefined-phony-target".to_string()))
            })
            .collect();
        assert_eq!(phony_diags.len(), 1);
        assert!(phony_diags[0].message.contains("clean"));
    }

    // Unused / unreferenced .PHONY target tests

    #[test]
    fn test_unused_phony_target_empty_rule() {
        // .PHONY: clena has an empty rule and nothing references it — typo.
        let text = ".PHONY: clena\nclena:\n";
        let diags = get_diags(text);
        let unused: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("unused-phony-target".to_string())))
            .collect();
        assert_eq!(unused.len(), 1);
        assert!(unused[0].message.contains("clena"));
        assert_eq!(unused[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn test_unused_phony_target_silenced_when_referenced() {
        // 'helper' has an empty rule but `all` depends on it -> ok.
        let text = ".PHONY: helper\nall: helper\n\t@:\nhelper:\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-phony-target".to_string()));
    }

    #[test]
    fn test_unused_phony_target_silenced_when_recipe_present() {
        // Has a recipe -> falls through to unreferenced-phony-target instead,
        // which is a hint not a warning.
        let text = ".PHONY: foo\nfoo:\n\techo foo\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-phony-target".to_string()));
        assert!(codes.contains(&"unreferenced-phony-target".to_string()));
    }

    #[test]
    fn test_unreferenced_phony_target_hint() {
        // 'lint' is not in PHONY_ENTRY_POINTS and nothing depends on it.
        let text = ".PHONY: format\nformat:\n\tfoo\n";
        let diags = get_diags(text);
        let hints: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "unreferenced-phony-target".to_string(),
                    ))
            })
            .collect();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].severity, Some(DiagnosticSeverity::HINT));
    }

    #[test]
    fn test_unreferenced_phony_silenced_for_entry_points() {
        // 'all', 'install', 'clean' are conventional entry points -> no hint.
        let text = concat!(
            ".PHONY: all install clean\n",
            "all:\n\t@:\n",
            "install:\n\t@:\n",
            "clean:\n\t@:\n",
        );
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unreferenced-phony-target".to_string()));
    }

    #[test]
    fn test_unreferenced_phony_silenced_when_referenced() {
        let text = ".PHONY: lint\nall: lint\n\t@:\nlint:\n\tfoo\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unreferenced-phony-target".to_string()));
    }

    #[test]
    fn test_unused_phony_self_reference_does_not_count() {
        // A rule listing itself as prerequisite is not a real incoming edge —
        // and check_self_dependency will flag it separately. The unused-phony
        // warning should still fire.
        let text = ".PHONY: foo\nfoo: foo\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"unused-phony-target".to_string()));
    }

    #[test]
    fn test_unused_phony_multiple_rules_one_nonempty() {
        // 'foo' has two rules; one is empty, the other has a recipe. As long
        // as any rule does real work it's not "unused"; demote to hint only.
        let text = ".PHONY: foo\nfoo:\nfoo:\n\techo hi\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-phony-target".to_string()));
        assert!(codes.contains(&"unreferenced-phony-target".to_string()));
    }

    // Include missing path tests

    #[test]
    fn test_include_with_path_ok() {
        let text = "include config.mk\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"include-missing-path".to_string()));
    }

    // Spaces instead of tab tests

    #[test]
    fn test_tab_indented_recipe_ok() {
        let text = "all:\n\techo done\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"spaces-instead-of-tab".to_string()));
    }

    #[test]
    fn test_spaces_instead_of_tab() {
        let text = "all:\n    echo done\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"spaces-instead-of-tab".to_string()));
    }

    #[test]
    fn test_spaces_instead_of_tab_message() {
        let text = "all:\n    echo done\n";
        let diags = get_diags(text);
        let space_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("spaces-instead-of-tab".to_string())))
            .collect();
        assert_eq!(space_diags.len(), 1);
        assert_eq!(
            space_diags[0].message,
            "recipe lines must start with a tab, not spaces"
        );
        assert_eq!(space_diags[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn test_multiple_space_indented_recipes() {
        let text = "all:\n    echo first\n    echo second\n";
        let diags = get_diags(text);
        let space_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("spaces-instead-of-tab".to_string())))
            .collect();
        assert_eq!(space_diags.len(), 2);
    }

    // Trailing whitespace in value tests

    #[test]
    fn test_trailing_whitespace_in_value() {
        let text = "FOO = bar \n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    #[test]
    fn test_trailing_tab_in_value() {
        let text = "FOO = bar\t\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    #[test]
    fn test_no_trailing_whitespace_clean_value() {
        let text = "FOO = bar\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    #[test]
    fn test_internal_whitespace_not_flagged() {
        let text = "FOO = bar baz\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    #[test]
    fn test_empty_value_not_flagged() {
        // `FOO = ` is an empty assignment; the whitespace is between `=` and EOL,
        // not part of the value.
        let text = "FOO = \n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    #[test]
    fn test_trailing_whitespace_before_comment_flagged() {
        // `FOO = bar # comment` sets FOO to "bar " (trailing space captured).
        let text = "FOO = bar # comment\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    #[test]
    fn test_line_continuation_not_flagged() {
        // The trailing whitespace before `\` is part of a continued line, not the
        // end of the value. We only care about the final value's tail.
        let text = "FOO = bar \\\n\tbaz\n";
        let codes = diag_codes(text);
        // The trailing token here is BACKSLASH, not WHITESPACE — so no warning.
        assert!(!codes.contains(&"trailing-whitespace-in-value".to_string()));
    }

    // Duplicate prerequisites tests

    #[test]
    fn test_duplicate_prerequisite() {
        let text = "foo: a b a\n\techo $@\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"duplicate-prerequisite".to_string()));
    }

    #[test]
    fn test_no_duplicate_prerequisites() {
        let text = "foo: a b c\n\techo $@\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"duplicate-prerequisite".to_string()));
    }

    #[test]
    fn test_duplicate_prerequisite_message() {
        let text = "foo: a b a\n\techo $@\n";
        let diags = get_diags(text);
        let dup_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code == Some(NumberOrString::String("duplicate-prerequisite".to_string()))
            })
            .collect();
        assert_eq!(dup_diags.len(), 1);
        assert_eq!(
            dup_diags[0].message,
            "prerequisite 'a' is listed more than once"
        );
    }

    #[test]
    fn test_duplicate_prerequisite_reported_once_per_name() {
        // `a` appears three times — should report once, not twice.
        let text = "foo: a b a c a\n\techo $@\n";
        let diags = get_diags(text);
        let dup_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code == Some(NumberOrString::String("duplicate-prerequisite".to_string()))
            })
            .collect();
        assert_eq!(dup_diags.len(), 1);
    }

    #[test]
    fn test_duplicate_prerequisites_distinct_names() {
        let text = "foo: a b a b\n\techo $@\n";
        let diags = get_diags(text);
        let dup_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code == Some(NumberOrString::String("duplicate-prerequisite".to_string()))
            })
            .collect();
        assert_eq!(dup_diags.len(), 2);
    }

    #[test]
    fn test_duplicate_prerequisite_across_rules_ok() {
        // Same prereq in two different rules is fine.
        let text = "foo: shared\n\techo foo\n\nbar: shared\n\techo bar\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"duplicate-prerequisite".to_string()));
    }

    // Shell in recursive assignment tests

    #[test]
    fn test_shell_in_recursive_assignment() {
        let text = "FILES = $(shell ls)\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"shell-in-recursive-assignment".to_string()));
    }

    #[test]
    fn test_shell_in_simply_expanded_assignment_ok() {
        let text = "FILES := $(shell ls)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"shell-in-recursive-assignment".to_string()));
    }

    #[test]
    fn test_shell_in_immediate_expand_assignment_ok() {
        let text = "FILES ::= $(shell ls)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"shell-in-recursive-assignment".to_string()));
    }

    #[test]
    fn test_no_shell_in_recursive_ok() {
        let text = "FOO = $(BAR)\nBAR = baz\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"shell-in-recursive-assignment".to_string()));
    }

    #[test]
    fn test_shell_variable_reference_not_function_call_ok() {
        // `$(shell)` (no args) is a variable reference to a variable named
        // "shell", not a function call. The bug only applies to the function form.
        let text = "shell = bash\nFOO = $(shell)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"shell-in-recursive-assignment".to_string()));
    }

    #[test]
    fn test_shell_in_recursive_message() {
        let text = "FILES = $(shell ls)\n";
        let diags = get_diags(text);
        let shell_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "shell-in-recursive-assignment".to_string(),
                    ))
            })
            .collect();
        assert_eq!(shell_diags.len(), 1);
        assert!(shell_diags[0].message.contains("re-run on every expansion"));
        assert_eq!(shell_diags[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn test_shell_nested_in_recursive_assignment() {
        // $(shell ...) wrapped in another function call still counts.
        let text = "FILES = $(strip $(shell ls))\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"shell-in-recursive-assignment".to_string()));
    }

    #[test]
    fn test_trailing_whitespace_message() {
        let text = "FOO = bar   \n";
        let diags = get_diags(text);
        let ws_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "trailing-whitespace-in-value".to_string(),
                    ))
            })
            .collect();
        assert_eq!(ws_diags.len(), 1);
        assert_eq!(
            ws_diags[0].message,
            "trailing whitespace is included in the variable value"
        );
        assert_eq!(ws_diags[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    // Empty automatic variable tests

    #[test]
    fn test_dollar_less_in_rule_with_no_prereqs() {
        let text = "foo:\n\techo $<\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_less_with_prereqs_ok() {
        let text = "foo: bar\n\techo $<\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_caret_in_rule_with_no_prereqs() {
        let text = "foo:\n\techo $^\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_plus_in_rule_with_no_prereqs() {
        let text = "foo:\n\techo $+\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_question_in_rule_with_no_prereqs() {
        let text = "foo:\n\techo $?\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_at_with_no_prereqs_ok() {
        // $@ is the target — always defined regardless of prereqs.
        let text = "foo:\n\techo $@\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_star_in_non_pattern_rule() {
        let text = "foo: bar\n\techo $*\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_star_in_pattern_rule_ok() {
        let text = "%.o: %.c\n\techo $*\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_dollar_less_in_pattern_rule_ok() {
        let text = "%.o: %.c\n\t$(CC) -c $<\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_parenthesized_form_flagged() {
        let text = "foo:\n\techo $(<)\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_braced_form_flagged() {
        let text = "foo:\n\techo ${<}\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_escaped_dollar_not_flagged() {
        let text = "foo:\n\techo $$<\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-automatic-variable".to_string()));
    }

    #[test]
    fn test_empty_auto_var_message() {
        let text = "foo:\n\techo $<\n";
        let diags = get_diags(text);
        let auto: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "empty-automatic-variable".to_string(),
                    ))
            })
            .collect();
        assert_eq!(auto.len(), 1);
        assert!(auto[0].message.contains("$<"));
        assert!(auto[0].message.contains("no prerequisites"));
    }

    // Unused variable tests

    #[test]
    fn test_unused_variable() {
        let text = "FOO = bar\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_used_in_other_variable_ok() {
        let text = "FOO = bar\nBAZ = $(FOO)\nused: \n\techo $(BAZ)\n";
        let codes = diag_codes(text);
        // BAZ is used in recipe; FOO is used in BAZ.
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_used_in_recipe_ok() {
        let text = "FOO = bar\nall:\n\techo $(FOO)\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_used_in_recipe_braces_ok() {
        let text = "FOO = bar\nall:\n\techo ${FOO}\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_used_in_ifdef_ok() {
        let text = "FOO = bar\nifdef FOO\nVAR = x\nendif\n";
        let codes = diag_codes(text);
        // FOO is referenced by `ifdef FOO`. VAR is inside a conditional, so
        // skipped from the unused check.
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_exported_variable_skipped() {
        let text = "export FOO = bar\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_overriding_builtin_skipped() {
        let text = "CC = my-special-gcc\n";
        let codes = diag_codes(text);
        // CC is a builtin; assigning it overrides make's default. Not unused.
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_inside_conditional_skipped() {
        let text = "ifdef DEBUG\nFOO = bar\nendif\n";
        let codes = diag_codes(text);
        // FOO is defined inside a conditional — likely a config toggle.
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_unused_variable_message() {
        let text = "FOO = bar\n";
        let diags = get_diags(text);
        let unused: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("unused-variable".to_string())))
            .collect();
        assert_eq!(unused.len(), 1);
        assert_eq!(
            unused[0].message,
            "variable 'FOO' is defined but never used"
        );
        assert_eq!(unused[0].severity, Some(DiagnosticSeverity::HINT));
    }

    #[test]
    fn test_used_in_target_or_prereq_ok() {
        let text = "SOURCES = a.c\n$(SOURCES):\n\techo $@\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unused-variable".to_string()));
    }

    #[test]
    fn test_dollar_dollar_in_recipe_does_not_count() {
        // `$$FOO` is shell variable expansion, not a make variable reference.
        let text = "FOO = bar\nall:\n\techo $$FOO\n";
        let codes = diag_codes(text);
        // FOO is unused — `$$FOO` is the shell's FOO, not make's.
        assert!(codes.contains(&"unused-variable".to_string()));
    }

    // Missing include file tests

    fn diags_with_dir(text: &str, dir: &std::path::Path) -> Vec<Diagnostic> {
        let parsed = Makefile::parse(text);
        get_diagnostics(text, &parsed, Some(dir))
    }

    fn codes_with_dir(text: &str, dir: &std::path::Path) -> Vec<String> {
        diags_with_dir(text, dir)
            .into_iter()
            .filter_map(|d| d.code)
            .map(|c| match c {
                NumberOrString::String(s) => s,
                NumberOrString::Number(n) => n.to_string(),
            })
            .collect()
    }

    #[test]
    fn test_missing_include_file_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let codes = codes_with_dir("include config.mk\n", dir.path());
        assert!(codes.contains(&"missing-include-file".to_string()));
    }

    #[test]
    fn test_existing_include_file_ok() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.mk"), "").unwrap();
        let codes = codes_with_dir("include config.mk\n", dir.path());
        assert!(!codes.contains(&"missing-include-file".to_string()));
    }

    #[test]
    fn test_optional_include_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let codes = codes_with_dir("-include config.mk\n", dir.path());
        assert!(!codes.contains(&"missing-include-file".to_string()));
    }

    #[test]
    fn test_include_with_variable_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let codes = codes_with_dir("CONFIG = config.mk\ninclude $(CONFIG)\n", dir.path());
        // Can't resolve $(CONFIG) at lint time, so we don't flag.
        assert!(!codes.contains(&"missing-include-file".to_string()));
    }

    #[test]
    fn test_no_base_dir_skips_check() {
        // When no base_dir is passed, the check doesn't run. (Used by the
        // default diag_codes helper.)
        let codes = diag_codes("include nonexistent.mk\n");
        assert!(!codes.contains(&"missing-include-file".to_string()));
    }

    #[test]
    fn test_missing_include_file_message() {
        let dir = tempfile::tempdir().unwrap();
        let diags = diags_with_dir("include config.mk\n", dir.path());
        let missing: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("missing-include-file".to_string())))
            .collect();
        assert_eq!(missing.len(), 1);
        assert_eq!(
            missing[0].message,
            "included file 'config.mk' does not exist"
        );
        assert_eq!(missing[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    // Empty rule probably phony tests

    #[test]
    fn test_empty_rule_flagged() {
        let text = "clean:\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"empty-rule-probably-phony".to_string()));
    }

    #[test]
    fn test_rule_with_recipe_not_flagged() {
        let text = "clean:\n\trm -f *.o\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-rule-probably-phony".to_string()));
    }

    #[test]
    fn test_rule_with_prereqs_not_flagged() {
        // `all: build test` is a meta-target — common and fine.
        let text = "all: build test\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-rule-probably-phony".to_string()));
    }

    #[test]
    fn test_phony_special_target_not_flagged() {
        let text = ".PHONY:\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-rule-probably-phony".to_string()));
    }

    #[test]
    fn test_pattern_rule_empty_ok() {
        let text = "%.o:\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-rule-probably-phony".to_string()));
    }

    #[test]
    fn test_already_phony_not_flagged() {
        let text = ".PHONY: clean\nclean:\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"empty-rule-probably-phony".to_string()));
    }

    #[test]
    fn test_empty_rule_message() {
        let text = "clean:\n";
        let diags = get_diags(text);
        let hints: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "empty-rule-probably-phony".to_string(),
                    ))
            })
            .collect();
        assert_eq!(hints.len(), 1);
        assert!(hints[0].message.contains("clean"));
        assert!(hints[0].message.contains(".PHONY"));
        assert_eq!(hints[0].severity, Some(DiagnosticSeverity::HINT));
    }

    // Orphan recipe line tests

    #[test]
    fn test_orphan_recipe_at_top() {
        let text = "\techo orphan\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"orphan-recipe-line".to_string()));
    }

    #[test]
    fn test_orphan_recipe_after_variable() {
        let text = "VAR = 1\n\techo orphan\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"orphan-recipe-line".to_string()));
    }

    #[test]
    fn test_recipe_inside_rule_ok() {
        let text = "all:\n\techo good\n\techo also-good\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"orphan-recipe-line".to_string()));
    }

    #[test]
    fn test_orphan_recipe_between_rules() {
        let text = "all:\n\techo good\n\nVAR = 2\n\techo orphan\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"orphan-recipe-line".to_string()));
    }

    #[test]
    fn test_orphan_recipe_message() {
        let text = "\techo orphan\n";
        let diags = get_diags(text);
        let orphans: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("orphan-recipe-line".to_string())))
            .collect();
        assert_eq!(orphans.len(), 1);
        assert_eq!(
            orphans[0].message,
            "recipe line is not attached to any target"
        );
        assert_eq!(orphans[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn test_multiple_orphan_recipes() {
        let text = "\techo a\n\techo b\n";
        let diags = get_diags(text);
        let orphans: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String("orphan-recipe-line".to_string())))
            .collect();
        assert_eq!(orphans.len(), 2);
    }

    // Mixed assignment operator tests

    #[test]
    fn test_mixed_recursive_and_immediate() {
        let text = "FOO = a\nFOO := b\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"mixed-assignment-operators".to_string()));
    }

    #[test]
    fn test_same_recursive_twice_not_mixed() {
        let text = "FOO = a\nFOO = b\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"mixed-assignment-operators".to_string()));
    }

    #[test]
    fn test_immediate_and_append_ok() {
        let text = "FOO := a\nFOO += b\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"mixed-assignment-operators".to_string()));
    }

    #[test]
    fn test_recursive_and_conditional_ok() {
        let text = "FOO = a\nFOO ?= b\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"mixed-assignment-operators".to_string()));
    }

    #[test]
    fn test_immediate_triple_colon_mix() {
        let text = "FOO = a\nFOO ::= b\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"mixed-assignment-operators".to_string()));
    }

    #[test]
    fn test_different_variables_not_mixed() {
        let text = "FOO = a\nBAR := b\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"mixed-assignment-operators".to_string()));
    }

    #[test]
    fn test_mixed_assignment_flags_both() {
        let text = "FOO = a\nFOO := b\n";
        let diags = get_diags(text);
        let mixed: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "mixed-assignment-operators".to_string(),
                    ))
            })
            .collect();
        assert_eq!(mixed.len(), 2);
        assert!(mixed[0].message.contains("FOO"));
        assert_eq!(mixed[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    // Unterminated conditional tests

    #[test]
    fn test_unterminated_ifdef() {
        let text = "ifdef DEBUG\nFOO = bar\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"unterminated-conditional".to_string()));
    }

    #[test]
    fn test_terminated_ifdef_ok() {
        let text = "ifdef DEBUG\nFOO = bar\nendif\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unterminated-conditional".to_string()));
    }

    #[test]
    fn test_unterminated_ifeq() {
        let text = "ifeq ($(CC),gcc)\nFOO = bar\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"unterminated-conditional".to_string()));
    }

    #[test]
    fn test_unterminated_with_else() {
        let text = "ifdef DEBUG\nFOO = bar\nelse\nFOO = baz\n";
        let codes = diag_codes(text);
        assert!(codes.contains(&"unterminated-conditional".to_string()));
    }

    #[test]
    fn test_unterminated_nested_outer() {
        // Inner is closed, outer is not.
        let text = "ifdef OUTER\nifdef INNER\nFOO = bar\nendif\n";
        let codes = diag_codes(text);
        let count = codes
            .iter()
            .filter(|c| c.as_str() == "unterminated-conditional")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_bare_else_not_double_flagged() {
        // Bare `else` already produces a parse error; we should not also flag it
        // as unterminated (it has no conditional_type).
        let text = "else\nFOO = bar\n";
        let codes = diag_codes(text);
        assert!(!codes.contains(&"unterminated-conditional".to_string()));
    }

    #[test]
    fn test_unterminated_conditional_message() {
        let text = "ifdef DEBUG\nFOO = bar\n";
        let diags = get_diags(text);
        let unt: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(NumberOrString::String(
                        "unterminated-conditional".to_string(),
                    ))
            })
            .collect();
        assert_eq!(unt.len(), 1);
        assert_eq!(unt[0].message, "'ifdef' is missing a matching 'endif'");
        assert_eq!(unt[0].severity, Some(DiagnosticSeverity::ERROR));
    }
}
