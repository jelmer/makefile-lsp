//! Hover information for Makefiles.

use makefile_lossless::{is_in_prerequisites, variable_at_offset, word_at_offset, Makefile};
use tower_lsp_server::ls_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::position::try_position_to_offset;

/// Well-known GNU Make automatic variables with documentation.
const AUTOMATIC_VARIABLES: &[(&str, &str)] = &[
    ("@", "The file name of the target of the rule."),
    ("<", "The name of the first prerequisite."),
    ("^", "The names of all the prerequisites, with spaces between them (no duplicates)."),
    ("+", "Like `$^`, but prerequisites listed more than once are duplicated in the order they were listed."),
    ("?", "The names of all the prerequisites that are newer than the target."),
    ("*", "The stem with which an implicit rule matches."),
    ("@D", "The directory part of `$@`."),
    ("@F", "The file-within-directory part of `$@`."),
    ("<D", "The directory part of `$<`."),
    ("<F", "The file-within-directory part of `$<`."),
];

/// Well-known GNU Make built-in functions with documentation.
const BUILTIN_FUNCTIONS: &[(&str, &str)] = &[
    ("subst", "`$(subst from,to,text)` — Replace occurrences of *from* with *to* in *text*."),
    ("patsubst", "`$(patsubst pattern,replacement,text)` — Pattern substitution."),
    ("strip", "`$(strip string)` — Remove leading/trailing whitespace."),
    ("findstring", "`$(findstring find,in)` — Search for *find* in *in*."),
    ("filter", "`$(filter pattern…,text)` — Keep words matching *pattern*."),
    ("filter-out", "`$(filter-out pattern…,text)` — Remove words matching *pattern*."),
    ("sort", "`$(sort list)` — Sort words and remove duplicates."),
    ("word", "`$(word n,text)` — Extract the *n*th word."),
    ("wordlist", "`$(wordlist s,e,text)` — Extract words *s* through *e*."),
    ("words", "`$(words text)` — Count the number of words."),
    ("firstword", "`$(firstword names…)` — The first word."),
    ("lastword", "`$(lastword names…)` — The last word."),
    ("dir", "`$(dir names…)` — Directory part of file names."),
    ("notdir", "`$(notdir names…)` — Non-directory part of file names."),
    ("suffix", "`$(suffix names…)` — Suffix of file names."),
    ("basename", "`$(basename names…)` — Basename of file names."),
    ("addsuffix", "`$(addsuffix suffix,names…)` — Append *suffix* to each word."),
    ("addprefix", "`$(addprefix prefix,names…)` — Prepend *prefix* to each word."),
    ("join", "`$(join list1,list2)` — Join two lists pairwise."),
    ("wildcard", "`$(wildcard pattern)` — Expand file name wildcards."),
    ("realpath", "`$(realpath names…)` — Canonical absolute path names."),
    ("abspath", "`$(abspath names…)` — Absolute path names."),
    ("if", "`$(if condition,then-part[,else-part])` — Conditional expansion."),
    ("or", "`$(or cond1[,cond2…])` — Short-circuiting logical OR."),
    ("and", "`$(and cond1[,cond2…])` — Short-circuiting logical AND."),
    ("foreach", "`$(foreach var,list,text)` — Iterate over a list."),
    ("call", "`$(call variable,param…)` — Call a user-defined function."),
    ("eval", "`$(eval text)` — Evaluate *text* as makefile syntax."),
    ("origin", "`$(origin variable)` — How a variable was defined."),
    ("flavor", "`$(flavor variable)` — The flavor of a variable (simple/recursive)."),
    ("value", "`$(value variable)` — The unexpanded value of a variable."),
    ("error", "`$(error text…)` — Generate a fatal error."),
    ("warning", "`$(warning text…)` — Generate a warning message."),
    ("info", "`$(info text…)` — Print an informational message."),
    ("shell", "`$(shell command)` — Execute a shell command and return its output."),
    ("file", "`$(file op filename[,text])` — Read from or write to a file."),
    ("guile", "`$(guile code)` — Evaluate GNU Guile code (if compiled with Guile support)."),
];

/// Well-known GNU Make built-in special targets.
const SPECIAL_TARGETS: &[(&str, &str)] = &[
    (".PHONY", "Declare targets that do not represent files."),
    (".SUFFIXES", "Define the list of suffixes for old-fashioned suffix rules."),
    (".DEFAULT", "Recipe for targets with no other rule."),
    (".PRECIOUS", "Targets that should not be deleted if make is interrupted."),
    (".INTERMEDIATE", "Targets treated as intermediate files."),
    (".SECONDARY", "Targets treated as secondary (intermediate but never auto-deleted)."),
    (".SECONDEXPANSION", "Enable second expansion of prerequisites."),
    (".DELETE_ON_ERROR", "Delete the target file if its recipe exits with a nonzero status."),
    (".IGNORE", "Ignore errors in recipes for these targets."),
    (".SILENT", "Do not echo recipes for these targets."),
    (".EXPORT_ALL_VARIABLES", "Export all variables to child processes."),
    (".NOTPARALLEL", "Disable parallel execution of recipes."),
    (".ONESHELL", "Run entire recipe in a single shell invocation."),
    (".POSIX", "Enable POSIX-conforming mode."),
];

