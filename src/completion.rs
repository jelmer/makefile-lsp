//! Completion provider for Makefiles.

use makefile_lossless::Makefile;
use tower_lsp_server::ls_types::{CompletionItem, CompletionItemKind, Position};

/// Well-known GNU Make built-in targets.
const BUILTIN_TARGETS: &[(&str, &str)] = &[
    (".PHONY", "Declare targets that are not files"),
    (".SUFFIXES", "Define suffix rules"),
    (".DEFAULT", "Rule for targets with no explicit rule"),
    (".PRECIOUS", "Preserve intermediate files"),
    (".INTERMEDIATE", "Mark targets as intermediate"),
    (".SECONDARY", "Mark targets as secondary (not auto-deleted)"),
    (
        ".SECONDEXPANSION",
        "Enable second expansion of prerequisites",
    ),
    (".DELETE_ON_ERROR", "Delete target on recipe error"),
    (".IGNORE", "Ignore errors in recipes"),
    (".SILENT", "Do not echo recipes"),
    (".EXPORT_ALL_VARIABLES", "Export all variables to sub-makes"),
    (".NOTPARALLEL", "Disable parallel execution"),
    (".ONESHELL", "Run entire recipe in one shell invocation"),
    (".POSIX", "Enable POSIX-conforming mode"),
];

/// Well-known GNU Make automatic variables.
const AUTOMATIC_VARIABLES: &[(&str, &str)] = &[
    ("$@", "The target of the rule"),
    ("$<", "The first prerequisite"),
    ("$^", "All prerequisites (no duplicates)"),
    ("$+", "All prerequisites (with duplicates)"),
    ("$?", "Prerequisites newer than the target"),
    ("$*", "The stem of an implicit rule match"),
    ("$(@D)", "Directory part of $@"),
    ("$(@F)", "File part of $@"),
    ("$(<D)", "Directory part of $<"),
    ("$(<F)", "File part of $<"),
];

/// Well-known GNU Make built-in functions.
const BUILTIN_FUNCTIONS: &[(&str, &str)] = &[
    ("$(subst ", "$(subst from,to,text) - String substitution"),
    (
        "$(patsubst ",
        "$(patsubst pattern,replacement,text) - Pattern substitution",
    ),
    (
        "$(strip ",
        "$(strip string) - Remove leading/trailing whitespace",
    ),
    ("$(findstring ", "$(findstring find,in) - Search for string"),
    (
        "$(filter ",
        "$(filter pattern...,text) - Keep matching words",
    ),
    (
        "$(filter-out ",
        "$(filter-out pattern...,text) - Remove matching words",
    ),
    ("$(sort ", "$(sort list) - Sort and deduplicate words"),
    ("$(word ", "$(word n,text) - Extract nth word"),
    (
        "$(wordlist ",
        "$(wordlist s,e,text) - Extract range of words",
    ),
    ("$(words ", "$(words text) - Count words"),
    ("$(firstword ", "$(firstword names...) - First word"),
    ("$(lastword ", "$(lastword names...) - Last word"),
    ("$(dir ", "$(dir names...) - Directory part of file names"),
    (
        "$(notdir ",
        "$(notdir names...) - Non-directory part of file names",
    ),
    ("$(suffix ", "$(suffix names...) - Suffix of file names"),
    (
        "$(basename ",
        "$(basename names...) - Basename of file names",
    ),
    (
        "$(addsuffix ",
        "$(addsuffix suffix,names...) - Add suffix to names",
    ),
    (
        "$(addprefix ",
        "$(addprefix prefix,names...) - Add prefix to names",
    ),
    ("$(join ", "$(join list1,list2) - Join two lists pairwise"),
    (
        "$(wildcard ",
        "$(wildcard pattern) - Expand file name wildcards",
    ),
    (
        "$(realpath ",
        "$(realpath names...) - Canonical absolute names",
    ),
    ("$(abspath ", "$(abspath names...) - Absolute names"),
    (
        "$(if ",
        "$(if condition,then-part[,else-part]) - Conditional",
    ),
    ("$(or ", "$(or condition1[,condition2...]) - Logical OR"),
    ("$(and ", "$(and condition1[,condition2...]) - Logical AND"),
    ("$(foreach ", "$(foreach var,list,text) - Iterate over list"),
    (
        "$(call ",
        "$(call variable,param...) - Call user-defined function",
    ),
    ("$(eval ", "$(eval text) - Evaluate as makefile syntax"),
    ("$(origin ", "$(origin variable) - Origin of a variable"),
    ("$(flavor ", "$(flavor variable) - Flavor of a variable"),
    ("$(value ", "$(value variable) - Value without expansion"),
    ("$(error ", "$(error text...) - Generate fatal error"),
    ("$(warning ", "$(warning text...) - Generate warning"),
    ("$(info ", "$(info text...) - Print informational message"),
    ("$(shell ", "$(shell command) - Execute shell command"),
];

