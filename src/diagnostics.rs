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
pub fn get_diagnostics(
    source_text: &str,
    parsed: &Parse<makefile_lossless::Makefile>,
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
    diagnostics.extend(check_duplicate_targets(source_text, &makefile));
    diagnostics.extend(check_missing_phony_targets(source_text, &makefile));
    diagnostics.extend(check_include_missing_path(source_text, &makefile));
    diagnostics.extend(check_spaces_in_recipes(source_text, &makefile));
    diagnostics.extend(check_trailing_whitespace_in_value(source_text, &makefile));
    diagnostics.extend(check_duplicate_prerequisites(source_text, &makefile));
    diagnostics.extend(check_shell_in_recursive_assignment(source_text, &makefile));
    diagnostics.extend(check_empty_automatic_variables(source_text, &makefile));
    diagnostics.extend(check_unterminated_conditionals(source_text, &makefile));

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
        get_diagnostics(text, &parsed)
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
        let diagnostics = get_diags(text);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_builtin_variable_no_warning() {
        let text = "CMD = $(MAKE) -C subdir\n";
        let diagnostics = get_diags(text);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_builtin_function_no_warning() {
        let text = "FILES = $(wildcard *.c)\n";
        let diagnostics = get_diags(text);
        assert!(diagnostics.is_empty());
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
