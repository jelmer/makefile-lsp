//! Diagnostics for Makefile files.

use std::collections::{HashMap, HashSet};

use makefile_lossless::{Makefile, MakefileItem, Parse, VariableReference};
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

use crate::position::text_range_to_lsp_range;

/// GNU Make automatic variables (single-character after $).
const AUTOMATIC_VARIABLES: &[&str] = &["@", "<", "^", "+", "?", "*", "%"];

/// GNU Make automatic variable variants (e.g. $(@D), $(@F)).
const AUTOMATIC_VARIABLE_VARIANTS: &[&str] = &[
    "@D", "@F", "<D", "<F", "^D", "^F", "+D", "+F", "?D", "?F", "*D", "*F",
];

/// Well-known GNU Make built-in variables.
const BUILTIN_VARIABLES: &[&str] = &[
    "MAKE",
    "MAKECMDGOALS",
    "MAKEFLAGS",
    "MAKEFILES",
    "MAKELEVEL",
    "MAKEOVERRIDES",
    "MAKESHELL",
    "MAKE_RESTARTS",
    "MAKE_TERMERR",
    "MAKE_TERMOUT",
    "MAKE_VERSION",
    "MFLAGS",
    "SHELL",
    "SUFFIXES",
    "VPATH",
    ".DEFAULT_GOAL",
    ".EXTRA_PREREQS",
    ".FEATURES",
    ".INCLUDE_DIRS",
    ".LOADED",
    ".RECIPEPREFIX",
    ".SHELLFLAGS",
    ".VARIABLES",
    // Implicit rule variables
    "AR",
    "ARFLAGS",
    "AS",
    "ASFLAGS",
    "CC",
    "CFLAGS",
    "CO",
    "COFLAGS",
    "CPP",
    "CPPFLAGS",
    "CTANGLE",
    "CWEAVE",
    "CXX",
    "CXXFLAGS",
    "FC",
    "FFLAGS",
    "GET",
    "GFLAGS",
    "LDFLAGS",
    "LDLIBS",
    "LEX",
    "LFLAGS",
    "LINT",
    "LINTFLAGS",
    "M2C",
    "MAKEINFO",
    "PC",
    "PFLAGS",
    "RFLAGS",
    "RM",
    "TANGLE",
    "TEX",
    "TEXI2DVI",
    "WEAVE",
    "YACC",
    "YFLAGS",
    // Common environment variables
    "CURDIR",
    "HOME",
    "PATH",
    "PWD",
    "TERM",
    "USER",
    // Output sync
    "OUTPUT_OPTION",
    ".LIBPATTERNS",
];

/// Built-in GNU Make functions.
const BUILTIN_FUNCTIONS: &[&str] = &[
    "subst",
    "patsubst",
    "strip",
    "findstring",
    "filter",
    "filter-out",
    "sort",
    "word",
    "wordlist",
    "words",
    "firstword",
    "lastword",
    "dir",
    "notdir",
    "suffix",
    "basename",
    "addsuffix",
    "addprefix",
    "join",
    "wildcard",
    "realpath",
    "abspath",
    "if",
    "or",
    "and",
    "foreach",
    "call",
    "eval",
    "origin",
    "flavor",
    "value",
    "error",
    "warning",
    "info",
    "shell",
    "file",
    "guile",
    "let",
];

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

/// Check if a variable name is a well-known built-in, automatic variable, or function.
fn is_known_variable(name: &str) -> bool {
    AUTOMATIC_VARIABLES.contains(&name)
        || AUTOMATIC_VARIABLE_VARIANTS.contains(&name)
        || BUILTIN_VARIABLES.contains(&name)
        || BUILTIN_FUNCTIONS.contains(&name)
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
        source_text, &makefile,
    ));
    diagnostics.extend(check_empty_variable_references(source_text, &makefile));
    diagnostics.extend(check_self_dependency(source_text, &makefile));
    diagnostics.extend(check_duplicate_targets(source_text, &makefile));
    diagnostics.extend(check_missing_phony_targets(source_text, &makefile));
    diagnostics.extend(check_include_missing_path(source_text, &makefile));

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
        if is_known_variable(&name) || defined_vars.contains(&name) {
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
                let rule_range =
                    text_range_to_lsp_range(source_text, rule.syntax().text_range());
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
                d.code == Some(NumberOrString::String("recursive-variable-reference".to_string()))
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
}