/// Get completions for a Makefile at the given position.
pub fn get_completions(
    makefile: &Makefile,
    source_text: &str,
    position: Position,
) -> Vec<CompletionItem> {
    let lines: Vec<&str> = source_text.lines().collect();
    let line = lines.get(position.line as usize).copied().unwrap_or("");

    // In a recipe line (starts with tab), offer function and variable completions after $
    if line.starts_with('\t') {
        let col = position.character as usize;
        let prefix = &line[..col.min(line.len())];
        if prefix.ends_with("$(") {
            return get_function_completions();
        }
        if prefix.ends_with('$') {
            return get_automatic_variable_completions();
        }
        return vec![];
    }

    // At column 0 on an empty line, offer target completions
    if position.character == 0 && line.trim().is_empty() {
        return get_target_completions(makefile);
    }

    // If typing a variable name (no = or : yet), offer variable completions
    if position.character > 0 && !line.contains('=') && !line.contains(':') {
        return get_variable_completions(makefile);
    }

    // After $( in any context, offer function completions
    let col = position.character as usize;
    if col >= 2 {
        let prefix = &line[..col.min(line.len())];
        if prefix.ends_with("$(") {
            return get_function_completions();
        }
    }

    vec![]
}

/// Generate target name completions including built-in special targets.
fn get_target_completions(makefile: &Makefile) -> Vec<CompletionItem> {
    let existing_targets: Vec<String> = makefile
        .rules()
        .flat_map(|r| r.targets().collect::<Vec<_>>())
        .collect();

    let completions: Vec<CompletionItem> = BUILTIN_TARGETS
        .iter()
        .filter(|(name, _)| !existing_targets.iter().any(|t| t == name))
        .map(|(name, desc)| CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some(desc.to_string()),
            insert_text: Some(format!("{}: ", name)),
            ..Default::default()
        })
        .collect();

    completions
}

/// Generate variable name completions from variables defined in the file.
fn get_variable_completions(makefile: &Makefile) -> Vec<CompletionItem> {
    makefile
        .variable_definitions()
        .filter_map(|v| {
            let name = v.name()?;
            Some(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: v.raw_value().map(|v| format!("= {}", v.trim())),
                insert_text: Some(format!("{} = ", name)),
                ..Default::default()
            })
        })
        .collect()
}

/// Generate automatic variable completions for use after $.
fn get_automatic_variable_completions() -> Vec<CompletionItem> {
    AUTOMATIC_VARIABLES
        .iter()
        .map(|(name, desc)| CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some(desc.to_string()),
            insert_text: Some(name.trim_start_matches('$').to_string()),
            ..Default::default()
        })
        .collect()
}

/// Generate function completions for use after $(.
fn get_function_completions() -> Vec<CompletionItem> {
    BUILTIN_FUNCTIONS
        .iter()
        .map(|(insert, desc)| {
            let label = insert.trim_start_matches("$(").trim_end().to_string();
            CompletionItem {
                label,
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(desc.to_string()),
                insert_text: Some(insert.to_string()),
                ..Default::default()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completions_empty_line() {
        let text = "all: build\n\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(1, 0));
        assert!(!completions.is_empty());
        assert!(completions.iter().any(|c| c.label == ".PHONY"));
    }

    #[test]
    fn test_completions_exclude_existing_targets() {
        let text = ".PHONY: all\n\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(1, 0));
        assert!(!completions.iter().any(|c| c.label == ".PHONY"));
    }

    #[test]
    fn test_completions_in_recipe() {
        let text = "all:\n\t";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(1, 1));
        assert!(completions.is_empty());
    }

    #[test]
    fn test_variable_completions() {
        let text = "CC = gcc\nCFLAGS = -Wall\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(2, 1));
        // Should not crash, may offer variable completions
        let _ = completions;
    }

    #[test]
    fn test_function_completions() {
        let completions = get_function_completions();
        assert!(!completions.is_empty());
        assert!(completions.iter().any(|c| c.label == "subst"));
        assert!(completions.iter().any(|c| c.label == "wildcard"));
    }
}
