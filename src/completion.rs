//! Completion provider for Makefiles.

use std::path::Path;

use makefile_lossless::{is_in_prerequisites, Makefile};
use tower_lsp_server::ls_types::{CompletionItem, CompletionItemKind, Position};

use crate::builtins;
use crate::position::try_position_to_offset;

/// Get completions for a Makefile at the given position.
///
/// `base_dir` is the directory of the source file; used to resolve relative
/// paths when offering filesystem completions for prerequisites.
pub fn get_completions(
    makefile: &Makefile,
    source_text: &str,
    position: Position,
    base_dir: Option<&Path>,
) -> Vec<CompletionItem> {
    let lines: Vec<&str> = source_text.lines().collect();
    let line = lines.get(position.line as usize).copied().unwrap_or("");

    // In a recipe line (starts with tab), offer function and variable completions after $
    if line.starts_with('\t') {
        let col = position.character as usize;
        let prefix = &line[..col.min(line.len())];
        if prefix.ends_with("$(") {
            let mut items = get_function_completions();
            items.extend(get_variable_reference_completions(makefile));
            return items;
        }
        if prefix.ends_with('$') {
            return get_automatic_variable_completions();
        }
        return vec![];
    }

    // On an include directive line, offer filesystem completions for the path,
    // ranking common Makefile fragment names first.
    let col = position.character as usize;
    if let Some(path_start) = include_path_start(line) {
        if col >= path_start {
            let partial = &line[path_start..col.min(line.len())];
            return get_include_completions(partial, base_dir);
        }
    }

    // If the cursor sits in the prerequisites area, offer target names and
    // filesystem paths matching whatever is being typed.
    if let Some(offset) = try_position_to_offset(source_text, position) {
        let byte_offset: usize = offset.into();
        if is_in_prerequisites(source_text, byte_offset) {
            return get_prerequisite_completions(makefile, source_text, byte_offset, base_dir);
        }
    }

    // At column 0 on an empty line, offer target completions
    if position.character == 0 && line.trim().is_empty() {
        return get_target_completions(makefile);
    }

    // If typing a variable name (no = or : yet), offer variable completions
    if position.character > 0 && !line.contains('=') && !line.contains(':') {
        return get_variable_completions(makefile);
    }

    // After $( in any context, offer function and variable completions
    let col = position.character as usize;
    if col >= 2 {
        let prefix = &line[..col.min(line.len())];
        if prefix.ends_with("$(") {
            let mut items = get_function_completions();
            items.extend(get_variable_reference_completions(makefile));
            return items;
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

    builtins::SPECIAL_TARGETS
        .iter()
        .filter(|(name, _)| !existing_targets.iter().any(|t| t == name))
        .map(|(name, desc)| CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some(desc.to_string()),
            insert_text: Some(format!("{}: ", name)),
            ..Default::default()
        })
        .collect()
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
///
/// Single-character variables (`$@`, `$<`, ...) insert bare; the `D`/`F`
/// variants insert wrapped in `(...)` (e.g. `$(@D)`). Both are derived from the
/// shared [`builtins`] tables so completion, hover, and the SCIP index agree.
fn get_automatic_variable_completions() -> Vec<CompletionItem> {
    let single = builtins::AUTOMATIC_VARIABLES
        .iter()
        .map(|(name, doc)| (format!("${}", name), name.to_string(), doc.to_string()));
    let variants = builtins::AUTOMATIC_VARIABLE_VARIANTS.iter().map(|name| {
        let doc = builtins::find_automatic_variable(name).unwrap_or_default();
        (format!("$({})", name), format!("({})", name), doc)
    });
    single
        .chain(variants)
        .map(|(label, insert, detail)| CompletionItem {
            label,
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some(detail),
            insert_text: Some(insert),
            ..Default::default()
        })
        .collect()
}

/// Generate function completions for use after $(.
fn get_function_completions() -> Vec<CompletionItem> {
    builtins::BUILTIN_FUNCTIONS
        .iter()
        .map(|f| {
            let insert = format!("{} ", f.name);
            let sig = format!("$({} {})", f.name, f.params.join(","));
            CompletionItem {
                label: f.name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(format!("{}: {}", sig, f.doc)),
                insert_text: Some(insert),
                ..Default::default()
            }
        })
        .collect()
}

/// Generate variable reference completions for use after `$(`: well-known
/// built-in variables (`$(MAKE)`, `$(CURDIR)`, ...) plus variables defined in
/// the file. The inserted text closes the parenthesis so accepting `MAKE`
/// yields `$(MAKE)`.
fn get_variable_reference_completions(makefile: &Makefile) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for (name, desc) in builtins::BUILTIN_VARIABLES {
        if !seen.insert((*name).to_string()) {
            continue;
        }
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some(desc.to_string()),
            insert_text: Some(format!("{})", name)),
            ..Default::default()
        });
    }

    for v in makefile.variable_definitions() {
        let Some(name) = v.name() else {
            continue;
        };
        if !seen.insert(name.clone()) {
            continue;
        }
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: v.raw_value().map(|val| format!("= {}", val.trim())),
            insert_text: Some(format!("{})", name)),
            ..Default::default()
        });
    }

    items
}

