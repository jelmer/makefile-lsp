//! Diagnostics for Makefile files.

use std::collections::HashSet;

use makefile_lossless::{Makefile, Parse};
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
        .errors()
        .iter()
        .map(|error| {
            let line = error.line.saturating_sub(1) as u32;
            let range = Range {
                start: Position::new(line, 0),
                end: Position::new(line, error.context.len() as u32),
            };
            make_diagnostic(
                range,
                DiagnosticSeverity::ERROR,
                "parse-error",
                error.message.clone(),
            )
        })
        .collect();

    let makefile = parsed.tree();
    diagnostics.extend(check_undefined_variables(source_text, &makefile));

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
}