fn markdown_hover(text: String) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: text,
        }),
        range: None,
    }
}

/// Get hover information for the symbol at the given position.
pub fn get_hover(
    makefile: &Makefile,
    source_text: &str,
    position: Position,
) -> Option<Hover> {
    let offset = try_position_to_offset(source_text, position)?;
    let byte_offset: usize = offset.into();

    // Variable reference: $(VAR) or ${VAR}
    if let Some(var_name) = variable_at_offset(source_text, byte_offset) {
        // Check automatic variables
        if let Some((_, doc)) = AUTOMATIC_VARIABLES.iter().find(|(n, _)| *n == var_name) {
            return Some(markdown_hover(format!("**`${}`** — {}", var_name, doc)));
        }

        // Check built-in functions (var_name may be "wildcard *.c", so match the first word)
        let func_name = var_name.split_whitespace().next().unwrap_or(var_name);
        if let Some((_, doc)) = BUILTIN_FUNCTIONS.iter().find(|(n, _)| *n == func_name) {
            return Some(markdown_hover(doc.to_string()));
        }

        // Check user-defined variables
        if let Some(var_def) = makefile
            .variable_definitions()
            .find(|v| v.name().as_deref() == Some(var_name))
        {
            let op = var_def.assignment_operator().unwrap_or_else(|| "=".to_string());
            let value = var_def
                .raw_value()
                .map(|v| v.trim().to_string())
                .unwrap_or_default();
            return Some(markdown_hover(format!(
                "```makefile\n{} {} {}\n```",
                var_name, op, value
            )));
        }

        return None;
    }

    // Word in prerequisites area or at start of line (target name)
    if let Some(word) = word_at_offset(source_text, byte_offset) {
        // Check special targets
        if let Some((_, doc)) = SPECIAL_TARGETS.iter().find(|(n, _)| *n == word) {
            return Some(markdown_hover(format!("**`{}`** — {}", word, doc)));
        }

        // Show rule info if hovering over a target reference in prerequisites
        if is_in_prerequisites(source_text, byte_offset) {
            if let Some(rule) = makefile.rules().find(|r| r.targets().any(|t| t == word)) {
                let prereqs: Vec<String> = rule.prerequisites().collect();
                let recipes: Vec<String> = rule.recipes().collect();
                let mut info = format!("**`{}`**", word);
                if !prereqs.is_empty() {
                    info.push_str(&format!("\n\nPrerequisites: `{}`", prereqs.join(" ")));
                }
                if !recipes.is_empty() {
                    info.push_str("\n\n```makefile");
                    for r in &recipes {
                        info.push_str(&format!("\n\t{}", r));
                    }
                    info.push_str("\n```");
                }
                return Some(markdown_hover(info));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hover_text(text: &str, pos: Position) -> Option<String> {
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        get_hover(&makefile, text, pos).map(|h| match h.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!("Expected markup content"),
        })
    }

    #[test]
    fn test_hover_user_variable() {
        let text = "CC = gcc\nall:\n\t$(CC) main.c\n";
        let result = hover_text(text, Position::new(2, 3));
        assert!(result.is_some());
        let content = result.unwrap();
        assert!(content.contains("CC"));
        assert!(content.contains("gcc"));
    }

    #[test]
    fn test_hover_automatic_variable() {
        let text = "all:\n\techo $(@D)\n";
        // @ is col 7, but $(@D) — let's test with a simple $(VAR) context
        // Actually, automatic vars like $@ are single-char after $, not inside $()
        // But $(@D) is inside $(). Offset for @D: "\techo $(@D)\n"
        // line 1: "\techo $(@D)", $ at col 6, ( at col 7, @ at col 8
        let result = hover_text(text, Position::new(1, 8));
        assert!(result.is_some());
        assert!(result.unwrap().contains("directory"));
    }

    #[test]
    fn test_hover_builtin_function() {
        let text = "FILES = $(wildcard *.c)\n";
        // "$(wildcard" — w at col 10
        let result = hover_text(text, Position::new(0, 10));
        assert!(result.is_some());
        assert!(result.unwrap().contains("wildcard"));
    }

    #[test]
    fn test_hover_special_target() {
        let text = "all: build\n.PHONY: all\n";
        // ".PHONY" on line 1, col 0
        let result = hover_text(text, Position::new(1, 0));
        assert!(result.is_some());
        assert!(result.unwrap().contains("do not represent files"));
    }

    #[test]
    fn test_hover_prerequisite_target() {
        let text = "all: build\n\nbuild:\n\techo ok\n";
        // "build" in prerequisites, col 5
        let result = hover_text(text, Position::new(0, 5));
        assert!(result.is_some());
        let content = result.unwrap();
        assert!(content.contains("build"));
        assert!(content.contains("echo ok"));
    }

    #[test]
    fn test_hover_nothing() {
        let text = "all:\n\techo hello\n";
        // On whitespace before recipe
        let result = hover_text(text, Position::new(0, 3));
        assert!(result.is_none());
    }

    #[test]
    fn test_hover_undefined_variable() {
        let text = "all:\n\t$(UNDEFINED)\n";
        let result = hover_text(text, Position::new(1, 3));
        assert!(result.is_none());
    }
}