/// If `line` is an `include`/`-include`/`sinclude` directive, return the byte
/// offset within the line where the (last) path argument begins. Returns `None`
/// for non-include lines. Multiple paths may be listed; we complete the one the
/// cursor is currently within, so we anchor on the start of the final
/// whitespace-separated word.
fn include_path_start(line: &str) -> Option<usize> {
    let trimmed_start = line.len() - line.trim_start().len();
    let rest = &line[trimmed_start..];

    let keyword = ["include", "-include", "sinclude"]
        .iter()
        .find(|kw| rest.strip_prefix(*kw).is_some_and(|r| r.starts_with(' ')))?;

    // Everything after the keyword and its following whitespace is the path
    // list. Anchor on the start of the final word so that, with several paths
    // on one line, we complete whichever the cursor sits in.
    let after_keyword = trimmed_start + keyword.len();
    let args = &line[after_keyword..];
    let last_word = args.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    Some(after_keyword + last_word)
}

/// Common Makefile fragment naming patterns, used to rank include completions.
fn is_makefile_fragment(name: &str) -> bool {
    name.ends_with(".mk")
        || name.ends_with(".make")
        || name.starts_with("Makefile.")
        || name.starts_with("makefile.")
        || name == "Makefile"
        || name == "makefile"
        || name == "GNUmakefile"
}

/// Generate filesystem completions for an include directive path, ranking
/// common Makefile fragment names (`*.mk`, `Makefile.*`, ...) ahead of other
/// entries.
fn get_include_completions(partial: &str, base_dir: Option<&Path>) -> Vec<CompletionItem> {
    let Some(base) = base_dir else {
        return Vec::new();
    };

    // TODO: also offer files from `-I`/`--include-dir` search directories once
    // those are tracked; for now we only complete paths relative to base_dir.
    filesystem_completions(base, partial)
        .into_iter()
        .map(|mut item| {
            let is_dir = item.kind == Some(CompletionItemKind::FOLDER);
            let basename = item.label.rsplit('/').next().unwrap_or(&item.label);
            // Sort directories and Makefile fragments first; LSP clients order
            // by sort_text lexically, so prefix with a rank digit.
            let rank = if is_dir || is_makefile_fragment(basename) {
                '0'
            } else {
                '1'
            };
            item.sort_text = Some(format!("{}{}", rank, item.label));
            item
        })
        .collect()
}

/// Generate completions for the prerequisites part of a rule: existing targets
/// defined in the makefile plus filesystem entries matching the partial word
/// the user is typing.
fn get_prerequisite_completions(
    makefile: &Makefile,
    source_text: &str,
    byte_offset: usize,
    base_dir: Option<&Path>,
) -> Vec<CompletionItem> {
    let partial = partial_word_before(source_text, byte_offset);

    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Targets defined elsewhere in this Makefile (excluding the one on the
    // current line, which we can't easily disambiguate without more parsing —
    // duplicates are fine, GNU Make accepts a target as its own prerequisite
    // only with explicit hand-written intent anyway).
    for rule in makefile.rules() {
        for target in rule.targets() {
            // Skip pattern rules and special targets like `.PHONY`.
            if target.contains('%') || target.starts_with('.') {
                continue;
            }
            if !seen.insert(target.clone()) {
                continue;
            }
            items.push(CompletionItem {
                label: target.clone(),
                kind: Some(CompletionItemKind::REFERENCE),
                detail: Some("target".to_string()),
                insert_text: Some(target),
                ..Default::default()
            });
        }
    }

    if let Some(base) = base_dir {
        for item in filesystem_completions(base, partial) {
            if seen.insert(item.label.clone()) {
                items.push(item);
            }
        }
    }

    items
}

/// Return the partial word ending at `byte_offset` — everything from the last
/// whitespace or `:` back through `byte_offset`. Used to figure out which
/// directory to list for filesystem completions.
fn partial_word_before(source_text: &str, byte_offset: usize) -> &str {
    let line_start = source_text[..byte_offset]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let segment = &source_text[line_start..byte_offset];
    let start_in_segment = segment
        .rfind(|c: char| c.is_whitespace() || c == ':')
        .map(|i| i + 1)
        .unwrap_or(0);
    &segment[start_in_segment..]
}

/// List filesystem entries under `base_dir` that match the directory implied
/// by `partial`. When `partial` contains a `/`, we recurse into the
/// corresponding subdirectory and complete the basename portion; otherwise we
/// list `base_dir`. Returns labels that, when accepted, replace the typed
/// prefix portion (the basename), keeping any leading directory portion intact
/// because the LSP client matches against `label`/`filter_text` from the
/// trigger character backwards through word boundaries — we include the full
/// path in `insert_text`.
fn filesystem_completions(base_dir: &Path, partial: &str) -> Vec<CompletionItem> {
    let (dir_part, basename_prefix) = match partial.rsplit_once('/') {
        Some((dir, base)) => (dir, base),
        None => ("", partial),
    };

    let dir_to_list = if dir_part.is_empty() {
        base_dir.to_path_buf()
    } else if Path::new(dir_part).is_absolute() {
        Path::new(dir_part).to_path_buf()
    } else {
        base_dir.join(dir_part)
    };

    let Ok(entries) = std::fs::read_dir(&dir_to_list) else {
        return Vec::new();
    };

    let mut items = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        // Skip hidden files unless the user explicitly typed a leading dot.
        if name.starts_with('.') && !basename_prefix.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let display = if dir_part.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", dir_part, name)
        };
        let insert = if is_dir {
            format!("{}/", display)
        } else {
            display.clone()
        };
        items.push(CompletionItem {
            label: display,
            kind: Some(if is_dir {
                CompletionItemKind::FOLDER
            } else {
                CompletionItemKind::FILE
            }),
            insert_text: Some(insert),
            ..Default::default()
        });
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completions_empty_line() {
        let text = "all: build\n\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(1, 0), None);
        assert!(!completions.is_empty());
        assert!(completions.iter().any(|c| c.label == ".PHONY"));
    }

    #[test]
    fn test_completions_exclude_existing_targets() {
        let text = ".PHONY: all\n\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(1, 0), None);
        assert!(!completions.iter().any(|c| c.label == ".PHONY"));
    }

    #[test]
    fn test_completions_in_recipe() {
        let text = "all:\n\t";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(1, 1), None);
        assert!(completions.is_empty());
    }

    #[test]
    fn test_variable_completions() {
        let text = "CC = gcc\nCFLAGS = -Wall\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(2, 1), None);
        // Should not crash, may offer variable completions
        let _ = completions;
    }

    #[test]
    fn test_completions_in_recipe_builtin_variables() {
        let text = "all:\n\t$(";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(1, 3), None);
        let make = completions.iter().find(|c| c.label == "MAKE").unwrap();
        assert_eq!(make.insert_text.as_deref(), Some("MAKE)"));
        assert!(completions.iter().any(|c| c.label == "MAKEFLAGS"));
        assert!(completions.iter().any(|c| c.label == "CURDIR"));
        // Functions are still offered alongside variables.
        assert!(completions.iter().any(|c| c.label == "wildcard"));
    }

    #[test]
    fn test_completions_in_recipe_user_variables() {
        let text = "CC = gcc\nall:\n\t$(";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(2, 3), None);
        let cc = completions.iter().find(|c| c.label == "CC").unwrap();
        assert_eq!(cc.insert_text.as_deref(), Some("CC)"));
    }

    #[test]
    fn test_variable_reference_completions_dedups() {
        // A user-defined variable that shares a name with a built-in should
        // appear once, taking the built-in's slot.
        let text = "CC = clang\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let items = get_variable_reference_completions(&makefile);
        let cc: Vec<_> = items.iter().filter(|c| c.label == "CC").collect();
        assert_eq!(cc.len(), 1);
    }

    #[test]
    fn test_function_completions() {
        let completions = get_function_completions();
        assert!(!completions.is_empty());
        assert!(completions.iter().any(|c| c.label == "subst"));
        assert!(completions.iter().any(|c| c.label == "wildcard"));
    }

    #[test]
    fn test_automatic_variable_completions_cover_all_variants() {
        let completions = get_automatic_variable_completions();
        // Single-character forms insert bare.
        let at = completions.iter().find(|c| c.label == "$@").unwrap();
        assert_eq!(at.insert_text.as_deref(), Some("@"));
        // Every D/F variant is offered, including the ^/+/?/* ones the hover and
        // SCIP index now document.
        for variant in builtins::AUTOMATIC_VARIABLE_VARIANTS {
            let label = format!("$({})", variant);
            let item = completions
                .iter()
                .find(|c| c.label == label)
                .unwrap_or_else(|| panic!("missing completion for {}", label));
            assert_eq!(
                item.insert_text.as_deref(),
                Some(&*format!("({})", variant))
            );
        }
    }

    #[test]
    fn test_prerequisite_target_completions() {
        let text = "build:\n\techo build\n\ntest:\n\techo test\n\nall: \n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        // Position cursor right after "all: "
        let completions = get_completions(&makefile, text, Position::new(6, 5), None);
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(
            labels.contains(&"build"),
            "expected 'build' in {:?}",
            labels
        );
        assert!(labels.contains(&"test"), "expected 'test' in {:?}", labels);
    }

    #[test]
    fn test_prerequisite_excludes_pattern_and_special_targets() {
        let text = ".PHONY: build\n\n%.o: %.c\n\techo compile\n\nbuild:\n\techo build\n\nall: \n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(8, 5), None);
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"build"));
        assert!(!labels.iter().any(|l| l.contains('%')));
        assert!(!labels.iter().any(|l| l.starts_with('.')));
    }

    #[test]
    fn test_prerequisite_filesystem_completions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.c"), "").unwrap();
        std::fs::write(dir.path().join("util.c"), "").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();

        let text = "all: \n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(0, 5), Some(dir.path()));

        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"main.c"), "got {:?}", labels);
        assert!(labels.contains(&"util.c"));
        assert!(labels.contains(&"src"));

        let src_item = completions.iter().find(|c| c.label == "src").unwrap();
        assert_eq!(src_item.kind, Some(CompletionItemKind::FOLDER));
        assert_eq!(src_item.insert_text.as_deref(), Some("src/"));
    }

    #[test]
    fn test_prerequisite_filesystem_subdir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("main.c"), "").unwrap();
        std::fs::write(dir.path().join("src").join("util.c"), "").unwrap();

        let text = "all: src/\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(0, 9), Some(dir.path()));

        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"src/main.c"), "got {:?}", labels);
        assert!(labels.contains(&"src/util.c"));
    }

    #[test]
    fn test_prerequisite_filesystem_skips_hidden() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.c"), "").unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();

        let text = "all: \n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(0, 5), Some(dir.path()));
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"visible.c"));
        assert!(!labels.contains(&".hidden"));
    }

    #[test]
    fn test_prerequisite_filesystem_includes_hidden_when_typed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.c"), "").unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();

        let text = "all: .\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(0, 6), Some(dir.path()));
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&".hidden"), "got {:?}", labels);
    }

    #[test]
    fn test_include_path_start() {
        assert_eq!(include_path_start("include "), Some(8));
        assert_eq!(include_path_start("include foo.mk"), Some(8));
        assert_eq!(include_path_start("-include .env"), Some(9));
        assert_eq!(include_path_start("sinclude bar"), Some(9));
        assert_eq!(include_path_start("  include foo"), Some(10));
        // Multiple paths: anchor on the last word.
        assert_eq!(include_path_start("include a.mk b.mk"), Some(13));
        assert_eq!(include_path_start("all: foo"), None);
        assert_eq!(include_path_start("includex foo"), None);
    }

    #[test]
    fn test_include_completions_rank_fragments_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.mk"), "").unwrap();
        std::fs::write(dir.path().join("README.txt"), "").unwrap();
        std::fs::write(dir.path().join("Makefile.local"), "").unwrap();

        let text = "include \n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(0, 8), Some(dir.path()));

        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"config.mk"), "got {:?}", labels);
        assert!(labels.contains(&"Makefile.local"));
        assert!(labels.contains(&"README.txt"));

        let mk = completions.iter().find(|c| c.label == "config.mk").unwrap();
        let readme = completions
            .iter()
            .find(|c| c.label == "README.txt")
            .unwrap();
        assert!(
            mk.sort_text.as_deref() < readme.sort_text.as_deref(),
            "fragment {:?} should rank before {:?}",
            mk.sort_text,
            readme.sort_text
        );
    }

    #[test]
    fn test_include_completions_partial_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.mk"), "").unwrap();
        std::fs::create_dir(dir.path().join("rules")).unwrap();
        std::fs::write(dir.path().join("rules").join("common.mk"), "").unwrap();

        let text = "include rules/\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let completions = get_completions(&makefile, text, Position::new(0, 14), Some(dir.path()));
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"rules/common.mk"), "got {:?}", labels);
    }

    #[test]
    fn test_partial_word_before() {
        assert_eq!(partial_word_before("all: src/", 9), "src/");
        assert_eq!(partial_word_before("all: main", 9), "main");
        assert_eq!(partial_word_before("all: ", 5), "");
        assert_eq!(partial_word_before("all: foo bar", 12), "bar");
    }
}
